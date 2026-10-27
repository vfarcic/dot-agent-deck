//! PRD #1666 part 3 (Q35-04): the classifier of whose editor invocation it
//! is (`src/composer_probe/classifier.rs`). Most cases walk a scripted
//! process table, which can hold what a test can't arrange (an agent that
//! `exec`ed, an auto-updated descendant, a process that vanishes between
//! reads); the rest read real processes through the Linux view.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use dot_agent_deck::composer_probe::classifier::{
    Class, FileId, Identity, MAX_TOOL_STEPS, ProcStat, ProcView, classify,
};
use spec::spec;

const ENOENT: i32 = 2;
const ESRCH: i32 = 3;
const EACCES: i32 = 13;

/// Claude's native binary: F1, under F2.
const CLAUDE: FileId = FileId { dev: 8, ino: 4242 };
const VERSIONS: &str = "/home/me/.local/share/claude/versions";
/// F2's `(dev, ino)`: the `versions/` directory.
const VERSIONS_DIR: FileId = FileId { dev: 8, ino: 4000 };
const A: u32 = 100;
const A_START: u64 = 10_000;

#[derive(Debug, Clone)]
struct Proc {
    ppid: u32,
    start: u64,
    exe: Result<FileId, i32>,
    path: Result<PathBuf, i32>,
    stat: Option<i32>,
}

/// Processes by pid.
type Procs = BTreeMap<u32, Proc>;

/// Directories by path, as `stat` reads them. A path not listed reads as a
/// directory of its own, never F2.
type Dirs = BTreeMap<PathBuf, Result<FileId, i32>>;

/// An edit to a process table.
type Change = fn(&mut Procs);

/// A scripted process table and the directories above executables. From
/// the second read of the first pid it was asked about (the second pass),
/// it answers from `then` and `dirs_then` instead, where set: a tree that
/// changed while it was read.
#[derive(Debug, Clone)]
struct Table {
    now: Procs,
    then: Option<Procs>,
    dirs: Dirs,
    dirs_then: Option<Dirs>,
    first: Option<u32>,
    passes: usize,
}

impl Table {
    /// `init` (1), the deck's daemon (50) and the agent `A` (100), running
    /// Claude's binary from `versions/2.1.310`.
    fn new() -> Self {
        let mut table = Table {
            now: BTreeMap::new(),
            then: None,
            dirs: BTreeMap::from([(PathBuf::from(VERSIONS), Ok(VERSIONS_DIR))]),
            dirs_then: None,
            first: None,
            passes: 0,
        };
        table.spawn(
            1,
            0,
            1,
            FileId { dev: 8, ino: 2 },
            "/usr/lib/systemd/systemd",
        );
        table.spawn(
            50,
            1,
            5_000,
            FileId { dev: 8, ino: 7 },
            "/usr/bin/dot-agent-deck",
        );
        table.spawn(A, 50, A_START, CLAUDE, &format!("{VERSIONS}/2.1.310"));
        table
    }

    fn spawn(&mut self, pid: u32, ppid: u32, start: u64, exe: FileId, path: &str) {
        self.now.insert(
            pid,
            Proc {
                ppid,
                start,
                exe: Ok(exe),
                path: Ok(PathBuf::from(path)),
                stat: None,
            },
        );
    }

    /// A chain of processes under `parent`, pids from `first`, each started
    /// a tick after the one before, all running `exe` from `path`. Returns
    /// the last pid.
    fn chain(&mut self, parent: u32, first: u32, len: usize, exe: FileId, path: &str) -> u32 {
        let mut ppid = parent;
        for pid in first..first + len as u32 {
            let start = self.now[&ppid].start + 1;
            self.spawn(pid, ppid, start, exe, path);
            ppid = pid;
        }
        ppid
    }

    fn get(&mut self, pid: u32) -> io::Result<&mut Proc> {
        let table = match &mut self.then {
            Some(then) if self.passes > 1 => then,
            _ => &mut self.now,
        };
        table
            .get_mut(&pid)
            .ok_or_else(|| io::Error::from_raw_os_error(ENOENT))
    }
}

impl ProcView for Table {
    fn stat(&mut self, pid: u32) -> io::Result<ProcStat> {
        if *self.first.get_or_insert(pid) == pid {
            self.passes += 1;
        }
        let proc = self.get(pid)?;
        match proc.stat {
            Some(errno) => Err(io::Error::from_raw_os_error(errno)),
            None => Ok(ProcStat {
                ppid: proc.ppid,
                start: proc.start,
            }),
        }
    }

    fn exe(&mut self, pid: u32) -> io::Result<FileId> {
        self.get(pid)?.exe.map_err(io::Error::from_raw_os_error)
    }

    fn exe_path(&mut self, pid: u32) -> io::Result<PathBuf> {
        self.get(pid)?
            .path
            .clone()
            .map_err(io::Error::from_raw_os_error)
    }

    fn file(&mut self, path: &Path) -> io::Result<FileId> {
        let dirs = match &self.dirs_then {
            Some(then) if self.passes > 1 => then,
            _ => &self.dirs,
        };
        match dirs.get(path) {
            Some(read) => read.map_err(io::Error::from_raw_os_error),
            None => Ok(FileId {
                dev: 9,
                ino: path
                    .as_os_str()
                    .as_encoded_bytes()
                    .iter()
                    .fold(17, |ino: u64, &byte| ino.wrapping_mul(31) ^ u64::from(byte)),
            }),
        }
    }
}

fn identity() -> Identity {
    Identity {
        pid: A,
        start: A_START,
        exe: CLAUDE,
        install_root: PathBuf::from(VERSIONS),
        install_root_id: VERSIONS_DIR,
        basename: "2.1.310".into(),
    }
}

fn class(table: &Table, q: u32) -> Class {
    classify(&mut table.clone(), Some(&identity()), q)
}

const BASH: FileId = FileId { dev: 8, ino: 300 };
const GIT: FileId = FileId { dev: 8, ino: 301 };
const PYTHON: FileId = FileId { dev: 8, ino: 302 };

/// Scenario: Classify the agent's own invocations on a scripted process table: the editor's parent is the agent `A` itself, and `A` with its executable link reading `(deleted)` (the running inode is still `A`'s). Both are `Owned`, also when `A`'s install root has since moved, which only `A`'s descendants depend on.
#[spec("inbox/probe/006")]
#[test]
fn probe_006_the_agents_own_invocations_are_owned() {
    let table = Table::new();
    assert_eq!(class(&table, A), Class::Owned);

    let path = format!("{VERSIONS}/2.1.310");
    let mut replaced = Table::new();
    replaced.now.get_mut(&A).unwrap().path = Ok(format!("{path} (deleted)").into());
    assert_eq!(class(&replaced, A), Class::Owned);

    let mut moved = Table::new();
    moved.dirs.insert(VERSIONS.into(), Err(ENOENT));
    assert_eq!(class(&moved, A), Class::Owned);
}

/// Scenario: Every process on `A`'s executable other than `A` is `Unknown`, because the tree can't say how it came to run that executable: a tool's shell below `A` that `exec`ed the agent's binary (`A` → F1), the same two and three steps down (`A` → `bash` → F1 after `bash` `exec`ed it, and `A` → F1 → F1), M24's overview chain (`claude daemon run` → `bg-pty-host` → `--session-id`, each on `A`'s executable), which is the same tree, and a `git` that such a process runs, which is a tool. Before the fix each chain was `Owned`.
#[spec("inbox/probe/006")]
#[test]
fn probe_006_an_agent_process_other_than_a_is_unknown() {
    let path = format!("{VERSIONS}/2.1.310");
    let mut table = Table::new();
    // `bash -c 'claude …'` from a tool: bash `exec`s its one command.
    table.spawn(201, A, A_START + 1, CLAUDE, &path);
    assert_eq!(class(&table, 201), Class::Unknown, "a tool shell exec'd F1");
    // The shell `exec`ed F1 after it had run a child of its own.
    table.spawn(211, A, A_START + 1, CLAUDE, "/home/me/.local/bin/claude");
    table.spawn(212, 211, A_START + 2, CLAUDE, &path);
    assert_eq!(class(&table, 212), Class::Unknown, "below an exec'd shell");
    let session = table.chain(A, 300, 3, CLAUDE, &path);
    assert_eq!(
        class(&table, session),
        Class::Unknown,
        "M24's overview chain"
    );
    let long = table.chain(A, 400, 8, CLAUDE, &path);
    assert_eq!(class(&table, long), Class::Unknown, "eight steps");

    table.spawn(221, 201, A_START + 2, BASH, "/usr/bin/bash");
    table.spawn(222, 221, A_START + 3, GIT, "/usr/bin/git");
    assert_eq!(class(&table, 222), Class::PositivelyTool);
}

/// Scenario: The fake-tree mismatch controls: the editor's parent has `A`'s pid and start ticks but now runs another executable (after an `exec`, or a replaced binary read as `(deleted)`); a descendant runs a new binary in the install root (an auto-update's `versions/2.1.311`), directly below `A` or below an overview-style chain; a copy of the agent named `claude`, `codex` or `A`'s own basename elsewhere; and a process whose executable was deleted. Each is `Unknown`, never a tool.
#[spec("inbox/probe/006")]
#[test]
fn probe_006_an_executable_mismatch_is_unknown() {
    let other = FileId { dev: 8, ino: 9999 };
    let mut execed = Table::new();
    execed.now.get_mut(&A).unwrap().exe = Ok(other);
    execed.now.get_mut(&A).unwrap().path = Ok("/usr/bin/vim".into());
    assert_eq!(class(&execed, A), Class::Unknown);
    let mut replaced = Table::new();
    replaced.now.get_mut(&A).unwrap().exe = Ok(other);
    replaced.now.get_mut(&A).unwrap().path = Ok(format!("{VERSIONS}/2.1.310 (deleted)").into());
    assert_eq!(class(&replaced, A), Class::Unknown);

    let mut table = Table::new();
    table.spawn(201, A, A_START + 1, other, &format!("{VERSIONS}/2.1.311"));
    assert_eq!(class(&table, 201), Class::Unknown, "an updated descendant");
    let middle = table.chain(A, 210, 2, CLAUDE, &format!("{VERSIONS}/2.1.310"));
    table.spawn(
        220,
        middle,
        A_START + 10,
        other,
        &format!("{VERSIONS}/2.1.311"),
    );
    assert_eq!(
        class(&table, 220),
        Class::Unknown,
        "below the overview chain"
    );

    for (pid, path) in [
        (250, "/tmp/x/claude"),
        (251, "/opt/tools/codex"),
        (252, "/home/me/bin/2.1.310"),
        (253, "/opt/thing (deleted)"),
    ] {
        table.spawn(pid - 10, A, A_START + 1, BASH, "/usr/bin/bash");
        table.spawn(pid, pid - 10, A_START + 2, other, path);
        assert_eq!(class(&table, pid), Class::Unknown, "{path}");
    }
}

/// Scenario: A family record gone stale: `A` ran from `versions/2.1.310`, then the `versions` directory was renamed `versions-moved` and an updated descendant runs `versions-moved/2.1.311` from it (a new inode, a path outside the recorded root, a basename that isn't the agent's); the same with a new, different directory made at the old path; the root reached through another path whose directory is the root's own (a bind mount); the recorded root unreadable (`EACCES`); and a directory above the editor's parent's executable that doesn't read. Each is `Unknown`, as is a `git` below `A` while the root is moved or unreadable, which is a tool again once the root reads as recorded. Before the fix the moved-root descendant was a tool.
#[spec("inbox/probe/006")]
#[test]
fn probe_006_a_stale_install_root_is_unknown() {
    let updated = FileId { dev: 8, ino: 9999 };
    let moved_root = "/home/me/.local/share/claude/versions-moved";
    let mut table = Table::new();
    table.spawn(
        201,
        A,
        A_START + 1,
        updated,
        &format!("{moved_root}/2.1.311"),
    );
    table.spawn(202, A, A_START + 1, GIT, "/usr/bin/git");
    assert_eq!(class(&table, 202), Class::PositivelyTool);

    table.dirs.insert(VERSIONS.into(), Err(ENOENT));
    table.dirs.insert(moved_root.into(), Ok(VERSIONS_DIR));
    assert_eq!(class(&table, 201), Class::Unknown, "the root renamed");
    assert_eq!(
        class(&table, 202),
        Class::Unknown,
        "a tool, the root renamed"
    );

    let mut remade = table.clone();
    remade
        .dirs
        .insert(VERSIONS.into(), Ok(FileId { dev: 8, ino: 4001 }));
    assert_eq!(
        class(&remade, 201),
        Class::Unknown,
        "a new root at the path"
    );

    let mut alias = Table::new();
    alias.dirs.insert("/mnt/claude".into(), Ok(VERSIONS_DIR));
    alias.spawn(201, A, A_START + 1, updated, "/mnt/claude/2.1.311");
    assert_eq!(class(&alias, 201), Class::Unknown, "through a bind mount");

    let mut unreadable = Table::new();
    unreadable.spawn(202, A, A_START + 1, GIT, "/usr/bin/git");
    unreadable.dirs.insert(VERSIONS.into(), Err(EACCES));
    assert_eq!(
        class(&unreadable, 202),
        Class::Unknown,
        "the root unreadable"
    );
    unreadable.dirs.insert(VERSIONS.into(), Ok(VERSIONS_DIR));
    assert_eq!(class(&unreadable, 202), Class::PositivelyTool);
    unreadable.dirs.insert("/usr/bin".into(), Err(EACCES));
    assert_eq!(
        class(&unreadable, 202),
        Class::Unknown,
        "/usr/bin unreadable"
    );
}

/// Scenario: Nested agents: `A` runs `bash`, which runs another `claude` on `A`'s executable, and that one invokes the editor; the same with Python and Bash between them, as M28 measured a Bash tool starting a nested Claude on a usable PTY; and a nested Codex reached the same way. Each nested agent's invocation is `Unknown`, while a `git` run by the nested agent's own shell, outside the family and below `A`, is a tool.
#[spec("inbox/probe/006")]
#[test]
fn probe_006_a_nested_agent_is_unknown() {
    let path = format!("{VERSIONS}/2.1.310");
    let mut table = Table::new();
    table.spawn(201, A, A_START + 1, BASH, "/usr/bin/bash");
    table.spawn(202, 201, A_START + 2, CLAUDE, &path);
    assert_eq!(class(&table, 202), Class::Unknown);

    table.spawn(211, A, A_START + 1, PYTHON, "/usr/bin/python3.12");
    table.spawn(212, 211, A_START + 2, BASH, "/usr/bin/bash");
    table.spawn(213, 212, A_START + 3, CLAUDE, &path);
    assert_eq!(class(&table, 213), Class::Unknown, "M28's usable PTY");

    let codex = FileId { dev: 8, ino: 777 };
    table.spawn(
        214,
        212,
        A_START + 3,
        codex,
        "/home/me/.npm/lib/node_modules/@openai/codex/vendor/x86_64-unknown-linux-musl/bin/codex",
    );
    assert_eq!(class(&table, 214), Class::Unknown);

    table.spawn(215, 213, A_START + 4, BASH, "/usr/bin/bash");
    table.spawn(216, 215, A_START + 5, GIT, "/usr/bin/git");
    assert_eq!(class(&table, 216), Class::PositivelyTool);
}

/// Scenario: Tools: `git` run by `bash` below `A`, `git` run by `A` directly, and a tool 16 steps below `A` are positively tools. A tool 17 steps below, a `git` reparented to `init`, one reparented to the deck's daemon above `A` (a subreaper), one whose walk has a start-time inversion, and one below a process with `A`'s pid but a later start (the pid reused) are `Unknown`.
#[spec("inbox/probe/006")]
#[test]
fn probe_006_a_tool_needs_unbroken_ancestry_to_the_agent() {
    let mut table = Table::new();
    table.spawn(201, A, A_START + 1, BASH, "/usr/bin/bash");
    table.spawn(202, 201, A_START + 2, GIT, "/usr/bin/git");
    assert_eq!(class(&table, 202), Class::PositivelyTool);
    table.spawn(203, A, A_START + 1, GIT, "/usr/bin/git");
    assert_eq!(class(&table, 203), Class::PositivelyTool);

    let deep = table.chain(A, 300, MAX_TOOL_STEPS, BASH, "/usr/bin/bash");
    assert_eq!(class(&table, deep), Class::PositivelyTool);
    let deeper = table.chain(A, 400, MAX_TOOL_STEPS + 1, BASH, "/usr/bin/bash");
    assert_eq!(class(&table, deeper), Class::Unknown);

    table.spawn(501, 1, A_START + 50, GIT, "/usr/bin/git");
    assert_eq!(class(&table, 501), Class::Unknown, "reparented to init");
    table.spawn(502, 50, A_START + 50, GIT, "/usr/bin/git");
    assert_eq!(
        class(&table, 502),
        Class::Unknown,
        "reparented to a subreaper"
    );

    table.spawn(511, A, A_START + 1, BASH, "/usr/bin/bash");
    table.spawn(512, 511, A_START, GIT, "/usr/bin/git");
    assert_eq!(class(&table, 512), Class::Unknown, "born before its parent");

    let mut reused = Table::new();
    reused.now.get_mut(&A).unwrap().start = A_START + 500;
    reused.now.get_mut(&A).unwrap().exe = Ok(BASH);
    reused.now.get_mut(&A).unwrap().path = Ok("/usr/bin/bash".into());
    reused.spawn(201, A, A_START + 501, GIT, "/usr/bin/git");
    assert_eq!(class(&reused, 201), Class::Unknown, "A's pid reused");
    assert_eq!(class(&reused, A), Class::Unknown);
}

/// Scenario: What can't be read: no identity; the editor's parent whose stat, executable or executable link can't be read, or that has vanished; a process on a tool's walk whose stat fails (`ESRCH`, `EACCES`); and the tree changing between the two reads (a `bash` reparented, a process gone, a start changed, the `bash` or the `git` reading another executable or another executable path, a `bash` whose executable stopped reading with `EACCES` and then reads `ENOENT`, as a zombie's does, the install root read as another directory, and a directory above the `git` read as another or no longer readable). Each is `Unknown`, while the deck's daemon above `A` being reparented between the reads leaves a tool a tool, since the walk stops at `A`. A process on a tool's walk whose executable alone can't be read still leaves the tool a tool.
#[spec("inbox/probe/006")]
#[test]
fn probe_006_what_cant_be_read_is_unknown() {
    let mut table = Table::new();
    table.spawn(201, A, A_START + 1, BASH, "/usr/bin/bash");
    table.spawn(202, 201, A_START + 2, GIT, "/usr/bin/git");
    assert_eq!(classify(&mut table.clone(), None, A), Class::Unknown);
    assert_eq!(classify(&mut table.clone(), None, 202), Class::Unknown);
    assert_eq!(class(&table, 999), Class::Unknown, "gone");

    for q in [A, 202] {
        let mut stat = table.clone();
        stat.now.get_mut(&q).unwrap().stat = Some(EACCES);
        assert_eq!(class(&stat, q), Class::Unknown, "{q}'s stat");
        let mut exe = table.clone();
        exe.now.get_mut(&q).unwrap().exe = Err(EACCES);
        assert_eq!(class(&exe, q), Class::Unknown, "{q}'s exe");
        let mut path = table.clone();
        path.now.get_mut(&q).unwrap().path = Err(ESRCH);
        assert_eq!(class(&path, q), Class::Unknown, "{q}'s exe link");
    }
    for errno in [ESRCH, EACCES] {
        let mut broken = table.clone();
        broken.now.get_mut(&201).unwrap().stat = Some(errno);
        assert_eq!(class(&broken, 202), Class::Unknown, "{errno}");
    }

    let mut hidden = table.clone();
    hidden.now.get_mut(&201).unwrap().exe = Err(EACCES);
    hidden.now.get_mut(&201).unwrap().path = Err(EACCES);
    assert_eq!(class(&hidden, 202), Class::PositivelyTool);

    let changes: [(&str, Change); 7] = [
        ("bash reparented", |then| {
            then.get_mut(&201).unwrap().ppid = 1
        }),
        ("bash gone", |then| drop(then.remove(&201))),
        ("a start changed", |then| {
            then.get_mut(&202).unwrap().start += 1;
        }),
        ("bash exec'd another binary", |then| {
            then.get_mut(&201).unwrap().exe = Ok(PYTHON);
        }),
        ("bash's link moved", |then| {
            then.get_mut(&201).unwrap().path = Ok("/usr/bin/sh".into());
        }),
        ("git exec'd another binary", |then| {
            then.get_mut(&202).unwrap().exe = Ok(BASH);
        }),
        ("git's link moved", |then| {
            then.get_mut(&202).unwrap().path = Ok("/usr/local/bin/git".into());
        }),
    ];
    for (label, change) in changes {
        let mut changing = table.clone();
        let mut then = changing.now.clone();
        change(&mut then);
        changing.then = Some(then);
        assert_eq!(class(&changing, 202), Class::Unknown, "{label}");
    }
    let mut unchanged = table.clone();
    unchanged.then = Some(unchanged.now.clone());
    assert_eq!(class(&unchanged, 202), Class::PositivelyTool);
    let mut zombie = table.clone();
    zombie.now.get_mut(&201).unwrap().exe = Err(EACCES);
    let mut then = zombie.now.clone();
    then.get_mut(&201).unwrap().exe = Err(ENOENT);
    zombie.then = Some(then);
    assert_eq!(class(&zombie, 202), Class::Unknown, "bash became a zombie");
    // The walk stops at `A`: what changes above it doesn't count.
    let mut above = table.clone();
    let mut then = above.now.clone();
    then.get_mut(&50).unwrap().ppid = 7;
    above.then = Some(then);
    assert_eq!(class(&above, 202), Class::PositivelyTool);

    for (label, path, read) in [
        (
            "the root replaced",
            VERSIONS,
            Ok(FileId { dev: 8, ino: 4001 }),
        ),
        (
            "/usr/bin replaced",
            "/usr/bin",
            Ok(FileId { dev: 8, ino: 5 }),
        ),
        ("/usr/bin unreadable", "/usr/bin", Err(EACCES)),
    ] {
        let mut changing = table.clone();
        let mut then = changing.dirs.clone();
        then.insert(path.into(), read);
        changing.dirs_then = Some(then);
        assert_eq!(class(&changing, 202), Class::Unknown, "{label}");
    }
}

/// Scenario: Codex's layout: a Node launcher above the native `codex` binary, which is `A`, from the npm package root. The editor's parent is `A` itself (`Owned`); a descendant running a different binary from the same package root is `Unknown`; a `git` from `A`'s shell tool is a tool; and the Node launcher above `A` is `Unknown`.
#[spec("inbox/probe/006")]
#[test]
fn probe_006_codex_below_its_node_launcher() {
    let root = "/home/me/.npm/lib/node_modules/@openai/codex";
    let codex = FileId { dev: 8, ino: 777 };
    let identity = Identity {
        pid: 120,
        start: 12_000,
        exe: codex,
        install_root: PathBuf::from(root),
        install_root_id: FileId { dev: 8, ino: 4100 },
        basename: "codex".into(),
    };
    let mut table = Table::new();
    table.dirs.insert(root.into(), Ok(identity.install_root_id));
    table.spawn(
        110,
        50,
        11_000,
        FileId { dev: 8, ino: 600 },
        "/usr/bin/node",
    );
    table.spawn(
        120,
        110,
        12_000,
        codex,
        &format!("{root}/vendor/x86_64-unknown-linux-musl/bin/codex"),
    );
    let class = |table: &Table, q| classify(&mut table.clone(), Some(&identity), q);
    assert_eq!(class(&table, 120), Class::Owned);
    table.spawn(
        121,
        120,
        12_001,
        FileId { dev: 8, ino: 778 },
        &format!("{root}/vendor/x86_64-unknown-linux-musl/path/rg"),
    );
    assert_eq!(class(&table, 121), Class::Unknown);
    table.spawn(122, 120, 12_001, BASH, "/usr/bin/bash");
    table.spawn(123, 122, 12_002, GIT, "/usr/bin/git");
    assert_eq!(class(&table, 123), Class::PositivelyTool);
    assert_eq!(class(&table, 110), Class::Unknown);
}

/// Real processes, through the Linux view.
#[cfg(target_os = "linux")]
mod real {
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command};

    use dot_agent_deck::composer_probe::classifier::{
        Class, FileId, Identity, ProcView as _, classify,
    };
    use dot_agent_deck::platform::proc::{LinuxProcView, process_start_of};
    use spec::spec;

    fn file_id(path: &Path) -> FileId {
        let metadata = fs::metadata(path).unwrap();
        FileId {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    /// Under the target directory, where the copies can be executed. Cargo
    /// creates it when it builds, but a test run from a nextest archive (CI's
    /// e2e shards) extracts `target/` without it, so it is created here.
    fn tempdir() -> tempfile::TempDir {
        fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).unwrap();
        tempfile::Builder::new()
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap()
    }

    /// A `sleep` that runs as `sleep` under any name. uutils' multi-call
    /// binary picks its program from its own file name, so where `/bin/sleep`
    /// is that, GNU's copy is used; [`Copy::running`] catches a host with
    /// neither.
    fn sleep() -> &'static str {
        ["/usr/bin/gnusleep", "/bin/sleep"]
            .into_iter()
            .find(|path| Path::new(path).exists())
            .unwrap()
    }

    /// `sleep`, copied to `name` in a fresh directory under the target
    /// directory, or in a directory of the test's, started for a minute.
    struct Copy {
        _dir: Option<tempfile::TempDir>,
        path: PathBuf,
        child: Child,
    }

    impl Copy {
        fn start(name: impl AsRef<OsStr>) -> Copy {
            let dir = tempdir();
            let mut copy = Copy::start_in(dir.path(), name);
            copy._dir = Some(dir);
            copy
        }

        fn start_in(dir: &Path, name: impl AsRef<OsStr>) -> Copy {
            let path = dir.join(name.as_ref());
            fs::copy(sleep(), &path).unwrap();
            // Another test thread forking while the copy was open for writing
            // holds it open until its child execs: `ETXTBSY` until then.
            let mut tries = 0;
            let child = loop {
                match Command::new(&path).arg("60").spawn() {
                    Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && tries < 100 => {
                        tries += 1;
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    spawned => break spawned.unwrap(),
                }
            };
            Copy {
                _dir: None,
                path,
                child,
            }
        }

        fn pid(&self) -> u32 {
            self.child.id()
        }

        /// Whether the copy is still running, not exited and unreaped.
        fn running(&mut self) -> bool {
            self.child.try_wait().unwrap().is_none()
        }
    }

    impl Drop for Copy {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// This test process as the agent `A`.
    fn me() -> Identity {
        let exe = std::env::current_exe().unwrap();
        Identity {
            pid: std::process::id(),
            start: process_start_of(std::process::id()).unwrap(),
            exe: file_id(&exe),
            install_root: exe.parent().unwrap().to_owned(),
            install_root_id: file_id(exe.parent().unwrap()),
            basename: exe.file_name().unwrap().to_owned(),
        }
    }

    /// Scenario: Read real processes through the Linux view: this test process's parent, start ticks, executable and its link match what the process and its parent report; a child started from a copy of `sleep` named `a) b (c 1 2` reads this process as its parent, a start at or after this process's, and the copy's inode and path, and after the copy is deleted its link reads `(deleted)` with the same inode; once the child is reaped, its stat fails. A copy whose name holds a newline among parentheses and digits reads this process as its parent too, while one whose name isn't UTF-8 has a stat that doesn't read, and classifies `Unknown`. The view reads a directory's `(dev, ino)` as `stat` does.
    #[spec("inbox/probe/006")]
    #[test]
    fn probe_006_the_linux_view_reads_proc() {
        let mut view = LinuxProcView;
        let me = std::process::id();
        let stat = view.stat(me).unwrap();
        // SAFETY: `getppid` has no preconditions.
        assert_eq!(stat.ppid, unsafe { libc::getppid() } as u32);
        assert_eq!(Some(stat.start), process_start_of(me));
        let exe = std::env::current_exe().unwrap();
        assert_eq!(view.exe(me).unwrap(), file_id(&exe));
        assert_eq!(view.exe_path(me).unwrap(), exe);

        let mut copy = Copy::start("a) b (c 1 2");
        let pid = copy.pid();
        let child = view.stat(pid).unwrap();
        assert_eq!(child.ppid, me);
        assert!(child.start >= stat.start);
        let id = file_id(&copy.path);
        assert_eq!(view.exe(pid).unwrap(), id);
        assert_eq!(view.exe_path(pid).unwrap(), copy.path);
        fs::remove_file(&copy.path).unwrap();
        let mut deleted = copy.path.clone().into_os_string();
        deleted.push(" (deleted)");
        assert_eq!(view.exe_path(pid).unwrap(), PathBuf::from(deleted));
        assert_eq!(view.exe(pid).unwrap(), id);

        copy.child.kill().unwrap();
        copy.child.wait().unwrap();
        assert!(view.stat(pid).is_err());

        let newline = Copy::start("n\n) 4 (5\n6");
        assert_eq!(view.stat(newline.pid()).unwrap().ppid, me);
        let bytes = Copy::start(OsStr::from_bytes(b"x\xff) 7 8"));
        assert!(view.stat(bytes.pid()).is_err());
        let git = Copy::start("git");
        let identity = self::me();
        assert_eq!(
            classify(&mut view, Some(&identity), git.pid()),
            Class::PositivelyTool
        );
        assert_eq!(
            classify(&mut view, Some(&identity), bytes.pid()),
            Class::Unknown
        );
        let (mut newline, mut bytes, mut git) = (newline, bytes, git);
        assert!(newline.running() && bytes.running() && git.running());

        let dir = tempdir();
        assert_eq!(view.file(dir.path()).unwrap(), file_id(dir.path()));
        assert!(view.file(&dir.path().join("missing")).is_err());
    }

    /// Scenario: A real family record gone stale. With this test process standing in as the agent and a `versions` directory of the test's recorded as its install root, a copy of `sleep` named `2.1.311` started from that directory is `Unknown` (in the family), while a copy named `git` elsewhere is a tool. Once `versions` is renamed `versions-moved`, the `2.1.311` copy, its path now outside the recorded root, and the `git` copy are both `Unknown`. Renamed back, `git` is a tool again.
    #[spec("inbox/probe/006")]
    #[test]
    fn probe_006_a_real_moved_install_root_is_unknown() {
        let dir = tempdir();
        let versions = dir.path().join("versions");
        fs::create_dir(&versions).unwrap();
        let moved = dir.path().join("versions-moved");
        let identity = Identity {
            install_root: versions.clone(),
            install_root_id: file_id(&versions),
            ..me()
        };
        let mut view = LinuxProcView;
        let updated = Copy::start_in(&versions, "2.1.311");
        let git = Copy::start("git");
        let class =
            |view: &mut LinuxProcView, copy: &Copy| classify(view, Some(&identity), copy.pid());
        assert_eq!(class(&mut view, &updated), Class::Unknown);
        assert_eq!(class(&mut view, &git), Class::PositivelyTool);

        fs::rename(&versions, &moved).unwrap();
        assert_eq!(view.exe_path(updated.pid()).unwrap(), moved.join("2.1.311"));
        assert_eq!(class(&mut view, &updated), Class::Unknown, "the root moved");
        assert_eq!(
            class(&mut view, &git),
            Class::Unknown,
            "a tool, the root moved"
        );

        fs::rename(&moved, &versions).unwrap();
        assert_eq!(class(&mut view, &git), Class::PositivelyTool);
        let (mut updated, mut git) = (updated, git);
        assert!(updated.running() && git.running());
    }

    /// Scenario: Classify real processes, with this test process standing in as the agent: itself is `Owned`; a child running a copy of `sleep` named `git` is a tool; a copy named `claude`, or named as this test binary, is `Unknown`; and with no identity, `Unknown`.
    #[spec("inbox/probe/006")]
    #[test]
    fn probe_006_real_processes_classify() {
        let me = me();
        let mut view = LinuxProcView;
        assert_eq!(classify(&mut view, Some(&me), me.pid), Class::Owned);
        let git = Copy::start("git");
        assert_eq!(
            classify(&mut view, Some(&me), git.pid()),
            Class::PositivelyTool
        );
        assert_eq!(classify(&mut view, None, git.pid()), Class::Unknown);
        let claude = Copy::start("claude");
        assert_eq!(classify(&mut view, Some(&me), claude.pid()), Class::Unknown);
        let mut twin = Copy::start(me.basename.to_str().unwrap());
        assert_eq!(classify(&mut view, Some(&me), twin.pid()), Class::Unknown);
        // Each class above was of a live process, not one that had exited.
        let mut git = git;
        let mut claude = claude;
        for copy in [&mut git, &mut claude, &mut twin] {
            assert!(copy.running(), "{}", copy.path.display());
        }
    }
}
