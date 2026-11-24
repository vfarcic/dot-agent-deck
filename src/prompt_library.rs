//! Issue #824: the deck's prompt library — reusable text for an
//! orchestration's task prompt and for messaging a running agent — owned and
//! persisted by the DAEMON, so it follows the deck rather than the machine the
//! desktop app happens to run on.
//!
//! Before this, the desktop kept the library in its webview's `localStorage`
//! (`dot-agent-deck.desktop.prompts.v1`), which put text that describes the
//! *work* on the client side of PRD #803's ownership boundary: a second
//! desktop, or the same desktop pointed at a remote deck, saw a different
//! library. The daemon is the process every client of a deck shares, so it
//! holds the library:
//!
//! * **listed** — [`crate::daemon_protocol::AttachRequest::ListPrompts`];
//! * **changed** — [`crate::daemon_protocol::AttachRequest::SavePrompt`] (add
//!   or replace one prompt by id) and
//!   [`crate::daemon_protocol::AttachRequest::DeletePrompt`];
//! * **seeded** — [`crate::daemon_protocol::AttachRequest::SeedPrompts`], which
//!   adds only the prompts whose id the daemon does not hold, so a client can
//!   hand over a library it kept before the daemon owned one without
//!   overwriting an edit another client made since. All or nothing: a seed
//!   that would not fit changes nothing, so the client keeps its copy.
//!
//! All four answer the library as it stands afterwards, and all four are
//! withheld by a client unless the daemon advertises
//! [`crate::daemon_protocol::CAP_PROMPT_LIBRARY`].
//!
//! **Persisted** in [`PROMPTS_FILE`] under the daemon's state directory
//! ([`crate::config::state_dir`], so `DOT_AGENT_DECK_STATE_DIR` moves it),
//! through the same owner-only, atomic writer and the same loader refusals
//! (a symlink, a non-regular file, a file another user owns) as
//! [`crate::last_command`] — for its reason: a prompt is where people paste
//! context, and context can carry a credential. Loaded once when the daemon
//! starts; after that the in-memory copy is authoritative, and every change is
//! written to disk after the daemon has answered the request that made it, on
//! a detached task, so a keystroke-driven save never waits for the disk.
//!
//! **The state directory is the deck's persistence identity**, as
//! [`crate::last_command`] explains: two daemons sharing one state directory
//! share this file, each writing through its own in-memory copy.
//!
//! **What it is not.** It is not project configuration — nothing here is
//! written to a project's `.dot-agent-deck.toml` — and it carries no change
//! notification: a client re-lists when it wants the current library.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use serde::{Deserialize, Serialize};

use crate::last_command::{
    StoreFileError, current_uid, read_owner_only_file, unparseable_line, write_owner_only_atomic,
};

/// The file the daemon keeps the deck's prompt library in, under
/// [`crate::config::state_dir`].
pub const PROMPTS_FILE: &str = "prompts.toml";

/// The most prompts one library holds.
pub const MAX_PROMPTS: usize = 500;
/// The longest prompt id, in bytes. Clients mint UUIDs, far below this.
pub const MAX_PROMPT_ID_BYTES: usize = 128;
/// The longest prompt name, in bytes.
pub const MAX_PROMPT_NAME_BYTES: usize = 512;
/// The longest prompt body, in bytes.
pub const MAX_PROMPT_BODY_BYTES: usize = 64 * 1024;
/// The longest prompt note, in bytes.
pub const MAX_PROMPT_NOTE_BYTES: usize = 4 * 1024;
/// The most text one library holds: the sum of every prompt's id, name, body
/// and note, in bytes. A reply carrying the whole library must fit one frame
/// (`MAX_FRAME_LEN`, 16 MiB) after JSON escaping, whose worst case is six
/// bytes per input byte, so this stays well under a sixth of it.
pub const MAX_LIBRARY_BYTES: usize = 2 * 1024 * 1024;

/// The largest [`PROMPTS_FILE`] the daemon will read: room for a full
/// [`MAX_LIBRARY_BYTES`] library after TOML escaping, so a file this module
/// wrote always loads, and a file something else put there cannot cost the
/// daemon unbounded memory at startup.
const MAX_FILE_BYTES: u64 = 8 * MAX_LIBRARY_BYTES as u64;

/// One saved prompt, as the daemon stores it and every client sees it.
///
/// `id` is the client-minted identity a save or delete names; `name`, `body`
/// and `note` are the user's own text, kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prompt {
    pub id: String,
    pub name: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Prompt {
    /// The bytes this prompt counts against [`MAX_LIBRARY_BYTES`].
    fn size(&self) -> usize {
        self.id.len() + self.name.len() + self.body.len() + self.note.as_deref().map_or(0, str::len)
    }
}

/// Why a prompt, or a seed, was refused: a fixed sentence naming the limit,
/// never the text, because the text is the user's and this reaches logs and
/// error replies.
pub fn check_prompt(prompt: &Prompt) -> Result<(), String> {
    if prompt.id.is_empty() {
        return Err("a prompt needs an id".into());
    }
    if prompt.id.len() > MAX_PROMPT_ID_BYTES {
        return Err(format!(
            "a prompt id is longer than {MAX_PROMPT_ID_BYTES} bytes"
        ));
    }
    if prompt.id.chars().any(char::is_control) {
        return Err("a prompt id contains a control character".into());
    }
    if prompt.name.len() > MAX_PROMPT_NAME_BYTES {
        return Err(format!(
            "a prompt name is longer than {MAX_PROMPT_NAME_BYTES} bytes"
        ));
    }
    if prompt.body.len() > MAX_PROMPT_BODY_BYTES {
        return Err(format!(
            "a prompt body is longer than {MAX_PROMPT_BODY_BYTES} bytes"
        ));
    }
    if prompt.note.as_deref().map_or(0, str::len) > MAX_PROMPT_NOTE_BYTES {
        return Err(format!(
            "a prompt note is longer than {MAX_PROMPT_NOTE_BYTES} bytes"
        ));
    }
    Ok(())
}

/// Whether `prompts` fits the library's count and size limits.
fn check_library(prompts: &[Prompt]) -> Result<(), String> {
    if prompts.len() > MAX_PROMPTS {
        return Err(format!(
            "the prompt library holds at most {MAX_PROMPTS} prompts"
        ));
    }
    if prompts.iter().map(Prompt::size).sum::<usize>() > MAX_LIBRARY_BYTES {
        return Err(format!(
            "the prompt library holds at most {MAX_LIBRARY_BYTES} bytes of text"
        ));
    }
    Ok(())
}

/// The on-disk shape of [`PROMPTS_FILE`]: one `[[prompt]]` table per prompt,
/// in library order.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default, rename = "prompt", skip_serializing_if = "Vec::is_empty")]
    prompts: Vec<Prompt>,
}

/// What a change to the library did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryOutcome {
    /// The library is changed in memory and not yet known to be on disk, so
    /// the caller owes a [`PromptLibraryStore::persist`].
    Changed,
    /// Nothing changed and nothing needs writing.
    Unchanged,
}

/// The daemon's copy of the deck's prompt library, and the file it persists
/// to. One per daemon, installed on its `AppState` at startup
/// ([`crate::state::AppState::set_prompt_library`]).
///
/// The same two-step shape as [`crate::last_command::LastCommandStore`]:
/// [`Self::save`], [`Self::delete`] and [`Self::seed`] change the in-memory
/// snapshot under a brief lock, with no I/O, so the dispatch calls them before
/// it replies; [`Self::persist`] is the blocking write, which writes whatever
/// the snapshot holds when it takes the writer lock and skips a generation
/// already on disk, so writes that finish out of order never leave an older
/// library over a newer one.
#[derive(Debug)]
pub struct PromptLibraryStore {
    path: PathBuf,
    snapshot: RwLock<Snapshot>,
    writer: Mutex<()>,
}

#[derive(Debug, Default)]
struct Snapshot {
    prompts: Vec<Prompt>,
    /// Bumped by every change to `prompts`.
    generation: u64,
    /// The generation the file on disk holds.
    persisted: u64,
}

impl Snapshot {
    fn outcome(&self) -> LibraryOutcome {
        if self.persisted < self.generation {
            LibraryOutcome::Changed
        } else {
            LibraryOutcome::Unchanged
        }
    }
}

impl PromptLibraryStore {
    /// Where the daemon keeps the file: [`PROMPTS_FILE`] under
    /// [`crate::config::state_dir`].
    pub fn default_path() -> PathBuf {
        crate::config::state_dir().join(PROMPTS_FILE)
    }

    /// Load the store from `path`. **Blocking.**
    ///
    /// Never fails: a missing file is an empty library, and an unreadable,
    /// oversized, untrusted or malformed one is logged — by its path and, for
    /// a parse error, its line number, never its text — and loads empty, so a
    /// bad file cannot stop the daemon starting. A loaded prompt that
    /// [`check_prompt`] refuses, a second prompt with an id already loaded,
    /// and any prompt past the library's limits are dropped, so nothing is
    /// served that a save could not have stored.
    pub fn load(path: PathBuf) -> Self {
        let prompts = match read_owner_only_file(&path, current_uid(), MAX_FILE_BYTES) {
            Ok(None) => Vec::new(),
            Err(StoreFileError::Refused(reason)) => {
                tracing::warn!(
                    path = %path.display(),
                    reason,
                    "ignoring a prompt library file this daemon does not trust"
                );
                Vec::new()
            }
            Err(StoreFileError::Io(error)) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "ignoring an unreadable prompt library file"
                );
                Vec::new()
            }
            Ok(Some(text)) => match toml::from_str::<Persisted>(&text) {
                Ok(persisted) => admit_loaded(persisted.prompts),
                Err(error) => {
                    // Never the parser's own message: it quotes the offending
                    // line, and the line is the user's prompt text.
                    tracing::warn!(
                        path = %path.display(),
                        line = unparseable_line(&text, &error),
                        "ignoring an unparseable prompt library file"
                    );
                    Vec::new()
                }
            },
        };
        Self {
            path,
            snapshot: RwLock::new(Snapshot {
                prompts,
                ..Snapshot::default()
            }),
            writer: Mutex::new(()),
        }
    }

    /// An empty store persisting to `path`, without reading it — for a daemon
    /// whose load could not run. Its first change replaces the file.
    pub fn load_empty(path: PathBuf) -> Self {
        Self {
            path,
            snapshot: RwLock::new(Snapshot::default()),
            writer: Mutex::new(()),
        }
    }

    /// The file this store persists to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The library, in order. Never waits on I/O.
    pub fn list(&self) -> Vec<Prompt> {
        self.read_snapshot().prompts.clone()
    }

    /// Add `prompt`, or replace the one with its id in place, **in memory
    /// only**. Refused — changing nothing — when [`check_prompt`] refuses it
    /// or the library would exceed its limits. Saving a prompt identical to
    /// the stored one answers `Changed` again while the library is not on
    /// disk, so a failed write is retried by the next save.
    pub fn save(&self, prompt: Prompt) -> Result<LibraryOutcome, String> {
        check_prompt(&prompt)?;
        let mut snapshot = self.write_snapshot();
        let mut next = snapshot.prompts.clone();
        match next.iter_mut().find(|held| held.id == prompt.id) {
            Some(held) if *held == prompt => return Ok(snapshot.outcome()),
            Some(held) => *held = prompt,
            None => next.push(prompt),
        }
        check_library(&next)?;
        snapshot.prompts = next;
        snapshot.generation += 1;
        Ok(snapshot.outcome())
    }

    /// Remove the prompt with `id`, **in memory only**. An id the library does
    /// not hold changes nothing and is not an error: another client may have
    /// removed it first.
    pub fn delete(&self, id: &str) -> LibraryOutcome {
        let mut snapshot = self.write_snapshot();
        let before = snapshot.prompts.len();
        snapshot.prompts.retain(|held| held.id != id);
        if snapshot.prompts.len() != before {
            snapshot.generation += 1;
        }
        snapshot.outcome()
    }

    /// Append every prompt in `prompts` whose id the library does not hold, in
    /// their order, **in memory only** — how a client hands over a library it
    /// kept before the daemon owned one. A prompt whose id the library already
    /// holds is skipped, never overwritten, so a seed cannot undo an edit
    /// another client made; a later duplicate within `prompts` is skipped too.
    ///
    /// **All or nothing:** if any prompt it would add is refused by
    /// [`check_prompt`], or the result would exceed the library's limits,
    /// nothing is added and the refusal is returned, so the client knows to keep
    /// its own copy. A prompt it skips is not checked, so a copy the library
    /// already holds cannot block the rest.
    pub fn seed(&self, prompts: Vec<Prompt>) -> Result<LibraryOutcome, String> {
        let mut snapshot = self.write_snapshot();
        let mut next = snapshot.prompts.clone();
        let mut added = false;
        for prompt in prompts {
            if next.iter().all(|held| held.id != prompt.id) {
                check_prompt(&prompt)?;
                next.push(prompt);
                added = true;
            }
        }
        if added {
            check_library(&next)?;
            snapshot.prompts = next;
            snapshot.generation += 1;
        }
        Ok(snapshot.outcome())
    }

    /// Write the library as it stands to disk, if it is not there already.
    /// **Blocking** — the dispatch runs it on a detached blocking task after
    /// answering. On failure the library stays in memory, the clients of this
    /// daemon still share it until it restarts, and the error (naming a path,
    /// never a prompt) is returned for the caller to log.
    pub fn persist(&self) -> Result<(), String> {
        let _writer = self.lock_writer();
        let (prompts, generation) = {
            let snapshot = self.read_snapshot();
            if snapshot.persisted >= snapshot.generation {
                return Ok(());
            }
            (snapshot.prompts.clone(), snapshot.generation)
        };
        let contents = toml::to_string(&Persisted { prompts })
            // No serialiser detail: the value being serialised is the user's text.
            .map_err(|_| "failed to serialise the prompt library".to_string())?;
        write_owner_only_atomic(&self.path, &contents)?;
        let mut snapshot = self.write_snapshot();
        snapshot.persisted = snapshot.persisted.max(generation);
        Ok(())
    }

    // Poison-tolerant, for `LastCommandStore`'s reason: the snapshot is only
    // ever changed by whole assignments.

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

/// The prompts of a loaded file that a save could have stored: valid, the
/// first of each id, and within the library's limits, in file order.
fn admit_loaded(loaded: Vec<Prompt>) -> Vec<Prompt> {
    let mut admitted: Vec<Prompt> = Vec::new();
    let mut size = 0usize;
    let mut dropped = 0usize;
    for prompt in loaded {
        let fits = admitted.len() < MAX_PROMPTS && size + prompt.size() <= MAX_LIBRARY_BYTES;
        if check_prompt(&prompt).is_ok() && fits && admitted.iter().all(|held| held.id != prompt.id)
        {
            size += prompt.size();
            admitted.push(prompt);
        } else {
            dropped += 1;
        }
    }
    if dropped > 0 {
        tracing::warn!(dropped, "dropped prompts the library could not have stored");
    }
    admitted
}

#[cfg(test)]
#[path = "prompt_library_tests.rs"]
mod tests;
