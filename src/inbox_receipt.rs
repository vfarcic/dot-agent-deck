//! PRD #1666 design v3.1 §3 and §5: the receipt spool, its trusted traversal
//! and bounded scans, and the suppression-evidence store.
//!
//! A receiver component publishes an immutable stage record for a delivery
//! attempt (`<msg>.<A>.<e>.<stage>`) into its agent generation's spool before
//! the daemon is asked anything, and the daemon reads it back. **In this slice
//! the spool sits beside the current ack** (§13 step 3): records are published
//! and read, nothing is pruned, and no suppression decision is drawn from them.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
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
/// Directory entries one scan pass examines.
pub const SCAN_MAX_ENTRIES: usize = 2048;
/// Record bytes one scan pass reads.
pub const SCAN_MAX_BYTES: u64 = 1024 * 1024;
/// Abandoned temps one scan pass removes.
pub const SCAN_MAX_TEMPS: usize = 256;
/// How old a temp must be, seen on a later pass, before it is removed.
pub const TEMP_MIN_AGE: Duration = Duration::from_secs(60);

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

/// The stage a record states (§2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordStage {
    Handed,
    Committed,
    NotHanded,
    HandedInert,
}

impl RecordStage {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordStage::Handed => "handed",
            RecordStage::Committed => "committed",
            RecordStage::NotHanded => "not_handed",
            RecordStage::HandedInert => "handed_inert",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "handed" => RecordStage::Handed,
            "committed" => RecordStage::Committed,
            "not_handed" => RecordStage::NotHanded,
            "handed_inert" => RecordStage::HandedInert,
            _ => return None,
        })
    }

    /// `Committed` is proven by the commit cap; every other stage by the
    /// hand-out cap, so a `Handed` cap can never become a commit (§5).
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

/// A 128-bit OS-random capability (§5). The daemon keeps only its hash.
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

fn refused(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, what.to_string())
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
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a message id must be one spool path component",
        ));
    }
    Ok(format!("{msg}.{a}.{e}"))
}

/// The daemon's spool namespace for one run: `<root>/<instance>/`, held by a
/// lock on `<root>/<instance>.lock` for as long as this value lives.
#[derive(Debug)]
pub struct Spool {
    root_path: PathBuf,
    instance: String,
    dir: fsdir::TrustedDir,
    _lock: File,
}

impl Spool {
    /// Create or open `<state_dir>/inbox-spool`, mint and lock this run's
    /// instance, and reap every sibling instance no running daemon holds.
    ///
    /// The root and every directory below it are opened one component at a
    /// time and verified: a symlink is refused rather than followed, and a
    /// pre-existing directory open to other users is refused rather than
    /// repaired. The lock file is locked under a temporary name and only then
    /// renamed into place, so a reaper never sees an instance's lock free
    /// while its daemon runs; an instance directory with no lock file is
    /// never reaped.
    pub fn start(state_dir: &Path) -> io::Result<Spool> {
        let anchor = fsdir::TrustedDir::anchor(state_dir)?;
        let root = anchor.child(SPOOL_DIR_NAME, true)?;
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
        let dir = root.child(&instance, true)?;
        reap_siblings(&root, &instance);
        Ok(Spool {
            root_path: state_dir.join(SPOOL_DIR_NAME),
            instance,
            dir,
            _lock: lock,
        })
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// The value of [`ENV_INBOX_SPOOL`] for this run.
    pub fn instance_path(&self) -> PathBuf {
        self.root_path.join(&self.instance)
    }

    /// The daemon's side of one agent generation's spool, created as needed.
    pub fn generation(&self, pane: &str, agent: &str) -> io::Result<GenerationSpool> {
        GenerationSpool::at(&self.dir, pane, agent, true)
    }
}

/// Reap every sibling instance whose lock no running daemon holds: try-lock
/// its lock file and keep holding it while the instance directory is removed,
/// unlinking the lock file last. A busy lock is skipped, and so is a
/// directory with no lock file. A reap that runs out of its entry budget keeps
/// the lock file, so the next start resumes it.
fn reap_siblings(root: &fsdir::TrustedDir, own: &str) {
    let Ok(mut stream) = root.stream() else {
        return;
    };
    let mut stems = Vec::new();
    for _ in 0..SCAN_MAX_ENTRIES {
        let Some(name) = stream.next_name() else {
            break;
        };
        if let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(LOCK_SUFFIX))
            && stem != own
            && is_instance(stem)
        {
            stems.push(stem.to_string());
        }
    }
    drop(stream);
    let mut budget = SCAN_MAX_ENTRIES;
    for stem in stems {
        let name = lock_name(&stem);
        let Ok(lock) = root.open_regular(&name) else {
            continue;
        };
        if !matches!(crate::platform::lock::try_lock_file(&lock), Ok(true)) {
            continue;
        }
        if let Ok(true) = remove_tree(root, &stem, 0, &mut budget) {
            let _ = root.remove_file(&name);
        }
        drop(lock);
    }
}

/// Remove `parent/name` and everything below it without following a link:
/// `Ok(false)` when the entry budget ran out first.
fn remove_tree(
    parent: &fsdir::TrustedDir,
    name: &str,
    depth: usize,
    budget: &mut usize,
) -> io::Result<bool> {
    let dir = match parent.child(name, false) {
        Ok(dir) => dir,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    let mut stream = dir.stream()?;
    let mut names = Vec::new();
    while let Some(entry) = stream.next_name() {
        if *budget == 0 {
            return Ok(false);
        }
        *budget -= 1;
        let Some(entry) = entry.to_str() else {
            return Ok(false);
        };
        names.push(entry.to_string());
    }
    drop(stream);
    for entry in names {
        match dir.entry(&entry)?.kind {
            fsdir::EntryKind::Dir if depth < 3 => {
                if !remove_tree(&dir, &entry, depth + 1, budget)? {
                    return Ok(false);
                }
            }
            fsdir::EntryKind::Dir => return Ok(false),
            _ => dir.remove_file(&entry)?,
        }
    }
    drop(dir);
    parent.remove_dir(name)?;
    Ok(true)
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
    /// pinned instance path, creating nothing.
    pub fn open(instance_path: &Path, pane: &str, agent: &str) -> io::Result<GenerationSpool> {
        let base = fsdir::TrustedDir::anchor(instance_path)?;
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
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a message id must be one spool path component",
            ));
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
            found: BTreeMap::new(),
            complete: false,
            sighted: BTreeSet::new(),
        })
    }
}

/// Write `bytes` to an owner-only temp in `dir` and link it to `name`, which
/// must not exist yet; the temp is removed either way.
fn write_linked(dir: &fsdir::TrustedDir, name: &str, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a spool record is at most 4 KiB",
        ));
    }
    let temp = format!("{TEMP_PREFIX}{}", random_hex(8)?);
    let mut file = dir.create_new_owner_only(&temp)?;
    let written = file.write_all(bytes);
    drop(file);
    let linked = written.and_then(|()| dir.link(&temp, name));
    let _ = dir.remove_file(&temp);
    linked
}

/// One scan pass's result. An incomplete pass is never absence.
#[derive(Debug, Default)]
pub struct ScanPass {
    pub records: Vec<RecordBody>,
    pub complete: bool,
    /// Entries this pass examined.
    pub examined: usize,
    /// Invalid record files and aged temps this pass removed.
    pub removed: usize,
}

/// Whether a record is known to be in the spool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Found,
    /// The latest complete scan did not find it.
    Absent,
    /// No complete scan has answered since the current one began.
    Unknown,
}

/// Reads a generation's records in passes bounded by [`SCAN_MAX_ENTRIES`],
/// [`SCAN_MAX_BYTES`] and [`SCAN_MAX_TEMPS`], resuming where the last pass
/// stopped.
///
/// Entries that are not regular files (a FIFO, a symlink, a directory) are
/// skipped without being opened; a regular file is opened without following a
/// link and without blocking. A record-named file larger than
/// [`MAX_RECORD_BYTES`], or whose body does not match its name, is removed; a
/// temp is removed once a later pass sees it again more than [`TEMP_MIN_AGE`]
/// old; any other name is left alone. A valid record is never removed.
#[derive(Debug)]
pub struct RecordScanner {
    rec: fsdir::TrustedDir,
    stream: Option<fsdir::DirStream>,
    carry: Option<String>,
    found: BTreeMap<RecordKey, RecordBody>,
    complete: bool,
    sighted: BTreeSet<String>,
}

impl RecordScanner {
    pub fn pass(&mut self) -> io::Result<ScanPass> {
        let mut pass = ScanPass::default();
        if self.complete {
            if let Some(stream) = self.stream.as_mut() {
                stream.rewind();
            }
            self.complete = false;
        }
        if self.stream.is_none() {
            self.stream = Some(self.rec.stream()?);
        }
        let mut bytes = 0u64;
        let mut temps_removed = 0usize;
        loop {
            if pass.examined >= SCAN_MAX_ENTRIES {
                return Ok(pass);
            }
            let name = match self.carry.take() {
                Some(name) => name,
                None => {
                    let Some(stream) = self.stream.as_mut() else {
                        return Ok(pass);
                    };
                    match stream.next_name() {
                        Some(name) => match name.into_string() {
                            Ok(name) => name,
                            Err(_) => {
                                pass.examined += 1;
                                continue;
                            }
                        },
                        None => {
                            self.complete = true;
                            pass.complete = true;
                            return Ok(pass);
                        }
                    }
                }
            };
            if name.starts_with(TEMP_PREFIX) {
                pass.examined += 1;
                let Ok(info) = self.rec.entry(&name) else {
                    continue;
                };
                let aged = info
                    .modified
                    .and_then(|at| std::time::SystemTime::now().duration_since(at).ok())
                    .is_some_and(|age| age >= TEMP_MIN_AGE);
                if self.sighted.contains(&name) && aged && temps_removed < SCAN_MAX_TEMPS {
                    if info.kind == fsdir::EntryKind::File && self.rec.remove_file(&name).is_ok() {
                        temps_removed += 1;
                        pass.removed += 1;
                    }
                    self.sighted.remove(&name);
                } else {
                    self.sighted.insert(name);
                }
                continue;
            }
            let Some(key) = RecordKey::parse(&name) else {
                pass.examined += 1;
                continue;
            };
            let Ok(info) = self.rec.entry(&name) else {
                pass.examined += 1;
                continue;
            };
            if info.kind != fsdir::EntryKind::File {
                pass.examined += 1;
                continue;
            }
            if info.len > MAX_RECORD_BYTES {
                pass.examined += 1;
                if self.rec.remove_file(&name).is_ok() {
                    pass.removed += 1;
                }
                continue;
            }
            if bytes > 0 && bytes + info.len > SCAN_MAX_BYTES {
                self.carry = Some(name);
                return Ok(pass);
            }
            pass.examined += 1;
            let Ok(file) = self.rec.open_regular(&name) else {
                continue;
            };
            let mut buf = Vec::new();
            if file
                .take(MAX_RECORD_BYTES + 1)
                .read_to_end(&mut buf)
                .is_err()
            {
                continue;
            }
            bytes += buf.len() as u64;
            match serde_json::from_slice::<RecordBody>(&buf) {
                Ok(body) if buf.len() as u64 <= MAX_RECORD_BYTES && body.key() == key => {
                    self.found.insert(key, body.clone());
                    pass.records.push(body);
                }
                _ => {
                    if self.rec.remove_file(&name).is_ok() {
                        pass.removed += 1;
                    }
                }
            }
        }
    }

    /// Records are immutable and never pruned, so one seen stays found; a
    /// record not seen is absent only while the latest pass completed a scan.
    pub fn presence(&self, key: &RecordKey) -> Presence {
        if self.found.contains_key(key) {
            Presence::Found
        } else if self.complete {
            Presence::Absent
        } else {
            Presence::Unknown
        }
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
/// issued attempt's caps and the records admitted on them. In this slice it
/// only records; nothing is pruned and nothing is suppressed from it.
#[derive(Debug, Default)]
pub struct EvidenceStore {
    issued: BTreeMap<(String, u32, u32), IssuedCaps>,
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
        Ok(caps)
    }

    /// Admit a record on bearer authority (§5): an issued `(msg, A, e)` and
    /// that stage's cap — the commit cap for `Committed`, the hand-out cap for
    /// every other stage, so a hand-out cap never proves a commit.
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
}

/// Directory handles the spool trusts, one verified component at a time.
mod fsdir {
    use std::io;
    use std::time::SystemTime;

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
        pub(super) modified: Option<SystemTime>,
    }

    /// A file name inside one directory: no separator, not `.` or `..`.
    pub(super) fn plain(name: &str) -> io::Result<()> {
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
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
        use std::ffi::{CStr, CString, OsString};
        use std::fs::File;
        use std::io;
        use std::mem::MaybeUninit;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        use std::path::Path;
        use std::time::{Duration, SystemTime};

        use super::super::{refused, valid_component};
        use super::{EntryInfo, EntryKind, plain};

        fn cstr(name: &str) -> io::Result<CString> {
            CString::new(name)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in a name"))
        }

        fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
            if rc < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(rc)
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
            /// The trusted starting point, opened by path: owned by this user.
            pub(in super::super) fn anchor(path: &Path) -> io::Result<Self> {
                let c = CString::new(path.as_os_str().as_bytes())
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in a path"))?;
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

            /// One component below: created owner-only when `create`, opened
            /// without following a link, and verified owner-only.
            pub(in super::super) fn child(&self, name: &str, create: bool) -> io::Result<Self> {
                if !valid_component(name) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{name:?} is not a spool path component"),
                    ));
                }
                let c = cstr(name)?;
                if create {
                    // SAFETY: a live directory descriptor and a valid name.
                    let rc = unsafe { libc::mkdirat(self.fd.as_raw_fd(), c.as_ptr(), 0o700) };
                    if rc < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() != io::ErrorKind::AlreadyExists {
                            return Err(error);
                        }
                    }
                }
                // SAFETY: as above; the descriptor is owned below.
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
                        return Err(refused(format!("{name} is not a plain directory")));
                    }
                    return Err(error);
                }
                // SAFETY: just opened, owned by nobody else.
                let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                verify(fd.as_raw_fd(), name, true)?;
                Ok(TrustedDir { fd })
            }

            pub(in super::super) fn try_clone(&self) -> io::Result<Self> {
                Ok(TrustedDir {
                    fd: self.fd.try_clone()?,
                })
            }

            /// A new owner-only file, refused when the name exists or is a link.
            pub(in super::super) fn create_new_owner_only(&self, name: &str) -> io::Result<File> {
                plain(name)?;
                let c = cstr(name)?;
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
                plain(from)?;
                plain(to)?;
                let (f, t) = (cstr(from)?, cstr(to)?);
                let fd = self.fd.as_raw_fd();
                // SAFETY: live descriptors and valid names; no link is followed.
                cvt(unsafe { libc::linkat(fd, f.as_ptr(), fd, t.as_ptr(), 0) })?;
                Ok(())
            }

            pub(in super::super) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
                plain(from)?;
                plain(to)?;
                let (f, t) = (cstr(from)?, cstr(to)?);
                let fd = self.fd.as_raw_fd();
                // SAFETY: live descriptors and valid names.
                cvt(unsafe { libc::renameat(fd, f.as_ptr(), fd, t.as_ptr()) })?;
                Ok(())
            }

            pub(in super::super) fn remove_file(&self, name: &str) -> io::Result<()> {
                plain(name)?;
                let c = cstr(name)?;
                // SAFETY: a live descriptor and a valid name; a link is
                // removed, never followed.
                cvt(unsafe { libc::unlinkat(self.fd.as_raw_fd(), c.as_ptr(), 0) })?;
                Ok(())
            }

            pub(in super::super) fn remove_dir(&self, name: &str) -> io::Result<()> {
                plain(name)?;
                let c = cstr(name)?;
                // SAFETY: as above.
                cvt(unsafe {
                    libc::unlinkat(self.fd.as_raw_fd(), c.as_ptr(), libc::AT_REMOVEDIR)
                })?;
                Ok(())
            }

            /// What `name` is, without following a link or opening it.
            pub(in super::super) fn entry(&self, name: &str) -> io::Result<EntryInfo> {
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
                    modified: u64::try_from(st.st_mtime)
                        .ok()
                        .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
                })
            }

            /// Open a regular file for reading: no link followed, no open that
            /// blocks on a FIFO, and refused when it is not a regular file.
            pub(in super::super) fn open_regular(&self, name: &str) -> io::Result<File> {
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
                let dot = cstr(".")?;
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
            pub(in super::super) fn next_name(&mut self) -> Option<OsString> {
                loop {
                    // SAFETY: `dir` is a live stream owned by self.
                    let entry = unsafe { libc::readdir(self.dir) };
                    if entry.is_null() {
                        return None;
                    }
                    // SAFETY: readdir returned a valid entry whose name is
                    // NUL-terminated and lives until the next readdir.
                    let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                    if name == b"." || name == b".." {
                        continue;
                    }
                    return Some(OsString::from_vec(name.to_vec()));
                }
            }

            pub(in super::super) fn rewind(&mut self) {
                // SAFETY: `dir` is a live stream owned by self.
                unsafe { libc::rewinddir(self.dir) };
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
        use std::ffi::OsString;
        use std::fs::{File, OpenOptions};
        use std::io;
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use std::os::windows::io::AsRawHandle;
        use std::path::{Path, PathBuf};

        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };

        use super::super::{refused, valid_component};
        use super::{EntryInfo, EntryKind, plain};

        /// A directory with no reparse point (no symlink or junction) whose
        /// owner is this user.
        fn verify(path: &Path, what: &str) -> io::Result<()> {
            let meta = std::fs::symlink_metadata(path)?;
            if meta.file_type().is_symlink()
                || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            {
                return Err(refused(format!("{what} is a reparse point")));
            }
            if !meta.is_dir() {
                return Err(refused(format!("{what} is not a directory")));
            }
            let handle = OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(path)?;
            crate::platform::fsperm::verify_file_owner_is_current_user(
                handle.as_raw_handle() as HANDLE
            )
            .map_err(|why| refused(format!("{what}: {why}")))
        }

        /// A verified directory, held by path: Windows has no `openat`, so
        /// each component is re-verified as a non-reparse directory owned by
        /// this user when it is opened.
        #[derive(Debug)]
        pub(in super::super) struct TrustedDir {
            path: PathBuf,
        }

        impl TrustedDir {
            pub(in super::super) fn anchor(path: &Path) -> io::Result<Self> {
                if !std::fs::metadata(path)?.is_dir() {
                    return Err(refused(format!("{} is not a directory", path.display())));
                }
                Ok(TrustedDir {
                    path: path.to_path_buf(),
                })
            }

            pub(in super::super) fn child(&self, name: &str, create: bool) -> io::Result<Self> {
                if !valid_component(name) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{name:?} is not a spool path component"),
                    ));
                }
                let path = self.path.join(name);
                if create {
                    match std::fs::create_dir(&path) {
                        Ok(()) => crate::platform::fsperm::ensure_owner_only_dir(&path)?,
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error),
                    }
                }
                verify(&path, name)?;
                Ok(TrustedDir { path })
            }

            pub(in super::super) fn try_clone(&self) -> io::Result<Self> {
                Ok(TrustedDir {
                    path: self.path.clone(),
                })
            }

            /// A new file whose owner-only DACL is set on its handle before
            /// anything is written.
            pub(in super::super) fn create_new_owner_only(&self, name: &str) -> io::Result<File> {
                plain(name)?;
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                crate::platform::fsperm::set_create_mode_owner_only(&mut options);
                options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
                let file = options.open(self.path.join(name))?;
                crate::platform::fsperm::set_file_owner_only(&file)?;
                Ok(file)
            }

            pub(in super::super) fn link(&self, from: &str, to: &str) -> io::Result<()> {
                plain(from)?;
                plain(to)?;
                std::fs::hard_link(self.path.join(from), self.path.join(to))
            }

            pub(in super::super) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
                plain(from)?;
                plain(to)?;
                std::fs::rename(self.path.join(from), self.path.join(to))
            }

            pub(in super::super) fn remove_file(&self, name: &str) -> io::Result<()> {
                plain(name)?;
                std::fs::remove_file(self.path.join(name))
            }

            pub(in super::super) fn remove_dir(&self, name: &str) -> io::Result<()> {
                plain(name)?;
                std::fs::remove_dir(self.path.join(name))
            }

            pub(in super::super) fn entry(&self, name: &str) -> io::Result<EntryInfo> {
                plain(name)?;
                let meta = std::fs::symlink_metadata(self.path.join(name))?;
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
                    modified: meta.modified().ok(),
                })
            }

            pub(in super::super) fn open_regular(&self, name: &str) -> io::Result<File> {
                if self.entry(name)?.kind != EntryKind::File {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "it is not a regular file",
                    ));
                }
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                    .open(self.path.join(name))?;
                if !file.metadata()?.is_file() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "it is not a regular file",
                    ));
                }
                Ok(file)
            }

            pub(in super::super) fn stream(&self) -> io::Result<DirStream> {
                Ok(DirStream {
                    path: self.path.clone(),
                    iter: Some(std::fs::read_dir(&self.path)?),
                })
            }
        }

        #[derive(Debug)]
        pub(in super::super) struct DirStream {
            path: PathBuf,
            iter: Option<std::fs::ReadDir>,
        }

        impl DirStream {
            pub(in super::super) fn next_name(&mut self) -> Option<OsString> {
                loop {
                    match self.iter.as_mut()?.next()? {
                        Ok(entry) => return Some(entry.file_name()),
                        Err(_) => continue,
                    }
                }
            }

            pub(in super::super) fn rewind(&mut self) {
                self.iter = std::fs::read_dir(&self.path).ok();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

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

    /// Run passes until one completes, collecting every record; at most
    /// `limit` passes.
    fn scan_cycle(scanner: &mut RecordScanner, limit: usize) -> (Vec<RecordBody>, Vec<ScanPass>) {
        let mut records = Vec::new();
        let mut passes = Vec::new();
        for _ in 0..limit {
            let pass = scanner.pass().expect("scan pass");
            records.extend(pass.records.iter().cloned());
            let complete = pass.complete;
            passes.push(pass);
            if complete {
                break;
            }
        }
        (records, passes)
    }

    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
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
    /// record body outside the spool and a directory, each named like a
    /// record, then scan. The scan finishes without blocking, returns only the
    /// genuine record, never follows the symlink, and leaves all three entries
    /// where they are.
    #[cfg(unix)]
    #[spec("inbox/receipt/003")]
    #[test]
    fn receipt_003_fifo_symlink_and_directory_entries_are_skipped_unopened() {
        use std::os::unix::ffi::OsStrExt as _;
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let generation = spool.generation("2", "agent-a").unwrap();
        let rec = rec_dir(&spool, "2", "agent-a");
        generation
            .publish(
                &key("m-good", 1, RecordStage::Handed),
                &Cap::random().unwrap(),
            )
            .unwrap();
        let fifo = rec.join(key("m-fifo", 1, RecordStage::Handed).file_name());
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path and a mode.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let outside = state.path().join("outside-record");
        let body = RecordBody {
            msg: "m-link".into(),
            a: 1,
            e: 1,
            stage: RecordStage::Handed,
            cap: Cap::random().unwrap().hex(),
        };
        std::fs::write(&outside, serde_json::to_vec(&body).unwrap()).unwrap();
        let link = rec.join(key("m-link", 1, RecordStage::Handed).file_name());
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let dir = rec.join(key("m-dir", 1, RecordStage::Handed).file_name());
        std::fs::create_dir(&dir).unwrap();

        let mut scanner = generation.scanner().unwrap();
        let (records, passes) = scan_cycle(&mut scanner, 4);
        assert!(passes.last().unwrap().complete);
        let msgs: Vec<&str> = records.iter().map(|r| r.msg.as_str()).collect();
        assert_eq!(msgs, vec!["m-good"]);
        assert!(std::fs::symlink_metadata(&fifo).is_ok());
        assert!(std::fs::symlink_metadata(&link).is_ok());
        assert!(dir.is_dir());
        assert!(outside.is_file());
    }

    /// Scenario: Create a record temp and inspect it before writing anything.
    /// The empty temp is already owner-only: mode 0600 on Unix, owned by the
    /// current user on Windows, so no record byte is ever written under a
    /// wider permission.
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
            crate::platform::fsperm::verify_file_owner_is_current_user(
                reopened.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE
            )
            .unwrap();
        }
    }

    /// Scenario: Swap a generation's `rec` directory, then a pane directory,
    /// then the spool root itself for a symlink to a decoy directory, and open
    /// or publish through each. Every open is refused, a handle anchored
    /// before the swap does not write into the decoy, and every decoy stays
    /// empty.
    #[cfg(unix)]
    #[spec("inbox/receipt/005")]
    #[test]
    fn receipt_005_an_intermediate_symlink_is_refused_not_followed() {
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let generation = spool.generation("2", "agent-a").unwrap();
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
        let _ = generation.publish(&key("m-1", 1, RecordStage::Handed), &Cap::random().unwrap());
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
    }

    /// Scenario: Pre-create the spool root world-writable, then
    /// group-readable, then owner-only, starting a spool each time; then make
    /// one pane directory group-readable. The unsafe root and pane are
    /// refused and left as they were, never repaired; the owner-only root is
    /// used.
    #[cfg(unix)]
    #[spec("inbox/receipt/006")]
    #[test]
    fn receipt_006_an_unsafe_pre_existing_root_is_refused_not_repaired() {
        use std::os::unix::fs::PermissionsExt as _;
        let state = crate::test_temp::tempdir().unwrap();
        let root = state.path().join(SPOOL_DIR_NAME);
        std::fs::create_dir(&root).unwrap();
        for mode in [0o777, 0o750] {
            chmod(&root, mode);
            let err = Spool::start(state.path()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
            let now = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
            assert_eq!(now, mode, "the root is refused, not repaired");
        }
        chmod(&root, 0o700);
        let spool = Spool::start(state.path()).unwrap();
        spool.generation("2", "agent-a").unwrap();
        chmod(&spool.instance_path().join("2"), 0o755);
        let err = spool.generation("2", "agent-a").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
    }

    /// Scenario: Fill a generation's record directory with 3,000 entries the
    /// spool does not recognise plus one genuine record, then scan pass by
    /// pass while asking about the record and about one that was never
    /// published. No pass examines more than the entry bound, the missing
    /// record reads unknown, never absent, until a pass completes, and then
    /// it reads absent while the genuine one reads found.
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
        let real = key("m-real", 1, RecordStage::Committed);
        let missing = key("m-missing", 1, RecordStage::Committed);
        generation.publish(&real, &Cap::random().unwrap()).unwrap();
        let mut scanner = generation.scanner().unwrap();
        let first = scanner.pass().unwrap();
        assert!(!first.complete, "3,001 entries take more than one pass");
        assert!(first.examined <= SCAN_MAX_ENTRIES);
        assert_eq!(scanner.presence(&missing), Presence::Unknown);
        let (_, rest) = scan_cycle(&mut scanner, 8);
        assert!(rest.iter().all(|p| p.examined <= SCAN_MAX_ENTRIES));
        assert!(rest.last().unwrap().complete);
        assert_eq!(scanner.presence(&missing), Presence::Absent);
        assert_eq!(scanner.presence(&real), Presence::Found);
    }

    /// Scenario: Put 300 large record-named files, an oversized one, one whose
    /// body names another message, a fresh temp, and (in a second generation)
    /// 300 temps more than a minute old into spools, then scan through
    /// several complete cycles. The byte bound splits the first cycle, the
    /// oversized and mismatched files are removed and never returned, the
    /// fresh temp stays, and aged temps go only when a later pass sees them
    /// again, at most 256 per pass.
    #[spec("inbox/receipt/008")]
    #[test]
    fn receipt_008_record_bytes_temps_and_mismatches_are_bounded() {
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let generation = spool.generation("2", "agent-a").unwrap();
        let rec = rec_dir(&spool, "2", "agent-a");
        for i in 0..300 {
            let body = RecordBody {
                msg: format!("m-big{i}"),
                a: 1,
                e: 1,
                stage: RecordStage::Handed,
                cap: "f".repeat(3900),
            };
            std::fs::write(
                rec.join(body.key().file_name()),
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap();
        }
        let oversized = rec.join(key("m-over", 1, RecordStage::Handed).file_name());
        std::fs::write(&oversized, vec![b' '; 5000]).unwrap();
        let mismatch = rec.join(key("m-x", 1, RecordStage::Handed).file_name());
        let other = RecordBody {
            msg: "m-y".into(),
            a: 1,
            e: 1,
            stage: RecordStage::Handed,
            cap: "00".into(),
        };
        std::fs::write(&mismatch, serde_json::to_vec(&other).unwrap()).unwrap();
        let fresh_temp = rec.join(format!("{TEMP_PREFIX}fresh"));
        std::fs::write(&fresh_temp, b"partial").unwrap();

        let mut scanner = generation.scanner().unwrap();
        let (records, passes) = scan_cycle(&mut scanner, 8);
        assert!(
            passes.len() >= 2,
            "1.2 MB of records takes more than one pass"
        );
        assert!(
            passes[0].examined < SCAN_MAX_ENTRIES,
            "split by bytes, not entries"
        );
        assert_eq!(records.len(), 300);
        assert!(records.iter().all(|r| r.msg.starts_with("m-big")));
        assert!(!oversized.exists());
        assert!(!mismatch.exists());
        let _ = scan_cycle(&mut scanner, 8);
        assert!(
            fresh_temp.exists(),
            "a fresh temp may be a publish in flight"
        );

        let aged_gen = spool.generation("3", "agent-b").unwrap();
        let aged_rec = rec_dir(&spool, "3", "agent-b");
        let old = std::time::SystemTime::now() - Duration::from_secs(120);
        for i in 0..300 {
            let f = File::create(aged_rec.join(format!("{TEMP_PREFIX}old{i}"))).unwrap();
            f.set_modified(old).unwrap();
        }
        let mut scanner = aged_gen.scanner().unwrap();
        let (_, first) = scan_cycle(&mut scanner, 4);
        assert_eq!(
            first.iter().map(|p| p.removed).sum::<usize>(),
            0,
            "first sighting"
        );
        let second = scanner.pass().unwrap();
        assert_eq!(second.removed, SCAN_MAX_TEMPS);
        let (_, third) = scan_cycle(&mut scanner, 4);
        let left: usize = third.iter().map(|p| p.removed).sum();
        assert_eq!(left, 300 - SCAN_MAX_TEMPS);
        assert_eq!(std::fs::read_dir(&aged_rec).unwrap().count(), 0);
    }

    /// Scenario: Mint caps for two attempts, write the first's caps file,
    /// publish its handed and committed records (and the handed one twice),
    /// scan and admit what was read, then offer forged records: a commit on
    /// the hand-out cap, an attempt never issued, and a random cap; mark the
    /// second attempt revoked. Each attempt's caps are found by its exact
    /// name, the duplicate publish is refused, genuine records are admitted
    /// once, every forgery is refused, the revoke marker names only its
    /// attempt, and no record is pruned.
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
        let (records, _) = scan_cycle(&mut scanner, 4);
        assert_eq!(records.len(), 2);
        for body in &records {
            assert_eq!(store.admit(body), Admission::Admitted);
            assert_eq!(store.admit(body), Admission::Duplicate);
        }
        assert!(store.admitted(&committed));

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

        let _ = scan_cycle(&mut scanner, 4);
        let rec = rec_dir(&spool, "2", "agent-a");
        let mut names: Vec<String> = std::fs::read_dir(&rec)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec![committed.file_name(), handed.file_name()]);
    }
}
