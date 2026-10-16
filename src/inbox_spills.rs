//! PR #1735 fifth and sixth reviews (B-05, B2-01..B2-04): how long the files
//! the deck saves long message bodies to are kept.
//!
//! A message too long to hand over whole names a file holding all of it
//! ([`crate::inbox_hub::retain_full_body_minted`]). The deck tracks the files
//! it wrote itself here — never a `spill_path` a caller supplied — and keeps
//! each one:
//!
//! * while its message is live;
//! * for a delegated task, until the task is retired — its worker's
//!   `work-done`, a newer task superseding it, or the worker exiting — since a
//!   worker that acknowledged the excerpt may still be working from the file;
//! * then for [`DEFAULT_SPILL_RETENTION`] after it settled.
//!
//! Settled files past [`DEFAULT_MAX_SETTLED_SPILL_BYTES`] are removed oldest
//! first, sooner, but only those settled at least
//! [`DEFAULT_SPILL_PROTECTED_GRACE`] ago: when every file over the bound is
//! within that grace, the bound is exceeded rather than a file removed.
//!
//! Each daemon run lists its files, with when each settled, in an inventory of
//! its own under the state directory, beside a lock file it holds for as long
//! as the run lasts; the inventory is replaced whole, by rename, so a failed
//! write leaves the previous one. A daemon starting up takes over the
//! inventories no running daemon holds: it measures each file's retention from
//! when it settled, and gives a file whose message had not settled — or whose
//! state is unknown — a fresh retention from the takeover, which it records so
//! a later restart does not start it again. A taken-over inventory is removed
//! only once this run's inventory lists what it held; one that cannot be read
//! is left in place. A removal that fails stays tracked and is tried again.
//!
//! This cleanup removes only a tracked path matching this deck's naming: a
//! regular file named as this deck mints them, directly inside a
//! `.dot-agent-deck` directory that is not a symlink, checked by path at the
//! moment of removal. It does not pin the identity of the file or directory
//! the deck created, so a process able to rename entries in that directory
//! can still have a file it put at a tracked path removed.

use std::collections::HashSet;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::inbox::{DeckId, Recipient};

/// How long a saved body is kept after its message settled.
pub const DEFAULT_SPILL_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// How long after its message settled a saved body is spared by the byte
/// bound ([`DEFAULT_MAX_SETTLED_SPILL_BYTES`]).
pub const DEFAULT_SPILL_PROTECTED_GRACE: Duration = Duration::from_secs(60 * 60);

/// The most bytes of saved bodies whose messages settled that are kept, once
/// their protected grace has passed.
pub const DEFAULT_MAX_SETTLED_SPILL_BYTES: u64 = 64 * 1024 * 1024;

/// The directory under the state directory holding each run's inventory.
const MANIFEST_DIR: &str = "inbox-spills";

/// The largest inventory a takeover reads; a larger one is left in place.
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

/// The inventory's format, so a later one can be told apart.
const MANIFEST_VERSION: u32 = 1;

/// The prefix of every saved body's file name.
const SPILL_PREFIX: &str = "inbox-message-";

/// The name of a saved body for `deck`: `inbox-message-<deck>-<unique>.md`.
pub fn spill_file_name(deck: &DeckId, unique: &str) -> String {
    format!("{SPILL_PREFIX}{deck}-{unique}.md")
}

/// `path` matches this deck's naming for a saved body: a regular file — not a
/// symlink — named by [`spill_file_name`] for `deck`, directly inside a
/// `.dot-agent-deck` directory that is not a symlink. Checked by path now;
/// see the module docs for what that does not establish.
fn is_own_spill(path: &Path, deck: &DeckId) -> bool {
    let named = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.strip_prefix(SPILL_PREFIX)
                .and_then(|rest| rest.strip_prefix(deck.as_str()))
                .is_some_and(|rest| rest.starts_with('-') && rest.ends_with(".md"))
        });
    let Some(parent) = path.parent() else {
        return false;
    };
    let placed = parent
        .file_name()
        .is_some_and(|dir| dir == crate::orchestrator_context::CONTEXT_DIR_NAME)
        && std::fs::symlink_metadata(parent).is_ok_and(|meta| meta.file_type().is_dir());
    named && placed && std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
}

/// What [`remove_spill`] did.
#[derive(Debug)]
pub enum Removal {
    Removed,
    /// Nothing was there any more.
    Gone,
    /// The path does not match this deck's naming ([`is_own_spill`]), so it
    /// was left alone.
    NotOurs,
    /// The removal failed; it is worth trying again.
    Failed(std::io::Error),
}

/// Remove `path` when it matches `deck`'s naming for a saved body
/// ([`is_own_spill`]).
pub fn remove_spill(path: &Path, deck: &DeckId) -> Removal {
    if std::fs::symlink_metadata(path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        return Removal::Gone;
    }
    if !is_own_spill(path, deck) {
        debug!(path = %path.display(), "inbox: not removing a file that is not a saved body");
        return Removal::NotOurs;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {
            debug!(path = %path.display(), "inbox: removed a saved body");
            Removal::Removed
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Removal::Gone,
        Err(error) => Removal::Failed(error),
    }
}

/// The worker a delegated task's saved body is held for until its task is
/// retired ([`SpillLedger::own_task`]).
#[derive(Debug)]
struct TaskHold {
    pane_id: String,
    agent_id: Option<String>,
}

/// One saved body the deck wrote.
#[derive(Debug)]
struct OwnedSpill {
    path: PathBuf,
    /// The message naming it; `None` for one taken over from an earlier run,
    /// whose messages are gone.
    message: Option<String>,
    bytes: u64,
    /// Set while the body belongs to a delegated task not yet retired.
    task: Option<TaskHold>,
    /// When its message was first seen settled or gone, or, taken over with no
    /// recorded settlement, when it was taken over.
    settled_at: Option<Instant>,
    /// The same moment by the wall clock, which the inventory records.
    settled_wall: Option<SystemTime>,
    /// How many removals of it have failed.
    failed_removals: u32,
}

/// The saved bodies one daemon run tracks. See the module docs.
#[derive(Debug)]
pub struct SpillLedger {
    owned: Vec<OwnedSpill>,
    pub retention: Duration,
    pub protected_grace: Duration,
    pub max_settled_bytes: u64,
}

impl Default for SpillLedger {
    fn default() -> Self {
        Self {
            owned: Vec::new(),
            retention: DEFAULT_SPILL_RETENTION,
            protected_grace: DEFAULT_SPILL_PROTECTED_GRACE,
            max_settled_bytes: DEFAULT_MAX_SETTLED_SPILL_BYTES,
        }
    }
}

/// What one [`SpillLedger::reclaim`] answered.
#[derive(Debug, Default)]
pub struct Reclaim {
    /// The bodies no longer tracked, to remove.
    pub removed: Vec<Reclaimed>,
    /// Whether what the inventory records changed: a body settled, or one
    /// stopped being tracked.
    pub changed: bool,
}

/// A body [`SpillLedger::reclaim`] stopped tracking, to remove. One whose
/// removal failed goes back with [`SpillLedger::restore`].
#[derive(Debug)]
pub struct Reclaimed(OwnedSpill);

impl Reclaimed {
    pub fn path(&self) -> &Path {
        &self.0.path
    }

    /// Record a failed removal, answering whether it is the first, so a
    /// failure that repeats every tick is reported once.
    pub fn failed(&mut self) -> bool {
        self.0.failed_removals += 1;
        self.0.failed_removals == 1
    }
}

impl SpillLedger {
    /// Track `path`, a body of `bytes` saved for message `message`.
    pub fn own(&mut self, path: PathBuf, message: &str, bytes: u64) {
        self.owned.push(OwnedSpill {
            path,
            message: Some(message.to_string()),
            bytes,
            task: None,
            settled_at: None,
            settled_wall: None,
            failed_removals: 0,
        });
    }

    /// [`Self::own`] for a delegated task to `worker`: held, whatever its
    /// message's state, until [`Self::release_tasks`] releases it.
    pub fn own_task(&mut self, path: PathBuf, message: &str, bytes: u64, worker: &Recipient) {
        self.own(path, message, bytes);
        if let Some(spill) = self.owned.last_mut() {
            spill.task = Some(TaskHold {
                pane_id: worker.pane_id.clone(),
                agent_id: worker.agent_id.clone(),
            });
        }
    }

    /// Release the task bodies held for `pane_id` that `retired` selects, by
    /// message id and the worker's agent: they settle at the next
    /// [`Self::reclaim`] once their messages are not live.
    pub fn release_tasks(&mut self, pane_id: &str, retired: impl Fn(&str, Option<&str>) -> bool) {
        for spill in &mut self.owned {
            let release = spill.task.as_ref().is_some_and(|task| {
                task.pane_id == pane_id
                    && retired(
                        spill.message.as_deref().unwrap_or_default(),
                        task.agent_id.as_deref(),
                    )
            });
            if release {
                spill.task = None;
            }
        }
    }

    /// Track the bodies an ended run left ([`SpillManifest::open`]), once
    /// each, and answer the inventories they came from, to release once this
    /// run's inventory lists them.
    pub fn adopt(&mut self, taken: TakenOver) -> TakenSources {
        let mut known: HashSet<PathBuf> = self.owned.iter().map(|s| s.path.clone()).collect();
        for spill in taken.spills {
            if known.insert(spill.path.clone()) {
                self.owned.push(spill);
            }
        }
        TakenSources(taken.sources)
    }

    /// Track again the bodies whose removal failed, to try again later.
    pub fn restore(&mut self, failed: Vec<Reclaimed>) {
        self.owned
            .extend(failed.into_iter().map(|reclaimed| reclaimed.0));
    }

    /// What this run's inventory records: every body tracked now, with when
    /// it settled. A path that is not UTF-8 cannot be recorded, and is not
    /// cleaned up after this run ends.
    pub fn manifest_text(&self) -> String {
        let spills = self
            .owned
            .iter()
            .filter_map(|spill| {
                Some(ManifestEntry {
                    path: spill.path.to_str()?.to_string(),
                    bytes: spill.bytes,
                    settled_unix_ms: spill
                        .settled_wall
                        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
                        .map(|since| since.as_millis() as u64),
                })
            })
            .collect();
        serde_json::to_string(&ManifestDoc {
            version: MANIFEST_VERSION,
            spills,
        })
        .unwrap_or_default()
    }

    /// The saved bodies tracked now.
    pub fn paths(&self) -> Vec<PathBuf> {
        self.owned.iter().map(|spill| spill.path.clone()).collect()
    }

    /// Settle every tracked body whose message is not among `live` and that no
    /// task holds, at `now` (`wall` by the wall clock); then stop tracking,
    /// and answer for removal, those settled at least [`Self::retention`]
    /// before `now`, then the oldest settled ones past
    /// [`Self::max_settled_bytes`] that settled at least
    /// [`Self::protected_grace`] before `now`. A body whose message is live,
    /// or that a task holds, is kept whatever its age, and does not count
    /// against the bound.
    pub fn reclaim(&mut self, now: Instant, wall: SystemTime, live: &HashSet<&str>) -> Reclaim {
        let mut changed = false;
        for spill in &mut self.owned {
            let is_live = spill.message.as_deref().is_some_and(|id| live.contains(id));
            if is_live || spill.task.is_some() {
                if spill.settled_at.take().is_some() {
                    spill.settled_wall = None;
                    changed = true;
                }
            } else if spill.settled_at.is_none() {
                spill.settled_at = Some(now);
                spill.settled_wall = Some(wall);
                changed = true;
            }
        }
        let past = |spill: &OwnedSpill, age: Duration| {
            spill
                .settled_at
                .is_some_and(|at| now.saturating_duration_since(at) >= age)
        };
        let mut removed = Vec::new();
        let mut kept = Vec::with_capacity(self.owned.len());
        for spill in std::mem::take(&mut self.owned) {
            if past(&spill, self.retention) {
                removed.push(Reclaimed(spill));
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
                .filter(|(_, spill)| past(spill, self.protected_grace))
                .filter_map(|(index, spill)| spill.settled_at.map(|at| (at, index)))
                .min()
                .map(|(_, index)| index)
            else {
                // Everything over the bound is within its grace.
                break;
            };
            let spill = self.owned.remove(oldest);
            settled_bytes -= spill.bytes;
            removed.push(Reclaimed(spill));
        }
        changed |= !removed.is_empty();
        Reclaim { removed, changed }
    }
}

/// The inventory's on-disk form.
#[derive(Debug, Serialize, Deserialize)]
struct ManifestDoc {
    version: u32,
    spills: Vec<ManifestEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestEntry {
    path: String,
    bytes: u64,
    /// When its message settled, by the wall clock; `None` while it had not,
    /// or a task held it.
    #[serde(default)]
    settled_unix_ms: Option<u64>,
}

/// The saved bodies [`SpillManifest::open`] took over from ended runs, for
/// [`SpillLedger::adopt`].
#[derive(Debug, Default)]
pub struct TakenOver {
    spills: Vec<OwnedSpill>,
    sources: Vec<TakenSource>,
}

/// An ended run's inventory a takeover read, its lock still held.
#[derive(Debug)]
struct TakenSource {
    _lock: std::fs::File,
    lock_path: PathBuf,
    list_path: PathBuf,
}

/// The inventories [`SpillLedger::adopt`] took bodies from, locked. Released
/// with [`Self::release`] once this run's inventory records those bodies;
/// dropped instead, they are left for the next run to take over.
#[derive(Debug, Default)]
#[must_use = "taken-over inventories stay in place unless released"]
pub struct TakenSources(Vec<TakenSource>);

impl TakenSources {
    /// Remove each taken-over inventory, then its lock file, while holding
    /// its lock. A lock file whose inventory could not be removed is kept, so
    /// that inventory is taken over again.
    pub fn release(self) {
        for source in self.0 {
            match std::fs::remove_file(&source.list_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    warn!(path = %source.list_path.display(), %error, "inbox: could not remove a taken-over inventory");
                    continue;
                }
            }
            if let Err(error) = std::fs::remove_file(&source.lock_path) {
                warn!(path = %source.lock_path.display(), %error, "inbox: could not remove a taken-over inventory's lock");
            }
        }
    }
}

/// One daemon run's inventory of its saved bodies, beside a lock file held
/// while the run lasts.
#[derive(Debug)]
pub struct SpillManifest {
    dir: PathBuf,
    unique: String,
    _lock: std::fs::File,
}

impl SpillManifest {
    /// Keep this run's inventory under `state_dir`, and take over the
    /// inventories no running daemon holds ([`take_over_orphans`]): the bodies
    /// of `deck` they list that settled at least `retention` before `wall`
    /// are removed, and the rest answered to be tracked — settled when they
    /// were recorded settled, or at `now` when no settlement was recorded.
    pub fn open(
        state_dir: &Path,
        deck: &DeckId,
        retention: Duration,
        now: Instant,
        wall: SystemTime,
    ) -> std::io::Result<(Self, TakenOver)> {
        let dir = state_dir.join(MANIFEST_DIR);
        crate::platform::fsperm::create_owner_only_dir(&dir)?;
        let (unique, lock) = create_locked_run(&dir)?;
        let (entries, sources) = take_over_orphans(&dir, deck);
        let mut spills = Vec::new();
        for entry in entries {
            let recorded = entry
                .settled_unix_ms
                .map(|ms| UNIX_EPOCH + Duration::from_millis(ms));
            let (settled_wall, age) = match recorded {
                Some(at) => (at, wall.duration_since(at).unwrap_or_default()),
                None => (wall, Duration::ZERO),
            };
            let mut spill = OwnedSpill {
                path: PathBuf::from(entry.path),
                message: None,
                bytes: entry.bytes,
                task: None,
                settled_at: Some(now.checked_sub(age).unwrap_or(now)),
                settled_wall: Some(settled_wall),
                failed_removals: 0,
            };
            if age >= retention {
                match remove_spill(&spill.path, deck) {
                    Removal::Failed(error) => {
                        warn!(path = %spill.path.display(), %error, "inbox: could not remove a saved body; it is tried again");
                        spill.failed_removals = 1;
                    }
                    _ => continue,
                }
            }
            spills.push(spill);
        }
        Ok((
            Self {
                dir,
                unique,
                _lock: lock,
            },
            TakenOver { spills, sources },
        ))
    }

    /// Replace the inventory with `text` ([`SpillLedger::manifest_text`]):
    /// written whole to a temporary file, synced, and renamed into place, so
    /// a failed write leaves the previous inventory as it was.
    pub fn write(&mut self, text: &str) -> std::io::Result<()> {
        let pending = self.dir.join(format!(".pending-{}.list", self.unique));
        let _ = std::fs::remove_file(&pending);
        let written = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.create_new(true).write(true);
            crate::platform::fsperm::set_create_mode_owner_only(&mut options);
            let mut file = options.open(&pending)?;
            crate::platform::fsperm::set_file_owner_only(&file)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&pending, list_path(&self.dir, &self.unique))?;
            sync_dir(&self.dir);
            Ok(())
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&pending);
        }
        written
    }
}

fn lock_path(dir: &Path, unique: &str) -> PathBuf {
    dir.join(format!("run-{unique}.lock"))
}

fn list_path(dir: &Path, unique: &str) -> PathBuf {
    dir.join(format!("run-{unique}.list"))
}

/// Make a rename in `dir` durable, where the platform can.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = std::fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Create a run's lock file in `dir` and lock it before it can be taken for an
/// orphan's: created under a temporary name, locked, then renamed into place.
/// The lock stays on the file through the rename and lasts while the returned
/// handle is open; the file is never replaced while the run lasts.
fn create_locked_run(dir: &Path) -> std::io::Result<(String, std::fs::File)> {
    let mut unique = [0u8; 8];
    getrandom::fill(&mut unique).map_err(std::io::Error::other)?;
    let unique: String = unique.iter().map(|b| format!("{b:02x}")).collect();
    let pending = dir.join(format!(".pending-{unique}"));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    crate::platform::fsperm::set_create_mode_owner_only(&mut options);
    let file = options.open(&pending)?;
    if let Err(error) = file.try_lock() {
        let _ = std::fs::remove_file(&pending);
        return Err(std::io::Error::other(format!(
            "could not lock the saved-body inventory: {error}"
        )));
    }
    std::fs::rename(&pending, lock_path(dir, &unique))?;
    Ok((unique, file))
}

/// Open `path` for reading without following a symlink at it, and without an
/// open that blocks on a FIFO; answered only when it is a regular file.
fn open_regular(path: &Path) -> std::io::Result<std::fs::File> {
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "it is a symlink",
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "it is not a regular file",
        ));
    }
    Ok(file)
}

/// An ended run's inventory at `path`: `Ok(None)` when there is none, and an
/// error — which leaves it in place — when it is not a regular file, is larger
/// than [`MAX_MANIFEST_BYTES`], or does not parse.
fn read_inventory(path: &Path) -> std::io::Result<Option<Vec<ManifestEntry>>> {
    let file = match open_regular(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let too_large = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("it is larger than {MAX_MANIFEST_BYTES} bytes"),
        )
    };
    if file.metadata()?.len() > MAX_MANIFEST_BYTES {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(too_large());
    }
    let doc: ManifestDoc = serde_json::from_slice(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if doc.version != MANIFEST_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("its format version is {}", doc.version),
        ));
    }
    Ok(Some(doc.spills))
}

/// Every run's inventory in `dir` that no running daemon holds — its lock
/// file's lock is free — is read, and the entries it lists that match `deck`'s
/// naming answered, with the inventory itself to release once they are
/// recorded elsewhere. An inventory some running daemon holds is left alone,
/// and so is one that cannot be read, still locked by nobody.
fn take_over_orphans(dir: &Path, deck: &DeckId) -> (Vec<ManifestEntry>, Vec<TakenSource>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Default::default();
    };
    let mut found = Vec::new();
    let mut sources = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with(".pending-") {
            // A run's lock file before it was in place, or an inventory being
            // written: a run just starting holds its lock within moments of
            // creating it, and a write renames its file away within moments.
            let Ok(file) = open_regular(&path) else {
                continue;
            };
            if file.try_lock().is_err() {
                continue;
            }
            let stale = file
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| SystemTime::now().duration_since(at).ok())
                .is_some_and(|age| age >= Duration::from_secs(60));
            if stale {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        let Some(unique) = name
            .strip_prefix("run-")
            .and_then(|rest| rest.strip_suffix(".lock"))
        else {
            continue;
        };
        let Ok(lock) = open_regular(&path) else {
            continue;
        };
        if lock.try_lock().is_err() {
            continue;
        }
        let list = list_path(dir, unique);
        let listed = match read_inventory(&list) {
            Ok(listed) => listed.unwrap_or_default(),
            Err(error) => {
                warn!(path = %list.display(), %error, "inbox: could not read an ended run's saved-body inventory; it is left in place");
                continue;
            }
        };
        found.extend(
            listed
                .into_iter()
                .filter(|entry| is_own_spill(Path::new(&entry.path), deck)),
        );
        sources.push(TakenSource {
            _lock: lock,
            lock_path: path,
            list_path: list,
        });
    }
    (found, sources)
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

    fn paths(reclaim: Reclaim) -> Vec<PathBuf> {
        reclaim
            .removed
            .into_iter()
            .map(|r| r.path().to_path_buf())
            .collect()
    }

    fn open(state: &Path, retention: Duration, wall: SystemTime) -> (SpillManifest, TakenOver) {
        SpillManifest::open(state, &deck(), retention, Instant::now(), wall).unwrap()
    }

    fn inventories(state: &Path) -> usize {
        std::fs::read_dir(state.join(MANIFEST_DIR))
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".lock")
            })
            .count()
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
        let wall = SystemTime::now();
        let live: HashSet<&str> = ["m-1"].into();
        assert!(ledger.reclaim(start, wall, &live).removed.is_empty());
        let settled = HashSet::new();
        let first = ledger.reclaim(start, wall, &settled);
        assert!(
            first.removed.is_empty() && first.changed,
            "a body is kept right after its message settles, which is recorded"
        );
        assert!(
            ledger
                .reclaim(start + Duration::from_secs(59), wall, &settled)
                .removed
                .is_empty()
        );
        assert_eq!(
            paths(ledger.reclaim(start + Duration::from_secs(60), wall, &settled)),
            vec![path]
        );
        assert!(ledger.paths().is_empty());
    }

    /// Past the byte bound, the oldest settled bodies past their protected
    /// grace go first; a live one is never reclaimed for the bound, and while
    /// every body over it is within its grace the bound is exceeded.
    #[test]
    fn settled_bodies_past_the_byte_bound_go_oldest_first_after_their_grace() {
        let mut ledger = SpillLedger {
            max_settled_bytes: 10,
            protected_grace: Duration::from_secs(10),
            ..SpillLedger::default()
        };
        let start = Instant::now();
        let wall = SystemTime::now();
        let live: HashSet<&str> = ["m-live"].into();
        for (index, id) in ["m-old", "m-live", "m-new"].into_iter().enumerate() {
            ledger.own(PathBuf::from(format!("/x/{id}")), id, 6);
            let reclaimed = ledger.reclaim(start + Duration::from_secs(index as u64), wall, &live);
            assert!(reclaimed.removed.is_empty(), "{id}: within the grace");
        }
        assert_eq!(ledger.paths().len(), 3, "the bound is exceeded meanwhile");
        assert_eq!(
            paths(ledger.reclaim(start + Duration::from_secs(10), wall, &live)),
            vec![PathBuf::from("/x/m-old")]
        );
        assert_eq!(
            ledger.paths(),
            vec![PathBuf::from("/x/m-live"), PathBuf::from("/x/m-new")]
        );
    }

    /// A delegated task's body is held, whatever its message's state and the
    /// byte bound, until its task is retired; then it settles like any other.
    #[test]
    fn a_task_body_is_held_until_its_task_is_retired() {
        let mut ledger = SpillLedger {
            max_settled_bytes: 0,
            protected_grace: Duration::ZERO,
            retention: Duration::from_secs(60),
            ..SpillLedger::default()
        };
        let worker = Recipient::bound("w", "wa");
        ledger.own_task(PathBuf::from("/x/task"), "m-task", 6, &worker);
        let start = Instant::now();
        let wall = SystemTime::now();
        let none = HashSet::new();
        let later = start + Duration::from_secs(600);
        assert!(ledger.reclaim(later, wall, &none).removed.is_empty());
        ledger.release_tasks("other", |_, _| true);
        ledger.release_tasks("w", |_, agent| agent == Some("another-agent"));
        assert!(ledger.reclaim(later, wall, &none).removed.is_empty());
        ledger.release_tasks("w", |id, agent| id == "m-task" && agent == Some("wa"));
        assert_eq!(
            paths(ledger.reclaim(later, wall, &none)),
            vec![PathBuf::from("/x/task")]
        );
    }

    /// Only a regular file named as this deck's saved body, in a
    /// `.dot-agent-deck` directory that is not a symlink, is removed.
    #[test]
    fn only_paths_matching_this_decks_naming_are_removed() {
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

    /// A tracked path whose `.dot-agent-deck` directory was swapped for a
    /// symlink is not removed through it.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_context_directory_is_not_removed_through() {
        let work = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("victims");
        std::fs::create_dir(&target).unwrap();
        let victim = target.join(spill_file_name(&deck(), "1"));
        std::fs::write(&victim, "x").unwrap();
        let context = work
            .path()
            .join(crate::orchestrator_context::CONTEXT_DIR_NAME);
        std::os::unix::fs::symlink(&target, &context).unwrap();
        let tracked = context.join(spill_file_name(&deck(), "1"));
        assert!(matches!(remove_spill(&tracked, &deck()), Removal::NotOurs));
        assert!(victim.exists());
    }

    /// A daemon starting up takes over an inventory no running daemon holds:
    /// it removes the listed bodies that settled longer than the retention
    /// ago, tracks the rest, and leaves an inventory a running daemon holds
    /// alone. The taken-over inventory goes only when released.
    #[test]
    fn startup_takes_over_the_inventories_of_runs_that_ended() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let old = spill_in(work.path(), "old");
        let young = spill_in(work.path(), "young");
        let held = spill_in(work.path(), "held");
        let stranger = work.path().join("an-agents-file.md");
        std::fs::write(&stranger, "x").unwrap();
        let hour = Duration::from_secs(60 * 60);
        let wall = SystemTime::now();

        // A run that ended, and one still running.
        let mut ended = SpillLedger::default();
        let (mut ended_manifest, _) = open(state.path(), DEFAULT_SPILL_RETENTION, wall);
        for (path, id) in [(&old, "m-old"), (&young, "m-young"), (&stranger, "m-x")] {
            ended.own(path.clone(), id, 1);
        }
        let start = Instant::now();
        ended.reclaim(start, wall - 2 * hour, &["m-young"].into());
        ended.reclaim(
            start,
            wall - 30 * 60 * Duration::from_secs(1),
            &HashSet::new(),
        );
        ended_manifest.write(&ended.manifest_text()).unwrap();
        let mut running = SpillLedger::default();
        let (mut running_manifest, _) = open(state.path(), DEFAULT_SPILL_RETENTION, wall);
        running.own(held.clone(), "m-live", 1);
        running_manifest.write(&running.manifest_text()).unwrap();
        drop(ended_manifest);

        let mut started = SpillLedger {
            retention: hour,
            ..SpillLedger::default()
        };
        let now = Instant::now();
        let (_started_manifest, taken) =
            SpillManifest::open(state.path(), &deck(), started.retention, now, wall).unwrap();
        let sources = started.adopt(taken);
        assert!(!old.exists(), "settled longer than the retention ago");
        assert!(young.exists() && held.exists() && stranger.exists());
        assert_eq!(started.paths(), vec![young.clone()]);
        assert_eq!(inventories(state.path()), 3, "kept until released");
        sources.release();
        assert_eq!(inventories(state.path()), 2, "the ended run's is gone");
        // Settled half an hour ago, so the half-hour left is what it keeps.
        let none = HashSet::new();
        assert!(
            started
                .reclaim(now + 29 * 60 * Duration::from_secs(1), wall, &none)
                .removed
                .is_empty()
        );
        assert_eq!(
            paths(started.reclaim(now + 30 * 60 * Duration::from_secs(1), wall, &none)),
            vec![young]
        );
        drop(running_manifest);
    }

    /// PR #1735 sixth review (B2-01): a body in an inventory with no recorded
    /// settlement — its message was live when the run ended, or a task held it
    /// — gets a fresh retention from the takeover, however old the file, and
    /// a later restart keeps that start instead of granting another.
    #[test]
    fn a_body_of_unknown_state_gets_one_fresh_retention_across_restarts() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let body = spill_in(work.path(), "unknown");
        std::fs::File::options()
            .write(true)
            .open(&body)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60))
            .unwrap();
        let hour = Duration::from_secs(60 * 60);
        let wall = SystemTime::now();
        let (mut first, _) = open(state.path(), hour, wall);
        let mut ledger = SpillLedger::default();
        ledger.own(body.clone(), "m-live", 14);
        first.write(&ledger.manifest_text()).unwrap();
        drop(first);

        // Taken over: a fresh retention, recorded.
        let mut second = SpillLedger::default();
        let (mut second_manifest, taken) = open(state.path(), hour, wall);
        second.adopt(taken).release();
        assert!(
            body.exists(),
            "an unknown state is not taken as settled long ago"
        );
        second_manifest.write(&second.manifest_text()).unwrap();
        drop(second_manifest);

        // Half an hour on, the retention runs from the first takeover.
        let mut third = SpillLedger::default();
        let (mut third_manifest, taken) = open(state.path(), hour, wall + hour / 2);
        third.adopt(taken).release();
        assert_eq!(third.paths(), vec![body.clone()]);
        third_manifest.write(&third.manifest_text()).unwrap();
        drop(third_manifest);
        let (_fourth, taken) = open(state.path(), hour, wall + hour);
        assert!(taken.spills.is_empty());
        assert!(!body.exists(), "its one retention ran out");
    }

    /// PR #1735 sixth review (B2-04, B2-03): an inventory that cannot be read
    /// — not JSON, too large, a symlink or a FIFO — is left in place, and
    /// reading it never blocks the startup.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_inventory_is_left_in_place() {
        let state = tempfile::tempdir().unwrap();
        let dir = state.path().join(MANIFEST_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let decoy = state.path().join("decoy");
        std::fs::write(&decoy, "{}").unwrap();
        type Make<'a> = (&'a str, &'a dyn Fn(&Path));
        let make: [Make; 4] = [
            ("garbled", &|p| std::fs::write(p, "not json").unwrap()),
            ("large", &|p| {
                let f = std::fs::File::create(p).unwrap();
                f.set_len(MAX_MANIFEST_BYTES + 1).unwrap();
            }),
            ("link", &|p| std::os::unix::fs::symlink(&decoy, p).unwrap()),
            ("fifo", &|p| {
                let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            }),
        ];
        for (unique, create) in make {
            std::fs::write(lock_path(&dir, unique), "").unwrap();
            create(&list_path(&dir, unique));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let state_path = state.path().to_path_buf();
        std::thread::spawn(move || {
            let (_manifest, taken) = SpillManifest::open(
                &state_path,
                &deck(),
                DEFAULT_SPILL_RETENTION,
                Instant::now(),
                SystemTime::now(),
            )
            .unwrap();
            let _ = tx.send((taken.spills.len(), taken.sources.len()));
        });
        let taken = rx.recv_timeout(Duration::from_secs(10));
        assert_eq!(taken, Ok((0, 0)), "nothing taken, and nothing blocked");
        for unique in ["garbled", "large", "link", "fifo"] {
            assert!(
                std::fs::symlink_metadata(list_path(&dir, unique)).is_ok(),
                "{unique} is left in place"
            );
            assert!(lock_path(&dir, unique).exists(), "{unique}'s lock is kept");
        }
    }

    /// PR #1735 sixth review (B2-04): taken-over inventories that are not
    /// released — this run's inventory did not record their bodies — stay for
    /// the next run to take over.
    #[test]
    fn an_unreleased_takeover_leaves_the_inventory_for_the_next_run() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let body = spill_in(work.path(), "kept");
        let wall = SystemTime::now();
        let (mut ended, _) = open(state.path(), DEFAULT_SPILL_RETENTION, wall);
        let mut ledger = SpillLedger::default();
        ledger.own(body.clone(), "m", 1);
        ended.write(&ledger.manifest_text()).unwrap();
        drop(ended);
        let (first, taken) = open(state.path(), DEFAULT_SPILL_RETENTION, wall);
        let mut ledger = SpillLedger::default();
        drop(ledger.adopt(taken));
        drop(first);
        let (_second, taken) = open(state.path(), DEFAULT_SPILL_RETENTION, wall);
        let mut ledger = SpillLedger::default();
        ledger.adopt(taken).release();
        assert_eq!(ledger.paths(), vec![body]);
    }

    /// PR #1735 sixth review (B2-04): the inventory is replaced whole beside a
    /// lock file that stays: a running daemon's inventory is never taken over
    /// across its rewrites, and the last write is what a takeover reads.
    #[test]
    fn the_inventory_is_replaced_beside_a_stable_lock() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let first = spill_in(work.path(), "first");
        let second = spill_in(work.path(), "second");
        let wall = SystemTime::now();
        let (mut manifest, _) = open(state.path(), DEFAULT_SPILL_RETENTION, wall);
        let mut ledger = SpillLedger::default();
        ledger.own(first.clone(), "m-1", 1);
        manifest.write(&ledger.manifest_text()).unwrap();
        ledger.own(second.clone(), "m-2", 1);
        manifest.write(&ledger.manifest_text()).unwrap();
        let (_other, taken) = open(state.path(), DEFAULT_SPILL_RETENTION, wall);
        assert!(taken.sources.is_empty(), "a running daemon's is left alone");
        drop(manifest);
        let (_next, taken) = open(state.path(), DEFAULT_SPILL_RETENTION, wall);
        let mut next = SpillLedger::default();
        next.adopt(taken).release();
        assert_eq!(next.paths(), vec![first, second]);
    }
}
