//! A command an agent runs that blocks until the test lets it go, with no
//! signal anywhere (PRD #1790 stage 2, verification R1/A1 and R4).
//!
//! [`FifoHold::create`] makes a FIFO at an absolute path inside the run's own
//! test root and opens it for reading and writing. While that endpoint is
//! open, `cat <path>` opens the FIFO without waiting and then waits in its
//! read, since a writer exists and nothing is written. Dropping the hold
//! unlinks the path and closes the endpoint: the last writer goes, so every
//! `cat` reading it reads end-of-file and exits by itself, and a `cat` started
//! after the unlink finds no file and exits too. The one opener this does not
//! release is one that resolved the path before the unlink and reaches the
//! FIFO's open only after the close; that needs an agent starting its command
//! in the instant the test tears down.
//!
//! The endpoint is opened close-on-exec (the standard library's default), so
//! no process the test spawns afterwards (a daemon, a TUI, the CLI) inherits a
//! writer that would keep a reader waiting after the drop.
//!
//! Which process holds the FIFO is decided from its exact path, which is
//! unique to the run: another run's FIFO with the same file name, or a relative
//! spelling of it, is not this one ([`FifoHold::held_by`]). Nothing here keeps
//! or acts on a process id, so a reused pid cannot be mistaken for the hold.

#![cfg(unix)]

use std::fs::File;
use std::path::{Path, PathBuf};

/// A FIFO this run created and holds open; dropping it releases every reader.
pub struct FifoHold {
    path: PathBuf,
    endpoint: Option<File>,
}

impl FifoHold {
    /// Create the FIFO at `path`, which must be absolute and free of
    /// whitespace (an agent is handed it unquoted, and `ps` output is split on
    /// spaces), and must not exist yet.
    pub fn create(path: &Path) -> Self {
        assert!(
            path.is_absolute(),
            "the hold's path is absolute: {}",
            path.display()
        );
        assert!(
            !path.to_string_lossy().chars().any(char::is_whitespace),
            "the hold's path has no whitespace: {}",
            path.display()
        );
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .expect("a path without NUL");
        // SAFETY: mkfifo(3) on a NUL-terminated path the caller owns.
        let made = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(
            made,
            0,
            "create the hold FIFO {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
        // Read and write: an open for writing alone would wait for a reader.
        let endpoint = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap_or_else(|e| panic!("open the hold FIFO {}: {e}", path.display()));
        Self {
            path: path.to_path_buf(),
            endpoint: Some(endpoint),
        }
    }

    /// The FIFO's absolute path, as the agent is told to read it.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether `argv` is a `cat` reading exactly this FIFO: the program's
    /// file name is `cat` and one argument is this run's absolute path, not
    /// merely one ending in the same file name.
    pub fn held_by(&self, argv: &[String]) -> bool {
        argv.first()
            .is_some_and(|program| Path::new(program).file_name().is_some_and(|n| n == "cat"))
            && argv.iter().skip(1).any(|arg| Path::new(arg) == self.path)
    }
}

impl Drop for FifoHold {
    fn drop(&mut self) {
        // Unlink first, so nothing opens the FIFO again, then close the last
        // writer, which ends every reader's wait with end-of-file.
        let _ = std::fs::remove_file(&self.path);
        drop(self.endpoint.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
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
        assert!(!first.held_by(&argv(&["cat"])));
        assert!(!first.held_by(&[]));
    }

    /// A `cat` of the hold waits while the hold lives and exits successfully,
    /// with no signal, once the hold is dropped; the FIFO is gone afterwards.
    #[test]
    fn fifo_hold_release_ends_its_reader_with_end_of_file() {
        let root = common::harness_tempdir().expect("the run's root");
        let hold = FifoHold::create(&root.path().join("release-under-test.fifo"));
        let path = hold.path().to_path_buf();
        let mut reader = Command::new("cat")
            .arg(&path)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn cat");
        // One line through the hold's own endpoint: once `cat` echoes it, it
        // has the FIFO open and is waiting in its next read.
        let mut feed: &File = hold.endpoint.as_ref().expect("held");
        feed.write_all(b"ready\n").expect("feed the FIFO");
        let mut line = String::new();
        let mut out = BufReader::new(reader.stdout.take().expect("cat's stdout"));
        out.read_line(&mut line).expect("read cat's echo");
        assert_eq!(line, "ready\n");
        assert!(
            reader.try_wait().expect("poll cat").is_none(),
            "cat is still waiting while the hold lives"
        );

        drop(hold);
        let reader = std::cell::RefCell::new(reader);
        let status = std::cell::Cell::new(None);
        assert!(
            common::wait_until(common::load_scaled(Duration::from_secs(30)), || {
                status.set(reader.borrow_mut().try_wait().expect("poll cat"));
                status.get().is_some()
            }),
            "cat did not exit after the hold was dropped"
        );
        let status = status.get().expect("cat exited");
        assert!(
            status.success(),
            "cat read end-of-file and exited cleanly: {status}"
        );
        assert!(!path.exists(), "the drop removed the FIFO");
    }
}
