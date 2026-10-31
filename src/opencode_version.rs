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
//! opencode`) is [`SpawnTarget::Wrapper`]: its version is unknown, and its
//! argv is never rewritten.

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

/// Cached answers, by resolved binary path: the file identity they were
/// asked of and what the binary said (`None` when it said nothing usable,
/// so a binary that cannot report its version is not asked on every spawn).
type VersionCache = HashMap<PathBuf, (FileKey, Option<Version>)>;
static CACHE: Mutex<Option<VersionCache>> = Mutex::new(None);

/// How long `--version` may take. Measured on this project's dev box: 2.x
/// answers in 0.2–0.4 s and 1.x in 1.0–1.3 s.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The version of the binary at `path`, from the cache when its file
/// identity is unchanged, otherwise from `probe` (which runs it). `None`
/// when the file cannot be read or the probe's output is not a version.
pub fn detect_with(path: &Path, probe: impl FnOnce(&Path) -> Option<String>) -> Option<Version> {
    let key = FileKey::of(path)?;
    {
        let cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((cached_key, version)) = cache.as_ref().and_then(|c| c.get(path))
            && *cached_key == key
        {
            return *version;
        }
    }
    let version = probe(path).as_deref().and_then(parse_version);
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    cache
        .get_or_insert_with(HashMap::new)
        .insert(path.to_path_buf(), (key, version));
    version
}

/// [`detect_with`], asking the binary itself.
pub fn detect(path: &Path) -> Option<Version> {
    detect_with(path, run_version)
}

/// Run `<path> --version` with no stdin, bounded by [`PROBE_TIMEOUT`]; its
/// stdout, or `None` when it cannot be started, fails or times out.
/// Blocking: callers run it off the async executor.
fn run_version(path: &Path) -> Option<String> {
    use std::io::Read as _;
    let mut child = std::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            Ok(None) if started.elapsed() < PROBE_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
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

/// The program token of `command`: its first whitespace-separated word, when
/// that names an OpenCode binary (`opencode`, `opencode2`, `opencode-1.18.35`,
/// a path ending in one of those). A quoted or otherwise unusual first word
/// is not one: the deck does not parse shell syntax here.
fn opencode_program(command: &str) -> Option<&str> {
    let program = command.split_whitespace().next()?;
    if program.contains(['"', '\'', '$', '`', '\\', '=']) {
        return None;
    }
    let name = Path::new(program).file_name()?.to_str()?;
    name.starts_with("opencode").then_some(program)
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
    spawn_target_with(command, path_env, detect)
}

/// OpenCode 2.x subcommands that start a session and take `--standalone`
/// (2.2.5 `--help`): `mini`, `run` and `session`. Any other subcommand
/// (`plugin`, `auth`, `serve`, …) is not an agent session and is left alone.
const STANDALONE_SUBCOMMANDS: [&str; 3] = ["mini", "run", "session"];

/// PRD #2050: `command` with `--standalone` added, for an OpenCode 2.x the
/// deck starts directly, so its server plugins run in a private server that
/// inherits the pane's environment rather than in the shared background
/// service (which carries whichever environment first started it). `None`
/// when nothing changes: the command is not a direct OpenCode invocation,
/// already says `--standalone` or `--server` (connecting to a named server),
/// or runs a subcommand that is not a session.
///
/// The flag goes right after the program (or after `mini`/`run`/`session`),
/// and the rest of the command is kept byte for byte, so a quoted prompt or
/// a trailing directory argument is not disturbed.
pub fn with_standalone(command: &str) -> Option<String> {
    let program = opencode_program(command)?;
    let words: Vec<&str> = command.split_whitespace().collect();
    if words.iter().any(|w| {
        *w == "--standalone"
            || w.starts_with("--standalone=")
            || *w == "--server"
            || w.starts_with("--server=")
    }) {
        return None;
    }
    // Byte offset just past the program word.
    let program_start = command.find(program)?;
    let mut insert_at = program_start + program.len();
    match words.get(1) {
        Some(sub) if STANDALONE_SUBCOMMANDS.contains(sub) => {
            let sub_start = insert_at + command[insert_at..].find(sub)?;
            insert_at = sub_start + sub.len();
        }
        // A flag, a directory argument, or nothing: the top-level TUI.
        Some(word) if word.starts_with('-') || word.contains('/') || *word == "." => {}
        None => {}
        // Any other word: another subcommand (or a bare directory name the
        // deck cannot tell from one), left alone rather than guessed at.
        Some(_) => return None,
    }
    Some(format!(
        "{} --standalone{}",
        &command[..insert_at],
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
