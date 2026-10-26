//! PRD #1666 part 3 (Q35-04): the classifier of whose editor invocation it
//! is (`src/composer_probe/classifier.rs`). Most cases walk a scripted
//! process table, which can hold what a test can't arrange (an agent that
//! `exec`ed, an auto-updated descendant, a process that vanishes between
//! reads); the rest read real processes through the Linux view.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use dot_agent_deck::composer_probe::classifier::{
    Class, FileId, Identity, MAX_OWNED_STEPS, MAX_TOOL_STEPS, ProcStat, ProcView, classify,
};
use spec::spec;

const ENOENT: i32 = 2;
const ESRCH: i32 = 3;
const EACCES: i32 = 13;

/// Claude's native binary: F1, under F2.
const CLAUDE: FileId = FileId { dev: 8, ino: 4242 };
const VERSIONS: &str = "/home/me/.local/share/claude/versions";
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

/// An edit to a process table.
type Change = fn(&mut Procs);

/// A scripted process table. From the second read of the first pid it was
/// asked about (the walk's second pass), it answers from `then` instead: a
/// tree that changed while it was read.
#[derive(Debug, Clone)]
struct Table {
    now: Procs,
    then: Option<Procs>,
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
}

fn identity() -> Identity {
    Identity {
        pid: A,
        start: A_START,
        exe: CLAUDE,
        install_root: PathBuf::from(VERSIONS),
        basename: "2.1.310".into(),
    }
}

fn class(table: &Table, q: u32) -> Class {
    classify(&mut table.clone(), Some(&identity()), q)
}

const BASH: FileId = FileId { dev: 8, ino: 300 };
const GIT: FileId = FileId { dev: 8, ino: 301 };
const PYTHON: FileId = FileId { dev: 8, ino: 302 };

/// Scenario: Classify the agent's own invocations on a scripted process table: the editor's parent is the agent `A` itself, a process running `A`'s executable whose link reads `(deleted)` (the running inode is still `A`'s), M24's overview chain (`claude daemon run` → `bg-pty-host` → `--session-id`, each running `A`'s executable), and a chain of exactly 8 such steps. Each is `Owned`.
#[spec("inbox/probe/006")]
#[test]
fn probe_006_the_agents_own_invocations_are_owned() {
    let mut table = Table::new();
    assert_eq!(class(&table, A), Class::Owned);

    let path = format!("{VERSIONS}/2.1.310");
    let session = table.chain(A, 200, 3, CLAUDE, &path);
    assert_eq!(class(&table, session), Class::Owned);

    let eighth = table.chain(A, 300, MAX_OWNED_STEPS, CLAUDE, &path);
    assert_eq!(class(&table, eighth), Class::Owned);

    let mut replaced = Table::new();
    replaced.now.get_mut(&A).unwrap().path = Ok(format!("{path} (deleted)").into());
    assert_eq!(class(&replaced, A), Class::Owned);
}

/// Scenario: The fake-tree mismatch controls: the editor's parent has `A`'s pid and start ticks but now runs another executable (after an `exec`, or a replaced binary read as `(deleted)`); a descendant runs a new binary in the install root (an auto-update's `versions/2.1.311`); a copy of the agent named `claude`, `codex` or `A`'s own basename elsewhere; a process whose executable was deleted; a chain of `A`'s executable one step past 8; and an overview-style chain with a start-time inversion. Each is `Unknown`, never a tool.
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

    let path = format!("{VERSIONS}/2.1.310");
    let ninth = table.chain(A, 400, MAX_OWNED_STEPS + 1, CLAUDE, &path);
    assert_eq!(class(&table, ninth), Class::Unknown);

    let session = table.chain(A, 500, 3, CLAUDE, &path);
    table.now.get_mut(&501).unwrap().start = A_START - 1;
    assert_eq!(class(&table, session), Class::Unknown);
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

/// Scenario: What can't be read: no identity; the editor's parent whose stat, executable or executable link can't be read, or that has vanished; a process on a tool's walk whose stat fails (`ESRCH`, `EACCES`); and the tree changing between the walk's two reads (a `bash` reparented, a process gone, a start changed, or a `bash` whose executable stopped reading with `EACCES` and then reads `ENOENT`, as a zombie's does). Each is `Unknown`, while the deck's daemon above `A` being reparented between the reads leaves a tool a tool, since the walk stops at `A`. A process on a tool's walk whose executable alone can't be read still leaves the tool a tool, and the same on an overview chain makes it `Unknown`.
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
    let path = format!("{VERSIONS}/2.1.310");
    let session = table.chain(A, 300, 3, CLAUDE, &path);
    assert_eq!(class(&table, session), Class::Owned);
    let mut hidden = table.clone();
    hidden.now.get_mut(&301).unwrap().exe = Err(EACCES);
    assert_eq!(class(&hidden, session), Class::Unknown);

    let changes: [(&str, Change); 3] = [
        ("bash reparented", |then| {
            then.get_mut(&201).unwrap().ppid = 1
        }),
        ("bash gone", |then| drop(then.remove(&201))),
        ("a start changed", |then| {
            then.get_mut(&202).unwrap().start += 1;
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
        basename: "codex".into(),
    };
    let mut table = Table::new();
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
    use std::fs;
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

    /// `sleep`, copied to `name` in a fresh directory under the target
    /// directory, started for a minute.
    struct Copy {
        _dir: tempfile::TempDir,
        path: PathBuf,
        child: Child,
    }

    impl Copy {
        fn start(name: &str) -> Copy {
            let dir = tempfile::Builder::new()
                .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
                .unwrap();
            let path = dir.path().join(name);
            fs::copy("/bin/sleep", &path).unwrap();
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
                _dir: dir,
                path,
                child,
            }
        }

        fn pid(&self) -> u32 {
            self.child.id()
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
            basename: exe.file_name().unwrap().to_owned(),
        }
    }

    /// Scenario: Read real processes through the Linux view: this test process's parent, start ticks, executable and its link match what the process and its parent report; a child started from a copy of `sleep` named `a) b (c 1 2` reads this process as its parent, a start at or after this process's, and the copy's inode and path, and after the copy is deleted its link reads `(deleted)` with the same inode; once the child is reaped, its stat fails.
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
        let twin = Copy::start(me.basename.to_str().unwrap());
        assert_eq!(classify(&mut view, Some(&me), twin.pid()), Class::Unknown);
    }
}
