//! PRD #1666 part 3 (Q35-14a; core §2.3, R3 §4.1, R6 §6.2, revalidation
//! from R35CODE4-U3-01 and Q3504-A-U3-02): the daemon's own attribution of
//! an editor invocation, and its read of the agent's export. Linux only,
//! like the helper it attributes.
//!
//! **Identity, refused rather than invented** ([`pin`]). A process is named
//! by a pidfd and then by the start ticks read after it, and the pidfd is
//! polled after that read. A pidfd that doesn't open, a stat that doesn't
//! read, start ticks other than the ones the sighting read, or a process
//! that exited before the poll make no [`Pinned`] process: the caller
//! records the identity refusal and makes no record
//! (`Records::refused_identity`).
//!
//! **An IPC peer is named by `SO_PEERPIDFD` or not at all** ([`peer_pidfd`],
//! [`Sighting::Peer`]; Q81). The kernel takes that pidfd for the process
//! that connected, so a peer that exited since reads as exited. A pid from
//! `SO_PEERCRED`, opened later, can name another process: the peer exits,
//! its pid is reused, the new process can pass every check, and nothing the
//! daemon reads binds it back to the connection. So where the socket
//! gives no peer pidfd (Linux before 6.5, such as Ubuntu 22.04's 5.15), an
//! IPC request is an identity refusal (`record_refused`, its claim kept)
//! and the editor-probe IPC path gives no authority. The pidfd and start
//! pairing still pins a process the daemon observed itself: a scan's or a
//! marker's pid with the start ticks read with it, or the helper's parent.
//!
//! **The attribution, computed once per record while the helper is alive**
//! ([`attribute`]), from the daemon's own reads, never from the helper's
//! claim:
//! 1. Take the helper's parent `Q` from the stat read when it was pinned,
//!    and pin `Q`. A `Q` that can't be pinned died, so the helper is no
//!    longer its composer's: `NotOwned(Unknown)` (`Unattributable` if the
//!    helper exited too).
//! 2. Classify `Q` against the generation's identity ([`super::classifier`]).
//!    `PositivelyTool` is `NotOwned(Tool)`, `Unknown` is `NotOwned(Unknown)`.
//!    **Only `A` itself is `Owned`,** so an attestation of a Claude
//!    invocation attests `A` and no process of the overview chain.
//! 3. Read the helper's argv from `/proc/<pid>/cmdline`: its last argument
//!    must have the agent's export shape (the helper's own rule,
//!    [`super::helper::export_shaped`]), and an IPC request's argv must equal
//!    it, else `NotOwned(Shape)`.
//! 4. Read the helper's and `A`'s time namespaces and the helper's stat,
//!    then poll the pins, so these last reads are the pinned processes'. A
//!    helper that exited before the poll, or a helper or `A` whose time
//!    namespace isn't the daemon's, is `Unattributable` (the reads may
//!    describe another process, or a birth floor can't be compared); a
//!    helper whose stat changed (it was reparented away from `Q`), or a `Q`
//!    that exited, is `NotOwned(Unknown)`. `A` is `Q` when `Q` is `A`, and
//!    is otherwise pinned by `gen-G`'s pid and start; an `A` that can't be
//!    pinned can't have its namespace compared, which is `Unattributable`
//!    too. With no `gen-G` there is no `A` to read, and nothing is `Owned`.
//!    All of them reading `ENOENT` is a kernel without time namespaces.
//!
//! **The read** ([`read_export`]), only for an `Owned` invocation, and with
//! `Q` (which is `A`) and the helper held by their pidfds from before the
//! first classification to after the read. Both are checked live before the
//! read. The export is opened through the path rule ([`super::path_rule`]),
//! every check made on the opened descriptor, and read through it, never
//! reopened by path: empty when its size is 0 and a one-byte read returns
//! nothing (R3 §4.1). After the read `Q` is classified again, and then the
//! helper's and `Q`'s stats are read again, and must read as they did, and
//! both pins are polled after them, so an exit before that poll is caught.
//! A failed check revokes the attribution and discards the read
//! ([`Revoked`]), and is decided before the second class: a helper that
//! exited is `Unattributable` whatever `Q` became. Only with both pins
//! passing does a second class that isn't `Owned`, as the first was, revoke
//! as the class. The classifier's sampling limit stands (its module doc): a
//! change undone between the reads, or an `exec` of the same executable,
//! leaves equal reads, and an exit after the last poll is not seen here.
//!
//! **What the caller owes.** This module takes no lock and touches no
//! generation state, but it can't make its callers hold none. Its one caller
//! is the daemon's observer (`super::observe::Observer::attest`, Q35-14b),
//! which reads `gen-G` (`InstanceReader::identity`) once, attests with no
//! lock held (a debug build asserts it holds no ranked lock there, and the
//! observer's own seam lock is taken only after), then takes the seam lock
//! and the input lock once to apply what it got
//! (`GenerationInput::record_observe` or `record_refused`), where a
//! generation retired in between refuses the step as stale. It passes the
//! same helper pin and the same `Agent` to [`attribute`] and
//! [`read_export`]: the API doesn't tie them together or hold the helper's
//! pin for it. Dropping an attestation closes its pidfds.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use super::classifier::{self, Class, FileId, Identity, ProcStat, ProcView};
use super::helper::export_shaped;
use super::instance::Kind;
use super::path_rule::{self, FsView, Refusal};
use super::record::{Attribution, NotOwned};
use crate::event::ExportRead;

const ENOENT: i32 = libc::ENOENT;

/// The processes as the attribution reads them: the classifier's view, with
/// pidfds, argv and time namespaces.
pub trait Kernel: ProcView {
    /// A pidfd: holding it keeps its pid from naming another process unseen.
    type Pidfd;

    /// `pidfd_open(pid)`.
    fn pidfd_open(&mut self, pid: u32) -> io::Result<Self::Pidfd>;
    /// The pid a pidfd names (its `fdinfo`'s `Pid:`), an error once the
    /// process is gone or outside the daemon's pid namespace.
    fn pidfd_pid(&mut self, pidfd: &Self::Pidfd) -> io::Result<u32>;
    /// Whether the process behind `pidfd` has exited (its pidfd reads
    /// ready).
    fn exited(&mut self, pidfd: &Self::Pidfd) -> io::Result<bool>;
    /// `/proc/<pid>/cmdline`, split at its NULs.
    fn cmdline(&mut self, pid: u32) -> io::Result<Vec<OsString>>;
    /// `stat(/proc/<pid>/ns/time)`: the process's time namespace.
    fn time_ns(&mut self, pid: u32) -> io::Result<FileId>;
}

/// The filesystem as the read reads it: the path rule's view, with the
/// opened file's size and a read through its handle.
pub trait ExportFs: FsView {
    /// `fstat`'s size.
    fn size(&mut self, file: &Self::Handle) -> io::Result<u64>;
    /// A read at offset 0 through the handle.
    fn read(&mut self, file: &Self::Handle, buf: &mut [u8]) -> io::Result<usize>;
}

/// An errno, or `None` for an error that carries none.
fn errno(error: io::Error) -> Option<i32> {
    error.raw_os_error()
}

/// How the daemon came to a process.
#[derive(Debug)]
pub enum Sighting<P> {
    /// An IPC peer, by what `SO_PEERPIDFD` gave ([`peer_pidfd`]): its
    /// pidfd, `None` where the socket gives none, or the error. Only a
    /// pidfd pins: never a pid from `SO_PEERCRED` (the module doc).
    Peer(io::Result<Option<P>>),
    /// A process the daemon observed itself: a scan's or a marker's pid
    /// with the start ticks read with it, or the helper's parent from its
    /// pinned stat. Never an IPC peer's pid.
    Pid { pid: u32, start: Option<u64> },
}

/// A process held by its pidfd, named by the pid and the stat read after
/// the pidfd was open.
#[derive(Debug)]
pub struct Pinned<P> {
    pub pid: u32,
    /// The stat read after the pidfd opened.
    pub stat: ProcStat,
    pidfd: P,
}

impl<P> Pinned<P> {
    /// The start ticks: with the pid, the record's key.
    pub fn start(&self) -> u64 {
        self.stat.start
    }

    pub fn pidfd(&self) -> &P {
        &self.pidfd
    }

    /// The pidfd, for the record to hold until it is deleted.
    pub fn into_pidfd(self) -> P {
        self.pidfd
    }
}

/// Why a sighting names no process: the identity refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unpinned {
    /// The pidfd didn't open, or its pid didn't read, or `SO_PEERPIDFD`
    /// failed.
    Pidfd(Option<i32>),
    /// The socket gave no peer pidfd: an IPC request without one gets no
    /// identity.
    NoPeerPidfd,
    /// The stat didn't read.
    Stat(Option<i32>),
    /// The start ticks differ from the ones the sighting read.
    StartChanged,
    /// The process exited while it was read.
    Exited,
}

/// Pin the process a sighting names: its pidfd first, then its stat, then
/// a check that the pidfd still names a live process, so the stat is that
/// process's (core §2.3).
pub fn pin<K: Kernel>(
    kernel: &mut K,
    sighting: Sighting<K::Pidfd>,
) -> Result<Pinned<K::Pidfd>, Unpinned> {
    let (pid, pidfd, seen) = match sighting {
        Sighting::Peer(peer) => {
            let pidfd = peer
                .map_err(|e| Unpinned::Pidfd(errno(e)))?
                .ok_or(Unpinned::NoPeerPidfd)?;
            let pid = kernel
                .pidfd_pid(&pidfd)
                .map_err(|e| Unpinned::Pidfd(errno(e)))?;
            (pid, pidfd, None)
        }
        Sighting::Pid { pid, start } => {
            let pidfd = kernel
                .pidfd_open(pid)
                .map_err(|e| Unpinned::Pidfd(errno(e)))?;
            (pid, pidfd, start)
        }
    };
    let stat = kernel.stat(pid).map_err(|e| Unpinned::Stat(errno(e)))?;
    if seen.is_some_and(|start| start != stat.start) {
        return Err(Unpinned::StartChanged);
    }
    if !matches!(kernel.exited(&pidfd), Ok(false)) {
        return Err(Unpinned::Exited);
    }
    Ok(Pinned { pid, stat, pidfd })
}

/// What the attribution knows of the generation's agent.
pub struct Agent<'a> {
    /// `gen-G` as the caller read it for this attestation
    /// (`InstanceReader::identity`): `None` when it is absent or unreadable,
    /// which the classifier reads as `Unknown`.
    pub identity: Option<&'a Identity>,
    pub kind: Kind,
    /// The agent's environment as it was spawned, which its export shape
    /// reads (`TMPDIR`, `CODEX_HOME`, `HOME`).
    pub env: &'a dyn Fn(&str) -> Option<OsString>,
    /// The user's uid: the daemon's and the agent's.
    pub uid: u32,
    /// The daemon's own time namespace as read ([`Kernel::time_ns`]), the
    /// errno when it didn't read.
    pub time_ns: Result<FileId, Option<i32>>,
}

/// Whether a process's time namespace, as read, is the daemon's: the same
/// namespace, or a kernel with none (both read `ENOENT`).
fn same_time_ns(daemon: Result<FileId, Option<i32>>, other: Result<FileId, Option<i32>>) -> bool {
    match (daemon, other) {
        (Ok(daemon), Ok(other)) => daemon == other,
        (Err(Some(ENOENT)), Err(Some(ENOENT))) => true,
        _ => false,
    }
}

/// An attribution, with what the read revalidates: `Q`, pinned before its
/// first classification, and the export path the helper's argv named.
#[derive(Debug)]
pub struct Attributed<P> {
    attribution: Attribution,
    q: Option<Pinned<P>>,
    export: Option<PathBuf>,
}

impl<P> Attributed<P> {
    pub fn attribution(&self) -> Attribution {
        self.attribution
    }
}

/// The helper's export path from its argv (everything after `argv[0]`): the
/// last argument, when it has the agent's export shape and, for an IPC
/// request, the request named exactly these arguments.
fn shaped_export(
    agent: &Agent<'_>,
    args: &[OsString],
    request: Option<&[String]>,
) -> Option<PathBuf> {
    if !export_shaped(agent.kind, args, agent.env, agent.uid) {
        return None;
    }
    if let Some(request) = request {
        let same = args.len() == request.len()
            && args
                .iter()
                .zip(request)
                .all(|(arg, named)| arg.as_bytes() == named.as_bytes());
        if !same {
            return None;
        }
    }
    args.last().map(PathBuf::from)
}

/// Attribute the invocation of the pinned `helper` (core §2.3's four
/// steps). `request` is an IPC request's argv, `None` for a scan or a
/// marker. The result holds `Q`'s pin for [`read_export`].
pub fn attribute<K: Kernel>(
    kernel: &mut K,
    agent: &Agent<'_>,
    helper: &Pinned<K::Pidfd>,
    request: Option<&[String]>,
) -> Attributed<K::Pidfd> {
    let done = |attribution| Attributed {
        attribution,
        q: None,
        export: None,
    };
    // Step 1: `Q`, from the stat the helper was pinned with, pinned before
    // anything about it is read. Step 4 rereads the helper's stat.
    let Ok(q) = pin(
        kernel,
        Sighting::Pid {
            pid: helper.stat.ppid,
            start: None,
        },
    ) else {
        return done(changed(kernel, helper));
    };
    // Step 2.
    let class = classifier::classify(kernel, agent.identity, q.pid);
    // Step 3.
    let export = kernel
        .cmdline(helper.pid)
        .ok()
        .and_then(|argv| shaped_export(agent, argv.get(1..).unwrap_or_default(), request));
    // Step 4: `A`, for its time namespace: `Q` when `Q` is `A`, otherwise
    // pinned by `gen-G`'s pid and start. `None` inside: it didn't pin.
    let other_a;
    let a = match agent.identity {
        None => None,
        Some(id) if (q.pid, q.start()) == (id.pid, id.start) => Some(Some(&q)),
        Some(id) => {
            other_a = pin(
                kernel,
                Sighting::Pid {
                    pid: id.pid,
                    start: Some(id.start),
                },
            )
            .ok();
            Some(other_a.as_ref())
        }
    };
    // The last reads, then the polls: an exit before them is seen.
    let helper_ns = kernel.time_ns(helper.pid).map_err(errno);
    let a_ns = a.map(|a| a.map(|a| kernel.time_ns(a.pid).map_err(errno)));
    let helper_stat = kernel.stat(helper.pid).ok();
    let helper_alive = alive(kernel, helper);
    let q_alive = alive(kernel, &q);
    let a_same = match a {
        None => true,
        Some(None) => false,
        Some(Some(a)) => {
            alive(kernel, a)
                && a_ns
                    .flatten()
                    .is_some_and(|ns| same_time_ns(agent.time_ns, ns))
        }
    };
    let attribution = if !helper_alive || !same_time_ns(agent.time_ns, helper_ns) {
        Attribution::Unattributable
    } else if !q_alive || helper_stat != Some(helper.stat) {
        Attribution::NotOwned(NotOwned::Unknown)
    } else if !a_same {
        Attribution::Unattributable
    } else {
        match (class, &export) {
            (Class::Owned, Some(_)) => Attribution::Owned,
            (Class::Owned, None) => Attribution::NotOwned(NotOwned::Shape),
            (Class::PositivelyTool, _) => Attribution::NotOwned(NotOwned::Tool),
            (Class::Unknown, _) => Attribution::NotOwned(NotOwned::Unknown),
        }
    };
    Attributed {
        attribution,
        q: Some(q),
        export,
    }
}

/// Whether a pinned process is still live by its pidfd.
fn alive<K: Kernel>(kernel: &mut K, pinned: &Pinned<K::Pidfd>) -> bool {
    matches!(kernel.exited(&pinned.pidfd), Ok(false))
}

/// A helper whose parent can't be pinned: `Unattributable` if it exited
/// too, otherwise `NotOwned(Unknown)`.
fn changed<K: Kernel>(kernel: &mut K, helper: &Pinned<K::Pidfd>) -> Attribution {
    if alive(kernel, helper) {
        Attribution::NotOwned(NotOwned::Unknown)
    } else {
        Attribution::Unattributable
    }
}

/// The syscall a failed read came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOp {
    Size,
    Read,
}

/// What the daemon read of the export, through the descriptor the path rule
/// opened and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Export {
    /// Its size is 0 and a one-byte read returned nothing.
    Empty,
    NonEmpty,
    /// The path rule refused it.
    Refused(Refusal),
    Failed {
        op: ReadOp,
        errno: Option<i32>,
    },
}

impl Export {
    /// The read as the helper is told it.
    pub fn read(&self) -> ExportRead {
        match self {
            Export::Empty => ExportRead::Empty,
            Export::NonEmpty => ExportRead::NonEmpty,
            Export::Refused(_) | Export::Failed { .. } => ExportRead::Unreadable,
        }
    }
}

/// Why an `Owned` attribution didn't survive its read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revoked {
    /// The helper exited before or after the read.
    HelperExited,
    /// The helper's stat changed: it was reparented away from `Q`.
    HelperChanged,
    /// `Q` (`A`) exited before or after the read.
    AgentExited,
    /// `Q`'s stat changed: it was reparented, or its start differs.
    AgentChanged,
    /// The second classification wasn't `Owned`, as the first was.
    Class(Class),
}

/// An attestation: the attribution the record takes and, for an `Owned`
/// one, what the export read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attested {
    pub attribution: Attribution,
    /// `Some` only when the attribution is `Owned`.
    pub export: Option<Export>,
    /// Why an `Owned` attribution was revoked around the read.
    pub revoked: Option<Revoked>,
}

/// Read the export of an `Owned` invocation and revalidate the attribution
/// around it (R3 §4.1, R6 §6.2, R35CODE4-U3-01). Anything not `Owned` is
/// returned as it is, with no read. A revoked attribution is
/// `Unattributable` when the helper exited, `NotOwned(Unknown)` otherwise,
/// and its read is discarded; the pins are checked on every path, before a
/// changed class is. `Q`'s pin is held to the end and dropped here.
pub fn read_export<K: Kernel, F: ExportFs>(
    kernel: &mut K,
    fs: &mut F,
    agent: &Agent<'_>,
    helper: &Pinned<K::Pidfd>,
    attributed: Attributed<K::Pidfd>,
) -> Attested {
    let (Attribution::Owned, Some(q), Some(path)) =
        (attributed.attribution, &attributed.q, &attributed.export)
    else {
        return Attested {
            attribution: attributed.attribution,
            export: None,
            revoked: None,
        };
    };
    let revoke = |why| Attested {
        attribution: match why {
            Revoked::HelperExited => Attribution::Unattributable,
            _ => Attribution::NotOwned(NotOwned::Unknown),
        },
        export: None,
        revoked: Some(why),
    };
    if let Some(why) = check_pins(kernel, helper, q) {
        return revoke(why);
    }
    let export = read_through(fs, path, agent.uid);
    // The class acted on is `Owned`: an `Owned` attribution had it first.
    let again = classifier::classify(kernel, agent.identity, q.pid);
    if let Some(why) = check_pins(kernel, helper, q) {
        return revoke(why);
    }
    if again != Class::Owned {
        return revoke(Revoked::Class(again));
    }
    Attested {
        attribution: Attribution::Owned,
        export: Some(export),
        revoked: None,
    }
}

/// Both stats as pinned (the helper still `Q`'s child, `Q` still where it
/// was), and both pins live when polled after those reads. An exit comes
/// first: a stat may fail because its process exited.
fn check_pins<K: Kernel>(
    kernel: &mut K,
    helper: &Pinned<K::Pidfd>,
    q: &Pinned<K::Pidfd>,
) -> Option<Revoked> {
    let helper_stat = kernel.stat(helper.pid).ok();
    let q_stat = kernel.stat(q.pid).ok();
    if !alive(kernel, helper) {
        return Some(Revoked::HelperExited);
    }
    if !alive(kernel, q) {
        return Some(Revoked::AgentExited);
    }
    if helper_stat != Some(helper.stat) {
        return Some(Revoked::HelperChanged);
    }
    if q_stat != Some(q.stat) {
        return Some(Revoked::AgentChanged);
    }
    None
}

/// Open `path` through the path rule and read it through what it opened.
fn read_through<F: ExportFs>(fs: &mut F, path: &Path, uid: u32) -> Export {
    let verified = match path_rule::verify_export(fs, path, uid) {
        Ok(verified) => verified,
        Err(refusal) => return Export::Refused(refusal),
    };
    let size = match fs.size(&verified.file) {
        Ok(size) => size,
        Err(e) => {
            return Export::Failed {
                op: ReadOp::Size,
                errno: errno(e),
            };
        }
    };
    if size != 0 {
        return Export::NonEmpty;
    }
    let mut byte = [0u8; 1];
    match fs.read(&verified.file, &mut byte) {
        Ok(0) => Export::Empty,
        Ok(_) => Export::NonEmpty,
        Err(e) => Export::Failed {
            op: ReadOp::Read,
            errno: errno(e),
        },
    }
}

// ---------------------------------------------------------------------------
// The Linux views.
// ---------------------------------------------------------------------------

/// What `getsockopt(SO_PEERPIDFD)` returned, decoded: the pidfd, `None`
/// when the option isn't there (`ENOPROTOOPT`), or the error. The feature
/// is read from this result, not from the kernel's version.
pub fn decode_peer_pidfd<P>(got: io::Result<P>) -> io::Result<Option<P>> {
    match got {
        Ok(pidfd) => Ok(Some(pidfd)),
        Err(error) if error.raw_os_error() == Some(libc::ENOPROTOOPT) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The peer's pidfd from `SO_PEERPIDFD` (Linux 6.5 and later), for
/// [`Sighting::Peer`]. `None` (no option) refuses the peer's identity, as
/// an error does: there is no fallback to `SO_PEERCRED`'s pid.
pub fn peer_pidfd(stream: &UnixStream) -> io::Result<Option<OwnedFd>> {
    peer_pidfd_of(stream.as_fd())
}

/// [`peer_pidfd`] for a connected socket the caller holds as a descriptor:
/// the daemon's hook connections, which aren't a [`UnixStream`].
pub fn peer_pidfd_of(socket: BorrowedFd<'_>) -> io::Result<Option<OwnedFd>> {
    let mut fd: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `fd` and `len` are valid for writes of an int and its length.
    let rc = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut fd as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    decode_peer_pidfd(if rc != 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: the kernel just installed this descriptor for us.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
    })
}

/// The real processes: `/proc` through the classifier's Linux view, and
/// pidfds.
#[derive(Debug, Default)]
pub struct LinuxKernel(crate::platform::proc::LinuxProcView);

impl ProcView for LinuxKernel {
    fn stat(&mut self, pid: u32) -> io::Result<ProcStat> {
        self.0.stat(pid)
    }

    fn exe(&mut self, pid: u32) -> io::Result<FileId> {
        self.0.exe(pid)
    }

    fn exe_path(&mut self, pid: u32) -> io::Result<PathBuf> {
        self.0.exe_path(pid)
    }

    fn file(&mut self, path: &Path) -> io::Result<FileId> {
        self.0.file(path)
    }
}

/// `fdinfo`'s `Pid:` value: a pid, or `None` for `-1` (exited), `0`
/// (outside the reader's pid namespace) or anything unparsable.
fn fdinfo_pid(fdinfo: &str) -> Option<u32> {
    let value = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("Pid:"))?
        .trim();
    value.parse::<u32>().ok().filter(|pid| *pid != 0)
}

/// `/proc/<pid>/cmdline`'s words: NUL-terminated, the last terminator
/// optional.
fn split_cmdline(bytes: &[u8]) -> Vec<OsString> {
    let bytes = bytes.strip_suffix(b"\0").unwrap_or(bytes);
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes
        .split(|b| *b == 0)
        .map(|word| OsStr::from_bytes(word).to_owned())
        .collect()
}

impl Kernel for LinuxKernel {
    type Pidfd = OwnedFd;

    fn pidfd_open(&mut self, pid: u32) -> io::Result<OwnedFd> {
        let pid =
            libc::pid_t::try_from(pid).map_err(|_| io::Error::from_raw_os_error(libc::ESRCH))?;
        // SAFETY: `pidfd_open(pid, 0)`; a non-negative result is a new
        // close-on-exec descriptor, owned here.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
    }

    fn pidfd_pid(&mut self, pidfd: &OwnedFd) -> io::Result<u32> {
        let fdinfo = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))?;
        fdinfo_pid(&fdinfo).ok_or_else(|| io::Error::from_raw_os_error(libc::ESRCH))
    }

    fn exited(&mut self, pidfd: &OwnedFd) -> io::Result<bool> {
        let mut fds = [libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: one valid pollfd, no wait.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(rc != 0)
    }

    fn cmdline(&mut self, pid: u32) -> io::Result<Vec<OsString>> {
        Ok(split_cmdline(&std::fs::read(format!(
            "/proc/{pid}/cmdline"
        ))?))
    }

    fn time_ns(&mut self, pid: u32) -> io::Result<FileId> {
        let metadata = std::fs::metadata(format!("/proc/{pid}/ns/time"))?;
        Ok(FileId {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
}

impl ExportFs for crate::platform::fsperm::fs_view::LinuxFsView {
    fn size(&mut self, file: &OwnedFd) -> io::Result<u64> {
        Ok(File::from(file.try_clone()?).metadata()?.len())
    }

    fn read(&mut self, file: &OwnedFd, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buf` is writable for its length, and `file` is open.
        let n = unsafe { libc::pread(file.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }
}
