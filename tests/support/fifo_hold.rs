//! A command an agent runs that blocks until the test lets it go, with no
//! signal from the test anywhere (PRD #1790 stage 2, verification R1/A1, R4,
//! and verify-8 A2/A3).
//!
//! [`FifoHold::create`] makes a FIFO at an absolute path inside the run's own
//! test root and opens it for reading and writing. The agent is handed
//! [`FifoHold::command`], `perl -e '<BOUND_SCRIPT>' <bound> cat '<path>'`
//! ([`BOUND_SCRIPT`]). While the hold's
//! endpoint is open, that `cat` opens the FIFO without waiting and then waits
//! in its read, since a writer exists and nothing is written.
//!
//! Dropping the hold unlinks the path and then closes the endpoint. What
//! happens to a reader then depends on where it is:
//!
//! - one that has the FIFO open reads end-of-file, because the last writer
//!   went, and exits successfully at once;
//! - one that looks the path up after the unlink finds no file and exits;
//! - one that resolved the path before the unlink and reaches the FIFO's
//!   open only after the close waits in that open for a writer that never
//!   comes. Nothing the drop does can reach it, so the command itself carries
//!   the bound: the `perl` it runs under ends it once `bound` has passed since
//!   it started.
//!
//! So every reader of the command ends within `bound` of starting, and almost
//! always at the drop. The test signals no process; the bounding `perl` is
//! part of the command the agent runs, and stops only its own child.
//! `fifo_hold_bounds_a_reader_that_opens_after_the_release` pins the third
//! case and `fifo_hold_release_ends_its_reader_with_end_of_file` the first.
//! The bound needs only `perl` on the agent's PATH, which a stock macOS and
//! the Linux images here both ship; GNU `timeout`, which a stock macOS lacks,
//! is not used.
//!
//! The endpoint is opened close-on-exec (the standard library's default), so
//! no process the test spawns afterwards (a daemon, a TUI, the CLI) inherits a
//! writer that would keep a reader waiting after the drop.
//!
//! The path reaches a shell as one single-quoted word ([`shell_quote`]), so a
//! test root whose spelling has `$`, quotes or `;` in it is read back exactly.
//! Which process holds the FIFO is decided from its exact path, which is
//! unique to the run: another run's FIFO with the same file name, or a relative
//! spelling of it, is not this one ([`FifoHold::held_by`]). Nothing here keeps
//! or acts on a process id, so a reused pid cannot be mistaken for the hold.

#![cfg(unix)]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `word` as one POSIX shell word that the shell reads back unchanged: the
/// whole of it in single quotes, each `'` in it closed, escaped and reopened.
pub fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The Perl program that bounds the held `cat`, run as
/// `perl -e BOUND_SCRIPT <secs> <program> <args>…`: a portable stand-in for
/// GNU `timeout`. It forks, the child execs the program, and the parent waits
/// for it. If `secs` pass first, its own alarm sends the child `SIGTERM`, reaps
/// it and exits 124, as `timeout` does; otherwise it exits with the child's
/// status (128 plus the signal for a child a signal ended), 127 when the
/// program cannot be run and 125 when it cannot fork. Because it forks rather
/// than execs, the `cat` runs as its child with this argv as its parent's,
/// which is how [`FifoHold::bounded_by`] sees the bound is in place. It has no
/// whitespace, so a whitespace split of `ps` output keeps it one word, and no
/// `'`, so one pair of single quotes carries it through a shell.
pub const BOUND_SCRIPT: &str = "$s=shift;defined($p=fork)||exit(125);$p||exec(@ARGV)||exit(127);\
$SIG{ALRM}=sub{kill(\"TERM\",$p);waitpid($p,0);exit(124)};alarm($s);waitpid($p,0);\
exit($?&127?128+($?&127):$?>>8)";

/// The command that reads the FIFO at `path` until its writer goes, and in
/// any case for no longer than `bound` (whole seconds, at least one).
pub fn held_command(path: &Path, bound: Duration) -> String {
    let path = path
        .to_str()
        .unwrap_or_else(|| panic!("the hold's path is UTF-8: {}", path.display()));
    format!(
        "perl -e {} {} cat {}",
        shell_quote(BOUND_SCRIPT),
        bound.as_secs().max(1),
        shell_quote(path)
    )
}

/// A FIFO this run created and holds open; dropping it releases every reader.
pub struct FifoHold {
    path: PathBuf,
    endpoint: Option<File>,
}

impl FifoHold {
    /// Create the FIFO at `path`, which must be absolute, UTF-8, free of
    /// whitespace (`ps` output is split on it) and of backticks (the agent is
    /// handed the command inside a Markdown code span), and must not exist
    /// yet.
    pub fn create(path: &Path) -> Self {
        assert!(
            path.is_absolute(),
            "the hold's path is absolute: {}",
            path.display()
        );
        let spelling = path
            .to_str()
            .unwrap_or_else(|| panic!("the hold's path is UTF-8: {}", path.display()));
        assert!(
            !spelling.chars().any(char::is_whitespace),
            "the hold's path has no whitespace: {spelling}"
        );
        assert!(
            !spelling.contains('`'),
            "the hold's path has no backtick: {spelling}"
        );
        let c_path = std::ffi::CString::new(spelling).expect("a path without NUL");
        // SAFETY: mkfifo(3) on a NUL-terminated path the caller owns.
        let made = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(
            made,
            0,
            "create the hold FIFO {spelling}: {}",
            std::io::Error::last_os_error()
        );
        // Read and write: an open for writing alone would wait for a reader.
        let endpoint = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap_or_else(|e| panic!("open the hold FIFO {spelling}: {e}"));
        Self {
            path: path.to_path_buf(),
            endpoint: Some(endpoint),
        }
    }

    /// The FIFO's absolute path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The command the agent is told to run: a `cat` of this FIFO that ends
    /// at the drop, and in any case once `bound` has passed.
    pub fn command(&self, bound: Duration) -> String {
        held_command(&self.path, bound)
    }

    /// Whether `argv` is a `cat` reading exactly this FIFO: the program's
    /// file name is `cat` and one argument is this run's absolute path, not
    /// merely one ending in the same file name.
    pub fn held_by(&self, argv: &[String]) -> bool {
        argv.first()
            .is_some_and(|program| Path::new(program).file_name().is_some_and(|n| n == "cat"))
            && argv.iter().skip(1).any(|arg| Path::new(arg) == self.path)
    }

    /// Whether `argv` is the bounding `perl` of [`FifoHold::command`] with
    /// `bound`: the process a held `cat` runs under when its bound is in
    /// place. The program's file name is `perl`, or a versioned `perl5…` (a
    /// stock macOS's `/usr/bin/perl` hands over to one), and it runs exactly
    /// [`BOUND_SCRIPT`].
    pub fn bounded_by(&self, argv: &[String], bound: Duration) -> bool {
        match argv {
            [program, flag, script, secs, reader @ ..] => {
                Path::new(program)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == "perl" || n.starts_with("perl5"))
                    && flag == "-e"
                    && script == BOUND_SCRIPT
                    && *secs == bound.as_secs().max(1).to_string()
                    && self.held_by(reader)
            }
            _ => false,
        }
    }
}

impl Drop for FifoHold {
    fn drop(&mut self) {
        // Unlink first, so nothing opens the FIFO again, then close the last
        // writer, which ends every open reader's wait with end-of-file. A
        // reader still on its way into the open is the command's bound's.
        let _ = std::fs::remove_file(&self.path);
        drop(self.endpoint.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, Command, ExitStatus, Stdio};

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    /// Every process now running, as `(pid, parent pid, argv)` from `ps`,
    /// split on whitespace as `tests/e2e_interrupted_work_live.rs` splits it.
    fn processes() -> Vec<(u32, u32, Vec<String>)> {
        let out = Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,args="])
            .output()
            .expect("run ps");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                let pid = words.next()?.parse().ok()?;
                let ppid = words.next()?.parse().ok()?;
                Some((pid, ppid, words.map(str::to_string).collect()))
            })
            .collect()
    }

    /// Wait, bounded, for `child` to exit, and return its status.
    fn exit_within(child: &mut Child, bound: Duration, what: &str) -> ExitStatus {
        let status = std::cell::Cell::new(None);
        let child = std::cell::RefCell::new(child);
        assert!(
            common::wait_until(bound, || {
                status.set(child.borrow_mut().try_wait().expect("poll the reader"));
                status.get().is_some()
            }),
            "{what} did not exit within {}s",
            bound.as_secs()
        );
        status.get().expect("exited")
    }

    /// Two runs, each with its own root and a FIFO of the same file name: each
    /// hold recognises only a `cat` of its own absolute path.
    #[test]
    fn fifo_hold_recognises_only_its_own_runs_fifo() {
        let first_root = common::harness_tempdir().expect("first run's root");
        let second_root = common::harness_tempdir().expect("second run's root");
        let name = "hold-under-test.fifo";
        let first = FifoHold::create(&first_root.path().join(name));
        let second = FifoHold::create(&second_root.path().join(name));
        let first_path = first.path().to_string_lossy().into_owned();
        let second_path = second.path().to_string_lossy().into_owned();

        assert!(first.held_by(&argv(&["cat", &first_path])));
        assert!(first.held_by(&argv(&["/usr/bin/cat", &first_path])));
        assert!(second.held_by(&argv(&["cat", &second_path])));

        assert!(
            !first.held_by(&argv(&["cat", &second_path])),
            "another run's FIFO of the same name is not this run's"
        );
        assert!(!second.held_by(&argv(&["cat", &first_path])));
        assert!(
            !first.held_by(&argv(&["cat", &format!(".dot-agent-deck/{name}")])),
            "a relative path ending in the file name is not this run's FIFO"
        );
        assert!(
            !first.held_by(&argv(&["tail", &first_path])),
            "only `cat` is the held command"
        );
        let perl = |program: &str, secs: &str, path: &str| {
            argv(&[program, "-e", BOUND_SCRIPT, secs, "cat", path])
        };
        assert!(
            !first.held_by(&perl("perl", "1200", &first_path)),
            "the bounding `perl` is not the reader; its `cat` is"
        );
        assert!(!first.held_by(&argv(&["cat"])));
        assert!(!first.held_by(&[]));

        let bound = Duration::from_secs(1200);
        assert!(first.bounded_by(&perl("perl", "1200", &first_path), bound));
        assert!(first.bounded_by(&perl("/usr/bin/perl", "1200", &first_path), bound));
        assert!(
            first.bounded_by(&perl("/usr/bin/perl5.34", "1200", &first_path), bound),
            "a stock macOS's versioned perl is the bound too"
        );
        assert!(second.bounded_by(&perl("perl", "1200", &second_path), bound));
        assert!(
            !first.bounded_by(&perl("perl", "60", &first_path), bound),
            "another bound is not this command's"
        );
        assert!(
            !first.bounded_by(&perl("perl", "1200", &second_path), bound),
            "a bound on another run's FIFO is not this run's"
        );
        assert!(
            !first.bounded_by(
                &argv(&["perl", "-e", "exec@ARGV", "1200", "cat", &first_path]),
                bound
            ),
            "another Perl program is not the bound"
        );
        assert!(
            !first.bounded_by(&perl("python3", "1200", &first_path), bound),
            "only `perl` runs the bound"
        );
        assert!(
            !first.bounded_by(&argv(&["timeout", "1200", "cat", &first_path]), bound),
            "GNU `timeout` is not this command's bound"
        );
        assert!(!first.bounded_by(&argv(&["cat", &first_path]), bound));
        assert!(!first.bounded_by(&argv(&["perl", "-e", BOUND_SCRIPT, "1200"]), bound));
        assert!(!first.bounded_by(&[], bound));
    }

    /// Words with `$`, both quotes, `;`, a backslash, a backtick and an
    /// empty word each come back from `sh` exactly as they went in.
    #[test]
    fn fifo_hold_shell_quote_round_trips_through_sh() {
        for word in [
            "/var/tmp/base-$HOME/hold.fifo",
            "it's",
            "a'b''c'",
            "x;y&&z|w",
            "\"double\"",
            "back\\slash",
            "`tick`$(echo no)",
            "",
        ] {
            let out = Command::new("sh")
                .arg("-c")
                .arg(format!("printf '%s' {}", shell_quote(word)))
                .output()
                .expect("run sh");
            assert!(out.status.success(), "sh ran {word:?}: {}", out.status);
            assert_eq!(
                String::from_utf8(out.stdout).expect("UTF-8"),
                word,
                "sh read the quoted word back unchanged"
            );
        }
    }

    /// In a directory whose name has `$`, both quotes and `;`, the held
    /// command run by `sh` reads this FIFO (its echo of a line written to
    /// the hold proves the path arrived intact), shows in `ps` as a `cat` of
    /// it whose parent is its bound, waits while the hold lives,
    /// and exits successfully, with no signal and well inside its bound, once
    /// the hold is dropped; the FIFO is gone afterwards.
    #[test]
    fn fifo_hold_release_ends_its_reader_with_end_of_file() {
        let root = common::harness_tempdir().expect("the run's root");
        let base = root.path().join("base-$HOME-'q'-\"d\";x");
        std::fs::create_dir(&base).expect("the metacharacter directory");
        let hold = FifoHold::create(&base.join("release-under-test.fifo"));
        let path = hold.path().to_path_buf();
        // A bound far beyond this test's waits: an exit by it is a failure.
        let mut reader = Command::new("sh")
            .arg("-c")
            .arg(hold.command(Duration::from_secs(600)))
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn the held command");
        // One line through the hold's own endpoint: once `cat` echoes it, it
        // has the FIFO open and is waiting in its next read. The echo is read
        // on a thread so a broken handshake fails here, bounded; that thread
        // ends when the drop below ends `cat` and with it its stdout.
        let mut feed: &File = hold.endpoint.as_ref().expect("held");
        feed.write_all(b"ready\n").expect("feed the FIFO");
        let stdout = reader.stdout.take().expect("the reader's stdout");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let read = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(read.map(|_| line));
        });
        let echo_wait = common::load_scaled(Duration::from_secs(30));
        let line = rx
            .recv_timeout(echo_wait)
            .unwrap_or_else(|e| {
                panic!(
                    "the reader did not echo the line within {}s: {e}",
                    echo_wait.as_secs()
                )
            })
            .expect("read the reader's echo");
        assert_eq!(line, "ready\n");
        assert!(
            reader.try_wait().expect("poll the reader").is_none(),
            "the reader is still waiting while the hold lives"
        );
        // As `ps` shows it, split on whitespace: one `cat` of this FIFO, run
        // by the bounding `perl` with this bound.
        let all = processes();
        let held: Vec<_> = all
            .iter()
            .filter(|(_, _, argv)| hold.held_by(argv))
            .collect();
        assert_eq!(held.len(), 1, "one `cat` of this FIFO: {held:?}");
        let parent = all
            .iter()
            .find(|(pid, _, _)| *pid == held[0].1)
            .map(|(_, _, argv)| argv);
        assert!(
            parent.is_some_and(|argv| hold.bounded_by(argv, Duration::from_secs(600))),
            "the `cat` runs under its bound: its parent is {parent:?}"
        );

        drop(hold);
        let status = exit_within(
            &mut reader,
            common::load_scaled(Duration::from_secs(30)),
            "the reader, after the hold was dropped,",
        );
        assert!(
            status.success(),
            "cat read end-of-file and exited cleanly, not by its bound: {status}"
        );
        assert!(!path.exists(), "the drop removed the FIFO");
    }

    /// The opener the drop cannot reach: a reader that resolved the FIFO
    /// before the drop (here through a path-only descriptor it inherits) and
    /// opens it only after the drop, so no writer will ever come. The held
    /// command still ends, by its own bound (status 124, which
    /// also shows the open really waited rather than failing or reading
    /// end-of-file), with no signal from the test.
    #[cfg(target_os = "linux")]
    #[test]
    fn fifo_hold_bounds_a_reader_that_opens_after_the_release() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        let root = common::harness_tempdir().expect("the run's root");
        let hold = FifoHold::create(&root.path().join("late-opener.fifo"));
        // Resolve the FIFO now, as an opener does in its path lookup, without
        // opening it for reading or writing.
        let resolved = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH)
            .open(hold.path())
            .expect("resolve the FIFO with O_PATH");
        let fd = resolved.as_raw_fd();
        // Inherited by the reader, so it opens this inode as `/proc/self/fd`.
        // SAFETY: fcntl(2) on a descriptor this test owns.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, 0);

        drop(hold);
        let bound = Duration::from_secs(2);
        let started = std::time::Instant::now();
        let mut reader = Command::new("sh")
            .arg("-c")
            .arg(held_command(
                Path::new(&format!("/proc/self/fd/{fd}")),
                bound,
            ))
            .spawn()
            .expect("spawn the late reader");
        drop(resolved);
        let status = exit_within(
            &mut reader,
            common::load_scaled(Duration::from_secs(30)),
            "the reader that opened after the release",
        );
        assert_eq!(
            status.code(),
            Some(124),
            "the late opener waited in its open and was ended by the command's bound: {status}"
        );
        assert!(
            started.elapsed() >= bound,
            "the reader ended by its bound, not before it"
        );
    }
}
