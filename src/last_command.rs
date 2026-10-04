//! Issue #1540: the command the deck's New agent form last started, owned and
//! persisted by the DAEMON so every client of the deck offers the same value.
//!
//! Before this, each client kept its own copy — the TUI in its `session.toml`
//! (`SavedSession::last_command`), the desktop in memory per deck — so a
//! command started from one was never offered by the other, and the desktop
//! forgot it on every restart. The daemon is the one process both clients
//! share for a given deck, so it holds the value:
//!
//! * **read** — [`crate::new_agent_options::NewAgentOptions::last_command`] on
//!   the answer the form already asks for;
//! * **recorded** — by the daemon itself, once it has ACCEPTED a start that
//!   carries [`crate::daemon_protocol::AttachRequest::StartAgent`]'s
//!   `remember_command` marker (see that field for which starts may carry it);
//! * **seeded** — [`crate::daemon_protocol::AttachRequest::SeedLastCommand`],
//!   which sets the value only when the daemon has none, so a client can hand
//!   over a value it kept before the daemon owned one.
//!
//! All three are withheld by a client unless the daemon advertises
//! [`crate::daemon_protocol::CAP_LAST_COMMAND`].
//!
//! **Persisted** in [`LAST_COMMAND_FILE`] under the daemon's state directory
//! ([`crate::config::state_dir`], so `DOT_AGENT_DECK_STATE_DIR` moves it), owner-only
//! (`0600` on Unix; on Windows a current-user DACL applied before the first byte
//! is written, see `write_atomic`) and written atomically —
//! the discipline `session.toml` follows, for its reason: a command line is
//! where people put credentials. Loaded once when the daemon starts; after
//! that the in-memory copy is authoritative and every change is written
//! through.
//!
//! **The state directory is the deck's persistence identity**, not its
//! endpoint — the precedent `schedules.toml` (found by the config directory)
//! and `daemon.log` / `spawn.lock` (in the same state directory) already set.
//! Two daemons sharing one state directory share this file: each writes through
//! its own in-memory copy, so the file holds whichever recorded last, and each
//! offers its own value until it restarts. Two decks on one host need two
//! `DOT_AGENT_DECK_STATE_DIR`s.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// The file the daemon keeps the deck's last command in, under
/// [`crate::config::state_dir`].
pub const LAST_COMMAND_FILE: &str = "last-command.toml";

/// The longest command the daemon records or accepts as a seed, in bytes.
///
/// A peer-chosen string the daemon keeps and writes to disk needs a bound, and
/// a command typed into a form is nowhere near this. A longer one is not
/// recorded (the start itself is unaffected).
pub const MAX_LAST_COMMAND_BYTES: usize = 16 * 1024;

/// The largest [`LAST_COMMAND_FILE`] the daemon will read: comfortably above a
/// [`MAX_LAST_COMMAND_BYTES`] command after TOML escaping, so a file this
/// module wrote always loads, and a file something else put there cannot cost
/// the daemon unbounded memory at startup.
const MAX_FILE_BYTES: u64 = 8 * MAX_LAST_COMMAND_BYTES as u64;

/// How many temp names a save draws before giving up — the
/// `deck_list::write_resolved` arrangement: the temp file is created with
/// `create_new`, so a leftover or a planted symlink at the predictable name
/// costs one draw rather than being written through.
const TEMP_NAME_ATTEMPTS: usize = 8;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The on-disk shape of [`LAST_COMMAND_FILE`].
#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_command: Option<String>,
}

/// Whether `command` is one the daemon records: not blank or whitespace-only
/// (an empty Command means the deck's default shell, which is not a command
/// worth offering back — the TUI's `record_candidate` rule), and at most
/// [`MAX_LAST_COMMAND_BYTES`] long.
pub fn is_recordable(command: &str) -> bool {
    !command.trim().is_empty() && command.len() <= MAX_LAST_COMMAND_BYTES
}

/// The daemon's copy of the deck's last command, and the file it persists to.
///
/// One per daemon, installed on its `AppState` at startup
/// ([`crate::state::AppState::set_last_command_store`]). Every method that
/// writes is **blocking** file I/O, so the dispatch calls them from a blocking
/// thread. The value and the file are changed under one lock, so the file
/// always ends up holding the last value set, even when two starts race.
#[derive(Debug)]
pub struct LastCommandStore {
    path: PathBuf,
    value: Mutex<Option<String>>,
}

/// What [`LastCommandStore::record`] / [`LastCommandStore::seed`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOutcome {
    /// The value was set (in memory, and written to disk).
    Set,
    /// Nothing changed: the command is not recordable, or — for a seed — the
    /// daemon already has a value.
    Unchanged,
}

impl LastCommandStore {
    /// Where the daemon keeps the file: [`LAST_COMMAND_FILE`] under
    /// [`crate::config::state_dir`].
    pub fn default_path() -> PathBuf {
        crate::config::state_dir().join(LAST_COMMAND_FILE)
    }

    /// Load the store from `path`. **Blocking.**
    ///
    /// Never fails: a missing file is "no last command", and an unreadable,
    /// oversized, non-regular or malformed one is logged and treated the same
    /// way — the value is a convenience, and a bad file must not stop the
    /// daemon starting. A loaded value that [`is_recordable`] rejects is
    /// dropped as well, so nothing reaches a form that a start could not have
    /// recorded.
    pub fn load(path: PathBuf) -> Self {
        let value = match crate::bounded_read::read_config_file(&path, MAX_FILE_BYTES) {
            Ok(None) => None,
            Ok(Some(text)) => match toml::from_str::<Persisted>(&text) {
                Ok(persisted) => persisted.last_command.filter(|c| is_recordable(c)),
                Err(error) => {
                    // Never the parser's own message: `toml::de::Error`'s
                    // Display quotes the offending source line, and the line
                    // in this file is the command — credentials included.
                    tracing::warn!(
                        path = %path.display(),
                        line = unparseable_line(&text, &error),
                        "ignoring an unparseable last-command file"
                    );
                    None
                }
            },
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "ignoring an unreadable last-command file"
                );
                None
            }
        };
        Self {
            path,
            value: Mutex::new(value),
        }
    }

    /// The file this store persists to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The deck's last command, if it has one. Never blocks on I/O.
    pub fn get(&self) -> Option<String> {
        self.lock().clone()
    }

    /// Record `command` as the deck's last command, replacing any value.
    /// **Blocking.**
    ///
    /// A command [`is_recordable`] rejects changes nothing. When the write to
    /// disk fails the in-memory value is still updated — the clients of this
    /// daemon still share it until it restarts — and the error is returned for
    /// the caller to log.
    pub fn record(&self, command: &str) -> Result<StoreOutcome, String> {
        if !is_recordable(command) {
            return Ok(StoreOutcome::Unchanged);
        }
        let mut value = self.lock();
        if value.as_deref() == Some(command) {
            return Ok(StoreOutcome::Unchanged);
        }
        *value = Some(command.to_string());
        write_atomic(&self.path, command)?;
        Ok(StoreOutcome::Set)
    }

    /// Set `command` only when the daemon has no last command yet — how a
    /// client hands over a value it kept before the daemon owned one, without
    /// overwriting a newer one another client already recorded. **Blocking.**
    pub fn seed(&self, command: &str) -> Result<StoreOutcome, String> {
        if !is_recordable(command) {
            return Ok(StoreOutcome::Unchanged);
        }
        let mut value = self.lock();
        if value.is_some() {
            return Ok(StoreOutcome::Unchanged);
        }
        *value = Some(command.to_string());
        write_atomic(&self.path, command)?;
        Ok(StoreOutcome::Set)
    }

    /// Poison-tolerant: an `Option<String>` has nothing a panic could leave
    /// half-written.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The 1-based line a parse of [`LAST_COMMAND_FILE`] failed on, or `0` when the
/// parser reports no position — the only detail of a malformed file the daemon
/// logs, because everything else in it is the command.
fn unparseable_line(text: &str, error: &toml::de::Error) -> usize {
    error.span().map_or(0, |span| {
        let start = span.start.min(text.len());
        text.as_bytes()[..start]
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
            + 1
    })
}

/// Replace the file at `path` with one holding `command`, owner-only and
/// atomically: a sibling temp file created `create_new`, its owner-only
/// permissions asserted before the first byte, then renamed into place. On Unix
/// it is created `0600`; on Windows it is created under the inherited DACL and
/// tightened to the current user before the first byte, held with no sharing
/// so no other handle can be opened on it while it is looser. The parent
/// directory is created owner-only if it is missing.
fn write_atomic(path: &Path, command: &str) -> Result<(), String> {
    use std::io::Write;

    let contents = toml::to_string(&Persisted {
        last_command: Some(command.to_string()),
    })
    // No serialiser detail in the message: the value being serialised is the
    // command, and this error is logged.
    .map_err(|_| "failed to serialise the last command".to_string())?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    crate::platform::fsperm::create_owner_only_dir(parent)
        .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    let file_name = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| LAST_COMMAND_FILE.to_string());

    let mut opened = None;
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let tmp_path = parent.join(format!(
            "{file_name}.{}.{}.tmp",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut open_opts = std::fs::OpenOptions::new();
        open_opts.create_new(true).write(true);
        crate::platform::fsperm::set_create_mode_owner_only(&mut open_opts);
        // Windows creates the file under the directory's inherited DACL and
        // `set_file_owner_only` only tightens it afterwards; opening it with no
        // sharing means no other handle can be opened on it in between, so
        // nothing holds a handle granted under the looser DACL.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            open_opts.share_mode(0);
        }
        match open_opts.open(&tmp_path) {
            Ok(file) => {
                opened = Some((tmp_path, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("failed to open {}: {e}", tmp_path.display())),
        }
    }
    let Some((tmp_path, mut tmp_file)) = opened else {
        return Err(format!(
            "every temp file name tried for {} was already taken",
            path.display()
        ));
    };
    let written = crate::platform::fsperm::set_file_owner_only(&tmp_file)
        .and_then(|()| tmp_file.write_all(contents.as_bytes()))
        .and_then(|()| tmp_file.sync_all());
    drop(tmp_file);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("failed to write {}: {e}", tmp_path.display()));
    }
    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("failed to write {}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in(dir: &tempfile::TempDir) -> LastCommandStore {
        LastCommandStore::load(dir.path().join("state").join(LAST_COMMAND_FILE))
    }

    /// Scenario (issue #1540): a command recorded by one daemon is what the
    /// next daemon loading the same state directory offers — the value
    /// survives a daemon restart.
    #[test]
    fn a_recorded_command_survives_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        assert_eq!(store.get(), None, "a fresh deck has no last command");
        assert_eq!(
            store.record("claude --model haiku").unwrap(),
            StoreOutcome::Set
        );
        assert_eq!(store.get().as_deref(), Some("claude --model haiku"));

        let reloaded = store_in(&dir);
        assert_eq!(reloaded.get().as_deref(), Some("claude --model haiku"));

        // A later record replaces it, and that is what the next load sees.
        reloaded.record("codex").unwrap();
        assert_eq!(store_in(&dir).get().as_deref(), Some("codex"));
    }

    /// Scenario (issue #1540): a command can carry quotes, a backslash or a
    /// newline, and it round-trips through the file unchanged.
    #[test]
    fn an_awkward_command_round_trips_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let command = "sh -c 'echo \"a\\b\"'\nsecond = line";
        store_in(&dir).record(command).unwrap();
        assert_eq!(store_in(&dir).get().as_deref(), Some(command));
    }

    /// Scenario (issue #1540): the file holding the command is owner-only,
    /// whatever the umask, because a command line is where credentials go.
    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        store.record("claude").unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "last-command file mode was {mode:o}");
        // A rewrite keeps it owner-only too: the temp file is created fresh.
        store.record("codex").unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "rewritten file mode was {mode:o}");
    }

    /// Scenario (issue #1540): a blank or whitespace-only command is never
    /// recorded or seeded, so it cannot overwrite a real one, and an oversized
    /// one is not recorded either.
    #[test]
    fn blank_and_oversized_commands_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        for blank in ["", "   ", "\t\n"] {
            assert_eq!(store.record(blank).unwrap(), StoreOutcome::Unchanged);
            assert_eq!(store.seed(blank).unwrap(), StoreOutcome::Unchanged);
        }
        assert_eq!(store.get(), None);
        assert!(
            !store.path().exists(),
            "nothing recordable was offered, so nothing is written"
        );

        store.record("claude").unwrap();
        store.record("  ").unwrap();
        let oversized = "x".repeat(MAX_LAST_COMMAND_BYTES + 1);
        assert_eq!(store.record(&oversized).unwrap(), StoreOutcome::Unchanged);
        assert_eq!(store.get().as_deref(), Some("claude"));
        assert_eq!(store_in(&dir).get().as_deref(), Some("claude"));
    }

    /// Scenario (issue #1540): a seed sets the value only when the daemon has
    /// none, so a client handing over its old copy never overwrites a newer
    /// command another client already recorded.
    #[test]
    fn a_seed_only_fills_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        assert_eq!(store.seed("from-session-toml").unwrap(), StoreOutcome::Set);
        assert_eq!(store_in(&dir).get().as_deref(), Some("from-session-toml"));
        assert_eq!(store.seed("another").unwrap(), StoreOutcome::Unchanged);
        store.record("recorded").unwrap();
        assert_eq!(store.seed("stale").unwrap(), StoreOutcome::Unchanged);
        assert_eq!(store.get().as_deref(), Some("recorded"));
    }

    /// Scenario (issue #1540): load a malformed last-command file whose command
    /// carries a credential, capture what the daemon logs about it, and assert
    /// the credential is not in the log — only a fixed message, the path and a
    /// line number — although the parser's own error text does quote it.
    #[test]
    fn a_malformed_file_never_logs_its_command() {
        use std::io::Write as _;

        #[derive(Clone, Default)]
        struct CapturedLog(std::sync::Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for CapturedLog {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
            type Writer = CapturedLog;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        const SENTINEL: &str = "sk-SENTINEL-1540-do-not-log";
        let malformed = [
            format!("last_command = \"claude --api-key {SENTINEL}\n"),
            format!("last_command = \"claude --api-key {SENTINEL}\" trailing\n"),
            format!("last_command = [\"claude --api-key {SENTINEL}\"]\n"),
            format!("# header\nlast_command = {SENTINEL}\n"),
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LAST_COMMAND_FILE);
        for contents in malformed {
            let parse_error =
                toml::from_str::<Persisted>(&contents).expect_err("each fixture must be malformed");
            assert!(
                parse_error.to_string().contains(SENTINEL),
                "the fixture only proves something if the parser's own text \
                 quotes the command: {contents:?}"
            );

            let mut file = std::fs::File::create(&path).unwrap();
            file.write_all(contents.as_bytes()).unwrap();
            drop(file);

            let captured = CapturedLog::default();
            let guard = crate::test_isolation::capture_tracing_on_this_thread(
                tracing_subscriber::fmt()
                    .with_writer(captured.clone())
                    .with_max_level(tracing_subscriber::filter::LevelFilter::TRACE)
                    .with_ansi(false)
                    .finish(),
            );
            let store = LastCommandStore::load(path.clone());
            drop(guard);
            assert_eq!(store.get(), None);

            let logged = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
            assert!(
                logged.contains("ignoring an unparseable last-command file"),
                "the malformed file must still be reported: {logged:?}"
            );
            assert!(
                !logged.contains(SENTINEL) && !logged.contains("api-key"),
                "the command reached the log: {logged:?}"
            );
        }
    }

    /// Scenario (issue #1540): a malformed, unrecordable or non-regular file
    /// loads as "no last command" rather than stopping the daemon, and the
    /// next record replaces it.
    #[test]
    fn a_bad_file_loads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LAST_COMMAND_FILE);
        std::fs::write(&path, "this is = = not toml").unwrap();
        assert_eq!(LastCommandStore::load(path.clone()).get(), None);
        std::fs::write(&path, "last_command = \"   \"\n").unwrap();
        assert_eq!(LastCommandStore::load(path.clone()).get(), None);
        std::fs::write(&path, "").unwrap();
        let store = LastCommandStore::load(path.clone());
        assert_eq!(store.get(), None);
        store.record("claude").unwrap();
        assert_eq!(
            LastCommandStore::load(path).get().as_deref(),
            Some("claude")
        );

        let as_dir = dir.path().join("a-directory");
        std::fs::create_dir(&as_dir).unwrap();
        assert_eq!(LastCommandStore::load(as_dir).get(), None);
    }
}
