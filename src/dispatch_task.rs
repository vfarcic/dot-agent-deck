//! PRD #1790 stage 1: the task a dispatched unit was given, saved beside its
//! worktree so a later stage can hand it back after a daemon restart.
//!
//! The daemon writes `<worktree>/.dot-agent-deck/dispatch-task-<work id>.md`
//! when it dispatches a unit, for `dot-agent-deck dispatch`
//! ([`crate::dispatch::handle_dispatch`]) and for scheduled issue dispatch
//! ([`crate::issue_dispatch_run`]), holding the task text exactly as the
//! dispatcher gave it (for an issue, the prompt with its issue number
//! substituted). The work id is the unit's work in the journal, so a reader
//! joins the file to the journal by name. The text itself never goes into
//! the journal.
//!
//! What [`save`] guarantees, and only when it answers `Ok`:
//!
//! - **By owner and mode bits, no other account can write the worktree
//!   root or redirect its path** (Unix; an ACL is not seen). Before it
//!   creates or changes anything, the save refuses a worktree root that is
//!   owned by another account, writable by other, or writable by a group
//!   that is not this account's private group
//!   ([`crate::orchestrator_context::project_root_refusal`]); a worktree
//!   path that another account could make name a different directory
//!   ([`crate::orchestrator_context::resolve_trusted_dir_path`], below); and
//!   a worktree that is not a git checkout. That is the trust boundary the
//!   rest relies on: git below is asked by pathname, and an account that
//!   could rename entries of the worktree root could swap `.dot-agent-deck`
//!   for another directory while git looks and put it back before the file
//!   is created; one that could rename entries of any directory above it
//!   could do the same to the whole worktree. Such an account can already
//!   change the worktree's source, or move it, so the save does not try to
//!   bind git's lookup to the directory it holds; it refuses a worktree
//!   where another account has that access.
//! - **The path check** resolves the worktree's path as the kernel does,
//!   from `/`, following each symlink, and judges every directory and
//!   symlink on the way, `/` included, the way OpenSSH's `StrictModes` does:
//!   each is owned by this account or root; a directory another account
//!   can write (other write, or group write for a group that is not this
//!   account's private group) has the sticky bit, and the entry below it on
//!   the path is owned by this account, since the sticky bit only stops an
//!   account renaming entries it does not own. So a worktree under `/tmp`
//!   (`1777`, root's) in a directory of this account's own is accepted, and
//!   one in a non-sticky `0777` directory, or reached through a symlink
//!   placed in one, is refused. The resolved path, which goes through no
//!   symlink, must name the worktree root the save holds open, and is where
//!   git runs.
//! - **The directory is the one it checked.** `.dot-agent-deck` is opened
//!   once, without following a symlink, owned by this account and with group
//!   and other write cleared on the held descriptor (or refused)
//!   ([`crate::orchestrator_context::open_held_context_dir`]); every file
//!   below is created relative to that descriptor. A save refused for its
//!   worktree root, or because the worktree is not a git checkout, has not
//!   created or re-permissioned it.
//! - **Git ignores the file, as git itself reports it, before any task byte
//!   exists.** It adds `.dot-agent-deck/` to the clone's `info/exclude`
//!   (issue #329's mechanism) and, when `.dot-agent-deck/` holds no
//!   `.gitignore`, creates one containing `*`, which a negation in a tracked
//!   `.gitignore` above it cannot override; an existing `.gitignore` there is
//!   never changed, and one that is not a regular file is refused. Then it
//!   asks git (`git check-ignore -q`, whose exit status is the verdict:
//!   0 ignored, 1 not; and `git ls-files --error-unmatch`: 0 tracked, 1 not)
//!   whether the task's path is ignored and untracked, and writes nothing
//!   when it is not, or when either answers anything else: a worktree whose
//!   own rules re-include the file gets no file and a warning. Each git call
//!   runs for at most [`GIT_DEADLINE`]; past it the call's process group is
//!   killed and the child reaped, and the save fails, so the dispatch goes
//!   on without a saved task. The held directory is then checked to still
//!   be the one at that path before the file is created. That holds for
//!   git's rules as they are at the save, not against a `.gitignore` edited
//!   afterwards.
//! - **Never overwritten.** The file is created `O_EXCL`, owner-only. Each
//!   dispatch mints a new work id, so a later dispatch into the same worktree
//!   writes a new file; a file already there for the same work id is kept
//!   and the save fails with a warning, never replaced.
//! - **Not swept.** The coordination sweep's age bound
//!   ([`crate::orchestrator_context::is_sweepable_coordination_name`]) and
//!   the inbox spill retention ([`crate::inbox_spills`], which removes only
//!   the `inbox-message-*` files it minted) both leave it alone. It goes with
//!   the worktree.
//!
//! Off Unix the worktree root's owner and mode are not judged and the
//! directory operations are path operations
//! ([`crate::orchestrator_context::HeldContextDir`]).
//!
//! A reader that acts on a saved task opens it the same way (no symlink
//! followed, owned by the user, not writable by others); the work journal's
//! maintainer page lists what else a restore must check.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::orchestrator_context::{CONTEXT_DIR_NAME, HeldContextDir};
use crate::work_journal::WorkId;

/// The prefix of every saved task's file name.
const PREFIX: &str = "dispatch-task-";

/// The ignore file [`save`] creates in `.dot-agent-deck/` when there is none.
const IGNORE_FILE: &str = ".gitignore";

/// What that ignore file holds: everything in the directory, itself included.
const IGNORE_ALL: &str = "*\n";

/// How long one git call of a [`save`] may run before it is killed. Both
/// calls read a few small files and normally answer in milliseconds, so
/// reaching it means git is blocked (a FIFO among its inputs, a hung
/// filesystem), not slow.
pub const GIT_DEADLINE: Duration = Duration::from_secs(10);

/// The file name of `work`'s saved task: `dispatch-task-<work id>.md`.
pub fn file_name(work: &WorkId) -> String {
    format!("{PREFIX}{work}.md")
}

/// `name` is named as a saved dispatch task ([`file_name`]).
pub fn is_task_file_name(name: &str) -> bool {
    name.strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(".md"))
        .is_some_and(|work| WorkId::parse(work).is_some())
}

/// Save `task` as `work`'s dispatched task in `worktree`, only once git
/// ignores it there. Answers the file's path. See the module docs.
pub fn save(worktree: &Path, work: &WorkId, task: &str) -> io::Result<PathBuf> {
    save_with(worktree, work, task, &crate::git_env::git_at, GIT_DEADLINE)
}

/// [`save`] with the git command (`git(dir)`, a `git` to run in `dir`) and
/// each call's deadline passed in, so a test can stand in a git that hangs.
fn save_with(
    worktree: &Path,
    work: &WorkId,
    task: &str,
    git: &dyn Fn(&Path) -> Command,
    deadline: Duration,
) -> io::Result<PathBuf> {
    let name = file_name(work);
    let held = crate::orchestrator_context::open_held_context_dir(worktree)?;
    ensure_ignore_file(&held)?;
    ensure_ignored(
        held.project_path(),
        &format!("{CONTEXT_DIR_NAME}/{name}"),
        git,
        deadline,
    )?;
    if !held.unchanged() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{CONTEXT_DIR_NAME} was replaced while the task was being saved"),
        ));
    }
    if held.has_entry(&name) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{name} already exists; the saved task is kept, not replaced"),
        ));
    }
    let mut file = held.create_new(&name)?;
    let written = file
        .write_all(task.as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(error) = written {
        drop(file);
        let _ = held.remove(&name);
        return Err(error);
    }
    Ok(held.path().join(name))
}

/// Create `.dot-agent-deck/.gitignore` holding `*` when the directory has no
/// entry of that name; an existing regular file, whatever it holds, is left
/// as it is, and an existing entry of any other kind (a FIFO, a symlink, a
/// directory) is refused, since git would block on or follow it.
fn ensure_ignore_file(held: &HeldContextDir) -> io::Result<()> {
    match held.is_regular_file(IGNORE_FILE)? {
        Some(true) => return Ok(()),
        Some(false) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{CONTEXT_DIR_NAME}/{IGNORE_FILE} is not a regular file"),
            ));
        }
        None => {}
    }
    let mut file = match held.create_new(IGNORE_FILE) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return match held.is_regular_file(IGNORE_FILE)? {
                Some(true) => Ok(()),
                _ => Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{CONTEXT_DIR_NAME}/{IGNORE_FILE} is not a regular file"),
                )),
            };
        }
        Err(e) => return Err(e),
    };
    let written = file
        .write_all(IGNORE_ALL.as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(error) = written {
        drop(file);
        let _ = held.remove(IGNORE_FILE);
        return Err(error);
    }
    Ok(())
}

/// `Ok` only when git, run in `worktree`, says `relative` is ignored
/// (`check-ignore -q` exits 0; 1 is "not ignored") and not tracked
/// (`ls-files --error-unmatch` exits 1, its "did not match"; 0 is
/// "tracked"). Any other exit, a signal, or a call past `deadline` is an
/// error, never read as either answer.
fn ensure_ignored(
    worktree: &Path,
    relative: &str,
    git: &dyn Fn(&Path) -> Command,
    deadline: Duration,
) -> io::Result<()> {
    let mut check = git(worktree);
    check.args(["check-ignore", "-q", "--", relative]);
    let ignored = status_within(check, deadline)?;
    match ignored.code() {
        Some(0) => {}
        Some(1) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "git does not ignore {relative} in this worktree (its own rules re-include it)"
                ),
            ));
        }
        _ => {
            return Err(io::Error::other(format!(
                "git could not say whether {relative} is ignored ({ignored})"
            )));
        }
    }
    let mut listed = git(worktree);
    listed.args(["ls-files", "--error-unmatch", "--", relative]);
    let tracked = status_within(listed, deadline)?;
    match tracked.code() {
        Some(1) => Ok(()),
        Some(0) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{relative} is tracked by git in this worktree"),
        )),
        _ => Err(io::Error::other(format!(
            "git could not say whether {relative} is tracked ({tracked})"
        ))),
    }
}

/// Run `cmd`, with no stdin and its output discarded, to its exit, or kill
/// it once `deadline` has passed: on Unix its whole process group (it is
/// given one of its own), and the child is reaped before this answers
/// `TimedOut`. The group is signalled while the child is still unreaped, so
/// its id cannot have been reissued.
fn status_within(mut cmd: Command, deadline: Duration) -> io::Result<ExitStatus> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn()?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        let elapsed = started.elapsed();
        if elapsed >= deadline {
            #[cfg(unix)]
            if let Ok(group) = libc::pid_t::try_from(child.id()) {
                // SAFETY: `kill` has no memory preconditions. The child is
                // not reaped yet (`try_wait` above answered `None`), so its
                // id, which is its group's, is still this command's.
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("git did not answer within {deadline:?} and was killed"),
            ));
        }
        std::thread::sleep((deadline - elapsed).min(Duration::from_millis(5)));
    }
}

/// [`save`] off the async runtime, logging a failure instead of answering
/// it: a dispatch whose task could not be saved still runs, it just cannot be
/// handed its task back after a restart. Returns within about two
/// [`GIT_DEADLINE`]s of git time plus the file operations.
pub async fn save_or_warn(worktree: &Path, work: Option<&WorkId>, task: &str) {
    save_or_warn_with(worktree, work, task, crate::git_env::git_at, GIT_DEADLINE).await;
}

/// [`save_or_warn`] with [`save_with`]'s seams.
async fn save_or_warn_with(
    worktree: &Path,
    work: Option<&WorkId>,
    task: &str,
    git: impl Fn(&Path) -> Command + Send + 'static,
    deadline: Duration,
) {
    let Some(work) = work.cloned() else {
        return;
    };
    let (dir, text) = (worktree.to_path_buf(), task.to_string());
    let saved = tokio::task::spawn_blocking(move || {
        save_with(&dir, &work, &text, &git, deadline).map_err(|e| (work, e))
    })
    .await;
    match saved {
        Ok(Ok(_)) => {}
        Ok(Err((work, error))) => tracing::warn!(
            worktree = %worktree.display(),
            %work,
            %error,
            "dispatch: the dispatched task was not saved beside its worktree, so a restore cannot hand it back"
        ),
        Err(error) => tracing::warn!(
            worktree = %worktree.display(),
            %error,
            "dispatch: saving the dispatched task did not finish"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deck_identity::DeckId;

    fn work() -> WorkId {
        WorkId::mint(&DeckId::parse("abcdefgh").unwrap()).unwrap()
    }

    #[test]
    fn its_name_is_recognised_and_nothing_else_is() {
        let w = work();
        let name = file_name(&w);
        assert!(name.starts_with("dispatch-task-w-abcdefgh-"));
        assert!(is_task_file_name(&name));
        for other in [
            "dispatch-task-.md",
            "dispatch-task-not-a-work.md",
            "worker-task-coder.md",
            "dispatch-task-w-abcdefgh-0123456789abcdef.txt",
        ] {
            assert!(!is_task_file_name(other), "{other}");
        }
    }

    /// The two retention sweeps in the worktree's `.dot-agent-deck/` leave a
    /// saved task alone: the coordination sweep by name, and the inbox spill
    /// retention because it is not one of its files.
    #[test]
    fn neither_sweep_takes_a_saved_task() {
        let name = file_name(&work());
        assert!(!crate::orchestrator_context::is_sweepable_coordination_name(&name));
        assert!(!crate::inbox_spills::is_spill_file_name(&name));
        // Control: an ordinary coordination file is still swept.
        assert!(crate::orchestrator_context::is_sweepable_coordination_name(
            "worker-task-coder.md"
        ));
    }

    #[test]
    fn a_task_outside_a_git_checkout_is_not_saved() {
        let dir = tempfile::tempdir().unwrap();
        let w = work();
        let error = save(dir.path(), &w, "TASK").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            !dir.path()
                .join(".dot-agent-deck")
                .join(file_name(&w))
                .exists()
        );
    }

    /// A one-commit repository at `root/repo`, its tracked `.gitignore`
    /// holding `gitignore` when given; answers the repository's path.
    fn repo(root: &Path, gitignore: Option<&str>) -> PathBuf {
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = crate::git_env::fixture_git(&repo, root)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        // Owner-only write, whatever the umask: the root check refuses a
        // root another account could write.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README"), "x\n").unwrap();
        if let Some(rules) = gitignore {
            std::fs::write(repo.join(".gitignore"), rules).unwrap();
        }
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        repo
    }

    /// Whether git, from `dir`, reports `path` ignored (exit status of the
    /// non-verbose `check-ignore -q`).
    fn git_ignores(dir: &Path, sandbox: &Path, path: &str) -> bool {
        crate::git_env::fixture_git(dir, sandbox)
            .args(["check-ignore", "-q", "--", path])
            .status()
            .unwrap()
            .success()
    }

    fn task_path(dir: &Path, work: &WorkId) -> PathBuf {
        dir.join(CONTEXT_DIR_NAME).join(file_name(work))
    }

    /// R3/A1: a tracked `.gitignore` at the root that re-includes
    /// `.dot-agent-deck/` does not expose the task: the local `.gitignore`
    /// the save creates in the directory wins, git reports the file ignored,
    /// and the tracked file is not changed.
    #[test]
    fn a_root_negation_does_not_expose_the_task() {
        let root = tempfile::tempdir().unwrap();
        let negation = "!.dot-agent-deck/\n!.dot-agent-deck/**\n";
        let repo = repo(root.path(), Some(negation));
        let w = work();
        let saved = save(&repo, &w, "TASK BODY").unwrap();
        assert_eq!(saved, task_path(&repo, &w));
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), "TASK BODY");
        let relative = format!("{CONTEXT_DIR_NAME}/{}", file_name(&w));
        assert!(git_ignores(&repo, root.path(), &relative));
        assert_eq!(
            std::fs::read_to_string(repo.join(".gitignore")).unwrap(),
            negation
        );
        let status = crate::git_env::fixture_git(&repo, root.path())
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            status.stdout.is_empty(),
            "{}",
            String::from_utf8_lossy(&status.stdout)
        );
    }

    /// R3/A1: a `.gitignore` the user keeps in `.dot-agent-deck/` that
    /// re-includes the task, under a tracked root rule that re-includes the
    /// directory, is never overwritten, so git does not ignore the task there
    /// and no file is written. (A nested negation alone cannot expose it: git
    /// re-includes nothing inside a directory `info/exclude` excludes.)
    #[test]
    fn a_nested_negation_means_no_task_is_written() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo(root.path(), Some("!.dot-agent-deck/\n"));
        let dir = repo.join(CONTEXT_DIR_NAME);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join(".gitignore"), "!dispatch-task-*.md\n").unwrap();
        let w = work();
        let error = save(&repo, &w, "TASK").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        assert!(!task_path(&repo, &w).exists());
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
            "!dispatch-task-*.md\n"
        );
    }

    /// R3/A1: in an ordinary linked worktree (what a dispatch makes) the task
    /// is saved and ignored there.
    #[test]
    fn a_linked_worktree_gets_an_ignored_task() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo(root.path(), None);
        let linked = root.path().join("linked");
        let out = crate::git_env::fixture_git(&repo, root.path())
            .args(["worktree", "add", "-q", "-b", "unit"])
            .arg(&linked)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&linked, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let w = work();
        save(&linked, &w, "TASK").unwrap();
        let relative = format!("{CONTEXT_DIR_NAME}/{}", file_name(&w));
        assert!(git_ignores(&linked, root.path(), &relative));
        assert!(task_path(&linked, &w).exists());
    }

    /// A2 and re-dispatch: a second save under the same work id keeps the
    /// first file and fails, and a new work id (a later dispatch into the
    /// same worktree) writes its own file.
    #[test]
    fn a_saved_task_is_never_replaced() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo(root.path(), None);
        let w = work();
        save(&repo, &w, "FIRST").unwrap();
        let error = save(&repo, &w, "SECOND").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(task_path(&repo, &w)).unwrap(),
            "FIRST"
        );
        let later = work();
        save(&repo, &later, "LATER").unwrap();
        assert_eq!(
            std::fs::read_to_string(task_path(&repo, &later)).unwrap(),
            "LATER"
        );
    }

    /// A2: a `.dot-agent-deck` that group or other can write is repaired on
    /// the held descriptor before the task is written; the file is
    /// owner-only.
    #[cfg(unix)]
    #[test]
    fn a_group_writable_directory_is_repaired_before_the_save() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let repo = repo(root.path(), None);
        let dir = repo.join(CONTEXT_DIR_NAME);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let w = work();
        let saved = save(&repo, &w, "TASK").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o755, "group and other write cleared");
        assert_eq!(mode(&saved), 0o600);
    }

    /// A2: a `.dot-agent-deck` that is a symlink is refused, and nothing is
    /// written where it points.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo(root.path(), None);
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, repo.join(CONTEXT_DIR_NAME)).unwrap();
        let w = work();
        let error = save(&repo, &w, "TASK").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    /// A stand-in for git: `sh -c <script>`, the git arguments reaching the
    /// script as `$1`, `$2`, ...
    fn fake_git(script: &'static str) -> impl Fn(&Path) -> Command + Send + 'static {
        move |dir: &Path| {
            let mut cmd = Command::new("/bin/sh");
            cmd.current_dir(dir).args(["-c", script, "sh"]);
            cmd
        }
    }

    /// F2: a git that never answers is killed at the deadline and reaped:
    /// the save fails `TimedOut` within a bound, writes no task, and the
    /// process it started is gone (not a zombie).
    #[cfg(unix)]
    #[test]
    fn a_hanging_git_is_killed_and_reaped_at_the_deadline() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo(root.path(), None);
        let pid_file = root.path().join("git.pid");
        let script: &'static str = Box::leak(
            format!("echo $$ > '{}'; exec sleep 30", pid_file.display()).into_boxed_str(),
        );
        let w = work();
        let started = Instant::now();
        let error = save_with(
            &repo,
            &w,
            "TASK",
            &fake_git(script),
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(!task_path(&repo, &w).exists());
        let pid: libc::pid_t = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 probes only; `pid` is a number, not memory.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "the timed-out git is still there (or unreaped)");
    }

    /// F2: the dispatch path's save returns within its bound when git
    /// hangs, so the caller goes on to spawn the unit.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dispatch_save_returns_when_git_hangs() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo(root.path(), None);
        let w = work();
        let started = Instant::now();
        tokio::time::timeout(
            Duration::from_secs(5),
            save_or_warn_with(
                &repo,
                Some(&w),
                "TASK",
                fake_git("exec sleep 30"),
                Duration::from_millis(300),
            ),
        )
        .await
        .expect("the save did not return within its bound");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!task_path(&repo, &w).exists());
    }

    /// F2 control: a git that answers at once ("ignored", "not tracked")
    /// is not cut short by the deadline, and the task is saved.
    #[cfg(unix)]
    #[test]
    fn a_git_that_answers_saves_within_the_deadline() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo(root.path(), None);
        let w = work();
        let git =
            fake_git(r#"case "$1" in check-ignore) exit 0;; ls-files) exit 1;; esac; exit 2"#);
        let saved = save_with(&repo, &w, "TASK", &git, Duration::from_secs(10)).unwrap();
        assert_eq!(std::fs::read_to_string(saved).unwrap(), "TASK");
    }

    /// F2: `ls-files --error-unmatch` failing for any reason but its "did
    /// not match" (exit 1) is a git error, not proof the task is untracked,
    /// and nothing is written; its exit 0 means tracked.
    #[cfg(unix)]
    #[test]
    fn a_git_error_is_not_read_as_untracked() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo(root.path(), None);
        for (script, kind) in [
            (
                r#"case "$1" in check-ignore) exit 0;; esac; exit 128"#,
                io::ErrorKind::Other,
            ),
            (
                r#"case "$1" in check-ignore) exit 0;; esac; exit 0"#,
                io::ErrorKind::PermissionDenied,
            ),
            (
                r#"case "$1" in check-ignore) exit 128;; esac; exit 1"#,
                io::ErrorKind::Other,
            ),
        ] {
            let w = work();
            let error = save_with(
                &repo,
                &w,
                "TASK",
                &fake_git(script),
                Duration::from_secs(10),
            )
            .unwrap_err();
            assert_eq!(error.kind(), kind, "{script}: {error}");
            assert!(!task_path(&repo, &w).exists(), "{script}");
        }
    }

    /// F2: an existing `.dot-agent-deck/.gitignore` that is a FIFO (git
    /// would block opening it once a tracked rule re-includes the
    /// directory) is refused before git runs, and no task is written.
    #[cfg(unix)]
    #[test]
    fn a_fifo_ignore_file_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo(root.path(), Some("!.dot-agent-deck/\n"));
        let dir = repo.join(CONTEXT_DIR_NAME);
        std::fs::create_dir(&dir).unwrap();
        let fifo = std::ffi::CString::new(dir.join(".gitignore").to_str().unwrap()).unwrap();
        // SAFETY: a NUL-terminated path; `mkfifo` reads nothing else.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let w = work();
        let started = Instant::now();
        let error = save(&repo, &w, "TASK").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!task_path(&repo, &w).exists());
    }

    /// F3: the decision on the worktree root's owner and mode.
    #[test]
    fn only_a_root_this_account_alone_can_write_is_accepted() {
        let (me, other, private, shared) = (1000, 1001, 1000, 100);
        let refusal = crate::orchestrator_context::project_root_refusal;
        assert_eq!(refusal(me, shared, 0o755, me, Some(private)), None);
        assert_eq!(refusal(me, shared, 0o700, me, None), None);
        // The user's private group may write (what `umask 002` gives).
        assert_eq!(refusal(me, private, 0o775, me, Some(private)), None);
        // Any other group may not, nor a group when no private group exists.
        assert!(refusal(me, shared, 0o775, me, Some(private)).is_some());
        assert!(refusal(me, private, 0o775, me, None).is_some());
        // Other write, or another owner, is refused whatever the group.
        assert!(refusal(me, private, 0o757, me, Some(private)).is_some());
        assert!(refusal(other, shared, 0o755, me, Some(private)).is_some());
    }

    /// G1: the decision on each entry met while resolving the worktree's
    /// path: owned by this account or root; a directory others can write
    /// only with the sticky bit; an entry of such a directory owned by this
    /// account; the private group's write is this account's own.
    #[test]
    fn only_a_path_other_accounts_cannot_redirect_is_accepted() {
        use crate::orchestrator_context::{PathEntry, path_entry_refusal as refusal};
        let (me, other, private, shared) = (1000, 1001, 1000, 100);
        let dir = |uid, gid, mode| PathEntry {
            uid,
            gid,
            mode,
            dir: true,
        };
        let link = |uid| PathEntry {
            uid,
            gid: shared,
            mode: 0o777,
            dir: false,
        };
        let slash = dir(0, 0, 0o755);
        // An ordinary path: `/`, `/home`, the user's own directories.
        assert_eq!(refusal(None, slash, me, Some(private)), None);
        assert_eq!(refusal(Some(slash), dir(0, 0, 0o755), me, None), None);
        assert_eq!(refusal(Some(slash), dir(me, shared, 0o755), me, None), None);
        assert_eq!(refusal(Some(slash), link(me), me, None), None);
        assert_eq!(refusal(Some(slash), link(0), me, None), None);
        // The private group may write; any other group, or other, may not
        // without the sticky bit.
        assert_eq!(
            refusal(Some(slash), dir(me, private, 0o775), me, Some(private)),
            None
        );
        assert!(refusal(Some(slash), dir(me, private, 0o775), me, None).is_some());
        assert!(refusal(Some(slash), dir(0, shared, 0o775), me, Some(private)).is_some());
        assert!(refusal(Some(slash), dir(0, 0, 0o777), me, None).is_some());
        assert!(
            refusal(None, dir(0, 0, 0o777), me, None).is_some(),
            "`/` too"
        );
        // Another account's directory or symlink, whatever its mode.
        assert!(refusal(Some(slash), dir(other, shared, 0o755), me, None).is_some());
        assert!(refusal(Some(slash), link(other), me, None).is_some());
        // A sticky shared directory is accepted, and then its entry on the
        // path must be this account's own.
        let tmp = dir(0, 0, 0o1777);
        assert_eq!(refusal(Some(slash), tmp, me, None), None);
        assert_eq!(refusal(Some(tmp), dir(me, shared, 0o700), me, None), None);
        assert_eq!(refusal(Some(tmp), link(me), me, None), None);
        assert!(refusal(Some(tmp), dir(0, 0, 0o755), me, None).is_some());
        assert!(refusal(Some(tmp), link(0), me, None).is_some());
        assert!(refusal(Some(tmp), dir(other, other, 0o700), me, None).is_some());
    }

    /// G1 (reviewer finding 1, auditor A1): a worktree whose parent every
    /// account can write, without the sticky bit, is refused before
    /// anything is created, since another account could rename the whole
    /// worktree aside while git looks; the same parent with the sticky bit
    /// (as `/tmp`) is saved into, and so is an ordinary parent only this
    /// account writes.
    #[cfg(unix)]
    #[test]
    fn a_worktree_under_a_shared_parent_is_saved_only_when_the_parent_is_sticky() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        let repo = repo(&shared, None);
        let exclude = repo.join(".git/info/exclude");
        let exclude_before = std::fs::read(&exclude).ok();
        let mode = |bits| std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(bits));
        mode(0o777).unwrap();
        let w = work();
        let error = save(&repo, &w, "TASK").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        assert!(error.to_string().contains("shared"), "{error}");
        assert!(!repo.join(CONTEXT_DIR_NAME).exists(), "nothing created");
        assert_eq!(
            std::fs::read(&exclude).ok(),
            exclude_before,
            "nothing changed"
        );
        mode(0o1777).unwrap();
        save(&repo, &w, "TASK").unwrap();
        assert!(task_path(&repo, &w).exists(), "a sticky parent is accepted");
        mode(0o755).unwrap();
        let w2 = work();
        save(&repo, &w2, "TASK").unwrap();
        assert!(
            task_path(&repo, &w2).exists(),
            "an ordinary parent is accepted"
        );
    }

    /// G1: a worktree named through a symlink that sits in a directory every
    /// account can write is refused (that account could point the link at
    /// another checkout while git looks); the same link in a directory only
    /// this account writes is followed, and the task is saved in the
    /// checkout it names.
    #[cfg(unix)]
    #[test]
    fn a_worktree_path_through_a_shared_symlink_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let repo = repo(root.path(), None);
        let shared = root.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::os::unix::fs::symlink(&repo, shared.join("link")).unwrap();
        let w = work();
        let error = save(&shared.join("link"), &w, "TASK").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        assert!(!repo.join(CONTEXT_DIR_NAME).exists(), "nothing created");
        let own = root.path().join("own");
        std::fs::create_dir(&own).unwrap();
        std::fs::set_permissions(&own, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&repo, own.join("link")).unwrap();
        save(&own.join("link"), &w, "TASK").unwrap();
        assert!(task_path(&repo, &w).exists());
    }

    /// F3: a worktree root every account can write is refused before
    /// anything is created: no `.dot-agent-deck`, no exclude rule, no task.
    /// The same root at 0755 is saved into.
    #[cfg(unix)]
    #[test]
    fn a_root_other_accounts_can_write_is_refused_and_nothing_is_written() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let repo = repo(root.path(), None);
        let exclude = repo.join(".git/info/exclude");
        let exclude_before = std::fs::read(&exclude).ok();
        std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o757)).unwrap();
        let w = work();
        let error = save(&repo, &w, "TASK").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        assert!(!repo.join(CONTEXT_DIR_NAME).exists(), "nothing created");
        assert_eq!(std::fs::read(&exclude).ok(), exclude_before);
        // Control: owner-only write is accepted.
        std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o755)).unwrap();
        save(&repo, &w, "TASK").unwrap();
        assert!(task_path(&repo, &w).exists());
    }

    /// F3: a group-writable worktree root is refused unless its group is
    /// this account's private group, and nothing is written when refused.
    #[cfg(unix)]
    #[test]
    fn a_group_writable_root_is_saved_into_only_for_the_private_group() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let root = tempfile::tempdir().unwrap();
        let repo = repo(root.path(), None);
        std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o775)).unwrap();
        let gid = std::fs::metadata(&repo).unwrap().gid();
        let private = crate::platform::paths::effective_private_group() == Some(gid);
        let w = work();
        let saved = save(&repo, &w, "TASK");
        if private {
            saved.unwrap();
        } else {
            assert_eq!(saved.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            assert!(!repo.join(CONTEXT_DIR_NAME).exists(), "nothing created");
        }
    }

    /// F6: a save refused because the worktree is not a git checkout
    /// leaves an existing `.dot-agent-deck` exactly as it was, 0777
    /// included, since eligibility is settled before the directory is
    /// opened or repaired.
    #[cfg(unix)]
    #[test]
    fn a_refused_save_leaves_the_directory_mode_as_it_was() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("not-a-checkout");
        std::fs::create_dir(&project).unwrap();
        std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir = project.join(CONTEXT_DIR_NAME);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let w = work();
        let error = save(&project, &w, "TASK").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o777, "a refused save changed the directory's mode");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }
}
