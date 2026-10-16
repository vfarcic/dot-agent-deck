//! PR #1735 fifth review (B-05): how long the files the deck saves long
//! message bodies to are kept.
//!
//! A message too long to hand over whole names a file holding all of it
//! ([`crate::inbox_hub::retain_full_body_minted`]). The deck tracks the files
//! it wrote itself here — never a `spill_path` a caller supplied — and keeps
//! each one while its message is live, then for [`DEFAULT_SPILL_RETENTION`]
//! after the message settles or leaves the inbox's history, since the agent may
//! still be reading the file after acknowledging the excerpt. Settled files past
//! [`DEFAULT_MAX_SETTLED_SPILL_BYTES`] are removed oldest first, sooner.
//!
//! Each daemon run lists its files in a manifest of its own under the state
//! directory, locked for as long as the run lasts. A daemon starting up takes
//! over the manifests no running daemon holds: it removes the files they list
//! that are older than the retention, and keeps tracking the younger ones.
//! This cleanup removes only a regular file named as this deck mints them,
//! inside a `.dot-agent-deck` directory.

use std::collections::HashSet;
use std::io::{Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use tracing::{debug, warn};

use crate::inbox::DeckId;

/// How long a saved body is kept after its message settled.
pub const DEFAULT_SPILL_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// The most bytes of saved bodies whose messages settled that are kept.
pub const DEFAULT_MAX_SETTLED_SPILL_BYTES: u64 = 64 * 1024 * 1024;

/// The directory under the state directory holding each run's manifest.
const MANIFEST_DIR: &str = "inbox-spills";

/// The prefix of every saved body's file name.
const SPILL_PREFIX: &str = "inbox-message-";

/// The name of a saved body for `deck`: `inbox-message-<deck>-<unique>.md`.
pub fn spill_file_name(deck: &DeckId, unique: &str) -> String {
    format!("{SPILL_PREFIX}{deck}-{unique}.md")
}

/// `path` names a file this deck saves bodies to: a regular file — not a
/// symlink — named by [`spill_file_name`] for `deck`, directly inside a
/// `.dot-agent-deck` directory.
fn is_own_spill(path: &Path, deck: &DeckId) -> bool {
    let named = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.strip_prefix(SPILL_PREFIX)
                .and_then(|rest| rest.strip_prefix(deck.as_str()))
                .is_some_and(|rest| rest.starts_with('-') && rest.ends_with(".md"))
        });
    let placed = path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|dir| dir == crate::orchestrator_context::CONTEXT_DIR_NAME);
    named && placed && std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
}

/// Remove `path` when it is a saved body of `deck`'s ([`is_own_spill`]).
pub fn remove_spill(path: &Path, deck: &DeckId) {
    if !is_own_spill(path, deck) {
        debug!(path = %path.display(), "inbox: not removing a file that is not a saved body");
        return;
    }
    match std::fs::remove_file(path) {
        Ok(()) => debug!(path = %path.display(), "inbox: removed a saved body"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => warn!(path = %path.display(), %error, "inbox: could not remove a saved body"),
    }
}

/// One saved body the deck wrote.
#[derive(Debug)]
struct OwnedSpill {
    path: PathBuf,
    /// The message naming it; `None` for one taken over from an earlier run,
    /// whose messages are gone.
    message: Option<String>,
    bytes: u64,
    /// When its message was first seen settled or gone.
    settled_at: Option<Instant>,
}

/// The saved bodies one daemon run tracks. See the module docs.
#[derive(Debug)]
pub struct SpillLedger {
    owned: Vec<OwnedSpill>,
    pub retention: Duration,
    pub max_settled_bytes: u64,
}

impl Default for SpillLedger {
    fn default() -> Self {
        Self {
            owned: Vec::new(),
            retention: DEFAULT_SPILL_RETENTION,
            max_settled_bytes: DEFAULT_MAX_SETTLED_SPILL_BYTES,
        }
    }
}

impl SpillLedger {
    /// Track `path`, a body of `bytes` saved for message `message`.
    pub fn own(&mut self, path: PathBuf, message: &str, bytes: u64) {
        self.owned.push(OwnedSpill {
            path,
            message: Some(message.to_string()),
            bytes,
            settled_at: None,
        });
    }

    /// Track the bodies an ended run left ([`SpillManifest::open`]).
    pub fn adopt(&mut self, taken: TakenOver) {
        self.owned.extend(taken.0);
    }

    /// What this run's manifest lists: every body tracked now, one path a line.
    pub fn manifest_text(&self) -> String {
        let mut text = String::new();
        for spill in &self.owned {
            if let Some(path) = spill.path.to_str() {
                text.push_str(path);
                text.push('\n');
            }
        }
        text
    }

    /// The saved bodies tracked now.
    pub fn paths(&self) -> Vec<PathBuf> {
        self.owned.iter().map(|spill| spill.path.clone()).collect()
    }

    /// Stop tracking, and answer for removal, every saved body whose message
    /// is not among `live` and settled at least [`Self::retention`] before
    /// `now`, then the oldest settled ones past [`Self::max_settled_bytes`].
    /// A body whose message is live is kept whatever its age.
    pub fn reclaim(&mut self, now: Instant, live: &HashSet<&str>) -> Vec<PathBuf> {
        for spill in &mut self.owned {
            let is_live = spill.message.as_deref().is_some_and(|id| live.contains(id));
            if is_live {
                spill.settled_at = None;
            } else {
                spill.settled_at.get_or_insert(now);
            }
        }
        let expired = |spill: &OwnedSpill| {
            spill
                .settled_at
                .is_some_and(|at| now.saturating_duration_since(at) >= self.retention)
        };
        let mut reclaimed: Vec<PathBuf> = Vec::new();
        let mut kept = Vec::with_capacity(self.owned.len());
        for spill in std::mem::take(&mut self.owned) {
            if expired(&spill) {
                reclaimed.push(spill.path);
            } else {
                kept.push(spill);
            }
        }
        self.owned = kept;
        let mut settled_bytes: u64 = self
            .owned
            .iter()
            .filter(|spill| spill.settled_at.is_some())
            .map(|spill| spill.bytes)
            .sum();
        while settled_bytes > self.max_settled_bytes {
            let Some(oldest) = self
                .owned
                .iter()
                .enumerate()
                .filter_map(|(index, spill)| spill.settled_at.map(|at| (at, index)))
                .min()
                .map(|(_, index)| index)
            else {
                break;
            };
            let spill = self.owned.remove(oldest);
            settled_bytes -= spill.bytes;
            reclaimed.push(spill.path);
        }
        reclaimed
    }
}

/// The saved bodies [`SpillManifest::open`] took over from ended runs, for
/// [`SpillLedger::adopt`].
#[derive(Debug, Default)]
pub struct TakenOver(Vec<OwnedSpill>);

/// One daemon run's manifest of its saved bodies, locked while the run lasts.
#[derive(Debug)]
pub struct SpillManifest(std::fs::File);

impl SpillManifest {
    /// Keep this run's manifest under `state_dir`, and take over the
    /// manifests no running daemon holds ([`take_over_orphans`]): the bodies of
    /// `deck` they list that are older than `retention` are removed, and the
    /// younger ones answered, as settled when they were saved, to be tracked.
    pub fn open(
        state_dir: &Path,
        deck: &DeckId,
        retention: Duration,
        now: Instant,
    ) -> std::io::Result<(Self, TakenOver)> {
        let dir = state_dir.join(MANIFEST_DIR);
        crate::platform::fsperm::create_owner_only_dir(&dir)?;
        let manifest = create_locked_manifest(&dir)?;
        let mut taken = Vec::new();
        for (path, age) in take_over_orphans(&dir, deck) {
            if age >= retention {
                remove_spill(&path, deck);
                continue;
            }
            let bytes = std::fs::metadata(&path).map_or(0, |meta| meta.len());
            taken.push(OwnedSpill {
                path,
                message: None,
                bytes,
                settled_at: Some(now.checked_sub(age).unwrap_or(now)),
            });
        }
        Ok((Self(manifest), TakenOver(taken)))
    }

    /// Rewrite the manifest in place — the lock is on the open file — with
    /// `text` ([`SpillLedger::manifest_text`]). A failed write is logged: it
    /// costs only the cleanup of those bodies after this run ends.
    pub fn write(&mut self, text: &str) {
        let file = &mut self.0;
        let written = file
            .set_len(0)
            .and_then(|()| file.rewind())
            .and_then(|()| file.write_all(text.as_bytes()))
            .and_then(|()| file.flush());
        if let Err(error) = written {
            warn!(%error, "inbox: could not record the saved bodies in this run's manifest");
        }
    }
}

/// Create a manifest in `dir` and lock it before it can be taken for an
/// orphan: written under a temporary name, locked, then renamed into place.
/// The lock stays on the file through the rename and lasts while the returned
/// handle is open.
fn create_locked_manifest(dir: &Path) -> std::io::Result<std::fs::File> {
    let mut unique = [0u8; 8];
    getrandom::fill(&mut unique).map_err(std::io::Error::other)?;
    let unique: String = unique.iter().map(|b| format!("{b:02x}")).collect();
    let pending = dir.join(format!(".pending-{unique}"));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).read(true).write(true);
    crate::platform::fsperm::set_create_mode_owner_only(&mut options);
    let file = options.open(&pending)?;
    if let Err(error) = file.try_lock() {
        let _ = std::fs::remove_file(&pending);
        return Err(std::io::Error::other(format!(
            "could not lock the saved-body manifest: {error}"
        )));
    }
    std::fs::rename(&pending, dir.join(format!("run-{unique}.list")))?;
    Ok(file)
}

/// Every manifest in `dir` that no running daemon holds is taken over: each
/// path it lists is answered with the age of its file, and the manifest is
/// removed. A manifest some running daemon holds — its lock is taken — is left
/// alone, and so is a listed file that is missing or not this deck's.
fn take_over_orphans(dir: &Path, deck: &DeckId) -> Vec<(PathBuf, Duration)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let pending = name.starts_with(".pending-");
        if !pending && !(name.starts_with("run-") && name.ends_with(".list")) {
            continue;
        }
        let Ok(file) = std::fs::OpenOptions::new().read(true).open(&path) else {
            continue;
        };
        if file.try_lock().is_err() {
            continue;
        }
        if pending {
            // A run that ended before its manifest was in place listed nothing;
            // one just starting holds its lock within moments of creating it.
            let stale = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| SystemTime::now().duration_since(at).ok())
                .is_some_and(|age| age >= Duration::from_secs(60));
            if stale {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        let listed = std::fs::read_to_string(&path).unwrap_or_default();
        for line in listed.lines().filter(|line| !line.is_empty()) {
            let spill = PathBuf::from(line);
            if !is_own_spill(&spill, deck) {
                continue;
            }
            let age = std::fs::metadata(&spill)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| SystemTime::now().duration_since(at).ok())
                .unwrap_or_default();
            found.push((spill, age));
        }
        if let Err(error) = std::fs::remove_file(&path) {
            warn!(path = %path.display(), %error, "inbox: could not remove a taken-over manifest");
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deck() -> DeckId {
        DeckId::parse("k3xzq2ab").unwrap()
    }

    fn spill_in(dir: &Path, unique: &str) -> PathBuf {
        let path = crate::orchestrator_context::write_new_coordination_file(
            dir,
            &spill_file_name(&deck(), unique),
            "the whole body",
        )
        .unwrap();
        assert!(is_own_spill(&path, &deck()));
        path
    }

    /// A saved body is kept while its message is live, and for the retention
    /// after it settles — an acknowledgement alone removes nothing — then
    /// reclaimed.
    #[test]
    fn a_saved_body_outlives_its_message_by_the_retention() {
        let dir = tempfile::tempdir().unwrap();
        let path = spill_in(dir.path(), "1");
        let mut ledger = SpillLedger {
            retention: Duration::from_secs(60),
            ..SpillLedger::default()
        };
        ledger.own(path.clone(), "m-1", 14);
        let start = Instant::now();
        let live: HashSet<&str> = ["m-1"].into();
        assert!(ledger.reclaim(start, &live).is_empty());
        let settled = HashSet::new();
        assert!(
            ledger.reclaim(start, &settled).is_empty(),
            "a body is kept right after its message settles"
        );
        assert!(
            ledger
                .reclaim(start + Duration::from_secs(59), &settled)
                .is_empty()
        );
        assert_eq!(
            ledger.reclaim(start + Duration::from_secs(60), &settled),
            vec![path]
        );
        assert!(ledger.paths().is_empty());
    }

    /// Past the byte bound, the oldest settled bodies go first; a live one is
    /// never reclaimed for the bound.
    #[test]
    fn settled_bodies_past_the_byte_bound_go_oldest_first() {
        let mut ledger = SpillLedger {
            max_settled_bytes: 10,
            ..SpillLedger::default()
        };
        let start = Instant::now();
        for (index, id) in ["m-old", "m-live", "m-new"].into_iter().enumerate() {
            ledger.own(PathBuf::from(format!("/x/{id}")), id, 6);
            let live: HashSet<&str> = ["m-live"].into();
            let reclaimed = ledger.reclaim(start + Duration::from_secs(index as u64), &live);
            if id == "m-new" {
                assert_eq!(reclaimed, vec![PathBuf::from("/x/m-old")]);
            } else {
                assert!(reclaimed.is_empty(), "{id}: {reclaimed:?}");
            }
        }
        assert_eq!(
            ledger.paths(),
            vec![PathBuf::from("/x/m-live"), PathBuf::from("/x/m-new")]
        );
    }

    /// Only a regular file named as this deck's saved body, in a
    /// `.dot-agent-deck` directory, is removed.
    #[test]
    fn only_this_decks_saved_bodies_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let own = spill_in(dir.path(), "1");
        let other_deck = crate::orchestrator_context::write_new_coordination_file(
            dir.path(),
            &spill_file_name(&DeckId::parse("zzzzzzzz").unwrap(), "1"),
            "x",
        )
        .unwrap();
        let outside = dir.path().join(spill_file_name(&deck(), "2"));
        std::fs::write(&outside, "x").unwrap();
        for path in [&own, &other_deck, &outside] {
            remove_spill(path, &deck());
        }
        assert!(!own.exists());
        assert!(other_deck.exists() && outside.exists());
    }

    /// A daemon starting up takes over a manifest no running daemon holds: it
    /// removes the listed bodies older than the retention, tracks the younger
    /// ones, and leaves a manifest a running daemon holds alone.
    #[test]
    fn startup_takes_over_the_manifests_of_runs_that_ended() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let old = spill_in(work.path(), "old");
        let young = spill_in(work.path(), "young");
        let held = spill_in(work.path(), "held");
        let stranger = work.path().join("an-agents-file.md");
        std::fs::write(&stranger, "x").unwrap();
        let aged = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(aged)
            .unwrap();

        // A run that ended, and one still running, which opened its ledger
        // while the other still ran.
        let open = |retention| {
            SpillManifest::open(state.path(), &deck(), retention, Instant::now()).unwrap()
        };
        let mut ended = SpillLedger::default();
        let (mut ended_manifest, _) = open(DEFAULT_SPILL_RETENTION);
        for path in [&old, &young, &stranger] {
            ended.own(path.clone(), "m-gone", 1);
        }
        ended_manifest.write(&ended.manifest_text());
        let mut running = SpillLedger::default();
        let (mut running_manifest, _) = open(DEFAULT_SPILL_RETENTION);
        running.own(held.clone(), "m-live", 1);
        running_manifest.write(&running.manifest_text());
        drop(ended_manifest);

        let mut started = SpillLedger {
            retention: Duration::from_secs(60 * 60),
            ..SpillLedger::default()
        };
        let now = Instant::now();
        let (_started_manifest, taken) =
            SpillManifest::open(state.path(), &deck(), started.retention, now).unwrap();
        started.adopt(taken);
        assert!(!old.exists(), "a body older than the retention is removed");
        assert!(young.exists() && held.exists() && stranger.exists());
        assert_eq!(started.paths(), vec![young.clone()]);
        // The taken-over body is reclaimed once its retention runs out.
        assert_eq!(
            started.reclaim(now + Duration::from_secs(60 * 60), &HashSet::new()),
            vec![young]
        );
        let manifests = std::fs::read_dir(state.path().join(MANIFEST_DIR))
            .unwrap()
            .count();
        assert_eq!(manifests, 2, "the ended run's manifest is gone");
        drop(running_manifest);
    }
}
