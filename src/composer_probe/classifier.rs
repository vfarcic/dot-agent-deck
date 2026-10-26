//! PRD #1666 part 3 (Q35-04; R5 §2.2–§2.3, R4 §7.1, core §2.3 steps 1–2):
//! whose editor invocation this is. The helper runs as the editor; its
//! parent `Q` invoked it. [`classify`] says whether `Q` is the agent's own
//! composer ([`Class::Owned`]), positively a tool the agent ran
//! ([`Class::PositivelyTool`]), or neither for certain ([`Class::Unknown`]).
//!
//! [`ProcView`] is the seam: the classifier reads processes only through it,
//! a scripted view drives the tests, and the Linux view reads `/proc`
//! (`platform::proc::LinuxProcView`). The helper (Q35-12) and the daemon's
//! attribution (Q35-14a) will run this one function; nothing calls it yet.
//!
//! **The agent** `A` is the generation's [`Identity`]: its pid and start
//! ticks name the process, its executable's `(dev, ino)` is F1, and its
//! install root (F2, a path and that directory's `(dev, ino)`) and command
//! basenames (F3) make up the agent's family with F1 (R5 §2.2). A process is
//! **in the family** when its executable is F1, a directory above its
//! resolved path is F2 (by `(dev, ino)`, so a renamed or bind-mounted root
//! still counts), its basename is `claude`, `codex` or `A`'s own, or its
//! executable link reads `… (deleted)`.
//!
//! **The classes, first match wins** (R5 §2.3, narrowed):
//! 1. No identity, or `Q`'s stat or executable unreadable: `Unknown`.
//! 2. `Q` is `A` by pid and start ticks: `Owned` if it still runs F1, else
//!    `Unknown` (`A` after an `exec` or a replaced binary is never a tool).
//! 3. F2's recorded path no longer reads as F2's `(dev, ino)`, or a
//!    directory above `Q`'s executable doesn't read: `Unknown`. The family's
//!    boundary can't be drawn, so nothing outside it is positively a tool.
//! 4. `Q` is in the family: `Unknown` (a nested agent, an agent process
//!    other than `A`, a descendant after an update, a copy of the agent, a
//!    deleted executable).
//! 5. The walk from `Q` reaches `A` within [`MAX_TOOL_STEPS`] through
//!    processes whose stat reads, with monotone starts: `PositivelyTool`.
//! 6. Anything else, a `Q` reparented away from `A` among them: `Unknown`.
//!
//! **Only `A` itself is `Owned`.** R5 §2.3 also admitted a chain of F1
//! processes from `Q` up to `A` (M24's overview: `claude daemon run` →
//! `bg-pty-host` → `--session-id`). That chain can't be told from a nested
//! agent a tool started: a tool's shell that `exec`s the agent's binary
//! leaves the same tree (`A` → F1 → …), and `/proc` keeps no record of what
//! a process ran before its last `exec` (R35CODE4-U2-01). So every F1
//! process but `A` is family, and `Unknown`; Claude's composer after the
//! overview is one of them.
//!
//! A tool is reached only on positive evidence, outside the family **and**
//! with unbroken ancestry to `A`; every executable mismatch is `Unknown`, and
//! so is a family record that has gone stale (R35CODE4-U2-02). A pid is never
//! trusted without its start ticks, so a process that reuses `A`'s pid isn't
//! `A`.
//!
//! **What a classification is, and what its caller owes.** The walk is read
//! twice, and two reads that differ are `Unknown`. That detects a change
//! between the samples, not every change: each process is read in separate
//! `stat`, `stat`-of-executable and `readlink` calls, a process can `exec`
//! or be reparented after the last read, and a change undone between reads
//! leaves equal samples. A pid with its start ticks names a process to the
//! tick, not beyond it. So a result is evidence about the samples, not a
//! live snapshot. The caller pins what it relies on: the helper holds
//! pidfds for itself and `Q` from before the first read (Q35-12), and the
//! daemon's attribution holds `Q`'s pidfd across the walk and the export
//! read, revalidating around that read (Q35-14a, Q35-14b).

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

/// How many parent steps a tool's walk takes from `Q` to `A`, at most.
pub const MAX_TOOL_STEPS: usize = 16;

/// The agents' command basenames (F3, with `A`'s own).
pub const AGENT_BASENAMES: &[&str] = &["claude", "codex"];

/// What Linux appends to `/proc/<pid>/exe`'s target once the file is gone.
const DELETED: &[u8] = b" (deleted)";

/// A file's identity: `st_dev` and `st_ino`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

/// `/proc/<pid>/stat`, as far as the classifier reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcStat {
    pub ppid: u32,
    /// The start time, in clock ticks since boot (field 22).
    pub start: u64,
}

/// The processes as the classifier reads them, by pid, and the directories
/// their executables are in, by path.
pub trait ProcView {
    /// `/proc/<pid>/stat`.
    fn stat(&mut self, pid: u32) -> io::Result<ProcStat>;
    /// `stat(/proc/<pid>/exe)`: the executable the process runs.
    fn exe(&mut self, pid: u32) -> io::Result<FileId>;
    /// `readlink(/proc/<pid>/exe)`: where that executable is, or was.
    fn exe_path(&mut self, pid: u32) -> io::Result<PathBuf>;
    /// `stat(path)`: a directory's identity.
    fn file(&mut self, path: &Path) -> io::Result<FileId>;
}

/// The generation's agent `A` and its family (R5 §2.2), from the identity
/// file written at spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub pid: u32,
    /// `A`'s start ticks.
    pub start: u64,
    /// F1: `A`'s executable.
    pub exe: FileId,
    /// F2: the install root, such as Claude's `versions/` directory or
    /// Codex's npm package root.
    pub install_root: PathBuf,
    /// F2's `(dev, ino)`, recorded with its path.
    pub install_root_id: FileId,
    /// `A`'s own executable basename, with [`AGENT_BASENAMES`] F3.
    pub basename: OsString,
}

/// Whose invocation it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// The agent's own composer: `A` itself, still on F1.
    Owned,
    /// A tool the agent ran: outside the family, below `A`.
    PositivelyTool,
    /// Neither for certain.
    Unknown,
}

/// A read, with an error kept as its errno only, so two reads compare.
type Read<T> = Result<T, Option<i32>>;

/// One process on the walk, as read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Link {
    pid: u32,
    stat: Read<ProcStat>,
    exe: Read<FileId>,
    exe_path: Read<PathBuf>,
}

impl Link {
    fn start(&self) -> Option<u64> {
        self.stat.as_ref().ok().map(|stat| stat.start)
    }

    fn is(&self, identity: &Identity) -> bool {
        self.pid == identity.pid && self.start() == Some(identity.start)
    }

    fn runs_f1(&self, identity: &Identity) -> bool {
        self.exe == Ok(identity.exe)
    }
}

/// One read of everything a classification looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sample {
    /// From `Q` up (see [`read_walk`]).
    walk: Vec<Link>,
    /// What F2's recorded path reads as now.
    root: Read<FileId>,
    /// The directories above `Q`'s executable, nearest first, when its link
    /// read.
    dirs: Vec<Read<FileId>>,
}

fn errno(error: io::Error) -> Option<i32> {
    error.raw_os_error()
}

/// Read the walk from `q` up: each process until `A`, a process with no
/// parent, a stat that doesn't read, or one step past [`MAX_TOOL_STEPS`].
fn read_walk<V: ProcView>(view: &mut V, identity: &Identity, q: u32) -> Vec<Link> {
    let mut walk = Vec::new();
    let mut pid = q;
    loop {
        let link = Link {
            pid,
            stat: view.stat(pid).map_err(errno),
            exe: view.exe(pid).map_err(errno),
            exe_path: view.exe_path(pid).map_err(errno),
        };
        let next = match link.stat {
            Ok(stat) if stat.ppid != 0 && !link.is(identity) && walk.len() < MAX_TOOL_STEPS => {
                Some(stat.ppid)
            }
            _ => None,
        };
        walk.push(link);
        match next {
            Some(ppid) => pid = ppid,
            None => return walk,
        }
    }
}

fn read_sample<V: ProcView>(view: &mut V, identity: &Identity, q: u32) -> Sample {
    let walk = read_walk(view, identity, q);
    let root = view.file(&identity.install_root).map_err(errno);
    let dirs = match &walk[0].exe_path {
        Ok(path) => path
            .ancestors()
            .skip(1)
            .map(|dir| view.file(dir).map_err(errno))
            .collect(),
        Err(_) => Vec::new(),
    };
    Sample { walk, root, dirs }
}

/// The steps from `Q` to `A` over a walk whose every start is at or after
/// its parent's, or `None` when it doesn't reach `A` that way.
fn steps_to_agent(walk: &[Link], identity: &Identity) -> Option<usize> {
    let at = walk.iter().position(|link| link.is(identity))?;
    let monotone = walk[..=at].windows(2).all(|pair| {
        matches!((pair[0].start(), pair[1].start()), (Some(child), Some(parent)) if child >= parent)
    });
    monotone.then_some(at)
}

/// Whether `link`, whose executable is at `path` below the directories
/// `dirs`, is in `identity`'s family (R5 §2.2). Under F2's recorded path is
/// within this: [`class_of`] has checked that path reads as F2.
fn in_family(link: &Link, path: &Path, dirs: &[Read<FileId>], identity: &Identity) -> bool {
    link.runs_f1(identity)
        || dirs.contains(&Ok(identity.install_root_id))
        || path.as_os_str().as_encoded_bytes().ends_with(DELETED)
        || path.file_name().is_some_and(|name| {
            name == identity.basename
                || AGENT_BASENAMES
                    .iter()
                    .any(|&agent| name == OsStr::new(agent))
        })
}

fn class_of(sample: &Sample, identity: &Identity) -> Class {
    let q = &sample.walk[0];
    let (Ok(_), Ok(_), Ok(path)) = (&q.stat, &q.exe, &q.exe_path) else {
        return Class::Unknown;
    };
    if q.is(identity) {
        return if q.runs_f1(identity) {
            Class::Owned
        } else {
            Class::Unknown
        };
    }
    if sample.root != Ok(identity.install_root_id) || sample.dirs.iter().any(Result::is_err) {
        return Class::Unknown;
    }
    if in_family(q, path, &sample.dirs, identity) {
        return Class::Unknown;
    }
    match steps_to_agent(&sample.walk, identity) {
        Some(_) => Class::PositivelyTool,
        None => Class::Unknown,
    }
}

/// Classify the invocation whose editor's parent is `q`, against the
/// generation's identity (`None` when its file can't be read). Everything is
/// read twice, and differing reads are [`Class::Unknown`]; the module doc
/// says what that does and doesn't establish, and what the caller pins.
pub fn classify<V: ProcView>(view: &mut V, identity: Option<&Identity>, q: u32) -> Class {
    let Some(identity) = identity else {
        return Class::Unknown;
    };
    let first = read_sample(view, identity, q);
    let second = read_sample(view, identity, q);
    if first != second {
        return Class::Unknown;
    }
    class_of(&first, identity)
}
