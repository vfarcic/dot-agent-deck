//! Bundling + materialization of the Pi orchestrator extension, and the pure
//! decision core for `dot-agent-deck orchestrator setup` (PRD #201, M3.1 + M3.2).
//!
//! Design Decision #1: we bundle the *glue* and detect the *engine*. The Pi
//! runtime is a detected PATH dependency (like `claude`/`opencode`); the one
//! thing that gives us deterministic control — our TypeScript orchestrator
//! extension — is compiled into this binary via [`include_str!`] and
//! materialized on demand into Pi's global extension directory.
//!
//! Keeping the embedded strings pointed at the real `pi-extension/src/` sources
//! (not a fork under `src/`) means a future edit to the extension flows into the
//! binary on the next `cargo build` with no copy step to forget.

use std::path::{Path, PathBuf};

/// The one command that installs Pi (verified). Printed verbatim when `pi` is
/// not on PATH so the user can copy-paste it. Kept as a constant so the CLI
/// wrapper and the `run_setup` core agree on the exact string.
pub const PI_INSTALL_HINT: &str = "npm install -g @earendil-works/pi-coding-agent";

/// The subdirectory name the extension is materialized into under Pi's
/// extensions dir. Pi auto-discovers `<extensions>/<name>/index.ts` (the
/// "subdirectory" layout documented for Pi 0.80.6), so materializing into this
/// dir is what *enables* the extension — there is no separate enable step.
pub const EXTENSION_DIR_NAME: &str = "dot-agent-deck";

/// Pi's own env var (`PI_CODING_AGENT_DIR`) that relocates its agent directory —
/// the dir that holds `extensions/`. Pi resolves it in `getAgentDir()`:
/// `$PI_CODING_AGENT_DIR` (tilde-expanded) if set, else `~/.pi/agent`.
/// dot-agent-deck mirrors that EXACT precedence so the bundled extension is
/// always materialized where pi will actually look for it:
///   - **production correctness** — a user who relocates pi via this var still
///     gets a status-tracked pane (materializing into `~/.pi` would miss); and
///   - **test isolation** — tests set it to a throwaway dir, so the suite never
///     writes the developer's real `~/.pi`.
pub const ENV_PI_AGENT_DIR: &str = "PI_CODING_AGENT_DIR";

/// The embedded extension files, as `(filename, contents)` pairs.
///
/// The files materialized beside the entry point, each after the files it
/// imports, which is the order they are published in (PR #1735 review
/// 4309809295):
///   - `inbox-receiver.js`  — the deck's inbox receiver, generated from
///     `src/inbox_receiver.js` ([`crate::inbox_receiver::pi_module`]; PRD
///     #1666), which `index.ts` imports. Its `.d.ts` is for the extension's
///     type check only and is not materialized,
///   - `orchestrator.ts`    — the pure logic `index.ts` imports via
///     `./orchestrator.ts`,
///   - `telemetry.ts`       — Pi's own OTLP export, inert unless the deck
///     switches it on (PRD #1741), which `index.ts` imports,
///   - `usage-file.ts`      — the per-turn usage file writer (#1932), which
///     `index.ts` imports and which itself imports `./orchestrator.ts`, and
///   - `index.ts`          — the Pi-API glue (default-export factory).
///
/// A `package.json` is intentionally **not** embedded: the subdirectory
/// `index.ts` discovery layout does not require one: the extension's one
/// package import (`typebox`) plus its type-only `@earendil-works/pi-coding-agent`
/// import are resolved from Pi's own installation when jiti loads the extension,
/// and `node:child_process` is a Node built-in.
///
/// `include_str!` paths are relative to this source file (`src/`), so they
/// reference the real `pi-extension/` sources and stay in sync on rebuild.
pub const EXTENSION_FILES: &[(&str, &str)] = &[
    (
        "inbox-receiver.js",
        include_str!("../pi-extension/src/inbox-receiver.js"),
    ),
    (
        "orchestrator.ts",
        include_str!("../pi-extension/src/orchestrator.ts"),
    ),
    (
        "telemetry.ts",
        include_str!("../pi-extension/src/telemetry.ts"),
    ),
    (
        "usage-file.ts",
        include_str!("../pi-extension/src/usage-file.ts"),
    ),
    ("index.ts", include_str!("../pi-extension/src/index.ts")),
];

/// Pi's agent directory under a given HOME: `<home>/.pi/agent` (the default
/// branch of pi's `getAgentDir()`, used when [`ENV_PI_AGENT_DIR`] is unset).
fn agent_dir_under_home(home: &Path) -> PathBuf {
    home.join(".pi").join("agent")
}

/// The `extensions/dot-agent-deck` subpath pi discovers inside an agent dir. The
/// single place the `<agent-dir>/extensions/<name>` layout is encoded, so the
/// HOME-based and `PI_CODING_AGENT_DIR`-based paths can't diverge.
fn extension_dir_in(agent_dir: &Path) -> PathBuf {
    agent_dir.join("extensions").join(EXTENSION_DIR_NAME)
}

/// Build the extension target dir under a given HOME:
/// `<home>/.pi/agent/extensions/dot-agent-deck`. The `PI_CODING_AGENT_DIR`-unset
/// layout; kept for the HOME-based resolvers and tests.
pub fn extension_dir_under(home: &Path) -> PathBuf {
    extension_dir_in(&agent_dir_under_home(home))
}

/// Expand a leading `~` / `~/` in a `PI_CODING_AGENT_DIR` value against `home`,
/// matching pi's `expandTildePath`. A bare `~` becomes `home`; `~/x` becomes
/// `home/x`; everything else (absolute or relative) passes through unchanged.
/// With no HOME to expand against, a `~`-prefixed value is returned verbatim
/// (pi would do the same fallback).
fn expand_tilde(value: &str, home: Option<&str>) -> PathBuf {
    match (value, home) {
        ("~", Some(h)) => PathBuf::from(h),
        (v, Some(h)) if v.starts_with("~/") => Path::new(h).join(&v[2..]),
        _ => PathBuf::from(value),
    }
}

/// Pure core mirroring pi's `getAgentDir()`: `$PI_CODING_AGENT_DIR`
/// (tilde-expanded against `home`) if set & non-empty, else `<home>/.pi/agent`.
/// `None` only when NEITHER the override nor `home` yields a non-empty value.
/// Pure over its inputs so every branch is testable without mutating
/// process-global env.
fn resolve_agent_dir_inner(
    agent_dir_override: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    if let Some(dir) = agent_dir_override.filter(|v| !v.is_empty()) {
        return Some(expand_tilde(dir, home));
    }
    home.filter(|v| !v.is_empty())
        .map(|h| agent_dir_under_home(Path::new(h)))
}

/// The Pi agent dir the EXPLICIT `orchestrator setup` path targets, resolved from
/// the process env: `$PI_CODING_AGENT_DIR` if set & non-empty, else
/// `<HOME>/.pi/agent`. `None` only when neither yields a non-empty value — the
/// CLI then errors rather than guess a location pi will never load.
///
/// **Windows: deliberately still a raw `$HOME` read (PRD #163 M1, reconfirmed in
/// review), not a missed platform-seam site.** `$HOME` is normally unset on
/// Windows, so this returns `None` and the caller reports "nowhere to install"
/// instead of materializing the extension somewhere. Resolving `%USERPROFILE%`
/// here would be *worse* than failing: the directory pi actually scans on Windows
/// is pi's decision, and the two named above are the only inputs pi itself reads
/// (`getAgentDir()`), which is the invariant this function exists to mirror. An
/// extension written to a path pi never scans is indistinguishable from success
/// and silently does nothing. `$PI_CODING_AGENT_DIR` is honoured on every
/// platform, so a Windows user has an explicit, correct way in.
fn agent_dir_strict() -> Option<PathBuf> {
    resolve_agent_dir_inner(
        std::env::var(ENV_PI_AGENT_DIR).ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// The Pi extensions dir the bundled extension is materialized into by the
/// EXPLICIT `orchestrator setup` path: `<agent-dir>/extensions/dot-agent-deck`,
/// where the agent dir is `$PI_CODING_AGENT_DIR` if set else `~/.pi/agent`.
///
/// Returns `None` when neither `PI_CODING_AGENT_DIR` nor HOME is set/non-empty:
/// the CLI must NOT guess a `/tmp` (unset) or relative (`./`, empty) location Pi
/// will never load — it errors instead. Matches the auto path's SKIP
/// (see [`auto_materialize`]); both refuse to guess.
pub fn default_extension_dir() -> Option<PathBuf> {
    agent_dir_strict().map(|dir| extension_dir_in(&dir))
}

/// Materialize the bundled extension into `target_dir` in the layout Pi
/// discovers: one `<target_dir>/<name>` per [`EXTENSION_FILES`] entry.
///
/// Creates `target_dir` (and parents) if needed and (over)writes each embedded
/// file, so re-running is idempotent and refreshes a stale copy. Each file is
/// published by an atomic rename of a temp file created exclusively and
/// owner-only, then given the existing destination's mode (owner-only for a
/// new file) before it is written, and a destination that is a symlink is
/// refused (PRD #1666 hardening M8). Every destination is judged and every
/// changed file written to its temp file before any is renamed, and the
/// renames run in embed order, each file after those it imports: a refusal or
/// a failed write changes nothing, and a failed rename leaves `index.ts` the
/// old one (PR #1735 review 4309809295). Each rename is atomic on its own; the
/// set is not a transaction, so a failed rename partway through, a crash, a
/// concurrent writer or a concurrent load can see a mix of old and new files.
/// Returns the
/// list of paths written, in embed order. Hermetic — pass any directory
/// (a temp dir in tests, the real `~/.pi/...` in the CLI); it never reads the
/// environment.
pub fn materialize(target_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    // PRD #1487: every destination is judged before the directory exists, so a
    // refused test write creates nothing. Each file is judged on its own as
    // well, because `std::fs::write` follows a symlink planted at its name.
    crate::config_write_guard::ensure_config_write_allowed(target_dir)?;
    for (name, _) in EXTENSION_FILES {
        crate::config_write_guard::ensure_config_write_allowed(&target_dir.join(name))?;
    }
    std::fs::create_dir_all(target_dir)?;
    for (name, _) in EXTENSION_FILES {
        let path = target_dir.join(name);
        // PRD #1666 hardening M8: a destination that is a symlink is refused,
        // so the extension is never written through a link to another file.
        if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "{} is a symlink; remove it so the deck can write its Pi extension there",
                    path.display()
                ),
            ));
        }
    }
    let mut written = Vec::with_capacity(EXTENSION_FILES.len());
    let mut staged = Vec::with_capacity(EXTENSION_FILES.len());
    for (name, contents) in EXTENSION_FILES {
        let path = target_dir.join(name);
        // An identical file is not rewritten (PRD #1487).
        if !std::fs::read(&path).is_ok_and(|existing| existing == contents.as_bytes()) {
            // Hardening M8: a temp file created exclusively and owner-only,
            // given the existing destination's mode, written, then renamed
            // over the destination, so Pi never loads a partial file
            // and a concurrent refresh never truncates one in place. A staged
            // file dropped unpublished is removed.
            staged.push(crate::agent_hook_config::stage_atomic(
                target_dir,
                &path,
                contents.as_bytes(),
            )?);
        }
        written.push(path);
    }
    for file in staged {
        file.publish()?;
    }
    Ok(written)
}

// ---------------------------------------------------------------------------
// PRD #201: daemon-startup auto-materialize
// ---------------------------------------------------------------------------
//
// Pi should need NO manual setup — parity with claude (hooks auto-install,
// `hooks_manage::auto_install`) and opencode (plugin auto-install,
// `opencode_manage::auto_install`), which install ONCE at startup. The bundled
// extension is materialized once when the daemon starts serving (the
// `dot-agent-deck daemon serve` entry, which both the lazy-spawned daemon and a
// headless serve go through), and again only for a spawn detected as Pi
// ([`refresh_for_spawn`], PRD #1666 G6).
//
// Why startup is not keyed off the command: the deck must not care HOW pi is
// launched, so a basename cannot be the only trigger (a wrapper like `devbox
// run pi-big` would be missed). Materializing per-spawn regardless of command
// instead meant every unrelated agent start (claude, a shell, a fast test)
// rewrote `~/.pi` whenever pi happened to be on PATH. So startup covers every
// launch command, and the per-spawn refresh covers what startup cannot: a Pi
// whose own `HOME` or `PI_CODING_AGENT_DIR` points elsewhere, and an extension
// another deck overwrote since. A wrapper gets the startup copy only.
//
// Location mirrors pi's own `getAgentDir()`: `$PI_CODING_AGENT_DIR` if set, else
// `<HOME>/.pi/agent`, + `/extensions/dot-agent-deck` (see [`ENV_PI_AGENT_DIR`]).
// Guarded (only when pi is present), idempotent (overwrite), and unset-safe
// (SKIP, never a `/tmp` guess).

/// Look up `key` in the env overlay first, its last entry being the one the
/// child gets ([`crate::agent_pty::overlay_value`]), then the process env.
/// `None` if absent in both.
fn lookup_env(env: &[(String, String)], key: &str) -> Option<String> {
    crate::agent_pty::overlay_value(env, key)
        .map(str::to_string)
        .or_else(|| std::env::var(key).ok())
}

/// Resolve the Pi agent dir the daemon should materialize the extension into,
/// mirroring pi's `getAgentDir()`: `$PI_CODING_AGENT_DIR` (tilde-expanded) if set
/// & non-empty, else `<HOME>/.pi/agent`. `None` only when neither the override
/// nor HOME yields a non-empty value (then auto-materialize SKIPs rather than
/// guess). Consults the env overlay before the process env for both keys so a
/// daemon/test that overrides them is judged on the effective environment.
fn resolve_agent_dir_from_env(env: &[(String, String)]) -> Option<PathBuf> {
    resolve_agent_dir_inner(
        lookup_env(env, ENV_PI_AGENT_DIR).as_deref(),
        lookup_env(env, "HOME").as_deref(),
    )
}

/// Where a Pi child starts, which is where its relative agent dir resolves.
pub enum ChildDir<'a> {
    /// The daemon's own directory, which the child inherits.
    Inherited,
    /// The pathname the child enters.
    Path(&'a Path),
    /// A prepared start's verified directory object, held open, which the
    /// refresh writes beneath whatever its pathname `path` names by then. On
    /// Linux the child enters that object too; on other Unix it enters `path`
    /// after an identity check, with the window [`crate::agent_pty::spawn_in`]
    /// documents.
    #[cfg(unix)]
    Held {
        dir: std::os::fd::BorrowedFd<'a>,
        path: &'a Path,
    },
}

/// PRD #1666 G6: rewrite the extension into the directory a Pi child spawned
/// with the env overlay `env` in `cwd` resolves (its `PI_CODING_AGENT_DIR`,
/// else its `HOME`, each from the overlay first), before the child starts, so
/// it loads the receiver this deck checks it against. A relative directory is
/// the child's, so it is taken from `cwd`: for [`ChildDir::Held`] every
/// directory and file is created relative to the held descriptor
/// ([`materialize_beneath`]), so a rename-and-replace of the project's
/// pathname cannot redirect a write into the replacement. Returns what
/// failed: the target directory and the error, so the daemon's warning names
/// where; empty when the files are current or no directory resolves (nothing
/// to refresh).
pub fn refresh_for_spawn(env: &[(String, String)], cwd: ChildDir<'_>) -> Vec<String> {
    let Some(agent_dir) = resolve_agent_dir_from_env(env) else {
        return Vec::new();
    };
    let extension = extension_dir_in(&agent_dir);
    let (target, written) = match cwd {
        #[cfg(unix)]
        ChildDir::Held { dir, path } if extension.is_relative() => {
            let target = path.join(&extension);
            let written = materialize_beneath(dir, &extension, &target);
            (target, written)
        }
        #[cfg(unix)]
        ChildDir::Held { .. } => {
            let written = materialize(&extension).map(drop);
            (extension, written)
        }
        ChildDir::Path(cwd) => {
            let target = cwd.join(&extension);
            let written = materialize(&target).map(drop);
            (target, written)
        }
        ChildDir::Inherited => {
            let written = materialize(&extension).map(drop);
            (extension, written)
        }
    };
    match written {
        Ok(()) => Vec::new(),
        Err(error) => vec![format!("{}: {error}", target.display())],
    }
}

/// [`materialize`], with `relative` (the extension directory) resolved
/// beneath the directory `base` holds open rather than by pathname: each
/// component is created (`mkdirat`) and opened (`openat`) from the one before
/// it, and each file is judged, compared and published by an atomic rename
/// (`renameat`) of a temp file in the directory reached that way, created
/// owner-only and given the existing destination's mode (owner-only for a
/// new file) before it is written. As in [`materialize`], each rename is
/// atomic on its own and the set is not a transaction.
/// `shown` is the pathname the target had when it was resolved, for the test
/// containment guard and for messages; nothing is written through it. A `..`
/// or a symlinked component is followed, as the child following the same
/// relative path would.
#[cfg(unix)]
pub fn materialize_beneath(
    base: std::os::fd::BorrowedFd<'_>,
    relative: &Path,
    shown: &Path,
) -> std::io::Result<()> {
    use std::io::{Read as _, Write as _};

    // PRD #1487: judged before anything exists, as `materialize` judges them.
    crate::config_write_guard::ensure_config_write_allowed(shown)?;
    for (name, _) in EXTENSION_FILES {
        crate::config_write_guard::ensure_config_write_allowed(&shown.join(name))?;
    }
    let mut dir = beneath::open_dir(base, std::ffi::OsStr::new("."))?;
    for component in relative.components() {
        use std::path::Component;
        let name = match component {
            Component::CurDir => continue,
            Component::ParentDir => std::ffi::OsStr::new(".."),
            Component::Normal(name) => {
                beneath::make_dir(&dir, name)?;
                name
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{} is not a relative directory", relative.display()),
                ));
            }
        };
        dir = beneath::open_dir(std::os::fd::AsFd::as_fd(&dir), name)?;
    }
    // As in `materialize`: every file is judged, then every changed one
    // staged, before any is renamed, in embed order.
    let mut existing = Vec::with_capacity(EXTENSION_FILES.len());
    for (name, _) in EXTENSION_FILES {
        let name = std::ffi::OsStr::new(name);
        let mode = beneath::lstat(&dir, name)?;
        // PRD #1666 hardening M8, as in `materialize`.
        if mode.is_some_and(|mode| mode & libc::S_IFMT == libc::S_IFLNK) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "{} is a symlink; remove it so the deck can write its Pi extension there",
                    shown.join(name).display()
                ),
            ));
        }
        existing.push(mode);
    }
    let mut staged = beneath::Staged::new(&dir);
    for ((name, contents), existing) in EXTENSION_FILES.iter().zip(existing) {
        let name = std::ffi::OsStr::new(name);
        // An identical file is not rewritten (PRD #1487).
        if existing.is_some() {
            let mut current = Vec::new();
            if beneath::open_file(&dir, name)
                .and_then(|mut file| file.read_to_end(&mut current))
                .is_ok()
                && current == contents.as_bytes()
            {
                continue;
            }
        }
        let landed = existing.map_or(0o600, |mode| mode & 0o777);
        let (mut file, tmp) = beneath::create_temp(&dir, name)?;
        staged.push(tmp, name);
        beneath::set_mode(&file, landed)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    staged.publish()
}

/// The `*at` system calls [`materialize_beneath`] is made of, each relative
/// to a directory descriptor.
#[cfg(unix)]
mod beneath {
    use std::ffi::{CString, OsStr, OsString};
    use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;

    fn c_name(name: &OsStr) -> std::io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "a name holds a NUL byte")
        })
    }

    fn opened(fd: libc::c_int) -> std::io::Result<OwnedFd> {
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a descriptor `openat` just returned, owned by nothing else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn checked(result: libc::c_int) -> std::io::Result<()> {
        if result < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Open the directory `name` beneath `at`.
    pub(super) fn open_dir(at: BorrowedFd<'_>, name: &OsStr) -> std::io::Result<OwnedFd> {
        let name = c_name(name)?;
        // SAFETY: a valid descriptor and a NUL-terminated name.
        opened(unsafe {
            libc::openat(
                at.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        })
    }

    /// Create the directory `name` beneath `at` unless something is there.
    pub(super) fn make_dir(at: &OwnedFd, name: &OsStr) -> std::io::Result<()> {
        let name = c_name(name)?;
        // SAFETY: a valid descriptor and a NUL-terminated name.
        match checked(unsafe { libc::mkdirat(at.as_raw_fd(), name.as_ptr(), 0o777) }) {
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            other => other,
        }
    }

    /// The mode of `name` beneath `at`, not following a symlink, or `None`
    /// when nothing is there.
    pub(super) fn lstat(at: &OwnedFd, name: &OsStr) -> std::io::Result<Option<libc::mode_t>> {
        let name = c_name(name)?;
        // SAFETY: `stat` is plain data, filled in by the call before it is read.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: a valid descriptor, a NUL-terminated name and a buffer.
        let result = unsafe {
            libc::fstatat(
                at.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        match checked(result) {
            Ok(()) => Ok(Some(stat.st_mode)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Open the file `name` beneath `at` to read it, never through a symlink.
    pub(super) fn open_file(at: &OwnedFd, name: &OsStr) -> std::io::Result<std::fs::File> {
        let name = c_name(name)?;
        // SAFETY: a valid descriptor and a NUL-terminated name.
        opened(unsafe {
            libc::openat(
                at.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })
        .map(std::fs::File::from)
    }

    /// Exclusively create an owner-only temp file for a publish of `name`
    /// beneath `at`, under the name `agent_hook_config`'s publishes use,
    /// redrawn on a collision.
    pub(super) fn create_temp(
        at: &OwnedFd,
        name: &OsStr,
    ) -> std::io::Result<(std::fs::File, OsString)> {
        let mut last = None;
        for _ in 0..16 {
            let drawn = crate::agent_hook_config::temp_path(
                std::path::Path::new(""),
                &name.to_string_lossy(),
            )
            .into_os_string();
            let tmp = c_name(&drawn)?;
            // SAFETY: a valid descriptor and a NUL-terminated name.
            let fd = unsafe {
                libc::openat(
                    at.as_raw_fd(),
                    tmp.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600 as libc::c_uint,
                )
            };
            match opened(fd) {
                Ok(fd) => return Ok((std::fs::File::from(fd), drawn)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    last = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not create a temp file for the publish",
            )
        }))
    }

    /// Give `file` the mode `mode`, which no umask filters.
    pub(super) fn set_mode(file: &std::fs::File, mode: libc::mode_t) -> std::io::Result<()> {
        // SAFETY: a valid descriptor.
        checked(unsafe { libc::fchmod(file.as_raw_fd(), mode) })
    }

    /// Rename `from` over `to`, both beneath `at`.
    pub(super) fn rename(at: &OwnedFd, from: &OsStr, to: &OsStr) -> std::io::Result<()> {
        let (from, to) = (c_name(from)?, c_name(to)?);
        let fd = at.as_fd().as_raw_fd();
        // SAFETY: a valid descriptor and NUL-terminated names.
        checked(unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) })
    }

    /// Remove an abandoned temp file `name` beneath `at`, best effort.
    pub(super) fn unlink(at: &OwnedFd, name: &OsStr) {
        if let Ok(name) = c_name(name) {
            // SAFETY: a valid descriptor and a NUL-terminated name.
            unsafe { libc::unlinkat(at.as_raw_fd(), name.as_ptr(), 0) };
        }
    }

    /// Temp files beneath one directory, each staged for its destination
    /// name. [`Staged::publish`] renames them in the order they were pushed;
    /// any it has not renamed when it returns or is dropped are removed.
    pub(super) struct Staged<'a> {
        at: &'a OwnedFd,
        files: Vec<(OsString, OsString)>,
    }

    impl<'a> Staged<'a> {
        pub(super) fn new(at: &'a OwnedFd) -> Self {
            Self {
                at,
                files: Vec::new(),
            }
        }

        pub(super) fn push(&mut self, tmp: OsString, name: &OsStr) {
            self.files.push((tmp, name.to_owned()));
        }

        pub(super) fn publish(mut self) -> std::io::Result<()> {
            while !self.files.is_empty() {
                let (tmp, name) = &self.files[0];
                rename(self.at, tmp, name)?;
                self.files.remove(0);
            }
            Ok(())
        }
    }

    impl Drop for Staged<'_> {
        fn drop(&mut self) {
            for (tmp, _) in &self.files {
                unlink(self.at, tmp);
            }
        }
    }
}

/// Whether `pi` is discoverable on the PATH the spawned child will actually
/// use: a `PATH` entry in the spawn env overlay if present, else the process
/// `PATH`. The seam resolves pi-presence from the child's effective env (not
/// merely the daemon's own PATH) so a caller that overrides `PATH` in
/// `opts.env` — as the real-pi e2e tests do — is judged against the same PATH
/// the child inherits.
fn pi_present_for_env(env: &[(String, String)]) -> bool {
    let path = crate::agent_pty::overlay_value(env, "PATH")
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"));
    match path {
        Some(p) => path_contains_binary(&p, "pi"),
        None => false,
    }
}

/// Pure decision core for the spawn-time auto-materialize (mirrors
/// [`run_setup`]'s testable shape). Given whether pi is present and the
/// resolved target dir (`None` = HOME unset), returns the paths written, or
/// `None` when it SKIPPED. No env / PATH reads, so the fast-tier test exercises
/// every branch — present/absent, HOME-set/unset, idempotent re-run — without
/// mutating process-global state.
///
///  - `!pi_present`        → SKIP (pi absent — nothing to enable).
///  - `target_dir == None` → SKIP (HOME unset — do NOT write to a `/tmp` guess).
///  - otherwise            → (over)write the bundled files into `target_dir`
///    ([`materialize`] is idempotent), returning the paths written.
pub fn auto_materialize_core(
    pi_present: bool,
    target_dir: Option<&Path>,
) -> std::io::Result<Option<Vec<PathBuf>>> {
    if !pi_present {
        return Ok(None);
    }
    match target_dir {
        Some(target) => Ok(Some(materialize(target)?)),
        None => Ok(None),
    }
}

/// Daemon-startup entry: silently (over)materialize the bundled Pi orchestrator
/// extension so a pi agent needs ZERO manual setup. Called ONCE from the
/// `dot-agent-deck daemon serve` entry (covering both the lazy-spawned daemon and
/// a headless serve) — NOT per spawn and NOT gated on any command, so it works
/// whether pi is launched as `pi`, an absolute path, or a wrapper (`devbox run …`,
/// `run_agent.sh`). `env` is an optional overlay consulted before the process env
/// for `PI_CODING_AGENT_DIR` / `HOME` / `PATH`; the daemon passes `&[]` to use its
/// own environment. The self-guard is pi-presence ([`pi_present_for_env`]), so a
/// daemon on a machine without pi is a cheap no-op. Idempotent (overwrite) and
/// unset-safe (SKIP, no `/tmp` write). Best-effort — a write failure is logged,
/// never fatal (matching `hooks_manage::auto_install`).
pub fn auto_materialize(env: &[(String, String)]) {
    let target = resolve_agent_dir_from_env(env).map(|dir| extension_dir_in(&dir));
    match auto_materialize_core(pi_present_for_env(env), target.as_deref()) {
        Ok(Some(written)) => tracing::info!(
            count = written.len(),
            "auto-materialized the Pi orchestrator extension into the Pi agent dir"
        ),
        Ok(None) => tracing::debug!(
            "auto-materialize: skipped the Pi extension (pi absent, or PI_CODING_AGENT_DIR/HOME unset)"
        ),
        Err(e) => {
            tracing::warn!("auto-materialize: failed to write the Pi orchestrator extension: {e}")
        }
    }
}

/// The outcome of the pure `orchestrator setup` core, independent of the real
/// PATH and real `~/.pi`. The thin CLI wrapper turns this into stdout/stderr +
/// an exit code.
#[derive(Debug)]
pub struct SetupReport {
    /// `true` → the extension was materialized (CLI exits zero); `false` → `pi`
    /// was absent and nothing was written (CLI exits non-zero).
    pub success: bool,
    /// Human-facing text: printed to stdout on success, stderr on failure. On
    /// the absent path it contains [`PI_INSTALL_HINT`] verbatim on its own line.
    pub message: String,
    /// Absolute paths materialized (empty when `pi` is absent).
    pub written: Vec<PathBuf>,
}

/// Pure decision core for `dot-agent-deck orchestrator setup`.
///
/// Takes an explicit `pi_present` flag and a `target_dir` so both branches are
/// testable without touching the real PATH or the real `~/.pi`. The CLI wrapper
/// supplies [`pi_on_path`] + [`default_extension_dir`].
///
///  - `pi` absent  → no filesystem writes; a failure report whose message names
///    the exact install command ([`PI_INSTALL_HINT`]).
///  - `pi` present → materialize the extension into `target_dir` and report
///    success, naming the files written.
pub fn run_setup(pi_present: bool, target_dir: &Path) -> std::io::Result<SetupReport> {
    if !pi_present {
        return Ok(SetupReport {
            success: false,
            message: format!(
                "pi is not installed or not on PATH — cannot enable the orchestrator extension.\n\
                 Install pi, then re-run `dot-agent-deck orchestrator setup`:\n\
                 {PI_INSTALL_HINT}"
            ),
            written: Vec::new(),
        });
    }

    let written = materialize(target_dir)?;
    let files = written
        .iter()
        .map(|p| format!("  {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(SetupReport {
        success: true,
        message: format!(
            "Enabled the dot-agent-deck orchestrator extension for Pi.\n\
             Materialized the bundled extension into {dir}:\n\
             {files}\n\
             Pi auto-discovers it there. Set `command = \"pi\"` for your orchestrator role \
             in .dot-agent-deck.toml to use it.",
            dir = target_dir.display()
        ),
        written,
    })
}

/// Whether a `pi` executable is discoverable on `PATH`. Used by the CLI wrapper
/// only; the pure [`run_setup`] core takes the boolean directly so tests never
/// depend on the machine's real PATH.
pub fn pi_on_path() -> bool {
    match std::env::var_os("PATH") {
        Some(path) => path_contains_binary(&path, "pi"),
        None => false,
    }
}

/// Scan a `PATH`-shaped value for a regular, *executable* file named `name`.
/// Pure over its `path` argument (no environment read) so it is testable without
/// mutating the process-global `PATH` — see the note on global-state races under
/// CI's shared `cargo test` process. Follows symlinks (Pi's launcher is
/// typically a symlink into its npm install) and requires a regular file, so a
/// directory of that name in a `PATH` entry does not count.
fn path_contains_binary(path: &std::ffi::OsStr, name: &str) -> bool {
    std::env::split_paths(path).any(|dir| is_executable_file(&dir.join(name)))
}

/// Whether `candidate` is a regular file that is *also* executable.
///
/// `is_file()` alone is not enough: a regular but non-executable file named `pi`
/// on `PATH` would make `orchestrator setup` falsely report success, only for
/// the Pi pane to fail to spawn later (Greptile #2). On Unix we additionally
/// require at least one exec bit (`mode & 0o111 != 0`); a non-executable
/// candidate is treated as "pi not usable", so the setup core takes the
/// not-present branch (prints the install hint, exits non-zero). On non-Unix
/// targets there is no cheap exec-bit check, so a regular file is accepted.
/// `metadata()` follows symlinks, matching `is_file()`'s symlink behavior.
fn is_executable_file(candidate: &Path) -> bool {
    if !candidate.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(candidate) {
            Ok(meta) => meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    // ---- M3.1 (test-plan row 11): materialize into a temp dir -------------

    /// Row 11: `materialize` writes exactly the embedded files into the target
    /// dir, byte-for-byte equal to the compiled-in strings.
    #[test]
    fn materialize_writes_embedded_files_to_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("dot-agent-deck");

        let written = materialize(&target).unwrap();

        // One path per embedded file, in embed order.
        assert_eq!(written.len(), EXTENSION_FILES.len());
        for (name, contents) in EXTENSION_FILES {
            let path = target.join(name);
            assert!(
                written.contains(&path),
                "expected {} in the written list",
                path.display()
            );
            let on_disk = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("expected materialized {}: {e}", path.display()));
            assert_eq!(
                &on_disk, contents,
                "{} must match the embedded source byte-for-byte",
                name
            );
        }
    }

    /// Scenario: A Pi child's relative agent dir is refreshed for a prepared start whose project directory is renamed away and replaced after the start was checked, before the extension is written. The extension is created in the directory object the start verified and holds open, where the child will enter, and nothing is written into the replacement; a symlink planted at a file's name there is refused, and a current file is left as it is.
    #[spec("inbox/boundary/032")]
    #[cfg(unix)]
    #[test]
    fn boundary_032_a_prepared_refresh_writes_beneath_the_held_directory_after_a_swap() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let held = crate::project_resolve::VerifiedProjectDir::open(&project).unwrap();
        // The swap lands after the start's own check, before the write.
        let moved = root.path().join("project-moved");
        std::fs::rename(&project, &moved).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let env = vec![(ENV_PI_AGENT_DIR.to_string(), ".pi-agent".to_string())];
        let held_dir = || ChildDir::Held {
            dir: std::os::fd::AsFd::as_fd(&held),
            path: &project,
        };
        assert_eq!(refresh_for_spawn(&env, held_dir()), Vec::<String>::new());
        let landed = moved
            .join(".pi-agent")
            .join("extensions")
            .join(EXTENSION_DIR_NAME);
        for (name, contents) in EXTENSION_FILES {
            assert_eq!(
                std::fs::read_to_string(landed.join(name)).unwrap(),
                *contents
            );
            let mode = std::fs::metadata(landed.join(name)).unwrap().permissions();
            assert_eq!(
                std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
                0o600
            );
        }
        assert!(
            !project.join(".pi-agent").exists(),
            "nothing is written into the replacement"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&landed)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "no temp file left: {leftovers:?}");

        // A current file is not rewritten.
        let (first, _) = EXTENSION_FILES[0];
        let inode =
            |path: &Path| std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(path).unwrap());
        let before = inode(&landed.join(first));
        assert_eq!(refresh_for_spawn(&env, held_dir()), Vec::<String>::new());
        assert_eq!(
            inode(&landed.join(first)),
            before,
            "an identical file stays"
        );

        // A symlink at a file's name is refused, and its target kept.
        let victim = root.path().join("victim.txt");
        std::fs::write(&victim, "keep me").unwrap();
        std::fs::remove_file(landed.join(first)).unwrap();
        std::os::unix::fs::symlink(&victim, landed.join(first)).unwrap();
        let failed = refresh_for_spawn(&env, held_dir());
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert!(failed[0].contains("is a symlink"), "{failed:?}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
    }

    /// Hardening M8: a destination that is a symlink is refused, and the file
    /// it points at is left exactly as it was — materialising never writes
    /// through a link.
    #[cfg(unix)]
    #[test]
    fn materialize_refuses_a_symlinked_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("dot-agent-deck");
        std::fs::create_dir_all(&target).unwrap();
        let victim = tmp.path().join("victim.txt");
        std::fs::write(&victim, "keep me").unwrap();
        std::os::unix::fs::symlink(&victim, target.join("index.ts")).unwrap();
        assert!(materialize(&target).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
        assert!(
            std::fs::symlink_metadata(target.join("index.ts"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    /// A stale extension, as an older deck left it, in `target`.
    #[cfg(unix)]
    fn stale_extension(target: &Path) {
        std::fs::create_dir_all(target).unwrap();
        for (name, _) in EXTENSION_FILES {
            std::fs::write(target.join(name), format!("// old {name}")).unwrap();
        }
    }

    /// Both ways the deck writes the extension: by pathname, and beneath a
    /// held directory.
    #[cfg(unix)]
    fn both_materializations(target: &Path) -> [std::io::Result<()>; 2] {
        let by_path = materialize(target).map(drop);
        let parent = std::fs::File::open(target.parent().unwrap()).unwrap();
        let name = Path::new(target.file_name().unwrap());
        let held = materialize_beneath(std::os::fd::AsFd::as_fd(&parent), name, target);
        [by_path, held]
    }

    /// PR #1735 review 4309809295: a file that cannot be published leaves the
    /// old `index.ts`, never a new one importing a module that is not there.
    /// Here the receiver's name is taken by a directory, so its rename fails:
    /// `index.ts`, which imports it, is published after it and so is never
    /// published, and no temp file is left behind.
    #[cfg(unix)]
    #[test]
    fn a_dependency_that_cannot_be_published_leaves_the_old_index() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("dot-agent-deck");
        stale_extension(&target);
        std::fs::remove_file(target.join("inbox-receiver.js")).unwrap();
        std::fs::create_dir_all(target.join("inbox-receiver.js").join("taken")).unwrap();
        for (how, result) in ["by path", "beneath"]
            .into_iter()
            .zip(both_materializations(&target))
        {
            assert!(result.is_err(), "{how}: the receiver cannot be published");
            assert_eq!(
                std::fs::read_to_string(target.join("index.ts")).unwrap(),
                "// old index.ts",
                "{how}: index.ts is not published ahead of what it imports"
            );
        }
        let leftovers: Vec<_> = std::fs::read_dir(&target)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "no temp file left: {leftovers:?}");
    }

    /// PR #1735 review 4309809295: every destination is judged before any
    /// file is published, so a refusal at the last one changes none of the
    /// files before it.
    #[cfg(unix)]
    #[test]
    fn a_refused_destination_publishes_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("dot-agent-deck");
        stale_extension(&target);
        let (last, _) = EXTENSION_FILES[EXTENSION_FILES.len() - 1];
        let victim = tmp.path().join("victim.txt");
        std::fs::write(&victim, "keep me").unwrap();
        std::fs::remove_file(target.join(last)).unwrap();
        std::os::unix::fs::symlink(&victim, target.join(last)).unwrap();
        for (how, result) in ["by path", "beneath"]
            .into_iter()
            .zip(both_materializations(&target))
        {
            assert!(result.is_err(), "{how}: the symlink is refused");
            for (name, _) in &EXTENSION_FILES[..EXTENSION_FILES.len() - 1] {
                assert_eq!(
                    std::fs::read_to_string(target.join(name)).unwrap(),
                    format!("// old {name}"),
                    "{how}: {name} is untouched"
                );
            }
        }
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
    }

    /// Hardening M8: a new file is published whole and owner-only, and a
    /// stale copy is replaced by a rename over it, not truncated in place.
    #[cfg(unix)]
    #[test]
    fn materialize_publishes_owner_only_files_by_rename() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let tmp = tempfile::tempdir().unwrap();
        let fresh = tmp.path().join("fresh");
        materialize(&fresh).unwrap();
        for (name, contents) in EXTENSION_FILES {
            let meta = std::fs::metadata(fresh.join(name)).unwrap();
            assert_eq!(meta.permissions().mode() & 0o077, 0, "{name} owner-only");
            assert_eq!(
                std::fs::read_to_string(fresh.join(name)).unwrap(),
                *contents
            );
        }
        let target = tmp.path().join("dot-agent-deck");
        std::fs::create_dir_all(&target).unwrap();
        let stale = target.join("index.ts");
        std::fs::write(&stale, "stale").unwrap();
        let before = std::fs::metadata(&stale).unwrap().ino();
        materialize(&target).unwrap();
        assert_eq!(
            std::fs::read_to_string(&stale).unwrap(),
            embedded("index.ts")
        );
        assert_ne!(
            std::fs::metadata(&stale).unwrap().ino(),
            before,
            "the stale file was replaced by a rename, not rewritten in place"
        );
    }

    /// The materialized layout is the files Pi's subdirectory discovery
    /// needs: `index.ts` (the loaded entry point), its `orchestrator.ts`
    /// sibling and the generated `inbox-receiver.js`. Pins the layout the
    /// docs describe.
    #[test]
    fn materialize_layout_is_index_plus_orchestrator() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("ext");

        materialize(&target).unwrap();

        assert!(
            target.join("index.ts").is_file(),
            "index.ts is the entry point"
        );
        assert!(
            target.join("orchestrator.ts").is_file(),
            "orchestrator.ts is imported by index.ts via ./orchestrator.ts"
        );
        assert!(
            target.join("inbox-receiver.js").is_file(),
            "the generated inbox receiver ships beside them"
        );
        // The subdirectory layout needs no package.json.
        assert!(!target.join("package.json").exists());
    }

    /// The embedded `index.ts` really is Pi's default-export factory that
    /// imports the sibling `./orchestrator.ts` — so the two-file layout is
    /// self-consistent (guards against embedding the wrong sources).
    #[test]
    fn embedded_index_imports_sibling_orchestrator() {
        let index = EXTENSION_FILES
            .iter()
            .find(|(n, _)| *n == "index.ts")
            .map(|(_, c)| *c)
            .expect("index.ts must be embedded");
        assert!(index.contains("export default function"));
        assert!(index.contains("./orchestrator.ts"));
    }

    /// The relative module specifiers (`./x`, `../x`) a source imports, in
    /// the forms the extension can use: `from "…"` (imports and re-exports),
    /// a bare `import "…"`, and `import("…")` / `require("…")`. A specifier
    /// in a comment counts too, which errs towards demanding a file.
    fn relative_imports(source: &str) -> Vec<String> {
        let mut found = Vec::new();
        for lead in ["from ", "import ", "import(", "require("] {
            for (at, _) in source.match_indices(lead) {
                let rest = &source[at + lead.len()..];
                let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
                    continue;
                };
                let Some(end) = rest[1..].find(quote) else {
                    continue;
                };
                let spec = &rest[1..1 + end];
                if spec.starts_with("./") || spec.starts_with("../") {
                    found.push(spec.to_string());
                }
            }
        }
        found
    }

    /// Issue #1965: every file the materialized extension imports, followed
    /// from `index.ts` transitively, is itself in [`EXTENSION_FILES`], flat
    /// beside it and published before the file that imports it. v0.49.2
    /// installed an `index.ts` importing `./usage-file.ts` without that file,
    /// so every Pi agent failed to load the extension and exited at start.
    #[test]
    fn every_relative_import_of_the_extension_is_embedded() {
        let position = |name: &str| EXTENSION_FILES.iter().position(|(n, _)| *n == name);
        let source_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("pi-extension/src");
        let mut problems = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut queue = vec!["index.ts".to_string()];
        while let Some(file) = queue.pop() {
            if !seen.insert(file.clone()) {
                continue;
            }
            // A missing dependency is still followed, from the checkout, so
            // one run names everything the install would lack.
            let source = match position(&file) {
                Some(i) => EXTENSION_FILES[i].1.to_string(),
                None => std::fs::read_to_string(source_dir.join(&file)).unwrap_or_default(),
            };
            for spec in relative_imports(&source) {
                let Some(name) = spec.strip_prefix("./").filter(|n| !n.contains('/')) else {
                    problems.push(format!(
                        "{file} imports {spec:?}, which is outside the flat extension directory"
                    ));
                    continue;
                };
                match (position(name), position(&file)) {
                    (None, _) => problems.push(format!(
                        "{file} imports {spec:?}, but {name} is not in EXTENSION_FILES"
                    )),
                    (Some(dep), Some(importer)) if dep >= importer => problems.push(format!(
                        "{file} imports {spec:?}, but {name} is published after it"
                    )),
                    _ => {}
                }
                queue.push(name.to_string());
            }
        }
        assert!(
            problems.is_empty(),
            "the materialized Pi extension would not load:\n  {}",
            problems.join("\n  ")
        );
    }

    /// The import scan reads every form the extension uses, so the test above
    /// cannot pass for want of seeing an import.
    #[test]
    fn relative_imports_reads_each_import_form() {
        let source = r#"
            import { spawn } from "node:child_process";
            import {
                a,
            } from "./multi.ts";
            import type { T } from './typed.ts';
            export { b } from "./reexport.ts";
            import "./side-effect.js";
            const c = await import("./dynamic.ts");
            const d = require("../up.js");
        "#;
        assert_eq!(
            relative_imports(source),
            [
                "./multi.ts",
                "./typed.ts",
                "./reexport.ts",
                "./side-effect.js",
                "./dynamic.ts",
                "../up.js"
            ]
        );
    }

    fn embedded(name: &str) -> &'static str {
        EXTENSION_FILES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, c)| *c)
            .unwrap_or_else(|| panic!("{name} must be embedded"))
    }

    /// The extension prefixes every inbox delivery with its own copy of the
    /// framing sentence, which must stay the deck's `AGENT_FRAMING` byte for
    /// byte (PRD #1666): the copy is a single-line JSON string literal, so it
    /// is the Rust constant's JSON encoding.
    #[test]
    fn embedded_inbox_framing_matches_agent_framing() {
        let literal = serde_json::to_string(crate::inbox_hub::AGENT_FRAMING).unwrap();
        assert!(
            embedded("orchestrator.ts")
                .contains(&format!("export const INBOX_FRAMING =\n\t{literal};")),
            "pi-extension/src/orchestrator.ts INBOX_FRAMING drifted from inbox_hub::AGENT_FRAMING"
        );
    }

    /// The extension recognises the doorbell line typed into Pi by the deck's
    /// own prefix and suffix, and stops respawning its watcher on the exit
    /// status `inbox wait --stream` gives outside a deck.
    #[test]
    fn embedded_doorbell_and_exit_constants_match_the_deck() {
        let source = embedded("orchestrator.ts");
        let prefix = crate::doorbell::LINE_PREFIX.trim();
        assert!(
            source.contains(&format!("export const DOORBELL_PREFIX = \"{prefix}\";")),
            "DOORBELL_PREFIX drifted from doorbell::LINE_PREFIX"
        );
        assert!(
            source.contains(&format!(
                "export const DOORBELL_SUFFIX = \"{}\";",
                crate::doorbell::LINE_SUFFIX
            )),
            "DOORBELL_SUFFIX drifted from doorbell::LINE_SUFFIX"
        );
        assert!(
            source.contains(&format!(
                "export const WATCHER_NOT_IN_DECK_EXIT = {};",
                crate::inbox_cli::STREAM_NOT_IN_DECK_EXIT
            )),
            "WATCHER_NOT_IN_DECK_EXIT drifted from inbox_cli::STREAM_NOT_IN_DECK_EXIT"
        );
    }

    /// Scenario: The deck generates the Pi extension's copy of the inbox receiver from the receiver OpenCode's plugins inline, and the copy checked in under `pi-extension/src/`, with its type declarations, is compared with that output byte for byte, placeholders filled. The copy the deck embeds for Pi is that same output, and when Node is available the module is imported and exports the receiver.
    #[spec("inbox/pi/006")]
    #[test]
    fn pi_006_the_generated_inbox_receiver_matches_its_generator() {
        use crate::inbox_receiver::{PI_MODULE_DTS, PI_MODULE_DTS_PATH, PI_MODULE_PATH, pi_module};
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let generated = [
            (PI_MODULE_PATH, pi_module()),
            (PI_MODULE_DTS_PATH, PI_MODULE_DTS.to_string()),
        ];
        if std::env::var_os("DOT_AGENT_DECK_WRITE_PI_RECEIVER").is_some() {
            for (path, contents) in &generated {
                std::fs::write(root.join(path), contents).unwrap();
            }
        }
        for (path, contents) in &generated {
            let on_disk = std::fs::read_to_string(root.join(path)).unwrap_or_default();
            assert!(
                on_disk == *contents,
                "{path} differs from its generator in src/inbox_receiver.rs; regenerate it with \
                 DOT_AGENT_DECK_WRITE_PI_RECEIVER=1 cargo test-fast pi_006"
            );
        }
        for placeholder in [
            "__FRAMING_JSON__",
            "__DOORBELL_PREFIX_JSON__",
            "__DOORBELL_SUFFIX_JSON__",
            "__ADAPTER_INTEGRATION__",
            "__RETIRE_KEEPALIVES__",
        ] {
            assert!(
                !pi_module().contains(placeholder),
                "{placeholder} left unfilled"
            );
        }
        assert_eq!(embedded("inbox-receiver.js"), pi_module());

        let Ok(node) = std::process::Command::new("node").arg("--version").output() else {
            eprintln!("SKIP: node is not available to import {PI_MODULE_PATH}");
            return;
        };
        assert!(node.status.success());
        let out = std::process::Command::new("node")
            .arg("--input-type=module")
            .arg("-e")
            .arg(
                "import { pathToFileURL } from 'node:url';\n\
                 const m = await import(pathToFileURL(process.argv[1]).href);\n\
                 for (const name of ['inboxReceiver', 'inboxBinary', 'inboxKey', 'inboxRef']) {\n\
                   if (typeof m[name] !== 'function') throw new Error(name + ' is not exported');\n\
                 }\n\
                 if (m.INBOX_ADAPTER_INTEGRATION !== Number(process.argv[2])) throw new Error('integration');\n",
            )
            .arg(root.join(PI_MODULE_PATH))
            .arg(crate::inbox_hub::ADAPTER_INTEGRATION.to_string())
            .output()
            .expect("run node");
        assert!(
            out.status.success(),
            "importing {PI_MODULE_PATH} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Re-running materialization overwrites in place (idempotent refresh),
    /// never erroring on an existing dir/file.
    #[test]
    fn materialize_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("ext");

        materialize(&target).unwrap();
        // Corrupt a file, then re-materialize: it must be restored.
        std::fs::write(target.join("index.ts"), "STALE").unwrap();
        let second = materialize(&target).unwrap();

        assert_eq!(second.len(), EXTENSION_FILES.len());
        let restored = std::fs::read_to_string(target.join("index.ts")).unwrap();
        assert_ne!(
            restored, "STALE",
            "re-materialize must overwrite a stale file"
        );
        assert!(restored.contains("export default function"));
    }

    // ---- M3.2 (test-plan row 12): setup core, both branches --------------

    /// Row 12 (a): `pi` PRESENT → materializes into the given temp dir and
    /// reports success (exit zero), naming the files it wrote.
    #[test]
    fn setup_present_materializes_and_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(".pi/agent/extensions/dot-agent-deck");

        let report = run_setup(true, &target).unwrap();

        assert!(report.success, "present pi must succeed (exit zero)");
        assert_eq!(report.written.len(), EXTENSION_FILES.len());
        // The files really landed on disk in the discovery layout.
        assert!(target.join("index.ts").is_file());
        assert!(target.join("orchestrator.ts").is_file());
        // The success message names the target dir and the entry file.
        assert!(report.message.contains(&target.display().to_string()));
        assert!(report.message.contains("index.ts"));
        // No install hint on the success path.
        assert!(!report.message.contains(PI_INSTALL_HINT));
    }

    /// Row 12 (b): `pi` ABSENT → emits the exact install-hint command, writes
    /// nothing, and signals failure (exit non-zero).
    #[test]
    fn setup_absent_emits_exact_hint_and_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(".pi/agent/extensions/dot-agent-deck");

        let report = run_setup(false, &target).unwrap();

        assert!(!report.success, "absent pi must fail (exit non-zero)");
        assert!(report.written.is_empty(), "absent pi must not materialize");
        // Nothing was written — the discovery dir does not exist.
        assert!(!target.exists(), "absent pi must not touch the target dir");
        // The exact install command appears verbatim on its own line.
        assert_eq!(
            PI_INSTALL_HINT,
            "npm install -g @earendil-works/pi-coding-agent"
        );
        assert!(
            report.message.lines().any(|l| l == PI_INSTALL_HINT),
            "message must contain the exact install hint on its own line; got:\n{}",
            report.message
        );
    }

    // ---- PRD #201: spawn-time auto-materialize --------------------------

    /// The auto path materializes into a CLEAN target HOME when pi is present:
    /// starting from a directory with NO pre-existing extension, the two
    /// bundled files land in the Pi discovery layout, byte-for-byte equal to
    /// the compiled-in strings. The clean start is essential — otherwise a
    /// materialize would be an indistinguishable no-op.
    #[test]
    fn auto_materialize_core_materializes_into_clean_home_when_pi_present() {
        let tmp = tempfile::tempdir().unwrap();
        let target = extension_dir_under(tmp.path());
        // CLEAN: nothing there yet.
        assert!(!target.exists(), "target must start without the extension");

        let written = auto_materialize_core(true, Some(&target))
            .unwrap()
            .expect("pi present + HOME resolved must materialize");

        assert_eq!(written.len(), EXTENSION_FILES.len());
        assert!(target.join("index.ts").is_file());
        assert!(target.join("orchestrator.ts").is_file());
        for (name, contents) in EXTENSION_FILES {
            let on_disk = std::fs::read_to_string(target.join(name)).unwrap();
            assert_eq!(&on_disk, contents, "{name} must match the embedded source");
        }
    }

    /// pi ABSENT → SKIP: nothing is written and the target dir is never even
    /// created (the guard mirrors `hooks_manage::auto_install`'s
    /// "only if the agent is detected").
    #[test]
    fn auto_materialize_core_skips_when_pi_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let target = extension_dir_under(tmp.path());

        let result = auto_materialize_core(false, Some(&target)).unwrap();

        assert!(result.is_none(), "pi absent must SKIP (return None)");
        assert!(!target.exists(), "pi absent must not create the target dir");
    }

    /// HOME unset (`target_dir == None`) → SKIP with NO write anywhere — in
    /// particular NOT to a predictable `/tmp` path. This is the auto-path
    /// answer to Greptile's `orchestrator_ext.rs:49` `/tmp` fallback concern:
    /// the resolver returns `None` (never a `/tmp` guess) and the core skips.
    #[test]
    fn auto_materialize_core_skips_and_writes_nothing_when_home_unset() {
        // pi is "present", but the target could not be resolved (HOME unset), so
        // the core takes NO `target` and therefore cannot write anywhere — in
        // particular it never falls back to a `/tmp` path. The companion
        // `resolve_agent_dir_inner_override_and_home_precedence` proves the
        // resolver yields `None` (not a `/tmp` guess) when neither is set.
        let result = auto_materialize_core(true, None).unwrap();
        assert!(result.is_none(), "HOME unset must SKIP (return None)");
    }

    /// Re-running the auto path over an already-materialized (or stale) target
    /// overwrites in place — idempotent refresh, never erroring on an existing
    /// dir/file.
    #[test]
    fn auto_materialize_core_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let target = extension_dir_under(tmp.path());

        auto_materialize_core(true, Some(&target)).unwrap();
        // Corrupt a materialized file, then re-run: it must be restored.
        std::fs::write(target.join("index.ts"), "STALE").unwrap();
        let second = auto_materialize_core(true, Some(&target))
            .unwrap()
            .expect("second run must materialize");

        assert_eq!(second.len(), EXTENSION_FILES.len());
        let restored = std::fs::read_to_string(target.join("index.ts")).unwrap();
        assert_ne!(restored, "STALE", "re-run must overwrite a stale file");
        assert!(restored.contains("export default function"));
    }

    /// The pure agent-dir core mirrors pi's `getAgentDir()`: the
    /// `PI_CODING_AGENT_DIR` override wins over HOME (tilde-expanded against it);
    /// with no override it is `<HOME>/.pi/agent`; with neither it is `None`
    /// (never a `/tmp` guess); empty values are ignored on both.
    #[test]
    fn resolve_agent_dir_inner_override_and_home_precedence() {
        // Override wins over HOME (absolute passes through).
        assert_eq!(
            resolve_agent_dir_inner(Some("/custom/agent"), Some("/home/alice")),
            Some(PathBuf::from("/custom/agent"))
        );
        // Override is tilde-expanded against HOME.
        assert_eq!(
            resolve_agent_dir_inner(Some("~/pi-agent"), Some("/home/alice")),
            Some(PathBuf::from("/home/alice/pi-agent"))
        );
        // No override → <HOME>/.pi/agent.
        assert_eq!(
            resolve_agent_dir_inner(None, Some("/home/alice")),
            Some(PathBuf::from("/home/alice/.pi/agent"))
        );
        // Neither set → None (the SKIP path; NO `/tmp` fallback).
        assert_eq!(resolve_agent_dir_inner(None, None), None);
        // Empty override is ignored → falls back to <HOME>/.pi/agent.
        assert_eq!(
            resolve_agent_dir_inner(Some(""), Some("/home/alice")),
            Some(PathBuf::from("/home/alice/.pi/agent"))
        );
        // Empty HOME with no override → None.
        assert_eq!(resolve_agent_dir_inner(None, Some("")), None);
    }

    /// `expand_tilde` expands ONLY a leading `~` / `~/` against HOME (matching
    /// pi's `expandTildePath`); absolute and relative paths, a mid-path `~`, and
    /// (with no HOME) a `~`-prefixed value all pass through unchanged.
    #[test]
    fn expand_tilde_expands_leading_home_only() {
        assert_eq!(expand_tilde("~", Some("/h")), PathBuf::from("/h"));
        assert_eq!(expand_tilde("~/a/b", Some("/h")), PathBuf::from("/h/a/b"));
        assert_eq!(expand_tilde("/abs/x", Some("/h")), PathBuf::from("/abs/x"));
        assert_eq!(expand_tilde("rel/x", Some("/h")), PathBuf::from("rel/x"));
        assert_eq!(expand_tilde("/x/~/y", Some("/h")), PathBuf::from("/x/~/y"));
        // No HOME → a `~` value is returned verbatim (pi's fallback).
        assert_eq!(expand_tilde("~/a", None), PathBuf::from("~/a"));
    }

    /// The env-based resolver honors `PI_CODING_AGENT_DIR` from the overlay (the
    /// value the child gets), winning over HOME, and maps to
    /// `<agent-dir>/extensions/dot-agent-deck`. Hermetic: the override is in the
    /// overlay, so the process env is never consulted.
    #[test]
    fn resolve_agent_dir_from_env_honors_pi_agent_dir_override() {
        let overlay = vec![
            (ENV_PI_AGENT_DIR.to_string(), "/iso/agent".to_string()),
            ("HOME".to_string(), "/should/not/win".to_string()),
        ];
        let dir = resolve_agent_dir_from_env(&overlay).expect("override yields a dir");
        assert_eq!(dir, PathBuf::from("/iso/agent"));
        assert_eq!(
            extension_dir_in(&dir),
            PathBuf::from("/iso/agent/extensions/dot-agent-deck")
        );
    }

    /// `pi_present_for_env` judges pi-presence against the PATH override in the
    /// spawn env overlay when present (so a caller — like the real-pi e2e
    /// tests — that puts pi on a PATH the daemon process itself lacks is
    /// detected correctly).
    #[cfg(unix)]
    #[test]
    fn pi_present_for_env_honors_env_path_override() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let pi = bin_dir.join("pi");
        std::fs::write(&pi, b"#!/bin/sh\n").unwrap();
        set_mode(&pi, 0o755);

        // An overlay PATH pointing only at our temp bin dir detects pi even
        // though the process PATH almost certainly does not contain THIS pi.
        let overlay = vec![("PATH".to_string(), bin_dir.to_string_lossy().into_owned())];
        assert!(pi_present_for_env(&overlay));

        // An overlay PATH pointing at an empty dir does NOT detect pi.
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let overlay_empty = vec![("PATH".to_string(), empty.to_string_lossy().into_owned())];
        assert!(!pi_present_for_env(&overlay_empty));

        // Of repeated entries the child gets the last, and so does the check.
        let twice = [overlay_empty.clone(), overlay.clone()].concat();
        assert!(pi_present_for_env(&twice));
        let twice = [overlay, overlay_empty].concat();
        assert!(!pi_present_for_env(&twice));
    }

    #[test]
    fn default_extension_dir_is_pi_global_layout() {
        // Pin the real target path shape without touching the real HOME's files.
        // With HOME set (the normal test env) the strict resolver yields the Pi
        // global layout; if HOME were unset it would refuse (`None`), which the
        // dedicated refusal test below covers.
        let Some(dir) = default_extension_dir() else {
            return;
        };
        assert!(dir.ends_with("dot-agent-deck"));
        assert!(dir.to_string_lossy().contains(".pi/agent/extensions"));
    }

    /// The EXPLICIT setup path refuses (yields no target dir) when NEITHER
    /// `PI_CODING_AGENT_DIR` nor HOME is set/non-empty — it never guesses a `/tmp`
    /// (unset) or relative `./` (empty) location Pi would never load. Exercised
    /// through the pure [`resolve_agent_dir_inner`] core (which `agent_dir_strict`
    /// feeds the process env) so it needs no process-global env mutation. With no
    /// dir resolved, [`materialize`] is never reached and nothing is written; the
    /// CLI turns the `None` into a non-zero error (see `src/main.rs`).
    #[test]
    fn explicit_setup_refuses_when_agent_dir_and_home_unset_or_empty() {
        // Neither override nor HOME → None (NO `/tmp` fallback).
        assert_eq!(resolve_agent_dir_inner(None, None), None);
        // Empty HOME, no override → None (NO relative `./` base).
        assert_eq!(resolve_agent_dir_inner(None, Some("")), None);
        // HOME set, no override → the extension dir under `<HOME>/.pi/agent`.
        assert_eq!(
            resolve_agent_dir_inner(None, Some("/home/alice")).map(|d| extension_dir_in(&d)),
            Some(extension_dir_under(Path::new("/home/alice")))
        );
    }

    #[test]
    fn path_contains_binary_finds_and_misses() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let faux = bin_dir.join("faux-pi");
        std::fs::write(&faux, b"#!/bin/sh\n").unwrap();
        set_mode(&faux, 0o755);
        // A directory that shares the name must NOT count as the binary.
        std::fs::create_dir_all(bin_dir.join("faux-dir")).unwrap();

        // Build a PATH-shaped value pointing only at our temp bin dir — no
        // process-global env mutation, so this is safe under shared-process runs.
        let path = std::env::join_paths([bin_dir.as_path()]).unwrap();

        assert!(
            path_contains_binary(&path, "faux-pi"),
            "a regular executable file on PATH must be detected"
        );
        assert!(
            !path_contains_binary(&path, "faux-dir"),
            "a directory sharing the name must not count"
        );
        assert!(
            !path_contains_binary(&path, "definitely-not-a-real-binary-xyz"),
            "an absent name must not be detected"
        );
    }

    /// Greptile #2 regression: a present-but-NON-EXECUTABLE `pi` on PATH must
    /// NOT be treated as usable. `path_contains_binary` rejects it (it is a
    /// regular file but has no exec bit), so the setup core takes the
    /// not-present branch and does NOT report success — otherwise setup would
    /// falsely succeed and the Pi pane would later fail to spawn.
    #[cfg(unix)]
    #[test]
    fn setup_non_executable_pi_does_not_succeed() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        // A regular file named `pi` with its exec bits explicitly cleared, so
        // the outcome is deterministic regardless of the process umask.
        let pi = bin_dir.join("pi");
        std::fs::write(&pi, b"#!/bin/sh\necho pi\n").unwrap();
        set_mode(&pi, 0o644);
        assert!(pi.is_file(), "the candidate is a regular file");

        let path = std::env::join_paths([bin_dir.as_path()]).unwrap();
        let pi_present = path_contains_binary(&path, "pi");
        assert!(
            !pi_present,
            "a non-executable `pi` must not count as present"
        );

        // Fed into the setup core, the not-present branch runs: no success, no
        // writes, and the target dir is untouched.
        let target = tmp.path().join(".pi/agent/extensions/dot-agent-deck");
        let report = run_setup(pi_present, &target).unwrap();
        assert!(
            !report.success,
            "a non-executable pi must not report setup success"
        );
        assert!(report.written.is_empty(), "nothing must be materialized");
        assert!(!target.exists(), "the target dir must not be created");
    }

    /// Set the permission bits on `path` (Unix only). Used by the PATH-scan
    /// tests to make a candidate executable or explicitly non-executable
    /// independent of the process umask.
    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(mode);
        std::fs::set_permissions(path, perms).unwrap();
    }

    /// No-op on non-Unix targets, where `is_executable_file` accepts any regular
    /// file and exec bits are not consulted.
    #[cfg(not(unix))]
    fn set_mode(_path: &Path, _mode: u32) {}

    /// What a `node --test` run of one pin proves: `Ok(true)` when its tests
    /// ran and passed, `Ok(false)` when the pin skipped itself on a platform
    /// it does not run on, otherwise `Err`. Off Unix, a run that passed
    /// nothing but skipped the pin is that pin's own platform skip (pi/031's
    /// shell fixtures are Unix-only); on Unix every pin must run, so a skip
    /// fails there like a filter that matched nothing.
    fn node_pin_verdict(success: bool, passed: u32, skipped: u32, unix: bool) -> Result<bool, ()> {
        match (success, passed, skipped) {
            (true, 1.., _) => Ok(true),
            (true, 0, 1..) if !unix => Ok(false),
            _ => Err(()),
        }
    }

    /// Scenario: A pin's Node run is judged from its exit status and its passed and skipped counts. On Unix only a run that passed tests counts, and a skipped or empty run fails; off Unix a run that skipped the pin itself is reported as a skip, while an empty or failed run still fails.
    #[test]
    fn node_pin_verdict_accepts_a_platform_skip_only_off_unix() {
        assert_eq!(node_pin_verdict(true, 3, 0, true), Ok(true));
        assert_eq!(node_pin_verdict(true, 0, 1, true), Err(()));
        assert_eq!(node_pin_verdict(true, 0, 0, true), Err(()));
        assert_eq!(node_pin_verdict(false, 3, 0, true), Err(()));
        assert_eq!(node_pin_verdict(true, 0, 1, false), Ok(false));
        assert_eq!(node_pin_verdict(true, 0, 0, false), Err(()));
        assert_eq!(node_pin_verdict(false, 0, 1, false), Err(()));
        assert_eq!(node_pin_verdict(true, 2, 0, false), Ok(true));
    }

    /// Runs the tests of the extension's Node file `file` whose name starts
    /// with `pin`, and fails unless at least one ran and none failed (see
    /// [`node_pin_verdict`] for a pin that skips itself off Unix). Prints
    /// `SKIP:` and passes without a Node that strips TypeScript types (23.6+).
    fn node_pin(file: &str, pin: &str) {
        let Ok(node) = std::process::Command::new("node").arg("--version").output() else {
            eprintln!("SKIP: node is not available for {file} {pin}");
            return;
        };
        let version = String::from_utf8_lossy(&node.stdout);
        let mut parts = version.trim().trim_start_matches('v').split('.');
        let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        if (major, minor) < (23, 6) {
            eprintln!("SKIP: node {version} cannot strip TypeScript types");
            return;
        }
        let out = std::process::Command::new("node")
            .arg("--test")
            .arg("--test-reporter=tap")
            .arg(format!("--test-name-pattern=^{pin} "))
            .arg(file)
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("pi-extension"))
            .output()
            .expect("run node --test");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let count = |what: &str| {
            stdout
                .lines()
                .find_map(|line| line.strip_prefix(what))
                .and_then(|n| n.trim().parse::<u32>().ok())
                .unwrap_or(0)
        };
        let (passed, skipped) = (count("# pass "), count("# skipped "));
        match node_pin_verdict(out.status.success(), passed, skipped, cfg!(unix)) {
            Ok(true) => {}
            Ok(false) => eprintln!("SKIP: {file} {pin} skips itself on this platform"),
            Err(()) => panic!(
                "{file} {pin}: {passed} passed, {skipped} skipped\n{stdout}{}",
                String::from_utf8_lossy(&out.stderr)
            ),
        }
    }

    /// Scenario: The generated receiver is given a resolver for the deck binary that returns a bare name a stand-in on the agent's PATH answers to, a relative path, nothing, a missing file or a non-executable file, and none of them runs: no watcher starts, nothing is delivered and the receiver tries again on its backoff. An absolute path to an executable, with spaces in it, does run.
    #[spec("inbox/pi/031")]
    #[test]
    fn pi_031_the_receiver_runs_only_an_absolute_executable() {
        node_pin("test/inbox-receiver.test.ts", "pi_031");
    }

    /// Scenario: Pi's first session starts the receiver's watcher on the receipt stream at the deck's adapter integration, and a respawned watcher registers as resumed. A doorbell fetch names that integration and the receiver it is for.
    #[spec("inbox/pi/005")]
    #[test]
    fn pi_005_the_watcher_and_the_doorbell_fetch_declare_the_receipt_integration() {
        node_pin("test/inbox.test.ts", "pi_005");
    }

    /// Scenario: The deck hands one attempt to Pi's receiver, then the same attempt again, then a marked repeat of it; and an attempt submitted but unseen by any gate is carried across a /new swap. Pi gets one follow-up for the repeated hand-out and one for the repeat, each behind the deck's approval; the swapped attempt is submitted again to the successor and passes a gate once.
    #[spec("inbox/pi/007")]
    #[test]
    fn pi_007_a_repeated_hand_out_is_sent_once_and_a_swap_resubmits_what_no_gate_saw() {
        node_pin("test/inbox.test.ts", "pi_007");
    }

    /// Scenario: Hand-overs reach the gate after the deck revoked the attempt, after the watcher restarted, after the approval went stale, and twice. Each is consumed rather than passed to the model; the restarted and stale ones are reported not handed, and a revoked one is not reported a second time.
    #[spec("inbox/pi/008")]
    #[test]
    fn pi_008_the_gate_consumes_refused_hand_overs_and_reports_not_handed_after_revoking() {
        node_pin("test/inbox.test.ts", "pi_008");
    }

    /// Scenario: A gate whose check throws consumes the hand-over instead of passing the tagged text to the model, and a user message carrying our block that reached Pi around the gate commits the attempt.
    #[spec("inbox/pi/009")]
    #[test]
    fn pi_009_the_gate_fails_closed_and_our_opener_without_our_tag_commits() {
        node_pin("test/inbox.test.ts", "pi_009");
    }

    /// Scenario: A hand-over is sent but Pi's gate never sees it. A revoke is answered revoked_ack and the late gate arrival is consumed; with no revoke, once Pi is idle with nothing queued the recheck reports it not handed, and a repeated revoke gets the same answer with no second not handed.
    #[spec("inbox/pi/010")]
    #[test]
    fn pi_010_a_sent_attempt_the_gate_never_saw() {
        node_pin("test/inbox.test.ts", "pi_010");
    }

    /// Scenario: Attempts pass the gate and then a revoke, Pi's refusal pattern, an Escape and a settle in preflight arrive. A revoke is answered already_handed; the refusal and the Escape release the attempt lost, never not handed; a settle while it is still in preflight leaves it held.
    #[spec("inbox/pi/011")]
    #[test]
    fn pi_011_after_the_gate_already_handed_lost_on_positive_settle_evidence_held_in_preflight() {
        node_pin("test/inbox.test.ts", "pi_011");
    }

    /// Scenario: Two attempts of one message pass the gate, and the first is released lost before the user message carrying the message starts. The start commits both, the released one included; a revoke that races the commit's publication is answered already handed.
    #[spec("inbox/pi/012")]
    #[test]
    fn pi_012_message_start_commits_every_called_attempt_of_the_id() {
        node_pin("test/inbox.test.ts", "pi_012");
    }

    /// Scenario: The receiver's possession snapshots number themselves, carry the contiguous watermark, and list a handed attempt until its commit, the gate's pass included; a keepalive brings a fresh snapshot.
    #[spec("inbox/pi/013")]
    #[test]
    fn pi_013_snapshots_hold_an_attempt_until_its_commit() {
        node_pin("test/inbox.test.ts", "pi_013");
    }

    /// Scenario: A session shuts down and a new one starts in the same Pi process. The watcher's stdin stays open, it is not killed, and the new session does not register a second watcher; it is handed the next message.
    #[spec("inbox/pi/014")]
    #[test]
    fn pi_014_a_session_replacement_keeps_the_one_watcher() {
        node_pin("test/inbox.test.ts", "pi_014");
    }

    /// Scenario: The watcher dies with one attempt held and another reported committed while no watcher ran. The respawn answers both resumes retained and the replacement publishes the commit; a commit of a Cancelled attempt is sent again only while the deck lists it correctable, across two respawns.
    #[spec("inbox/pi/015")]
    #[test]
    fn pi_015_respawn_resumes_retained_reports_republished_correctable_re_sent() {
        node_pin("test/inbox.test.ts", "pi_015");
    }

    /// Scenario: A doorbell line is submitted while the watcher is registered, is recalled, arrives just before the watcher registers, and arrives with no registration at all. Registered, or registering within the wait, it is fetched to this receiver and transformed in place; recalled, it is consumed; unregistered past the wait, it is consumed and the mail stays queued.
    #[spec("inbox/pi/016")]
    #[test]
    fn pi_016_doorbell_fetched_to_the_receiver_and_transformed_in_place() {
        node_pin("test/inbox.test.ts", "pi_016");
    }

    /// Scenario: Pi's session shuts down for /new with one attempt past the gate and uncommitted and another sent but unseen. The first is released lost and stays possibly handed; the second goes uncalled to the successor; a commit the outgoing session made while the watcher restarted is still published after the swap.
    #[spec("inbox/pi/024")]
    #[test]
    fn pi_024_a_new_swap_releases_called_uncommitted_attempts_lost() {
        node_pin("test/inbox.test.ts", "pi_024");
    }

    /// Scenario: Each of /new, resume, fork and reload is driven through a stand-in Pi with one attempt called and uncommitted and one held uncalled. The first three release the called one lost and hand the held one to the successor; reload releases nothing and a later start in the successor commits it; no restored opener commits; one watcher throughout.
    #[spec("inbox/pi/030")]
    #[test]
    fn pi_030_each_session_replacement_in_pi_0_84_4_s_order() {
        node_pin("test/inbox.test.ts", "pi_030");
    }

    /// Scenario: Pi's watcher is registered and no session is current. A message is taken into the receiver and held, not handed to Pi; the next session is handed it through a fresh approval; with no successor it stays held, and a Cancel is answered revoked_ack.
    #[spec("inbox/pi/025")]
    #[test]
    fn pi_025_no_current_session_held_in_pending_until_a_successor() {
        node_pin("test/inbox.test.ts", "pi_025");
    }

    /// Scenario: A hand-over approved for one session resumes after a successor took over, a session's delayed shutdown arrives after its successor started (before and after the successor's turn began, for a /new and for a reload), and the outgoing session reports Thinking after it was replaced. The approval does not reach the successor's Pi; the delayed shutdown leaves the successor current, reports no Idle into its turn and still releases the outgoing session's attempts (lost for a /new, the successor's for a reload); the stale Thinking is not reported while an in-order outgoing Idle is.
    #[spec("inbox/pi/026")]
    #[test]
    fn pi_026_session_tokens_a_delayed_approval_shutdown_and_report() {
        node_pin("test/inbox.test.ts", "pi_026");
    }

    /// Scenario: Inputs carrying a hand-over tag this process never issued, a tag from an older version's format and a well-formed tag for an unknown attempt reach the gate. Each is consumed, and nothing is reported to the deck.
    #[spec("inbox/pi/027")]
    #[test]
    fn pi_027_unknown_or_older_tags_are_consumed_silently() {
        node_pin("test/inbox.test.ts", "pi_027");
    }

    /// Scenario: A reload evaluates a different version of the extension while the old receiver holds attempts, one waiting for approval with another queued behind it. The old receiver is disposed before its drained watcher is awaited, keeps reading that watcher (a message, a revoke, and a commit by the reload's successor) without handing anything to Pi, and two initializers share one replacement. A compatible state is carried and answered retained, its waiting and drained messages go to the successor in order, the drained commit is published, and the reload's ownership of a passed attempt lets the successor release it lost; an incompatible state, or a never-released version 1, answers unknown and never not handed, and across it the outgoing session's delayed shutdown reports no Idle into the successor's turn.
    #[spec("inbox/pi/028")]
    #[test]
    fn pi_028_a_version_replacement_dispose_drain_transfer() {
        node_pin("test/inbox.test.ts", "pi_028");
    }

    /// Scenario: The watcher dies while no session is current and is respawned; a later session_start finds it healthy and starts no second one; a watcher that says it runs outside a deck pane is not respawned for the rest of the process.
    #[spec("inbox/pi/029")]
    #[test]
    fn pi_029_the_watcher_s_lifetime_is_the_process_s_not_a_session_s() {
        node_pin("test/inbox.test.ts", "pi_029");
    }

    /// Scenario: Hand-overs reach the gate late — after a /new swap, after a revoke and after the watcher restarted — and doorbell fetches return late across the same three; and a busy hand-over passes the gate while an earlier report holds the report queue, across a swap, a revoke, a rebind or an expiry. The late ones are consumed, never transformed, reported not handed when still held after a swap or rebind; the passed one returns its transform without waiting for the queue, and what follows treats it as handed.
    #[spec("inbox/pi/032")]
    #[test]
    fn pi_032_the_session_and_authorization_fence_at_both_hand_over_points() {
        node_pin("test/inbox.test.ts", "pi_032");
    }

    /// Scenario: Five deck messages reach Pi together on a stand-in with Pi 0.84.4's admission — one prompt at a time, a refused prompt settling the session's running flag — first while Pi is idle at boot, then behind a user's turn. Each is handed over only once the one before it started or was seen queued, so Pi refuses none, and all five commit in inbox order, each once, with nothing left held or reported not handed. A marked repeat of a message Pi never started is handed over at once, while another message waits until Pi starts on the first.
    #[spec("inbox/pi/036")]
    #[test]
    fn pi_036_a_burst_is_handed_to_pi_one_admission_at_a_time() {
        node_pin("test/inbox.test.ts", "pi_036");
    }

    /// The extension's own Node suites — its inbox delivery and
    /// acknowledgement (`inbox.test.ts`) and its orchestrator helpers
    /// (`orchestrator.test.ts`) — run here, so a change to the extension this
    /// module embeds is checked by the fast tier and CI's `build` job.
    /// `questions.test.ts` runs from `question_detect_008` in `src/hook.rs`.
    /// They need a Node that strips TypeScript types (23.6+) and nothing from
    /// `node_modules`; without one this prints `SKIP:` and passes.
    #[test]
    fn the_pi_extensions_node_suites_pass() {
        const SUITES: [&str; 3] = [
            "test/inbox.test.ts",
            "test/orchestrator.test.ts",
            "test/telemetry.test.ts",
        ];
        let Ok(node) = std::process::Command::new("node").arg("--version").output() else {
            eprintln!("SKIP: node is not available for the pi-extension suites {SUITES:?}");
            return;
        };
        let version = String::from_utf8_lossy(&node.stdout);
        let mut parts = version.trim().trim_start_matches('v').split('.');
        let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        if (major, minor) < (23, 6) {
            eprintln!("SKIP: node {version} cannot strip TypeScript types");
            return;
        }
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("pi-extension");
        let out = std::process::Command::new("node")
            .arg("--test")
            .args(SUITES)
            .current_dir(&dir)
            .output()
            .expect("run node --test");
        assert!(
            out.status.success(),
            "pi-extension suites {SUITES:?} failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
