//! PRD #1666 part 3 (Q35-13; plan §1.2, R3 §4.1 and §8, R4 §7.1, R5 §3.5 and
//! §10.3): the daemon's instance directory, the files in it, and their
//! readers. Linux only, like the rest of the observer until macOS's M12.
//!
//! **The layout.** `<root>/<pid>-<start ticks>/`, 0700, made fresh by one
//! daemon and never reused ([`Instance::create`]), where `<root>` is
//! `<state_dir>/editor` or, when that path holds a character an agent can't
//! split, `$XDG_RUNTIME_DIR/dot-agent-deck/editor` ([`choose_root`]):
//! - `daemon`, 0400: the daemon's pid, start ticks, boot id and a random
//!   nonce, written once ([`DaemonId`]);
//! - `dot-agent-deck-editor`, 0500: the private copy of the daemon's own
//!   executable that agents run as their editor (D6);
//! - `inv/`, 0700, and per generation `G`: `inv/G/`, 0700; `arm-G`, one
//!   page, 0600, which the daemon maps shared ([`ArmPage`]);
//!   `env-G`, 0600, the `CLAUDE_ENV_FILE` of a Claude-style generation;
//!   `kind-G`, 0400, the generation's agent once it is known
//!   ([`InstanceReader::kind`]); `gen-G`, 0400, its identity, written only
//!   once the deck may arm it ([`Instance::write_identity`]).
//!
//! Every file is created through the instance directory's descriptor,
//! exclusively and without following a link, so nothing already there is
//! reused or written through. A file that exists is an error, never
//! overwritten.
//!
//! **Retention** (R3 §8). Creating an instance removes another daemon's
//! directory only when that daemon is gone (its pid absent or started at
//! another time) **and** a scan of every process of this user, through its
//! `/proc/<pid>/environ` and `/proc/<pid>/maps`, found none naming the
//! directory, **and** every entry the scan reached was readable. Anything
//! less keeps it: a process of this user whose entries can't be read keeps
//! every directory, so on a host with such a process nothing is removed
//! ([`removable`]). Files stay after their generation retires; the daemon
//! only unmaps the arm page.

use std::ffi::{CStr, CString, OsString};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use super::arm::Publisher;
use super::classifier::{FileId, Identity};
use super::install::{ProbeAgent, SetupError};

/// The helper copy's name, which the binary dispatches on (Q35-12).
pub const HELPER_NAME: &str = "dot-agent-deck-editor";

/// The daemon's identity file.
pub const DAEMON_FILE: &str = "daemon";

/// The directory holding each generation's invocation directory.
pub const INV_DIR: &str = "inv";

/// An arm page file's length: one page.
pub const PAGE_BYTES: usize = 4096;

/// The helper copy's temporary name, before it is renamed into place.
const PART_NAME: &str = ".dot-agent-deck-editor.part";

/// The longest generation name the instance files carry.
const GENERATION_MAX: usize = 64;

/// The largest instance file a reader reads: room for an identity whose
/// install root is as long as a path can be, hex-encoded.
pub const READ_MAX: u64 = 16 << 10;

/// Whether `path` holds only `[A-Za-z0-9._/-]`: the characters an agent's
/// split of `VISUAL` on spaces keeps whole (R3 §4.6).
pub fn splittable(path: &Path) -> bool {
    path.as_os_str()
        .as_bytes()
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || b".-_/".contains(b))
}

/// The first of `<state_dir>/editor` and `<runtime_dir>/dot-agent-deck/editor`
/// whose path is [`splittable`], both taken as absolute.
pub fn choose_root(state_dir: &Path, runtime_dir: Option<&Path>) -> Result<PathBuf, SetupError> {
    let state = state_dir.join("editor");
    let runtime = runtime_dir.map(|r| r.join("dot-agent-deck").join("editor"));
    std::iter::once(state)
        .chain(runtime)
        .find(|root| root.is_absolute() && splittable(root))
        .ok_or(SetupError::PathCharacters)
}

/// Whether `name` can name a generation's files: 1–64 of `[A-Za-z0-9_-]`.
pub fn generation_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= GENERATION_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// An instance directory's name for its daemon.
pub fn instance_name(pid: u32, start: u64) -> String {
    format!("{pid}-{start}")
}

/// The daemon's pid and start ticks from an instance directory's name.
pub fn parse_instance_name(name: &str) -> Option<(u32, u64)> {
    let (pid, start) = name.split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(pid) || !digits(start) {
        return None;
    }
    Some((pid.parse().ok()?, start.parse().ok()?))
}

/// What `<instance>/daemon` records: the daemon that made the instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonId {
    pub pid: u32,
    /// Start ticks since boot (`/proc/<pid>/stat` field 22).
    pub start: u64,
    /// `/proc/sys/kernel/random/boot_id`.
    pub boot: String,
    /// 16 random bytes, hex.
    pub nonce: String,
}

impl DaemonId {
    /// This process's identity, with a fresh nonce.
    pub fn this_process() -> Result<DaemonId, SetupError> {
        let pid = std::process::id();
        let start = crate::platform::proc::process_start_of(pid).ok_or(SetupError::Io(None))?;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_string();
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|e| SetupError::Io(e.raw_os_error()))?;
        Ok(DaemonId {
            pid,
            start,
            boot,
            nonce: hex(&nonce),
        })
    }

    fn encode(&self) -> String {
        format!(
            "pid={}\nstart={}\nboot={}\nnonce={}\n",
            self.pid, self.start, self.boot, self.nonce
        )
    }

    /// The file's contents back, or `None` for anything but exactly the four
    /// lines [`DaemonId::encode`] writes.
    pub fn decode(bytes: &[u8]) -> Option<DaemonId> {
        let [pid, start, boot, nonce] = fields(bytes, ["pid", "start", "boot", "nonce"])?;
        let id_text = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
        };
        if !id_text(boot) || !id_text(nonce) || nonce.contains('-') {
            return None;
        }
        Some(DaemonId {
            pid: number(pid)?,
            start: number(start)?,
            boot: boot.to_string(),
            nonce: nonce.to_string(),
        })
    }
}

/// `gen-G`'s contents for `identity`.
pub fn encode_identity(identity: &Identity) -> String {
    format!(
        "pid={}\nstart={}\nexe={}:{}\nroot_id={}:{}\nroot={}\nbasename={}\n",
        identity.pid,
        identity.start,
        identity.exe.dev,
        identity.exe.ino,
        identity.install_root_id.dev,
        identity.install_root_id.ino,
        hex(identity.install_root.as_os_str().as_bytes()),
        hex(identity.basename.as_bytes()),
    )
}

/// `gen-G`'s contents back, or `None` for anything else: every field once,
/// in order, an absolute root and a basename that isn't empty. A reader that
/// gets `None` has no identity, which the classifier reads as `Unknown`.
pub fn decode_identity(bytes: &[u8]) -> Option<Identity> {
    let [pid, start, exe, root_id, root, basename] = fields(
        bytes,
        ["pid", "start", "exe", "root_id", "root", "basename"],
    )?;
    let file_id = |s: &str| {
        let (dev, ino) = s.split_once(':')?;
        Some(FileId {
            dev: number(dev)?,
            ino: number(ino)?,
        })
    };
    let install_root = PathBuf::from(OsString::from_vec(unhex(root)?));
    let basename = OsString::from_vec(unhex(basename)?);
    if !install_root.is_absolute() || basename.is_empty() {
        return None;
    }
    Some(Identity {
        pid: number(pid)?,
        start: number(start)?,
        exe: file_id(exe)?,
        install_root,
        install_root_id: file_id(root_id)?,
        basename,
    })
}

/// A kind's contents: the agent's name and a newline.
fn encode_kind(agent: ProbeAgent) -> &'static str {
    match agent {
        ProbeAgent::Claude => "claude\n",
        ProbeAgent::Codex => "codex\n",
    }
}

/// What the helper takes a generation to be (R5 §10.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The agent is known.
    Agent(ProbeAgent),
    /// No `kind-G` yet: an undeclared wrapper whose agent isn't learned.
    Learned,
}

/// The instance directory's name for a generation's file.
fn generation_file(prefix: &str, generation: &str) -> Result<CString, SetupError> {
    if !generation_name_ok(generation) {
        return Err(SetupError::GenerationName);
    }
    Ok(cstring(&format!("{prefix}-{generation}")))
}

/// A name of the instance's own, which holds no NUL.
fn cstring(name: &str) -> CString {
    CString::new(name).expect("instance file names hold no NUL")
}

/// A daemon's instance directory, made by [`Instance::create`].
#[derive(Debug)]
pub struct Instance {
    dir: PathBuf,
    dir_fd: OwnedFd,
    helper: PathBuf,
    helper_id: FileId,
    daemon: DaemonId,
    swept: Sweep,
}

impl Instance {
    /// Make `daemon`'s instance directory under `root`, after removing what
    /// retention allows of other daemons' ([`sweep`]): the directories down
    /// to `root` (made 0700 where missing, and each this user's and not
    /// writable by group or others), the instance directory itself (which
    /// mustn't exist), `daemon`, `inv/`, and the helper, copied from
    /// `source` and checked against it byte for byte before it is renamed
    /// into place without replacing anything. Any failure leaves what it made
    /// for retention to remove once this daemon is gone.
    pub fn create(
        root: &Path,
        daemon: DaemonId,
        source: &mut File,
    ) -> Result<Instance, SetupError> {
        if !root.is_absolute() || !splittable(root) {
            return Err(SetupError::PathCharacters);
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
        if let Some(parent) = root.parent() {
            trusted_dir(&open_dir_path(parent)?)?;
        }
        let root_fd = open_dir_path(root)?;
        trusted_dir(&root_fd)?;
        let name = instance_name(daemon.pid, daemon.start);
        let swept = sweep(root, &name);
        let name_c = cstring(&name);
        mkdir_at(root_fd.as_fd(), &name_c, 0o700).map_err(|e| match e.raw_os_error() {
            Some(libc::EEXIST) => SetupError::InstanceExists,
            _ => e.into(),
        })?;
        let dir_fd = open_dir_at(root_fd.as_fd(), &name_c)?;
        trusted_dir(&dir_fd)?;
        let dir = root.join(&name);
        create_file_at(dir_fd.as_fd(), &cstring(DAEMON_FILE), 0o400)?
            .write_all(daemon.encode().as_bytes())?;
        mkdir_at(dir_fd.as_fd(), &cstring(INV_DIR), 0o700)?;
        let helper_id = copy_helper(dir_fd.as_fd(), source)?;
        Ok(Instance {
            helper: dir.join(HELPER_NAME),
            dir,
            dir_fd,
            helper_id,
            daemon,
            swept,
        })
    }

    /// The instance directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The helper copy's path, which goes in an installed agent's `VISUAL`.
    pub fn helper(&self) -> &Path {
        &self.helper
    }

    /// The helper copy's `(dev, ino)`.
    pub fn helper_id(&self) -> FileId {
        self.helper_id
    }

    /// The daemon this instance belongs to.
    pub fn daemon(&self) -> &DaemonId {
        &self.daemon
    }

    /// What the sweep at creation did.
    pub fn swept(&self) -> &Sweep {
        &self.swept
    }

    /// Make generation `generation`'s files: `inv/G/`, the arm page, mapped,
    /// `kind-G` when `kind` is known, and `env-G` holding `env_file` when
    /// there is one. Every one is new; one that exists already refuses.
    pub fn generation(
        &self,
        generation: &str,
        kind: Option<ProbeAgent>,
        env_file: Option<&[u8]>,
    ) -> Result<GenerationFiles, SetupError> {
        let arm_name = generation_file("arm", generation)?;
        let inv = open_dir_at(self.dir_fd.as_fd(), &cstring(INV_DIR))?;
        mkdir_at(inv.as_fd(), &cstring(generation), 0o700).map_err(exists_is_generation)?;
        let arm = ArmPage::create(self.dir_fd.as_fd(), &arm_name)?;
        if let Some(agent) = kind {
            self.write_kind(generation, agent)?;
        }
        let env_file = match env_file {
            Some(contents) => {
                let name = format!("env-{generation}");
                create_file_at(self.dir_fd.as_fd(), &cstring(&name), 0o600)
                    .map_err(exists_is_generation)?
                    .write_all(contents)?;
                Some(self.dir.join(name))
            }
            None => None,
        };
        Ok(GenerationFiles { arm, env_file })
    }

    /// Write `kind-G` once, for a generation whose agent is known.
    pub fn write_kind(&self, generation: &str, agent: ProbeAgent) -> Result<(), SetupError> {
        let name = generation_file("kind", generation)?;
        create_file_at(self.dir_fd.as_fd(), &name, 0o400)
            .map_err(exists_is_generation)?
            .write_all(encode_kind(agent).as_bytes())?;
        Ok(())
    }

    /// Write `gen-G` once. The identity must name its install root (F2) as an
    /// absolute path that, read now, is a directory with the recorded
    /// `(dev, ino)`; otherwise nothing is written (R35CODE4-U2-02).
    pub fn write_identity(&self, generation: &str, identity: &Identity) -> Result<(), SetupError> {
        let name = generation_file("gen", generation)?;
        if decode_identity(encode_identity(identity).as_bytes()).as_ref() != Some(identity) {
            return Err(SetupError::InstallRoot);
        }
        let root =
            std::fs::metadata(&identity.install_root).map_err(|_| SetupError::InstallRoot)?;
        if !root.is_dir() || file_id(&root) != identity.install_root_id {
            return Err(SetupError::InstallRoot);
        }
        create_file_at(self.dir_fd.as_fd(), &name, 0o400)
            .map_err(exists_is_generation)?
            .write_all(encode_identity(identity).as_bytes())?;
        Ok(())
    }
}

/// An `EEXIST` as [`SetupError::GenerationExists`].
fn exists_is_generation(error: io::Error) -> SetupError {
    match error.raw_os_error() {
        Some(libc::EEXIST) => SetupError::GenerationExists,
        _ => error.into(),
    }
}

/// A generation's files, held while it lives. Dropping it unmaps the arm
/// page; the files stay.
#[derive(Debug)]
pub struct GenerationFiles {
    arm: ArmPage,
    env_file: Option<PathBuf>,
}

impl GenerationFiles {
    /// The mapped arm page.
    pub fn arm(&mut self) -> &mut ArmPage {
        &mut self.arm
    }

    /// `env-G`, for a Claude-style generation.
    pub fn env_file(&self) -> Option<&Path> {
        self.env_file.as_deref()
    }
}

/// An arm page file mapped shared and writable: the daemon's side of the
/// page the helper maps read-only ([`super::arm`]).
#[derive(Debug)]
pub struct ArmPage {
    ptr: NonNull<u8>,
    _file: File,
}

// SAFETY: the mapping is this value's alone, and its memory is reached only
// through [`ArmPage::publisher`], which takes `&mut self` and accesses the
// words atomically; a shared `&ArmPage` reads nothing through the pointer.
unsafe impl Send for ArmPage {}
// SAFETY: as above.
unsafe impl Sync for ArmPage {}

impl ArmPage {
    /// Create `name` in `dir`, one zeroed page long, 0600, and map it with its
    /// pages populated; locking it in memory is tried and its failure
    /// tolerated (R5 §3.5).
    fn create(dir: BorrowedFd<'_>, name: &CStr) -> Result<ArmPage, SetupError> {
        let file = create_file_at(dir, name, 0o600).map_err(exists_is_generation)?;
        file.set_len(PAGE_BYTES as u64)?;
        // SAFETY: a fresh shared mapping of an open file `PAGE_BYTES` long;
        // the result is checked before use.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PAGE_BYTES,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: the mapping made above; a failure costs only paging.
        unsafe { libc::mlock(ptr, PAGE_BYTES) };
        Ok(ArmPage {
            ptr: NonNull::new(ptr.cast()).ok_or(SetupError::Io(None))?,
            _file: file,
        })
    }

    /// The page's only publisher.
    pub fn publisher(&mut self) -> Publisher<'_> {
        // SAFETY: the mapping is page-aligned and `PAGE_BYTES` long, lives as
        // long as `self`, which the publisher borrows mutably, so it is the
        // only writer; the helper only loads the words atomically.
        unsafe { Publisher::at(self.ptr.as_ptr(), PAGE_BYTES) }
            .expect("a page-aligned mapping a page long is a page")
    }
}

impl Drop for ArmPage {
    fn drop(&mut self) {
        // SAFETY: the mapping `create` made, unmapped once.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), PAGE_BYTES) };
    }
}

/// The instance files as a reader sees them: the helper (Q35-12) and the
/// daemon's attribution (Q35-14a). Every file is opened through the
/// directory's descriptor without following a link, and anything not exactly
/// in its format reads as absent.
#[derive(Debug)]
pub struct InstanceReader {
    dir_fd: OwnedFd,
}

impl InstanceReader {
    /// The instance directory at `dir`, which mustn't be a link.
    pub fn open(dir: &Path) -> Result<InstanceReader, SetupError> {
        Ok(InstanceReader {
            dir_fd: open_dir_path(dir)?,
        })
    }

    /// `daemon`.
    pub fn daemon(&self) -> Option<DaemonId> {
        DaemonId::decode(&self.read(&cstring(DAEMON_FILE)).ok()??)
    }

    /// `gen-G`: `None` when it is absent, unreadable or malformed.
    pub fn identity(&self, generation: &str) -> Option<Identity> {
        let name = generation_file("gen", generation).ok()?;
        decode_identity(&self.read(&name).ok()??)
    }

    /// `kind-G`: [`Kind::Learned`] when it is absent, `None` when it can't
    /// be read or holds anything but a kind.
    pub fn kind(&self, generation: &str) -> Option<Kind> {
        let name = generation_file("kind", generation).ok()?;
        match self.read(&name).ok()? {
            None => Some(Kind::Learned),
            Some(bytes) => [ProbeAgent::Claude, ProbeAgent::Codex]
                .into_iter()
                .find(|agent| encode_kind(*agent).as_bytes() == bytes)
                .map(Kind::Agent),
        }
    }

    /// A file's contents, `None` when it doesn't exist; an error for one
    /// that is a link, not a regular file, or longer than [`READ_MAX`].
    fn read(&self, name: &CStr) -> io::Result<Option<Vec<u8>>> {
        let fd = match openat(
            self.dir_fd.as_fd(),
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW,
            0,
        ) {
            Ok(fd) => fd,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(e) => return Err(e),
        };
        let file = File::from(fd);
        if !file.metadata()?.is_file() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let mut bytes = Vec::new();
        file.take(READ_MAX + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > READ_MAX {
            return Err(io::Error::from_raw_os_error(libc::EFBIG));
        }
        Ok(Some(bytes))
    }
}

/// What a sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sweep {
    /// The directories removed.
    pub removed: Vec<String>,
    /// Entries kept: a live daemon's, one a process names, one whose name
    /// isn't an instance's, or every one when the scan failed.
    pub kept: usize,
    /// Why the scan kept everything, if it did.
    pub scan_error: Option<ScanError>,
    /// Removals that failed.
    pub failed: usize,
}

/// Why a scan of this user's processes can't be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanError {
    /// An entry of a process of this user couldn't be read.
    Unreadable,
}

/// Which of `candidates` (each an instance directory's name and its path's
/// bytes) retention removes: those whose name is an instance's, whose
/// daemon isn't `alive`, and that no process in `scan` names. `scan` calls
/// its visitor with each process's environment and maps; its error keeps
/// every candidate. `scan` isn't called when no daemon is gone.
pub fn removable(
    candidates: &[(String, Vec<u8>)],
    alive: impl Fn(u32, u64) -> bool,
    scan: impl FnOnce(&mut dyn FnMut(&[u8])) -> Result<(), ScanError>,
) -> Result<Vec<String>, ScanError> {
    let mut gone: Vec<(&str, &[u8], bool)> = candidates
        .iter()
        .filter(|(name, _)| {
            parse_instance_name(name).is_some_and(|(pid, start)| !alive(pid, start))
        })
        .map(|(name, path)| (name.as_str(), path.as_slice(), false))
        .collect();
    if gone.is_empty() {
        return Ok(Vec::new());
    }
    scan(&mut |bytes| {
        for (_, path, named) in gone.iter_mut() {
            *named |= contains(bytes, path);
        }
    })?;
    Ok(gone
        .into_iter()
        .filter(|(_, _, named)| !named)
        .map(|(name, _, _)| name.to_string())
        .collect())
}

/// Whether `haystack` holds `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Remove what [`removable`] allows under `root`, keeping `own`, with this
/// host's processes as the scan ([`scan_proc`]).
pub fn sweep(root: &Path, own: &str) -> Sweep {
    let alive = |pid, start| crate::platform::proc::process_start_of(pid) == Some(start);
    sweep_with(root, own, alive, scan_proc)
}

/// [`sweep`], with liveness and the scan given.
pub fn sweep_with(
    root: &Path,
    own: &str,
    alive: impl Fn(u32, u64) -> bool,
    scan: impl FnOnce(&mut dyn FnMut(&[u8])) -> Result<(), ScanError>,
) -> Sweep {
    let mut sweep = Sweep::default();
    let Ok(entries) = std::fs::read_dir(root) else {
        return sweep;
    };
    let candidates: Vec<(String, Vec<u8>)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| name != own)
        .map(|name| {
            let path = root.join(&name).into_os_string().into_vec();
            (name, path)
        })
        .collect();
    match removable(&candidates, alive, scan) {
        Ok(names) => {
            sweep.kept = candidates.len() - names.len();
            for name in names {
                // `remove_dir_all` works beneath the directory's descriptor
                // and never follows a link out of it.
                match std::fs::remove_dir_all(root.join(&name)) {
                    Ok(()) => sweep.removed.push(name),
                    Err(_) => sweep.failed += 1,
                }
            }
        }
        Err(error) => {
            sweep.kept = candidates.len();
            sweep.scan_error = Some(error);
        }
    }
    sweep
}

/// Visit the environment and maps of every process of this user. A process
/// that is gone by the time it is read is skipped; any other failure to read
/// one is [`ScanError::Unreadable`]. Ownership is the real uid in
/// `/proc/<pid>/status`, not the owner of `/proc/<pid>`, which reads as root
/// for a process that made itself non-dumpable.
pub fn scan_proc(visit: &mut dyn FnMut(&[u8])) -> Result<(), ScanError> {
    // SAFETY: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    let entries = std::fs::read_dir("/proc").map_err(|_| ScanError::Unreadable)?;
    for entry in entries {
        let entry = entry.map_err(|_| ScanError::Unreadable)?;
        let name = entry.file_name();
        if !name.as_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        let dir = entry.path();
        let status = match proc_read(&dir.join("status"))? {
            Some(status) => status,
            None => continue,
        };
        if status_uid(&status) != Some(uid) {
            continue;
        }
        for file in ["environ", "maps"] {
            if let Some(bytes) = proc_read(&dir.join(file))? {
                visit(&bytes);
            }
        }
    }
    Ok(())
}

/// A `/proc` file, `None` when its process is gone.
fn proc_read(path: &Path) -> Result<Option<Vec<u8>>, ScanError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => Ok(None),
        Err(_) => Err(ScanError::Unreadable),
    }
}

/// The real uid on a `status` file's `Uid:` line.
fn status_uid(status: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(status).ok()?;
    let line = text.lines().find_map(|l| l.strip_prefix("Uid:"))?;
    line.split_whitespace().next()?.parse().ok()
}

/// Copy `source` to the helper's temporary name, check the copy reads as
/// `source` byte for byte, make it 0500 and rename it into place without
/// replacing anything. The copy's `(dev, ino)`.
fn copy_helper(dir: BorrowedFd<'_>, source: &mut File) -> Result<FileId, SetupError> {
    let mut part = create_file_at(dir, &cstring(PART_NAME), 0o600)?;
    source.seek(SeekFrom::Start(0))?;
    io::copy(source, &mut part)?;
    if !same_contents(source, &mut part)? {
        return Err(SetupError::CopyMismatch);
    }
    fchmod(&part, 0o500)?;
    rename_noreplace(dir, &cstring(PART_NAME), &cstring(HELPER_NAME))?;
    Ok(file_id(&part.metadata()?))
}

/// Whether `a` and `b` hold the same bytes, read from the start.
pub fn same_contents(a: &mut File, b: &mut File) -> io::Result<bool> {
    a.seek(SeekFrom::Start(0))?;
    b.seek(SeekFrom::Start(0))?;
    let (mut x, mut y) = (vec![0; 1 << 20], vec![0; 1 << 20]);
    loop {
        let n = read_full(a, &mut x)?;
        if n != read_full(b, &mut y)? || x[..n] != y[..n] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

/// Read until `buf` is full or the file ends.
fn read_full(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..])? {
            0 => break,
            m => n += m,
        }
    }
    Ok(n)
}

fn file_id(metadata: &std::fs::Metadata) -> FileId {
    use std::os::unix::fs::MetadataExt;
    FileId {
        dev: metadata.dev(),
        ino: metadata.ino(),
    }
}

/// A directory this user owns that its group and others can't write.
fn trusted_dir(fd: &OwnedFd) -> Result<(), SetupError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = File::from(fd.try_clone()?).metadata()?;
    // SAFETY: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        return Err(SetupError::UntrustedDirectory);
    }
    Ok(())
}

/// The directory at `path`, its last component not followed.
fn open_dir_path(path: &Path) -> Result<OwnedFd, SetupError> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| SetupError::Io(None))?;
    // SAFETY: `AT_FDCWD` is a valid directory argument for `openat`.
    let cwd = unsafe { BorrowedFd::borrow_raw(libc::AT_FDCWD) };
    open_dir_at(cwd, &path)
}

/// `name` in `dir`, a directory, not followed.
fn open_dir_at(dir: BorrowedFd<'_>, name: &CStr) -> Result<OwnedFd, SetupError> {
    openat(
        dir,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )
    .map_err(|e| match e.raw_os_error() {
        Some(libc::ELOOP | libc::ENOTDIR) => SetupError::UntrustedDirectory,
        _ => e.into(),
    })
}

/// A new file `name` in `dir`, read-write, exactly `mode`.
fn create_file_at(dir: BorrowedFd<'_>, name: &CStr, mode: u32) -> io::Result<File> {
    let fd = openat(
        dir,
        name,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
        mode,
    )?;
    let file = File::from(fd);
    fchmod(&file, mode)?;
    Ok(file)
}

/// A new directory `name` in `dir`, exactly `mode`.
fn mkdir_at(dir: BorrowedFd<'_>, name: &CStr, mode: u32) -> io::Result<()> {
    // SAFETY: a valid descriptor and NUL-terminated name.
    if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = openat(
        dir,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )?;
    fchmod(&File::from(fd), mode)
}

fn openat(dir: BorrowedFd<'_>, name: &CStr, flags: i32, mode: u32) -> io::Result<OwnedFd> {
    // SAFETY: a valid descriptor and NUL-terminated name; the result is
    // checked before it is owned.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor `openat` just returned, owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fchmod(file: &File, mode: u32) -> io::Result<()> {
    // SAFETY: a valid descriptor.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn rename_noreplace(dir: BorrowedFd<'_>, from: &CStr, to: &CStr) -> io::Result<()> {
    // SAFETY: valid descriptors and NUL-terminated names. Through `syscall`
    // so every libc this builds against has it.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            dir.as_raw_fd(),
            from.as_ptr(),
            dir.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The values of `bytes`' lines `key=value`, exactly `keys` in order and
/// nothing else, each line ending in a newline.
fn fields<'a, const N: usize>(bytes: &'a [u8], keys: [&str; N]) -> Option<[&'a str; N]> {
    let text = std::str::from_utf8(bytes).ok()?.strip_suffix('\n')?;
    let mut lines = text.split('\n');
    let mut values = [""; N];
    for (value, key) in values.iter_mut().zip(keys) {
        *value = lines.next()?.strip_prefix(key)?.strip_prefix('=')?;
    }
    lines.next().is_none().then_some(values)
}

/// A decimal number with nothing else around it.
fn number<T: std::str::FromStr>(s: &str) -> Option<T> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            let pair = s.get(i..i + 2)?;
            pair.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                .then(|| u8::from_str_radix(pair, 16).ok())?
        })
        .collect()
}
