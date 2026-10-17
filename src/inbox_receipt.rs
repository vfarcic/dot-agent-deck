//! PRD #1666 design v3.2 §3 and §5: the receipt spool, its trusted traversal
//! and bounded scans, and the suppression-evidence store.
//!
//! A receiver component publishes an immutable stage record for a delivery
//! attempt (`<msg>.<A>.<e>.<stage>`) into its agent generation's spool before
//! the daemon is asked anything, and the daemon reads it back. Since §13 step
//! 4 the daemon starts one [`Spool`] per run and the hub's receipt engine
//! (`crate::inbox_engine`) reads it before it lets an attempt lapse; an
//! attempt's files are removed when its evidence reservation is returned.
//!
//! A spool record is bearer evidence: whoever holds an attempt's cap can
//! publish it. So it carries only the stages whose meaning holds whoever bears
//! the cap, `Handed` and `Committed`. `NotHanded`, `HandedInert` and revoke
//! answers release or close an attempt for one receiver, so they travel only
//! on the authenticated current-incarnation control path, where a retired
//! receiver is refused.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The spool root's name under the deck's state directory.
pub const SPOOL_DIR_NAME: &str = "inbox-spool";
/// The absolute `<root>/<instance>` path the daemon pins into every spawned
/// process.
pub const ENV_INBOX_SPOOL: &str = "DOT_AGENT_DECK_INBOX_SPOOL";
/// A record's largest body.
pub const MAX_RECORD_BYTES: u64 = 4096;
/// Directory entries one scan pass examines, and entries one sibling reap
/// visits.
pub const SCAN_MAX_ENTRIES: usize = 2048;
/// Record bytes one scan pass reads.
pub const SCAN_MAX_BYTES: u64 = 1024 * 1024;

/// What one record read may consume: a whole record plus the byte that proves
/// a file oversized. A pass opens a record only while this much budget is left,
/// so no read can take it past [`SCAN_MAX_BYTES`].
const READ_RESERVE: u64 = MAX_RECORD_BYTES + 1;
/// Names one sibling reap step lists in the spool root.
const ROOT_LIST_MAX: usize = 4 * SCAN_MAX_ENTRIES;
/// How often a running daemon takes its next sibling reap step.
pub const REAP_INTERVAL: Duration = Duration::from_secs(60);
/// How deep below an instance a reap descends: pane, agent, `rec`/`caps`/
/// `revoked`.
const REAP_DEPTH: usize = 3;
/// New instances one start mints before giving up, when a reaper took the
/// pending lock of the one before.
const START_ATTEMPTS: usize = 4;

const TEMP_PREFIX: &str = ".tmp-";
const PENDING_LOCK_PREFIX: &str = ".pending-";
const LOCK_SUFFIX: &str = ".lock";

/// Whether `name` is one spool path component: `[A-Za-z0-9_-]`, 1–128 bytes.
pub fn valid_component(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The stage a spool record states: only the two whose evidence holds whoever
/// bears the cap (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordStage {
    Handed,
    Committed,
}

impl RecordStage {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordStage::Handed => "handed",
            RecordStage::Committed => "committed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "handed" => RecordStage::Handed,
            "committed" => RecordStage::Committed,
            _ => return None,
        })
    }

    /// `Committed` is proven by the commit cap, `Handed` by the hand-out cap,
    /// so a hand-out cap can never become a commit (§5).
    fn needs_commit_cap(self) -> bool {
        self == RecordStage::Committed
    }
}

/// A record's identity, which is also its file name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordKey {
    pub msg: String,
    pub a: u32,
    pub e: u32,
    pub stage: RecordStage,
}

impl RecordKey {
    pub fn file_name(&self) -> String {
        format!("{}.{}.{}.{}", self.msg, self.a, self.e, self.stage.as_str())
    }

    pub fn parse(name: &str) -> Option<Self> {
        let mut parts = name.split('.');
        let msg = parts.next()?;
        let a = parts.next()?.parse().ok()?;
        let e = parts.next()?.parse().ok()?;
        let stage = RecordStage::parse(parts.next()?)?;
        if parts.next().is_some() || !valid_component(msg) {
            return None;
        }
        Some(RecordKey {
            msg: msg.to_string(),
            a,
            e,
            stage,
        })
    }
}

/// A 128-bit OS-random capability (§5). The daemon admits a record by the
/// cap's hash, and also keeps an issued attempt's raw caps in memory
/// ([`EvidenceStore::caps`]) to hand them to a replacement watcher in
/// `rebound`.
#[derive(Clone, PartialEq, Eq)]
pub struct Cap(pub [u8; 16]);

impl std::fmt::Debug for Cap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cap(..)")
    }
}

impl Cap {
    pub fn random() -> io::Result<Cap> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(io::Error::other)?;
        Ok(Cap(bytes))
    }

    pub fn hex(&self) -> String {
        hex(&self.0)
    }

    /// The cap whose [`Self::hex`] is `text`; `None` for anything else.
    pub fn from_hex(text: &str) -> Option<Cap> {
        if text.len() != 32 {
            return None;
        }
        let mut bytes = [0u8; 16];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
        }
        Some(Cap(bytes))
    }

    fn hash_hex(hex_cap: &str) -> [u8; 32] {
        Sha256::digest(hex_cap.as_bytes()).into()
    }
}

/// An attempt's two capabilities.
#[derive(Debug, Clone)]
pub struct AttemptCaps {
    pub handed: Cap,
    pub commit: Cap,
}

/// What a record file holds. Its fields must match its name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordBody {
    pub msg: String,
    pub a: u32,
    pub e: u32,
    pub stage: RecordStage,
    pub cap: String,
}

impl RecordBody {
    pub fn key(&self) -> RecordKey {
        RecordKey {
            msg: self.msg.clone(),
            a: self.a,
            e: self.e,
            stage: self.stage,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_hex(len: usize) -> io::Result<String> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(hex(&bytes))
}

fn random_index(len: usize) -> usize {
    let mut bytes = [0u8; 8];
    if len == 0 || getrandom::fill(&mut bytes).is_err() {
        return 0;
    }
    (u64::from_le_bytes(bytes) % len as u64) as usize
}

fn refused(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, what.to_string())
}

fn invalid(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what.to_string())
}

fn lock_name(instance: &str) -> String {
    format!("{instance}{LOCK_SUFFIX}")
}

fn is_instance(name: &str) -> bool {
    name.len() == 32
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn attempt_name(msg: &str, a: u32, e: u32) -> io::Result<String> {
    if !valid_component(msg) {
        return Err(invalid("a message id must be one spool path component"));
    }
    Ok(format!("{msg}.{a}.{e}"))
}

/// The daemon's spool namespace for one run: `<root>/<instance>/`, held by a
/// lock on `<root>/<instance>.lock` for as long as this value lives.
#[derive(Debug)]
pub struct Spool {
    root_path: PathBuf,
    instance: String,
    root: fsdir::TrustedDir,
    dir: fsdir::TrustedDir,
    root_list_max: usize,
    /// Where the next reap step goes on listing the root.
    cursor: Mutex<Option<fsdir::DirStream>>,
    _lock: File,
}

impl Spool {
    /// Create or open `<state_dir>/inbox-spool`, mint and lock this run's
    /// instance, and reap sibling instances no running daemon holds.
    ///
    /// `state_dir` is the trust anchor: an absolute path to a directory owned
    /// by this user, whose ancestors are trusted as the OS resolves them. The
    /// root and every directory below it are opened one component at a time
    /// and verified: a symlink is refused rather than followed, and a
    /// pre-existing directory open to other users is refused rather than
    /// repaired. The lock file is locked under a pending name and only then
    /// renamed into place, so a reaper never sees an instance's lock free
    /// while its daemon runs; an instance directory with no lock file is
    /// never reaped.
    pub fn start(state_dir: &Path) -> io::Result<Spool> {
        Spool::start_listing(state_dir, ROOT_LIST_MAX)
    }

    /// [`Self::start`], listing at most `root_list_max` root names per reap
    /// step.
    fn start_listing(state_dir: &Path, root_list_max: usize) -> io::Result<Spool> {
        if !state_dir.is_absolute() {
            return Err(invalid(
                "the inbox spool's state directory must be absolute",
            ));
        }
        let anchor = fsdir::TrustedDir::anchor(state_dir)?;
        let root = anchor.child(SPOOL_DIR_NAME, true)?;
        let mut last = None;
        for _ in 0..START_ATTEMPTS {
            match mint(&root) {
                Ok((instance, lock)) => {
                    let dir = root.child(&instance, true)?;
                    let spool = Spool {
                        root_path: state_dir.join(SPOOL_DIR_NAME),
                        instance,
                        root,
                        dir,
                        root_list_max,
                        cursor: Mutex::new(None),
                        _lock: lock,
                    };
                    spool.reap_step();
                    return Ok(spool);
                }
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("could not start an inbox spool")))
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// One bounded step of reaping sibling instances no running daemon
    /// holds, going on from where the previous step stopped listing the
    /// root. [`Self::start`] takes the first; a running daemon takes one
    /// every [`REAP_INTERVAL`].
    pub fn reap_step(&self) {
        let mut cursor = self.cursor.lock().unwrap_or_else(PoisonError::into_inner);
        reap_siblings(&self.root, &self.instance, self.root_list_max, &mut cursor);
    }

    /// The value of [`ENV_INBOX_SPOOL`] for this run.
    pub fn instance_path(&self) -> PathBuf {
        self.root_path.join(&self.instance)
    }

    /// The daemon's side of one agent generation's spool, created as needed.
    pub fn generation(&self, pane: &str, agent: &str) -> io::Result<GenerationSpool> {
        GenerationSpool::at(&self.dir, pane, agent, true)
    }

    /// Remove an exited generation's spool (§2 T9): every record, cap and
    /// revoke marker it held, without following a link. Its pane directory
    /// stays for the pane's next generation.
    pub fn remove_generation(&self, pane: &str, agent: &str) -> io::Result<()> {
        if !valid_component(pane) || !valid_component(agent) {
            return Err(invalid(
                "a spool generation is named by two path components",
            ));
        }
        let pane_dir = match self.dir.child(pane, false) {
            Ok(dir) => dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut budget = usize::MAX;
        if remove_tree(&pane_dir, OsStr::new(agent), 1, &mut budget)? {
            Ok(())
        } else {
            Err(io::Error::other(
                "an exited generation's receipts were not all removed",
            ))
        }
    }
}

/// Mint an instance and lock it under its pending name, then rename the lock
/// into place. A reaper that took the pending lock first makes the lock or
/// the rename fail, and the caller mints another.
fn mint(root: &fsdir::TrustedDir) -> io::Result<(String, File)> {
    let instance = random_hex(16)?;
    let pending = format!("{PENDING_LOCK_PREFIX}{instance}");
    let lock = root.create_new_owner_only(&pending)?;
    if !crate::platform::lock::try_lock_file(&lock)? {
        let _ = root.remove_file(&pending);
        return Err(io::Error::other(
            "could not lock a new inbox spool instance",
        ));
    }
    if let Err(error) = root.rename(&pending, &lock_name(&instance)) {
        let _ = root.remove_file(&pending);
        return Err(error);
    }
    Ok((instance, lock))
}

/// A sibling the reaper may claim.
enum Sibling {
    /// `<instance>.lock`: the instance directory is reaped under its lock.
    Instance(String),
    /// `.pending-<instance>`: a start that died before its rename.
    Pending(String),
}

impl Sibling {
    fn of(name: &OsStr, own: &str) -> Option<Sibling> {
        let name = name.to_str()?;
        if let Some(stem) = name.strip_suffix(LOCK_SUFFIX) {
            return (stem != own && is_instance(stem)).then(|| Sibling::Instance(stem.into()));
        }
        let stem = name.strip_prefix(PENDING_LOCK_PREFIX)?;
        (stem != own && is_instance(stem)).then(|| Sibling::Pending(name.into()))
    }
}

/// One step of reaping sibling instances no running daemon holds, within one
/// shared budget of [`SCAN_MAX_ENTRIES`] entries: try-lock each lock file and
/// keep holding it while its instance is removed, unlinking the lock file
/// last; a busy lock, or a directory with no lock file, is skipped. A pending
/// lock a start left behind is removed under its own lock, never by age.
/// Removal deletes as it enumerates, so a reap cut short by the budget still
/// makes progress and keeps the lock file for a later step.
///
/// The root is listed through `cursor`, kept across steps: a step reads at
/// most `root_list_max` names on from where the previous one stopped, and the
/// end of the listing starts a new pass at the next step. So every entry that
/// stays in the root is listed once per pass, however many names come before
/// it. The siblings one step lists are visited from a random rotation, so one
/// large sibling does not take the budget first in every pass; a sibling the
/// budget does not reach waits for a later pass.
fn reap_siblings(
    root: &fsdir::TrustedDir,
    own: &str,
    root_list_max: usize,
    cursor: &mut Option<fsdir::DirStream>,
) {
    if cursor.is_none() {
        let Ok(stream) = root.stream() else {
            return;
        };
        *cursor = Some(stream);
    }
    let Some(stream) = cursor.as_mut() else {
        return;
    };
    let mut siblings = Vec::new();
    let mut ended = false;
    for _ in 0..root_list_max {
        match stream.next_name() {
            None => {
                ended = true;
                break;
            }
            Some(Err(_)) => continue,
            Some(Ok(name)) => siblings.extend(Sibling::of(&name, own)),
        }
    }
    if ended {
        *cursor = None;
    }
    let start = random_index(siblings.len());
    siblings.rotate_left(start);
    let mut budget = SCAN_MAX_ENTRIES;
    for sibling in siblings {
        if budget == 0 {
            break;
        }
        match sibling {
            Sibling::Instance(stem) => {
                let name = lock_name(&stem);
                let Ok(lock) = root.open_regular(&name) else {
                    continue;
                };
                if !matches!(crate::platform::lock::try_lock_file(&lock), Ok(true)) {
                    continue;
                }
                if let Ok(true) = remove_tree(root, OsStr::new(&stem), 0, &mut budget) {
                    let _ = root.remove_file(&name);
                }
                drop(lock);
            }
            Sibling::Pending(name) => {
                budget -= 1;
                let Ok(lock) = root.open_regular(&name) else {
                    continue;
                };
                if matches!(crate::platform::lock::try_lock_file(&lock), Ok(true)) {
                    let _ = root.remove_file(&name);
                }
                drop(lock);
            }
        }
    }
}

/// Remove `parent/name` and everything below it without following a link,
/// charging every entry it visits to `budget`. It deletes as it enumerates,
/// so a reap cut short still leaves less behind; an entry it cannot remove is
/// left in place while the others are still removed. `Ok(true)` once the
/// directory is gone.
fn remove_tree(
    parent: &fsdir::TrustedDir,
    name: &OsStr,
    depth: usize,
    budget: &mut usize,
) -> io::Result<bool> {
    loop {
        let dir = match parent.child_for_removal(name) {
            Ok(dir) => dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error),
        };
        let mut removed_any = false;
        let mut stream = dir.stream()?;
        while let Some(entry) = stream.next_name() {
            if *budget == 0 {
                return Ok(false);
            }
            *budget -= 1;
            let entry = entry?;
            match dir.entry(&entry) {
                Ok(info) if info.kind == fsdir::EntryKind::Dir => {
                    if depth < REAP_DEPTH && remove_tree(&dir, &entry, depth + 1, budget)? {
                        removed_any = true;
                    } else if *budget == 0 {
                        return Ok(false);
                    }
                }
                Ok(_) => removed_any |= dir.remove_file(&entry).is_ok(),
                Err(_) => {}
            }
        }
        drop(stream);
        drop(dir);
        match parent.remove_dir(name) {
            Ok(()) => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            // Entries deleted while enumerating can make a stream skip some on
            // a few filesystems; go round again while that still deletes.
            Err(_) if removed_any && *budget > 0 => continue,
            Err(_) => return Ok(false),
        }
    }
}

/// One agent generation's `rec`, `caps` and `revoked` directories, each an
/// anchored, verified handle.
#[derive(Debug)]
pub struct GenerationSpool {
    rec: fsdir::TrustedDir,
    caps: fsdir::TrustedDir,
    revoked: fsdir::TrustedDir,
}

impl GenerationSpool {
    /// A component's side: open an existing generation spool under the
    /// absolute pinned `<state-dir>/inbox-spool/<instance>` path, creating
    /// nothing. The walk starts again from the state directory, the trust
    /// anchor, and verifies the root and the instance one component at a time
    /// exactly as the daemon does, so a symlink or a wider mode at either is
    /// refused rather than followed.
    pub fn open(instance_path: &Path, pane: &str, agent: &str) -> io::Result<GenerationSpool> {
        let shown = instance_path.display();
        if !instance_path.is_absolute() {
            return Err(invalid(format!(
                "{shown}: the pinned spool path must be absolute"
            )));
        }
        let instance = instance_path
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|name| is_instance(name))
            .ok_or_else(|| invalid(format!("{shown} does not name a spool instance")))?;
        let root = instance_path
            .parent()
            .filter(|root| root.file_name() == Some(OsStr::new(SPOOL_DIR_NAME)))
            .ok_or_else(|| invalid(format!("{shown} is not under an {SPOOL_DIR_NAME} root")))?;
        let state = root
            .parent()
            .ok_or_else(|| invalid(format!("{shown} has no state directory")))?;
        let base = fsdir::TrustedDir::anchor(state)?
            .child(SPOOL_DIR_NAME, false)?
            .child(instance, false)?;
        GenerationSpool::at(&base, pane, agent, false)
    }

    fn at(
        base: &fsdir::TrustedDir,
        pane: &str,
        agent: &str,
        create: bool,
    ) -> io::Result<GenerationSpool> {
        let generation = base.child(pane, create)?.child(agent, create)?;
        Ok(GenerationSpool {
            rec: generation.child("rec", create)?,
            caps: generation.child("caps", create)?,
            revoked: generation.child("revoked", create)?,
        })
    }

    /// A record temp, owner-only before any byte is written to it (on
    /// Windows its DACL is set on the handle before the first write).
    pub fn create_record_temp(&self) -> io::Result<(String, File)> {
        let name = format!("{TEMP_PREFIX}{}", random_hex(8)?);
        let file = self.rec.create_new_owner_only(&name)?;
        Ok((name, file))
    }

    /// Publish `key`'s record carrying `cap`: written to an owner-only temp,
    /// linked to its final name (refused when that exists), temp removed.
    pub fn publish(&self, key: &RecordKey, cap: &Cap) -> io::Result<()> {
        if !valid_component(&key.msg) {
            return Err(invalid("a message id must be one spool path component"));
        }
        let body = serde_json::to_vec(&RecordBody {
            msg: key.msg.clone(),
            a: key.a,
            e: key.e,
            stage: key.stage,
            cap: cap.hex(),
        })
        .map_err(io::Error::other)?;
        write_linked(&self.rec, &key.file_name(), &body)
    }

    /// Write an attempt's caps for a one-shot committer (§5), named by its
    /// exact `(msg, A, e)`.
    pub fn write_caps(&self, msg: &str, a: u32, e: u32, caps: &AttemptCaps) -> io::Result<()> {
        let text = format!("{}\n{}\n", caps.handed.hex(), caps.commit.hex());
        write_linked(&self.caps, &attempt_name(msg, a, e)?, text.as_bytes())
    }

    /// Read an attempt's caps, found by its exact `(msg, A, e)` and never by
    /// whichever caps file is newest.
    pub fn read_caps(&self, msg: &str, a: u32, e: u32) -> io::Result<(String, String)> {
        let file = self.caps.open_regular(&attempt_name(msg, a, e)?)?;
        let mut text = String::new();
        file.take(MAX_RECORD_BYTES).read_to_string(&mut text)?;
        let mut lines = text.lines();
        match (lines.next(), lines.next()) {
            (Some(handed), Some(commit)) => Ok((handed.to_string(), commit.to_string())),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a caps file holds two lines",
            )),
        }
    }

    /// Mark an attempt revoked (§3). Marking it twice is not an error.
    pub fn write_revoke(&self, msg: &str, a: u32, e: u32) -> io::Result<()> {
        match self
            .revoked
            .create_new_owner_only(&attempt_name(msg, a, e)?)
        {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Remove every spool file of one attempt — its two records, its caps
    /// and its revoke marker — once its evidence reservation is returned
    /// (§7). A file already gone is not an error; the first other failure is
    /// returned after every removal was tried.
    pub fn remove_attempt(&self, msg: &str, a: u32, e: u32) -> io::Result<()> {
        let attempt = attempt_name(msg, a, e)?;
        let mut first = None;
        let mut gone = |result: io::Result<()>| {
            if let Err(error) = result
                && error.kind() != io::ErrorKind::NotFound
            {
                first.get_or_insert(error);
            }
        };
        for stage in [RecordStage::Handed, RecordStage::Committed] {
            gone(
                self.rec
                    .remove_file(format!("{attempt}.{}", stage.as_str())),
            );
        }
        gone(self.caps.remove_file(&attempt));
        gone(self.revoked.remove_file(&attempt));
        first.map_or(Ok(()), Err)
    }

    pub fn revoked(&self, msg: &str, a: u32, e: u32) -> bool {
        attempt_name(msg, a, e)
            .and_then(|name| self.revoked.entry(&name))
            .is_ok_and(|info| info.kind == fsdir::EntryKind::File)
    }

    /// A bounded, resumable reader of this generation's records.
    pub fn scanner(&self) -> io::Result<RecordScanner> {
        Ok(RecordScanner {
            rec: self.rec.try_clone()?,
            stream: None,
            carry: None,
            ended: false,
            errored: false,
            complete: false,
            degraded: None,
            faults: Faults::default(),
        })
    }
}

/// Write `bytes` to an owner-only temp in `dir` and link it to `name`, which
/// must not exist yet; the temp is removed either way.
fn write_linked(dir: &fsdir::TrustedDir, name: &str, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(invalid("a spool record is at most 4 KiB"));
    }
    let temp = format!("{TEMP_PREFIX}{}", random_hex(8)?);
    let mut file = dir.create_new_owner_only(&temp)?;
    let written = file.write_all(bytes);
    drop(file);
    let linked = written.and_then(|()| dir.link(&temp, name));
    let _ = dir.remove_file(&temp);
    linked
}

/// One scan pass's result. Only an error-free cycle that reached the end of
/// the directory is complete, and nothing short of that is absence.
#[derive(Debug, Default)]
pub struct ScanPass {
    /// Records this pass admitted for the first time.
    pub admitted: Vec<RecordKey>,
    /// Well-formed records the evidence store refused (never issued, or the
    /// wrong cap).
    pub refused: usize,
    /// This pass reached the end of the directory, ending the cycle.
    pub ended: bool,
    /// It ended a cycle that met no error.
    pub complete: bool,
    /// Entries this pass examined, failed ones included.
    pub examined: usize,
    /// Record bytes this pass consumed, failed reads included.
    pub bytes: u64,
    /// Invalid record files this pass removed.
    pub removed: usize,
    /// The first error this pass met.
    pub error: Option<String>,
}

/// Whether an attempt's receipt is known to be in the spool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Found,
    /// The latest cycle completed without error and an exact lookup of the
    /// attempt's record names found neither.
    Absent,
    /// Neither can be said yet.
    Unknown,
}

/// What reading one record-named file found.
enum Fetched {
    Body(RecordBody),
    /// Oversized, unparseable or naming another record: never a receipt.
    Invalid,
    /// Not a regular file: never opened.
    NotRegular,
    Missing,
}

/// Read the record-named file `name` of `dir`, never following a link and
/// never opening a non-regular file, through at most [`READ_RESERVE`] bytes.
/// Bytes it consumed are added to `consumed` even when the read fails.
fn fetch(
    dir: &fsdir::TrustedDir,
    name: &str,
    key: &RecordKey,
    consumed: &mut u64,
    faults: &mut Faults,
) -> io::Result<Fetched> {
    let info = match dir.entry(name) {
        Ok(info) => info,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Fetched::Missing),
        Err(error) => return Err(error),
    };
    if info.kind != fsdir::EntryKind::File {
        return Ok(Fetched::NotRegular);
    }
    if info.len > MAX_RECORD_BYTES {
        return Ok(Fetched::Invalid);
    }
    faults.before_open(name);
    let file = dir.open_regular(name)?;
    let mut buf = Vec::new();
    let read = faults.read_to_end(file.take(READ_RESERVE), &mut buf);
    *consumed += buf.len() as u64;
    read?;
    if buf.len() as u64 > MAX_RECORD_BYTES {
        return Ok(Fetched::Invalid);
    }
    match serde_json::from_slice::<RecordBody>(&buf) {
        Ok(body) if body.key() == *key => Ok(Fetched::Body(body)),
        _ => Ok(Fetched::Invalid),
    }
}

/// Reads a generation's records in passes bounded by [`SCAN_MAX_ENTRIES`]
/// and [`SCAN_MAX_BYTES`], resuming where the last pass stopped, and streams
/// each one to the evidence store: it keeps no record, body or name of its
/// own between passes beyond the one record a byte-bounded pass defers.
///
/// Entries that are not regular files (a FIFO, a symlink, a directory) are
/// skipped without being opened; a regular file is opened without following a
/// link and without blocking. A record-named file larger than
/// [`MAX_RECORD_BYTES`], or whose body does not match its name, is removed;
/// temps and any other name are left alone, so a publisher paused between
/// write and link still links. A valid record is never removed.
///
/// An enumeration error, a failed rewind, or a failed stat, open or read of a
/// record-named file is charged to the pass, keeps the cycle from completing
/// and marks the scanner degraded, sticky for its life.
#[derive(Debug)]
pub struct RecordScanner {
    rec: fsdir::TrustedDir,
    stream: Option<fsdir::DirStream>,
    /// A record name a byte-bounded pass left for the next one.
    carry: Option<String>,
    /// The current cycle reached the end of the directory.
    ended: bool,
    /// The current cycle met an error.
    errored: bool,
    /// The latest cycle ended without error and no new one has begun.
    complete: bool,
    degraded: Option<String>,
    faults: Faults,
}

impl RecordScanner {
    pub fn pass(&mut self, store: &mut EvidenceStore) -> ScanPass {
        let mut pass = ScanPass::default();
        if self.ended {
            self.ended = false;
            self.errored = false;
            self.complete = false;
            if let Some(stream) = self.stream.as_mut() {
                let rewound = match self.faults.rewind() {
                    Some(error) => Err(error),
                    None => stream.rewind(),
                };
                if let Err(error) = rewound {
                    self.stream = None;
                    self.fail(&mut pass, error);
                    return pass;
                }
            }
        }
        if self.stream.is_none() {
            match self.rec.stream() {
                Ok(stream) => self.stream = Some(stream),
                Err(error) => {
                    self.fail(&mut pass, error);
                    return pass;
                }
            }
        }
        while pass.examined < SCAN_MAX_ENTRIES {
            let name = match self.carry.take() {
                Some(name) => name,
                None => {
                    let next = match self.faults.enumerate() {
                        Some(error) => Some(Err(error)),
                        None => self.stream.as_mut().and_then(|s| s.next_name()),
                    };
                    match next {
                        None => {
                            self.ended = true;
                            self.complete = !self.errored;
                            pass.ended = true;
                            pass.complete = self.complete;
                            break;
                        }
                        Some(Err(error)) => {
                            pass.examined += 1;
                            self.fail(&mut pass, error);
                            continue;
                        }
                        Some(Ok(name)) => match name.into_string() {
                            Ok(name) => name,
                            Err(_) => {
                                pass.examined += 1;
                                continue;
                            }
                        },
                    }
                }
            };
            let Some(key) = RecordKey::parse(&name) else {
                pass.examined += 1;
                continue;
            };
            if SCAN_MAX_BYTES.saturating_sub(pass.bytes) < READ_RESERVE {
                self.carry = Some(name);
                break;
            }
            pass.examined += 1;
            match fetch(&self.rec, &name, &key, &mut pass.bytes, &mut self.faults) {
                Ok(Fetched::Body(body)) => match store.admit(&body) {
                    Admission::Admitted => pass.admitted.push(key),
                    Admission::Duplicate => {}
                    Admission::NotIssued | Admission::WrongCap => pass.refused += 1,
                },
                Ok(Fetched::Invalid) => {
                    if self.rec.remove_file(&name).is_ok() {
                        pass.removed += 1;
                    }
                }
                Ok(Fetched::NotRegular) => {}
                Ok(Fetched::Missing) => self.fail(
                    &mut pass,
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("{name} vanished during a scan"),
                    ),
                ),
                Err(error) => self.fail(&mut pass, error),
            }
        }
        pass
    }

    fn fail(&mut self, pass: &mut ScanPass, error: io::Error) {
        self.errored = true;
        self.complete = false;
        let text = error.to_string();
        if self.degraded.is_none() {
            self.degraded = Some(text.clone());
        }
        pass.error.get_or_insert(text);
    }

    /// Whether the latest cycle ended without error and no new one has begun.
    pub fn complete(&self) -> bool {
        self.complete
    }

    /// The first spool error this scanner met, if any: the generation's
    /// `receipts_degraded` reason (§3), sticky for the scanner's life.
    pub fn degraded(&self) -> Option<&str> {
        self.degraded.as_deref()
    }

    /// The point-in-time check before a lapse may treat `(msg, A, e)` as
    /// receipt-less (§3): found when the store already admitted one of its
    /// records or an exact anchored lookup of its two immutable names finds
    /// one the store admits; absent only when the latest cycle completed
    /// without error **and** that lookup finds neither name; unknown
    /// otherwise. Directory enumeration is not a snapshot, so the lookup, not
    /// the scan, is what sees a record linked behind the cursor.
    pub fn presence(&mut self, store: &mut EvidenceStore, msg: &str, a: u32, e: u32) -> Presence {
        self.presence_of(
            store,
            msg,
            a,
            e,
            &[RecordStage::Handed, RecordStage::Committed],
        )
    }

    /// [`Self::presence`] of the attempt's `Committed` receipt alone: what a
    /// lapse needs ruled out before a repeat (§3). A `Handed` receipt says the
    /// attempt reached the agent, which a marked repeat already allows for, so
    /// it never stands in for this lookup.
    pub fn commit_presence(
        &mut self,
        store: &mut EvidenceStore,
        msg: &str,
        a: u32,
        e: u32,
    ) -> Presence {
        self.presence_of(store, msg, a, e, &[RecordStage::Committed])
    }

    fn presence_of(
        &mut self,
        store: &mut EvidenceStore,
        msg: &str,
        a: u32,
        e: u32,
        stages: &[RecordStage],
    ) -> Presence {
        if !valid_component(msg) {
            return Presence::Unknown;
        }
        let mut unknown = !self.complete;
        for &stage in stages {
            let key = RecordKey {
                msg: msg.to_string(),
                a,
                e,
                stage,
            };
            if store.admitted(&key) {
                return Presence::Found;
            }
            let mut consumed = 0;
            match fetch(
                &self.rec,
                &key.file_name(),
                &key,
                &mut consumed,
                &mut self.faults,
            ) {
                Ok(Fetched::Missing) => {}
                Ok(Fetched::Body(body))
                    if matches!(
                        store.admit(&body),
                        Admission::Admitted | Admission::Duplicate
                    ) =>
                {
                    return Presence::Found;
                }
                Ok(_) => unknown = true,
                Err(error) => {
                    unknown = true;
                    if self.degraded.is_none() {
                        self.degraded = Some(error.to_string());
                    }
                }
            }
        }
        if unknown {
            Presence::Unknown
        } else {
            Presence::Absent
        }
    }
}

/// Test-only fault injection at the scanner's IO seams; empty and inert in
/// every other build.
#[derive(Default)]
struct Faults {
    /// Errors the next enumerations return, in order.
    #[cfg(test)]
    enumerate: std::collections::VecDeque<io::Error>,
    /// The error the next rewind returns.
    #[cfg(test)]
    rewind: Option<io::Error>,
    /// The next record read fails after this many bytes.
    #[cfg(test)]
    read_error_after: Option<usize>,
    /// Called with a record's name between its stat and its open.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    before_open: Option<Box<dyn FnMut(&str) + Send>>,
}

impl std::fmt::Debug for Faults {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Faults")
    }
}

#[cfg(not(test))]
impl Faults {
    fn enumerate(&mut self) -> Option<io::Error> {
        None
    }

    fn rewind(&mut self) -> Option<io::Error> {
        None
    }

    fn before_open(&mut self, _name: &str) {}

    fn read_to_end(&mut self, mut reader: impl Read, buf: &mut Vec<u8>) -> io::Result<usize> {
        reader.read_to_end(buf)
    }
}

#[cfg(test)]
impl Faults {
    fn enumerate(&mut self) -> Option<io::Error> {
        self.enumerate.pop_front()
    }

    fn rewind(&mut self) -> Option<io::Error> {
        self.rewind.take()
    }

    fn before_open(&mut self, name: &str) {
        if let Some(hook) = self.before_open.as_mut() {
            hook(name);
        }
    }

    fn read_to_end(&mut self, reader: impl Read, buf: &mut Vec<u8>) -> io::Result<usize> {
        match self.read_error_after.take() {
            Some(left) => FailAfter {
                inner: reader,
                left,
            }
            .read_to_end(buf),
            None => {
                let mut reader = reader;
                reader.read_to_end(buf)
            }
        }
    }
}

/// A reader that fails once `left` bytes have been read through it.
#[cfg(test)]
struct FailAfter<R> {
    inner: R,
    left: usize,
}

#[cfg(test)]
impl<R: Read> Read for FailAfter<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Err(io::Error::other("injected read failure"));
        }
        let n = buf.len().min(self.left);
        let got = self.inner.read(&mut buf[..n])?;
        if got == 0 {
            return Err(io::Error::other("injected read failure"));
        }
        self.left -= got;
        Ok(got)
    }
}

/// How a record fared against the evidence store (§5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Admitted,
    Duplicate,
    NotIssued,
    WrongCap,
}

#[derive(Debug, Clone)]
struct IssuedCaps {
    handed: [u8; 32],
    commit: [u8; 32],
}

/// The daemon's per-generation suppression evidence (§7): the hashes of every
/// issued attempt's caps, which admit records, the caps themselves, kept only
/// to hand a replacement watcher the attempts rebound to it (§4, §5), and the
/// records admitted on them. It holds keys only for issued attempts, at most
/// two records each, so its size follows the attempts issued. An attempt is forgotten ([`Self::forget`]) when the
/// attempt model returns its evidence reservation, which is also when its
/// spool files are removed ([`GenerationSpool::remove_attempt`]).
#[derive(Debug, Default)]
pub struct EvidenceStore {
    issued: BTreeMap<(String, u32, u32), IssuedCaps>,
    /// The caps themselves, while the attempt is issued: a watcher that
    /// replaces a dead one is handed its retained attempts' caps again.
    caps: BTreeMap<(String, u32, u32), AttemptCaps>,
    admitted: BTreeSet<RecordKey>,
}

impl EvidenceStore {
    /// Mint an attempt's caps, keeping only their hashes.
    pub fn issue(&mut self, msg: &str, a: u32, e: u32) -> io::Result<AttemptCaps> {
        let caps = AttemptCaps {
            handed: Cap::random()?,
            commit: Cap::random()?,
        };
        self.issued.insert(
            (msg.to_string(), a, e),
            IssuedCaps {
                handed: Cap::hash_hex(&caps.handed.hex()),
                commit: Cap::hash_hex(&caps.commit.hex()),
            },
        );
        self.caps.insert((msg.to_string(), a, e), caps.clone());
        Ok(caps)
    }

    /// An issued attempt's caps, to hand to the watcher it is rebound to.
    pub fn caps(&self, msg: &str, a: u32, e: u32) -> Option<&AttemptCaps> {
        self.caps.get(&(msg.to_string(), a, e))
    }

    /// Admit a record on bearer authority (§5): an issued `(msg, A, e)` and
    /// that stage's cap — the commit cap for `Committed`, the hand-out cap for
    /// `Handed`, so a hand-out cap never proves a commit.
    pub fn admit(&mut self, body: &RecordBody) -> Admission {
        let Some(issued) = self.issued.get(&(body.msg.clone(), body.a, body.e)) else {
            return Admission::NotIssued;
        };
        let expected = if body.stage.needs_commit_cap() {
            &issued.commit
        } else {
            &issued.handed
        };
        if Cap::hash_hex(&body.cap) != *expected {
            return Admission::WrongCap;
        }
        if !self.admitted.insert(body.key()) {
            return Admission::Duplicate;
        }
        Admission::Admitted
    }

    pub fn admitted(&self, key: &RecordKey) -> bool {
        self.admitted.contains(key)
    }

    /// Forget an attempt whose evidence was reclaimed: its caps no longer
    /// admit a record, and its admitted records are dropped.
    pub fn forget(&mut self, msg: &str, a: u32, e: u32) {
        self.issued.remove(&(msg.to_string(), a, e));
        self.caps.remove(&(msg.to_string(), a, e));
        self.admitted
            .retain(|key| !(key.msg == msg && key.a == a && key.e == e));
    }

    /// Keys held: issued attempts plus admitted records (an issued attempt's
    /// caps are kept under its key).
    pub fn entries(&self) -> usize {
        self.issued.len() + self.admitted.len()
    }
}

/// Directory handles the spool trusts, one verified component at a time.
mod fsdir {
    use std::ffi::OsStr;
    use std::io;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum EntryKind {
        File,
        Dir,
        /// A symlink, FIFO, socket, device or reparse point: never opened.
        Other,
    }

    #[derive(Debug, Clone, Copy)]
    pub(super) struct EntryInfo {
        pub(super) kind: EntryKind,
        pub(super) len: u64,
    }

    /// A file name inside one directory: no separator, not `.` or `..`.
    pub(super) fn plain(name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        let bad = {
            use std::os::unix::ffi::OsStrExt as _;
            let bytes = name.as_bytes();
            bytes.is_empty()
                || bytes == b"."
                || bytes == b".."
                || bytes.iter().any(|b| matches!(b, b'/' | b'\0'))
        };
        #[cfg(windows)]
        let bad = {
            use std::os::windows::ffi::OsStrExt as _;
            let wide: Vec<u16> = name.encode_wide().collect();
            wide.is_empty()
                || name == OsStr::new(".")
                || name == OsStr::new("..")
                || wide
                    .iter()
                    .any(|&c| c == u16::from(b'/') || c == u16::from(b'\\') || c == 0)
        };
        if bad {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name:?} is not a plain file name"),
            ));
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(super) use unix::{DirStream, TrustedDir};
    #[cfg(windows)]
    pub(super) use windows::{DirStream, TrustedDir};

    #[cfg(unix)]
    mod unix {
        use std::ffi::{CStr, CString, OsStr, OsString};
        use std::fs::File;
        use std::io;
        use std::mem::MaybeUninit;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        use std::path::Path;

        use super::super::{refused, valid_component};
        use super::{EntryInfo, EntryKind, plain};

        fn cstr(name: &OsStr) -> io::Result<CString> {
            CString::new(name.as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in a name"))
        }

        fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
            if rc < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(rc)
            }
        }

        /// Clear `errno`, so a null `readdir` can tell the end of the
        /// directory from an error.
        fn clear_errno() {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            // SAFETY: returns this thread's own `errno` slot, always writable.
            unsafe {
                *libc::__errno_location() = 0;
            }
            #[cfg(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "freebsd",
                target_os = "dragonfly"
            ))]
            // SAFETY: returns this thread's own `errno` slot, always writable.
            unsafe {
                *libc::__error() = 0;
            }
            #[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
            // SAFETY: returns this thread's own `errno` slot, always writable.
            unsafe {
                *libc::__errno() = 0;
            }
        }

        fn fstat(fd: RawFd) -> io::Result<libc::stat> {
            let mut st = MaybeUninit::<libc::stat>::uninit();
            // SAFETY: `fd` is a live descriptor and `st` is writable storage
            // for one `stat`, initialised by a successful call.
            cvt(unsafe { libc::fstat(fd, st.as_mut_ptr()) })?;
            // SAFETY: fstat succeeded and filled it.
            Ok(unsafe { st.assume_init() })
        }

        /// A directory owned by this user; `strict` also refuses any group or
        /// other permission bit.
        fn verify(fd: RawFd, what: &str, strict: bool) -> io::Result<()> {
            let st = fstat(fd)?;
            if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
                return Err(refused(format!("{what} is not a directory")));
            }
            // SAFETY: geteuid cannot fail and touches no memory.
            if st.st_uid != unsafe { libc::geteuid() } {
                return Err(refused(format!("{what} is owned by another user")));
            }
            if strict && st.st_mode & 0o077 != 0 {
                return Err(refused(format!(
                    "{what} is open to other users (mode {:o})",
                    st.st_mode & 0o777
                )));
            }
            Ok(())
        }

        /// A verified directory, held by descriptor: every operation is
        /// relative to it, so a path swapped above or beside it later cannot
        /// redirect one.
        #[derive(Debug)]
        pub(in super::super) struct TrustedDir {
            fd: OwnedFd,
        }

        impl TrustedDir {
            /// The trust anchor, opened by path: a directory owned by this
            /// user. Its ancestors are trusted as the OS resolves them.
            pub(in super::super) fn anchor(path: &Path) -> io::Result<Self> {
                let c = cstr(path.as_os_str())?;
                // SAFETY: a valid NUL-terminated path; the descriptor is owned
                // below.
                let fd = cvt(unsafe {
                    libc::open(
                        c.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                    )
                })?;
                // SAFETY: `fd` was just opened and is owned by nobody else.
                let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                verify(fd.as_raw_fd(), &path.display().to_string(), false)?;
                Ok(TrustedDir { fd })
            }

            /// Open the directory `name` below without following a link.
            fn open_dir(&self, name: &OsStr) -> io::Result<OwnedFd> {
                let c = cstr(name)?;
                // SAFETY: a live directory descriptor and a valid name; the
                // descriptor is owned below.
                let fd = unsafe {
                    libc::openat(
                        self.fd.as_raw_fd(),
                        c.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    let error = io::Error::last_os_error();
                    if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
                        return Err(refused(format!("{name:?} is not a plain directory")));
                    }
                    return Err(error);
                }
                // SAFETY: just opened, owned by nobody else.
                Ok(unsafe { OwnedFd::from_raw_fd(fd) })
            }

            /// One component below: created owner-only when `create`, opened
            /// without following a link, and verified owner-only.
            pub(in super::super) fn child(&self, name: &str, create: bool) -> io::Result<Self> {
                if !valid_component(name) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{name:?} is not a spool path component"),
                    ));
                }
                if create {
                    let c = cstr(OsStr::new(name))?;
                    // SAFETY: a live directory descriptor and a valid name.
                    let rc = unsafe { libc::mkdirat(self.fd.as_raw_fd(), c.as_ptr(), 0o700) };
                    if rc < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() != io::ErrorKind::AlreadyExists {
                            return Err(error);
                        }
                    }
                }
                let fd = self.open_dir(OsStr::new(name))?;
                verify(fd.as_raw_fd(), name, true)?;
                Ok(TrustedDir { fd })
            }

            /// A directory below that is about to be removed: opened without
            /// following a link and owned by this user, any mode.
            pub(in super::super) fn child_for_removal(&self, name: &OsStr) -> io::Result<Self> {
                plain(name)?;
                let fd = self.open_dir(name)?;
                verify(fd.as_raw_fd(), &format!("{name:?}"), false)?;
                Ok(TrustedDir { fd })
            }

            pub(in super::super) fn try_clone(&self) -> io::Result<Self> {
                Ok(TrustedDir {
                    fd: self.fd.try_clone()?,
                })
            }

            /// A new owner-only file, refused when the name exists or is a link.
            pub(in super::super) fn create_new_owner_only(&self, name: &str) -> io::Result<File> {
                plain(OsStr::new(name))?;
                let c = cstr(OsStr::new(name))?;
                let mode: libc::c_uint = 0o600;
                // SAFETY: a live directory descriptor, a valid name and a mode;
                // the descriptor is owned below.
                let fd = cvt(unsafe {
                    libc::openat(
                        self.fd.as_raw_fd(),
                        c.as_ptr(),
                        libc::O_WRONLY
                            | libc::O_CREAT
                            | libc::O_EXCL
                            | libc::O_NOFOLLOW
                            | libc::O_CLOEXEC,
                        mode,
                    )
                })?;
                // SAFETY: just opened, owned by nobody else.
                let file = unsafe { File::from_raw_fd(fd) };
                crate::platform::fsperm::set_file_owner_only(&file)?;
                Ok(file)
            }

            pub(in super::super) fn link(&self, from: &str, to: &str) -> io::Result<()> {
                let (from, to) = (OsStr::new(from), OsStr::new(to));
                plain(from)?;
                plain(to)?;
                let (f, t) = (cstr(from)?, cstr(to)?);
                let fd = self.fd.as_raw_fd();
                // SAFETY: live descriptors and valid names; no link is followed.
                cvt(unsafe { libc::linkat(fd, f.as_ptr(), fd, t.as_ptr(), 0) })?;
                Ok(())
            }

            pub(in super::super) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
                let (from, to) = (OsStr::new(from), OsStr::new(to));
                plain(from)?;
                plain(to)?;
                let (f, t) = (cstr(from)?, cstr(to)?);
                let fd = self.fd.as_raw_fd();
                // SAFETY: live descriptors and valid names.
                cvt(unsafe { libc::renameat(fd, f.as_ptr(), fd, t.as_ptr()) })?;
                Ok(())
            }

            pub(in super::super) fn remove_file(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
                let name = name.as_ref();
                plain(name)?;
                let c = cstr(name)?;
                // SAFETY: a live descriptor and a valid name; a link is
                // removed, never followed.
                cvt(unsafe { libc::unlinkat(self.fd.as_raw_fd(), c.as_ptr(), 0) })?;
                Ok(())
            }

            pub(in super::super) fn remove_dir(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
                let name = name.as_ref();
                plain(name)?;
                let c = cstr(name)?;
                // SAFETY: as above.
                cvt(unsafe {
                    libc::unlinkat(self.fd.as_raw_fd(), c.as_ptr(), libc::AT_REMOVEDIR)
                })?;
                Ok(())
            }

            /// What `name` is, without following a link or opening it.
            pub(in super::super) fn entry(&self, name: impl AsRef<OsStr>) -> io::Result<EntryInfo> {
                let name = name.as_ref();
                plain(name)?;
                let c = cstr(name)?;
                let mut st = MaybeUninit::<libc::stat>::uninit();
                // SAFETY: a live descriptor, a valid name and storage for one
                // `stat`, initialised by a successful call.
                cvt(unsafe {
                    libc::fstatat(
                        self.fd.as_raw_fd(),
                        c.as_ptr(),
                        st.as_mut_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                })?;
                // SAFETY: fstatat succeeded and filled it.
                let st = unsafe { st.assume_init() };
                let kind = match st.st_mode & libc::S_IFMT {
                    libc::S_IFREG => EntryKind::File,
                    libc::S_IFDIR => EntryKind::Dir,
                    _ => EntryKind::Other,
                };
                Ok(EntryInfo {
                    kind,
                    len: u64::try_from(st.st_size).unwrap_or(0),
                })
            }

            /// Open a regular file for reading: no link followed, no open that
            /// blocks on a FIFO, and refused when it is not a regular file.
            pub(in super::super) fn open_regular(
                &self,
                name: impl AsRef<OsStr>,
            ) -> io::Result<File> {
                let name = name.as_ref();
                plain(name)?;
                let c = cstr(name)?;
                // SAFETY: a live descriptor and a valid name; owned below.
                let fd = cvt(unsafe {
                    libc::openat(
                        self.fd.as_raw_fd(),
                        c.as_ptr(),
                        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                    )
                })?;
                // SAFETY: just opened, owned by nobody else.
                let file = unsafe { File::from_raw_fd(fd) };
                if !file.metadata()?.is_file() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "it is not a regular file",
                    ));
                }
                Ok(file)
            }

            /// A directory stream over a fresh open of this directory, so two
            /// streams never share a read position.
            pub(in super::super) fn stream(&self) -> io::Result<DirStream> {
                let dot = cstr(OsStr::new("."))?;
                // SAFETY: a live descriptor and a valid name.
                let fd = cvt(unsafe {
                    libc::openat(
                        self.fd.as_raw_fd(),
                        dot.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                    )
                })?;
                // SAFETY: `fd` is ours; fdopendir takes it over on success.
                let dir = unsafe { libc::fdopendir(fd) };
                if dir.is_null() {
                    let error = io::Error::last_os_error();
                    // SAFETY: fdopendir failed, so `fd` is still ours to close.
                    unsafe { libc::close(fd) };
                    return Err(error);
                }
                Ok(DirStream { dir })
            }
        }

        #[derive(Debug)]
        pub(in super::super) struct DirStream {
            dir: *mut libc::DIR,
        }

        // SAFETY: the stream is used by one owner at a time; a `DIR` may move
        // between threads.
        unsafe impl Send for DirStream {}

        impl DirStream {
            /// The next name as bytes, `None` at the end of the directory, or
            /// the error `readdir` reported (`EILSEQ` included): an error is
            /// never mistaken for the end.
            pub(in super::super) fn next_name(&mut self) -> Option<io::Result<OsString>> {
                loop {
                    clear_errno();
                    // SAFETY: `dir` is a live stream owned by self.
                    let entry = unsafe { libc::readdir(self.dir) };
                    if entry.is_null() {
                        let error = io::Error::last_os_error();
                        return match error.raw_os_error() {
                            None | Some(0) => None,
                            Some(_) => Some(Err(error)),
                        };
                    }
                    // SAFETY: readdir returned a valid entry whose name is
                    // NUL-terminated and lives until the next readdir.
                    let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                    if name == b"." || name == b".." {
                        continue;
                    }
                    return Some(Ok(OsString::from_vec(name.to_vec())));
                }
            }

            pub(in super::super) fn rewind(&mut self) -> io::Result<()> {
                // SAFETY: `dir` is a live stream owned by self.
                unsafe { libc::rewinddir(self.dir) };
                Ok(())
            }
        }

        impl Drop for DirStream {
            fn drop(&mut self) {
                // SAFETY: `dir` is a live stream owned by self, closed once.
                unsafe { libc::closedir(self.dir) };
            }
        }
    }

    #[cfg(windows)]
    mod windows {
        use std::ffi::{OsStr, OsString};
        use std::fs::{File, OpenOptions};
        use std::io;
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use std::os::windows::io::AsRawHandle;
        use std::path::{Path, PathBuf};
        use std::sync::Arc;

        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        use super::super::{refused, valid_component};
        use super::{EntryInfo, EntryKind, plain};

        /// An open directory handle and the handles of every component above
        /// it, back to the anchor. Each is opened **without**
        /// `FILE_SHARE_DELETE`, so while it is held no component of the chain
        /// can be renamed or deleted. Write sharing stays allowed. The reason
        /// it was kept is unverified: the belief that linking or renaming
        /// into a directory opens it for writing, and that dropping write
        /// sharing would not stop `FSCTL_SET_REPARSE_POINT` anyway, since a
        /// handle asking only for attribute access is not checked against
        /// share mode. inbox/receipt/017 makes the mount point through a
        /// generic writable handle, so it cannot tell the two apart. A
        /// same-user writer can make a held, empty directory a mount point in
        /// place; [`TrustedDir::guarded`] detects one that is there when it
        /// looks.
        #[derive(Debug)]
        struct Held {
            dir: File,
            parent: Option<Arc<Held>>,
        }

        fn open_dir(path: &Path, follow: bool) -> io::Result<File> {
            let flags = if follow {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
            };
            OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(flags)
                .open(path)
        }

        fn handle(file: &File) -> HANDLE {
            file.as_raw_handle() as HANDLE
        }

        /// The opened handle itself is a directory and not a reparse point,
        /// and its owner is this user.
        fn verify_dir(file: &File, what: &str) -> io::Result<()> {
            let attributes = file.metadata()?.file_attributes();
            if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(refused(format!("{what} is a reparse point")));
            }
            if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
                return Err(refused(format!("{what} is not a directory")));
            }
            crate::platform::fsperm::verify_file_owner_is_current_user(handle(file))
                .map_err(|why| refused(format!("{what}: {why}")))
        }

        /// A verified directory, held as a handle chain back to the anchor.
        /// Windows has no `openat`, so operations go by path. Every component
        /// of that path is held open without delete sharing, so it cannot be
        /// renamed or deleted, and every operation checks through those
        /// handles, before and after, that none is a reparse point. That
        /// detects a reparse point present when a check looks; it is not
        /// confinement. A postcheck cannot undo a create, link, rename or
        /// remove that already went through a redirected path, and a junction
        /// installed and removed between the two checks is not seen.
        #[derive(Debug)]
        pub(in super::super) struct TrustedDir {
            path: PathBuf,
            held: Arc<Held>,
        }

        impl TrustedDir {
            /// The trust anchor: a directory owned by this user, opened by
            /// path (its ancestors are trusted as the OS resolves them) and
            /// held.
            pub(in super::super) fn anchor(path: &Path) -> io::Result<Self> {
                let dir = open_dir(path, true)?;
                if dir.metadata()?.file_attributes() & FILE_ATTRIBUTE_DIRECTORY == 0 {
                    return Err(refused(format!("{} is not a directory", path.display())));
                }
                crate::platform::fsperm::verify_file_owner_is_current_user(handle(&dir))
                    .map_err(|why| refused(format!("{}: {why}", path.display())))?;
                Ok(TrustedDir {
                    path: path.to_path_buf(),
                    held: Arc::new(Held { dir, parent: None }),
                })
            }

            /// Every held handle of the chain still names a directory that is
            /// not a reparse point, read through the handle itself, so an
            /// in-place `FSCTL_SET_REPARSE_POINT` on any component is seen.
            fn intact(&self) -> io::Result<()> {
                let mut held = Some(&self.held);
                while let Some(h) = held {
                    if h.dir.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                        return Err(refused(format!(
                            "a spool directory at or above {} became a reparse point",
                            self.path.display()
                        )));
                    }
                    held = h.parent.as_ref();
                }
                Ok(())
            }

            /// Run one path operation between two [`Self::intact`] checks:
            /// refused when a component is a reparse point before it, and an
            /// error after it even when it succeeded, since it may then have
            /// been redirected. The error does not undo what the redirected
            /// operation did. A reparse point both made and removed between
            /// the two checks is not seen.
            fn guarded<T>(&self, op: impl FnOnce(&Path) -> io::Result<T>) -> io::Result<T> {
                self.intact()?;
                let result = op(&self.path);
                self.intact()?;
                result
            }

            fn below(&self, path: PathBuf, dir: File) -> Self {
                TrustedDir {
                    path,
                    held: Arc::new(Held {
                        dir,
                        parent: Some(self.held.clone()),
                    }),
                }
            }

            /// One component below: created with a protected owner-only DACL
            /// in the same call when `create`, then opened without following
            /// a reparse point and held; refused unless the handle is a
            /// directory owned by this user whose stored DACL is protected and
            /// owner-only. A pre-existing directory that fails is refused,
            /// never repaired.
            pub(in super::super) fn child(&self, name: &str, create: bool) -> io::Result<Self> {
                if !valid_component(name) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{name:?} is not a spool path component"),
                    ));
                }
                let path = self.path.join(name);
                let dir = self.guarded(|_| {
                    if create {
                        crate::platform::fsperm::create_dir_owner_only(&path)?;
                    }
                    open_dir(&path, false)
                })?;
                verify_dir(&dir, name)?;
                crate::platform::fsperm::verify_dacl_owner_only(handle(&dir))
                    .map_err(|why| refused(format!("{name}: {why}")))?;
                Ok(self.below(path, dir))
            }

            /// A directory below that is about to be removed: not a reparse
            /// point and owned by this user, any DACL.
            pub(in super::super) fn child_for_removal(&self, name: &OsStr) -> io::Result<Self> {
                plain(name)?;
                let path = self.path.join(name);
                let dir = self.guarded(|_| open_dir(&path, false))?;
                verify_dir(&dir, &format!("{name:?}"))?;
                Ok(self.below(path, dir))
            }

            pub(in super::super) fn try_clone(&self) -> io::Result<Self> {
                Ok(TrustedDir {
                    path: self.path.clone(),
                    held: self.held.clone(),
                })
            }

            /// A new file whose owner-only DACL is set on its handle before
            /// anything is written.
            pub(in super::super) fn create_new_owner_only(&self, name: &str) -> io::Result<File> {
                plain(OsStr::new(name))?;
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                crate::platform::fsperm::set_create_mode_owner_only(&mut options);
                options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
                let file = self.guarded(|dir| options.open(dir.join(name)))?;
                crate::platform::fsperm::set_file_owner_only(&file)?;
                Ok(file)
            }

            pub(in super::super) fn link(&self, from: &str, to: &str) -> io::Result<()> {
                plain(OsStr::new(from))?;
                plain(OsStr::new(to))?;
                self.guarded(|dir| std::fs::hard_link(dir.join(from), dir.join(to)))
            }

            pub(in super::super) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
                plain(OsStr::new(from))?;
                plain(OsStr::new(to))?;
                self.guarded(|dir| std::fs::rename(dir.join(from), dir.join(to)))
            }

            pub(in super::super) fn remove_file(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
                plain(name.as_ref())?;
                self.guarded(|dir| std::fs::remove_file(dir.join(name.as_ref())))
            }

            pub(in super::super) fn remove_dir(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
                plain(name.as_ref())?;
                self.guarded(|dir| std::fs::remove_dir(dir.join(name.as_ref())))
            }

            pub(in super::super) fn entry(&self, name: impl AsRef<OsStr>) -> io::Result<EntryInfo> {
                plain(name.as_ref())?;
                let meta =
                    self.guarded(|dir| std::fs::symlink_metadata(dir.join(name.as_ref())))?;
                let reparse = meta.file_type().is_symlink()
                    || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
                let kind = if reparse {
                    EntryKind::Other
                } else if meta.is_file() {
                    EntryKind::File
                } else if meta.is_dir() {
                    EntryKind::Dir
                } else {
                    EntryKind::Other
                };
                Ok(EntryInfo {
                    kind,
                    len: meta.len(),
                })
            }

            pub(in super::super) fn open_regular(
                &self,
                name: impl AsRef<OsStr>,
            ) -> io::Result<File> {
                let name = name.as_ref();
                if self.entry(name)?.kind != EntryKind::File {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "it is not a regular file",
                    ));
                }
                let file = self.guarded(|dir| {
                    OpenOptions::new()
                        .read(true)
                        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                        .open(dir.join(name))
                })?;
                let meta = file.metadata()?;
                if !meta.is_file() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "it is not a regular file",
                    ));
                }
                Ok(file)
            }

            pub(in super::super) fn stream(&self) -> io::Result<DirStream> {
                Ok(DirStream {
                    dir: self.try_clone()?,
                    iter: Some(self.guarded(|dir| std::fs::read_dir(dir))?),
                })
            }
        }

        #[derive(Debug)]
        pub(in super::super) struct DirStream {
            dir: TrustedDir,
            iter: Option<std::fs::ReadDir>,
        }

        impl DirStream {
            /// The next name, `None` at the end of the directory, or a
            /// per-entry error: an error is never mistaken for the end, and a
            /// stream a failed rewind lost reports an error, not the end.
            pub(in super::super) fn next_name(&mut self) -> Option<io::Result<OsString>> {
                let Some(iter) = self.iter.as_mut() else {
                    return Some(Err(io::Error::other(
                        "the directory stream was lost by a failed rewind",
                    )));
                };
                Some(iter.next()?.map(|entry| entry.file_name()))
            }

            pub(in super::super) fn rewind(&mut self) -> io::Result<()> {
                self.iter = None;
                self.iter = Some(self.dir.guarded(|dir| std::fs::read_dir(dir))?);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;
    use std::time::Duration;

    fn key(msg: &str, e: u32, stage: RecordStage) -> RecordKey {
        RecordKey {
            msg: msg.to_string(),
            a: 1,
            e,
            stage,
        }
    }

    fn rec_dir(spool: &Spool, pane: &str, agent: &str) -> PathBuf {
        spool.instance_path().join(pane).join(agent).join("rec")
    }

    /// Run passes until one ends a cycle, collecting every admitted key; at
    /// most `limit` passes.
    fn scan_cycle(
        scanner: &mut RecordScanner,
        store: &mut EvidenceStore,
        limit: usize,
    ) -> (Vec<RecordKey>, Vec<ScanPass>) {
        let mut admitted = Vec::new();
        let mut passes = Vec::new();
        for _ in 0..limit {
            let pass = scanner.pass(store);
            admitted.extend(pass.admitted.iter().cloned());
            let ended = pass.ended;
            passes.push(pass);
            if ended {
                break;
            }
        }
        (admitted, passes)
    }

    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Write a record file by hand (as a forger or a stale component would).
    fn write_record(rec: &Path, name: &str, msg: &str, e: u32, stage: &str, cap: &str) {
        let body = format!(r#"{{"msg":"{msg}","a":1,"e":{e},"stage":"{stage}","cap":"{cap}"}}"#);
        std::fs::write(rec.join(name), body).unwrap();
    }

    /// Scenario: Open one lock file through two handles, take the lock on the
    /// first, try the second, then close the first and try again. The second
    /// handle is refused at once while the first holds the lock, without
    /// waiting, and gets it once the first is closed.
    #[spec("inbox/receipt/001")]
    #[test]
    fn receipt_001_try_lock_file_refuses_a_held_lock_without_waiting() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("instance.lock");
        let first = File::create(&path).unwrap();
        assert!(crate::platform::lock::try_lock_file(&first).unwrap());
        let second = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(!crate::platform::lock::try_lock_file(&second).unwrap());
        drop(first);
        assert!(crate::platform::lock::try_lock_file(&second).unwrap());
    }

    /// Scenario: Start eight spools on one state directory at the same time,
    /// publish through each, add an instance directory with no lock file, stop
    /// three spools and start a ninth. Every live spool survives every
    /// concurrent start, the ninth reaps exactly the three stopped ones (their
    /// lock files last), and the unlocked directory is left alone.
    #[spec("inbox/receipt/002")]
    #[test]
    fn receipt_002_concurrent_starts_never_reap_a_live_spool() {
        let state = crate::test_temp::tempdir().unwrap();
        let mut spools: Vec<Spool> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| s.spawn(|| Spool::start(state.path())))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap().expect("start"))
                .collect()
        });
        let root = state.path().join(SPOOL_DIR_NAME);
        for spool in &spools {
            assert!(
                spool.instance_path().is_dir(),
                "a live spool is never reaped"
            );
            assert!(
                root.join(format!("{}{LOCK_SUFFIX}", spool.instance()))
                    .is_file()
            );
            spool
                .generation("2", "agent-a")
                .unwrap()
                .publish(&key("m-1", 1, RecordStage::Handed), &Cap::random().unwrap())
                .unwrap();
        }
        let unlocked = "0123456789abcdef0123456789abcdef";
        std::fs::create_dir(root.join(unlocked)).unwrap();
        let stopped: Vec<String> = spools
            .drain(..3)
            .map(|s| s.instance().to_string())
            .collect();
        let ninth = Spool::start(state.path()).unwrap();
        for instance in &stopped {
            assert!(!root.join(instance).exists(), "{instance} was not reaped");
            assert!(!root.join(format!("{instance}{LOCK_SUFFIX}")).exists());
        }
        for spool in &spools {
            assert!(
                spool.instance_path().is_dir(),
                "a live spool is never reaped"
            );
        }
        assert!(
            root.join(unlocked).is_dir(),
            "a directory with no lock file is skipped"
        );
        assert!(ninth.instance_path().is_dir());
    }

    /// Scenario: Next to one genuine record, put a FIFO, a symlink to a valid
    /// record body (carrying a genuinely issued cap) outside the spool and a
    /// directory, each named like a record, then scan. The scan finishes
    /// without blocking, admits only the genuine record, never follows the
    /// symlink, and leaves all three entries where they are.
    #[cfg(unix)]
    #[spec("inbox/receipt/003")]
    #[test]
    fn receipt_003_fifo_symlink_and_directory_entries_are_skipped_unopened() {
        use std::os::unix::ffi::OsStrExt as _;
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let generation = spool.generation("2", "agent-a").unwrap();
        let rec = rec_dir(&spool, "2", "agent-a");
        let mut store = EvidenceStore::default();
        let good = store.issue("m-good", 1, 1).unwrap();
        let linked = store.issue("m-link", 1, 1).unwrap();
        let good_key = key("m-good", 1, RecordStage::Handed);
        generation.publish(&good_key, &good.handed).unwrap();
        let fifo = rec.join(key("m-fifo", 1, RecordStage::Handed).file_name());
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path and a mode.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let outside = state.path().join("outside-record");
        write_record(
            state.path(),
            "outside-record",
            "m-link",
            1,
            "handed",
            &linked.handed.hex(),
        );
        let link = rec.join(key("m-link", 1, RecordStage::Handed).file_name());
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let dir = rec.join(key("m-dir", 1, RecordStage::Handed).file_name());
        std::fs::create_dir(&dir).unwrap();

        let mut scanner = generation.scanner().unwrap();
        let (admitted, passes) = scan_cycle(&mut scanner, &mut store, 4);
        assert!(passes.last().unwrap().complete);
        assert_eq!(admitted, vec![good_key]);
        assert!(std::fs::symlink_metadata(&fifo).is_ok());
        assert!(std::fs::symlink_metadata(&link).is_ok());
        assert!(dir.is_dir());
        assert!(outside.is_file());
    }

    /// Scenario: Create a record temp and inspect it before writing anything.
    /// The empty temp is already owner-only: mode 0600 on Unix; on Windows
    /// its stored DACL, read back from the kernel, is protected and grants
    /// access to the current user alone, so no record byte is ever written
    /// under a wider permission.
    #[spec("inbox/receipt/004")]
    #[test]
    fn receipt_004_a_record_temp_is_owner_only_before_any_byte() {
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let generation = spool.generation("2", "agent-a").unwrap();
        let (name, file) = generation.create_record_temp().unwrap();
        assert!(name.starts_with(TEMP_PREFIX));
        assert_eq!(file.metadata().unwrap().len(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            let reopened = File::open(rec_dir(&spool, "2", "agent-a").join(&name)).unwrap();
            let handle = reopened.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
            let stored = crate::platform::fsperm::read_dacl(handle).unwrap();
            let ours = crate::platform::paths::current_user_sid().unwrap();
            crate::platform::fsperm::dacl_is_owner_only(&stored, &ours)
                .unwrap_or_else(|why| panic!("{why}: {stored:?}"));
            assert_eq!(stored.aces.len(), 1, "{stored:?}");
        }
    }

    /// Scenario: Swap a generation's `rec` directory, then a pane directory,
    /// then the spool root itself for a symlink to a decoy directory; open the
    /// component side through an instance symlink and through a symlinked
    /// root above the instance; and rename a live generation's `rec` aside
    /// with a symlink to a decoy in its place. Every open is refused, a
    /// relative pinned path is refused, every decoy stays empty, and a handle
    /// anchored before the rename still publishes successfully into the
    /// renamed directory.
    #[cfg(unix)]
    #[spec("inbox/receipt/005")]
    #[test]
    fn receipt_005_an_intermediate_symlink_is_refused_not_followed() {
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        spool.generation("2", "agent-a").unwrap();
        let agent = spool.instance_path().join("2").join("agent-a");
        let decoy = state.path().join("decoy");
        std::fs::create_dir(&decoy).unwrap();
        chmod(&decoy, 0o700);
        std::fs::remove_dir(agent.join("rec")).unwrap();
        std::os::unix::fs::symlink(&decoy, agent.join("rec")).unwrap();
        let err = GenerationSpool::open(&spool.instance_path(), "2", "agent-a").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        let err = spool.generation("2", "agent-a").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert_eq!(std::fs::read_dir(&decoy).unwrap().count(), 0);

        let decoy_pane = state.path().join("decoy-pane");
        std::fs::create_dir(&decoy_pane).unwrap();
        chmod(&decoy_pane, 0o700);
        std::os::unix::fs::symlink(&decoy_pane, spool.instance_path().join("3")).unwrap();
        let err = spool.generation("3", "agent-b").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert_eq!(std::fs::read_dir(&decoy_pane).unwrap().count(), 0);

        let state2 = crate::test_temp::tempdir().unwrap();
        let real = state2.path().join("real");
        std::fs::create_dir(&real).unwrap();
        chmod(&real, 0o700);
        std::os::unix::fs::symlink(&real, state2.path().join(SPOOL_DIR_NAME)).unwrap();
        let err = Spool::start(state2.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);

        // The component side, through an instance symlink.
        let live = Spool::start(state.path()).unwrap();
        live.generation("4", "agent-c").unwrap();
        let instance = live.instance_path();
        let moved = state.path().join("moved-instance");
        std::fs::rename(&instance, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &instance).unwrap();
        let err = GenerationSpool::open(&instance, "4", "agent-c").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");

        // …and through a symlinked root above an owner-only instance tree.
        let state3 = crate::test_temp::tempdir().unwrap();
        let elsewhere = state3.path().join("elsewhere");
        let inst = "0123456789abcdef0123456789abcdef";
        for dir in ["", inst, "5", "agent-d", "rec", "caps", "revoked"] {
            let path = match dir {
                "" => elsewhere.clone(),
                d if d == inst => elsewhere.join(inst),
                "5" => elsewhere.join(inst).join("5"),
                "agent-d" => elsewhere.join(inst).join("5").join("agent-d"),
                leaf => elsewhere.join(inst).join("5").join("agent-d").join(leaf),
            };
            std::fs::create_dir(&path).unwrap();
            chmod(&path, 0o700);
        }
        std::os::unix::fs::symlink(&elsewhere, state3.path().join(SPOOL_DIR_NAME)).unwrap();
        let pinned = state3.path().join(SPOOL_DIR_NAME).join(inst);
        let err = GenerationSpool::open(&pinned, "5", "agent-d").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        let relative = Path::new(SPOOL_DIR_NAME).join(inst);
        let err = GenerationSpool::open(&relative, "5", "agent-d").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");

        // A handle anchored before a swap publishes into the inode it holds.
        let generation = live.generation("6", "agent-e").unwrap();
        let agent = moved.join("6").join("agent-e");
        std::fs::rename(agent.join("rec"), agent.join("rec-moved")).unwrap();
        let decoy_rec = state.path().join("decoy-rec");
        std::fs::create_dir(&decoy_rec).unwrap();
        chmod(&decoy_rec, 0o700);
        std::os::unix::fs::symlink(&decoy_rec, agent.join("rec")).unwrap();
        let anchored = key("m-anchored", 1, RecordStage::Handed);
        generation
            .publish(&anchored, &Cap::random().unwrap())
            .expect("the anchored handle still publishes");
        assert!(agent.join("rec-moved").join(anchored.file_name()).is_file());
        assert_eq!(std::fs::read_dir(&decoy_rec).unwrap().count(), 0);
    }

    /// Scenario: Pre-create the spool root world-writable, then
    /// group-readable, then owner-only, starting a spool each time; then make
    /// one pane directory group-readable; then make a live instance
    /// group-readable and open it from the component side. The unsafe root,
    /// pane and instance are refused and left as they were, never repaired;
    /// the owner-only root is used.
    #[cfg(unix)]
    #[spec("inbox/receipt/006")]
    #[test]
    fn receipt_006_an_unsafe_pre_existing_root_is_refused_not_repaired() {
        use std::os::unix::fs::PermissionsExt as _;
        let mode_of = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let state = crate::test_temp::tempdir().unwrap();
        let root = state.path().join(SPOOL_DIR_NAME);
        std::fs::create_dir(&root).unwrap();
        for mode in [0o777, 0o750] {
            chmod(&root, mode);
            let err = Spool::start(state.path()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
            assert_eq!(mode_of(&root), mode, "the root is refused, not repaired");
        }
        chmod(&root, 0o700);
        let spool = Spool::start(state.path()).unwrap();
        spool.generation("2", "agent-a").unwrap();
        chmod(&spool.instance_path().join("2"), 0o755);
        let err = spool.generation("2", "agent-a").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");

        chmod(&spool.instance_path().join("2"), 0o700);
        chmod(&spool.instance_path(), 0o750);
        let err = GenerationSpool::open(&spool.instance_path(), "2", "agent-a").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert_eq!(mode_of(&spool.instance_path()), 0o750, "not repaired");
        chmod(&spool.instance_path(), 0o700);
        GenerationSpool::open(&spool.instance_path(), "2", "agent-a").expect("owner-only opens");
    }

    /// Scenario: Fill a generation's record directory with 3,000 entries the
    /// spool does not recognise plus one genuine record, then scan pass by
    /// pass while asking about the record and about one that was never
    /// published. No pass examines more than the entry bound, the missing
    /// record reads unknown, never absent, until a pass completes the cycle,
    /// and then it reads absent while the genuine one reads found.
    #[spec("inbox/receipt/007")]
    #[test]
    fn receipt_007_a_bounded_scan_resumes_and_is_never_read_as_absence() {
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let generation = spool.generation("2", "agent-a").unwrap();
        let rec = rec_dir(&spool, "2", "agent-a");
        for i in 0..3000 {
            std::fs::write(rec.join(format!("junk-{i}")), b"").unwrap();
        }
        let mut store = EvidenceStore::default();
        let caps = store.issue("m-real", 1, 1).unwrap();
        store.issue("m-missing", 1, 1).unwrap();
        let real = key("m-real", 1, RecordStage::Committed);
        generation.publish(&real, &caps.commit).unwrap();
        let mut scanner = generation.scanner().unwrap();
        let first = scanner.pass(&mut store);
        assert!(!first.ended, "3,001 entries take more than one pass");
        assert!(first.examined <= SCAN_MAX_ENTRIES);
        assert_eq!(
            scanner.presence(&mut store, "m-missing", 1, 1),
            Presence::Unknown
        );
        let (_, rest) = scan_cycle(&mut scanner, &mut store, 8);
        assert!(rest.iter().all(|p| p.examined <= SCAN_MAX_ENTRIES));
        assert!(rest.last().unwrap().complete);
        assert_eq!(
            scanner.presence(&mut store, "m-missing", 1, 1),
            Presence::Absent
        );
        assert_eq!(
            scanner.presence(&mut store, "m-real", 1, 1),
            Presence::Found
        );
        assert!(store.admitted(&real));
    }

    /// Scenario: Put 300 large record-named files with uncertified caps, an
    /// oversized one, one whose body names another message, and a fresh
    /// temp into a spool, then scan through several complete cycles. The
    /// byte bound splits the first cycle and no pass reads more than 1 MiB,
    /// all 300 are refused admission, the oversized and mismatched files are
    /// removed, and the temp stays.
    #[spec("inbox/receipt/008")]
    #[test]
    fn receipt_008_record_bytes_and_mismatches_are_bounded() {
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let generation = spool.generation("2", "agent-a").unwrap();
        let rec = rec_dir(&spool, "2", "agent-a");
        for i in 0..300 {
            let msg = format!("m-big{i}");
            let name = key(&msg, 1, RecordStage::Handed).file_name();
            write_record(&rec, &name, &msg, 1, "handed", &"f".repeat(3900));
        }
        let oversized = rec.join(key("m-over", 1, RecordStage::Handed).file_name());
        std::fs::write(&oversized, vec![b' '; 5000]).unwrap();
        let mismatch = key("m-x", 1, RecordStage::Handed).file_name();
        write_record(&rec, &mismatch, "m-y", 1, "handed", "00");
        let temp = rec.join(format!("{TEMP_PREFIX}fresh"));
        std::fs::write(&temp, b"partial").unwrap();

        let mut store = EvidenceStore::default();
        let mut scanner = generation.scanner().unwrap();
        let (admitted, passes) = scan_cycle(&mut scanner, &mut store, 8);
        assert!(
            passes.len() >= 2,
            "1.2 MB of records takes more than one pass"
        );
        assert!(
            passes[0].examined < SCAN_MAX_ENTRIES,
            "split by bytes, not entries"
        );
        assert!(passes.iter().all(|p| p.bytes <= SCAN_MAX_BYTES));
        assert!(admitted.is_empty());
        assert_eq!(passes.iter().map(|p| p.refused).sum::<usize>(), 300);
        assert!(!oversized.exists());
        assert!(!rec.join(&mismatch).exists());
        for _ in 0..3 {
            let _ = scan_cycle(&mut scanner, &mut store, 8);
        }
        assert!(
            temp.exists(),
            "a temp in a live generation is never removed"
        );
    }

    /// Scenario: Mint caps for two attempts, write the first's caps file,
    /// publish its handed and committed records (and the handed one twice),
    /// scan, then offer forged records: a commit on the hand-out cap, an
    /// attempt never issued, and a random cap; mark the second attempt
    /// revoked. Each attempt's caps are found by its exact name, the
    /// duplicate publish is refused, genuine records are admitted once and
    /// then answer duplicate, every forgery is refused, the revoke marker
    /// names only its attempt, and no record is pruned.
    #[spec("inbox/receipt/009")]
    #[test]
    fn receipt_009_records_are_admitted_on_their_own_stage_cap_and_never_pruned() {
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let generation = spool.generation("2", "agent-a").unwrap();
        let mut store = EvidenceStore::default();
        let caps = store.issue("m-1", 1, 1).unwrap();
        let newer = store.issue("m-1", 1, 2).unwrap();
        generation.write_caps("m-1", 1, 1, &caps).unwrap();
        generation.write_caps("m-1", 1, 2, &newer).unwrap();
        assert_eq!(
            generation.read_caps("m-1", 1, 1).unwrap(),
            (caps.handed.hex(), caps.commit.hex()),
            "found by its exact attempt, not the newest"
        );

        let handed = key("m-1", 1, RecordStage::Handed);
        let committed = key("m-1", 1, RecordStage::Committed);
        generation.publish(&handed, &caps.handed).unwrap();
        let again = generation.publish(&handed, &caps.handed).unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
        generation.publish(&committed, &caps.commit).unwrap();

        let mut scanner = generation.scanner().unwrap();
        let (mut admitted, _) = scan_cycle(&mut scanner, &mut store, 4);
        admitted.sort();
        assert_eq!(admitted, vec![handed.clone(), committed.clone()]);
        let (again, _) = scan_cycle(&mut scanner, &mut store, 4);
        assert!(again.is_empty(), "a second cycle admits nothing twice");

        let forged_commit = RecordBody {
            msg: "m-1".into(),
            a: 1,
            e: 2,
            stage: RecordStage::Committed,
            cap: newer.handed.hex(),
        };
        assert_eq!(store.admit(&forged_commit), Admission::WrongCap);
        let not_issued = RecordBody {
            e: 9,
            cap: caps.commit.hex(),
            ..forged_commit.clone()
        };
        assert_eq!(store.admit(&not_issued), Admission::NotIssued);
        let random = RecordBody {
            cap: Cap::random().unwrap().hex(),
            ..forged_commit.clone()
        };
        assert_eq!(store.admit(&random), Admission::WrongCap);
        assert!(!store.admitted(&forged_commit.key()));

        generation.write_revoke("m-1", 1, 2).unwrap();
        assert!(generation.revoked("m-1", 1, 2));
        assert!(!generation.revoked("m-1", 1, 1));

        let rec = rec_dir(&spool, "2", "agent-a");
        let mut names: Vec<String> = std::fs::read_dir(&rec)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec![committed.file_name(), handed.file_name()]);
    }

    /// Scenario: Complete a scan cycle over an empty generation, then link a
    /// genuine commit record for an issued attempt (as a publisher racing a
    /// scan does behind its cursor), and ask about that attempt and about one
    /// that never published. The completed cycle never saw the late record,
    /// yet the exact lookup finds it, so it is never read as absent; the
    /// never-published attempt reads absent.
    #[spec("inbox/receipt/010")]
    #[test]
    fn receipt_010_a_record_linked_behind_the_cursor_is_found_by_the_exact_lookup() {
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let generation = spool.generation("2", "agent-a").unwrap();
        let mut store = EvidenceStore::default();
        let late = store.issue("m-late", 1, 1).unwrap();
        store.issue("m-never", 1, 1).unwrap();
        let mut scanner = generation.scanner().unwrap();
        let (admitted, passes) = scan_cycle(&mut scanner, &mut store, 4);
        assert!(passes.last().unwrap().complete && admitted.is_empty());
        let record = key("m-late", 1, RecordStage::Committed);
        generation.publish(&record, &late.commit).unwrap();
        assert!(scanner.complete(), "no new cycle has begun");
        assert!(!store.admitted(&record), "the scan never saw it");
        assert_eq!(
            scanner.presence(&mut store, "m-late", 1, 1),
            Presence::Found
        );
        assert!(store.admitted(&record));
        assert_eq!(
            scanner.presence(&mut store, "m-never", 1, 1),
            Presence::Absent
        );
    }

    /// Scenario: A publisher writes a record temp, then pauses for ten
    /// minutes of file age across three complete scan cycles before linking
    /// it; an abandoned-looking caps temp sits beside it. The scans leave
    /// both temps alone, the paused publisher's link succeeds, and the next
    /// scan admits its record.
    #[spec("inbox/receipt/011")]
    #[test]
    fn receipt_011_a_publisher_paused_between_write_and_link_still_publishes() {
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let generation = spool.generation("2", "agent-a").unwrap();
        let mut store = EvidenceStore::default();
        let caps = store.issue("m-slow", 1, 1).unwrap();
        let record = key("m-slow", 1, RecordStage::Handed);
        let body = serde_json::to_vec(&RecordBody {
            msg: "m-slow".into(),
            a: 1,
            e: 1,
            stage: RecordStage::Handed,
            cap: caps.handed.hex(),
        })
        .unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(600);
        let (temp, mut file) = generation.create_record_temp().unwrap();
        file.write_all(&body).unwrap();
        file.set_modified(old).unwrap();
        drop(file);
        let caps_temp = format!("{TEMP_PREFIX}caps");
        let file = generation.caps.create_new_owner_only(&caps_temp).unwrap();
        file.set_modified(old).unwrap();
        drop(file);

        let mut scanner = generation.scanner().unwrap();
        for _ in 0..3 {
            let (admitted, _) = scan_cycle(&mut scanner, &mut store, 4);
            assert!(admitted.is_empty());
        }
        generation
            .rec
            .link(&temp, &record.file_name())
            .expect("the paused publisher links");
        generation.rec.remove_file(&temp).unwrap();
        let (admitted, _) = scan_cycle(&mut scanner, &mut store, 4);
        assert_eq!(admitted, vec![record]);
        assert!(generation.caps.entry(&caps_temp).is_ok());
    }

    /// Scenario: Scan a generation holding one genuine record while
    /// injecting, in turn, an enumeration error mid-cycle (an `EILSEQ` on
    /// Unix), a failed rewind at the start of the next cycle, and a record
    /// read that fails after 10 bytes. Each errored entry is charged to its
    /// pass, the failed read's bytes are charged, no errored cycle is
    /// complete, an attempt that never published reads unknown rather than
    /// absent, the scanner reports itself degraded from the first error on,
    /// and a later clean cycle completes again.
    #[spec("inbox/receipt/012")]
    #[test]
    fn receipt_012_scan_errors_never_certify_absence() {
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let generation = spool.generation("2", "agent-a").unwrap();
        let mut store = EvidenceStore::default();
        let caps = store.issue("m-real", 1, 1).unwrap();
        store.issue("m-missing", 1, 1).unwrap();
        generation
            .publish(&key("m-real", 1, RecordStage::Committed), &caps.commit)
            .unwrap();
        let mut scanner = generation.scanner().unwrap();

        #[cfg(unix)]
        let injected = io::Error::from_raw_os_error(libc::EILSEQ);
        #[cfg(windows)]
        let injected = io::Error::other("injected enumeration failure");
        scanner.faults.enumerate.push_back(injected);
        let first = scanner.pass(&mut store);
        assert!(first.error.is_some());
        assert!(
            first.ended && !first.complete,
            "an errored cycle is not complete"
        );
        assert!(first.examined >= 2, "the errored entry is charged");
        assert!(!scanner.complete());
        assert!(scanner.degraded().is_some());
        assert_eq!(
            scanner.presence(&mut store, "m-missing", 1, 1),
            Presence::Unknown
        );

        let (_, clean) = scan_cycle(&mut scanner, &mut store, 4);
        assert!(clean.last().unwrap().complete);
        assert_eq!(
            scanner.presence(&mut store, "m-missing", 1, 1),
            Presence::Absent
        );

        scanner.faults.rewind = Some(io::Error::other("injected rewind failure"));
        let rewound = scanner.pass(&mut store);
        assert!(rewound.error.is_some() && !rewound.complete);
        assert_eq!(
            scanner.presence(&mut store, "m-missing", 1, 1),
            Presence::Unknown
        );
        let (_, after) = scan_cycle(&mut scanner, &mut store, 4);
        assert!(
            !after.last().unwrap().complete,
            "the cycle met the rewind error"
        );
        let (_, clean) = scan_cycle(&mut scanner, &mut store, 4);
        assert!(clean.last().unwrap().complete);

        let lone = spool.generation("3", "agent-b").unwrap();
        let mut fresh = EvidenceStore::default();
        let caps2 = fresh.issue("m-two", 1, 1).unwrap();
        fresh.issue("m-missing", 1, 1).unwrap();
        lone.publish(&key("m-two", 1, RecordStage::Handed), &caps2.handed)
            .unwrap();
        let mut scanner = lone.scanner().unwrap();
        scanner.faults.read_error_after = Some(10);
        let (admitted, passes) = scan_cycle(&mut scanner, &mut fresh, 4);
        let failed = &passes[0];
        assert!(failed.error.is_some());
        assert!(admitted.is_empty());
        assert_eq!(failed.bytes, 10, "a failed read's bytes are charged");
        assert!(!passes.last().unwrap().complete);
        assert_eq!(
            scanner.presence(&mut fresh, "m-missing", 1, 1),
            Presence::Unknown
        );
        assert!(scanner.degraded().is_some());
    }

    /// Scenario: Fill a generation with 600 empty record-named files that
    /// each grow to 4,097 bytes between the scanner's stat and its read, then
    /// scan; separately run 64 complete cycles, each over 32 fresh forged
    /// records nobody issued beside one genuine record. No pass reads more
    /// than 1 MiB however the files grew, so the first cycle takes several
    /// passes; across the 64 cycles the evidence store holds only the issued
    /// attempt and its one admitted record, and every cycle ends with nothing
    /// carried over.
    #[spec("inbox/receipt/013")]
    #[test]
    fn receipt_013_growing_files_and_forged_records_stay_within_bounds() {
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let generation = spool.generation("2", "agent-a").unwrap();
        let rec = rec_dir(&spool, "2", "agent-a");
        for i in 0..600 {
            std::fs::write(
                rec.join(key(&format!("m-grow{i}"), 1, RecordStage::Handed).file_name()),
                b"",
            )
            .unwrap();
        }
        let mut store = EvidenceStore::default();
        let mut scanner = generation.scanner().unwrap();
        let grow = rec.clone();
        scanner.faults.before_open = Some(Box::new(move |name: &str| {
            if name.starts_with("m-grow") {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(grow.join(name))
                    .unwrap();
                file.write_all(&[b'x'; READ_RESERVE as usize]).unwrap();
            }
        }));
        let (_, passes) = scan_cycle(&mut scanner, &mut store, 8);
        assert!(passes.len() >= 3, "{} passes", passes.len());
        for pass in &passes {
            assert!(pass.bytes <= SCAN_MAX_BYTES, "{} bytes", pass.bytes);
        }
        assert_eq!(passes.iter().map(|p| p.removed).sum::<usize>(), 600);

        let generation = spool.generation("3", "agent-b").unwrap();
        let rec = rec_dir(&spool, "3", "agent-b");
        let mut store = EvidenceStore::default();
        let caps = store.issue("m-real", 1, 1).unwrap();
        generation
            .publish(&key("m-real", 1, RecordStage::Handed), &caps.handed)
            .unwrap();
        let mut scanner = generation.scanner().unwrap();
        let mut refused = 0;
        for cycle in 0..64 {
            let names: Vec<String> = (0..32)
                .map(|i| {
                    let msg = format!("m-fake{cycle}x{i}");
                    let name = key(&msg, 1, RecordStage::Committed).file_name();
                    let forged = Cap::random().unwrap().hex();
                    write_record(&rec, &name, &msg, 1, "committed", &forged);
                    name
                })
                .collect();
            let (_, passes) = scan_cycle(&mut scanner, &mut store, 4);
            assert!(passes.last().unwrap().complete);
            assert!(scanner.carry.is_none(), "nothing carried past a cycle");
            refused += passes.iter().map(|p| p.refused).sum::<usize>();
            for name in names {
                std::fs::remove_file(rec.join(name)).unwrap();
            }
        }
        assert_eq!(refused, 64 * 32);
        assert_eq!(
            store.entries(),
            2,
            "one issued attempt, one admitted record"
        );
    }

    /// Scenario: Stop a spool whose record directory holds 2,548 files (on
    /// Linux, one with a name that is not UTF-8), stop a second, small one,
    /// leave a pending lock file nobody holds and one a live start still
    /// holds, then start spools one after another. Each start removes more of
    /// the large dead spool while keeping its lock file, the small one is
    /// reaped too, both are gone after a few starts with their lock files,
    /// the unheld pending file goes, and the held one stays.
    #[spec("inbox/receipt/014")]
    #[test]
    fn receipt_014_a_dead_spool_over_the_budget_is_reaped_across_starts() {
        let state = crate::test_temp::tempdir().unwrap();
        let root = state.path().join(SPOOL_DIR_NAME);
        let big = Spool::start(state.path()).unwrap();
        big.generation("2", "agent-a").unwrap();
        let rec = rec_dir(&big, "2", "agent-a");
        for i in 0..(SCAN_MAX_ENTRIES + 500) {
            std::fs::write(rec.join(format!("junk-{i}")), b"").unwrap();
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStrExt as _;
            std::fs::write(rec.join(OsStr::from_bytes(b"bad-\xff-name")), b"").unwrap();
        }
        let small = Spool::start(state.path()).unwrap();
        small
            .generation("3", "agent-b")
            .unwrap()
            .publish(&key("m-1", 1, RecordStage::Handed), &Cap::random().unwrap())
            .unwrap();
        let dead = [
            (big.instance_path(), root.join(lock_name(big.instance()))),
            (
                small.instance_path(),
                root.join(lock_name(small.instance())),
            ),
        ];
        drop(big);
        drop(small);
        let unheld = root.join(format!("{PENDING_LOCK_PREFIX}{}", "a".repeat(32)));
        std::fs::write(&unheld, b"").unwrap();
        let held_path = root.join(format!("{PENDING_LOCK_PREFIX}{}", "b".repeat(32)));
        let held = File::create(&held_path).unwrap();
        assert!(crate::platform::lock::try_lock_file(&held).unwrap());

        let count = || std::fs::read_dir(&rec).map(|d| d.count()).unwrap_or(0);
        let mut last = count();
        let mut live = Vec::new();
        for _ in 0..8 {
            live.push(Spool::start(state.path()).unwrap());
            if !dead[0].0.exists() {
                break;
            }
            let now = count();
            assert!(now < last, "each start makes progress: {now} after {last}");
            assert!(dead[0].1.exists(), "the lock file stays while work remains");
            last = now;
        }
        for _ in 0..8 {
            if !dead[1].0.exists() {
                break;
            }
            live.push(Spool::start(state.path()).unwrap());
        }
        for (dir, lock) in &dead {
            assert!(!dir.exists(), "{} was not reaped", dir.display());
            assert!(!lock.exists());
        }
        assert!(!unheld.exists(), "an unheld pending lock is reaped");
        assert!(held_path.exists(), "a held pending lock is never reaped");
        for spool in &live {
            assert!(spool.instance_path().is_dir());
        }
    }

    /// Scenario: A Pi context takes an attempt and is retired; its watcher
    /// reconnects with the callback queue intact and answers retained, so
    /// the attempt is rebound to the new incarnation. The old incarnation,
    /// still holding the attempt's hand-out cap, then writes not_handed and
    /// handed_inert records into the spool. The scan admits neither, so
    /// nothing reaches the model, which keeps the attempt bound to the new
    /// incarnation and the message attempting.
    #[spec("inbox/receipt/015")]
    #[test]
    fn receipt_015_a_retired_receiver_cannot_release_an_attempt_through_the_spool() {
        use crate::inbox_attempts::{
            AttemptModel, Limits, MessageState, ResumeAnswer, Slot, Stage,
        };
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let generation = spool.generation("2", "agent-a").unwrap();
        let rec = rec_dir(&spool, "2", "agent-a");
        let t = std::time::Instant::now();
        let mut model = AttemptModel::new(Limits::default(), 1);
        let mut store = EvidenceStore::default();
        let rx1 = model.register(t, Slot::Pi, true).rx;
        model.post("m-1", false);
        let issued = model.issue(t, "m-1", rx1).unwrap();
        model.snapshot(t, rx1, 1, issued.d, &[("m-1", issued.a, issued.e)]);
        let caps = store.issue("m-1", issued.a, issued.e).unwrap();
        let rx2 = model.register(t, Slot::Pi, true).rx;
        model.resume_answer(t, rx2, "m-1", issued.a, issued.e, ResumeAnswer::Retained);

        for stage in ["not_handed", "handed_inert"] {
            let name = format!("m-1.{}.{}.{stage}", issued.a, issued.e);
            write_record(&rec, &name, "m-1", issued.e, stage, &caps.handed.hex());
        }
        let mut scanner = generation.scanner().unwrap();
        let (admitted, passes) = scan_cycle(&mut scanner, &mut store, 4);
        assert!(admitted.is_empty(), "{admitted:?}");
        assert_eq!(passes.iter().map(|p| p.refused).sum::<usize>(), 0);
        for key in &admitted {
            let stage = match key.stage {
                RecordStage::Handed => Stage::Handed,
                RecordStage::Committed => Stage::Committed,
            };
            model.stage(t, &key.msg, key.a, key.e, stage, None);
        }
        let attempt = model
            .delivery("m-1")
            .unwrap()
            .attempt(issued.a, issued.e)
            .unwrap()
            .clone();
        assert_eq!((attempt.rx, attempt.stage), (rx2, Stage::Issued));
        assert_eq!(model.state("m-1"), Some(MessageState::Attempting));
    }

    /// Scenario: On Windows, start a spool and open a generation, then read
    /// back the stored DACL of the root, the instance and every generation
    /// directory; pre-create a spool root under the temp directory's
    /// inherited ACL and start a spool on it; and try to rename a held
    /// generation's `rec` directory aside (one route to a junction swap; the
    /// in-place route is inbox/receipt/017) before and after the generation
    /// is dropped. Every created directory's
    /// DACL is protected with one owner ACE; the permissive root is refused
    /// with its DACL unchanged; the rename fails while the generation holds
    /// the directory and succeeds once it is released. Runs only on Windows
    /// CI.
    #[cfg(windows)]
    #[spec("inbox/receipt/016")]
    #[test]
    fn receipt_016_windows_directories_are_owner_only_and_held_against_replacement() {
        use std::os::windows::fs::OpenOptionsExt as _;
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
        let stored = |path: &Path| {
            let dir = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)
                .unwrap();
            crate::platform::fsperm::read_dacl(
                dir.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE
            )
            .unwrap()
        };
        let ours = crate::platform::paths::current_user_sid().unwrap();
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let generation = spool.generation("2", "agent-a").unwrap();
        let agent = spool.instance_path().join("2").join("agent-a");
        for path in [
            state.path().join(SPOOL_DIR_NAME),
            spool.instance_path(),
            spool.instance_path().join("2"),
            agent.clone(),
            agent.join("rec"),
            agent.join("caps"),
            agent.join("revoked"),
        ] {
            let dacl = stored(&path);
            crate::platform::fsperm::dacl_is_owner_only(&dacl, &ours)
                .unwrap_or_else(|why| panic!("{}: {why}: {dacl:?}", path.display()));
            assert_eq!(dacl.aces.len(), 1, "{}: {dacl:?}", path.display());
        }

        let state2 = crate::test_temp::tempdir().unwrap();
        let root = state2.path().join(SPOOL_DIR_NAME);
        std::fs::create_dir(&root).unwrap();
        let before = stored(&root);
        let err = Spool::start(state2.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert_eq!(stored(&root), before, "refused, not repaired");

        let moved = agent.join("rec-moved");
        assert!(
            std::fs::rename(agent.join("rec"), &moved).is_err(),
            "a held component cannot be renamed for a swap"
        );
        generation
            .publish(&key("m-1", 1, RecordStage::Handed), &Cap::random().unwrap())
            .unwrap();
        drop(generation);
        std::fs::rename(agent.join("rec"), &moved).expect("released once dropped");
        assert!(
            moved
                .join(key("m-1", 1, RecordStage::Handed).file_name())
                .is_file()
        );
    }

    /// Scenario: On Windows, open a generation whose `rec` is empty, then
    /// from a second, writable handle turn that held `rec` into a mount point
    /// aimed at a decoy directory in place, with `FSCTL_SET_REPARSE_POINT`
    /// and no rename. A publish into it is refused and nothing lands in the
    /// decoy; once the mount point is deleted again the same generation
    /// publishes normally, a fresh generation is created and publishes, and
    /// after everything is dropped a later start reaps the instance. Runs
    /// only on Windows CI.
    #[cfg(windows)]
    #[spec("inbox/receipt/017")]
    #[test]
    fn receipt_017_windows_an_in_place_reparse_point_is_refused() {
        use std::os::windows::ffi::OsStrExt as _;
        use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;
        const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
        const FSCTL_DELETE_REPARSE_POINT: u32 = 0x0009_00AC;
        const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
        let control = |file: &std::fs::File, code: u32, buffer: &[u8]| {
            let mut returned = 0u32;
            // SAFETY: `file` is an open handle, `buffer` is a live input
            // buffer of the length passed, and no output or overlapped
            // structure is used.
            let ok = unsafe {
                DeviceIoControl(
                    file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
                    code,
                    buffer.as_ptr().cast(),
                    buffer.len() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        };
        let reparse = |path: &Path| {
            std::fs::symlink_metadata(path).unwrap().file_attributes()
                & FILE_ATTRIBUTE_REPARSE_POINT
                != 0
        };

        let state = crate::test_temp::tempdir().unwrap();
        let decoy = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let generation = spool.generation("2", "agent-a").unwrap();
        let rec = spool.instance_path().join("2").join("agent-a").join("rec");
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&rec)
            .expect("a same-user writer opens the held, empty rec");
        let target = decoy.path().to_string_lossy().into_owned();
        let target = target.strip_prefix(r"\\?\").unwrap_or(&target);
        let substitute: Vec<u16> = std::ffi::OsStr::new(&format!(r"\??\{target}\"))
            .encode_wide()
            .collect();
        let names = (substitute.len() as u16 + 2) * 2;
        let mut buffer = Vec::new();
        buffer.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        buffer.extend((8 + names).to_le_bytes());
        buffer.extend(0u16.to_le_bytes());
        buffer.extend(0u16.to_le_bytes());
        buffer.extend((substitute.len() as u16 * 2).to_le_bytes());
        buffer.extend((substitute.len() as u16 * 2 + 2).to_le_bytes());
        buffer.extend(0u16.to_le_bytes());
        for unit in substitute.iter().chain(&[0u16, 0]) {
            buffer.extend(unit.to_le_bytes());
        }
        control(&writer, FSCTL_SET_REPARSE_POINT, &buffer)
            .expect("the in-place reparse is possible, so the boundary must catch it");
        assert!(reparse(&rec), "rec is now a mount point, with no rename");

        let handed = key("m-1", 1, RecordStage::Handed);
        let err = generation
            .publish(&handed, &Cap::random().unwrap())
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert!(
            std::fs::read_dir(decoy.path()).unwrap().next().is_none(),
            "nothing is written through the mount point"
        );

        let mut header = Vec::new();
        header.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        header.extend([0u8; 4]);
        control(&writer, FSCTL_DELETE_REPARSE_POINT, &header).expect("delete the mount point");
        drop(writer);
        assert!(!reparse(&rec));
        generation
            .publish(&handed, &Cap::random().unwrap())
            .expect("an intact chain publishes again");
        assert!(rec.join(handed.file_name()).is_file());
        let fresh = spool.generation("3", "agent-b").unwrap();
        let other = key("m-2", 1, RecordStage::Committed);
        fresh.publish(&other, &Cap::random().unwrap()).unwrap();
        assert!(
            spool
                .instance_path()
                .join("3")
                .join("agent-b")
                .join("rec")
                .join(other.file_name())
                .is_file()
        );

        let old = spool.instance_path();
        drop((generation, fresh, spool));
        let next = Spool::start(state.path()).unwrap();
        assert!(!old.exists(), "the released instance is reaped");
        drop(next);
    }

    /// Scenario: Fill a spool root with three times one reap step's listing
    /// of names that stay (files no daemon uses), add eight stopped spools
    /// beside them, start a spool, and run further reap steps. Every stopped
    /// spool is reached and removed with its lock file within a few steps,
    /// wherever the directory lists it, and the names that stay and the live
    /// spool remain.
    #[spec("inbox/receipt/018")]
    #[test]
    fn receipt_018_reaping_advances_past_a_root_listing_that_stays() {
        let state = crate::test_temp::tempdir().unwrap();
        let root = state.path().join(SPOOL_DIR_NAME);
        drop(Spool::start(state.path()).unwrap());
        let window = 64;
        for i in 0..3 * window {
            std::fs::write(root.join(format!("junk-{i}")), b"").unwrap();
        }
        let dead: Vec<String> = (0..8).map(|_| random_hex(16).unwrap()).collect();
        for stem in &dead {
            std::fs::create_dir(root.join(stem)).unwrap();
            std::fs::write(root.join(stem).join("left"), b"x").unwrap();
            std::fs::write(root.join(lock_name(stem)), b"").unwrap();
        }
        let spool = Spool::start_listing(state.path(), window).unwrap();
        let gone = || {
            dead.iter()
                .filter(|stem| !root.join(stem).exists() && !root.join(lock_name(stem)).exists())
                .count()
        };
        let mut steps = 1;
        while gone() < dead.len() && steps < 20 {
            spool.reap_step();
            steps += 1;
        }
        assert_eq!(
            gone(),
            dead.len(),
            "every stopped spool is reached, after {steps} steps"
        );
        for i in 0..3 * window {
            assert!(root.join(format!("junk-{i}")).exists());
        }
        assert!(spool.instance_path().is_dir());
    }
}
