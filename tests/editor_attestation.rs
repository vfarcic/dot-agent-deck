//! PRD #1666 part 3 (Q35-14a): the daemon's attribution of an editor
//! invocation and its read of the export (`src/composer_probe/attest.rs`).
//! Most cases run over a scripted process table and filesystem, which can
//! change at an exact point of an attestation (between the first
//! classification and the read, during the read, during the second
//! classification); the rest read this process and a real file through the
//! Linux views.
#![cfg(target_os = "linux")]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use dot_agent_deck::composer_probe::attest::{
    Agent, Attested, Export, ExportFs, Kernel, Pinned, ReadOp, Revoked, Sighting, Unpinned,
    attribute, pin, read_export,
};
use dot_agent_deck::composer_probe::classifier::{Class, FileId, Identity, ProcStat, ProcView};
use dot_agent_deck::composer_probe::install::ProbeAgent;
use dot_agent_deck::composer_probe::instance::Kind;
use dot_agent_deck::composer_probe::path_rule::{
    Cause, Entry, FsView, Kind as NodeKind, Stat, Xattr,
};
use dot_agent_deck::composer_probe::record::{Attribution, NotOwned};
use dot_agent_deck::event::ExportRead;
use spec::spec;

#[path = "../src/test_temp.rs"]
mod test_temp;

const ENOENT: i32 = 2;
const ESRCH: i32 = 3;
const EIO: i32 = 5;
const EACCES: i32 = 13;

const ME: u32 = 1000;

/// Claude's native binary: F1, under F2.
const CLAUDE: FileId = FileId { dev: 8, ino: 4242 };
const VERSIONS: &str = "/home/me/.local/share/claude/versions";
const VERSIONS_DIR: FileId = FileId { dev: 8, ino: 4000 };
const BASH: FileId = FileId { dev: 8, ino: 77 };
const HELPER_EXE: FileId = FileId { dev: 8, ino: 999 };
const TIME_NS: FileId = FileId {
    dev: 4,
    ino: 4026531834,
};

/// `A`, a tool's shell below it, a nested agent the shell started, and the
/// helper, whose parent each case chooses.
const A: u32 = 100;
const A_START: u64 = 10_000;
const SHELL: u32 = 300;
const NESTED: u32 = 400;
const H: u32 = 500;
const H_START: u64 = 10_500;

const HELPER_PATH: &str = "/state/editor/1-1/dot-agent-deck-editor";
const CLAUDE_EXPORT: &str = "/tmp/claude-1000/claude-prompt-1.md";
const CODEX_EXPORT: &str = "/home/me/.codex/editor/.tmpAbC.md";

#[derive(Debug, Clone)]
struct Proc {
    ppid: u32,
    start: u64,
    exe: FileId,
    path: PathBuf,
    /// `None`: the cmdline doesn't read.
    argv: Option<Vec<OsString>>,
    ns: Result<FileId, i32>,
    exited: bool,
    stat_fails: Option<i32>,
}

type Procs = BTreeMap<u32, Proc>;

/// Where a scripted change happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    /// Before the `n`th read (from 1) of this pid's stat.
    Stat(u32, usize),
    /// Before the helper's cmdline is read: during the attribution's reads.
    Cmdline,
    /// Before the export's size is read: during the read.
    Size,
}

type Change = fn(&mut Procs);

/// A pidfd: the process it was opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fd {
    pid: u32,
    start: u64,
}

#[derive(Default)]
struct World {
    procs: Procs,
    changes: Vec<(At, Change)>,
    stats: BTreeMap<u32, usize>,
    pidfd_fails: Option<i32>,
    /// The filesystem calls made, by name.
    fs_calls: Vec<&'static str>,
}

impl World {
    fn fire(&mut self, at: At) {
        let mut i = 0;
        while i < self.changes.len() {
            if self.changes[i].0 == at {
                let (_, change) = self.changes.remove(i);
                change(&mut self.procs);
            } else {
                i += 1;
            }
        }
    }
}

#[derive(Clone)]
struct Sys(Rc<RefCell<World>>);

fn err<T>(errno: i32) -> io::Result<T> {
    Err(io::Error::from_raw_os_error(errno))
}

impl Sys {
    fn proc(&self, pid: u32) -> io::Result<Proc> {
        self.0
            .borrow()
            .procs
            .get(&pid)
            .cloned()
            .ok_or_else(|| io::Error::from_raw_os_error(ESRCH))
    }

    fn on(&self, at: At, change: Change) {
        self.0.borrow_mut().changes.push((at, change));
    }

    fn change(&self, change: impl FnOnce(&mut Procs)) {
        change(&mut self.0.borrow_mut().procs);
    }

    fn fs_calls(&self) -> Vec<&'static str> {
        self.0.borrow().fs_calls.clone()
    }
}

impl ProcView for Sys {
    fn stat(&mut self, pid: u32) -> io::Result<ProcStat> {
        {
            let mut world = self.0.borrow_mut();
            let n = world.stats.entry(pid).or_default();
            *n += 1;
            let n = *n;
            world.fire(At::Stat(pid, n));
        }
        let p = self.proc(pid)?;
        match p.stat_fails {
            Some(errno) => err(errno),
            None => Ok(ProcStat {
                ppid: p.ppid,
                start: p.start,
            }),
        }
    }

    fn exe(&mut self, pid: u32) -> io::Result<FileId> {
        Ok(self.proc(pid)?.exe)
    }

    fn exe_path(&mut self, pid: u32) -> io::Result<PathBuf> {
        Ok(self.proc(pid)?.path)
    }

    fn file(&mut self, path: &Path) -> io::Result<FileId> {
        if path == Path::new(VERSIONS) {
            return Ok(VERSIONS_DIR);
        }
        // Any other directory is one of its own, never F2.
        let ino = path.as_os_str().len() as u64 + 50_000;
        Ok(FileId { dev: 8, ino })
    }
}

impl Kernel for Sys {
    type Pidfd = Fd;

    fn pidfd_open(&mut self, pid: u32) -> io::Result<Fd> {
        if let Some(errno) = self.0.borrow().pidfd_fails {
            return err(errno);
        }
        let p = self.proc(pid)?;
        Ok(Fd {
            pid,
            start: p.start,
        })
    }

    fn pidfd_pid(&mut self, pidfd: &Fd) -> io::Result<u32> {
        match self.exited(pidfd)? {
            true => err(ESRCH),
            false => Ok(pidfd.pid),
        }
    }

    fn exited(&mut self, pidfd: &Fd) -> io::Result<bool> {
        Ok(match self.0.borrow().procs.get(&pidfd.pid) {
            Some(p) => p.exited || p.start != pidfd.start,
            None => true,
        })
    }

    fn cmdline(&mut self, pid: u32) -> io::Result<Vec<OsString>> {
        self.0.borrow_mut().fire(At::Cmdline);
        self.proc(pid)?
            .argv
            .ok_or_else(|| io::Error::from_raw_os_error(EACCES))
    }

    fn time_ns(&mut self, pid: u32) -> io::Result<FileId> {
        self.proc(pid)?.ns.or_else(err)
    }
}

fn proc(ppid: u32, start: u64, exe: FileId, path: &str) -> Proc {
    Proc {
        ppid,
        start,
        exe,
        path: path.into(),
        argv: Some(vec![path.into()]),
        ns: Ok(TIME_NS),
        exited: false,
        stat_fails: None,
    }
}

fn args(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

/// `A` on F1, a tool's shell below it, a nested agent the shell started on
/// F1, and the helper below `parent` with the export `export`.
fn world(parent: u32, export: &str) -> Sys {
    let mut procs = Procs::new();
    procs.insert(A, proc(1, A_START, CLAUDE, &format!("{VERSIONS}/2.1.310")));
    procs.insert(SHELL, proc(A, 10_100, BASH, "/usr/bin/bash"));
    procs.insert(
        NESTED,
        proc(SHELL, 10_200, CLAUDE, &format!("{VERSIONS}/2.1.310")),
    );
    let mut helper = proc(parent, H_START, HELPER_EXE, HELPER_PATH);
    helper.argv = Some(args(&[HELPER_PATH, export]));
    procs.insert(H, helper);
    Sys(Rc::new(RefCell::new(World {
        procs,
        ..World::default()
    })))
}

fn identity() -> Identity {
    Identity {
        pid: A,
        start: A_START,
        exe: CLAUDE,
        install_root: VERSIONS.into(),
        install_root_id: VERSIONS_DIR,
        basename: "2.1.310".into(),
    }
}

fn no_env(_: &str) -> Option<OsString> {
    None
}

fn codex_env(name: &str) -> Option<OsString> {
    (name == "HOME").then(|| "/home/me".into())
}

fn agent<'a>(identity: Option<&'a Identity>, kind: Kind) -> Agent<'a> {
    Agent {
        identity,
        kind,
        env: match kind {
            Kind::Agent(ProbeAgent::Codex) => &codex_env,
            _ => &no_env,
        },
        uid: ME,
        time_ns: Ok(TIME_NS),
    }
}

const CLAUDE_KIND: Kind = Kind::Agent(ProbeAgent::Claude);
const CODEX_KIND: Kind = Kind::Agent(ProbeAgent::Codex);

fn pinned(sys: &mut Sys) -> Pinned<Fd> {
    pin(
        sys,
        Sighting::Pid {
            pid: H,
            start: Some(H_START),
        },
    )
    .expect("the helper pins")
}

fn request(export: &str) -> Vec<String> {
    vec![export.to_owned()]
}

/// The attribution of `H` in `sys`, for an IPC request naming `export`.
fn attribution_of(sys: &mut Sys, agent: &Agent<'_>, export: &str) -> Attribution {
    let helper = pinned(sys);
    attribute(sys, agent, &helper, Some(request(export).as_slice())).attribution()
}

// ---------------------------------------------------------------------------
// A scripted filesystem: what the path rule walks and the read reads.
// ---------------------------------------------------------------------------

const EXT4: u32 = 0xEF53;
const TMPFS: u32 = 0x0102_1994;

#[derive(Debug, Clone)]
struct Node {
    path: PathBuf,
    kind: NodeKind,
    mode: u32,
    uid: u32,
    fs: u32,
    /// A file's bytes, and how many a read returns when its size says 0.
    size: u64,
    reads: usize,
}

#[derive(Clone)]
struct Fs {
    nodes: Vec<Node>,
    sys: Sys,
    size_fails: Option<i32>,
    read_fails: Option<i32>,
}

impl Fs {
    /// `/` and `/home` (root's, 0755, ext4), `/home/me` (0750), `/tmp`
    /// (root's, 01777, tmpfs), Claude's `/tmp/claude-1000` (0700) holding
    /// its empty 0664 export, and Codex's `~/.codex/editor` (0700) holding
    /// its empty 0600 export.
    fn new(sys: &Sys) -> Fs {
        let node = |path: &str, kind, mode, uid, fs| Node {
            path: path.into(),
            kind,
            mode,
            uid,
            fs,
            size: 0,
            reads: 0,
        };
        Fs {
            nodes: vec![
                node("/", NodeKind::Dir, 0o755, 0, EXT4),
                node("/home", NodeKind::Dir, 0o755, 0, EXT4),
                node("/home/me", NodeKind::Dir, 0o750, ME, EXT4),
                node("/home/me/.codex", NodeKind::Dir, 0o700, ME, EXT4),
                node("/home/me/.codex/editor", NodeKind::Dir, 0o700, ME, EXT4),
                node(CODEX_EXPORT, NodeKind::File, 0o600, ME, EXT4),
                node("/tmp", NodeKind::Dir, 0o1777, 0, TMPFS),
                node("/tmp/claude-1000", NodeKind::Dir, 0o700, ME, TMPFS),
                node(CLAUDE_EXPORT, NodeKind::File, 0o664, ME, TMPFS),
            ],
            sys: sys.clone(),
            size_fails: None,
            read_fails: None,
        }
    }

    fn at(&mut self, path: &str) -> &mut Node {
        self.nodes
            .iter_mut()
            .find(|n| n.path == Path::new(path))
            .expect("a scripted node")
    }

    fn log(&self, call: &'static str) {
        self.sys.0.borrow_mut().fs_calls.push(call);
    }

    fn open(&self, dir: &usize, name: &OsStr) -> io::Result<Entry<usize>> {
        let path = self.nodes[*dir].path.join(name);
        match self.nodes.iter().position(|n| n.path == path) {
            Some(i) => Ok(Entry::Opened(i)),
            None => err(ENOENT),
        }
    }
}

impl FsView for Fs {
    type Handle = usize;

    fn open_root(&mut self) -> io::Result<usize> {
        self.log("open_root");
        Ok(0)
    }

    fn open_dir(&mut self, dir: &usize, name: &OsStr) -> io::Result<Entry<usize>> {
        self.open(dir, name)
    }

    fn open_file(&mut self, dir: &usize, name: &OsStr) -> io::Result<Entry<usize>> {
        self.log("open_file");
        self.open(dir, name)
    }

    fn stat(&mut self, node: &usize) -> io::Result<Stat> {
        let n = &self.nodes[*node];
        Ok(Stat {
            kind: n.kind,
            mode: n.mode,
            uid: n.uid,
            nlink: 1,
        })
    }

    fn fs_type(&mut self, node: &usize) -> io::Result<u32> {
        Ok(self.nodes[*node].fs)
    }

    fn xattr(&mut self, _: &usize, _: &str) -> io::Result<Xattr> {
        Ok(Xattr::Absent)
    }
}

impl ExportFs for Fs {
    fn size(&mut self, file: &usize) -> io::Result<u64> {
        self.log("size");
        self.sys.0.borrow_mut().fire(At::Size);
        match self.size_fails {
            Some(errno) => err(errno),
            None => Ok(self.nodes[*file].size),
        }
    }

    fn read(&mut self, file: &usize, buf: &mut [u8]) -> io::Result<usize> {
        self.log("read");
        match self.read_fails {
            Some(errno) => err(errno),
            None => Ok(self.nodes[*file].reads.min(buf.len())),
        }
    }
}

/// A whole attestation of `H` for an IPC request naming `export`.
fn attest(sys: &mut Sys, fs: &mut Fs, agent: &Agent<'_>, export: &str) -> Attested {
    let helper = pinned(sys);
    let attributed = attribute(sys, agent, &helper, Some(request(export).as_slice()));
    read_export(sys, fs, agent, &helper, attributed)
}

fn owned(export: Export) -> Attested {
    Attested {
        attribution: Attribution::Owned,
        export: Some(export),
        revoked: None,
    }
}

fn revoked(attribution: Attribution, why: Revoked) -> Attested {
    Attested {
        attribution,
        export: None,
        revoked: Some(why),
    }
}

const UNKNOWN: Attribution = Attribution::NotOwned(NotOwned::Unknown);

fn exit_a(procs: &mut Procs) {
    procs.get_mut(&A).unwrap().exited = true;
}

fn exit_helper(procs: &mut Procs) {
    procs.get_mut(&H).unwrap().exited = true;
}

fn exec_a(procs: &mut Procs) {
    let a = procs.get_mut(&A).unwrap();
    a.exe = BASH;
    a.path = "/usr/bin/bash".into();
}

fn reparent_a(procs: &mut Procs) {
    procs.get_mut(&A).unwrap().ppid = 2;
}

fn reparent_helper(procs: &mut Procs) {
    procs.get_mut(&H).unwrap().ppid = 1;
}

// ---------------------------------------------------------------------------
// The cases.
// ---------------------------------------------------------------------------

/// Scenario: The daemon pins a scripted helper from its pidfd and from its pid, with and without the start ticks a scan read. A pidfd that won't open or whose pid won't read, a stat that fails, start ticks that differ from the scan's, and a process that exited while it was read each refuse the identity, and no process is pinned.
#[spec("inbox/probe/068")]
#[test]
fn probe_068_a_process_is_pinned_by_its_pidfd_then_its_start() {
    let mut sys = world(A, CLAUDE_EXPORT);
    let by_pid = pinned(&mut sys);
    assert_eq!((by_pid.pid, by_pid.start()), (H, H_START));
    assert_eq!(
        by_pid.stat,
        ProcStat {
            ppid: A,
            start: H_START
        }
    );
    assert_eq!(
        *by_pid.pidfd(),
        Fd {
            pid: H,
            start: H_START
        }
    );
    let unseen = pin(
        &mut sys,
        Sighting::Pid {
            pid: H,
            start: None,
        },
    )
    .unwrap();
    assert_eq!(unseen.start(), H_START);
    let fd = by_pid.into_pidfd();
    let by_pidfd = pin(&mut sys, Sighting::Pidfd(fd)).unwrap();
    assert_eq!((by_pidfd.pid, by_pidfd.start()), (H, H_START));

    let refused = |sys: &mut Sys, sighting| pin(sys, sighting).map(|p| p.pid).unwrap_err();
    assert_eq!(
        refused(
            &mut sys,
            Sighting::Pid {
                pid: H,
                start: Some(H_START + 1)
            }
        ),
        Unpinned::StartChanged
    );
    assert_eq!(
        refused(
            &mut sys,
            Sighting::Pid {
                pid: 9,
                start: None
            }
        ),
        Unpinned::Pidfd(Some(ESRCH))
    );
    sys.0.borrow_mut().pidfd_fails = Some(EACCES);
    assert_eq!(
        refused(
            &mut sys,
            Sighting::Pid {
                pid: H,
                start: None
            }
        ),
        Unpinned::Pidfd(Some(EACCES))
    );
    sys.0.borrow_mut().pidfd_fails = None;
    sys.change(|p| p.get_mut(&H).unwrap().stat_fails = Some(EACCES));
    assert_eq!(
        refused(
            &mut sys,
            Sighting::Pid {
                pid: H,
                start: None
            }
        ),
        Unpinned::Stat(Some(EACCES))
    );
    sys.change(|p| p.get_mut(&H).unwrap().stat_fails = None);

    // Exited between the pidfd and the stat: the stat may be another's.
    let n = sys.0.borrow().stats.get(&H).copied().unwrap_or(0);
    sys.on(At::Stat(H, n + 1), exit_helper);
    assert_eq!(
        refused(
            &mut sys,
            Sighting::Pid {
                pid: H,
                start: None
            }
        ),
        Unpinned::Exited
    );
    let fd = Fd {
        pid: H,
        start: H_START,
    };
    assert_eq!(
        refused(&mut sys, Sighting::Pidfd(fd)),
        Unpinned::Pidfd(Some(ESRCH))
    );
}

/// Scenario: Scripted helpers are attributed for IPC requests and scans: one whose parent is `A` with Claude's or Codex's export shape is `Owned`; a tool shell's is `NotOwned(Tool)`, a nested agent's or one with no identity file is `NotOwned(Unknown)`; an export outside the shape, an argv the request doesn't name, a learned generation or an unreadable cmdline is `NotOwned(Shape)`. A parent that can't be pinned or a helper reparented is `NotOwned(Unknown)`, and a helper that exits or lives in another time namespace is `Unattributable`.
#[spec("inbox/probe/068")]
#[test]
fn probe_068_the_attribution_steps() {
    let id = identity();
    let claude = agent(Some(&id), CLAUDE_KIND);
    let codex = agent(Some(&id), CODEX_KIND);

    // Owned: `A`'s own, by IPC and by scan, for both agents.
    let mut sys = world(A, CLAUDE_EXPORT);
    assert_eq!(
        attribution_of(&mut sys, &claude, CLAUDE_EXPORT),
        Attribution::Owned
    );
    let helper = pinned(&mut sys);
    assert_eq!(
        attribute(&mut sys, &claude, &helper, None).attribution(),
        Attribution::Owned
    );
    let mut sys = world(A, CODEX_EXPORT);
    assert_eq!(
        attribution_of(&mut sys, &codex, CODEX_EXPORT),
        Attribution::Owned
    );

    // Step 2: the class.
    let mut sys = world(SHELL, CLAUDE_EXPORT);
    assert_eq!(
        attribution_of(&mut sys, &claude, CLAUDE_EXPORT),
        Attribution::NotOwned(NotOwned::Tool)
    );
    let mut sys = world(NESTED, CLAUDE_EXPORT);
    assert_eq!(attribution_of(&mut sys, &claude, CLAUDE_EXPORT), UNKNOWN);
    let mut sys = world(A, CLAUDE_EXPORT);
    assert_eq!(
        attribution_of(&mut sys, &agent(None, CLAUDE_KIND), CLAUDE_EXPORT),
        UNKNOWN
    );

    // Step 3: the shape, and the request's argv.
    let shape = Attribution::NotOwned(NotOwned::Shape);
    let mut sys = world(A, "/tmp/elsewhere/claude-prompt-1.md");
    assert_eq!(
        attribution_of(&mut sys, &claude, "/tmp/elsewhere/claude-prompt-1.md"),
        shape
    );
    let mut sys = world(A, CLAUDE_EXPORT);
    assert_eq!(
        attribution_of(&mut sys, &claude, "/tmp/claude-1000/claude-prompt-2.md"),
        shape
    );
    let helper = pinned(&mut sys);
    let two = vec!["+1".to_owned(), CLAUDE_EXPORT.to_owned()];
    assert_eq!(
        attribute(&mut sys, &claude, &helper, Some(two.as_slice())).attribution(),
        shape
    );
    assert_eq!(
        attribute(&mut sys, &claude, &helper, Some(&[][..])).attribution(),
        shape
    );
    let mut sys = world(A, CODEX_EXPORT);
    assert_eq!(
        attribution_of(&mut sys, &claude, CODEX_EXPORT),
        shape,
        "Codex's export isn't Claude's shape"
    );
    assert_eq!(
        attribution_of(&mut sys, &agent(Some(&id), Kind::Learned), CODEX_EXPORT),
        shape
    );
    let mut sys = world(A, CLAUDE_EXPORT);
    sys.change(|p| p.get_mut(&H).unwrap().argv = None);
    assert_eq!(attribution_of(&mut sys, &claude, CLAUDE_EXPORT), shape);
    // A tool's helper is a tool whatever its argument.
    let mut sys = world(SHELL, "/home/me/notes.md");
    assert_eq!(
        attribution_of(&mut sys, &claude, "/home/me/notes.md"),
        Attribution::NotOwned(NotOwned::Tool)
    );

    // Step 1: `Q` can't be pinned, or the helper moved since it was pinned.
    let mut sys = world(A, CLAUDE_EXPORT);
    let helper = pinned(&mut sys);
    sys.change(|p| {
        p.remove(&A);
    });
    assert_eq!(
        attribute(&mut sys, &claude, &helper, None).attribution(),
        UNKNOWN
    );
    let mut sys = world(A, CLAUDE_EXPORT);
    let helper = pinned(&mut sys);
    sys.change(reparent_helper);
    assert_eq!(
        attribute(&mut sys, &claude, &helper, None).attribution(),
        UNKNOWN
    );
    let mut sys = world(A, CLAUDE_EXPORT);
    let helper = pinned(&mut sys);
    sys.change(exit_helper);
    assert_eq!(
        attribute(&mut sys, &claude, &helper, None).attribution(),
        Attribution::Unattributable
    );
    let mut sys = world(A, CLAUDE_EXPORT);
    let helper = pinned(&mut sys);
    sys.change(|p| {
        p.remove(&A);
        exit_helper(p);
    });
    assert_eq!(
        attribute(&mut sys, &claude, &helper, None).attribution(),
        Attribution::Unattributable
    );

    // Step 4: changes during the reads.
    for (change, expected) in [
        (exit_helper as Change, Attribution::Unattributable),
        (exit_a, UNKNOWN),
        (reparent_helper, UNKNOWN),
    ] {
        let mut sys = world(A, CLAUDE_EXPORT);
        sys.on(At::Cmdline, change);
        assert_eq!(attribution_of(&mut sys, &claude, CLAUDE_EXPORT), expected);
    }

    // The time namespace.
    let mut sys = world(A, CLAUDE_EXPORT);
    sys.change(|p| p.get_mut(&H).unwrap().ns = Ok(FileId { dev: 4, ino: 1 }));
    assert_eq!(
        attribution_of(&mut sys, &claude, CLAUDE_EXPORT),
        Attribution::Unattributable
    );
    for (daemon, helper, expected) in [
        (Err(Some(ENOENT)), Err(ENOENT), Attribution::Owned),
        (Ok(TIME_NS), Err(ENOENT), Attribution::Unattributable),
        (Err(Some(ENOENT)), Ok(TIME_NS), Attribution::Unattributable),
        (Err(Some(EACCES)), Err(EACCES), Attribution::Unattributable),
    ] {
        let mut sys = world(A, CLAUDE_EXPORT);
        sys.change(move |p| p.get_mut(&H).unwrap().ns = helper);
        let a = Agent {
            time_ns: daemon,
            ..agent(Some(&id), CLAUDE_KIND)
        };
        assert_eq!(
            attribution_of(&mut sys, &a, CLAUDE_EXPORT),
            expected,
            "{daemon:?} {helper:?}"
        );
    }
}

/// Scenario: The daemon reads an `Owned` invocation's export through the path rule's checked descriptor: Claude's and Codex's empty exports read `Empty`, a non-zero size or a byte read reads `NonEmpty`, a group-writable directory refuses, and a failed size or read is unreadable. A tool's invocation is returned as it was, with no file touched.
#[spec("inbox/probe/068")]
#[test]
fn probe_068_the_read() {
    let id = identity();
    let claude = agent(Some(&id), CLAUDE_KIND);
    let codex = agent(Some(&id), CODEX_KIND);

    let mut sys = world(A, CLAUDE_EXPORT);
    let mut fs = Fs::new(&sys);
    let empty = attest(&mut sys, &mut fs, &claude, CLAUDE_EXPORT);
    assert_eq!(empty, owned(Export::Empty));
    assert_eq!(empty.export.unwrap().read(), ExportRead::Empty);
    assert_eq!(sys.fs_calls(), ["open_root", "open_file", "size", "read"]);

    let mut sys = world(A, CODEX_EXPORT);
    let mut fs = Fs::new(&sys);
    assert_eq!(
        attest(&mut sys, &mut fs, &codex, CODEX_EXPORT),
        owned(Export::Empty)
    );

    let mut sys = world(A, CLAUDE_EXPORT);
    let mut fs = Fs::new(&sys);
    fs.at(CLAUDE_EXPORT).size = 3;
    let full = attest(&mut sys, &mut fs, &claude, CLAUDE_EXPORT);
    assert_eq!(full, owned(Export::NonEmpty));
    assert_eq!(full.export.unwrap().read(), ExportRead::NonEmpty);
    assert_eq!(sys.fs_calls(), ["open_root", "open_file", "size"]);

    let mut fs = Fs::new(&sys);
    fs.at(CLAUDE_EXPORT).reads = 1;
    assert_eq!(
        attest(&mut sys, &mut fs, &claude, CLAUDE_EXPORT),
        owned(Export::NonEmpty),
        "a size of 0 with a byte to read isn't empty"
    );

    let mut fs = Fs::new(&sys);
    fs.at("/tmp/claude-1000").mode = 0o770;
    let refused = attest(&mut sys, &mut fs, &claude, CLAUDE_EXPORT);
    let Some(Export::Refused(refusal)) = &refused.export else {
        panic!("{refused:?}");
    };
    assert_eq!(refusal.at, Path::new("/tmp/claude-1000"));
    assert!(matches!(refusal.cause, Cause::Writable { mode: 0o770, .. }));
    assert_eq!(refused.attribution, Attribution::Owned);
    assert_eq!(refused.export.unwrap().read(), ExportRead::Unreadable);

    let mut fs = Fs::new(&sys);
    fs.size_fails = Some(EIO);
    let failed = attest(&mut sys, &mut fs, &claude, CLAUDE_EXPORT);
    assert_eq!(
        failed,
        owned(Export::Failed {
            op: ReadOp::Size,
            errno: Some(EIO)
        })
    );
    assert_eq!(failed.export.unwrap().read(), ExportRead::Unreadable);
    let mut fs = Fs::new(&sys);
    fs.read_fails = Some(EIO);
    assert_eq!(
        attest(&mut sys, &mut fs, &claude, CLAUDE_EXPORT),
        owned(Export::Failed {
            op: ReadOp::Read,
            errno: Some(EIO)
        })
    );

    // Not `Owned`: no read, and the attribution as it was.
    for (parent, expected) in [
        (SHELL, Attribution::NotOwned(NotOwned::Tool)),
        (NESTED, UNKNOWN),
    ] {
        let mut sys = world(parent, CLAUDE_EXPORT);
        let mut fs = Fs::new(&sys);
        assert_eq!(
            attest(&mut sys, &mut fs, &claude, CLAUDE_EXPORT),
            Attested {
                attribution: expected,
                export: None,
                revoked: None
            }
        );
        assert!(sys.fs_calls().is_empty());
    }
    let mut sys = world(A, "/tmp/elsewhere.md");
    let mut fs = Fs::new(&sys);
    assert_eq!(
        attest(&mut sys, &mut fs, &claude, "/tmp/elsewhere.md").export,
        None
    );
    assert!(sys.fs_calls().is_empty());
}

/// Scenario: Between the first classification and the read, a scripted `A` exits, is reparented or `exec`s, or the helper exits or is reparented. An exit or a reparenting is caught before the export is opened, and an `exec` by the second classification after the read; each revokes the attribution and discards the read, while the unchanged control reads the export as `Owned`.
#[spec("inbox/probe/068")]
#[test]
fn probe_068_revalidation_between_the_first_classification_and_the_read() {
    let id = identity();
    let claude = agent(Some(&id), CLAUDE_KIND);
    let cases: [(Option<Change>, Attested, bool); 6] = [
        (None, owned(Export::Empty), true),
        (Some(exit_a), revoked(UNKNOWN, Revoked::AgentExited), false),
        (
            Some(reparent_a),
            revoked(UNKNOWN, Revoked::AgentChanged),
            false,
        ),
        (
            Some(exit_helper),
            revoked(Attribution::Unattributable, Revoked::HelperExited),
            false,
        ),
        (
            Some(reparent_helper),
            revoked(UNKNOWN, Revoked::HelperChanged),
            false,
        ),
        (
            Some(exec_a),
            revoked(UNKNOWN, Revoked::Class(Class::Unknown)),
            true,
        ),
    ];
    for (change, expected, read) in cases {
        let mut sys = world(A, CLAUDE_EXPORT);
        let mut fs = Fs::new(&sys);
        let helper = pinned(&mut sys);
        let attributed = attribute(
            &mut sys,
            &claude,
            &helper,
            Some(request(CLAUDE_EXPORT).as_slice()),
        );
        assert_eq!(attributed.attribution(), Attribution::Owned);
        if let Some(change) = change {
            sys.change(change);
        }
        assert_eq!(
            read_export(&mut sys, &mut fs, &claude, &helper, attributed),
            expected
        );
        assert_eq!(!sys.fs_calls().is_empty(), read, "{expected:?}");
    }
}

/// Scenario: During the export's read, a scripted `A` exits, is reparented or `exec`s, or the helper exits or is reparented; and during the second classification `A` exits. Each is caught after the read, by the pins, the stats or the second classification, revoking the attribution and discarding the read; the unchanged control reads the export as `Owned`.
#[spec("inbox/probe/068")]
#[test]
fn probe_068_revalidation_between_the_read_and_the_second_classification() {
    let id = identity();
    let claude = agent(Some(&id), CLAUDE_KIND);
    let cases: [(Option<Change>, Attested); 6] = [
        (None, owned(Export::Empty)),
        (Some(exit_a), revoked(UNKNOWN, Revoked::AgentExited)),
        (Some(reparent_a), revoked(UNKNOWN, Revoked::AgentChanged)),
        (
            Some(exit_helper),
            revoked(Attribution::Unattributable, Revoked::HelperExited),
        ),
        (
            Some(reparent_helper),
            revoked(UNKNOWN, Revoked::HelperChanged),
        ),
        (
            Some(exec_a),
            revoked(UNKNOWN, Revoked::Class(Class::Unknown)),
        ),
    ];
    for (change, expected) in cases {
        let mut sys = world(A, CLAUDE_EXPORT);
        let mut fs = Fs::new(&sys);
        if let Some(change) = change {
            sys.on(At::Size, change);
        }
        assert_eq!(attest(&mut sys, &mut fs, &claude, CLAUDE_EXPORT), expected);
        assert!(sys.fs_calls().contains(&"size"));
    }

    // `A` exits between the second classification's two samples: they
    // read alike, so only the pins checked after it catch the exit. `A`'s
    // stat is read once to pin it, twice by each classification and once
    // by the check of the pins before the read.
    let mut sys = world(A, CLAUDE_EXPORT);
    let mut fs = Fs::new(&sys);
    sys.on(At::Stat(A, 6), exit_a);
    assert_eq!(
        attest(&mut sys, &mut fs, &claude, CLAUDE_EXPORT),
        revoked(UNKNOWN, Revoked::AgentExited)
    );
    assert_eq!(sys.0.borrow().stats[&A], 6, "the exit came where scripted");
}

/// The running kernel's major and minor release.
fn kernel_release() -> (u32, u32) {
    // SAFETY: `utsname` is plain data, so all-zero is a valid value.
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: a writable `utsname`.
    assert_eq!(unsafe { libc::uname(&mut uts) }, 0);
    // SAFETY: `uname` NUL-terminates `release`.
    let release = unsafe { std::ffi::CStr::from_ptr(uts.release.as_ptr()) }.to_string_lossy();
    let mut parts = release.split(|c: char| !c.is_ascii_digit());
    let mut next = || parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (next(), next())
}

/// Scenario: Through the Linux views, this test process is pinned by its pid and, on Linux 6.5 and later, by a socket pair's `SO_PEERPIDFD`; it reads its own executable, argv and time namespace, a pid above Linux's limit opens no pidfd, and a child's pidfd reports the child's exit. An empty real file and one with bytes, opened as descriptors, read their sizes and first byte through them.
#[spec("inbox/probe/068")]
#[test]
fn probe_068_the_linux_views() {
    use dot_agent_deck::composer_probe::attest::{LinuxKernel, peer_pidfd};
    use dot_agent_deck::platform::fsperm::fs_view::LinuxFsView;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    let me = std::process::id();
    let mut kernel = LinuxKernel::default();
    let by_pid = pin(
        &mut kernel,
        Sighting::Pid {
            pid: me,
            start: None,
        },
    )
    .unwrap();
    assert_eq!(by_pid.pid, me);
    let (a, _b) = UnixStream::pair().unwrap();
    match peer_pidfd(&a).unwrap() {
        Some(pidfd) => {
            let by_pidfd = pin(&mut kernel, Sighting::Pidfd(pidfd)).unwrap();
            assert_eq!((by_pidfd.pid, by_pidfd.stat), (me, by_pid.stat));
        }
        None => {
            assert!(
                kernel_release() < (6, 5),
                "SO_PEERPIDFD is Linux 6.5's, and this is {:?}",
                kernel_release()
            );
            eprintln!("SKIP-PEERPIDFD: this kernel has no SO_PEERPIDFD");
        }
    }
    let exe = std::env::current_exe().unwrap();
    let meta = std::fs::metadata(&exe).unwrap();
    let exe_id = FileId {
        dev: std::os::unix::fs::MetadataExt::dev(&meta),
        ino: std::os::unix::fs::MetadataExt::ino(&meta),
    };
    assert_eq!(kernel.exe(me).unwrap(), exe_id);
    assert_eq!(kernel.exe_path(me).unwrap(), exe);
    assert_eq!(kernel.file(&exe).unwrap(), exe_id);
    assert!(
        kernel.pidfd_open(0x3fff_ffff).is_err(),
        "no pid is above Linux's limit"
    );
    assert_eq!(
        pin(
            &mut kernel,
            Sighting::Pid {
                pid: me,
                start: Some(by_pid.start() + 1),
            },
        )
        .map(|p| p.pid)
        .unwrap_err(),
        Unpinned::StartChanged
    );
    assert_eq!(
        kernel.cmdline(me).unwrap(),
        std::env::args_os().collect::<Vec<_>>()
    );
    let ns = |kernel: &mut LinuxKernel| kernel.time_ns(me).map_err(|e| e.raw_os_error());
    assert_eq!(ns(&mut kernel), ns(&mut kernel));

    let mut child = std::process::Command::new("true").spawn().unwrap();
    let fd = kernel.pidfd_open(child.id()).unwrap();
    child.wait().unwrap();
    assert!(kernel.exited(&fd).unwrap());
    assert!(!kernel.exited(by_pid.pidfd()).unwrap());

    let dir = test_temp::tempdir().unwrap();
    let mut fs = LinuxFsView;
    for (name, bytes, size, first) in [("empty.md", &b""[..], 0, 0), ("full.md", b"hi", 2, 1)] {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        let file = OwnedFd::from(std::fs::File::open(&path).unwrap());
        assert_eq!(fs.size(&file).unwrap(), size);
        let mut buf = [0u8; 1];
        assert_eq!(fs.read(&file, &mut buf).unwrap(), first);
        assert_eq!(fs.read(&file, &mut buf).unwrap(), first, "read at offset 0");
    }
}
