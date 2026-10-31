//! PRD #2050: which OpenCode a spawn will run, and what that changes about
//! how the deck starts it.
//!
//! OpenCode 1.x and 2.x load different plugin layouts (see
//! [`crate::opencode_manage`]) and 2.x runs server plugins in a shared
//! background service unless it is started with `--standalone`. So before an
//! OpenCode agent starts, the deck resolves the binary its command names,
//! asks it `--version`, and keeps the answer, keyed on the binary's file
//! identity, so an unchanged binary is not asked again and an in-place
//! upgrade (same path, new file) is.
//!
//! What this module does NOT do: see through a wrapper. A command whose
//! program is not an OpenCode binary (`devbox run oc-big`, `env FOO=1
//! opencode`) is [`SpawnTarget::Wrapper`]: its version is unknown at spawn,
//! and its argv is never rewritten. The `opencode` its PATH resolves to is
//! probed too ([`probe_path_opencode`]), so that once the daemon's watch sees
//! the pane's client run that very file, its version is known
//! ([`recorded_version`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// A parsed `opencode --version`: (major, minor, patch).
pub type Version = (u32, u32, u32);

/// The OpenCode generations the deck installs a different layout for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Major {
    /// OpenCode 1.x (and 0.x, which loads plugins the 1.x way).
    V1,
    /// OpenCode 2.x and later.
    V2,
}

impl Major {
    /// The generation of `version`.
    pub fn of(version: Version) -> Self {
        if version.0 >= 2 { Major::V2 } else { Major::V1 }
    }
}

/// Parse what `opencode --version` prints: `opencode v2.2.5` on 2.x and
/// `1.18.35` on 1.x (both measured). The first non-empty line is read; an
/// optional `opencode` word and `v` prefix are skipped, and anything after
/// the patch number (a pre-release or build suffix) is ignored. `None` for
/// anything else.
pub fn parse_version(output: &str) -> Option<Version> {
    let line = output.lines().map(str::trim).find(|l| !l.is_empty())?;
    let rest = line
        .strip_prefix("opencode")
        .map(str::trim_start)
        .unwrap_or(line);
    let rest = rest.strip_prefix('v').unwrap_or(rest);
    let mut parts = rest.splitn(3, '.');
    let number = |part: Option<&str>| -> Option<u32> {
        let digits: String = part?.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            None
        } else {
            digits.parse().ok()
        }
    };
    let major_text = parts.next()?;
    // The major must be the whole first component, so `x1.2.3` or `1a.2.3`
    // is not read as a version.
    if major_text.is_empty() || !major_text.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let major = major_text.parse().ok()?;
    let minor_text = parts.next()?;
    if !minor_text.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let minor = number(Some(minor_text))?;
    let patch = number(parts.next())?;
    Some((major, minor, patch))
}

/// A binary's file identity: an in-place upgrade changes at least one of
/// these, so a cached answer for the old file is not reused for the new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileKey {
    size: u64,
    mtime_ns: i128,
    inode: u64,
}

impl FileKey {
    /// The identity of the file `path` resolves to (symlinks followed), or
    /// `None` when it cannot be read.
    pub fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        let mtime_ns = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(-1);
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&metadata);
        #[cfg(not(unix))]
        let inode = 0;
        Some(FileKey {
            size: metadata.len(),
            mtime_ns,
            inode,
        })
    }
}

/// One cached answer: the file identity it was asked of, what the binary
/// said (`None` when it said nothing usable, so a binary that cannot report
/// its version is not asked on every spawn), when it was stored, for
/// eviction, and, for a probe that did not complete, when the answer stops
/// counting ([`PROBE_FAILURE_TTL`]).
#[derive(Debug, Clone)]
struct Entry {
    key: FileKey,
    version: Option<Version>,
    stored: u64,
    expires: Option<std::time::Instant>,
}

/// Cached answers, by resolved binary path, at most [`MAX_CACHE_ENTRIES`].
#[derive(Debug, Default)]
struct VersionCache {
    entries: HashMap<PathBuf, Entry>,
    stored: u64,
}

impl VersionCache {
    fn insert(&mut self, path: PathBuf, key: FileKey, version: Option<Version>) {
        self.insert_expiring(path, key, version, None);
    }

    fn insert_expiring(
        &mut self,
        path: PathBuf,
        key: FileKey,
        version: Option<Version>,
        expires: Option<std::time::Instant>,
    ) {
        self.stored += 1;
        let stored = self.stored;
        self.entries.insert(
            path,
            Entry {
                key,
                version,
                stored,
                expires,
            },
        );
        while self.entries.len() > MAX_CACHE_ENTRIES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.stored)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }
}

static CACHE: Mutex<Option<VersionCache>> = Mutex::new(None);

/// The most binaries the cache remembers; the oldest answer goes first.
/// A machine runs a handful of OpenCode binaries, so this is a bound, not a
/// working-set size.
const MAX_CACHE_ENTRIES: usize = 64;

/// How long `--version` may take. Measured on this project's dev box: 2.x
/// answers in 0.2–0.4 s and 1.x in 1.0–1.3 s.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The most of `--version`'s output the probe reads.
const PROBE_OUTPUT_LIMIT: u64 = 4096;

/// How long a probe that did not complete (it could not be started, failed,
/// or ran out of time) stands for the binary's answer before it is asked
/// again (review F4/F5): long enough that a binary whose `--version` hangs
/// does not hold up every spawn by [`PROBE_TIMEOUT`], short enough that one
/// that timed out on a loaded machine is asked again soon.
const PROBE_FAILURE_TTL: Duration = Duration::from_secs(60);

/// The version of the binary at `path`, from the cache when its file
/// identity is unchanged, otherwise from `probe` (which runs it). `None`
/// when the file cannot be read, the probe did not complete, or its output
/// is not a version.
///
/// `probe` answers the binary's output when it ran to a successful exit, and
/// `None` when it could not be started, failed or timed out. A completed run
/// is cached for the file's life; one that did not complete only for
/// [`PROBE_FAILURE_TTL`], so a probe that timed out on a loaded machine is
/// asked again a minute later rather than leaving the binary's version
/// unknown for the daemon's lifetime, and a binary that hangs costs one
/// timeout a minute rather than one per spawn.
pub fn detect_with(path: &Path, probe: impl FnOnce(&Path) -> Option<String>) -> Option<Version> {
    detect_with_at(path, std::time::Instant::now(), probe)
}

/// [`detect_with`], as of `now`.
fn detect_with_at(
    path: &Path,
    now: std::time::Instant,
    probe: impl FnOnce(&Path) -> Option<String>,
) -> Option<Version> {
    let key = FileKey::of(path)?;
    {
        let cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = cache.as_ref().and_then(|c| c.entries.get(path))
            && entry.key == key
            && entry.expires.is_none_or(|expires| now < expires)
        {
            return entry.version;
        }
    }
    let output = probe(path);
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    let cache = cache.get_or_insert_with(VersionCache::default);
    let Some(output) = output else {
        cache.insert_expiring(path.to_path_buf(), key, None, Some(now + PROBE_FAILURE_TTL));
        return None;
    };
    let version = parse_version(&output);
    cache.insert(path.to_path_buf(), key, version);
    version
}

/// [`detect_with`], asking the binary itself with `path_env` as its `PATH`.
pub fn detect(path: &Path, path_env: Option<&std::ffi::OsStr>) -> Option<Version> {
    detect_with(path, |p| run_version(p, path_env))
}

/// PRD #2050, audit M1: the version a spawn-time probe recorded for the file
/// `exe` is, matched by file identity rather than path, so a client found
/// through a symlink or another path to the same file is still known, for
/// whichever pane's spawn probed it. Never runs anything: a file no spawn
/// probed is `None`, an unknown version.
pub fn recorded_version(exe: &Path) -> Option<Version> {
    let key = FileKey::of(exe)?;
    let cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    cache
        .as_ref()?
        .entries
        .values()
        .filter(|entry| entry.key == key && entry.version.is_some())
        .max_by_key(|entry| entry.stored)
        .and_then(|entry| entry.version)
}

/// Run `<path> --version` with no stdin, bounded by [`PROBE_TIMEOUT`]; its
/// stdout (at most [`PROBE_OUTPUT_LIMIT`] bytes), or `None` when it cannot be
/// started, fails or times out. Blocking: callers run it off the async
/// executor.
///
/// The probe runs with a minimal environment (`PATH`, the child's, so a
/// launcher script finds what it runs; `HOME`), from `/`, in a process group
/// of its own.
/// Its output is read on a separate thread joined against the deadline, so a
/// descendant that keeps the pipe open cannot hold the spawn past the
/// timeout, and on a timeout the whole group is killed.
fn run_version(path: &Path, path_env: Option<&std::ffi::OsStr>) -> Option<String> {
    run_version_within(path, path_env, PROBE_TIMEOUT)
}

/// [`run_version`], bounded by `timeout`.
fn run_version_within(
    path: &Path,
    path_env: Option<&std::ffi::OsStr>,
    timeout: Duration,
) -> Option<String> {
    use std::io::Read as _;
    let mut command = std::process::Command::new(path);
    command
        .arg("--version")
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some(path_env) = path_env {
        command.env("PATH", path_env);
    }
    if let Some(home) = std::env::var_os("HOME") {
        command.env("HOME", home);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.current_dir("/").process_group(0);
    }
    let mut child = command.spawn().ok()?;
    let deadline = std::time::Instant::now() + timeout;
    let Some(stdout) = child.stdout.take() else {
        kill_probe(&mut child);
        return None;
    };
    // The reader ends at EOF, when every process holding the pipe has closed
    // it. A descendant that left the probe's process group (`setsid`) while
    // holding it escapes the group kill below and keeps this thread blocked
    // until it exits: one thread per such probe, which the failure record
    // ([`PROBE_FAILURE_TTL`]) limits to one a minute per binary.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let read = stdout.take(PROBE_OUTPUT_LIMIT).read_to_end(&mut out);
        let _ = tx.send(read.map(|_| out));
    });
    let exited = loop {
        match leader_exited(&mut child) {
            Ok(true) => break true,
            Ok(false) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(false) => break false,
            // The leader's state cannot be read: it is not signalled, since
            // its pid may no longer be its own.
            Err(_) => return None,
        }
    };
    if !exited {
        // Out of time, the leader still running or unreaped: its pid is still
        // the group's id, so the group is the probe's own.
        kill_probe(&mut child);
        return None;
    }
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    match rx.recv_timeout(remaining) {
        Ok(read) => {
            // EOF with the leader gone: nothing of the probe's holds the pipe,
            // so nothing is signalled, only the leader reaped.
            let status = child.wait().ok()?;
            let out = read.ok()?;
            status
                .success()
                .then(|| String::from_utf8_lossy(&out).into_owned())
        }
        Err(_) => {
            // The leader exited but a descendant still holds the pipe. The
            // leader is not reaped yet ([`leader_exited`] leaves it a zombie),
            // so its pid still names the group, and the group is killed
            // before it is reaped.
            kill_probe(&mut child);
            None
        }
    }
}

/// Whether the probe's leader has exited, leaving it unreaped, so its pid
/// (and with it the process group's id) cannot be reissued until
/// [`kill_probe`] or `wait` reaps it (audit L3, review F5).
#[cfg(unix)]
fn leader_exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    let pid = libc::id_t::try_from(child.id()).map_err(std::io::Error::other)?;
    // SAFETY: a zeroed `siginfo_t` is a valid out-parameter for `waitid`,
    // which only writes into it; `si_pid` reads the field `waitid` filled
    // (it stays 0 when no child changed state, per POSIX for `WNOHANG`).
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let rc = libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        );
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(info.si_pid() != 0)
    }
}

/// Elsewhere than Unix there are no process groups to keep: whether the
/// probe exited, reaping it.
#[cfg(not(unix))]
fn leader_exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    child.try_wait().map(|status| status.is_some())
}

/// Kill a probe whose leader has not been reaped, with every process in its
/// group, then reap it. Never called once the leader was reaped: its pid could
/// by then name another process group (review F5).
fn kill_probe(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pgid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: `killpg` takes plain integers; the group is the probe's
        // own (`process_group(0)` made the child its leader, and the leader
        // is unreaped, so the id is still the group's).
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// What a spawn's command starts, as far as the deck can see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnTarget {
    /// The command's program is an OpenCode binary the deck resolved; its
    /// version is `None` when it could not be read.
    Direct {
        program: PathBuf,
        version: Option<Version>,
    },
    /// The program is not an OpenCode binary (a wrapper, a launcher, an
    /// environment prefix), or could not be resolved: the deck cannot tell
    /// which OpenCode will run.
    Wrapper,
}

impl SpawnTarget {
    /// The generation it runs, when known.
    pub fn major(&self) -> Option<Major> {
        match self {
            SpawnTarget::Direct {
                version: Some(v), ..
            } => Some(Major::of(*v)),
            _ => None,
        }
    }
}

/// Whether `name` (a file name) names an OpenCode binary: `opencode`, or
/// `opencode` followed by a version (`opencode2`, `opencode-1.18.35`,
/// `opencode.2`), optionally ending `.exe` (the native binary an npm install
/// ships). Anything else starting `opencode` (`opencode-wrapper.sh`,
/// `opencode-fresh`) is a wrapper of the user's: the deck neither runs it to
/// ask its version nor rewrites its command (audit L4).
///
/// A file with such a name is accepted whatever it holds, a `#!` script
/// included. Not because of the pinned 1.19.5: its npm `bin` is
/// `bin/opencode.exe`, which its postinstall hard-links to the native ELF.
/// Because older npm 1.x installs (1.0.0, read from its tarball) ship
/// `bin/opencode` as a `#!/bin/sh` launcher that `exec`s the native binary,
/// and that launcher must still be recognised, so its version is read and its
/// card can say when it is too old.
pub fn opencode_name(name: &str) -> bool {
    let name = name.strip_suffix(".exe").unwrap_or(name);
    let Some(rest) = name.strip_prefix("opencode") else {
        return false;
    };
    let rest = rest
        .strip_prefix('-')
        .or_else(|| rest.strip_prefix('.'))
        .unwrap_or(rest);
    if rest.is_empty() {
        return name == "opencode";
    }
    rest.starts_with(|c: char| c.is_ascii_digit())
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The program token of `command`: its first whitespace-separated word, when
/// that names an OpenCode binary ([`opencode_name`], or a path ending in
/// one). A quoted or otherwise unusual first word is not one: the deck does
/// not parse shell syntax here.
fn opencode_program(command: &str) -> Option<&str> {
    let program = command.split_whitespace().next()?;
    if program.contains(['"', '\'', '$', '`', '\\', '=']) {
        return None;
    }
    let name = Path::new(program).file_name()?.to_str()?;
    opencode_name(name).then_some(program)
}

/// The file `program` resolves to: itself when it names a path, otherwise
/// the first executable file of that name on `path_env` (the child's
/// `PATH`).
fn resolve_program(program: &str, path_env: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let is_file = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.is_file());
    if program.contains('/') {
        let path = PathBuf::from(program);
        return (path.is_absolute() && is_file(&path)).then_some(path);
    }
    std::env::split_paths(path_env?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| is_file(candidate) && is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

/// What `command` starts, resolving its program on `path_env` and asking it
/// `--version` through `detect` (cached). Blocking: call it off the async
/// executor.
pub fn spawn_target_with(
    command: Option<&str>,
    path_env: Option<&std::ffi::OsStr>,
    detect: impl FnOnce(&Path) -> Option<Version>,
) -> SpawnTarget {
    let Some(program) = command.and_then(opencode_program) else {
        return SpawnTarget::Wrapper;
    };
    match resolve_program(program, path_env) {
        Some(path) => {
            let version = detect(&path);
            SpawnTarget::Direct {
                program: path,
                version,
            }
        }
        None => SpawnTarget::Wrapper,
    }
}

/// [`spawn_target_with`], asking the real binary.
pub fn spawn_target(command: Option<&str>, path_env: Option<&std::ffi::OsStr>) -> SpawnTarget {
    spawn_target_with(command, path_env, |program| detect(program, path_env))
}

/// PRD #2050 (review F2): for a spawn whose command is a wrapper, the file
/// `opencode` resolves to on `spawn_path` (the PATH the deck hands the
/// wrapper), else on `fallback_path` (the daemon's login-shell PATH), asked
/// `--version` through `detect` (cached by file identity, like a direct
/// spawn's probe). The deck chooses that file, as it chooses a direct
/// command's, so this runs nothing a pane started. Whether the wrapper runs
/// it is not known here: the daemon's watch counts the answer for a pane's
/// client only when the client runs that same file
/// ([`recorded_version`]). `None` when no `opencode` resolves. Blocking.
pub fn probe_path_opencode_with(
    spawn_path: Option<&std::ffi::OsStr>,
    fallback_path: Option<&std::ffi::OsStr>,
    detect: impl FnOnce(&Path, Option<&std::ffi::OsStr>) -> Option<Version>,
) -> Option<(PathBuf, Option<Version>)> {
    let (path, path_env) = resolve_program("opencode", spawn_path)
        .map(|path| (path, spawn_path))
        .or_else(|| resolve_program("opencode", fallback_path).map(|path| (path, fallback_path)))?;
    let version = detect(&path, path_env);
    Some((path, version))
}

/// [`probe_path_opencode_with`], asking the real binary.
pub fn probe_path_opencode(
    spawn_path: Option<&std::ffi::OsStr>,
    fallback_path: Option<&std::ffi::OsStr>,
) -> Option<(PathBuf, Option<Version>)> {
    probe_path_opencode_with(spawn_path, fallback_path, detect)
}

/// OpenCode 2.x subcommands that start a session and take `--standalone`
/// (2.2.5 `--help`): `mini`, `run` and `session`. Any other subcommand
/// (`plugin`, `auth`, `serve`, …) is not an agent session and is left alone.
const STANDALONE_SUBCOMMANDS: [&str; 3] = ["mini", "run", "session"];

/// OpenCode flags that take a value as the next word (2.2.5 `--help` for the
/// top level and `mini`, and 1.x's `--model`, `--port` and `--hostname`).
/// Every other flag is read as a switch.
pub const VALUE_FLAGS: &[&str] = &[
    "--server",
    "--session",
    "-s",
    "--prompt",
    "--log-level",
    "--completions",
    "--model",
    "-m",
    "--agent",
    "--replay-limit",
    "--port",
    "--hostname",
];

/// The index in `args` (a command's words after its program) of the first
/// word that is not a flag or a flag's value: OpenCode's subcommand, or a
/// directory, when the command names one. Flags may come before the
/// subcommand (`opencode --auto mini`, accepted by 2.2.5), so they are
/// skipped; a [`VALUE_FLAGS`] flag written without `=` skips its value too.
pub fn first_positional<S: AsRef<str>>(args: &[S]) -> Option<usize> {
    let mut n = 0;
    while let Some(arg) = args.get(n).map(AsRef::as_ref) {
        if arg == "--" {
            return (n + 1 < args.len()).then_some(n + 1);
        }
        if arg.starts_with('-') && arg.len() > 1 {
            n += if !arg.contains('=') && VALUE_FLAGS.contains(&arg) {
                2
            } else {
                1
            };
            continue;
        }
        return Some(n);
    }
    None
}

/// One shell word of a command: its byte range and its text, and whether any
/// of it was quoted or escaped (so it is a value, never a flag the deck
/// reads).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Word<'a> {
    start: usize,
    end: usize,
    text: &'a str,
    quoted: bool,
}

/// `command` split into shell words, honouring single and double quotes and
/// backslashes, so a quoted prompt (`run "explain --standalone"`) is one word
/// rather than several the deck could misread as flags (review N4).
fn shell_words(command: &str) -> Vec<Word<'_>> {
    let mut words = Vec::new();
    let mut chars = command.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut quoted = false;
        let mut quote: Option<char> = None;
        let mut end = start;
        while let Some(&(_, c)) = chars.peek() {
            match quote {
                Some(q) if c == q => quote = None,
                Some('"') if c == '\\' => {
                    chars.next();
                }
                Some(_) => {}
                None if c.is_whitespace() => break,
                None if c == '\'' || c == '"' => {
                    quote = Some(c);
                    quoted = true;
                }
                None if c == '\\' => {
                    quoted = true;
                    chars.next();
                }
                None => {}
            }
            chars.next();
            end = chars.peek().map_or(command.len(), |&(next, _)| next);
        }
        words.push(Word {
            start,
            end,
            text: &command[start..end],
            quoted,
        });
    }
    words
}

/// `path` as one shell word: itself when it holds only characters a shell
/// leaves alone, otherwise single-quoted.
///
/// The quoting is POSIX (review F7): correct for sh, bash, zsh, dash and ksh,
/// which is what `$SHELL -c` runs on the machines the deck supports. Under
/// fish a backslash inside single quotes is an escape, so a path holding one
/// changes (though nothing is injected), and shells such as nushell or elvish
/// parse `'\''` differently. Only a path outside the plain set is quoted at
/// all, and the registry keeps the configured command, not this rewrite.
fn shell_quote(path: &str) -> String {
    let plain = !path.is_empty()
        && path.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '/' | '_' | '.' | ',' | '+' | '@' | '%' | ':' | '-')
        });
    if plain {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

/// PRD #2050: `command` with `--standalone` added, for an OpenCode 2.x the
/// deck starts directly, so its server plugins run in a private server that
/// inherits the pane's environment rather than in the shared background
/// service (which carries whichever environment first started it). `None`
/// when nothing changes: the command is not a direct OpenCode invocation,
/// already says `--standalone` or `--server` (connecting to a named server),
/// or runs a subcommand that is not a session.
///
/// The flag goes right after the session subcommand (`mini`/`run`/`session`)
/// when there is one, flags before it skipped ([`first_positional`]), and
/// otherwise right after the program; the rest of the command is kept byte
/// for byte, so a quoted prompt or a trailing directory argument is not
/// disturbed.
pub fn with_standalone(command: &str) -> Option<String> {
    standalone_rewrite(command, None)
}

/// [`with_standalone`], with the program word replaced by `program`, the
/// absolute path the deck resolved and asked `--version` (review S6): the
/// rewritten command runs through the user's shell, whose startup files may
/// change `PATH`, so naming the file keeps the binary that runs the one the
/// deck probed.
pub fn with_standalone_at(command: &str, program: &Path) -> Option<String> {
    standalone_rewrite(command, program.to_str())
}

fn standalone_rewrite(command: &str, program_path: Option<&str>) -> Option<String> {
    opencode_program(command)?;
    let words = shell_words(command);
    let (program, rest) = words.split_first()?;
    if rest.iter().any(|w| {
        !w.quoted
            && (w.text == "--standalone"
                || w.text.starts_with("--standalone=")
                || w.text == "--server"
                || w.text.starts_with("--server="))
    }) {
        return None;
    }
    let texts: Vec<&str> = rest.iter().map(|w| w.text).collect();
    let insert_at = match first_positional(&texts).map(|n| &rest[n]) {
        Some(sub) if !sub.quoted && STANDALONE_SUBCOMMANDS.contains(&sub.text) => sub.end,
        // A directory argument, or no positional word: the top-level TUI.
        Some(word) if word.quoted || word.text.contains('/') || word.text == "." => program.end,
        None => program.end,
        // Any other word: another subcommand (or a bare directory name the
        // deck cannot tell from one), left alone rather than guessed at.
        Some(_) => return None,
    };
    let head = match program_path {
        Some(path) => format!("{}{}", shell_quote(path), &command[program.end..insert_at]),
        None => command[program.start..insert_at].to_string(),
    };
    Some(format!(
        "{}{head} --standalone{}",
        &command[..program.start],
        &command[insert_at..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_version_formats() {
        assert_eq!(parse_version("opencode v2.2.5\n"), Some((2, 2, 5)));
        assert_eq!(parse_version("1.18.35\n"), Some((1, 18, 35)));
        assert_eq!(parse_version("\n  v2.0.0-beta.3\n"), Some((2, 0, 0)));
        assert_eq!(parse_version("opencode 1.19.5"), Some((1, 19, 5)));
        assert_eq!(Major::of((2, 2, 5)), Major::V2);
        assert_eq!(Major::of((1, 18, 35)), Major::V1);
        assert_eq!(Major::of((0, 9, 0)), Major::V1);
    }

    #[test]
    fn rejects_garbage_versions() {
        for garbage in [
            "",
            "\n\n",
            "opencode",
            "error: unknown flag",
            "2.2",
            "v.1.2",
            "x1.2.3",
            "1a.2.3",
            "1.b.3",
            "1.2.x",
            "Usage: opencode [flags]",
        ] {
            assert_eq!(parse_version(garbage), None, "{garbage:?}");
        }
    }

    #[test]
    fn caches_by_file_identity_and_reprobes_on_change() {
        let dir = crate::test_temp::tempdir().unwrap();
        let bin = dir.path().join("opencode");
        std::fs::write(&bin, b"one").unwrap();
        let mut probes = 0;
        let mut probe = |answer: &'static str| {
            probes += 1;
            move |_: &Path| Some(answer.to_string())
        };
        assert_eq!(detect_with(&bin, probe("1.18.35")), Some((1, 18, 35)));
        // Unchanged file: the cached answer, no probe.
        assert_eq!(
            detect_with(&bin, |_| panic!("probed an unchanged binary")),
            Some((1, 18, 35))
        );

        // Size changes (an in-place upgrade): asked again.
        std::fs::write(&bin, b"two bytes longer").unwrap();
        assert_eq!(detect_with(&bin, probe("opencode v2.2.5")), Some((2, 2, 5)));

        // Same size, new mtime: asked again.
        std::fs::write(&bin, b"two bytes LONGER").unwrap();
        let file = std::fs::File::options().write(true).open(&bin).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000))
            .unwrap();
        drop(file);
        assert_eq!(detect_with(&bin, probe("1.19.5")), Some((1, 19, 5)));

        // Replaced by a new file (new inode) with the same size and mtime.
        let replacement = dir.path().join("opencode.new");
        std::fs::write(&replacement, b"two bytes LONGER").unwrap();
        let file = std::fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap();
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000))
            .unwrap();
        drop(file);
        std::fs::rename(&replacement, &bin).unwrap();
        #[cfg(unix)]
        assert_eq!(detect_with(&bin, probe("opencode v2.3.0")), Some((2, 3, 0)));

        // A probe that did not complete (a timeout, a spawn failure) stands
        // for PROBE_FAILURE_TTL: a spawn inside it is not held up by asking
        // again, and one after it asks again.
        let slow = dir.path().join("opencode-slow");
        std::fs::write(&slow, b"x").unwrap();
        let now = std::time::Instant::now();
        assert_eq!(detect_with_at(&slow, now, |_| None), None);
        assert_eq!(
            detect_with_at(&slow, now + PROBE_FAILURE_TTL / 2, |_| panic!(
                "asked again inside the failure's TTL"
            )),
            None
        );
        assert_eq!(recorded_version(&slow), None);
        assert_eq!(
            detect_with_at(
                &slow,
                now + PROBE_FAILURE_TTL + Duration::from_secs(1),
                probe("2.2.5")
            ),
            Some((2, 2, 5))
        );
        assert_eq!(recorded_version(&slow), Some((2, 2, 5)));

        // A binary that prints no version is remembered as such.
        let silent = dir.path().join("opencode-silent");
        std::fs::write(&silent, b"x").unwrap();
        assert_eq!(detect_with(&silent, probe("garbage")), None);
        assert_eq!(detect_with(&silent, |_| panic!("asked again")), None);
        assert!(probes >= 4);

        // A missing file is not probed at all.
        assert_eq!(
            detect_with(&dir.path().join("missing"), |_| panic!("probed")),
            None
        );
    }

    /// Scenario (review F2): a wrapper pane's PATH holds an `opencode`, which
    /// the deck resolves and asks its version with that PATH; with none on
    /// the pane's PATH it falls back to the daemon's, and with none on either
    /// nothing is asked. Only a file named exactly `opencode` is taken.
    #[cfg(unix)]
    #[test]
    fn a_wrapper_panes_path_opencode_is_probed_with_its_own_path() {
        let dir = crate::test_temp::tempdir().unwrap();
        let pane_bin = dir.path().join("pane");
        let daemon_bin = dir.path().join("daemon");
        let empty = dir.path().join("empty");
        for d in [&pane_bin, &daemon_bin, &empty] {
            std::fs::create_dir_all(d).unwrap();
        }
        for d in [&pane_bin, &daemon_bin] {
            let file = d.join("opencode");
            crate::test_isolation::write_script(&file, b"x").unwrap();
            std::fs::set_permissions(
                &file,
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
            )
            .unwrap();
        }
        std::fs::write(empty.join("opencode-wrapper.sh"), b"x").unwrap();
        let pane_path = std::env::join_paths([&empty, &pane_bin]).unwrap();
        let daemon_path = std::env::join_paths([&daemon_bin]).unwrap();
        let empty_path = std::env::join_paths([&empty]).unwrap();
        let mut asked = Vec::new();
        let mut probe = |path: &Path, env: Option<&std::ffi::OsStr>| {
            asked.push((path.to_path_buf(), env.map(std::ffi::OsStr::to_os_string)));
            Some((2, 2, 5))
        };
        assert_eq!(
            probe_path_opencode_with(Some(&pane_path), Some(&daemon_path), &mut probe),
            Some((pane_bin.join("opencode"), Some((2, 2, 5))))
        );
        assert_eq!(
            probe_path_opencode_with(Some(&empty_path), Some(&daemon_path), &mut probe),
            Some((daemon_bin.join("opencode"), Some((2, 2, 5))))
        );
        assert_eq!(
            probe_path_opencode_with(Some(&empty_path), Some(&empty_path), &mut probe),
            None
        );
        assert_eq!(
            asked,
            vec![
                (pane_bin.join("opencode"), Some(pane_path.clone())),
                (daemon_bin.join("opencode"), Some(daemon_path.clone())),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolves_direct_commands_and_refuses_wrappers() {
        let dir = crate::test_temp::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        for name in ["opencode", "opencode-1.18.35", "devbox"] {
            let path = bin_dir.join(name);
            crate::test_isolation::write_script(&path, b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(
                &path,
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
            )
            .unwrap();
        }
        let path_env = std::ffi::OsString::from(bin_dir.as_os_str());
        let target =
            |command: &str| spawn_target_with(Some(command), Some(&path_env), |_| Some((2, 2, 5)));
        assert_eq!(
            target("opencode mini -m openai/gpt-5-mini"),
            SpawnTarget::Direct {
                program: bin_dir.join("opencode"),
                version: Some((2, 2, 5))
            }
        );
        let absolute = bin_dir.join("opencode-1.18.35");
        assert_eq!(
            target(absolute.to_str().unwrap()).major(),
            Some(Major::V2),
            "an absolute path to a renamed OpenCode binary is direct"
        );
        assert_eq!(target("devbox run oc-big"), SpawnTarget::Wrapper);
        assert_eq!(target("FOO=1 opencode"), SpawnTarget::Wrapper);
        assert_eq!(target("'opencode' mini"), SpawnTarget::Wrapper);
        assert_eq!(
            target("opencode2"),
            SpawnTarget::Wrapper,
            "not on PATH: unresolved"
        );
        assert_eq!(
            spawn_target_with(None, Some(&path_env), |_| panic!("probed")),
            SpawnTarget::Wrapper
        );
        // A version that cannot be read is direct with no version.
        let unknown = spawn_target_with(Some("opencode"), Some(&path_env), |_| None);
        assert_eq!(unknown.major(), None);
        assert!(matches!(unknown, SpawnTarget::Direct { version: None, .. }));
    }

    #[test]
    fn only_opencode_and_versioned_names_are_opencode_binaries() {
        for name in [
            "opencode",
            "opencode2",
            "opencode-1.18.35",
            "opencode.2",
            "opencode.exe",
            "opencode-2.2.5-beta.1",
        ] {
            assert!(opencode_name(name), "{name}");
        }
        for name in [
            "opencode-wrapper.sh",
            "opencode-fresh",
            "opencode-sync",
            "opencodex",
            "opencode-",
            "opencode.",
            "xopencode",
            "",
        ] {
            assert!(!opencode_name(name), "{name}");
        }
        assert_eq!(with_standalone("opencode-wrapper.sh mini"), None);
        assert_eq!(
            with_standalone("opencode-1.18.35 mini").as_deref(),
            Some("opencode-1.18.35 mini --standalone")
        );
    }

    /// Scenario: the cache answers a client found in a pane's process tree
    /// only from what a spawn-time probe recorded, by file identity — through
    /// another path to the same file too — and never runs a binary it was not
    /// asked to probe (audit M1).
    #[cfg(unix)]
    #[test]
    fn a_recorded_version_is_matched_by_file_identity_and_never_probed() {
        let dir = crate::test_temp::tempdir().unwrap();
        let bin = dir.path().join("opencode");
        std::fs::write(&bin, b"recorded-binary").unwrap();
        let link = dir.path().join("linked-opencode");
        std::os::unix::fs::symlink(&bin, &link).unwrap();
        let stranger = dir.path().join("opencode-x");
        std::fs::write(&stranger, b"never probed").unwrap();
        assert_eq!(recorded_version(&bin), None, "nothing recorded yet");
        assert_eq!(
            detect_with(&bin, |_| Some("opencode v2.2.5".to_string())),
            Some((2, 2, 5))
        );
        assert_eq!(recorded_version(&bin), Some((2, 2, 5)));
        assert_eq!(recorded_version(&link), Some((2, 2, 5)), "same file");
        assert_eq!(recorded_version(&stranger), None, "never asked");
        std::fs::write(&bin, b"upgraded in place, longer").unwrap();
        assert_eq!(recorded_version(&bin), None, "a changed file is unknown");
    }

    #[test]
    fn the_cache_is_bounded() {
        let mut cache = VersionCache::default();
        let key = FileKey {
            size: 1,
            mtime_ns: 1,
            inode: 1,
        };
        for n in 0..MAX_CACHE_ENTRIES + 10 {
            cache.insert(
                PathBuf::from(format!("/bin/opencode{n}")),
                key.clone(),
                None,
            );
        }
        assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
        assert!(!cache.entries.contains_key(Path::new("/bin/opencode0")));
        assert!(cache.entries.contains_key(&PathBuf::from(format!(
            "/bin/opencode{}",
            MAX_CACHE_ENTRIES + 9
        ))));
    }

    /// Scenario: a `--version` that prints a version and exits while a
    /// background child it started keeps stdout open is cut off at the
    /// timeout instead of holding the spawn, and its whole process group,
    /// the background child included, is killed (audit L3). A plain one is
    /// read, from `/` and with only `PATH` and `HOME` in its environment.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_version_probe_is_bounded_and_kills_its_process_group() {
        let dir = crate::test_temp::tempdir().unwrap();
        let pid_file = dir.path().join("background.pid");
        let lingering = dir.path().join("opencode");
        crate::test_isolation::write_script(
            &lingering,
            format!(
                "#!/bin/sh
sleep 30 &
echo $! > '{}'
echo 'opencode v2.2.5'
",
                pid_file.display()
            )
            .as_bytes(),
        )
        .unwrap();
        std::fs::set_permissions(
            &lingering,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        let path_env = std::ffi::OsString::from("/usr/bin:/bin");
        let started = std::time::Instant::now();
        let out = run_version_within(&lingering, Some(&path_env), Duration::from_millis(500));
        assert_eq!(out, None, "the pipe never closed");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let background: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let gone = (0..100).any(|_| {
            let state = std::fs::read_to_string(format!("/proc/{background}/stat")).ok();
            let dead = state.is_none_or(|s| s.split_whitespace().nth(2) == Some("Z"));
            if !dead {
                std::thread::sleep(Duration::from_millis(20));
            }
            dead
        });
        assert!(gone, "the background child was killed with the group");

        let plain = dir.path().join("opencode-plain");
        crate::test_isolation::write_script(
            &plain,
            b"#!/bin/sh
printf '%s|%s|%s\n' \"$(pwd)\" \"${HOME:+home}\" \"${DOT_AGENT_DECK_PANE_ID:-none}\"\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &plain,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        let out = run_version_within(&plain, Some(&path_env), Duration::from_secs(5)).unwrap();
        let expected_home = if std::env::var_os("HOME").is_some() {
            "home"
        } else {
            ""
        };
        assert_eq!(out.trim(), format!("/|{expected_home}|none"));
    }

    /// Scenario: a `--version` that fails after starting a background child
    /// that does not hold its output; the probe returns at once and leaves
    /// that child alone, since once the probe's leader has exited and its
    /// output reached EOF nothing of the probe needs stopping (review F5: a
    /// group kill after the leader is reaped could reach a process that
    /// reused its pid).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_finished_probe_signals_nothing() {
        let dir = crate::test_temp::tempdir().unwrap();
        let pid_file = dir.path().join("background.pid");
        let failing = dir.path().join("opencode");
        crate::test_isolation::write_script(
            &failing,
            format!(
                "#!/bin/sh
sleep 30 >/dev/null 2>&1 &
echo $! > '{}'
exit 3
",
                pid_file.display()
            )
            .as_bytes(),
        )
        .unwrap();
        std::fs::set_permissions(
            &failing,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        let path_env = std::ffi::OsString::from("/usr/bin:/bin");
        let started = std::time::Instant::now();
        let out = run_version_within(&failing, Some(&path_env), Duration::from_secs(5));
        assert_eq!(out, None, "a failed probe answers nothing");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "it did not wait for the timeout: {:?}",
            started.elapsed()
        );
        let background: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let state = std::fs::read_to_string(format!("/proc/{background}/stat")).ok();
        let alive = state.is_some_and(|s| s.split_whitespace().nth(2) != Some("Z"));
        // SAFETY: plain integers; the background `sleep` this test started.
        unsafe {
            libc::kill(background, libc::SIGKILL);
        }
        assert!(alive, "the finished probe's group was not signalled");
    }

    #[test]
    fn adds_standalone_after_leading_flags_and_ignores_quoted_words() {
        // The subcommand is found past leading flags (review S2).
        assert_eq!(
            with_standalone("opencode --auto mini").as_deref(),
            Some("opencode --auto mini --standalone")
        );
        assert_eq!(
            with_standalone("opencode --log-level info mini -m openai/gpt-5-mini").as_deref(),
            Some("opencode --log-level info mini --standalone -m openai/gpt-5-mini")
        );
        assert_eq!(with_standalone("opencode --auto plugin list"), None);
        assert_eq!(
            with_standalone("opencode --session ses_1").as_deref(),
            Some("opencode --standalone --session ses_1"),
            "a flag's value is not a subcommand"
        );
        // A quoted prompt is never read as the flag (review N4).
        assert_eq!(
            with_standalone("opencode run \"explain --standalone\"").as_deref(),
            Some("opencode run --standalone \"explain --standalone\"")
        );
        assert_eq!(
            with_standalone("opencode run 'and --server too'").as_deref(),
            Some("opencode run --standalone 'and --server too'")
        );
        assert_eq!(
            with_standalone("opencode \"my project\"").as_deref(),
            Some("opencode --standalone \"my project\"")
        );
    }

    #[test]
    fn the_rewrite_names_the_binary_the_deck_probed() {
        assert_eq!(
            with_standalone_at(
                "opencode mini -m x/y",
                Path::new("/home/u/.opencode/bin/opencode")
            )
            .as_deref(),
            Some("/home/u/.opencode/bin/opencode mini --standalone -m x/y")
        );
        assert_eq!(
            with_standalone_at(
                "opencode",
                Path::new("/Users/u/Application Support/it's/opencode")
            )
            .as_deref(),
            Some("'/Users/u/Application Support/it'\\''s/opencode' --standalone")
        );
        assert_eq!(
            with_standalone_at("opencode --standalone", Path::new("/bin/opencode")),
            None
        );
    }

    #[test]
    fn adds_standalone_to_direct_2x_sessions_only() {
        assert_eq!(
            with_standalone("opencode").as_deref(),
            Some("opencode --standalone")
        );
        assert_eq!(
            with_standalone("opencode mini -m openai/gpt-5-mini").as_deref(),
            Some("opencode mini --standalone -m openai/gpt-5-mini")
        );
        assert_eq!(
            with_standalone("/opt/bin/opencode --prompt \"hi there\"").as_deref(),
            Some("/opt/bin/opencode --standalone --prompt \"hi there\"")
        );
        assert_eq!(
            with_standalone("opencode run  'say hi'").as_deref(),
            Some("opencode run --standalone  'say hi'")
        );
        assert_eq!(
            with_standalone("opencode ./project").as_deref(),
            Some("opencode --standalone ./project")
        );
        // Never twice, and never next to an explicit server.
        assert_eq!(with_standalone("opencode --standalone"), None);
        assert_eq!(with_standalone("opencode mini --standalone"), None);
        assert_eq!(
            with_standalone("opencode --server http://127.0.0.1:4096"),
            None
        );
        // Not a session subcommand, or not a direct OpenCode command.
        assert_eq!(with_standalone("opencode plugin list"), None);
        assert_eq!(with_standalone("opencode serve"), None);
        assert_eq!(with_standalone("devbox run oc-big"), None);
        assert_eq!(with_standalone("FOO=1 opencode"), None);
        assert_eq!(with_standalone(""), None);
    }
}
