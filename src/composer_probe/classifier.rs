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
//! install root (F2) and command basenames (F3) make up the agent's family
//! with F1 (R5 §2.2). A process is **in the family** when its executable is
//! F1, its resolved path lies under F2, its basename is `claude`, `codex` or
//! `A`'s own, or its executable link reads `… (deleted)`.
//!
//! **The classes, first match wins** (R5 §2.3):
//! 1. No identity, or `Q`'s stat or executable unreadable: `Unknown`.
//! 2. `Q` is `A` by pid and start ticks: `Owned` if it still runs F1, else
//!    `Unknown` (`A` after an `exec` or a replaced binary is never a tool).
//! 3. The parent walk from `Q` reaches `A` within [`MAX_OWNED_STEPS`], every
//!    process on it, `A` included, runs F1, and every start is at or after
//!    its parent's: `Owned` (M24's overview chain).
//! 4. `Q` is in the family: `Unknown` (a nested agent, a descendant after an
//!    update, a copy of the agent, a deleted executable).
//! 5. The walk from `Q` reaches `A` within [`MAX_TOOL_STEPS`] through
//!    processes whose stat reads, with monotone starts: `PositivelyTool`.
//! 6. Anything else, a `Q` reparented away from `A` among them: `Unknown`.
//!
//! A tool is reached only on positive evidence, outside the family **and**
//! with unbroken ancestry to `A`; every executable mismatch is `Unknown`.
//! The walk is read twice, and two reads that differ are `Unknown`: the tree
//! changed while it was read. A pid is never trusted without its start
//! ticks, so a process that reuses `A`'s pid isn't `A`.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

/// How many parent steps an `Owned` walk takes from `Q` to `A`, at most.
pub const MAX_OWNED_STEPS: usize = 8;

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

/// The processes as the classifier reads them, by pid.
pub trait ProcView {
    /// `/proc/<pid>/stat`.
    fn stat(&mut self, pid: u32) -> io::Result<ProcStat>;
    /// `stat(/proc/<pid>/exe)`: the executable the process runs.
    fn exe(&mut self, pid: u32) -> io::Result<FileId>;
    /// `readlink(/proc/<pid>/exe)`: where that executable is, or was.
    fn exe_path(&mut self, pid: u32) -> io::Result<PathBuf>;
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
    /// `A`'s own executable basename, with [`AGENT_BASENAMES`] F3.
    pub basename: OsString,
}

/// Whose invocation it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// The agent's own composer: `A`, or `A`'s executable all the way to it.
    Owned,
    /// A tool the agent ran: outside the family, below `A`.
    PositivelyTool,
    /// Neither for certain.
    Unknown,
}

/// One process on the walk, as read. Errors keep only their errno, so two
/// reads compare.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Link {
    pid: u32,
    stat: Result<ProcStat, Option<i32>>,
    exe: Result<FileId, Option<i32>>,
    exe_path: Result<PathBuf, Option<i32>>,
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

/// The steps from `Q` to `A` over a walk whose every start is at or after
/// its parent's, or `None` when it doesn't reach `A` that way.
fn steps_to_agent(walk: &[Link], identity: &Identity) -> Option<usize> {
    let at = walk.iter().position(|link| link.is(identity))?;
    let monotone = walk[..=at].windows(2).all(|pair| {
        matches!((pair[0].start(), pair[1].start()), (Some(child), Some(parent)) if child >= parent)
    });
    monotone.then_some(at)
}

/// Whether `link` is in `identity`'s family (R5 §2.2). Its executable reads.
fn in_family(link: &Link, path: &Path, identity: &Identity) -> bool {
    link.runs_f1(identity)
        || path.starts_with(&identity.install_root)
        || path.as_os_str().as_encoded_bytes().ends_with(DELETED)
        || path.file_name().is_some_and(|name| {
            name == identity.basename
                || AGENT_BASENAMES
                    .iter()
                    .any(|&agent| name == OsStr::new(agent))
        })
}

fn class_of(walk: &[Link], identity: &Identity) -> Class {
    let q = &walk[0];
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
    let steps = steps_to_agent(walk, identity);
    if let Some(at) = steps
        && at <= MAX_OWNED_STEPS
        && walk[..=at].iter().all(|link| link.runs_f1(identity))
    {
        return Class::Owned;
    }
    if in_family(q, path, identity) {
        return Class::Unknown;
    }
    match steps {
        Some(_) => Class::PositivelyTool,
        None => Class::Unknown,
    }
}

/// Classify the invocation whose editor's parent is `q`, against the
/// generation's identity (`None` when its file can't be read). The walk is
/// read twice, and differing reads are [`Class::Unknown`].
pub fn classify<V: ProcView>(view: &mut V, identity: Option<&Identity>, q: u32) -> Class {
    let Some(identity) = identity else {
        return Class::Unknown;
    };
    let first = read_walk(view, identity, q);
    let second = read_walk(view, identity, q);
    if first != second {
        return Class::Unknown;
    }
    class_of(&first, identity)
}
