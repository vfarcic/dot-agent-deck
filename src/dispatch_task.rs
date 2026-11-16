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
//! - **The directory is the one it checked.** `.dot-agent-deck` is opened
//!   once, without following a symlink, owned by this account and with group
//!   and other write cleared on the held descriptor (or refused)
//!   ([`crate::orchestrator_context::open_held_context_dir`]); every file
//!   below is created relative to that descriptor.
//! - **Git ignores the file, as git itself reports it, before any task byte
//!   exists.** It adds `.dot-agent-deck/` to the clone's `info/exclude`
//!   (issue #329's mechanism) and, when `.dot-agent-deck/` holds no
//!   `.gitignore`, creates one containing `*`, which a negation in a tracked
//!   `.gitignore` above it cannot override; an existing `.gitignore` there is
//!   never changed. Then it asks git (`git check-ignore -q`, whose exit status
//!   is the verdict, and `git ls-files`) whether the task's path is ignored
//!   and untracked, and writes nothing when it is not: a worktree whose own
//!   rules re-include the file, or that is not a git checkout, gets no file
//!   and a warning. Git is asked by pathname; the held directory is then
//!   checked to still be the one at that path before the file is created.
//!   That holds for git's rules as they are at the save, not against a
//!   `.gitignore` edited afterwards.
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
//! A reader that acts on a saved task opens it the same way (no symlink
//! followed, owned by the user, not writable by others); the work journal's
//! maintainer page lists what else a restore must check.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::orchestrator_context::{CONTEXT_DIR_NAME, GitExcludeOutcome, HeldContextDir};
use crate::work_journal::WorkId;

/// The prefix of every saved task's file name.
const PREFIX: &str = "dispatch-task-";

/// The ignore file [`save`] creates in `.dot-agent-deck/` when there is none.
const IGNORE_FILE: &str = ".gitignore";

/// What that ignore file holds: everything in the directory, itself included.
const IGNORE_ALL: &str = "*\n";

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
    let name = file_name(work);
    let held = crate::orchestrator_context::open_held_context_dir(worktree)?;
    if held.ensure_git_excludes()? == GitExcludeOutcome::NotAGitRepo {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the worktree is not a git checkout, so the task file could not be kept out of a commit",
        ));
    }
    ensure_ignore_file(&held)?;
    ensure_ignored(worktree, &format!("{CONTEXT_DIR_NAME}/{name}"))?;
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
/// entry of that name; an existing one, whatever it holds, is left as it is.
fn ensure_ignore_file(held: &HeldContextDir) -> io::Result<()> {
    if held.has_entry(IGNORE_FILE) {
        return Ok(());
    }
    let mut file = match held.create_new(IGNORE_FILE) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
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
/// (`check-ignore -q`'s exit status: 0 ignored, 1 not) and not tracked
/// (`ls-files --error-unmatch` fails).
fn ensure_ignored(worktree: &Path, relative: &str) -> io::Result<()> {
    let ignored = crate::git_env::git_at(worktree)
        .args(["check-ignore", "-q", "--", relative])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
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
    let tracked = crate::git_env::git_at(worktree)
        .args(["ls-files", "--error-unmatch", "--", relative])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if tracked.success() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{relative} is tracked by git in this worktree"),
        ));
    }
    Ok(())
}

/// [`save`] off the async runtime, logging a failure instead of answering
/// it: a dispatch whose task could not be saved still runs, it just cannot be
/// handed its task back after a restart.
pub async fn save_or_warn(worktree: &Path, work: Option<&WorkId>, task: &str) {
    let Some(work) = work.cloned() else {
        return;
    };
    let (dir, text) = (worktree.to_path_buf(), task.to_string());
    let saved =
        tokio::task::spawn_blocking(move || save(&dir, &work, &text).map_err(|e| (work, e))).await;
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
}
