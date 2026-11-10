//! PRD #1666 part 3 (Q35-22; Q28(b)'s D1 resolver as Q31 and Q102b made it
//! concrete): the shared discovery of a generation's agent `A` below the PTY
//! child the deck spawned, for every installed class but a direct Claude
//! (whose `A` comes from the deck's own launch record, `observe::bound_agent`).
//!
//! **The pins** ([`pin_at_spawn`]) are one resolution, at spawn, before the
//! child exists, from the `PATH` the deck hands the PTY child: a direct
//! pane's program word, a launcher's agent name, and an undeclared wrapper's
//! two agent names, each pinned independently. A name resolves as a shell
//! would find it on that `PATH`, passing over the deck's own telemetry shims
//! (`crate::telemetry::shim::is_shim_file`), which `exec` the real program,
//! and symlinks are followed to a kernel identity `(dev, ino)`, read from the
//! file the pin opened (`fstat`), as its shebang is. Nothing later looks the
//! agent's name up again; the one later lookup is an `env NAME` shebang's
//! interpreter, below. A native candidate is checked by its `/proc/<pid>/exe`
//! against the pinned pair, so a pane started before an agent update keeps
//! its old inode, a wrapper that `exec`s a different inode after the pin is
//! refused, and a pane spawned after the update pins the new one. A name
//! that doesn't resolve stays unresolved: no candidate's claim stands in for
//! it.
//!
//! **Interpreter-hosted entrypoints** (plan Q35-14b's npm decision): when the
//! pinned file is a script whose shebang names an interpreter (npm Codex's
//! `#!/usr/bin/env node` `codex.js`, reached through a symlink), the pin also
//! carries that interpreter, resolved at spawn as the kernel and `env` would,
//! and the argv the kernel and `env` give it (`interpreter [+ its argument] +
//! the script path as found`). A process running that interpreter with that
//! argv is an attested entrypoint, and `A` is the native agent it spawns as
//! its own child: that native is on no `PATH`, so the entrypoint's provenance
//! is what attests it. Another script under the same interpreter, or the
//! script under another interpreter, isn't an entrypoint, and a native below
//! it is refused as an executable mismatch.
//!
//! **An `env NAME` shebang's interpreter** (issue #2258) is whatever `env`
//! finds as `NAME` on the `PATH` of the process it runs in, and a launcher
//! sets that `PATH` after the spawn: `devbox run`, `nix develop`, `mise` and
//! `nvm` put their own `node` first, so the spawn's resolution names another
//! inode than the entrypoint runs. Such an entrypoint is also attested when
//! its executable is what `NAME` resolves to, the same way, on its own
//! `PATH` as its environment began (`/proc/<pid>/environ`), read when the
//! entrypoint is checked. An interpreter the entrypoint runs from anywhere
//! else (another inode on no directory of that `PATH`, or one behind an
//! earlier `NAME` there) is still refused. This widens nothing the windows
//! below leave open: a process that can set the entrypoint's `PATH` controls
//! its environment, and so its `NODE_OPTIONS`, which nothing compares.
//!
//! **What holds for the hosted route, and what doesn't.** The entrypoint's
//! executable is the interpreter's, so it says nothing about which script
//! the interpreter read. Validation therefore also requires the script's
//! path to hold the pinned file still: a script replaced after the pin (an
//! npm update, or any writer of that directory) refuses the native as an
//! executable mismatch. That read is a refusal only; it never attests
//! anything. The pin compares file identities, and what it compares has
//! three windows, each failing open only to a process that can already
//! write the agent's installation as this user:
//! - The path is read at validation, not when the interpreter opened the
//!   script, so a file swapped in before the interpreter's read and swapped
//!   back before validation isn't seen.
//! - A pin is a file identity, not its content: a write into the pinned
//!   file itself, keeping its inode, isn't seen. That holds for a native's
//!   pin too.
//! - The native below an attested entrypoint is attested by that parentage
//!   alone: its own executable is compared with nothing. Where it lives is
//!   the script's choice at run time (npm's `codex.js` resolves a platform
//!   package, which npm may hoist), so there is no spawn-time fact to pin it
//!   to, and a writer of the package's directory can replace it.
//!
//! Code the attested processes load from elsewhere, or through their
//! environment, is not compared at all: the modules `codex.js` requires
//! before it spawns the native, and a preload such as
//! `NODE_OPTIONS=--require …` on the interpreter (which the argv comparison
//! never sees) or `LD_PRELOAD` on a native pin. That route needs write
//! access to the pane's launcher or environment, not to the installation.
//!
//! **The walk** ([`find`]) goes down from the PTY child, breadth first and
//! bounded, and takes the first process named as an agent (its `argv[0]`'s
//! or its executable's basename is `claude` or `codex`). Below the deck's own
//! `wrap` it passes over a child still running the deck's executable (forked,
//! not yet `exec`'d) and the wrapper's `codex app-server` trust probe.
//!
//! **Validation** ([`validate`]), after the candidate is found, reads it again
//! and refuses, in this order, a candidate that exited, one that left the
//! parent it was found under (or whose ancestry to the PTY child changed),
//! one another user owns, and one whose executable is neither the pin nor a
//! child of a still-attested entrypoint whose script path still holds the
//! pinned file. Lifecycle and owner refusals come before an executable
//! mismatch, and a kind claim only changes which pin is expected, never what
//! attests the candidate. [`validate_held`] then reads the candidate again
//! before and after the pidfd poll: both reads must see the executable
//! validation accepted, which is the one the identity publishes.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::classifier::{AGENT_BASENAMES, FileId};
use super::install::{PaneClass, ProbeAgent};
use crate::event::AgentType;

/// The most processes one walk reads.
pub const WALK_PROCESSES: usize = 256;

/// The deepest a walk goes below the PTY child.
pub const WALK_DEPTH: usize = 16;

/// The most bytes of a pinned file read for its shebang line.
const SHEBANG_BYTES: usize = 256;

/// What Linux appends to `/proc/<pid>/exe`'s target once the file is gone.
const DELETED: &[u8] = b" (deleted)";

/// An agent's executable as pinned at spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    /// The path the `PATH` lookup found (or the program word, when it was a
    /// path), as the kernel is handed it: a hosted entrypoint's script
    /// argument is compared with it.
    pub path: PathBuf,
    /// What that path resolves to, symlinks followed.
    pub exe: FileId,
    /// When the file is an interpreter-hosted script: its interpreter.
    pub hosted: Option<Hosted>,
}

/// A pinned script's interpreter, from its shebang line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hosted {
    /// The interpreter's executable, symlinks followed.
    pub interpreter: FileId,
    /// The argv in front of the script path: `env`'s program word, or the
    /// shebang's interpreter path and its one argument.
    pub prefix: Vec<OsString>,
}

impl Hosted {
    /// The `NAME` of an `env NAME` shebang, which `env` looks up on the
    /// `PATH` of the process it runs in; `None` for a shebang that names
    /// its interpreter by path. [`hosted_by`] gives an `env` shebang its
    /// name alone as the prefix, and any other its interpreter's absolute
    /// path first.
    fn env_name(&self) -> Option<&OsStr> {
        use std::os::unix::ffi::OsStrExt;
        match self.prefix.as_slice() {
            [name] if !name.as_bytes().contains(&b'/') => Some(name),
            _ => None,
        }
    }
}

/// The pins a generation's spawn took: one per agent its class may run.
/// `None` is a name that didn't resolve, or one the class doesn't run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pins {
    pub claude: Option<Pin>,
    pub codex: Option<Pin>,
}

impl Pins {
    /// `agent`'s pin, if one resolved.
    pub fn of(&self, agent: ProbeAgent) -> Option<&Pin> {
        match agent {
            ProbeAgent::Claude => self.claude.as_ref(),
            ProbeAgent::Codex => self.codex.as_ref(),
        }
    }
}

/// Why discovery refused a generation's candidate. Each is final for the
/// generation's life: its permission settles inert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The expected agent's name resolved nothing at spawn.
    NotResolved(ProbeAgent),
    /// The candidate's executable is neither the pin nor a native below an
    /// attested entrypoint.
    Differs(ProbeAgent),
    /// The candidate isn't below the parent it was found under any more.
    LeftParent,
    /// The candidate exited.
    Exited,
    /// The candidate's identity (its executable's path, its installation)
    /// couldn't be read while it was still running: nothing to publish.
    NoIdentity,
    /// The candidate couldn't be held by its pidfd for a reason other than
    /// its exit (the errno, when there was one: `EMFILE`, say). The
    /// validation can't be bracketed, so it isn't run.
    Unheld(Option<i32>),
    /// Another user owns the candidate.
    ForeignOwner,
    /// The generation retired while discovery ran.
    Retired,
    /// The PTY child exited.
    PaneExited,
}

impl Refusal {
    /// Why pinning the candidate by its pidfd failed, as a refusal: gone
    /// (`ESRCH` from `pidfd_open`, no `/proc/<pid>/stat`, a start that moved,
    /// or the pidfd reading exited) is [`Refusal::Exited`]; anything else,
    /// such as a full descriptor table, is [`Refusal::Unheld`] with its
    /// errno, never reported as an exit.
    pub fn unpinned(why: super::attest::Unpinned) -> Refusal {
        use super::attest::Unpinned;
        match why {
            Unpinned::Exited
            | Unpinned::StartChanged
            | Unpinned::Pidfd(Some(libc::ESRCH))
            | Unpinned::Stat(Some(libc::ENOENT | libc::ESRCH)) => Refusal::Exited,
            Unpinned::Pidfd(errno) | Unpinned::Stat(errno) => Refusal::Unheld(errno),
            Unpinned::NoPeerPidfd => Refusal::Unheld(None),
        }
    }

    /// The reason in words: what the `e2e` build's `binding-status` and the
    /// log say.
    pub fn reason(&self) -> String {
        match self {
            Refusal::NotResolved(agent) => {
                format!("executable not resolved: expected {}", agent_name(*agent))
            }
            Refusal::Differs(agent) => {
                format!("executable differs: expected {}", agent_name(*agent))
            }
            Refusal::LeftParent => "candidate left its parent".to_string(),
            Refusal::Exited => "candidate exited".to_string(),
            Refusal::NoIdentity => {
                "no identity: the candidate's executable or installation can't be read".to_string()
            }
            Refusal::Unheld(Some(errno)) => format!("candidate not held: errno {errno}"),
            Refusal::Unheld(None) => "candidate not held".to_string(),
            Refusal::ForeignOwner => "foreign owner".to_string(),
            Refusal::Retired => "generation retired".to_string(),
            Refusal::PaneExited => "pane child exited".to_string(),
        }
    }
}

/// The agent type's wire name (`claude_code`, `codex`).
fn agent_name(agent: ProbeAgent) -> &'static str {
    match agent {
        ProbeAgent::Claude => "claude_code",
        ProbeAgent::Codex => "codex",
    }
}

/// The names `agent` runs under: its registry detection basenames.
fn names_of(agent: ProbeAgent) -> &'static [&'static str] {
    let agent = match agent {
        ProbeAgent::Claude => AgentType::ClaudeCode,
        ProbeAgent::Codex => AgentType::Codex,
    };
    crate::agent_registry::spec(&agent).detect_basenames
}

/// Pin, at spawn, the executables a generation of class `class` running
/// `command` may run, with `path` the `PATH` the PTY child gets: a direct
/// pane's program word, a launcher's agent name, and both agents' names for
/// an undeclared wrapper. A direct Claude pins nothing here: its `A` comes
/// from the launch record.
pub fn pin_at_spawn(class: PaneClass, command: Option<&str>, path: Option<&OsStr>) -> Pins {
    let by_name = |agent: ProbeAgent| {
        names_of(agent)
            .iter()
            .find_map(|name| pin_program(OsStr::new(name), path))
    };
    let mut pins = Pins::default();
    match class {
        PaneClass::Direct(ProbeAgent::Claude) | PaneClass::Unprobed | PaneClass::Shell => {}
        PaneClass::Direct(agent) => {
            let program = command
                .and_then(super::observe::shell_words)
                .and_then(|words| words.into_iter().next());
            let pin = program.and_then(|word| pin_program(OsStr::new(&word), path));
            set(&mut pins, agent, pin);
        }
        PaneClass::Launcher(agent) => set(&mut pins, agent, by_name(agent)),
        PaneClass::Wrapper => {
            pins.claude = by_name(ProbeAgent::Claude);
            pins.codex = by_name(ProbeAgent::Codex);
        }
    }
    pins
}

fn set(pins: &mut Pins, agent: ProbeAgent, pin: Option<Pin>) {
    match agent {
        ProbeAgent::Claude => pins.claude = pin,
        ProbeAgent::Codex => pins.codex = pin,
    }
}

/// `program` pinned as the PTY child's shell would run it with `path`: an
/// absolute path as it is, a bare name by the first `PATH` directory
/// holding an executable file of that name that isn't a deck telemetry shim.
/// A relative path, or a relative or empty `PATH` entry reached before a
/// match, pins nothing: what it names depends on a directory the pin
/// doesn't read. When the file can be read, it is opened once, and both its
/// identity and its shebang are read from that open file ([`pin_opened`]),
/// so a file swapped in at the path between the two reads can't pair its
/// interpreter with another file's identity. A file this user can't read is
/// pinned by its path, with a second `stat`, and hosts nothing.
pub fn pin_program(program: &OsStr, path: Option<&OsStr>) -> Option<Pin> {
    use std::os::unix::fs::OpenOptionsExt;
    let found = locate(program, path, true)?;
    // `O_NONBLOCK`: a FIFO swapped in after `locate` can't stall the spawn
    // in `open`; `O_NOCTTY`: a terminal swapped in can't become the daemon's
    // controlling terminal. Neither changes anything for a regular file.
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(&found)
    {
        Ok(file) => pin_opened(found, file, path),
        // An executable this user can't read can't be a script its
        // interpreter reads either: pinned by its path, hosting nothing.
        Err(_) => Some(Pin {
            exe: executable_id(&found)?,
            path: found,
            hosted: None,
        }),
    }
}

/// `found` pinned from `file`, opened from it: the identity is the open
/// file's (`fstat`), and the shebang is read from the same file.
fn pin_opened(found: PathBuf, file: std::fs::File, path: Option<&OsStr>) -> Option<Pin> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = file
        .metadata()
        .ok()
        .filter(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)?;
    let exe = FileId {
        dev: meta.dev(),
        ino: meta.ino(),
    };
    let mut head = Vec::with_capacity(SHEBANG_BYTES);
    let hosted = file
        .take(SHEBANG_BYTES as u64)
        .read_to_end(&mut head)
        .ok()
        .and_then(|_| hosted_by(&head, path));
    Some(Pin {
        path: found,
        exe,
        hosted,
    })
}

/// Where `program` is found with `path`, as [`pin_program`] says; with
/// `skip_shims`, a deck telemetry shim is passed over.
fn locate(program: &OsStr, path: Option<&OsStr>, skip_shims: bool) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let named = Path::new(program);
    if program.as_bytes().contains(&b'/') {
        return (named.is_absolute() && executable_id(named).is_some()).then(|| named.into());
    }
    for dir in std::env::split_paths(path?) {
        if !dir.is_absolute() {
            return None;
        }
        let candidate = dir.join(program);
        if executable_id(&candidate).is_some()
            && !(skip_shims && crate::telemetry::shim::is_shim_file(&candidate))
        {
            return Some(candidate);
        }
    }
    None
}

/// The `(dev, ino)` of the executable regular file at `path`, symlinks
/// followed.
fn executable_id(path: &Path) -> Option<FileId> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::metadata(path)
        .ok()
        .filter(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .map(|m| FileId {
            dev: m.dev(),
            ino: m.ino(),
        })
}

/// The interpreter a script starting with `head` runs under, with `path`
/// the `PATH` `env` would search, as the kernel reads a shebang line: the
/// interpreter's path, then the rest of the line as one argument. `env NAME`
/// is `NAME` as `env` finds it on `path`, its argv `NAME`; any `env` option
/// (`-S` and the rest) or assignment is not followed. `None` when the file
/// isn't a script or its interpreter doesn't resolve.
pub fn hosted_by(head: &[u8], path: Option<&OsStr>) -> Option<Hosted> {
    use std::os::unix::ffi::OsStrExt;
    let line = head.strip_prefix(b"#!")?;
    let line = &line[..line.iter().position(|b| *b == b'\n')?];
    let line = line.trim_ascii();
    let split = line
        .iter()
        .position(|b| b.is_ascii_whitespace())
        .unwrap_or(line.len());
    let (interp, rest) = line.split_at(split);
    let arg = rest.trim_ascii();
    let interp = OsStr::from_bytes(interp);
    if !Path::new(interp).is_absolute() {
        return None;
    }
    let arg = (!arg.is_empty()).then(|| OsStr::from_bytes(arg).to_os_string());
    if Path::new(interp).file_name() == Some(OsStr::new("env")) {
        let name = arg?;
        let bytes = name.as_bytes();
        if bytes.starts_with(b"-")
            || bytes.contains(&b'=')
            || bytes.iter().any(u8::is_ascii_whitespace)
        {
            return None;
        }
        let found = locate(&name, path, false)?;
        return Some(Hosted {
            interpreter: executable_id(&found)?,
            prefix: vec![name],
        });
    }
    let mut prefix = vec![interp.to_os_string()];
    prefix.extend(arg);
    Some(Hosted {
        interpreter: executable_id(Path::new(interp))?,
        prefix,
    })
}

/// One process as a walk reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub ppid: u32,
    pub start: u64,
    /// The `stat` state letter: `Z` and `X` are gone.
    pub state: u8,
    pub exe: Option<FileId>,
    /// `/proc/<pid>/exe`'s target's file name, ` (deleted)` taken off.
    pub exe_name: Option<OsString>,
    pub argv: Vec<OsString>,
    pub uid: Option<u32>,
}

impl Proc {
    fn gone(&self) -> bool {
        matches!(self.state, b'Z' | b'X')
    }

    /// Whether the process is named as an agent: its `argv[0]`'s or its
    /// executable's basename.
    fn agent_named(&self) -> bool {
        let named = |name: &OsStr| AGENT_BASENAMES.iter().any(|agent| name == *agent);
        self.argv
            .first()
            .and_then(|arg0| Path::new(arg0).file_name())
            .is_some_and(named)
            || self.exe_name.as_deref().is_some_and(named)
    }
}

/// The processes a walk reads, behind a seam: `/proc` on Linux
/// ([`LinuxProcs`]), a scripted table in the tests.
pub trait Procs {
    fn read(&mut self, pid: u32) -> Option<Proc>;
    fn children(&mut self, pid: u32) -> Vec<u32>;
    /// The executable regular file at `path` now, symlinks followed: what a
    /// hosted pin's script path holds at validation.
    fn file(&mut self, path: &Path) -> Option<FileId> {
        executable_id(path)
    }
    /// `pid`'s `PATH` as its environment began: what `env` searched for the
    /// interpreter it ran. `None` when it can't be read or has none.
    fn env_path(&mut self, _pid: u32) -> Option<OsString> {
        None
    }
}

/// Whether `proc`, process `pid`, runs the interpreter `hosted` names: the
/// one pinned at spawn, or, for an `env NAME` shebang, the `NAME` its own
/// `PATH` resolves as `env` would (the module doc's issue #2258 paragraph).
fn runs_interpreter(procs: &mut impl Procs, pid: u32, proc: &Proc, hosted: &Hosted) -> bool {
    let Some(exe) = proc.exe else {
        return false;
    };
    if exe == hosted.interpreter {
        return true;
    }
    let (Some(name), Some(path)) = (hosted.env_name(), procs.env_path(pid)) else {
        return false;
    };
    // As `locate` searches: in order, absolute directories only.
    for dir in std::env::split_paths(&path) {
        if !dir.is_absolute() {
            return false;
        }
        if let Some(found) = procs.file(&dir.join(name)) {
            return found == exe;
        }
    }
    false
}

/// A process the walk found named as an agent, with what it was found
/// under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub pid: u32,
    pub start: u64,
    /// The processes from the PTY child down to the candidate's parent, each
    /// with its start ticks.
    pub chain: Vec<(u32, u64)>,
    /// The candidate's parent, when it was an attested entrypoint: its pid,
    /// start ticks and argv as attested.
    pub entry: Option<(u32, u64, Vec<OsString>)>,
}

/// What the walk needs to know besides the processes.
pub struct Walk<'a> {
    /// The deck's own executable: its `wrap` is seen through.
    pub own_exe: Option<FileId>,
    /// The expected agent's pin, for the entrypoint rule.
    pub pin: &'a Pin,
}

/// Codex's embedded-mode pair (#2002), left out of every argv comparison on
/// both sides (A-20d-13): the deck's own spawn adds it to a direct Codex.
fn strip_pair(args: &[OsString]) -> Vec<OsString> {
    let pair = crate::wrap::CODEX_EMBEDDED_ARGS;
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        if i + 1 < args.len() && args[i] == pair[0] && args[i + 1] == pair[1] {
            i += 2;
            continue;
        }
        out.push(args[i].clone());
        i += 1;
    }
    out
}

/// The command a deck `wrap` process runs: what follows its `--`, when
/// `proc` runs the deck's executable as `<deck> wrap … -- <command>`.
fn wrapped(proc: &Proc, own_exe: Option<FileId>) -> Option<&[OsString]> {
    if own_exe.is_none()
        || proc.exe != own_exe
        || proc.argv.get(1).map(OsString::as_os_str) != Some(OsStr::new("wrap"))
    {
        return None;
    }
    let dashes = proc.argv.iter().position(|a| a == "--")?;
    let rest = &proc.argv[dashes + 1..];
    (!rest.is_empty()).then_some(rest)
}

/// Whether `proc` is an entrypoint `hosted` attests: the interpreter it
/// names, with its argv prefix and the pinned script path, and, when it is
/// the deck wrapper's agent child, the wrapped command's arguments at its
/// end (the deck's own spawn: the telemetry shim may add arguments in front
/// of them).
fn attested_entry(
    procs: &mut impl Procs,
    pid: u32,
    proc: &Proc,
    pin: &Pin,
    below_wrapper: Option<&[OsString]>,
) -> bool {
    let Some(hosted) = &pin.hosted else {
        return false;
    };
    let p = hosted.prefix.len();
    if proc.argv.len() <= p
        || proc.argv[..p] != hosted.prefix[..]
        || !Path::new(&proc.argv[p])
            .components()
            .eq(pin.path.components())
        || !runs_interpreter(procs, pid, proc, hosted)
    {
        return false;
    }
    match below_wrapper {
        None => true,
        Some(command) => {
            let want = strip_pair(command.get(1..).unwrap_or_default());
            strip_pair(&proc.argv[p + 1..]).ends_with(&want)
        }
    }
}

/// The first process below and including `root` (the PTY child, `root_start`
/// its start ticks) named as an agent, breadth first, at most
/// [`WALK_PROCESSES`] read and [`WALK_DEPTH`] deep. `None` when there is none
/// yet.
pub fn find(
    procs: &mut impl Procs,
    root: u32,
    root_start: u64,
    walk: &Walk<'_>,
) -> Option<Candidate> {
    struct Node {
        pid: u32,
        chain: Vec<(u32, u64)>,
        /// The parent's wrapped command, when the parent is the deck's wrap.
        below_wrapper: Option<Vec<OsString>>,
        /// The parent, when it is an attested entrypoint.
        entry: Option<(u32, u64, Vec<OsString>)>,
    }
    let mut queue = std::collections::VecDeque::from([Node {
        pid: root,
        chain: Vec::new(),
        below_wrapper: None,
        entry: None,
    }]);
    let mut read = 0;
    while let Some(node) = queue.pop_front() {
        if read == WALK_PROCESSES {
            return None;
        }
        read += 1;
        let Some(proc) = procs.read(node.pid) else {
            continue;
        };
        if node.chain.is_empty() && proc.start != root_start {
            return None;
        }
        if proc.gone() {
            continue;
        }
        if node.below_wrapper.is_some()
            && (proc.exe == walk.own_exe || proc.argv.iter().any(|arg| arg == "app-server"))
        {
            // The deck's wrap forked it and it hasn't `exec`'d, or it is the
            // wrapper's `codex app-server` trust probe: not the agent.
            continue;
        }
        if proc.agent_named() && wrapped(&proc, walk.own_exe).is_none() {
            return Some(Candidate {
                pid: node.pid,
                start: proc.start,
                chain: node.chain,
                entry: node.entry,
            });
        }
        if node.chain.len() >= WALK_DEPTH {
            continue;
        }
        let entry = attested_entry(
            procs,
            node.pid,
            &proc,
            walk.pin,
            node.below_wrapper.as_deref(),
        )
        .then(|| (node.pid, proc.start, proc.argv.clone()));
        let below_wrapper = wrapped(&proc, walk.own_exe).map(<[OsString]>::to_vec);
        let mut chain = node.chain;
        chain.push((node.pid, proc.start));
        for child in procs.children(node.pid) {
            queue.push_back(Node {
                pid: child,
                chain: chain.clone(),
                below_wrapper: below_wrapper.clone(),
                entry: entry.clone(),
            });
        }
    }
    None
}

/// How a validated candidate is attested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attested {
    /// Its executable is the pin.
    Pinned,
    /// It is the native child of an attested entrypoint.
    BelowEntry,
}

/// What [`validate`] accepted: how the candidate is attested, and the
/// executable it read while accepting it. Only that executable may be
/// published ([`validate_held`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accepted {
    pub how: Attested,
    pub exe: FileId,
}

/// Validate `candidate` against `pin` (the expected agent `agent`'s), the
/// daemon's `uid`, with `reject_uid` the `e2e` build's forced foreign-owner
/// refusal: read again, the refusals in the module doc's order.
///
/// Below an attested entrypoint the script's path must still hold the
/// pinned file: one replaced after the pin (an npm update, or any writer of
/// that directory) refuses the native as an executable mismatch. That
/// check is a refusal only, never a lookup that could attest anything; what
/// it can't see is named in the module doc.
pub fn validate(
    procs: &mut impl Procs,
    candidate: &Candidate,
    agent: ProbeAgent,
    pin: &Pin,
    uid: u32,
    reject_uid: bool,
) -> Result<Accepted, Refusal> {
    let Some(proc) = procs
        .read(candidate.pid)
        .filter(|p| p.start == candidate.start && !p.gone())
    else {
        return Err(Refusal::Exited);
    };
    // Its ancestry as it was found: each process still there with its start
    // ticks, the parent of the next.
    let mut child_ppid = proc.ppid;
    for &(pid, start) in candidate.chain.iter().rev() {
        if child_ppid != pid {
            return Err(Refusal::LeftParent);
        }
        let Some(link) = procs.read(pid).filter(|p| p.start == start && !p.gone()) else {
            return Err(Refusal::LeftParent);
        };
        child_ppid = link.ppid;
    }
    if reject_uid || proc.uid != Some(uid) {
        return Err(Refusal::ForeignOwner);
    }
    let Some(exe) = proc.exe else {
        return Err(Refusal::Differs(agent));
    };
    if exe == pin.exe {
        return Ok(Accepted {
            how: Attested::Pinned,
            exe,
        });
    }
    if let (Some((pid, start, argv)), Some(hosted)) = (&candidate.entry, &pin.hosted)
        && let Some(entry) = procs.read(*pid)
        && entry.start == *start
        && !entry.gone()
        && runs_interpreter(procs, *pid, &entry, hosted)
        && entry.argv == *argv
        && entry.uid == Some(uid)
        && proc.ppid == *pid
        && procs.file(&pin.path) == Some(pin.exe)
    {
        return Ok(Accepted {
            how: Attested::BelowEntry,
            exe,
        });
    }
    Err(Refusal::Differs(agent))
}

/// [`validate`], then `candidate` read again around the pidfd poll that
/// `exited` is: `read` takes what the identity needs (given the accepted
/// executable) while the pidfd still holds the candidate. The executable
/// read before the poll and after it must both be the one validation
/// accepted, so a candidate that `exec`s anything between validation's read
/// and the poll's is refused as a mismatch, never published with an
/// executable nothing compared with the pin. A `read` that gives nothing
/// for a candidate the poll still finds running is [`Refusal::NoIdentity`],
/// not an exit. Returns `read`'s value.
#[allow(clippy::too_many_arguments)]
pub fn validate_held<T>(
    procs: &mut impl Procs,
    candidate: &Candidate,
    agent: ProbeAgent,
    pin: &Pin,
    uid: u32,
    reject_uid: bool,
    read: impl FnOnce(FileId) -> Option<T>,
    exited: impl FnOnce() -> bool,
) -> Result<T, Refusal> {
    let accepted = validate(procs, candidate, agent, pin, uid, reject_uid)?;
    still_accepted(procs, candidate, agent, &accepted)?;
    let out = read(accepted.exe);
    if exited() {
        return Err(Refusal::Exited);
    }
    still_accepted(procs, candidate, agent, &accepted)?;
    out.ok_or(Refusal::NoIdentity)
}

/// `candidate` read again: still there, and still running the executable
/// validation accepted.
fn still_accepted(
    procs: &mut impl Procs,
    candidate: &Candidate,
    agent: ProbeAgent,
    accepted: &Accepted,
) -> Result<(), Refusal> {
    let proc = procs
        .read(candidate.pid)
        .filter(|p| p.start == candidate.start && !p.gone())
        .ok_or(Refusal::Exited)?;
    if proc.exe == Some(accepted.exe) {
        Ok(())
    } else {
        Err(Refusal::Differs(agent))
    }
}

/// `/proc` as a walk reads it.
#[cfg(target_os = "linux")]
#[derive(Default)]
pub struct LinuxProcs;

#[cfg(target_os = "linux")]
impl Procs for LinuxProcs {
    fn read(&mut self, pid: u32) -> Option<Proc> {
        use std::io::Read;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
        let close = stat.iter().rposition(|b| *b == b')')?;
        let fields: Vec<&[u8]> = stat[close + 1..]
            .split(u8::is_ascii_whitespace)
            .filter(|f| !f.is_empty())
            .collect();
        let number =
            |i: usize| -> Option<u64> { std::str::from_utf8(fields.get(i)?).ok()?.parse().ok() };
        let state = *fields.first()?.first()?;
        let ppid = u32::try_from(number(1)?).ok()?;
        let start = number(19)?;
        let exe = std::fs::metadata(format!("/proc/{pid}/exe"))
            .ok()
            .map(|m| FileId {
                dev: m.dev(),
                ino: m.ino(),
            });
        let exe_name = std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|target| {
                let bytes = target.as_os_str().as_bytes();
                let bytes = bytes.strip_suffix(DELETED).unwrap_or(bytes);
                Path::new(OsStr::from_bytes(bytes))
                    .file_name()
                    .map(OsStr::to_os_string)
            });
        let mut cmdline = Vec::new();
        std::fs::File::open(format!("/proc/{pid}/cmdline"))
            .ok()?
            .take(128 << 10)
            .read_to_end(&mut cmdline)
            .ok()?;
        if cmdline.last() == Some(&0) {
            cmdline.pop();
        }
        let argv = if cmdline.is_empty() {
            Vec::new()
        } else {
            cmdline
                .split(|b| *b == 0)
                .map(|a| OsStr::from_bytes(a).to_os_string())
                .collect()
        };
        let uid = std::fs::read(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|status| super::instance::status_uid(&status));
        Some(Proc {
            ppid,
            start,
            state,
            exe,
            exe_name,
            argv,
            uid,
        })
    }

    fn children(&mut self, pid: u32) -> Vec<u32> {
        let mut found = Vec::new();
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            return found;
        };
        for task in tasks.flatten() {
            if let Ok(text) = std::fs::read_to_string(task.path().join("children")) {
                found.extend(
                    text.split_whitespace()
                        .filter_map(|c| c.parse::<u32>().ok()),
                );
            }
        }
        found.sort_unstable();
        found.dedup();
        found
    }

    fn env_path(&mut self, pid: u32) -> Option<OsString> {
        use std::io::Read;
        use std::os::unix::ffi::OsStrExt;
        let mut environ = Vec::new();
        std::fs::File::open(format!("/proc/{pid}/environ"))
            .ok()?
            .take(ENVIRON_BYTES)
            .read_to_end(&mut environ)
            .ok()?;
        environ
            .split(|b| *b == 0)
            .find_map(|var| var.strip_prefix(b"PATH="))
            .map(|path| OsStr::from_bytes(path).to_os_string())
    }
}

/// The most bytes of a process's environment [`LinuxProcs::env_path`] reads:
/// a `PATH` past it isn't found, and the interpreter isn't attested by it.
#[cfg(target_os = "linux")]
const ENVIRON_BYTES: u64 = 1 << 20;

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;
    use std::collections::HashMap;

    const UID: u32 = 1000;
    const OWN: FileId = FileId { dev: 1, ino: 1 };
    const NATIVE: FileId = FileId { dev: 1, ino: 2 };
    const COPY: FileId = FileId { dev: 1, ino: 3 };
    const SCRIPT: FileId = FileId { dev: 1, ino: 4 };
    const PYTHON: FileId = FileId { dev: 1, ino: 5 };
    const SH: FileId = FileId { dev: 1, ino: 6 };

    #[derive(Default)]
    struct Table {
        procs: HashMap<u32, Proc>,
        /// A scripted `exec`: once `pid` has been read this many times, it
        /// runs the executable given.
        exec_after: Option<(u32, usize, FileId)>,
        reads: HashMap<u32, usize>,
        /// What a path holds; a path not listed is read from the disk.
        files: HashMap<PathBuf, FileId>,
        /// Each process's `PATH`, as its environment began.
        paths: HashMap<u32, OsString>,
    }

    impl Table {
        fn add(
            &mut self,
            pid: u32,
            ppid: u32,
            exe: FileId,
            name: &str,
            argv: &[&str],
        ) -> &mut Self {
            self.procs.insert(
                pid,
                Proc {
                    ppid,
                    start: u64::from(pid) * 10,
                    state: b'S',
                    exe: Some(exe),
                    exe_name: Some(name.into()),
                    argv: argv.iter().map(OsString::from).collect(),
                    uid: Some(UID),
                },
            );
            self
        }
    }

    impl Procs for Table {
        fn read(&mut self, pid: u32) -> Option<Proc> {
            let reads = self.reads.entry(pid).or_default();
            *reads += 1;
            if let Some((execs, after, exe)) = self.exec_after
                && execs == pid
                && *reads > after
                && let Some(proc) = self.procs.get_mut(&pid)
            {
                proc.exe = Some(exe);
            }
            self.procs.get(&pid).cloned()
        }
        fn children(&mut self, pid: u32) -> Vec<u32> {
            let mut c: Vec<u32> = self
                .procs
                .iter()
                .filter(|(child, p)| p.ppid == pid && **child != pid)
                .map(|(child, _)| *child)
                .collect();
            c.sort_unstable();
            c
        }
        fn file(&mut self, path: &Path) -> Option<FileId> {
            self.files
                .get(path)
                .copied()
                .or_else(|| executable_id(path))
        }
        fn env_path(&mut self, pid: u32) -> Option<OsString> {
            self.paths.get(&pid).cloned()
        }
    }

    fn native_pin() -> Pin {
        Pin {
            path: "/opt/agent/codex".into(),
            exe: NATIVE,
            hosted: None,
        }
    }

    fn npm_pin() -> Pin {
        Pin {
            path: "/npm/bin/codex".into(),
            exe: SCRIPT,
            hosted: Some(Hosted {
                interpreter: PYTHON,
                prefix: vec!["python3".into()],
            }),
        }
    }

    fn walk(pin: &Pin) -> Walk<'_> {
        Walk {
            own_exe: Some(OWN),
            pin,
        }
    }

    /// Scenario: A deck wrapper runs a launcher script that spawns the native Codex. The walk goes down from the PTY child, passes over the wrapper and the launcher, and finds the native by name; validation attests it by its pinned executable.
    #[spec("inbox/probe/035")]
    #[test]
    fn probe_035_the_walk_finds_the_native_below_a_launcher() {
        let mut t = Table::default();
        t.add(
            10,
            1,
            OWN,
            "dot-agent-deck",
            &[
                "dot-agent-deck",
                "wrap",
                "--agent",
                "codex",
                "--",
                "/l/launcher",
            ],
        )
        .add(
            11,
            10,
            PYTHON,
            "python3.12",
            &["/usr/bin/python3", "/l/launcher"],
        )
        .add(12, 11, NATIVE, "codex", &["/opt/agent/codex", "--x"]);
        let pin = native_pin();
        let c = find(&mut t, 10, 100, &walk(&pin)).expect("a candidate");
        assert_eq!(c.pid, 12);
        assert_eq!(c.chain, vec![(10, 100), (11, 110)]);
        assert_eq!(
            validate(&mut t, &c, ProbeAgent::Codex, &pin, UID, false),
            Ok(Accepted {
                how: Attested::Pinned,
                exe: NATIVE
            })
        );
    }

    /// Scenario: The deck's wrapper has forked a child that hasn't exec'd yet (it still runs the deck's executable) and runs its `codex app-server` trust probe, which is the genuine Codex. Neither is taken for the agent.
    #[spec("inbox/probe/035")]
    #[test]
    fn probe_035_the_walk_passes_over_the_wrappers_fork_and_trust_probe() {
        let mut t = Table::default();
        t.add(
            10,
            1,
            OWN,
            "dot-agent-deck",
            &["dot-agent-deck", "wrap", "--agent", "codex", "--", "codex"],
        )
        .add(11, 10, NATIVE, "codex", &["codex", "app-server"])
        .add(
            12,
            10,
            OWN,
            "dot-agent-deck",
            &["dot-agent-deck", "wrap", "--agent", "codex", "--", "codex"],
        );
        let pin = native_pin();
        assert_eq!(find(&mut t, 10, 100, &walk(&pin)), None);
    }

    /// Scenario: An npm-shaped Codex: the wrapper's child runs the pinned script under the pinned interpreter with the wrapped arguments (the #2002 pair on one side only), and spawns the native. The native is attested through the entrypoint; the same native under another script, or the script under another interpreter, is an executable mismatch.
    #[spec("inbox/probe/035")]
    #[test]
    fn probe_035_an_entrypoint_attests_only_its_shebang_shape() {
        let pin = npm_pin();
        for (entry_exe, script, ok) in [
            (PYTHON, "/npm/bin/codex", true),
            (PYTHON, "/npm//bin/codex", true),
            (PYTHON, "/npm/other/entry.py", false),
            (COPY, "/npm/bin/codex", false),
        ] {
            let mut t = Table::default();
            t.add(
                10,
                1,
                OWN,
                "dot-agent-deck",
                &[
                    "deck",
                    "wrap",
                    "--agent",
                    "codex",
                    "--",
                    "codex",
                    "--fixture",
                ],
            )
            .add(
                11,
                10,
                entry_exe,
                "python3",
                &[
                    "python3",
                    script,
                    "-c",
                    "shell_environment_policy.set={}",
                    "--fixture",
                ],
            )
            .add(12, 11, NATIVE, "codex", &["/n/codex", "--fixture"]);
            t.files.insert(pin.path.clone(), SCRIPT);
            let c = find(&mut t, 10, 100, &walk(&pin)).expect("the native");
            assert_eq!(c.pid, 12);
            assert_eq!(c.entry.is_some(), ok, "{script}");
            let got = validate(&mut t, &c, ProbeAgent::Codex, &pin, UID, false);
            if ok {
                assert_eq!(
                    got,
                    Ok(Accepted {
                        how: Attested::BelowEntry,
                        exe: NATIVE
                    })
                );
            } else {
                assert_eq!(got, Err(Refusal::Differs(ProbeAgent::Codex)));
            }
        }
    }

    /// Scenario: An entrypoint attested when the native was found has since exec'd another interpreter by the time validation reads it again. The native is refused as an executable mismatch.
    #[spec("inbox/probe/035")]
    #[test]
    fn probe_035_an_entrypoint_is_read_again_at_validation() {
        let pin = npm_pin();
        let mut t = Table::default();
        t.add(
            10,
            1,
            OWN,
            "dot-agent-deck",
            &["deck", "wrap", "--agent", "codex", "--", "codex"],
        )
        .add(11, 10, PYTHON, "python3", &["python3", "/npm/bin/codex"])
        .add(12, 11, NATIVE, "codex", &["/n/codex"]);
        t.files.insert(pin.path.clone(), SCRIPT);
        let c = find(&mut t, 10, 100, &walk(&pin)).unwrap();
        assert!(
            validate(&mut t, &c, ProbeAgent::Codex, &pin, UID, false).is_ok(),
            "attested as found"
        );
        t.procs.get_mut(&11).unwrap().exe = Some(COPY);
        assert_eq!(
            validate(&mut t, &c, ProbeAgent::Codex, &pin, UID, false),
            Err(Refusal::Differs(ProbeAgent::Codex))
        );
    }

    /// `body` written to `at`, executable.
    fn write_executable(at: &Path, body: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        crate::test_isolation::write_script(at, body).unwrap();
        std::fs::set_permissions(at, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The wrapper, its entrypoint running `interp` with `script` as its
    /// argument, and the native that entrypoint spawned.
    fn hosted_table(interp: FileId, script: &str) -> Table {
        let mut t = Table::default();
        t.add(
            10,
            1,
            OWN,
            "dot-agent-deck",
            &["deck", "wrap", "--agent", "codex", "--", "codex"],
        )
        .add(11, 10, interp, "node", &["node", script])
        .add(12, 11, NATIVE, "codex", &["/n/codex"]);
        t
    }

    /// Scenario: An npm-shaped Codex (`#!/usr/bin/env node`) is pinned at spawn from a PATH whose `node` is one inode, and a launcher such as `devbox run` puts a directory with another `node` first on the entrypoint's own PATH, so its entrypoint runs that other `node` (issue #2258). The native below it binds. The same entrypoint is refused when its own PATH is unknown, names the spawn's `node` first, finds a third `node` before the one it runs, or puts a relative directory first: the interpreter must be what `env` would find on that PATH.
    #[spec("inbox/probe/035")]
    #[test]
    fn probe_035_an_env_interpreter_is_found_on_the_entrypoints_own_path() {
        let dir = tempfile::tempdir().unwrap();
        let (spawn, launcher, third) = (
            dir.path().join("spawn"),
            dir.path().join("launcher"),
            dir.path().join("third"),
        );
        for at in [&spawn, &launcher, &third] {
            std::fs::create_dir(at).unwrap();
            std::fs::copy("/bin/sh", at.join("node")).unwrap();
        }
        let script = spawn.join("codex");
        write_executable(&script, b"#!/usr/bin/env node\n// codex.js\n");
        let pin = pin_program(OsStr::new("codex"), Some(spawn.as_os_str())).expect("pinned");
        let launchers = executable_id(&launcher.join("node")).unwrap();
        assert_ne!(
            Some(launchers),
            pin.hosted.as_ref().map(|h| h.interpreter),
            "the launcher's node is another inode than the spawn's"
        );
        let at = script.to_str().unwrap();
        let joined = |dirs: &[&Path]| std::env::join_paths(dirs).unwrap();
        let attempt = |path: Option<OsString>| {
            let mut t = hosted_table(launchers, at);
            if let Some(path) = path {
                t.paths.insert(11, path);
            }
            find(&mut t, 10, 100, &walk(&pin))
                .ok_or(Refusal::Differs(ProbeAgent::Codex))
                .and_then(|c| validate(&mut t, &c, ProbeAgent::Codex, &pin, UID, false))
                .map(|accepted| accepted.how)
        };
        assert_eq!(
            attempt(Some(joined(&[&launcher, &spawn]))),
            Ok(Attested::BelowEntry),
            "the launcher's node, first on the entrypoint's own PATH"
        );
        for (path, why) in [
            (None, "no PATH read"),
            (Some(joined(&[&spawn, &launcher])), "the spawn's node first"),
            (
                Some(joined(&[&third, &launcher])),
                "another node before the one it runs",
            ),
            (
                Some(OsString::from(format!("relative:{}", launcher.display()))),
                "a relative directory first",
            ),
        ] {
            assert_eq!(
                attempt(path),
                Err(Refusal::Differs(ProbeAgent::Codex)),
                "{why}"
            );
        }
    }

    /// Scenario: An npm-shaped Codex (a `#!/usr/bin/env node` script on the PATH, its interpreter beside it) is pinned at spawn, and its entrypoint and the native it spawns bind. Then npm replaces the script at the same path with a new file (a new inode) before validation: the same entrypoint and native are refused as an executable mismatch, as the native route refuses a wrapper that execs a replaced binary.
    #[spec("inbox/probe/035")]
    #[test]
    fn probe_035_a_hosted_script_replaced_after_its_pin_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy("/bin/sh", dir.path().join("node")).unwrap();
        let script = dir.path().join("codex");
        write_executable(&script, b"#!/usr/bin/env node\n// codex.js 1\n");
        let pin = pin_program(OsStr::new("codex"), Some(dir.path().as_os_str())).expect("pinned");
        let interp = pin.hosted.as_ref().expect("an npm-shaped pin").interpreter;
        let at = script.to_str().unwrap();
        let mut t = hosted_table(interp, at);
        let c = find(&mut t, 10, 100, &walk(&pin)).expect("the native");
        assert!(
            validate(&mut t, &c, ProbeAgent::Codex, &pin, UID, false).is_ok(),
            "the pinned script's native binds"
        );
        let staged = dir.path().join("codex.staged");
        write_executable(&staged, b"#!/usr/bin/env node\n// codex.js 2\n");
        std::fs::rename(&staged, &script).unwrap();
        let mut t = hosted_table(interp, at);
        let c = find(&mut t, 10, 100, &walk(&pin)).expect("the native");
        assert_eq!(
            validate(&mut t, &c, ProbeAgent::Codex, &pin, UID, false).err(),
            Some(Refusal::Differs(ProbeAgent::Codex)),
            "the script at the pinned path was replaced after the pin"
        );
    }

    /// Scenario: The pin opens an npm-shaped `#!/usr/bin/env node` script, and another script (`#!/usr/bin/env python3`) is renamed over its path before the pin reads the open file. The pin's identity and its interpreter both come from the file it opened, never one from each; and since the path no longer holds the pinned file, the entrypoint's native is refused at validation.
    #[spec("inbox/probe/035")]
    #[test]
    fn probe_035_a_pin_reads_identity_and_shebang_from_one_open_file() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let id = |p: &Path| {
            let m = std::fs::metadata(p).unwrap();
            FileId {
                dev: m.dev(),
                ino: m.ino(),
            }
        };
        let (node, python) = (dir.path().join("node"), dir.path().join("python3"));
        std::fs::copy("/bin/sh", &node).unwrap();
        std::fs::copy("/bin/sh", &python).unwrap();
        let script = dir.path().join("codex");
        write_executable(&script, b"#!/usr/bin/env node\n");
        let opened_id = id(&script);
        let opened = std::fs::File::open(&script).unwrap();
        let staged = dir.path().join("codex.staged");
        write_executable(&staged, b"#!/usr/bin/env python3\n");
        std::fs::rename(&staged, &script).unwrap();
        let pin = pin_opened(script.clone(), opened, Some(dir.path().as_os_str())).expect("pinned");
        assert_eq!(pin.exe, opened_id, "the open file's identity");
        assert_eq!(
            pin.hosted.as_ref().map(|h| h.interpreter),
            Some(id(&node)),
            "the open file's interpreter, not the swapped-in file's"
        );
        let mut t = hosted_table(id(&node), script.to_str().unwrap());
        let c = find(&mut t, 10, 100, &walk(&pin)).expect("the native");
        assert_eq!(
            validate(&mut t, &c, ProbeAgent::Codex, &pin, UID, false).err(),
            Some(Refusal::Differs(ProbeAgent::Codex)),
            "the path holds another file than the pinned one"
        );
    }

    /// Scenario: Pinning a candidate by its pidfd fails. A failure that says the process is gone (exited, `ESRCH`, no stat, a moved start) is refused as "candidate exited"; any other (a full descriptor table, an unreadable stat) is refused as not held with its errno, never reported as an exit.
    #[spec("inbox/probe/035")]
    #[test]
    fn probe_035_a_pidfd_failure_is_an_exit_only_when_the_process_is_gone() {
        use super::super::attest::Unpinned;
        for gone in [
            Unpinned::Exited,
            Unpinned::StartChanged,
            Unpinned::Pidfd(Some(libc::ESRCH)),
            Unpinned::Stat(Some(libc::ENOENT)),
            Unpinned::Stat(Some(libc::ESRCH)),
        ] {
            assert_eq!(Refusal::unpinned(gone), Refusal::Exited, "{gone:?}");
        }
        for (held, errno) in [
            (Unpinned::Pidfd(Some(libc::EMFILE)), Some(libc::EMFILE)),
            (Unpinned::Pidfd(None), None),
            (Unpinned::Stat(Some(libc::EACCES)), Some(libc::EACCES)),
            (Unpinned::NoPeerPidfd, None),
        ] {
            let refusal = Refusal::unpinned(held);
            assert_eq!(refusal, Refusal::Unheld(errno), "{held:?}");
            assert!(!refusal.reason().contains("exited"), "{}", refusal.reason());
        }
        assert_eq!(
            Refusal::Unheld(Some(libc::EMFILE)).reason(),
            format!("candidate not held: errno {}", libc::EMFILE)
        );
    }

    /// Scenario: A candidate validated against its pin `exec`s another binary right after validation reads it, before its identity is read again; another one `exec`s across the pidfd poll. Each is refused as an executable mismatch rather than published with an executable validation never compared with the pin.
    #[spec("inbox/probe/035")]
    #[test]
    fn probe_035_the_published_executable_is_the_validated_one() {
        let pin = native_pin();
        let base = || {
            let mut t = Table::default();
            t.add(10, 1, SH, "dash", &["sh", "-c", "launcher"]).add(
                12,
                10,
                NATIVE,
                "codex",
                &["/opt/agent/codex"],
            );
            t
        };
        let held = |t: &mut Table, c: &Candidate| {
            validate_held(t, c, ProbeAgent::Codex, &pin, UID, false, Some, || false)
        };
        let mut t = base();
        let c = find(&mut t, 10, 100, &walk(&pin)).unwrap();
        assert_eq!(
            held(&mut t, &c),
            Ok(NATIVE),
            "no exec: the pin is published"
        );
        // The candidate's reads from here: validation's, then the identity's
        // read, then the read after the poll.
        for (after, when) in [(1, "before the identity's read"), (2, "across the poll")] {
            let mut t = base();
            let c = find(&mut t, 10, 100, &walk(&pin)).unwrap();
            t.reads.clear();
            t.exec_after = Some((12, after, COPY));
            assert_eq!(
                held(&mut t, &c),
                Err(Refusal::Differs(ProbeAgent::Codex)),
                "an exec {when}"
            );
        }
    }

    /// Scenario: The identity of a validated candidate can't be read (its executable's path or its installation directory gone, as an npm update can leave it). While the candidate still runs, that is refused as "no identity", never as an exit; when the poll finds it gone, it is "candidate exited". Either way nothing is published.
    #[spec("inbox/probe/035")]
    #[test]
    fn probe_035_an_unreadable_identity_is_not_an_exit() {
        let pin = native_pin();
        let mut t = Table::default();
        t.add(10, 1, SH, "dash", &["sh", "-c", "launcher"]).add(
            12,
            10,
            NATIVE,
            "codex",
            &["/opt/agent/codex"],
        );
        let c = find(&mut t, 10, 100, &walk(&pin)).unwrap();
        let mut unread = |gone: bool| {
            let read = |_: FileId| None::<FileId>;
            validate_held(
                &mut t,
                &c,
                ProbeAgent::Codex,
                &pin,
                UID,
                false,
                read,
                || gone,
            )
        };
        assert_eq!(
            unread(false),
            Err(Refusal::NoIdentity),
            "alive: no identity"
        );
        assert_eq!(
            unread(true),
            Err(Refusal::Exited),
            "gone at the poll: an exit"
        );
        assert!(
            Refusal::NoIdentity.reason().starts_with("no identity: "),
            "{}",
            Refusal::NoIdentity.reason()
        );
        assert_ne!(Refusal::NoIdentity.reason(), Refusal::Exited.reason());
    }

    /// Scenario: A lookalike (another inode) is refused as an executable mismatch, but a lookalike that also exited, left its parent or belongs to another user is refused for that first: lifecycle and owner refusals come before the mismatch.
    #[spec("inbox/probe/035")]
    #[test]
    fn probe_035_lifecycle_and_owner_refusals_come_before_a_mismatch() {
        let pin = native_pin();
        let base = || {
            let mut t = Table::default();
            t.add(10, 1, SH, "dash", &["sh", "-c", "launcher"])
                .add(11, 10, PYTHON, "python3", &["python3", "/l/launcher"])
                .add(12, 11, COPY, "codex", &["/opt/agent/codex"]);
            t
        };
        let mut t = base();
        let c = find(&mut t, 10, 100, &walk(&pin)).unwrap();
        assert_eq!(
            validate(&mut t, &c, ProbeAgent::Codex, &pin, UID, false),
            Err(Refusal::Differs(ProbeAgent::Codex))
        );
        assert_eq!(
            validate(&mut t, &c, ProbeAgent::Codex, &pin, UID, true),
            Err(Refusal::ForeignOwner)
        );
        t.procs.get_mut(&12).unwrap().uid = Some(UID + 1);
        assert_eq!(
            validate(&mut t, &c, ProbeAgent::Codex, &pin, UID, false),
            Err(Refusal::ForeignOwner)
        );
        t.procs.get_mut(&12).unwrap().ppid = 1;
        assert_eq!(
            validate(&mut t, &c, ProbeAgent::Codex, &pin, UID, false),
            Err(Refusal::LeftParent)
        );
        t.procs.get_mut(&12).unwrap().state = b'Z';
        assert_eq!(
            validate(&mut t, &c, ProbeAgent::Codex, &pin, UID, false),
            Err(Refusal::Exited)
        );
        let mut t = base();
        let c = find(&mut t, 10, 100, &walk(&pin)).unwrap();
        t.procs.remove(&11);
        assert_eq!(
            validate(&mut t, &c, ProbeAgent::Codex, &pin, UID, false),
            Err(Refusal::LeftParent),
            "a broken ancestry"
        );
    }

    /// Scenario: The production validator reads a genuinely foreign-owned process from `/proc` (PID 1, when another user owns it) as a candidate found with no ancestry: it is refused as a foreign owner, whatever its executable, with no fault injected. Skipped where PID 1 is the test user's.
    #[spec("inbox/probe/035")]
    #[test]
    fn probe_035_a_genuinely_foreign_owner_is_refused() {
        // SAFETY: `getuid(2)` has no failure mode.
        let uid = unsafe { libc::getuid() };
        let Some(init) = LinuxProcs.read(1) else {
            eprintln!("SKIP: PID 1 isn't readable here");
            return;
        };
        if init.uid == Some(uid) || init.uid.is_none() {
            eprintln!("SKIP: PID 1 is this user's");
            return;
        }
        let candidate = Candidate {
            pid: 1,
            start: init.start,
            chain: Vec::new(),
            entry: None,
        };
        let pin = Pin {
            path: "/sbin/init".into(),
            exe: init.exe.unwrap_or(NATIVE),
            hosted: None,
        };
        assert_eq!(
            validate(
                &mut LinuxProcs,
                &candidate,
                ProbeAgent::Codex,
                &pin,
                uid,
                false
            ),
            Err(Refusal::ForeignOwner),
            "even with its own executable as the pin"
        );
    }

    /// Scenario: The PTY child's pid now names another process (its start ticks differ): the walk finds nothing below it.
    #[spec("inbox/probe/035")]
    #[test]
    fn probe_035_a_reused_root_pid_finds_nothing() {
        let pin = native_pin();
        let mut t = Table::default();
        t.add(10, 1, NATIVE, "codex", &["codex"]);
        assert_eq!(find(&mut t, 10, 7, &walk(&pin)), None);
        assert_eq!(find(&mut t, 10, 100, &walk(&pin)).map(|c| c.pid), Some(10));
    }

    /// Scenario: Shebang lines as the kernel and `env` read them: `env NAME` is the name found on PATH with argv `NAME`; an absolute interpreter keeps its path and one argument; `env -S`, an assignment, a relative interpreter or no shebang at all give no interpreter.
    #[spec("inbox/probe/035")]
    #[test]
    fn probe_035_shebangs_as_the_kernel_and_env_read_them() {
        let dir = tempfile::tempdir().unwrap();
        let interp = dir.path().join("node");
        std::fs::copy("/bin/sh", &interp).unwrap();
        let path = dir.path().as_os_str();
        let env = hosted_by(b"#!/usr/bin/env node\nrest", Some(path)).expect("env node");
        assert_eq!(env.prefix, vec![OsString::from("node")]);
        assert_eq!(Some(env.interpreter), executable_id(&interp));
        let direct = hosted_by(b"#!/bin/sh -e\n", Some(path)).expect("direct");
        assert_eq!(
            direct.prefix,
            vec![OsString::from("/bin/sh"), OsString::from("-e")]
        );
        for refused in [
            &b"#!/usr/bin/env -S node --x\n"[..],
            b"#!/usr/bin/env A=1 node\n",
            b"#!node\n",
            b"\x7fELF",
            b"#!/usr/bin/env node",
        ] {
            assert_eq!(hosted_by(refused, Some(path)), None, "{refused:?}");
        }
    }

    /// Scenario: A launcher pane pins its agent's name on the PTY child's PATH, symlinks followed, passing over a deck telemetry shim in front; a name on no PATH entry, or behind a relative entry, pins nothing; a direct pane pins its absolute program word; an undeclared wrapper pins both agents independently; a direct Claude pins nothing here.
    #[spec("inbox/probe/035")]
    #[test]
    fn probe_035_pins_are_taken_from_the_spawn_path() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let (shim, bin, real) = (
            dir.path().join("shim"),
            dir.path().join("bin"),
            dir.path().join("real"),
        );
        for d in [&shim, &bin, &real] {
            std::fs::create_dir(d).unwrap();
        }
        std::fs::copy("/bin/sh", real.join("codex-1")).unwrap();
        symlink(real.join("codex-1"), bin.join("codex")).unwrap();
        std::fs::copy("/bin/sh", bin.join("claude")).unwrap();
        let shim_script = shim.join("codex");
        crate::test_isolation::write_script(
            &shim_script,
            "#!/bin/sh\nexec '/deck' __telemetry-shim codex \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&shim_script, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            crate::telemetry::shim::is_shim_file(&shim_script),
            "the stand-in reads as a deck shim script"
        );
        let path = std::env::join_paths([&shim, &bin]).unwrap();
        let id = |p: &Path| {
            let m = std::fs::metadata(p).unwrap();
            FileId {
                dev: m.dev(),
                ino: m.ino(),
            }
        };
        let launcher = pin_at_spawn(
            PaneClass::Launcher(ProbeAgent::Codex),
            Some("devbox run x"),
            Some(&path),
        );
        let codex = launcher.codex.as_ref().expect("codex pinned");
        assert_eq!(codex.path, bin.join("codex"));
        assert_eq!(codex.exe, id(&real.join("codex-1")));
        assert_eq!(launcher.claude, None, "a Codex launcher pins only Codex");
        let wrapper = pin_at_spawn(PaneClass::Wrapper, Some("x"), Some(&path));
        assert_eq!(wrapper.codex.as_ref().map(|p| p.exe), Some(codex.exe));
        assert_eq!(
            wrapper.claude.as_ref().map(|p| p.exe),
            Some(id(&bin.join("claude")))
        );
        let direct = pin_at_spawn(
            PaneClass::Direct(ProbeAgent::Codex),
            Some(&format!("{} --model m", real.join("codex-1").display())),
            None,
        );
        assert_eq!(direct.codex.map(|p| p.exe), Some(codex.exe));
        assert_eq!(
            pin_at_spawn(
                PaneClass::Launcher(ProbeAgent::Codex),
                None,
                Some(shim.as_os_str())
            ),
            Pins::default(),
            "only a shim on PATH: unresolved"
        );
        let relative = std::env::join_paths([Path::new("rel"), &bin]).unwrap();
        assert_eq!(
            pin_at_spawn(
                PaneClass::Launcher(ProbeAgent::Codex),
                None,
                Some(&relative)
            ),
            Pins::default(),
            "a relative entry before the match"
        );
        assert_eq!(
            pin_at_spawn(
                PaneClass::Direct(ProbeAgent::Claude),
                Some("claude"),
                Some(&path)
            ),
            Pins::default()
        );
    }
}
