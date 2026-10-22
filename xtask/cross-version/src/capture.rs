//! File publication for pane-local commands. The lease covers execution and
//! publication; an attempt token fences commands queued in a replaced shell.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

pub(crate) fn shell_word(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\"'\"'"))
}

fn path_word(path: &Path) -> String {
    shell_word(&path.to_string_lossy())
}

pub(crate) struct Session {
    root: PathBuf,
    name: String,
    lock: PathBuf,
    token: PathBuf,
}

pub(crate) struct Attempt {
    pub tmp: PathBuf,
    pub out: PathBuf,
    number: usize,
}

impl Session {
    pub fn new(root: &Path, name: &str) -> Self {
        Self {
            root: root.to_path_buf(),
            name: name.to_string(),
            lock: root.join(format!("{name}.lock")),
            token: root.join(format!("{name}.token")),
        }
    }

    fn lease(&self) -> Result<Option<File>, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock)
            .map_err(|e| format!("open capture lease: {e}"))?;
        // SAFETY: flock uses this live file descriptor and addresses no process.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            Ok(Some(file))
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(format!("lock capture lease: {error}"))
            }
        }
    }

    /// Remove abandoned temporary output and advance the token while holding
    /// the same lease as the command. A live command refuses replacement.
    pub fn prepare(&self, number: usize) -> Result<Option<Attempt>, String> {
        let Some(_lease) = self.lease()? else {
            return Ok(None);
        };
        for old in 1..number {
            let tmp = self.attempt(old).tmp;
            match std::fs::remove_file(&tmp) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("remove abandoned {}: {e}", tmp.display())),
            }
        }
        std::fs::write(&self.token, number.to_string())
            .map_err(|e| format!("advance capture token: {e}"))?;
        Ok(Some(self.attempt(number)))
    }

    fn attempt(&self, number: usize) -> Attempt {
        Attempt {
            tmp: self.root.join(format!("{}-{number}.tmp", self.name)),
            out: self.root.join(format!("{}-{number}.out", self.name)),
            number,
        }
    }

    /// One client per command. timeout owns its child process group and bounds
    /// a stuck client; flock also stays held by the child across shell respawn.
    pub fn command(&self, attempt: &Attempt, body: &str, seconds: u64) -> String {
        let script = format!(
            "[ \"$(cat {})\" = {} ] || exit 0; \
             {{ echo \"pane=$DOT_AGENT_DECK_PANE_ID\"; {body}; }} > {} 2>&1; mv {} {}",
            path_word(&self.token),
            attempt.number,
            path_word(&attempt.tmp),
            path_word(&attempt.tmp),
            path_word(&attempt.out),
        );
        format!(
            "flock -n {} timeout -k 1s {seconds}s sh -c {}",
            path_word(&self.lock),
            shell_word(&script)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("xver-capture-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn run(command: &str) -> std::process::Output {
        Command::new("sh")
            .args(["-c", command])
            .env("DOT_AGENT_DECK_PANE_ID", "2")
            .output()
            .unwrap()
    }

    /// Scenario: A shell exits before publishing either partial output or a complete
    /// hand-over from a destructive inbox fetch. A retry must discard the partial
    /// output but keep the hand-over even though the inbox is now empty.
    #[test]
    fn replacement_after_temporary_creation_publishes_only_the_new_attempt() {
        let root = scratch("replacement");
        let session = Session::new(&root, "fetch");
        let first = session.prepare(1).unwrap().unwrap();
        let old = session.command(&first, "echo partial; exit 17", 10);
        assert!(!run(&old).status.success());
        assert!(first.tmp.exists());
        assert!(!first.out.exists());
        let second = session.prepare(2).unwrap().unwrap();
        assert!(!first.tmp.exists(), "abandoned temporary capture remains");
        let command = session.command(&second, "printf '%s\\n' replacement", 10);
        assert!(run(&command).status.success());
        assert_eq!(
            std::fs::read_to_string(&second.out).unwrap(),
            "pane=2\nreplacement\n"
        );
        assert!(!second.tmp.exists());
        assert!(run(&old).status.success());
        assert!(!first.tmp.exists(), "a superseded command ran again");
        assert!(!first.out.exists());
        std::fs::remove_dir_all(root).unwrap();

        let root = scratch("consumed-inbox");
        let session = Session::new(&root, "fetch");
        let inbox = root.join("inbox.json");
        let message = r#"{"messages":[{"id":"m-consumed","text":"XVER-DELEGATE-consumed"}]}"#;
        std::fs::write(&inbox, message).unwrap();
        let first = session.prepare(1).unwrap().unwrap();
        // Stand in for fetch's write/flush/ack sequence, then interrupt the
        // capture shell before its rename. Acknowledgement empties the inbox.
        let fetch = format!("cat {}; : > {}", path_word(&inbox), path_word(&inbox));
        let interrupted = session.command(&first, &format!("{fetch}; exit 17"), 10);
        assert!(!run(&interrupted).status.success());
        assert_eq!(std::fs::read_to_string(&inbox).unwrap(), "");
        assert!(first.tmp.exists());
        assert!(!first.out.exists());
        let second = session.prepare(2).unwrap().unwrap();
        let captured = if first.out.exists() {
            std::fs::read_to_string(&first.out).unwrap()
        } else {
            let retry = format!(
                "if [ -s {} ]; then {fetch}; else echo '{{\"messages\":[]}}'; fi",
                path_word(&inbox)
            );
            assert!(run(&session.command(&second, &retry, 10)).status.success());
            std::fs::read_to_string(&second.out).unwrap()
        };
        let listing = "pane=1\n{\"messages\":[{\"id\":\"m-consumed\",\"recipient\":{\"pane_id\":\"2\"},\"state\":\"acknowledged\",\"ack_how\":\"explicit\"}]}";
        let (delivered, detail) = crate::inner::judge_inbox_delivery(
            Some(&captured),
            Some(listing),
            "XVER-DELEGATE-consumed",
            "2",
            "1",
        );
        assert!(
            delivered,
            "acknowledged hand-over lost after interruption: {detail}; capture={captured:?}"
        );
        assert!(!first.tmp.exists());
        assert!(!second.out.exists(), "recovery started another fetch");
        assert!(run(&interrupted).status.success());
        assert!(!first.tmp.exists(), "a superseded fetch ran again");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Scenario: A capture still holds the execution lease when the harness
    /// reaches its retry bound. It must keep its temporary output and token.
    #[test]
    fn a_running_capture_cannot_be_duplicated_or_have_its_output_removed() {
        let root = scratch("running");
        let session = Session::new(&root, "fetch");
        let first = session.prepare(1).unwrap().unwrap();
        std::fs::write(&first.tmp, "partial").unwrap();
        let lease = session.lease().unwrap().unwrap();
        assert!(session.prepare(2).unwrap().is_none());
        assert_eq!(std::fs::read_to_string(&first.tmp).unwrap(), "partial");
        assert_eq!(std::fs::read_to_string(&session.token).unwrap(), "1");
        // The shell uses the same kernel lease as the retry driver.
        assert!(
            !run(&session.command(&first, "echo duplicate", 10))
                .status
                .success()
        );
        assert!(!first.out.exists());
        drop(lease);
        assert!(session.prepare(2).unwrap().is_some());
        assert!(!first.tmp.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Scenario: A custom run root contains spaces, apostrophes and shell syntax.
    /// The capture must publish in that directory and preserve literal output.
    #[test]
    fn shell_paths_and_body_are_quoted_as_literal_words() {
        let root = scratch("quote").join("it's a $(false); root");
        std::fs::create_dir_all(&root).unwrap();
        let session = Session::new(&root, "fetch");
        let attempt = session.prepare(1).unwrap().unwrap();
        let text = "it's literal $(false); `false`";
        let command = session.command(
            &attempt,
            &format!("printf '%s\\n' {}", shell_word(text)),
            10,
        );
        let output = run(&command);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            std::fs::read_to_string(&attempt.out).unwrap(),
            format!("pane=2\n{text}\n")
        );
        std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }
}
