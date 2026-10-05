//! The build lock: one run at a time on a build clone and a target dir
//! (issue #1530).
//!
//! Two runs on the same `--source-clone` used to interleave freely: one run's
//! `git checkout --detach FETCH_HEAD` could land between the other's checkout
//! and its `cargo build`, and that other run then built — and reported on — a
//! commit it was never asked about. Measured on 2026-10-03: a run for one
//! branch built `3715d563`, another unit's branch. A shared `--target-dir` has
//! the same hole one step later: Cargo serialises the two builds itself, but
//! the second overwrites `debug/dot-agent-deck` before the first has staged it
//! into its sandbox.
//!
//! So a run takes an exclusive `flock(2)` on a lock file beside each of the
//! two directories before its first `git` command in the clone, and keeps it
//! until the branch binary has been copied into its sandbox. An OS lock rather
//! than a PID file because the kernel releases it when the holder dies, however
//! it dies: a `SIGKILL`ed run cannot leave a stale lock behind for the next one
//! to argue with.
//!
//! The lock file sits BESIDE the directory (`<dir>.xver-lock`), never inside
//! it. Inside the target dir it would be writable by the build, which runs
//! with that dir bound read-write: a build that deleted it would leave the
//! next run to create and lock a fresh inode while this one still held the
//! old. Beside it, it is under the masked home at the default paths and
//! read-only anywhere else a namespace can see it; and a process inside a
//! namespace cannot outlive the namespace to hold a lock past the build.
//!
//! The file names its holder: after taking the lock, a run writes one line —
//! its pid, branch, direction, start time and both paths — so a run that has
//! to wait can say whom it is waiting for. The line is information, not the
//! lock: a run that finds the file but not the lock simply takes it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The suffix of the lock file beside each locked directory.
pub const LOCK_SUFFIX: &str = ".xver-lock";

/// How often a waiting run retries the lock.
pub const POLL: Duration = Duration::from_millis(500);

/// One directory a run needs to itself, and the flag that moves it.
pub struct Need<'a> {
    /// What the directory is, for messages: "build clone", "target dir".
    pub what: &'a str,
    /// The option that gives a run its own: `--source-clone`, `--target-dir`.
    pub flag: &'a str,
    pub dir: &'a Path,
}

/// The locks a run holds. Dropping it clears the holder line from each lock
/// file and releases every one of them.
#[derive(Debug)]
pub struct BuildLock {
    held: Vec<(PathBuf, File)>,
    /// How long acquiring them took.
    pub waited: Duration,
    /// The holder line of each run this one found in its way, however briefly;
    /// empty when no other run held either lock.
    pub waited_for: Vec<String>,
}

impl BuildLock {
    /// The lock files held, in acquisition order.
    pub fn files(&self) -> Vec<&Path> {
        self.held.iter().map(|(p, _)| p.as_path()).collect()
    }

    /// Whether every lock file is still the file this run locked.
    ///
    /// A lock lives on an inode, not on a name: a lock file deleted while a
    /// run holds it lets the next run create a fresh one at the same path and
    /// lock that, so both use the directory at once. Nothing a run can do with
    /// `flock` prevents that, so the holder checks before it lets go, and a
    /// lock that was removed or replaced under it fails the run.
    pub fn still_held(&self) -> Result<(), String> {
        for (path, file) in &self.held {
            if !names_inode(path, file) {
                return Err(format!(
                    "the build lock {} was removed or replaced while this run held it, so \
                     another run could have locked a new file at that path and used the same \
                     directory at the same time. Do not delete `*{LOCK_SUFFIX}` files while a \
                     `cargo xver` run is going",
                    path.display()
                ));
            }
        }
        Ok(())
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        // Clear the holder line while the lock is still held, so a run that
        // takes it next never finds this one's line and names it.
        for (_, file) in &mut self.held {
            let _ = file.set_len(0);
        }
        // Then release in the reverse of the order `acquire` took them. A
        // `Vec` drops front to back, which let a run waiting on the first lock
        // take it while this run still held the second — whose holder line is
        // already cleared above — and record a second wait, on "a run that has
        // not recorded itself yet", that was this run letting go
        // (`a_brief_wait_is_still_recorded_with_the_holder`, red under load).
        while self.held.pop().is_some() {}
    }
}

/// Whether `path` names the inode `file` has open.
fn names_inode(path: &Path, file: &File) -> bool {
    match (std::fs::symlink_metadata(path), file.metadata()) {
        (Ok(at_path), Ok(held)) => at_path.dev() == held.dev() && at_path.ino() == held.ino(),
        _ => false,
    }
}

/// The lock file for `dir`: `<dir>.xver-lock` beside it.
///
/// Resolved through the real directory, so two spellings of one clone — a
/// symlink to it, a path through a symlinked parent — share one lock file. A
/// directory that does not exist yet (a clone about to be created) resolves
/// through its parent, which is created if needed.
pub fn lock_path(dir: &Path) -> Result<PathBuf, String> {
    let real = match std::fs::canonicalize(dir) {
        Ok(real) => real,
        Err(_) => {
            let name = dir
                .file_name()
                .ok_or_else(|| format!("{} names no directory to lock beside", dir.display()))?;
            let parent = dir
                .parent()
                .ok_or_else(|| format!("{} has no parent", dir.display()))?;
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
            std::fs::canonicalize(parent)
                .map_err(|e| format!("canonicalize {}: {e}", parent.display()))?
                .join(name)
        }
    };
    let name = real
        .file_name()
        .ok_or_else(|| format!("{} names no directory to lock beside", real.display()))?;
    let mut name = name.to_os_string();
    name.push(LOCK_SUFFIX);
    Ok(real.with_file_name(name))
}

/// Open (creating if needed) a lock file this harness owns: a regular file,
/// owned by this uid, never reached through a symlink. A symlink planted at the
/// path would otherwise have the holder line written through it.
fn open_lock_file(path: &Path) -> Result<File, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| {
            format!(
                "open the build lock {}: {e} — it must be a regular file this harness created, \
                 not a symlink",
                path.display()
            )
        })?;
    let md = file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?;
    if !md.file_type().is_file() {
        return Err(format!(
            "the build lock {} is not a regular file",
            path.display()
        ));
    }
    let uid = crate::sandbox::current_uid();
    if md.uid() != uid {
        return Err(format!(
            "the build lock {} is owned by uid {}, not this uid {uid}; refusing a lock file this \
             harness did not create",
            path.display(),
            md.uid()
        ));
    }
    Ok(file)
}

/// `flock(LOCK_EX | LOCK_NB)`: `Ok(true)` when taken, `Ok(false)` when another
/// open file description holds it.
fn try_lock_exclusive(file: &File) -> Result<bool, String> {
    // SAFETY: `flock` on a descriptor this `File` owns for its whole lifetime.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(false);
    }
    Err(format!("flock: {err}"))
}

/// The holder line the current holder wrote.
///
/// The line can be missing or out of date: the holder may be between taking
/// the lock and writing it, or the line may be a run's that died holding the
/// lock (a run that exits cleanly clears it). A line whose pid is no longer
/// running is reported as that, never as the holder.
fn read_holder(file: &mut File) -> String {
    const UNRECORDED: &str = "a run that has not recorded itself yet";
    let mut s = String::new();
    let ok = file.seek(SeekFrom::Start(0)).is_ok() && file.read_to_string(&mut s).is_ok();
    let s = s.trim();
    if !ok || s.is_empty() {
        return UNRECORDED.to_string();
    }
    let pid = s
        .strip_prefix("pid ")
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse::<libc::pid_t>().ok());
    // SAFETY: signal 0 only checks that the pid exists; nothing is sent.
    let exited = pid.is_some_and(|pid| {
        pid > 0
            && unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    });
    if exited {
        format!(
            "{UNRECORDED}; the last line recorded there, `{s}`, names a process that has exited"
        )
    } else {
        s.to_string()
    }
}

fn write_holder(file: &mut File, holder: &str) -> Result<(), String> {
    file.set_len(0)
        .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
        .and_then(|()| file.write_all(format!("{holder}\n").as_bytes()))
        .and_then(|()| file.flush())
        .map_err(|e| format!("record the build lock's holder: {e}"))
}

/// Take an exclusive lock on every directory in `needs`, in order, waiting up
/// to `wait` in total for another run to release one.
///
/// `holder` is the line this run records in each lock file once it holds it.
/// `on_wait` is called once per directory this run has to wait for, with a
/// message naming the run that holds it; a `wait` of zero refuses at once.
/// When the wait runs out the error names the holder and the flag that gives
/// this run its own directory instead.
///
/// The order is fixed by the caller — the clone, then the target dir — so two
/// runs never hold one each while waiting for the other's.
pub fn acquire(
    needs: &[Need<'_>],
    holder: &str,
    wait: Duration,
    poll: Duration,
    mut on_wait: impl FnMut(&str),
) -> Result<BuildLock, String> {
    let started = Instant::now();
    let deadline = started + wait;
    let mut paths: Vec<PathBuf> = Vec::new();
    for need in needs {
        let path = lock_path(need.dir)?;
        if let Some(j) = paths.iter().position(|p| *p == path) {
            // The run would wait on its own lock for the whole of `wait`.
            return Err(format!(
                "the {} {} and the {} {} are the same directory; give them different paths",
                needs[j].what,
                needs[j].dir.display(),
                need.what,
                need.dir.display()
            ));
        }
        paths.push(path);
    }
    let mut held = Vec::new();
    let mut waited_for = Vec::new();
    for (need, path) in needs.iter().zip(paths) {
        let mut file = open_lock_file(&path)?;
        let mut announced = false;
        loop {
            if try_lock_exclusive(&file)? {
                if names_inode(&path, &file) {
                    break;
                }
                // The file was removed or replaced between the open and the
                // lock — a holder's lock on a name it no longer has. Lock the
                // file that is there now instead.
                file = open_lock_file(&path)?;
                continue;
            }
            let other = read_holder(&mut file);
            let advice = format!(
                "Give each concurrent run its own --source-clone and --target-dir (a lane reused \
                 across branches, such as `<repo parent>/dot-agent-deck-xver-src-a` and \
                 `-xver-target-a`, keeps the build warm); this run needs its own {}",
                need.flag
            );
            if Instant::now() >= deadline {
                return Err(format!(
                    "the {} {} is in use by another `cargo xver` run ({other}){}. Refusing rather \
                     than building on top of it: the other run's checkout or build would decide \
                     which commit this one tested. {advice}, or raise --lock-wait-secs. Lock: {}",
                    need.what,
                    need.dir.display(),
                    if wait.is_zero() {
                        String::new()
                    } else {
                        format!(" and was still held after waiting {}s", wait.as_secs())
                    },
                    path.display()
                ));
            }
            if !announced {
                announced = true;
                waited_for.push(other.clone());
                on_wait(&format!(
                    "the {} {} is in use by another `cargo xver` run ({other}); waiting up to {}s \
                     for it (--lock-wait-secs). {advice}.",
                    need.what,
                    need.dir.display(),
                    deadline.saturating_duration_since(Instant::now()).as_secs()
                ));
            }
            std::thread::sleep(poll.min(deadline.saturating_duration_since(Instant::now())));
        }
        write_holder(&mut file, holder)?;
        held.push((path, file));
    }
    Ok(BuildLock {
        held,
        waited: started.elapsed(),
        waited_for,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::process::{Command, Stdio};

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xver-lock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch dir");
        std::fs::canonicalize(&d).expect("canonicalize scratch dir")
    }

    fn needs<'a>(clone: &'a Path, target: &'a Path) -> [Need<'a>; 2] {
        [
            Need {
                what: "build clone",
                flag: "--source-clone",
                dir: clone,
            },
            Need {
                what: "target dir",
                flag: "--target-dir",
                dir: target,
            },
        ]
    }

    fn take(clone: &Path, target: &Path, who: &str, wait: Duration) -> Result<BuildLock, String> {
        acquire(
            &needs(clone, target),
            who,
            wait,
            Duration::from_millis(50),
            |_| {},
        )
    }

    /// The reported defect, at the seam where it happens: a second run on the
    /// same default clone and target dir must not get them while a first run
    /// holds them. Before the lock, nothing refused, so both "had" the clone at
    /// once and either one's checkout could decide what the other built.
    #[test]
    fn a_second_run_on_the_same_clone_and_target_dir_is_refused_while_the_first_holds_them() {
        let s = scratch("contend");
        let clone = s.join("dot-agent-deck-xver-src");
        let target = s.join("dot-agent-deck-xver-target");
        let first = take(&clone, &target, "pid 1 branch `a`", Duration::ZERO)
            .expect("the first run takes the lock");
        let second = take(&clone, &target, "pid 2 branch `b`", Duration::ZERO);
        let err = second.expect_err("a second run must not hold the clone at the same time");
        assert!(err.contains("build clone"), "{err}");
        assert!(
            err.contains("pid 1 branch `a`"),
            "names the run holding it: {err}"
        );
        assert!(
            err.contains("--source-clone") && err.contains("--target-dir"),
            "{err}"
        );
        drop(first);
        take(&clone, &target, "pid 2 branch `b`", Duration::ZERO)
            .expect("free again once the first run released it");
        let _ = std::fs::remove_dir_all(&s);
    }

    /// The control: two runs on lanes of their own never wait for each other.
    #[test]
    fn runs_on_their_own_clone_and_target_dir_do_not_contend() {
        let s = scratch("lanes");
        let a = take(&s.join("src-a"), &s.join("target-a"), "a", Duration::ZERO).expect("lane a");
        let b = take(&s.join("src-b"), &s.join("target-b"), "b", Duration::ZERO)
            .expect("lane b runs beside lane a");
        assert_eq!(a.files().len(), 2);
        assert_eq!(b.files().len(), 2);
        let _ = std::fs::remove_dir_all(&s);
    }

    /// A private clone is not enough: a shared target dir alone is contended,
    /// because the second build overwrites the binary before the first run has
    /// staged it.
    #[test]
    fn a_shared_target_dir_alone_is_contended() {
        let s = scratch("target");
        let target = s.join("target");
        let _a = take(&s.join("src-a"), &target, "run a", Duration::ZERO).expect("first");
        let err = take(&s.join("src-b"), &target, "run b", Duration::ZERO)
            .expect_err("the target dir is shared");
        assert!(err.contains("target dir"), "{err}");
        assert!(err.contains("run a"), "{err}");
        assert!(err.contains("its own --target-dir"), "{err}");
        let _ = std::fs::remove_dir_all(&s);
    }

    /// The default is to wait, bounded, with a message naming the holder — and
    /// a waiting run proceeds as soon as the holder lets go.
    #[test]
    fn a_waiting_run_says_whom_it_waits_for_and_proceeds_when_the_holder_releases() {
        let s = scratch("wait");
        let clone = s.join("src");
        let target = s.join("target");
        let first = take(&clone, &target, "pid 7 branch `first`", Duration::ZERO).expect("first");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(first);
        });
        let mut said = Vec::new();
        let second = acquire(
            &needs(&clone, &target),
            "second",
            Duration::from_secs(30),
            Duration::from_millis(20),
            |m| said.push(m.to_string()),
        )
        .expect("the second run gets the lock once the first releases it");
        releaser.join().unwrap();
        assert_eq!(said.len(), 1, "one message for the one wait: {said:?}");
        assert!(said[0].contains("pid 7 branch `first`"), "{}", said[0]);
        assert!(said[0].contains("waiting up to"), "{}", said[0]);
        assert!(second.waited >= Duration::from_millis(200));
        let _ = std::fs::remove_dir_all(&s);
    }

    /// The wait is bounded: when the holder keeps the lock, the waiting run
    /// gives up with the refusal, not a hang.
    #[test]
    fn a_bounded_wait_ends_in_a_refusal_naming_the_holder() {
        let s = scratch("bounded");
        let clone = s.join("src");
        let target = s.join("target");
        let _first = take(&clone, &target, "pid 9 branch `slow`", Duration::ZERO).expect("first");
        let started = Instant::now();
        let err = take(&clone, &target, "second", Duration::from_millis(400))
            .expect_err("the holder never released");
        assert!(started.elapsed() >= Duration::from_millis(400));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(err.contains("still held after waiting"), "{err}");
        assert!(err.contains("pid 9 branch `slow`"), "{err}");
        let _ = std::fs::remove_dir_all(&s);
    }

    /// Two spellings of one clone — the directory and a symlink to it — are one
    /// lock, so a symlinked `--source-clone` cannot sidestep it.
    #[test]
    fn two_spellings_of_one_clone_share_one_lock() {
        let s = scratch("spelling");
        let real = s.join("real-src");
        std::fs::create_dir_all(&real).unwrap();
        let link = s.join("link-src");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(lock_path(&real).unwrap(), lock_path(&link).unwrap());
        let _a = take(&real, &s.join("ta"), "via the real path", Duration::ZERO).expect("first");
        let err = take(&link, &s.join("tb"), "via the link", Duration::ZERO)
            .expect_err("the same clone through a symlink is the same lock");
        assert!(err.contains("via the real path"), "{err}");
        let _ = std::fs::remove_dir_all(&s);
    }

    /// A symlink planted at the lock path is refused, and nothing is written
    /// through it.
    #[test]
    fn a_symlink_at_the_lock_path_is_refused_and_not_written_through() {
        let s = scratch("planted");
        let victim = s.join("victim");
        std::fs::write(&victim, "untouched").unwrap();
        let clone = s.join("src");
        std::os::unix::fs::symlink(&victim, lock_path(&clone).unwrap()).unwrap();
        let err = take(&clone, &s.join("target"), "x", Duration::ZERO)
            .expect_err("a symlinked lock file is refused");
        assert!(err.contains("not a symlink"), "{err}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
        let _ = std::fs::remove_dir_all(&s);
    }

    /// Greptile on #1533: one directory given as both would wait on its own
    /// lock for the whole bound before refusing.
    #[test]
    fn the_same_directory_as_clone_and_target_is_refused_at_once() {
        let s = scratch("same");
        let d = s.join("both");
        let started = Instant::now();
        let err = take(&d, &d, "x", Duration::from_secs(30)).expect_err("one directory, two roles");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "refused, not waited"
        );
        assert!(err.contains("same directory"), "{err}");
        let _ = std::fs::remove_dir_all(&s);
    }

    /// Qodo on #1533: a wait shorter than a second is still contention, and
    /// the run records whom it waited for.
    #[test]
    fn a_brief_wait_is_still_recorded_with_the_holder() {
        let s = scratch("brief");
        let (clone, target) = (s.join("src"), s.join("target"));
        let first = take(&clone, &target, "pid 1 branch `quick`", Duration::ZERO).expect("first");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(first);
        });
        let second = take(&clone, &target, "second", Duration::from_secs(30)).expect("second");
        releaser.join().unwrap();
        assert_eq!(second.waited_for, vec!["pid 1 branch `quick`".to_string()]);
        let _ = std::fs::remove_dir_all(&s);
    }

    /// Qodo on #1533: a lock is on an inode, so a lock file deleted while held
    /// lets a second run lock a new file at the same path. The holder catches
    /// it before it lets go.
    #[test]
    fn a_lock_file_removed_while_held_is_caught_by_the_holder() {
        let s = scratch("removed");
        let (clone, target) = (s.join("src"), s.join("target"));
        let first = take(&clone, &target, "first", Duration::ZERO).expect("first");
        first.still_held().expect("nothing removed yet");
        std::fs::remove_file(lock_path(&clone).unwrap()).unwrap();
        let _second = take(
            &clone,
            &target.with_file_name("other-target"),
            "second",
            Duration::ZERO,
        )
        .expect("a new file at the path is a new lock — the hole this check exists for");
        let err = first
            .still_held()
            .expect_err("the holder sees its lock file was replaced");
        assert!(err.contains("removed or replaced"), "{err}");
        let _ = std::fs::remove_dir_all(&s);
    }

    /// Qodo on #1533: a run that releases clears its line, and a line left by
    /// a run that died is never reported as the current holder.
    #[test]
    fn a_released_or_dead_holder_is_never_named_as_the_holder() {
        let s = scratch("stale");
        let (clone, target) = (s.join("src"), s.join("target"));
        drop(take(&clone, &target, "pid 1 branch `gone`", Duration::ZERO).expect("first"));
        let path = lock_path(&clone).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "",
            "cleared on release"
        );

        let mut dead = Command::new("true").spawn().expect("spawn true");
        let dead_pid = dead.id();
        dead.wait().unwrap();
        std::fs::write(&path, format!("pid {dead_pid}, branch `crashed`\n")).unwrap();
        // Hold it through a second open file description, as a run that has
        // not yet recorded itself would.
        let holding = open_lock_file(&path).unwrap();
        assert!(try_lock_exclusive(&holding).unwrap());
        let err = take(&clone, &target, "waiter", Duration::ZERO).expect_err("held");
        assert!(err.contains("has not recorded itself yet"), "{err}");
        assert!(err.contains("names a process that has exited"), "{err}");
        let _ = std::fs::remove_dir_all(&s);
    }

    const CHILD_ENV: &str = "XVER_BUILD_LOCK_CHILD_DIR";

    /// Not a test of its own: the child process for
    /// [`a_run_that_dies_releases_its_lock`]. Without the variable it returns
    /// at once; with it, it takes the lock, says so, and sleeps until killed.
    #[test]
    fn build_lock_holder_child() {
        let Some(dir) = std::env::var_os(CHILD_ENV) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let _held = take(
            &dir.join("src"),
            &dir.join("target"),
            &format!("pid {} branch `doomed`", std::process::id()),
            Duration::ZERO,
        )
        .expect("the child takes the lock");
        println!("XVER-LOCK-HELD");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    /// The reason for an OS lock over a PID file: a run killed with `SIGKILL`,
    /// which runs no cleanup at all, leaves nothing for the next run to argue
    /// with. The child is this test binary re-executed, holding the lock in a
    /// process of its own.
    #[test]
    fn a_run_that_dies_releases_its_lock() {
        let s = scratch("dies");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "buildlock::tests::build_lock_holder_child",
                "--nocapture",
            ])
            .env(CHILD_ENV, &s)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("re-exec the test binary");
        let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let held = lines.any(|l| l.map(|l| l.contains("XVER-LOCK-HELD")).unwrap_or(false));
        assert!(held, "the child never reported holding the lock");
        let err = take(
            &s.join("src"),
            &s.join("target"),
            "survivor",
            Duration::ZERO,
        )
        .expect_err("held by the child");
        assert!(
            err.contains(&format!("pid {} branch `doomed`", child.id())),
            "names the child: {err}"
        );
        // SAFETY: signalling the child this test spawned, by its own pid.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
        child.wait().unwrap();
        take(
            &s.join("src"),
            &s.join("target"),
            "survivor",
            Duration::ZERO,
        )
        .expect("the kernel released the dead run's lock");
        let _ = std::fs::remove_dir_all(&s);
    }
}
