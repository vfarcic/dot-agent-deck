//! PRD #1666: the deck's persistent identity, the `<deck>` token in every inbox
//! address and message id.
//!
//! A deck id is 8 lowercase base32 characters (`[a-z2-7]{8}`, 40 random bits),
//! created on a daemon's first start at `<state_dir>/deck-id` and read back on
//! every later one. Two daemons sharing a state directory therefore share a deck
//! id, which matches the state directory being the deck's persistence identity.
//! The file is written owner-only and published atomically, and a first start
//! never replaces an id another daemon published in the meantime.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// The file under the state directory that holds the deck id.
pub const DECK_ID_FILE: &str = "deck-id";

/// Characters in a deck id.
pub const DECK_ID_LEN: usize = 8;

/// RFC 4648 base32, lower-cased.
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// A deck-id file larger than this is not one this module wrote.
const MAX_FILE_BYTES: u64 = 64;

/// Temp names tried before a write gives up.
const TEMP_NAME_ATTEMPTS: usize = 16;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A validated deck id. Serialises as its bare string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DeckId(String);

impl DeckId {
    /// A fresh random id from the operating system's randomness.
    pub fn generate() -> Self {
        let mut bytes = [0u8; DECK_ID_LEN];
        // As for hook tokens (`hook_provenance`): the OS randomness source fails
        // only when the OS cannot provide it at all.
        getrandom::fill(&mut bytes).expect("OS randomness is unavailable; cannot mint a deck id");
        // 256 is a multiple of 32, so masking keeps every character uniform.
        Self(
            bytes
                .iter()
                .map(|b| ALPHABET[(b & 31) as usize] as char)
                .collect(),
        )
    }

    /// `Some` when `text` is exactly a deck id.
    pub fn parse(text: &str) -> Option<Self> {
        is_valid_deck_id(text).then(|| Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Read the deck id under `state_dir`, creating it when it is missing and
    /// replacing it when the file does not hold one.
    ///
    /// PR #1735 review: every step after the first read runs under a lock on
    /// a sidecar beside the file (`.deck-id.lock`), so two daemons sharing the
    /// state directory that start at once agree on one id: the second waits,
    /// reads what the first published and adopts it. A new id is published
    /// with a hard link, which fails rather than replaces an id published
    /// meanwhile; a filesystem without hard links publishes by rename, which
    /// could replace one, so it is used only while the lock is held. PR #1735
    /// second review: on a filesystem that can neither lock nor hard-link, or
    /// that cannot lock while the file needs replacing, the start fails with
    /// an error saying so rather than risk two daemons holding different ids.
    pub fn load_or_create(state_dir: &Path) -> Result<Self, String> {
        Self::load_or_create_with(state_dir, &|| {})
    }

    /// [`Self::load_or_create`], running `before_lock` once the first read
    /// found no deck id, just before the lock is taken.
    fn load_or_create_with(state_dir: &Path, before_lock: &dyn Fn()) -> Result<Self, String> {
        let path = deck_id_path(state_dir);
        if let FileState::Valid(id) = read_deck_id_file(&path)? {
            return Ok(id);
        }
        crate::platform::fsperm::create_owner_only_dir(state_dir)
            .map_err(|e| format!("failed to create {}: {e}", state_dir.display()))?;
        before_lock();
        let lock = crate::agent_hook_config::lock_config(&path)
            .map_err(|e| format!("failed to lock {}: {e}", path.display()))?;
        let locked = lock.is_held();
        let unlocked_replace = || {
            format!(
                "{} does not hold a deck id, and this filesystem cannot lock it, so it cannot be \
                 replaced without risking a second daemon holding a different deck id; remove \
                 the file and start the daemon again",
                path.display()
            )
        };
        match read_deck_id_file(&path)? {
            FileState::Valid(id) => return Ok(id),
            FileState::Invalid if !locked => return Err(unlocked_replace()),
            FileState::Invalid => {
                tracing::warn!(
                    path = %path.display(),
                    "the deck-id file does not hold a deck id; replacing it"
                );
                let id = Self::generate();
                write_deck_id(&path, &id, Publish::Replace, locked)?;
                return Ok(id);
            }
            FileState::Missing => {}
        }
        let id = Self::generate();
        match write_deck_id(&path, &id, Publish::CreateNew, locked)? {
            Published::Ours => Ok(id),
            // Another daemon sharing this state directory published first; its
            // id is the deck's.
            Published::Theirs => match read_deck_id_file(&path)? {
                FileState::Valid(theirs) => Ok(theirs),
                _ if !locked => Err(unlocked_replace()),
                _ => {
                    write_deck_id(&path, &id, Publish::Replace, locked)?;
                    Ok(id)
                }
            },
        }
    }
}

impl std::fmt::Display for DeckId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for DeckId {
    type Error = String;
    fn try_from(text: String) -> Result<Self, Self::Error> {
        if is_valid_deck_id(&text) {
            Ok(Self(text))
        } else {
            Err(format!("not a deck id: {text:?}"))
        }
    }
}

impl From<DeckId> for String {
    fn from(id: DeckId) -> Self {
        id.0
    }
}

/// `[a-z2-7]{8}`.
pub fn is_valid_deck_id(text: &str) -> bool {
    text.len() == DECK_ID_LEN && text.bytes().all(|b| ALPHABET.contains(&b))
}

/// `<state_dir>/deck-id`.
pub fn deck_id_path(state_dir: &Path) -> PathBuf {
    state_dir.join(DECK_ID_FILE)
}

enum FileState {
    Missing,
    Invalid,
    Valid(DeckId),
}

fn read_deck_id_file(path: &Path) -> Result<FileState, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FileState::Missing),
        Err(e) => return Err(format!("failed to inspect {}: {e}", path.display())),
    };
    if !meta.file_type().is_file() {
        return Err(format!(
            "{} is not a regular file; refusing to read or replace it",
            path.display()
        ));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Ok(FileState::Invalid);
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => return Ok(FileState::Invalid),
        Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
    };
    Ok(DeckId::parse(text.trim()).map_or(FileState::Invalid, FileState::Valid))
}

enum Publish {
    /// Never overwrite: a concurrent first start may already have published.
    CreateNew,
    /// Overwrite a file that holds no deck id.
    Replace,
}

enum Published {
    Ours,
    Theirs,
}

/// Write `id` to a fresh owner-only temp file beside `path`, then publish it
/// ([`publish_temp`]). `locked`: whether the caller holds the deck-id lock.
fn write_deck_id(
    path: &Path,
    id: &DeckId,
    publish: Publish,
    locked: bool,
) -> Result<Published, String> {
    use std::io::Write;

    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    crate::platform::fsperm::create_owner_only_dir(parent)
        .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;

    let mut opened = None;
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let tmp_path = parent.join(format!(
            "{DECK_ID_FILE}.{}.{}.tmp",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut open_opts = std::fs::OpenOptions::new();
        open_opts.create_new(true).write(true);
        crate::platform::fsperm::set_create_mode_owner_only(&mut open_opts);
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
        .and_then(|()| tmp_file.write_all(format!("{id}\n").as_bytes()))
        .and_then(|()| tmp_file.sync_all());
    drop(tmp_file);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("failed to write {}: {e}", tmp_path.display()));
    }

    let result = publish_temp(&tmp_path, path, publish, locked, &|from, to| {
        std::fs::hard_link(from, to)
    });
    let _ = std::fs::remove_file(&tmp_path);
    result
}

/// Publish the complete temp file `tmp_path` as `path`. A new id is published
/// with `link` (a hard link), which publishes the whole file under its final
/// name and fails rather than overwrites when that name is taken. Where the
/// filesystem has no hard links, an atomic rename is the fallback; it could
/// replace an id another start published, so it is used only when `locked`
/// says the caller holds the deck-id lock that keeps every other start out.
fn publish_temp(
    tmp_path: &Path,
    path: &Path,
    publish: Publish,
    locked: bool,
    link: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<Published, String> {
    match publish {
        Publish::CreateNew => match link(tmp_path, path) {
            Ok(()) => Ok(Published::Ours),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(Published::Theirs),
            Err(_) if locked => rename(tmp_path, path),
            Err(e) => Err(format!(
                "cannot publish the deck id at {}: this filesystem supports neither file \
                 locking nor hard links ({e}), so a daemon starting at the same moment could \
                 publish a different one; put the deck's state directory \
                 (DOT_AGENT_DECK_STATE_DIR) on a local filesystem",
                path.display()
            )),
        },
        Publish::Replace => rename(tmp_path, path),
    }
}

fn rename(tmp_path: &Path, path: &Path) -> Result<Published, String> {
    std::fs::rename(tmp_path, path)
        .map(|()| Published::Ours)
        .map_err(|e| format!("failed to write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    /// Scenario: Load the deck id from an empty state directory, which creates
    /// `deck-id`, then load it again. Both loads return the same valid id, the
    /// file is owner-only, and a file holding garbage is replaced by a fresh
    /// valid id rather than trusted.
    #[spec("inbox/core/002")]
    #[test]
    fn inbox_core_002_deck_id_persists_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let first = DeckId::load_or_create(&state_dir).unwrap();
        assert!(is_valid_deck_id(first.as_str()), "{first}");
        let path = deck_id_path(&state_dir);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            first.as_str()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "deck-id must be owner-only");
        }
        assert_eq!(DeckId::load_or_create(&state_dir).unwrap(), first);
        // The lock sidecar stays by design (deleting a lock file others may be
        // waiting on is unsafe); it is empty and owner-only.
        let sidecar = format!(".{DECK_ID_FILE}.lock");
        let leftovers: Vec<_> = std::fs::read_dir(&state_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != DECK_ID_FILE && *n != *sidecar)
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let lock = std::fs::metadata(state_dir.join(&sidecar)).unwrap();
            assert_eq!(lock.permissions().mode() & 0o777, 0o600);
            assert_eq!(lock.len(), 0);
        }

        std::fs::write(&path, "not a deck id\n").unwrap();
        let replaced = DeckId::load_or_create(&state_dir).unwrap();
        assert!(is_valid_deck_id(replaced.as_str()));
        assert_eq!(DeckId::load_or_create(&state_dir).unwrap(), replaced);
    }

    #[test]
    fn deck_id_format_is_eight_base32_characters() {
        for _ in 0..200 {
            let id = DeckId::generate();
            assert!(is_valid_deck_id(id.as_str()), "{id}");
        }
        for bad in [
            "",
            "abcdefg",
            "abcdefghi",
            "ABCDEFGH",
            "abcdefg1",
            "abcdefg8",
            "abcd-efg",
        ] {
            assert!(!is_valid_deck_id(bad), "{bad:?}");
            assert!(DeckId::parse(bad).is_none());
        }
        // The design's example id `k3x9q2ab` is not one: 9 is outside base32.
        assert!(DeckId::parse("k3x9q2ab").is_none());
        let id = DeckId::parse("k3xzq2ab").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"k3xzq2ab\"");
        assert!(serde_json::from_str::<DeckId>("\"K3XZQ2AB\"").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_deck_id_path_that_is_not_a_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(deck_id_path(dir.path())).unwrap();
        assert!(DeckId::load_or_create(dir.path()).is_err());
    }

    /// PR #1735 review: a daemon creating the deck id while another one is
    /// still publishing its own waits for it, and adopts the id that was
    /// published. The loader signals once its first read found no id, just
    /// before it takes the lock, so the other id is published after that read
    /// and before the loader could publish its own.
    #[test]
    fn a_first_start_waits_for_a_concurrent_one_and_adopts_its_id() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let path = deck_id_path(&state_dir);
        let theirs = DeckId::generate();
        let publishing = crate::agent_hook_config::lock_config(&path).unwrap();
        assert!(
            publishing.is_held(),
            "this test needs a filesystem that locks"
        );
        let (read_nothing, reached) = std::sync::mpsc::channel();
        let loader = {
            let state_dir = state_dir.clone();
            std::thread::spawn(move || {
                DeckId::load_or_create_with(&state_dir, &|| {
                    let _ = read_nothing.send(());
                })
            })
        };
        reached
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the loader reads no deck id and goes for the lock");
        write_deck_id(&path, &theirs, Publish::Replace, true).unwrap();
        drop(publishing);
        let adopted = loader.join().unwrap().unwrap();
        assert_eq!(adopted, theirs);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            theirs.as_str()
        );
    }

    /// PR #1735 second review: without the deck-id lock and without hard
    /// links, publishing a new id would be a rename that could replace one
    /// another start published meanwhile, so it fails instead and the
    /// published id stays. Holding the lock, the rename is safe.
    #[test]
    fn without_a_lock_or_hard_links_a_new_id_never_replaces_a_published_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DECK_ID_FILE);
        let theirs = DeckId::generate();
        std::fs::write(&path, format!("{theirs}\n")).unwrap();
        let no_links =
            |_: &Path, _: &Path| Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
        let ours = dir.path().join("ours.tmp");
        std::fs::write(&ours, format!("{}\n", DeckId::generate())).unwrap();
        let unlocked = publish_temp(&ours, &path, Publish::CreateNew, false, &no_links);
        assert!(unlocked.is_err(), "an unlocked publish must not rename");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            theirs.as_str(),
            "the published id stays"
        );
        let locked = publish_temp(&ours, &path, Publish::CreateNew, true, &no_links);
        assert!(matches!(locked, Ok(Published::Ours)));
    }
}
