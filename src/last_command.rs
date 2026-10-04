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
//! that the in-memory copy is authoritative, and every change is written to
//! disk after the daemon has answered the request that made it, so a start or
//! a seed never waits for the state directory.
//!
//! **What the loader refuses.** The file is a value the form offers back and
//! the user then starts, so on Unix the loader opens it `O_NOFOLLOW` and
//! judges the open handle: a symlink, a file that is not regular, or one not
//! owned by the daemon's effective uid loads as "no last command" with a
//! fixed log line that carries none of its contents. So another local user
//! who can write the state directory can delete or replace the file, but a
//! file they put there, or a link they point it through, is not offered. The
//! check is made on the file at load only; the state directory's own
//! permissions are not checked. Windows gets the same check from the open
//! handle: the file is opened without following a reparse point, a symbolic
//! link or junction is refused, and the file's owner SID must be the current
//! user's.
//!
//! **The state directory is the deck's persistence identity**, not its
//! endpoint — the precedent `schedules.toml` (found by the config directory)
//! and `daemon.log` / `spawn.lock` (in the same state directory) already set.
//! Two daemons sharing one state directory share this file: each writes through
//! its own in-memory copy, so the file holds whichever recorded last, and each
//! offers its own value until it restarts. Two decks on one host need two
//! `DOT_AGENT_DECK_STATE_DIR`s.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

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
/// ([`crate::state::AppState::set_last_command_store`]). Changing the value and
/// writing it to disk are separate steps, so a start never waits for the disk:
///
/// * [`Self::remember`] / [`Self::remember_if_empty`] change the in-memory
///   snapshot only — a brief lock, no I/O — so the dispatch calls them inline,
///   before it replies, and the next [`Self::get`] already sees the value;
/// * [`Self::persist`] is the **blocking** write, which the dispatch runs on a
///   detached blocking task after deciding the reply.
///
/// Every change bumps a generation. `persist` takes the writer lock, then writes
/// whatever the snapshot holds **at that moment**, not the value it was spawned
/// for, and skips a generation already on disk — so two persists that run out
/// of order can never leave an older value on disk over a newer one. Readers
/// never take the writer lock, so a form asking for the value never waits on a
/// disk write either.
#[derive(Debug)]
pub struct LastCommandStore {
    path: PathBuf,
    /// What [`Self::get`] returns, and how far it has been persisted. Locked
    /// only briefly, never across I/O.
    snapshot: RwLock<Snapshot>,
    /// Held by [`Self::persist`] for the whole of a file write, so writes are
    /// serialised and each one writes the latest snapshot.
    writer: Mutex<()>,
}

#[derive(Debug, Default)]
struct Snapshot {
    value: Option<String>,
    /// Bumped by every change to `value`.
    generation: u64,
    /// The generation the file on disk holds. Below `generation` while a write
    /// is pending or the last one failed, so an equal [`LastCommandStore::remember`]
    /// asks for the write again instead of reporting the value unchanged.
    persisted: u64,
}

/// What [`LastCommandStore::remember`] / [`LastCommandStore::remember_if_empty`]
/// (and the blocking [`LastCommandStore::record`] / [`LastCommandStore::seed`])
/// did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOutcome {
    /// The value is set in memory and not yet known to be on disk — a new
    /// value, or one an earlier write has not persisted — so the caller owes a
    /// [`LastCommandStore::persist`]. From `record` / `seed`, the write has
    /// been made.
    Set,
    /// Nothing changed and nothing needs writing: the command is not
    /// recordable, it is already the persisted value, or — for a seed — the
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
    /// daemon starting. So, on Unix, is a symlink or a file another user owns
    /// (the module doc has what that check does and does not cover). A loaded
    /// value that [`is_recordable`] rejects is dropped as well, so nothing
    /// reaches a form that a start could not have recorded.
    pub fn load(path: PathBuf) -> Self {
        let value = match read_store_file(&path, current_uid()) {
            Ok(None) => None,
            Err(StoreFileError::Refused(reason)) => {
                tracing::warn!(
                    path = %path.display(),
                    reason,
                    "ignoring a last-command file this daemon does not trust"
                );
                None
            }
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
            Err(StoreFileError::Io(error)) => {
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
            snapshot: RwLock::new(Snapshot {
                value,
                ..Snapshot::default()
            }),
            writer: Mutex::new(()),
        }
    }

    /// The file this store persists to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The deck's last command, if it has one. Never waits on I/O: it takes
    /// only the snapshot lock, which no writer holds across a file write.
    pub fn get(&self) -> Option<String> {
        self.read_snapshot().value.clone()
    }

    /// Make `command` the deck's last command, replacing any value, **in memory
    /// only** — no I/O, so it is safe to call before a reply. `Set` means the
    /// caller owes a [`Self::persist`].
    ///
    /// A command [`is_recordable`] rejects changes nothing. Remembering the
    /// value already held answers `Set` again while it is not on disk (a write
    /// pending or failed), so a failed write is retried by the next equal
    /// record instead of being reported unchanged.
    pub fn remember(&self, command: &str) -> StoreOutcome {
        if !is_recordable(command) {
            return StoreOutcome::Unchanged;
        }
        let mut snapshot = self.write_snapshot();
        if snapshot.value.as_deref() != Some(command) {
            snapshot.value = Some(command.to_string());
            snapshot.generation += 1;
        }
        snapshot.outcome()
    }

    /// Set `command` only when the daemon has no last command yet, in memory
    /// only — how a client hands over a value it kept before the daemon owned
    /// one, without overwriting a newer one another client already recorded.
    /// `Set` means the caller owes a [`Self::persist`]; seeding the value the
    /// daemon already holds answers `Set` while that value is not on disk, so
    /// it retries a failed write like [`Self::remember`] does.
    pub fn remember_if_empty(&self, command: &str) -> StoreOutcome {
        if !is_recordable(command) {
            return StoreOutcome::Unchanged;
        }
        let mut snapshot = self.write_snapshot();
        match snapshot.value.as_deref() {
            None => {
                snapshot.value = Some(command.to_string());
                snapshot.generation += 1;
                snapshot.outcome()
            }
            Some(held) if held == command => snapshot.outcome(),
            Some(_) => StoreOutcome::Unchanged,
        }
    }

    /// Write the current value to disk unless that generation is already
    /// there. **Blocking.**
    ///
    /// It writes what the snapshot holds when the writer lock is taken, so a
    /// persist that runs after a newer change writes the newer value, and one
    /// that runs after the newer value was persisted writes nothing. On failure
    /// the value stays in memory — the clients of this daemon still share it
    /// until it restarts — and the error is returned for the caller to log
    /// (it names a path, never the command).
    pub fn persist(&self) -> Result<(), String> {
        let _writer = self.lock_writer();
        let (value, generation) = {
            let snapshot = self.read_snapshot();
            if snapshot.persisted >= snapshot.generation {
                return Ok(());
            }
            (snapshot.value.clone(), snapshot.generation)
        };
        // Only a recordable value ever enters the snapshot after load, and a
        // change always sets one, so a pending generation always has a value.
        let Some(value) = value else {
            return Ok(());
        };
        write_atomic(&self.path, &value)?;
        let mut snapshot = self.write_snapshot();
        snapshot.persisted = snapshot.persisted.max(generation);
        Ok(())
    }

    /// [`Self::remember`] then, when it answers `Set`, [`Self::persist`].
    /// **Blocking** — for callers that may wait on the disk; the dispatch does
    /// not, and calls the two halves itself.
    pub fn record(&self, command: &str) -> Result<StoreOutcome, String> {
        self.remember_then_persist(Self::remember, command)
    }

    /// [`Self::remember_if_empty`] then, when it answers `Set`,
    /// [`Self::persist`]. **Blocking**, like [`Self::record`].
    pub fn seed(&self, command: &str) -> Result<StoreOutcome, String> {
        self.remember_then_persist(Self::remember_if_empty, command)
    }

    fn remember_then_persist(
        &self,
        remember: fn(&Self, &str) -> StoreOutcome,
        command: &str,
    ) -> Result<StoreOutcome, String> {
        match remember(self, command) {
            StoreOutcome::Set => self.persist().map(|()| StoreOutcome::Set),
            StoreOutcome::Unchanged => Ok(StoreOutcome::Unchanged),
        }
    }

    // Both locks are poison-tolerant: a `Snapshot` is only ever changed by
    // single assignments, so a panic cannot leave it half-written.

    fn read_snapshot(&self) -> std::sync::RwLockReadGuard<'_, Snapshot> {
        self.snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_snapshot(&self) -> std::sync::RwLockWriteGuard<'_, Snapshot> {
        self.snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_writer(&self) -> std::sync::MutexGuard<'_, ()> {
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Snapshot {
    /// `Set` while the held value is not yet on disk.
    fn outcome(&self) -> StoreOutcome {
        if self.persisted < self.generation {
            StoreOutcome::Set
        } else {
            StoreOutcome::Unchanged
        }
    }
}

/// Why [`read_store_file`] produced no value.
#[derive(Debug)]
enum StoreFileError {
    /// The file could not be read, or is not a regular file within
    /// [`MAX_FILE_BYTES`].
    Io(std::io::Error),
    /// The file is one the daemon declines to trust (Unix and Windows): a
    /// symlink (on Windows any reparse point), or owned by another user. A
    /// fixed description, never file contents.
    #[cfg_attr(not(any(unix, windows)), allow(dead_code))]
    Refused(&'static str),
}

impl From<std::io::Error> for StoreFileError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// The daemon's effective uid, which the store file must be owned by.
#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Unused off Unix: Windows compares owner SIDs instead (see the Windows
/// `read_store_file`).
#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

/// Read [`LAST_COMMAND_FILE`] at `path`: `Ok(None)` when it does not exist.
///
/// On Unix the file is opened `O_NOFOLLOW | O_NONBLOCK` and judged from the
/// open handle, so what is checked is what is read: a symlink at `path` is
/// refused rather than followed, and the opened file must be regular, owned by
/// `owner_uid`, and at most [`MAX_FILE_BYTES`] long. `owner_uid` is a parameter
/// so a test can stand in for "another user" without being root. The Windows
/// counterpart is the next function.
#[cfg(unix)]
fn read_store_file(path: &Path, owner_uid: u32) -> Result<Option<String>, StoreFileError> {
    use std::io::{Error, ErrorKind};
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        // `O_NOFOLLOW` fails a symlink with `ELOOP` (Linux, macOS) or `EMLINK`
        // (FreeBSD). Confirm it from the path rather than trust the errno: the
        // open has already failed, so nothing is read either way.
        Err(error) => {
            return Err(
                if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                    StoreFileError::Refused("it is a symbolic link")
                } else {
                    StoreFileError::Io(error)
                },
            );
        }
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(StoreFileError::Io(Error::new(
            ErrorKind::InvalidInput,
            "it is not a regular file",
        )));
    }
    if metadata.uid() != owner_uid {
        return Err(StoreFileError::Refused(
            "it is owned by another user than the daemon's",
        ));
    }
    read_opened(file, metadata.len()).map(Some)
}

/// The Windows counterpart of the Unix reader above, judged from the open
/// handle the same way: the file is opened `FILE_FLAG_OPEN_REPARSE_POINT`, so a
/// symbolic link or junction at `path` is opened itself rather than followed,
/// and is refused; the opened file must be regular, owned by the current user
/// (its owner SID, read through
/// [`crate::platform::fsperm::verify_object_owner_is_current_user`]), and at
/// most [`MAX_FILE_BYTES`] long. A directory fails to open at all, which reads
/// as an unreadable file.
#[cfg(windows)]
fn read_store_file(path: &Path, _owner_uid: u32) -> Result<Option<String>, StoreFileError> {
    use std::io::{Error, ErrorKind};
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StoreFileError::Io(error)),
    };
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(StoreFileError::Refused(
            "it is a reparse point (a symbolic link or junction)",
        ));
    }
    if !metadata.is_file() {
        return Err(StoreFileError::Io(Error::new(
            ErrorKind::InvalidInput,
            "it is not a regular file",
        )));
    }
    // The helper's message names SIDs only, never contents, but `Refused`
    // carries a fixed description, so an unreadable owner is refused the same
    // way as a foreign one: either way the file is not offered.
    if crate::platform::fsperm::verify_object_owner_is_current_user(file.as_raw_handle() as HANDLE)
        .is_err()
    {
        return Err(StoreFileError::Refused(
            "it is owned by another user than the daemon's, or its owner cannot be read",
        ));
    }
    read_opened(file, metadata.len()).map(Some)
}

#[cfg(not(any(unix, windows)))]
fn read_store_file(path: &Path, _owner_uid: u32) -> Result<Option<String>, StoreFileError> {
    Ok(crate::bounded_read::read_config_file(path, MAX_FILE_BYTES)?)
}

/// Read an already-vetted store file of reported length `len`, refusing one
/// over [`MAX_FILE_BYTES`] — by its length first, then by what is actually
/// read, in case it grew — and one that is not UTF-8.
#[cfg(any(unix, windows))]
fn read_opened(file: std::fs::File, len: u64) -> Result<String, StoreFileError> {
    use std::io::{Error, ErrorKind, Read as _};

    let too_large = || {
        StoreFileError::Io(Error::new(
            ErrorKind::InvalidData,
            format!("it is larger than the {MAX_FILE_BYTES}-byte limit"),
        ))
    };
    if len > MAX_FILE_BYTES {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(too_large());
    }
    String::from_utf8(bytes)
        .map_err(|error| StoreFileError::Io(Error::new(ErrorKind::InvalidData, error.utf8_error())))
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

    /// Scenario (issue #1540): a record whose disk write fails still updates
    /// the value the forms are offered, and recording the same command again
    /// once the write can succeed persists it rather than reporting it
    /// unchanged. The same holds for a seed.
    #[test]
    fn a_failed_write_is_retried_by_the_next_equal_record_or_seed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LAST_COMMAND_FILE);
        // A non-empty directory where the file belongs: the rename into place
        // fails on every platform.
        let block = || {
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("occupant"), b"x").unwrap();
        };
        let unblock = || std::fs::remove_dir_all(&path).unwrap();

        let store = LastCommandStore::load(path.clone());
        block();
        assert!(store.record("claude").is_err(), "the write must fail");
        assert_eq!(store.get().as_deref(), Some("claude"));
        assert!(
            store.record("claude").is_err(),
            "an equal record retries the write instead of reporting Unchanged"
        );
        unblock();
        assert_eq!(store.record("claude").unwrap(), StoreOutcome::Set);
        assert_eq!(
            LastCommandStore::load(path.clone()).get().as_deref(),
            Some("claude")
        );
        assert_eq!(store.record("claude").unwrap(), StoreOutcome::Unchanged);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LAST_COMMAND_FILE);
        let store = LastCommandStore::load(path.clone());
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("occupant"), b"x").unwrap();
        assert!(store.seed("from-session-toml").is_err());
        assert_eq!(store.get().as_deref(), Some("from-session-toml"));
        assert_eq!(
            store.seed("another").unwrap(),
            StoreOutcome::Unchanged,
            "the daemon has a value, so a different seed is still refused"
        );
        std::fs::remove_dir_all(&path).unwrap();
        assert_eq!(store.seed("from-session-toml").unwrap(), StoreOutcome::Set);
        assert_eq!(
            LastCommandStore::load(path).get().as_deref(),
            Some("from-session-toml")
        );
        assert_eq!(
            store.seed("from-session-toml").unwrap(),
            StoreOutcome::Unchanged
        );
    }

    /// Scenario (issue #1540): while a writer holds the store across its disk
    /// write, a form asking for the last command still gets an answer at once.
    #[test]
    fn get_does_not_wait_for_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(store_in(&dir));
        store.record("claude").unwrap();
        let writer = store.lock_writer();
        let reader = std::sync::Arc::clone(&store);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(reader.get()).unwrap());
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("get blocked on the writer lock");
        drop(writer);
        assert_eq!(got.as_deref(), Some("claude"));
    }

    /// Scenario (issue #1540): remember one command, then a second, and run
    /// the persist owed for the SECOND before the one owed for the first — the
    /// order two detached writes can finish in. The file ends up holding the
    /// second command, and the late persist for the first neither writes the
    /// first back nor rewrites the file at all.
    #[test]
    fn persists_run_out_of_order_leave_the_newest_value_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        assert_eq!(store.remember("first"), StoreOutcome::Set);
        assert_eq!(store.remember("second"), StoreOutcome::Set);
        assert_eq!(
            store.get().as_deref(),
            Some("second"),
            "the snapshot changes before any write"
        );
        assert!(!store.path().exists(), "remembering does no I/O");

        store.persist().unwrap(); // the persist owed for "second"
        assert_eq!(store_in(&dir).get().as_deref(), Some("second"));

        // Prove the late persist for "first" writes nothing: anything it wrote
        // would replace this marker file.
        std::fs::write(store.path(), "last_command = \"marker\"\n").unwrap();
        store.persist().unwrap(); // the persist owed for "first", finishing last
        assert_eq!(store_in(&dir).get().as_deref(), Some("marker"));
        assert_eq!(store.remember("second"), StoreOutcome::Unchanged);
    }

    /// Scenario (issue #1540): many threads each remember a command and then
    /// persist, racing one another; once all are done the file holds exactly
    /// the value the forms are offered, never an older one.
    #[test]
    fn racing_records_leave_the_offered_value_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(store_in(&dir));
        let threads: Vec<_> = (0..16)
            .map(|i| {
                let store = std::sync::Arc::clone(&store);
                std::thread::spawn(move || {
                    for round in 0..4 {
                        if store.remember(&format!("cmd-{i}-{round}")) == StoreOutcome::Set {
                            store.persist().unwrap();
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let offered = store.get();
        assert!(offered.is_some());
        assert_eq!(store_in(&dir).get(), offered);
    }

    /// Scenario (issue #1540): a last-command file reached through a symlink
    /// is not offered — not even when the link points at a well-formed file —
    /// and the log says why without quoting the command.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_loads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("planted.toml");
        std::fs::write(&target, "last_command = \"planted --secret\"\n").unwrap();
        let path = dir.path().join(LAST_COMMAND_FILE);
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(matches!(
            read_store_file(&path, current_uid()),
            Err(StoreFileError::Refused(_))
        ));
        assert_eq!(LastCommandStore::load(path.clone()).get(), None);

        // The next record replaces the link with a file of the daemon's own,
        // and leaves the link's target alone.
        let store = LastCommandStore::load(path.clone());
        store.record("claude").unwrap();
        assert!(!std::fs::symlink_metadata(&path).unwrap().is_symlink());
        assert_eq!(
            LastCommandStore::load(path).get().as_deref(),
            Some("claude")
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "last_command = \"planted --secret\"\n"
        );
    }

    /// Scenario (issue #1540): a last-command file owned by another user than
    /// the daemon's is refused, while the same file owned by the daemon's user
    /// loads. "Another user" is stood in for by asking for a different owner
    /// uid, since changing a file's owner needs root.
    #[cfg(unix)]
    #[test]
    fn a_file_owned_by_another_user_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LAST_COMMAND_FILE);
        std::fs::write(&path, "last_command = \"claude\"\n").unwrap();
        assert_eq!(
            read_store_file(&path, current_uid()).unwrap().as_deref(),
            Some("last_command = \"claude\"\n")
        );
        assert!(matches!(
            read_store_file(&path, current_uid().wrapping_add(1)),
            Err(StoreFileError::Refused(_))
        ));
    }
}
