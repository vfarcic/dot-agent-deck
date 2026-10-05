//! Helpers shared by the per-agent hook-config adapters — how the deck's hook
//! command is spelled, and how an agent's config file is published.
//!
//! [`crate::codex_hooks_manage`] and [`crate::devin_hooks_manage`] each install
//! the deck's hooks by rewriting a config file a *third-party* tool owns:
//! Codex's `~/.codex/hooks.json` and `~/.codex/config.toml`, Devin's
//! `~/.config/devin/config.json`. The Devin adapter was copied verbatim from the
//! Codex one, so both carried byte-identical copies of these two helpers — which
//! is how the permissions defect below was fixed in one (#360) and left standing
//! in the other (#382). They live here once so the next adapter inherits the fix
//! instead of the bug.
//!
//! **[`crate::hooks_manage`] deliberately keeps its own `write_atomic`.** The
//! Claude adapter's is not a third copy of this one: it takes `dest` alone
//! (deriving the directory). It publishes through `create_new` too (#534), but
//! at the fixed `.<name>.tmp.<pid>` path, unlinking a squatter and retrying
//! once; this one draws an unpredictable name instead (#731). Folding them
//! together would push a rewrite onto an adapter that has not been reviewed for
//! it, so it stays where it is.
//!
//! It does share [`backup_malformed`], though, and all three adapters do — that
//! one is new rather than a rewrite of anything, and the alternative was a
//! fourth hand-rolled copy of the write whose triplicated version is what #731
//! is about.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Which shell will actually EXECUTE the hook command line the deck writes, and
/// therefore which dialect [`build_command`] quotes for. It is a property of
/// the **consuming agent**, not of the machine the deck was compiled on (issue
/// #734).
///
/// Spelled per writer rather than defaulted, because the two writers genuinely
/// differ and a single host-derived answer is wrong for one of them. Deriving
/// it from `cfg!(windows)` for both is the same category error #734 fixed —
/// reading the dialect off the compile target instead of off the interpreter —
/// just one level further down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookShell {
    /// The host's native shell: `cmd.exe` on Windows, a POSIX shell elsewhere.
    ///
    /// **Codex.** Its hooks engine
    /// (`codex-rs/hooks/src/engine/command_runner.rs`, read at 0.149.0) hands
    /// the whole command string to `%COMSPEC%` else `cmd.exe` with `/C` on
    /// Windows, and to `$SHELL` else `/bin/sh` with `-lc` otherwise. **The deck
    /// writes no per-entry `shell` override**, so that default is what runs
    /// every deck hook and the interpreter really does follow the host — and
    /// there would be nothing to gain by writing one: measured on 0.149.0, a
    /// handler's `shell`, `cwd`, `env` and `timeoutSec` are silently dropped and
    /// do not even reach `currentHash`. (The load-bearing claim is the first
    /// one, about this project's own writer, which [`build_command`] below makes
    /// verifiable here. The measurement is four field names from one version of
    /// somebody else's schema, and is kept as corroboration only — read as a
    /// claim about Codex's whole schema it would be an absolute standing on far
    /// less than it needs, CLAUDE.md rule 17.) `codex_home` honours
    /// `$CODEX_HOME` on every platform, which is what makes the Windows arm
    /// reachable rather than theoretical.
    Native,
    /// A POSIX shell, whatever the host is.
    ///
    /// **Devin.** `devin_hooks_manage::devin_config_dir` returns `None` off
    /// Unix, so the only machine that can ever read the config this writer
    /// produces is a Unix one, and its interpreter is POSIX by construction.
    ///
    /// The gate that makes that true lives one level up, and
    /// `devin_hooks_manage::install_to` is reachable *without* passing through
    /// it — so a host-derived dialect did not stay theoretical either: it gave
    /// a Windows CI runner double-quoted Devin output and went red, which is
    /// the correct outcome, since that output contradicts the very claim
    /// ("byte-identical on every platform Devin can run on") that justified
    /// leaving the Devin path alone in the first place. Asking for POSIX here
    /// makes the claim true at the call site instead of borrowing it from a
    /// caller.
    Posix,
}

/// Build the deck's hook command for `binary_path`, robustly quoting the
/// executable path so a path containing whitespace or shell metacharacters still
/// produces a valid command that the agent parses to the intended argv. A "safe"
/// path (only path-typical characters) is emitted verbatim so the common case
/// stays human-readable and stable; anything else is quoted in `shell`'s
/// dialect — single quotes for a POSIX shell, double quotes for `cmd.exe`.
///
/// `suffix` is the caller's `HOOK_COMMAND_SUFFIX` — the fixed
/// `hook --agent <agent>` signature that also identifies the resulting command
/// as deck-owned on the way back in, so the two must stay the same string.
///
/// **The quoting follows the interpreter, not the compile target** (issue
/// #734); [`HookShell`] records which writer names which interpreter, and why.
/// Before #734 it was POSIX on every platform, so a Windows Codex user
/// (reachable only via `$CODEX_HOME` — see `codex_hooks_manage::codex_home`)
/// got `'C:\…\dot-agent-deck.exe' hook --agent codex` written into
/// `hooks.json`, which `cmd.exe` cannot run: it reads `'` as an ordinary
/// character and looks for a file whose name literally starts with one.
pub(crate) fn build_command(binary_path: &str, suffix: &str, shell: HookShell) -> String {
    build_command_for(binary_path, suffix, shell, cfg!(windows))
}

/// [`build_command`] with the host as a parameter.
///
/// The split exists for testability and nothing else: production passes
/// `cfg!(windows)`, a compile-time constant, so the branch costs nothing at
/// runtime — but a `#[cfg]` here would leave the Windows spelling of these
/// command lines asserted by nothing on any machine this project is developed
/// or CI-tested on except `build-windows`, which type-checks the arm without
/// ever running it. That is exactly how #734 shipped.
///
/// It is also what lets [`HookShell::Posix`]'s host-independence be *asserted*
/// from Linux rather than trusted, which matters because that property was
/// wrong once already and only a Windows runner noticed.
fn build_command_for(
    binary_path: &str,
    suffix: &str,
    shell: HookShell,
    windows_host: bool,
) -> String {
    let windows_dialect = match shell {
        HookShell::Native => windows_host,
        HookShell::Posix => false,
    };
    format!(
        "{} {suffix}",
        crate::platform::paths::native_shell_command_word(binary_path, windows_dialect)
    )
}

/// Atomically publish `bytes` to `dest` by writing a temp file in `dir` — which
/// must be `dest`'s OWN directory, so `rename(2)` stays on one filesystem and is
/// atomic — and renaming over `dest`. A crash mid-write leaves either the old
/// file or the temp file intact, never a truncated `dest`.
///
/// The temp name carries `dest`'s file name (see [`temp_path`]), so one
/// adapter's `hooks.json` and `config.toml` publishes never race on a single
/// temp path. (The `"config"` fallback is only reachable for a `dest` with no
/// UTF-8 file name, where the `rename` below cannot succeed either; the two
/// adapters spelled that unreachable literal differently before this was
/// extracted.)
///
/// # The temp file is never an entry that already exists (#731)
///
/// This used to build `.<name>.tmp.<pid>` and open it with `File::create`. Both
/// halves were wrong together: the name is fully derivable from the destination
/// and a pid anyone on the box can read, and `File::create` **follows a
/// symlink**. A writer able to add an entry to the agent's config directory —
/// `~/.codex`, `~/.config/devin` — could pre-plant that name pointing anywhere
/// it could write, and the publish would truncate that target, chmod it and
/// fill it with the deck's bytes. The `rename` then moved the *symlink* onto
/// `dest` (rename does not follow one either), so the destination did not even
/// end up holding the evidence.
///
/// The fix is [`create_temp_excl`]'s `create_new` — `O_CREAT|O_EXCL`, which
/// POSIX requires to fail with `EEXIST` when the path names a symlink, dangling
/// or not — over an unpredictable name, retried on collision. Two independent
/// properties, deliberately: `O_EXCL` is what makes following impossible, and
/// it holds even if the name were guessed outright. The unpredictable name is
/// the second layer, and it demotes the remaining attack from a redirected
/// write to a squat that costs one retry.
///
/// # Permissions
///
/// The temp file is published with the destination's OWN mode, or owner-only
/// when the file is new. `File::create` would otherwise apply `0666 & !umask` —
/// 0644 under a typical 022 umask, **0664 (group-writable) under 002** — and the
/// rename would then silently widen a config the user had kept private. That is
/// not theoretical: a real `devin` install ships its config at 0600 and it holds
/// `devin.org_id`, and Codex's `config.toml` holds the user's model choice,
/// hook-trust records and any hand-written settings (#360, #382).
///
/// Creation itself is owner-only on Unix rather than umask-derived, so the file
/// is never briefly group- or world-readable between `open` and the `chmod`
/// below. That is only a tightening of the pre-content window — the mode the
/// publish lands is still the destination's own, applied by `fchmod`, which no
/// umask filters.
pub(crate) fn write_atomic(dir: &Path, dest: &Path, bytes: &[u8]) -> io::Result<()> {
    // PRD #1487: before the temp file exists, so a refused test write leaves
    // nothing behind. The temp sits in `dest`'s own directory, so judging
    // `dest` judges it too.
    crate::config_write_guard::ensure_config_write_allowed(dest)?;
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");

    let (mut file, tmp) = create_temp(dir, name)?;
    // Removes the temp file on every early return AND on unwind; disarmed only
    // once the rename has consumed it.
    let mut cleanup = TempFileGuard::new(tmp.clone());

    // Everything after the create is fallible with a temp file already on disk,
    // so it runs in one closure and shares a single cleanup path. The previous
    // shape leaked the temp file whenever `write_all` or `sync_all` failed — it
    // only removed it when the `rename` did.
    let written = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            // Set explicitly rather than leaning on `create_temp_excl`'s
            // `mode(0o600)`, which an unusual umask can narrow further; `fchmod`
            // is filtered by no umask.
            let landed = std::fs::metadata(dest)
                .map(|meta| meta.permissions().mode() & 0o777)
                .unwrap_or(0o600);
            file.set_permissions(std::fs::Permissions::from_mode(landed))?;
        }
        file.write_all(bytes)?;
        file.sync_all()
    })();
    drop(file);

    written.and_then(|()| std::fs::rename(&tmp, dest))?;
    cleanup.disarm();
    Ok(())
}

/// How long a writer waits for another process's read-modify-write of the same
/// agent config to finish. One is a few milliseconds, most of it the `fsync`;
/// a holder still busy after this long is stuck, and the caller's own error
/// path (a logged warning on the startup install, a printed one from the CLI)
/// beats blocking a pane's spawn forever.
const CONFIG_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// How often a waiting writer retries the lock.
const CONFIG_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// An exclusive, cross-process lock on one agent config file, held until
/// dropped. See [`lock_config`].
pub(crate) struct ConfigLock {
    _file: Option<std::fs::File>,
}

/// Serialise a read-modify-write of `dest` — an agent config the deck merges
/// its entries into — across PROCESSES (issue #1493's follow-up).
///
/// Each adapter already holds an in-process mutex across its read, merge and
/// publish, which keeps two threads of one deck apart. Nothing kept two deck
/// processes apart: several Codex panes starting at once each spawn a wrapper
/// that records hook trust, so each read the same `config.toml`, added its own
/// `[hooks.state."<key>"]` record and published — and the later rename dropped
/// the earlier record, leaving that agent's hooks untrusted. Measured: eight
/// processes recording twenty records each kept 33 of 160 (`codex/trust/008`).
///
/// # A sidecar, never deleted
///
/// The config itself cannot carry the lock: every publish REPLACES it by
/// rename, so a lock on the old inode would not exclude a process that opened
/// the new one. The lock is on `.<name>.lock` beside it, which is never
/// replaced or removed — deleting a lock file others may be waiting on is how
/// two processes end up holding locks on different inodes. It is empty and
/// owner-only, and deleting it by hand is harmless while no write is running.
/// The same reasoning as the desktop's settings save lock
/// (`desktop/src-tauri/src/settings.rs`, issue #828), whose shape this follows.
///
/// # When it does not lock
///
/// - The config's directory does not exist: there is no file to lose an
///   update from, and every installer creates the directory before it locks.
///   (An uninstall of something never installed reaches this.)
/// - The filesystem cannot lock at all (the call reports `Unsupported`):
///   refusing would make the deck's hooks uninstallable there, so the write
///   goes ahead as it always did and says so in the log.
///
/// Every other failure is an error: the sidecar's name is taken by something
/// that is not a regular file (a lock would land on whatever it points at), it
/// cannot be opened, or another process still holds it after
/// [`CONFIG_LOCK_WAIT`]. Going ahead unlocked is exactly the race this exists
/// to stop.
pub(crate) fn lock_config(dest: &Path) -> io::Result<ConfigLock> {
    let dir = match dest.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    if !dir.is_dir() {
        return Ok(ConfigLock { _file: None });
    }
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let lock_path = dir.join(format!(".{name}.lock"));
    match std::fs::symlink_metadata(&lock_path) {
        Ok(meta) if !meta.file_type().is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} is not a regular file, so the deck cannot lock {} to update it; \
                     remove it and try again",
                    lock_path.display(),
                    dest.display()
                ),
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut opts = std::fs::OpenOptions::new();
    // Nothing is ever written to it; `write` is what `create` requires, and
    // `LockFileEx` needs a handle opened for reading or writing.
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let file = opts.open(&lock_path)?;
    let deadline = std::time::Instant::now() + CONFIG_LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => {
                reap_stale_temps(dir, name);
                return Ok(ConfigLock { _file: Some(file) });
            }
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(CONFIG_LOCK_POLL);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "another process has been updating {} for over {}s; not updating it",
                        dest.display(),
                        CONFIG_LOCK_WAIT.as_secs()
                    ),
                ));
            }
            Err(std::fs::TryLockError::Error(e)) if e.kind() == io::ErrorKind::Unsupported => {
                tracing::warn!(
                    "updating {} without a cross-process lock, which this filesystem does \
                     not support: {e}",
                    dest.display()
                );
                return Ok(ConfigLock { _file: None });
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// How old a leftover temp file must be before [`reap_stale_temps`] removes it.
/// A publish holds its temp file for milliseconds; an hour is far past any
/// write, including one by a process in another PID namespace sharing the
/// directory, whose pid this one cannot see.
#[cfg(unix)]
const STALE_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Remove leftover temp files of earlier publishes of `name` in `dir` that are
/// provably the deck's and provably abandoned. Called with the config's lock
/// held, so no other deck is publishing `name` meanwhile.
///
/// [`publish`] already removes its temp file whenever a write or the rename
/// fails. What it cannot clean up is a process killed between creating the file
/// and renaming it — a `SIGKILL`, a crash, a machine losing power — which is
/// how `~/.codex` came to hold 21 `.config.toml.tmp.*` files. They are never
/// read (only `<name>` itself is), and never in the way of a later publish
/// (each draws a fresh name), so this is tidiness, not correctness — which is
/// why it is best-effort and silent about anything it cannot settle.
///
/// A file is removed only when ALL of these hold, and kept otherwise:
/// - its name is exactly [`temp_path`]'s shape for `name`:
///   `.<name>.tmp.<pid>.<16 lowercase hex digits>`;
/// - it is a regular file (a symlink or anything else is never touched);
/// - the process that named it is not running ([`process_is_gone`]): a reused
///   pid reads as running, which keeps the file — the safe direction;
/// - it was last modified more than [`STALE_TEMP_AGE`] ago.
///
/// Unix only: where the deck cannot ask whether a pid is running, it keeps
/// everything.
fn reap_stale_temps(dir: &Path, name: &str) {
    #[cfg(unix)]
    {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let prefix = format!(".{name}.tmp.");
        let now = std::time::SystemTime::now();
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            let Some(pid) = stale_temp_pid(file_name, &prefix) else {
                continue;
            };
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            let old_enough = meta
                .modified()
                .ok()
                .and_then(|at| now.duration_since(at).ok())
                .is_some_and(|age| age > STALE_TEMP_AGE);
            if meta.file_type().is_file() && old_enough && process_is_gone(pid) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (dir, name);
}

/// The pid in `file_name` when it is exactly `<prefix><pid>.<16 lowercase hex>`
/// — the shape [`temp_path`] draws — and `None` for anything else.
#[cfg_attr(not(unix), allow(dead_code))]
fn stale_temp_pid(file_name: &str, prefix: &str) -> Option<u32> {
    let rest = file_name.strip_prefix(prefix)?;
    let (pid, tail) = rest.split_once('.')?;
    let hex = tail.len() == 16 && tail.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let digits = !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit());
    if hex && digits {
        pid.parse().ok()
    } else {
        None
    }
}

/// Whether no process with `pid` exists, as far as this process can tell:
/// `kill(pid, 0)` failing with `ESRCH`. Anything else — success, or `EPERM`
/// for another user's process — reads as running.
#[cfg(unix)]
fn process_is_gone(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs only the existence and permission checks.
    let rc = unsafe { libc::kill(pid, 0) };
    rc != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// What [`backup_malformed`] did with the bytes it was handed, so
/// [`preserved_phrase`] can say exactly that and nothing more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Backup {
    /// The bytes are at this path: copied there now, or already there byte for
    /// byte — the same config refused again, as `hooks_manage::auto_install`
    /// does on every launch while it stays malformed.
    Preserved(PathBuf),
    /// Something else already holds the backup name and was left as it was.
    Occupied(PathBuf),
    /// No copy was made.
    Failed,
}

/// Removes a publish's temp file when dropped, unless [`TempFileGuard::disarm`]
/// ran first — so the temp goes on every returned error and on a panic between
/// its creation and the rename (PRD #1487), not only on the error arms someone
/// remembered to clean up after. A process killed outright still runs no
/// destructor; nothing here sweeps such leftovers.
pub(crate) struct TempFileGuard {
    path: Option<PathBuf>,
}

impl TempFileGuard {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// The temp file was renamed into place; there is nothing left to remove.
    pub(crate) fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take()
            && let Err(e) = std::fs::remove_file(&path)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(
                "could not remove the abandoned temp file {}: {e}",
                path.display()
            );
        }
    }
}

/// Preserve `bytes` — the content of a config file that would not parse — beside
/// the original as `<file name>.bak`, **unless that name is already taken**, and
/// report what happened.
///
/// Best-effort by contract: every caller is on its way to returning an
/// `InvalidData` error with the user's file left untouched, so a failed copy must
/// not replace that error with its own. That is also why the copy is close to
/// redundant — the original bytes are still on disk at the original path — and
/// why nothing here may cost the user a file to make it.
///
/// # An existing `<name>.bak` is never replaced (#537)
///
/// A config is malformed because somebody is hand-editing it, and copying it to
/// `<name>.bak` first is what a careful hand-editor does. This used to publish
/// over whatever held the name, so the first refusal destroyed the one copy of
/// the user's config that still parsed, to protect an original that was never
/// at risk. The name is now taken only when it is free:
///
/// - free → the bytes land there, [`Backup::Preserved`];
/// - holding exactly these bytes, as a regular file → the earlier refusal's own
///   copy, [`Backup::Preserved`] without a write;
/// - holding anything else — a different file, a directory, a symlink →
///   [`Backup::Occupied`], left exactly as found.
///
/// Repeat refusals therefore never accumulate files (the reason #855 chose to
/// replace rather than draw a fresh name each time): a config that stays
/// malformed reuses its backup, and one that changes keeps the first copy and
/// says so.
///
/// # The copy is never written THROUGH a symlink (#731)
///
/// All three adapters once spelled this as `std::fs::write` at this same, fully
/// predictable path, which opens with `O_TRUNC` and **follows a symlink**: a
/// writer able to add an entry to the agent's config directory — `~/.claude`,
/// `~/.codex`, `~/.config/devin` — could plant `<name>.bak` pointing at any file
/// it could write and have the deck fill it with the malformed config's bytes.
/// Nothing at the backup name is ever opened for writing now: the bytes go to
/// an unpredictable temp created with `O_CREAT|O_EXCL` ([`create_temp`]), and
/// the name is taken by `link(2)`, which fails with `EEXIST` when the path
/// names a symlink, dangling or not, without following it — so a planted link
/// is reported as [`Backup::Occupied`]. A symlink is never counted as an
/// earlier copy either — the comparison reads only a regular file.
///
/// # Permissions
///
/// The backup lands **owner-only, always** — not at the destination's mode. It
/// is a byte-for-byte copy of a config that may hold an org id or an auth
/// reference, so 0600 is the right answer on its merits (#360, #382). The mode
/// is set on the temp, which the link shares an inode with, so a planted link
/// at the name cannot choose it either (Greptile's P1 on PR #855).
///
/// The copy is published atomically, by a hard link from a fully written temp,
/// so the name never holds a partial backup, and this function never unlinks
/// it. A filesystem without hard links gets no backup ([`Backup::Failed`]),
/// which costs nothing the refusal promised: the original is still on disk.
///
/// # The name
///
/// `.bak` is APPENDED to the whole file name. Two of the three adapters spelled
/// this `path.with_extension("json.bak")`, which *replaces* the extension
/// instead — the same answer for every path they actually pass, since
/// `settings.json`, `hooks.json` and `config.json` each reach
/// `<that name>.bak` either way, and a different one only for a destination not
/// named `*.json`, where appending is what keeps the original name legible.
pub(crate) fn backup_malformed(dest: &Path, bytes: &[u8]) -> Backup {
    let Some(file_name) = dest.file_name() else {
        return Backup::Failed;
    };
    let mut name = file_name.to_os_string();
    name.push(".bak");
    let backup = dest.with_file_name(name);
    // `dest`'s OWN directory, so the temp and the backup share a filesystem —
    // a hard link cannot cross one.
    let dir = match dest.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    // PRD #1487: the same containment `write_atomic` applies, before the temp
    // exists — a refused test write makes no copy and leaves nothing behind.
    if crate::config_write_guard::ensure_config_write_allowed(&backup).is_err() {
        return Backup::Failed;
    }

    // The complete copy goes to an unpredictable temp first and is published
    // by `hard_link`, which is atomic and never replaces: it fails with
    // `EEXIST` (`ERROR_ALREADY_EXISTS` on Windows) when the name is taken, and
    // does not follow a symlink there. So `<name>.bak` is either absent or
    // whole — a crash leaves at most a stray temp, never a short backup that
    // later refusals would have to report as an occupant, and a second deck
    // process refusing the same file at the same moment sees the first one's
    // finished copy rather than a half-written one.
    let Ok((mut file, tmp)) = create_temp(
        dir,
        &backup.file_name().unwrap_or_default().to_string_lossy(),
    ) else {
        return Backup::Failed;
    };
    let written = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            // Explicit for the same reason as in `write_atomic`: an unusual umask
            // can narrow `create_temp_excl`'s 0600, and `fchmod` is filtered by
            // none.
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(bytes)?;
        file.sync_all()
    })();
    drop(file);

    let outcome = match written.and_then(|()| std::fs::hard_link(&tmp, &backup)) {
        Ok(()) => Backup::Preserved(backup),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            if holds_exactly(&backup, bytes) {
                Backup::Preserved(backup)
            } else {
                Backup::Occupied(backup)
            }
        }
        // Including a filesystem with no hard links (FAT, some network
        // mounts): no copy, and the caller's message says so. The original is
        // still untouched, which is what the refusal promises.
        Err(_) => Backup::Failed,
    };
    // The temp is this call's own unpredictable name; the backup, published or
    // not, is never unlinked here.
    let _ = std::fs::remove_file(&tmp);
    outcome
}

/// Whether `path` is a regular file holding exactly `bytes` — read without
/// following a symlink at `path` itself, so a planted link is never taken for
/// an earlier backup.
fn holds_exactly(path: &Path, bytes: &[u8]) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.file_type().is_file() && meta.len() == bytes.len() as u64)
        && std::fs::read(path).is_ok_and(|existing| existing == bytes)
}

/// Spell what [`backup_malformed`] did, for the caller's error message.
///
/// One phrasing shared by all three adapters, so the sentence a user reads names
/// a file that holds what it says, and never claims a file the deck did not
/// write as its backup.
pub(crate) fn preserved_phrase(backup: &Backup) -> String {
    match backup {
        Backup::Preserved(path) => format!("preserved at {}", path.display()),
        Backup::Occupied(path) => format!(
            "not copied: {} already exists and was left as it was",
            path.display()
        ),
        Backup::Failed => "not preserved: the copy aside failed".to_string(),
    }
}

/// How many temp names [`create_temp`] draws before giving up. Each attempt
/// draws a fresh unpredictable name, so a natural collision is already
/// vanishingly unlikely at the first; the budget exists for a directory being
/// actively squatted, where retrying is what keeps an attacker from turning a
/// name clash into a refusal to install the deck's hooks at all. Bounded rather
/// than unbounded so a genuinely undrainable directory reports an error instead
/// of spinning.
const TEMP_NAME_ATTEMPTS: usize = 16;

/// Exclusively create a fresh temp file in `dir` for a publish of `name`,
/// redrawing the name on collision. Returns the open file and its path.
fn create_temp(dir: &Path, name: &str) -> io::Result<(std::fs::File, PathBuf)> {
    create_temp_at(std::iter::repeat_with(|| temp_path(dir, name)).take(TEMP_NAME_ATTEMPTS))
}

/// Take the first of `candidates` that does not already exist, exclusively.
///
/// Split from [`create_temp`] so the collision path can be driven by a fixed
/// list of paths in a test — a randomly drawn name cannot be made to collide on
/// purpose, and "retries instead of failing" is the half of #731 that keeps a
/// squatter from turning an unfollowable name into a refusal to install hooks.
///
/// A collision is never resolved by unlinking whatever holds the name: that
/// would let a squatter steer which entry the deck deletes, and there is no need
/// — the next candidate is a different name.
fn create_temp_at(
    candidates: impl Iterator<Item = PathBuf>,
) -> io::Result<(std::fs::File, PathBuf)> {
    let mut last = None;
    for tmp in candidates {
        match create_temp_excl(&tmp) {
            Ok(file) => return Ok((file, tmp)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a temp file for the publish",
        )
    }))
}

/// Open `tmp` with `O_CREAT|O_EXCL` (owner-only on Unix), failing rather than
/// opening anything that is already there.
///
/// This is the whole security property of #731 in one call: `create_new` maps to
/// `O_EXCL` on Unix and `CREATE_NEW` on Windows, and POSIX requires `O_EXCL` to
/// fail with `EEXIST` when the path names a symbolic link — so a pre-planted
/// symlink can never be followed, whether or not its target exists.
fn create_temp_excl(tmp: &Path) -> io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    opts.open(tmp)
}

/// Draw an unpredictable same-directory temp path for a publish of `name`:
/// `.<name>.tmp.<pid>.<random>`.
///
/// `name` and the pid are kept for what they were always worth — they keep two
/// concurrent publishes in one directory apart by construction and make a
/// leftover attributable to a process — and the random tail is what an outside
/// writer cannot precompute.
///
/// The tail is a keyed hash under [`RandomState`](std::hash::RandomState)'s
/// keys — SipHash-1-3 as the standard library implements it today, though it
/// promises no particular algorithm — and those keys are seeded once per thread
/// from the OS random source. What an outside writer can see is 64 hashed bits
/// of output, never the keys, so it cannot precompute the next name; a
/// process-wide counter is mixed in as well, so no two draws in one run share an
/// input, and the retry loop covers the vanishing chance that two of them
/// nevertheless hash alike.
///
/// This deliberately does not pull in a random-number crate. `O_EXCL` above —
/// not the quality of this tail — is what makes a squatted name unfollowable, so
/// the tail carries only the weaker second-layer job of being unguessable to a
/// writer that cannot observe the keys, which a keyed hash already is.
fn temp_path(dir: &Path, name: &str) -> PathBuf {
    use std::hash::{BuildHasher as _, Hasher as _, RandomState};
    use std::sync::atomic::{AtomicU64, Ordering};

    static DRAWS: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(DRAWS.fetch_add(1, Ordering::Relaxed));
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0),
    );
    dir.join(format!(
        ".{name}.tmp.{}.{:016x}",
        std::process::id(),
        hasher.finish()
    ))
}

// ---------------------------------------------------------------------------
// Which hook commands the deck owns, and at what granularity they are removed
// ---------------------------------------------------------------------------
//
// [`crate::hooks_manage`] (Claude) worked this out first, over issues #535,
// #536, #733 and PRD #381, and then kept it to itself: the Codex and Devin
// adapters each carried a one-line `ends_with(SUFFIX)` predicate and a
// **whole-rule** `retain` on top of it. Issue #730 is the two defects that
// follow from the difference — a user's sibling handler sharing a rule object
// with a deck command is deleted along with it, and a deck rule naming a
// different but perfectly valid install is repointed on every launch.
//
// These are the parts all three adapters need, parameterized by the caller's
// own `HOOK_COMMAND_SUFFIX` so the three predicates cannot drift apart again.
// What stays per-adapter is what genuinely differs: Claude's LEGACY
// (`<path> hook`, no `--agent`) rule shape, which Codex and Devin never wrote
// and must not start recognising, and which hook events each installs.

/// Parse `command` as `<executable> <suffix>` — the shape
/// [`build_command`] produces — recovering the executable by parsing from the
/// RIGHT (`strip_suffix`), not by counting whitespace-split tokens, so a quoted
/// (or historically unquoted) executable path containing spaces still
/// round-trips. Returns `None` for a command that is not deck-owned at all, and
/// for one that is nothing *but* the suffix: `hook --agent codex` names some
/// program called `hook` on the agent's `$PATH`, which is not a command this
/// project has ever written.
///
/// The returned token may still be shell-quoted; pass it through
/// [`unquote_if_needed`] before comparing it as a path.
pub(crate) fn command_executable<'a>(command: &'a str, suffix: &str) -> Option<&'a str> {
    let exe = command.trim_end().strip_suffix(suffix)?;
    let exe = exe.strip_suffix(' ')?;
    if exe.is_empty() { None } else { Some(exe) }
}

/// Undo the quoting [`build_command`] applies: strip a single- or
/// double-quoted wrapper and unescape it back to the raw path, or return `exe`
/// unchanged if it was never quoted. Tries BOTH quoting forms regardless of
/// platform — not just the one this platform's writer produces — so a config
/// written on one platform and read on another is not stranded.
pub(crate) fn unquote_if_needed(exe: &str) -> std::borrow::Cow<'_, str> {
    if let Some(inner) = exe.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return std::borrow::Cow::Owned(inner.replace(r"'\''", "'"));
    }
    if let Some(inner) = exe.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return std::borrow::Cow::Owned(inner.replace("\\\"", "\""));
    }
    std::borrow::Cow::Borrowed(exe)
}

/// Whether two executable FILE NAMES name the same binary, judged by the host
/// platform's own conventions rather than by byte equality.
///
/// **Unix: byte equality, unchanged.** [`std::env::consts::EXE_SUFFIX`] is
/// empty, so [`strip_suffix_ignoring_ascii_case`] is a literal no-op and the
/// comparison stays exact and case-sensitive. `foo.exe` on Unix is a genuinely
/// different file name from `foo` and this must keep saying so — which is why
/// the suffix is taken from `EXE_SUFFIX` and never hardcoded as `".exe"`.
///
/// **Windows: the suffix and the case are not part of a program's identity.**
/// `dot-agent-deck` and `dot-agent-deck.exe` are the same binary — that is
/// precisely what `PATHEXT` resolution means — and the filesystem is
/// case-insensitive, so `Dot-Agent-Deck.EXE` is that same binary again.
///
/// PR #733's `build-windows` run is what proved both call sites needed it:
/// `durable_binary_path` always resolves a name carrying `EXE_SUFFIX` while
/// `DEFAULT_BINARY_NAME` never does, so comparing raw basenames could not
/// recognise a legacy Windows pin as ours to repair and issue #536 stayed open
/// on that platform.
pub(crate) fn binary_names_match(a: &str, b: &str) -> bool {
    binary_names_match_under(a, b, std::env::consts::EXE_SUFFIX, cfg!(windows))
}

/// [`binary_names_match`] with the host's two conventions injected instead of
/// read from the target: the executable suffix, and whether file names are
/// case-insensitive.
///
/// Split out **so the arithmetic is testable on any platform**, which is not a
/// stylistic preference here. PR #733's defect was Windows-only, could not be
/// reproduced on the machine that had to fix it (`aws-lc-sys` does not
/// cross-compile), and a `cfg!(windows)` branch covered by no test that runs
/// where its author works is precisely how the first one shipped green.
/// Passing `("", false)` reproduces every Unix exactly — an empty suffix makes
/// [`strip_suffix_ignoring_ascii_case`] the identity, leaving plain `==`.
pub(crate) fn binary_names_match_under(
    a: &str,
    b: &str,
    exe_suffix: &str,
    case_insensitive: bool,
) -> bool {
    let a = strip_suffix_ignoring_ascii_case(a, exe_suffix);
    let b = strip_suffix_ignoring_ascii_case(b, exe_suffix);
    if case_insensitive {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// `name` without one trailing `suffix`, matched case-insensitively because
/// Windows spells its executable suffix both `.exe` and `.EXE`. Returns `name`
/// untouched when `suffix` is empty (every Unix), when it is absent, and when
/// the name is nothing BUT the suffix — a file called `.exe` is a name in its
/// own right, not an empty one.
pub(crate) fn strip_suffix_ignoring_ascii_case<'a>(name: &'a str, suffix: &str) -> &'a str {
    if suffix.is_empty() {
        return name;
    }
    match name.len().checked_sub(suffix.len()) {
        // `is_char_boundary` is load-bearing, not defensive: a basename ending
        // in a multi-byte character can put `cut` inside one, and slicing
        // there panics.
        Some(cut)
            if cut > 0
                && name.is_char_boundary(cut)
                && name[cut..].eq_ignore_ascii_case(suffix) =>
        {
            &name[..cut]
        }
        _ => name,
    }
}

/// Whether `existing` and `installing` (both already unquoted) name the SAME
/// binary, so a rule for `existing` should be replaced rather than left
/// alongside a fresh rule for `installing`. Symlinks are resolved first — the
/// real-world case this exists for: a `dot-agent-deck` symlink pointing at a
/// renamed `worker-agent-deck` collapses to one rule. Every path here can fail
/// to resolve (most fixtures are never written to disk), so resolution failure
/// falls back to a literal string comparison; this never panics or unwraps on
/// it.
pub(crate) fn executables_match(existing: &str, installing: &str) -> bool {
    if let (Ok(existing_real), Ok(installing_real)) = (
        Path::new(existing).canonicalize(),
        Path::new(installing).canonicalize(),
    ) {
        return existing_real == installing_real;
    }
    existing == installing
}

/// Whether `exe` — the executable a deck-owned command names, already unquoted
/// — is a STALE SIBLING of the binary currently installing: it shares that
/// binary's own basename ([`binary_names_match`], so the host's
/// executable-suffix and case conventions decide what "same basename" means)
/// and its pin is not one the deck would write
/// ([`crate::platform::paths::pin_is_repairable`]).
///
/// This is the repair gate PRD #381 Open Question 3 settles on, and the whole of
/// its conservatism lives in those two conjuncts. #381 spelt it "repair only
/// when the target is **positively missing**", which was accurate for the
/// `try_exists`-only check of the day; [`crate::platform::paths::pin_is_repairable`]
/// has since widened it to the whole "is this a pin the deck would write"
/// question, so a bare or relative pin, a non-executable file and a
/// `target/{debug,release}` path are replaceable too — two of them while naming
/// a file that exists. The conservatism is unchanged in direction, but do not
/// read "missing" as the boundary.
///
/// **Nor "positively known" as the standard**, which is what this conjunct said
/// until issue #1027 (item 6) checked it against those four true-cases. Only one
/// of them — an absolute path the OS answers `Ok(false)` for — is a positive
/// determination about the file. A bare or relative pin is judged with no
/// filesystem access at all, and is replaceable because it is *cwd-dependent*,
/// not because it fails to run: #536 is about such a pin running, through the
/// agent's `$PATH`, as a binary nobody chose. A live `target/{debug,release}`
/// path works this minute and is replaceable for not being durable. The claim
/// this makes is "not a pin the deck would write", and that is the one it can
/// carry.
///
/// The basename half is what keeps a deck rule for a genuinely
/// *different-looking* binary out of it — most hook fixtures name fictional
/// paths that were never on disk, and they must not be swept up just because
/// they do not exist. The `pin_is_repairable` half is what keeps a
/// working binary behind an unmounted volume, or one this process cannot
/// `stat`, out of it: a stat error on a well-formed absolute pin means "leave
/// alone", because deleting a working user's hook is worse than leaving a stale
/// rule.
///
/// Callers must have established deck ownership already — pass only the
/// executable of a command [`command_executable`] (or an adapter's legacy
/// equivalent) claimed. So this never sees a command that fails the suffix
/// test, which is what keeps an ordinary user hook out of it.
///
/// **That is the narrow claim, and the wide one would be false.** Ownership
/// upstream is the *suffix*, and the suffix is a convention, not a capability:
/// a user-authored command that deliberately ends in `hook --agent codex` is
/// indistinguishable from a deck entry under it — which is the whole premise of
/// issue #730. Such a command, under this binary's own basename, with a pin the
/// OS positively reports missing, IS pruned here. The two conjuncts above are
/// what keep that case rare rather than impossible; nothing at this layer makes
/// it impossible.
pub(crate) fn pin_is_dead_sibling(exe: &str, binary_path: &str) -> bool {
    pin_is_same_named(exe, binary_path) && crate::platform::paths::pin_is_repairable(exe)
}

/// Whether `exe` names the same program as `binary_path`, wherever either sits
/// on disk — the basename half of [`pin_is_dead_sibling`], without its liveness
/// half.
///
/// Split out for issue #1171. The install path needs to notice a deck rule for
/// the same binary NAME at a different path that is very much alive: two
/// installs of the deck (Homebrew's and `~/.local/bin`'s, say) each keep a rule,
/// every hook event is then delivered twice, and nothing said so. That rule is
/// not dead, so [`pin_is_dead_sibling`] cannot see it.
///
/// The fail-safe is the same and is deliberate: an installing path with no
/// usable basename (empty, `..`-terminated, or non-UTF-8) matches nothing,
/// because the one direction that costs a user their rule is a false match.
pub(crate) fn pin_is_same_named(exe: &str, binary_path: &str) -> bool {
    let Some(installing) = Path::new(binary_path)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    Path::new(exe)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|existing| binary_names_match(existing, installing))
}

/// Every command string a rule carries, from either JSON shape: the current
/// nested `{"hooks": [{"command": ...}]}` or the legacy flat
/// `{"command": ...}`.
///
/// **Test-only since PR #1029, and the gate is a deliberate speed bump rather
/// than tidying.** Its one production caller was [`strip_deck_commands`]'s
/// emptiness check, and that was the P1: "does this rule still carry a COMMAND"
/// is a narrower question than "does it still carry a HANDLER", and answering
/// the first deleted user handlers that answer only the second. Every remaining
/// caller is an assertion, where reading the commands back IS the question. If a
/// production path ever needs this, un-gating it is the moment to check which of
/// the two questions is actually being asked — see [`rule_retains_a_handler`].
#[cfg(test)]
pub(crate) fn rule_commands(rule: &Value) -> impl Iterator<Item = &str> {
    let nested = rule
        .get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str));
    let flat = rule.get("command").and_then(Value::as_str).into_iter();
    nested.chain(flat)
}

/// Whether a rule object still carries a HANDLER of any kind — the emptiness
/// question [`strip_deck_commands`] drops a rule on, asked AFTER the strip.
///
/// Deliberately not `rule_commands(rule).next().is_some()`, which is what the
/// check was until Greptile found the gap on PR #1029. `rule_commands` yields
/// only handlers carrying a **string** `command`, so a sibling handler with no
/// `command` key, or with a non-string one, survives the `retain` in
/// [`strip_deck_commands`] and is then invisible to the emptiness test — and the
/// rule was dropped, taking that handler and the user's `matcher` with it. Codex
/// accepts such handler objects (`validate_structure` there requires only that
/// each event VALUE be an array; it says nothing about the objects inside one),
/// so this was issue #730's own harm reached through a narrower door, and it
/// applied to all four adapters because this helper is shared.
///
/// So the question here is "is anything left?", not "is a command left?":
///
/// - a `hooks` value that is a non-empty array means a handler survived,
///   whatever shape that handler is;
/// - a `hooks` value that is not an array at all is something this module does
///   not understand, so it counts as content rather than as emptiness;
/// - a surviving `command` key of ANY type means the legacy flat rule still
///   carries one — [`strip_deck_commands`] removes that key only when its value
///   is a string it claimed, so whatever is still there is not the deck's.
///
/// Every arm errs toward keeping, which is the direction this whole helper is
/// written in: never delete what we did not write.
fn rule_retains_a_handler(rule: &Value) -> bool {
    rule.get("hooks")
        .is_some_and(|hooks| hooks.as_array().is_none_or(|list| !list.is_empty()))
        || rule.get("command").is_some()
}

/// Remove every command matching `is_target` from `rules`, dropping a rule
/// object only once it carries no handler at all — the fix for issue #535
/// (Claude) and, for the Codex and Devin adapters, for issue #730.
///
/// A rule's `hooks` array is a LIST of commands sharing one matcher, so a user
/// who put their own hook and the deck's in the same rule object is doing a
/// normal thing. Removal used to be a `retain` over whole rules keyed on an
/// `any()` across that list, so one deck command anywhere in a rule deleted the
/// user's commands with it — measured in #535, where a user's
/// `/usr/local/bin/my-critical-audit.sh` disappeared on `hooks uninstall` and
/// nothing said so. Install has the identical granularity and is the more
/// frequent path, since every adapter's auto-install runs unattended at
/// startup.
///
/// Two deliberate conservatisms, both in the "never delete what we did not
/// write" direction:
///
/// - a rule NOTHING matched in is returned untouched, so an already-empty or
///   command-less rule object is never tidied away as a side effect;
/// - a rule is dropped only when [`rule_retains_a_handler`] reports nothing
///   left in it at all — not merely no *command*, which is a narrower question
///   that used to delete a user's command-less handler along with their
///   `matcher`.
///
/// Returns the number of individual commands removed.
/// Every command string in `rules`, across both shapes
/// [`strip_deck_commands`] walks — the current
/// `{"hooks": [{"command": …}]}` and the legacy flat `{"command": …}`.
///
/// Read-only twin of that traversal, kept beside it so the two cannot drift on
/// which shapes they understand.
pub(crate) fn rule_command_strs(rules: &[Value]) -> Vec<&str> {
    let mut out = Vec::new();
    for rule in rules {
        if let Some(hooks) = rule.get("hooks").and_then(Value::as_array) {
            out.extend(
                hooks
                    .iter()
                    .filter_map(|hook| hook.get("command").and_then(Value::as_str)),
            );
        }
        if let Some(command) = rule.get("command").and_then(Value::as_str) {
            out.push(command);
        }
    }
    out
}

pub(crate) fn strip_deck_commands(
    rules: &mut Vec<Value>,
    mut is_target: impl FnMut(&str) -> bool,
) -> usize {
    let mut removed = 0usize;
    rules.retain_mut(|rule| {
        let before = removed;

        // Current shape: `{"hooks": [{"command": …}, …]}` — drop just the
        // matching command objects and leave the rest of the array, and the
        // rule's own `matcher`, exactly as the user wrote them.
        if let Some(hooks) = rule.get_mut("hooks").and_then(Value::as_array_mut) {
            let len = hooks.len();
            hooks.retain(|hook| {
                !hook
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(&mut is_target)
            });
            removed += len - hooks.len();
        }

        // Legacy flat shape: `{"command": …}` — the command IS the rule, so
        // there is nothing smaller to remove. Take the key out and let the
        // nothing-left check below decide the rule's fate, rather than assuming
        // it carries nothing else.
        if rule
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(&mut is_target)
        {
            if let Some(obj) = rule.as_object_mut() {
                obj.remove("command");
            }
            removed += 1;
        }

        if removed == before {
            return true;
        }
        rule_retains_a_handler(rule)
    });
    removed
}

/// Whether `exe` — the unquoted executable of a command that already has the
/// deck's hook-command shape — is a deck INSTALL the installing binary may
/// replace: the same binary ([`executables_match`]), or any executable sharing
/// its basename ([`pin_is_same_named`]), live or dead.
///
/// This is the "one deck entry per event" ownership test (PRD #1487), and it
/// deliberately reverses issue #730's preserve-a-valid-sibling policy for the
/// events an install writes: two installs of the deck (Homebrew's and
/// `~/.local/bin`'s, a sidecar and a CLI, a build left on `$PATH`) each kept a
/// rule, every hook event was delivered once per rule, and an added rule is a
/// new untrusted hook Codex holds every start on until someone reviews it.
///
/// The identity check is the suffix shape plus the basename — the same gate
/// dead-pin repair has used since PRD #381. It never matches a command naming a
/// differently named executable, so a user's handler that merely ends with the
/// deck's verb survives unless it also names a `dot-agent-deck` (or whatever
/// the installing binary is called). Nothing is executed to verify it.
pub(crate) fn is_replaceable_deck_install(exe: &str, binary_path: &str) -> bool {
    executables_match(exe, binary_path) || pin_is_same_named(exe, binary_path)
}

/// Refresh the deck's command inside ONE event array **without moving any
/// handler that is not the deck's**, and consolidate the deck's other copies in
/// that array to the one it keeps (PRD #1487). Returns where the kept copy sits,
/// or `None` when there is nothing to claim — no nested handler `is_own`
/// accepts, or a legacy flat `{"command": …}` deck rule reached before any —
/// and the caller then falls back to strip-then-append, which moves nothing that
/// was not removed.
///
/// Shared by the Codex, Claude and Devin writers so the three apply one policy
/// (CLAUDE.md rule 20). Position matters most to Codex, which keys a trust
/// grant by `<event>:<group_idx>:<handler_idx>` (issue #1034 — the measurement
/// is on `codex_hooks_manage::refresh_deck_rule_in_place`), so the rules below
/// are written for it and cost the other two nothing:
///
/// - The FIRST own handler in walk order (rules in order, handlers in order) is
///   claimed and its `command` overwritten in place; every other key on it, and
///   the rule's `matcher`, are left as they are.
/// - Every later own handler is removed **only from the tail of its rule**,
///   where removing it shifts nothing. An own handler that a non-deck handler
///   follows inside the same rule is kept and refreshed instead, because
///   removing it would re-key the handler after it — so in that one shape a
///   duplicate survives, and it runs the current deck rather than a stale pin.
/// - A rule this pass emptied is dropped only when it is TRAILING; an interior
///   one stays as an empty rule so every rule after it keeps its index (Codex
///   accepts and ignores an empty rule — measured on 0.149.0).
/// - A legacy flat own command after the claim is removed; the rule object
///   stays unless it is trailing and was emptied by that.
pub(crate) fn consolidate_deck_handlers_in_place(
    rules: &mut Vec<Value>,
    command: &str,
    is_own: impl Fn(&str) -> bool,
) -> Option<(usize, usize)> {
    let own = |value: &Value| value.as_str().is_some_and(&is_own);

    let mut claim = None;
    'scan: for (rule_idx, rule) in rules.iter().enumerate() {
        if let Some(handlers) = rule.get("hooks").and_then(Value::as_array) {
            for (handler_idx, handler) in handlers.iter().enumerate() {
                if handler.get("command").is_some_and(&own) {
                    claim = Some((rule_idx, handler_idx));
                    break 'scan;
                }
            }
        }
        if rule.get("command").is_some_and(&own) {
            return None;
        }
    }
    let (claimed_rule, claimed_handler) = claim?;

    let mut emptied = vec![false; rules.len()];
    for (rule_idx, rule) in rules.iter_mut().enumerate().skip(claimed_rule) {
        let had_a_handler = rule_retains_a_handler(rule);
        if let Some(handlers) = rule.get_mut("hooks").and_then(Value::as_array_mut) {
            while let Some(last_idx) = handlers.len().checked_sub(1) {
                if (rule_idx, last_idx) == (claimed_rule, claimed_handler)
                    || !handlers[last_idx].get("command").is_some_and(&own)
                {
                    break;
                }
                handlers.pop();
            }
        }
        if rule.get("command").is_some_and(&own)
            && let Some(object) = rule.as_object_mut()
        {
            object.remove("command");
        }
        emptied[rule_idx] = had_a_handler && !rule_retains_a_handler(rule);
    }

    for rule in rules.iter_mut().skip(claimed_rule) {
        let Some(handlers) = rule.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        for handler in handlers.iter_mut() {
            if !handler.get("command").is_some_and(&own) {
                continue;
            }
            let Some(object) = handler.as_object_mut() else {
                continue;
            };
            object.insert("command".into(), Value::String(command.to_string()));
            // Only when absent: an existing `type` is not ours to change.
            object
                .entry("type")
                .or_insert_with(|| Value::String("command".into()));
        }
    }

    while rules.len() > claimed_rule + 1 && emptied[rules.len() - 1] {
        rules.pop();
    }
    Some((claimed_rule, claimed_handler))
}

/// Which kind of install is running, because the two treat another install's
/// working entry differently (PRD #1487).
///
/// - [`InstallMode::Explicit`] is `hooks install`: the user asked for THIS
///   binary, so it replaces whatever deck entry is there.
/// - [`InstallMode::Automatic`] is every silent path (TUI and daemon startup,
///   `wrap --agent codex`): a deck entry naming another install that is live and
///   durable is kept as the one entry, so two installs that each resolve to
///   themselves (Homebrew's TUI and a `~/.local/bin` daemon, the desktop's
///   bundled daemon and a CLI) do not rewrite the agent's config on every start.
///   See [`auto_install_kept_entry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallMode {
    Explicit,
    Automatic,
}

/// Whether an automatic install keeps a deck entry pinned to `exe`: an absolute
/// path to an executable that is not cargo build output — the inverse of
/// [`crate::platform::paths::pin_is_repairable`], so "dead or unusable" means
/// exactly what dead-pin repair has always meant. A pin that cannot be stat'ed
/// is kept, which errs toward writing nothing.
pub(crate) fn auto_install_keeps(exe: &str) -> bool {
    !crate::platform::paths::pin_is_repairable(exe)
}

/// The live, durable deck entry an [`InstallMode::Automatic`] install keeps in
/// one event array — see [`auto_install_kept_entry`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KeptDeckEntry {
    /// The installing binary's own: the ordinary refresh writes the same
    /// command, so there is nothing to keep apart from it.
    ThisBinary,
    /// Another install's: written back verbatim, so it stays byte for byte.
    Other { command: String, exe: String },
}

impl KeptDeckEntry {
    /// The command an event array is written with, given what it keeps itself
    /// (`here`) and what the file's first kept entry is (`keeper`, for an
    /// event with no live entry of its own): another install's command when
    /// one decides it, else `own` — so an automatic install leaves the file
    /// naming the install it already names.
    pub(crate) fn command_for<'a>(
        here: Option<&'a Self>,
        keeper: Option<&'a Self>,
        own: &'a str,
    ) -> &'a str {
        match here.or(keeper) {
            Some(Self::Other { command, .. }) => command,
            Some(Self::ThisBinary) | None => own,
        }
    }

    /// The binary the deck's entries name after an install whose file-wide
    /// keeper is `keeper`.
    pub(crate) fn named_binary(keeper: Option<&Self>, binary_path: &str) -> String {
        match keeper {
            Some(Self::Other { exe, .. }) => exe.clone(),
            Some(Self::ThisBinary) | None => binary_path.to_string(),
        }
    }
}

/// For an [`InstallMode::Automatic`] install by `binary_path`: the first deck
/// command in `rules` (walk order, nested handlers) that `is_own` accepts and
/// whose executable is a live durable install ([`auto_install_keeps`]), or
/// `None` when no deck entry there is.
///
/// The caller writes another install's command back instead of its own, so
/// [`consolidate_deck_handlers_in_place`] keeps that entry where it sits, byte
/// for byte, and still consolidates the other deck copies down to it. A dead or
/// build-output entry ahead of it is overwritten in place with the kept command.
pub(crate) fn auto_install_kept_entry(
    rules: &[Value],
    binary_path: &str,
    is_own: impl Fn(&str) -> bool,
    executable_of: impl Fn(&str) -> Option<String>,
) -> Option<KeptDeckEntry> {
    let (command, exe) = rules
        .iter()
        .filter_map(|rule| rule.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|handler| handler.get("command").and_then(Value::as_str))
        .filter(|command| is_own(command))
        .find_map(|command| {
            executable_of(command)
                .filter(|exe| auto_install_keeps(exe))
                .map(|exe| (command.to_string(), exe))
        })?;
    Some(if executables_match(&exe, binary_path) {
        KeptDeckEntry::ThisBinary
    } else {
        KeptDeckEntry::Other { command, exe }
    })
}

/// Log an automatic install that actually changed an agent's config (PRD
/// #1487). A no-op logs nothing at info: what this records is that the deck
/// rewrote somebody else's file, which is exactly what went unnoticed when test
/// fixtures rewrote the operator's real Codex hooks.
pub(crate) fn log_auto_install_change(
    agent: &str,
    destination: &Path,
    binary: &str,
    trigger: &str,
) {
    tracing::info!(
        agent,
        destination = %destination.display(),
        binary,
        trigger,
        pid = std::process::id(),
        process = %process_label(),
        "auto-install changed agent hook config"
    );
}

/// The deck subcommand this process is running (`daemon serve`, `wrap`, or the
/// TUI when there is none), for [`log_auto_install_change`]. Only the leading
/// words that are not flags — never an argument value that could carry a path
/// or a secret.
fn process_label() -> String {
    let words: Vec<String> = std::env::args()
        .skip(1)
        .take_while(|arg| !arg.starts_with('-'))
        .take(2)
        .filter(|arg| arg.chars().all(|c| c.is_ascii_lowercase() || c == '-'))
        .collect();
    if words.is_empty() {
        "tui".to_string()
    } else {
        words.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(unix)]
    fn config_fingerprint(path: &Path) -> (Vec<u8>, u64, i64, i64) {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).expect("stat fixture config");
        (
            std::fs::read(path).expect("read fixture config"),
            metadata.ino(),
            metadata.mtime(),
            metadata.mtime_nsec(),
        )
    }

    #[cfg(unix)]
    fn install_config(agent: &str, home: &Path, binary: &str) -> io::Result<()> {
        match agent {
            "codex" => crate::codex_hooks_manage::install_to(home, binary),
            "claude-code" => crate::hooks_manage::install_to(&home.join("settings.json"), binary),
            "devin" => crate::devin_hooks_manage::install_to(home, binary),
            _ => panic!("unknown fixture agent"),
        }
    }

    #[cfg(unix)]
    fn uninstall_config(agent: &str, home: &Path) -> io::Result<()> {
        match agent {
            "codex" => crate::codex_hooks_manage::uninstall_from(home),
            "claude-code" => crate::hooks_manage::uninstall_from(&home.join("settings.json")),
            "devin" => crate::devin_hooks_manage::uninstall_from(home).map(|_| ()),
            _ => panic!("unknown fixture agent"),
        }
    }

    #[cfg(unix)]
    fn config_name(agent: &str) -> &str {
        match agent {
            "codex" => "hooks.json",
            "claude-code" => "settings.json",
            "devin" => "config.json",
            _ => panic!("unknown fixture agent"),
        }
    }

    #[cfg(unix)]
    fn no_op_preserves_config(agent: &str, uninstall: bool) {
        let fixture = crate::test_temp::tempdir().expect("owned config fixture");
        let home = fixture.path().join("fake-operator-home");
        std::fs::create_dir_all(&home).unwrap();
        let binary = fixture.path().join("installed/dot-agent-deck");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        crate::test_isolation::write_script(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(
            &binary,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        let path = home.join(config_name(agent));
        if uninstall {
            std::fs::write(&path, b"{\"hooks\":{}, \"userSetting\":true}\n").unwrap();
        } else {
            install_config(agent, &home, binary.to_str().unwrap()).unwrap();
            // Deliberately keep the same definitions with user formatting.
            let document: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        }
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(123456)),
            )
            .unwrap();
        let before = config_fingerprint(&path);
        if uninstall {
            uninstall_config(agent, &home).unwrap();
        } else {
            install_config(agent, &home, binary.to_str().unwrap()).unwrap();
        }
        assert_eq!(
            config_fingerprint(&path),
            before,
            "{agent}: equal merged definitions must preserve bytes, inode and mtime"
        );
    }

    /// Scenario: Repeat a Codex install with identical definitions and user formatting. Its hooks file must keep its bytes, inode and mtime.
    #[cfg(unix)]
    #[test]
    fn config_no_op_codex_install_preserves_file() {
        no_op_preserves_config("codex", false);
    }

    /// Scenario: Uninstall Codex hooks from a user file with no deck entries. Nothing on disk may change.
    #[cfg(unix)]
    #[test]
    fn config_no_op_codex_uninstall_preserves_file() {
        no_op_preserves_config("codex", true);
    }

    /// Scenario: Repeat a Claude install with identical definitions and user formatting. Its settings file must keep its bytes, inode and mtime.
    #[cfg(unix)]
    #[test]
    fn config_no_op_claude_install_preserves_file() {
        no_op_preserves_config("claude-code", false);
    }

    /// Scenario: Uninstall Claude hooks from settings containing no deck hooks. The file remains untouched.
    #[cfg(unix)]
    #[test]
    fn config_no_op_claude_uninstall_preserves_file() {
        no_op_preserves_config("claude-code", true);
    }

    /// Scenario: Repeat a Devin install with identical definitions and user formatting. Its config file must keep its bytes, inode and mtime.
    #[cfg(unix)]
    #[test]
    fn config_no_op_devin_install_preserves_file() {
        no_op_preserves_config("devin", false);
    }

    /// Scenario: Uninstall Devin hooks from a config containing no deck hooks. The file remains untouched.
    #[cfg(unix)]
    #[test]
    fn config_no_op_devin_uninstall_preserves_file() {
        no_op_preserves_config("devin", true);
    }

    #[cfg(unix)]
    fn consolidate_config(agent: &str) {
        let fixture = crate::test_temp::tempdir().unwrap();
        let home = fixture.path().join("fake-operator-home");
        std::fs::create_dir_all(&home).unwrap();
        let old = fixture.path().join("old/dot-agent-deck");
        let new = fixture.path().join("new/dot-agent-deck");
        for binary in [&old, &new] {
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            crate::test_isolation::write_script(binary, b"#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(
                binary,
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
            )
            .unwrap();
        }
        let suffix = format!("hook --agent {agent}");
        let old_command = build_command(old.to_str().unwrap(), &suffix, HookShell::Posix);
        let new_command = build_command(new.to_str().unwrap(), &suffix, HookShell::Posix);
        let user = json!({"type":"command", "command":"/user/audit-handler"});
        let lookalike = json!({"type":"command", "command":format!("/user/not-the-deck {suffix}")});
        let original = json!({"hooks":{"PreToolUse":[
            {"matcher":"Bash", "hooks":[{"type":"command", "command":old_command}, user]},
            {"hooks":[{"type":"command", "command":old_command}]},
            {"matcher":"Read", "hooks":[lookalike]},
            {"hooks":[{"type":"command", "command":old_command}]}
        ]}, "userSetting":true});
        let path = home.join(config_name(agent));
        std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
        install_config(agent, &home, new.to_str().unwrap()).unwrap();
        let document: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let rules = document["hooks"]["PreToolUse"].as_array().unwrap();
        let commands = rule_command_strs(rules);
        assert_eq!(
            commands
                .iter()
                .filter(|c| **c == old_command || **c == new_command)
                .count(),
            1,
            "{agent}: a different valid install must consolidate deck duplicates to one: {document:#}"
        );
        assert_eq!(
            document["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            new_command
        );
        assert_eq!(
            document["hooks"]["PreToolUse"][0]["hooks"][1], user,
            "user handler index changed"
        );
        assert_eq!(
            document["hooks"]["PreToolUse"][2], original["hooks"]["PreToolUse"][2],
            "foreign handler or its group index changed"
        );
        assert_eq!(document["userSetting"], true);
    }

    /// Scenario: Install Codex from a different valid path over duplicate deck entries mixed with user handlers. Only one deck entry remains, while user handlers keep their positions.
    #[cfg(unix)]
    #[test]
    fn config_consolidation_codex_preserves_user_indices() {
        consolidate_config("codex");
    }

    /// Scenario: Install Claude from a different valid path over duplicate deck entries mixed with user handlers. Only one deck entry remains, while user handlers keep their positions.
    #[cfg(unix)]
    #[test]
    fn config_consolidation_claude_preserves_user_indices() {
        consolidate_config("claude-code");
    }

    /// Scenario: Install Devin from a different valid path over duplicate deck entries mixed with user handlers. Only one deck entry remains, while user handlers keep their positions.
    #[cfg(unix)]
    #[test]
    fn config_consolidation_devin_preserves_user_indices() {
        consolidate_config("devin");
    }

    // PRD #1487 ruling: an AUTOMATIC install keeps another live, durable
    // install's deck entry; only a dead or build-output one is replaced, and an
    // explicit `hooks install` still replaces whatever is there.

    #[cfg(unix)]
    fn seed_deck(path: &Path) -> String {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::test_isolation::write_script(path, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(
            path,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        path.to_str().unwrap().to_string()
    }

    /// The binary an automatic install says the file names, where the writer
    /// reports one (Claude's seam does not).
    #[cfg(unix)]
    fn auto_install_config(agent: &str, home: &Path, binary: &str) -> Option<String> {
        match agent {
            "codex" => Some(
                crate::codex_hooks_manage::auto_install_to(home, binary)
                    .unwrap()
                    .1,
            ),
            "claude-code" => {
                crate::hooks_manage::auto_install_to(&home.join("settings.json"), || {
                    Ok(binary.to_string())
                });
                None
            }
            "devin" => Some(
                crate::devin_hooks_manage::auto_install_to(home, binary)
                    .unwrap()
                    .1,
            ),
            _ => panic!("unknown fixture agent"),
        }
    }

    #[cfg(unix)]
    fn age_config(path: &Path) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(123456)),
            )
            .unwrap();
    }

    /// One event's deck commands as `(rule_idx, handler_idx, command)`.
    #[cfg(unix)]
    type DeckPositions = Vec<(usize, usize, String)>;

    /// Every event's [`DeckPositions`].
    #[cfg(unix)]
    fn deck_positions(agent: &str, path: &Path) -> Vec<(String, DeckPositions)> {
        let suffix = format!("hook --agent {agent}");
        let document: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let mut out = Vec::new();
        for (event, rules) in document["hooks"].as_object().unwrap() {
            let mut positions = Vec::new();
            for (rule_idx, rule) in rules.as_array().unwrap().iter().enumerate() {
                for (handler_idx, handler) in
                    rule["hooks"].as_array().into_iter().flatten().enumerate()
                {
                    if let Some(command) = handler["command"].as_str()
                        && command.ends_with(&suffix)
                    {
                        positions.push((rule_idx, handler_idx, command.to_string()));
                    }
                }
            }
            out.push((event.clone(), positions));
        }
        out
    }

    /// Put a user rule in front of every event's rules (so a replacement in
    /// place is told apart from an append) and, when given, a rule holding
    /// `trailing` after them.
    #[cfg(unix)]
    fn surround_deck_rules(path: &Path, trailing: Option<&str>) {
        let mut document: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        for rules in document["hooks"].as_object_mut().unwrap().values_mut() {
            let rules = rules.as_array_mut().unwrap();
            rules.insert(
                0,
                json!({"hooks":[{"type":"command", "command":"/user/audit-handler"}]}),
            );
            if let Some(command) = trailing {
                rules.push(json!({"hooks":[{"type":"command", "command":command}]}));
            }
        }
        std::fs::write(path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    }

    #[cfg(unix)]
    fn assert_every_event_names(agent: &str, path: &Path, binary: &str, why: &str) {
        let expected = build_command(binary, &format!("hook --agent {agent}"), HookShell::Posix);
        for (event, positions) in deck_positions(agent, path) {
            assert_eq!(
                positions,
                vec![(1, 0, expected.clone())],
                "{agent}/{event}: {why}"
            );
        }
    }

    #[cfg(unix)]
    fn auto_keeps_a_live_install(agent: &str) {
        let fixture = crate::test_temp::tempdir().unwrap();
        let home = fixture.path().join("fake-operator-home");
        std::fs::create_dir_all(&home).unwrap();
        let a = seed_deck(&fixture.path().join("homebrew").join("dot-agent-deck"));
        let b = seed_deck(&fixture.path().join("local").join("dot-agent-deck"));
        install_config(agent, &home, &a).unwrap();
        let path = home.join(config_name(agent));
        surround_deck_rules(&path, None);
        age_config(&path);
        let before = config_fingerprint(&path);

        let named = auto_install_config(agent, &home, &b);

        assert_eq!(
            config_fingerprint(&path),
            before,
            "{agent}: an automatic install from B must not touch a file whose deck entry names \
             a live install A"
        );
        if let Some(named) = named {
            assert_eq!(named, a, "{agent}: the entries still name A");
        }
    }

    /// Scenario: Install Codex hooks from live install A, then auto-install from install B. The hooks file keeps its bytes, inode and mtime and still names A.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_codex_keeps_a_live_install() {
        auto_keeps_a_live_install("codex");
    }

    /// Scenario: Install Claude hooks from live install A, then auto-install from install B. The settings file keeps its bytes, inode and mtime.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_claude_keeps_a_live_install() {
        auto_keeps_a_live_install("claude-code");
    }

    /// Scenario: Install Devin hooks from live install A, then auto-install from install B. The config file keeps its bytes, inode and mtime and still names A.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_devin_keeps_a_live_install() {
        auto_keeps_a_live_install("devin");
    }

    #[cfg(unix)]
    fn auto_replaces_an_unusable_install(agent: &str, unusable: &str) {
        let fixture = crate::test_temp::tempdir().unwrap();
        let home = fixture.path().join("fake-operator-home");
        std::fs::create_dir_all(&home).unwrap();
        let a = match unusable {
            "dead" => seed_deck(&fixture.path().join("pruned").join("dot-agent-deck")),
            "cargo-output" => {
                // A custom target-dir name: recognised by cargo's own siblings.
                let profile = fixture.path().join("custom-target").join("debug");
                std::fs::create_dir_all(profile.join(".fingerprint")).unwrap();
                std::fs::create_dir_all(profile.join("deps")).unwrap();
                seed_deck(&profile.join("dot-agent-deck"))
            }
            _ => unreachable!(),
        };
        let b = seed_deck(&fixture.path().join("local").join("dot-agent-deck"));
        install_config(agent, &home, &a).unwrap();
        if unusable == "dead" {
            std::fs::remove_file(&a).unwrap();
        }
        let path = home.join(config_name(agent));
        surround_deck_rules(&path, None);

        let named = auto_install_config(agent, &home, &b);

        assert_every_event_names(
            agent,
            &path,
            &b,
            &format!("a {unusable} A entry is replaced by B in place"),
        );
        if let Some(named) = named {
            assert_eq!(named, b);
        }
    }

    /// Scenario: Install Codex hooks from A, delete A, then auto-install from B. B's command replaces A's at the same position, after the user's rule.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_codex_replaces_a_dead_install_in_place() {
        auto_replaces_an_unusable_install("codex", "dead");
    }

    /// Scenario: Install Claude hooks from A, delete A, then auto-install from B. B's command replaces A's at the same position, after the user's rule.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_claude_replaces_a_dead_install_in_place() {
        auto_replaces_an_unusable_install("claude-code", "dead");
    }

    /// Scenario: Install Devin hooks from A, delete A, then auto-install from B. B's command replaces A's at the same position, after the user's rule.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_devin_replaces_a_dead_install_in_place() {
        auto_replaces_an_unusable_install("devin", "dead");
    }

    /// Scenario: Install Codex hooks from a cargo build in a custom target dir, then auto-install from B. The build-output entry is replaced by B in place.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_codex_replaces_cargo_output() {
        auto_replaces_an_unusable_install("codex", "cargo-output");
    }

    /// Scenario: Install Claude hooks from a cargo build in a custom target dir, then auto-install from B. The build-output entry is replaced by B in place.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_claude_replaces_cargo_output() {
        auto_replaces_an_unusable_install("claude-code", "cargo-output");
    }

    /// Scenario: Install Devin hooks from a cargo build in a custom target dir, then auto-install from B. The build-output entry is replaced by B in place.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_devin_replaces_cargo_output() {
        auto_replaces_an_unusable_install("devin", "cargo-output");
    }

    #[cfg(unix)]
    fn explicit_replaces_a_live_install(agent: &str) {
        let fixture = crate::test_temp::tempdir().unwrap();
        let home = fixture.path().join("fake-operator-home");
        std::fs::create_dir_all(&home).unwrap();
        let a = seed_deck(&fixture.path().join("homebrew").join("dot-agent-deck"));
        let b = seed_deck(&fixture.path().join("local").join("dot-agent-deck"));
        install_config(agent, &home, &a).unwrap();
        let path = home.join(config_name(agent));
        surround_deck_rules(&path, None);

        install_config(agent, &home, &b).unwrap();

        assert_every_event_names(
            agent,
            &path,
            &b,
            "an explicit install from B replaces live A in place",
        );
    }

    /// Scenario: Install Codex hooks from live A, then run the explicit install from B. B's command replaces A's in place.
    #[cfg(unix)]
    #[test]
    fn config_explicit_install_codex_replaces_a_live_install() {
        explicit_replaces_a_live_install("codex");
    }

    /// Scenario: Install Claude hooks from live A, then run the explicit install from B. B's command replaces A's in place.
    #[cfg(unix)]
    #[test]
    fn config_explicit_install_claude_replaces_a_live_install() {
        explicit_replaces_a_live_install("claude-code");
    }

    /// Scenario: Install Devin hooks from live A, then run the explicit install from B. B's command replaces A's in place.
    #[cfg(unix)]
    #[test]
    fn config_explicit_install_devin_replaces_a_live_install() {
        explicit_replaces_a_live_install("devin");
    }

    #[cfg(unix)]
    fn auto_consolidates_duplicates_to_the_kept_install(agent: &str) {
        let fixture = crate::test_temp::tempdir().unwrap();
        let home = fixture.path().join("fake-operator-home");
        std::fs::create_dir_all(&home).unwrap();
        let a = seed_deck(&fixture.path().join("homebrew").join("dot-agent-deck"));
        let b = seed_deck(&fixture.path().join("local").join("dot-agent-deck"));
        install_config(agent, &home, &a).unwrap();
        let path = home.join(config_name(agent));
        let b_command = build_command(&b, &format!("hook --agent {agent}"), HookShell::Posix);
        surround_deck_rules(&path, Some(&b_command));

        auto_install_config(agent, &home, &b);

        assert_every_event_names(
            agent,
            &path,
            &a,
            "B's duplicate is consolidated into live A's entry, which keeps its position",
        );
    }

    /// Scenario: A Codex hooks file names live A and, after it, a duplicate entry for B. Auto-install from B leaves only A's entry, where it was.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_codex_consolidates_duplicates_to_the_kept_install() {
        auto_consolidates_duplicates_to_the_kept_install("codex");
    }

    /// Scenario: A Claude settings file names live A and, after it, a duplicate entry for B. Auto-install from B leaves only A's entry, where it was.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_claude_consolidates_duplicates_to_the_kept_install() {
        auto_consolidates_duplicates_to_the_kept_install("claude-code");
    }

    /// Scenario: A Devin config names live A and, after it, a duplicate entry for B. Auto-install from B leaves only A's entry, where it was.
    #[cfg(unix)]
    #[test]
    fn config_auto_install_devin_consolidates_duplicates_to_the_kept_install() {
        auto_consolidates_duplicates_to_the_kept_install("devin");
    }

    #[cfg(unix)]
    fn alternating_auto_installs_write_once(agent: &str) {
        let fixture = crate::test_temp::tempdir().unwrap();
        let home = fixture.path().join("fake-operator-home");
        std::fs::create_dir_all(&home).unwrap();
        let a = seed_deck(&fixture.path().join("homebrew").join("dot-agent-deck"));
        let b = seed_deck(&fixture.path().join("local").join("dot-agent-deck"));
        let path = home.join(config_name(agent));
        if agent == "claude-code" {
            // Claude's automatic install needs an existing settings file.
            std::fs::write(&path, b"{}").unwrap();
        }

        auto_install_config(agent, &home, &a);
        age_config(&path);
        let first = config_fingerprint(&path);
        for (start, binary) in [&b, &a, &b].into_iter().enumerate() {
            auto_install_config(agent, &home, binary);
            assert_eq!(
                config_fingerprint(&path),
                first,
                "{agent}: automatic start {} (A, B, A, B) rewrote the file",
                start + 2
            );
        }
        let a_command = build_command(&a, &format!("hook --agent {agent}"), HookShell::Posix);
        for (event, positions) in deck_positions(agent, &path) {
            assert_eq!(
                positions,
                vec![(0, 0, a_command.clone())],
                "{agent}/{event}"
            );
        }
    }

    /// Scenario: Alternate automatic Codex installs from A, B, A, B. Only the first writes; the file then keeps its bytes, inode and mtime and names A.
    #[cfg(unix)]
    #[test]
    fn config_alternating_auto_installs_codex_write_once() {
        alternating_auto_installs_write_once("codex");
    }

    /// Scenario: Alternate automatic Claude installs from A, B, A, B. Only the first writes; the file then keeps its bytes, inode and mtime and names A.
    #[cfg(unix)]
    #[test]
    fn config_alternating_auto_installs_claude_write_once() {
        alternating_auto_installs_write_once("claude-code");
    }

    /// Scenario: Alternate automatic Devin installs from A, B, A, B. Only the first writes; the file then keeps its bytes, inode and mtime and names A.
    #[cfg(unix)]
    #[test]
    fn config_alternating_auto_installs_devin_write_once() {
        alternating_auto_installs_write_once("devin");
    }

    // Re-exec rather than mutating process-global HOME while test threads run.
    // Contract assumed for the coder: this explicit test marker arms containment;
    // the root is mandatory and every writer must check it before any mutation.
    #[cfg(unix)]
    fn containment_case(agent: &str, case: &str) -> Vec<String> {
        let fixture = crate::test_temp::tempdir().unwrap();
        let sandbox = fixture.path().join("sandbox");
        let operator = fixture.path().join("fake-operator-home");
        std::fs::create_dir_all(&sandbox).unwrap();
        std::fs::create_dir_all(&operator).unwrap();
        let destination = match case {
            "inside" | "missing-root" => sandbox.join("new-config"),
            "outside" | "malformed" | "uninstall" => operator.join("new-config"),
            "lexical" => sandbox.join("../fake-operator-home/new-config"),
            "symlink" => {
                std::os::unix::fs::symlink(&operator, sandbox.join("escape")).unwrap();
                sandbox.join("escape/new-config")
            }
            _ => unreachable!(),
        };
        let mut snapshots = Vec::new();
        if case == "malformed" || case == "uninstall" {
            std::fs::create_dir_all(&destination).unwrap();
            let path = if agent == "opencode" {
                destination.join("plugin/dot-agent-deck.js")
            } else if agent == "pi" {
                destination.join("dot-agent-deck.ts")
            } else {
                destination.join(config_name(agent))
            };
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let bytes = if case == "malformed" {
                b"{ malformed user config".to_vec()
            } else {
                serde_json::to_vec(&json!({"hooks":{"PreToolUse":[{"hooks":[{"type":"command", "command":format!("/opt/dot-agent-deck hook --agent {agent}")}]}]}})).unwrap()
            };
            std::fs::write(&path, bytes).unwrap();
            snapshots.push((path.clone(), config_fingerprint(&path)));
        }
        let home = sandbox.join("home");
        let installed = home.join(".local/bin/dot-agent-deck");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        crate::test_isolation::write_script(&installed, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(
            &installed,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent_hook_config::tests::config_containment_child",
                "--nocapture",
            ])
            .env("HOME", &home)
            .env("CODEX_HOME", &destination)
            .env("XDG_CONFIG_HOME", &destination)
            .env("PI_CODING_AGENT_DIR", &destination)
            .env("DOT_AGENT_DECK_TEST_CONFIG_WRITE", "1")
            .env(
                "DOT_AGENT_DECK_TEST_CONFIG_ROOT",
                if case == "missing-root" {
                    "".into()
                } else {
                    sandbox.as_os_str().to_owned()
                },
            )
            .env("DAD_CONFIG_TEST_AGENT", agent)
            .env("DAD_CONFIG_TEST_DEST", &destination)
            .env("DAD_CONFIG_TEST_CASE", case)
            .output()
            .unwrap();
        let mut problems = Vec::new();
        if !output.status.success() {
            problems.push(format!(
                "{agent}/{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        for (path, before) in snapshots {
            if !path.exists() || config_fingerprint(&path) != before {
                problems.push(format!(
                    "{agent}/{case}: refused writer changed operator config {}",
                    path.display()
                ));
            }
        }
        if case != "inside" {
            if case == "malformed" || case == "uninstall" {
                let entries: Vec<_> = std::fs::read_dir(&destination)
                    .unwrap()
                    .map(|e| e.unwrap().file_name())
                    .collect();
                if entries.len() != 1 {
                    problems.push(format!(
                        "{agent}/{case}: refused writer created a temp or backup: {entries:?}"
                    ));
                }
            } else {
                if destination.exists() {
                    problems.push(format!("{agent}/{case}: refused writer created its destination before checking containment"));
                }
            }
        } else {
            if !destination.exists() {
                problems.push(format!(
                    "{agent}/{case}: the owned-root control never wrote config"
                ));
            }
        }
        problems
    }

    /// Scenario: Re-execute each config writer with an owned home, then with no root, an outside destination, a symlink escape, and existing malformed or installed files. Refusals happen before directory, temporary-file, backup or deletion side effects.
    #[cfg(unix)]
    #[test]
    fn config_containment_all_agent_writers_refuse_unowned_destinations() {
        let mut problems = Vec::new();
        for agent in ["codex", "claude-code", "devin", "opencode", "pi"] {
            for case in [
                "inside",
                "missing-root",
                "outside",
                "lexical",
                "symlink",
                "malformed",
                "uninstall",
            ] {
                // Pi materializes extensions but has no config-uninstall API.
                if agent == "pi" && case == "uninstall" {
                    continue;
                }
                problems.extend(containment_case(agent, case));
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    /// Scenario: In a child process with explicit sandbox variables, call the selected real agent-config writer and require an owned write or an early refusal. This helper is inert without its parent test's selector.
    #[cfg(unix)]
    #[test]
    fn config_containment_child() {
        let Ok(agent) = std::env::var("DAD_CONFIG_TEST_AGENT") else {
            return;
        };
        let destination = PathBuf::from(std::env::var_os("DAD_CONFIG_TEST_DEST").unwrap());
        let case = std::env::var("DAD_CONFIG_TEST_CASE").unwrap();
        let result = match agent.as_str() {
            "opencode" => {
                // Explicit install's fallback comes from XDG_CONFIG_HOME.
                // Preserve a missing destination so the guard precedes mkdir.
                if case == "uninstall" {
                    crate::opencode_manage::uninstall_from(
                        &destination.join("plugin/dot-agent-deck.js"),
                    )
                } else {
                    crate::opencode_manage::tests::config_test_install(&destination)
                }
            }
            "pi" => crate::orchestrator_ext::materialize(&destination).map(|_| ()),
            _ if case == "uninstall" => uninstall_config(&agent, &destination),
            _ => install_config(&agent, &destination, "/opt/dot-agent-deck"),
        };
        if case == "inside" {
            result.expect("owned config writer control must work");
        } else {
            assert!(
                result.is_err(),
                "{agent}/{case}: writer must refuse before touching an unowned config destination"
            );
        }
    }

    #[test]
    fn build_command_appends_the_agent_suffix_and_quotes_only_when_needed() {
        assert_eq!(
            build_command_for(
                "/abs/dot-agent-deck",
                "hook --agent codex",
                HookShell::Native,
                false
            ),
            "/abs/dot-agent-deck hook --agent codex"
        );
        assert_eq!(
            build_command_for(
                "/with space/dot-agent-deck",
                "hook --agent devin",
                HookShell::Posix,
                false
            ),
            "'/with space/dot-agent-deck' hook --agent devin"
        );
    }

    /// Issue #734. The command written into a Windows Codex user's `hooks.json`
    /// must be one `cmd.exe` can run — Codex hands the whole string to
    /// `%COMSPEC%`/`cmd.exe /C` there. The pre-fix output single-quoted the
    /// path, which `cmd.exe` does not implement as quoting at all: it looked
    /// for a file whose name literally began with `'`, so every deck hook
    /// silently failed.
    ///
    /// Driven through `build_command_for`'s parameter rather than `cfg!`, so
    /// this runs on the Linux box the project is developed on. The one link it
    /// does not cover is `build_command`'s own `cfg!(windows)`, which is a
    /// constant.
    #[test]
    fn build_command_for_a_windows_host_is_runnable_by_cmd_exe() {
        let path = r"C:\Users\somebody\AppData\Local\dot-agent-deck.exe";
        let command = build_command_for(path, "hook --agent codex", HookShell::Native, true);
        assert_eq!(
            command,
            format!(r"{path} hook --agent codex"),
            "an ordinary Windows path is emitted verbatim — the safe set has `\\`"
        );
        assert!(
            !command.starts_with('\''),
            "#734's defect: a single-quoted Windows path is not runnable by cmd.exe; \
             got {command}"
        );

        let spaced = r"C:\Program Files\dot-agent-deck\dot-agent-deck.exe";
        assert_eq!(
            build_command_for(spaced, "hook --agent codex", HookShell::Native, true),
            format!(r#""{spaced}" hook --agent codex"#),
            "a spaced Windows path is double-quoted, the form cmd.exe understands"
        );
    }

    /// The suffix is what both installers use to recognise their own rules
    /// (`command_is_deck_owned` is an `ends_with` on it), so it must survive
    /// the dialect change untouched — that is what makes the repair automatic
    /// for a user who already has a POSIX-quoted rule on disk: the next install
    /// still identifies it, strips it, and writes the runnable spelling.
    #[test]
    fn build_command_ends_with_the_ownership_suffix_in_either_dialect() {
        for windows_host in [true, false] {
            for shell in [HookShell::Native, HookShell::Posix] {
                for path in [
                    "/home/somebody/bin/dot-agent-deck",
                    r"C:\Program Files\deck\dot-agent-deck.exe",
                    "/with space/dot-agent-deck",
                ] {
                    for suffix in ["hook --agent codex", "hook --agent devin"] {
                        let command = build_command_for(path, suffix, shell, windows_host);
                        assert!(
                            command.ends_with(suffix),
                            "quoting must never disturb the ownership suffix; got {command}"
                        );
                    }
                }
            }
        }
    }

    /// The regression `build-windows` caught on PR #782, pinned from Linux.
    ///
    /// Devin's writer must not take its dialect from the host: `install_to` is
    /// reachable without the `devin_config_dir()` gate that confines Devin to
    /// Unix, so a host-derived choice quoted Devin's command for `cmd.exe` on a
    /// Windows runner — contradicting #734's own "byte-identical on every
    /// platform Devin can run on", which is what justified leaving that writer
    /// alone. Asserted as an equality across BOTH hosts rather than against one
    /// spelling, so it states the invariant (the host is not an input) instead
    /// of a snapshot of today's POSIX quoter.
    #[test]
    fn a_posix_writer_ignores_the_host_dialect() {
        for path in [
            "/home/somebody/bin/dot-agent-deck",
            "/Applications/My Deck/dot-agent-deck",
            r"C:\Program Files\deck\dot-agent-deck.exe",
        ] {
            assert_eq!(
                build_command_for(path, "hook --agent devin", HookShell::Posix, true),
                build_command_for(path, "hook --agent devin", HookShell::Posix, false),
                "a POSIX writer's output must not depend on the host; {path} differed"
            );
        }

        assert_eq!(
            build_command_for(
                "/Applications/My Deck/dot-agent-deck",
                "hook --agent devin",
                HookShell::Posix,
                true
            ),
            "'/Applications/My Deck/dot-agent-deck' hook --agent devin",
            "on a Windows host a POSIX writer still single-quotes"
        );

        // And the enum is not inert: the SAME path on the SAME host takes the
        // other dialect for a writer whose interpreter really does follow the
        // host. Without this, a `HookShell` that always returned POSIX would
        // satisfy everything above.
        let spaced = r"C:\Program Files\deck\dot-agent-deck.exe";
        assert_ne!(
            build_command_for(spaced, "hook --agent codex", HookShell::Native, true),
            build_command_for(spaced, "hook --agent codex", HookShell::Posix, true),
            "Native and Posix must differ on a Windows host, or the choice is doing nothing"
        );
    }

    /// Issue #1493's follow-up: only the exact temp-name shape `temp_path`
    /// draws names a pid the reaper may act on.
    #[test]
    fn stale_temp_pid_recognises_only_the_deck_temp_shape() {
        let prefix = ".config.toml.tmp.";
        assert_eq!(
            stale_temp_pid(".config.toml.tmp.4242.0123456789abcdef", prefix),
            Some(4242)
        );
        for other in [
            ".config.toml.tmp.4242.0123456789ABCDEF",
            ".config.toml.tmp.4242.0123456789abcde",
            ".config.toml.tmp.4242.0123456789abcdef0",
            ".config.toml.tmp..0123456789abcdef",
            ".config.toml.tmp.42x.0123456789abcdef",
            ".config.toml.tmp.4242",
            "config.toml.tmp.4242.0123456789abcdef",
            ".hooks.json.tmp.4242.0123456789abcdef",
            "config.toml",
        ] {
            assert_eq!(stale_temp_pid(other, prefix), None, "{other}");
        }
    }

    /// Issue #1493's follow-up: taking the config lock removes a leftover temp
    /// file only when it has the deck's exact shape, its process is gone and it
    /// is over an hour old — and keeps one whose process is running, a fresh
    /// one, a symlink, and anything named otherwise.
    #[cfg(unix)]
    #[test]
    fn the_config_lock_reaps_only_provably_abandoned_temp_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("config.toml");
        std::fs::write(&dest, "").expect("config");
        let gone = {
            let mut child = std::process::Command::new("true").spawn().expect("spawn");
            let pid = child.id();
            child.wait().expect("reap");
            pid
        };
        let live = std::process::id();
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
        let make = |name: &str, old: bool| {
            let path = dir.path().join(name);
            let file = std::fs::File::create(&path).expect("create");
            if old {
                file.set_modified(hour_ago).expect("age it");
            }
            path
        };
        let abandoned = make(&format!(".config.toml.tmp.{gone}.0123456789abcdef"), true);
        let running = make(&format!(".config.toml.tmp.{live}.0123456789abcdef"), true);
        let fresh = make(&format!(".config.toml.tmp.{gone}.fedcba9876543210"), false);
        let foreign = make(&format!(".config.toml.tmp.{gone}.not-ours"), true);
        let link = dir
            .path()
            .join(format!(".config.toml.tmp.{gone}.00000000000000ff"));
        std::os::unix::fs::symlink(&foreign, &link).expect("symlink");

        let _lock = lock_config(&dest).expect("lock");
        assert!(!abandoned.exists(), "an abandoned deck temp file is reaped");
        for kept in [&running, &fresh, &foreign] {
            assert!(kept.exists(), "{} must be kept", kept.display());
        }
        assert!(
            link.symlink_metadata().is_ok(),
            "a symlink is never touched"
        );
    }

    /// Issue #1493's follow-up: no directory means no file to lose an update
    /// from, so nothing is created; a lock name taken by a symlink is refused
    /// rather than locking whatever it points at.
    #[cfg(unix)]
    #[test]
    fn the_config_lock_skips_a_missing_directory_and_refuses_a_symlinked_sidecar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("absent").join("config.toml");
        lock_config(&missing).expect("no directory, no lock");
        assert!(!dir.path().join("absent").exists());

        let dest = dir.path().join("config.toml");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, "").expect("target");
        std::os::unix::fs::symlink(&elsewhere, dir.path().join(".config.toml.lock"))
            .expect("symlink");
        assert!(lock_config(&dest).is_err());
    }

    #[test]
    fn write_atomic_replaces_the_destination_without_truncating_it() {
        let dir = crate::test_temp::tempdir().expect("publish tempdir");
        let dest = dir.path().join("config.json");
        std::fs::write(&dest, b"old").expect("seed destination");

        write_atomic(dir.path(), &dest, b"new").expect("publish");

        assert_eq!(std::fs::read(&dest).expect("read published"), b"new");
        // The temp file is renamed away, never left beside the destination.
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .expect("list dir")
            .map(|e| e.expect("dir entry").file_name())
            .filter(|n| n != "config.json")
            .collect();
        assert!(strays.is_empty(), "temp file left behind: {strays:?}");
    }

    /// The publish must never widen the destination. `File::create` applies
    /// `0666 & !umask`, so without the mode carry-over the rename would replace
    /// a 0600 config with a 0644 (or, under a 002 umask, group-writable 0664)
    /// one the first time the deck installed its hooks.
    #[cfg(unix)]
    #[test]
    fn write_atomic_preserves_the_destination_mode_and_creates_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::test_temp::tempdir().expect("publish tempdir");
        let mode_of = |path: &Path| {
            std::fs::metadata(path)
                .expect("stat published file")
                .permissions()
                .mode()
                & 0o777
        };

        // A file the deck creates itself is owner-only, not umask-dependent.
        let fresh = dir.path().join("fresh.json");
        write_atomic(dir.path(), &fresh, b"{}").expect("publish fresh");
        assert_eq!(mode_of(&fresh), 0o600, "a new config must be owner-only");

        // An existing file keeps exactly the mode the user chose — both a mode
        // narrower than the umask default and one wider than 0600.
        for existing_mode in [0o600, 0o644] {
            let dest = dir.path().join(format!("existing-{existing_mode:o}.json"));
            std::fs::write(&dest, b"{}").expect("seed destination");
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(existing_mode))
                .expect("set destination mode");

            write_atomic(dir.path(), &dest, br#"{"hooks":{}}"#).expect("publish over existing");

            assert_eq!(
                mode_of(&dest),
                existing_mode,
                "publish must reapply the destination's own mode"
            );
        }
    }

    /// The reproduction for issue #731. The temp path used to be
    /// `.<name>.tmp.<pid>` — fully derivable from the destination's file name
    /// and a pid anyone on the box can read — and it was opened with
    /// `File::create`, which follows a symlink. Anyone able to create an entry
    /// in the agent's config directory could therefore pre-plant that name as a
    /// symlink and have the deck truncate, chmod and fill a file of the
    /// attacker's choosing with the deck's bytes, while the destination itself
    /// received nothing.
    #[cfg(unix)]
    #[test]
    fn write_atomic_does_not_follow_a_symlink_planted_at_the_legacy_temp_path() {
        let dir = crate::test_temp::tempdir().expect("publish tempdir");
        let dest = dir.path().join("config.json");
        std::fs::write(&dest, b"old").expect("seed destination");

        // The victim lives outside the config directory, exactly as a real
        // redirection would: the point of the attack is to escape it.
        let victim_dir = crate::test_temp::tempdir().expect("victim tempdir");
        let victim = victim_dir.path().join("victim");
        std::fs::write(&victim, b"victim bytes").expect("seed victim");

        let planted = dir
            .path()
            .join(format!(".config.json.tmp.{}", std::process::id()));
        std::os::unix::fs::symlink(&victim, &planted).expect("plant symlink");

        write_atomic(dir.path(), &dest, b"new").expect("publish");

        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"victim bytes",
            "the publish followed a planted symlink and overwrote the victim"
        );
        assert_eq!(
            std::fs::read(&dest).expect("read published"),
            b"new",
            "the publish must still land the new bytes at the destination"
        );
        assert!(
            !std::fs::symlink_metadata(&dest)
                .expect("stat destination")
                .file_type()
                .is_symlink(),
            "the destination must be a real file, not the renamed symlink"
        );
    }

    /// The other half of #731, driven deterministically: even a temp name an
    /// attacker guessed outright cannot be followed, and a squatted name costs
    /// a retry rather than the whole publish. `create_temp_at` takes a fixed
    /// candidate list here because a randomly drawn name cannot be made to
    /// collide on purpose.
    #[cfg(unix)]
    #[test]
    fn create_temp_at_skips_planted_symlinks_and_lands_on_a_free_name() {
        let dir = crate::test_temp::tempdir().expect("publish tempdir");
        let victim_dir = crate::test_temp::tempdir().expect("victim tempdir");

        // Two squatted candidates: a symlink onto a live file, and a dangling
        // one. `O_EXCL` must refuse both — POSIX fails it on a symlink whether
        // or not the target exists.
        let victim = victim_dir.path().join("victim");
        std::fs::write(&victim, b"victim bytes").expect("seed victim");
        let squatted_live = dir.path().join(".config.json.tmp.live");
        std::os::unix::fs::symlink(&victim, &squatted_live).expect("plant live symlink");

        let dangling_target = victim_dir.path().join("absent");
        let squatted_dangling = dir.path().join(".config.json.tmp.dangling");
        std::os::unix::fs::symlink(&dangling_target, &squatted_dangling)
            .expect("plant dangling symlink");

        let free = dir.path().join(".config.json.tmp.free");
        let candidates = vec![
            squatted_live.clone(),
            squatted_dangling.clone(),
            free.clone(),
        ];

        let (file, landed) = create_temp_at(candidates.into_iter()).expect("create temp");
        drop(file);

        assert_eq!(
            landed, free,
            "must skip both squatters and take the free name"
        );
        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"victim bytes",
            "the exclusive create followed a planted symlink"
        );
        assert!(
            !dangling_target.exists(),
            "the exclusive create created the dangling symlink's target"
        );
        // The squatters are left exactly as they were — never unlinked, so a
        // squatter cannot steer what the deck deletes.
        for squatted in [&squatted_live, &squatted_dangling] {
            assert!(
                std::fs::symlink_metadata(squatted)
                    .expect("stat squatted candidate")
                    .file_type()
                    .is_symlink(),
                "{} must be left untouched",
                squatted.display()
            );
        }
    }

    /// Exhausting every candidate is an error, not a silent write somewhere
    /// else, and it does not disturb what holds the names.
    #[test]
    fn create_temp_at_reports_alreadyexists_when_every_candidate_is_taken() {
        let dir = crate::test_temp::tempdir().expect("publish tempdir");
        let taken: Vec<_> = ["a", "b"]
            .iter()
            .map(|n| {
                let path = dir.path().join(format!(".config.json.tmp.{n}"));
                std::fs::write(&path, b"squatter").expect("seed squatter");
                path
            })
            .collect();

        let err = create_temp_at(taken.clone().into_iter()).expect_err("must refuse");

        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        for path in &taken {
            assert_eq!(
                std::fs::read(path).expect("read squatter"),
                b"squatter",
                "a refused create must not have touched {}",
                path.display()
            );
        }
    }

    /// The name must no longer be derivable from the destination and the pid,
    /// which together were the whole of the old `.<name>.tmp.<pid>`.
    #[test]
    fn temp_path_is_unpredictable_and_never_the_legacy_shape() {
        let dir = Path::new("/agent/config");
        let legacy = dir.join(format!(".config.json.tmp.{}", std::process::id()));

        let draws: std::collections::HashSet<PathBuf> =
            (0..64).map(|_| temp_path(dir, "config.json")).collect();

        assert_eq!(draws.len(), 64, "two draws collided: {draws:?}");
        for drawn in &draws {
            assert_ne!(drawn, &legacy, "the legacy predictable name came back");
            assert_eq!(drawn.parent(), Some(dir), "the temp must stay beside dest");
            let name = drawn
                .file_name()
                .and_then(|n| n.to_str())
                .expect("temp file name");
            assert!(
                name.starts_with(&format!(".config.json.tmp.{}.", std::process::id())),
                "unexpected temp name shape: {name}"
            );
        }
    }

    /// The backup half of #731, on the shared helper.
    ///
    /// `<name>.bak` is as predictable as the old temp name was, and the
    /// `std::fs::write` all three adapters used follows a symlink — so a writer
    /// able to add an entry to the agent's config directory could point that
    /// name at any file it could write and have the deck fill it with the
    /// malformed config's bytes. The link must be neither followed nor, since
    /// #537, replaced: it is something already at the backup name.
    #[cfg(unix)]
    #[test]
    fn backup_malformed_does_not_follow_a_symlink_planted_at_the_backup_path() {
        let dir = crate::test_temp::tempdir().expect("backup tempdir");
        let dest = dir.path().join("config.json");
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"victim bytes").expect("seed victim");

        let planted = dir.path().join("config.json.bak");
        std::os::unix::fs::symlink(&victim, &planted).expect("plant symlink");

        let outcome = backup_malformed(&dest, b"{ not json");

        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"victim bytes",
            "the copy followed the planted symlink and overwrote the victim"
        );
        assert_eq!(outcome, Backup::Occupied(planted.clone()));
        assert!(
            std::fs::symlink_metadata(&planted)
                .expect("stat planted link")
                .file_type()
                .is_symlink(),
            "the planted link must be left as it was found"
        );
    }

    /// The comparison that recognises an earlier copy must not follow a planted
    /// link either: a link to a file that happens to hold the same bytes is
    /// still not a backup the deck made, and must not be named as one.
    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_to_identical_bytes_is_not_taken_for_a_backup() {
        let dir = crate::test_temp::tempdir().expect("backup tempdir");
        let dest = dir.path().join("config.json");
        let lookalike = dir.path().join("lookalike");
        std::fs::write(&lookalike, b"{ not json").expect("seed lookalike");
        let planted = dir.path().join("config.json.bak");
        std::os::unix::fs::symlink(&lookalike, &planted).expect("plant symlink");

        assert_eq!(
            backup_malformed(&dest, b"{ not json"),
            Backup::Occupied(planted)
        );
    }

    /// Issue #537 item 1: a `<name>.bak` that is already there is somebody's
    /// file, and the copy aside must not replace it.
    ///
    /// The realistic owner is the user: a config becomes malformed because
    /// somebody is hand-editing it, and copying it to `settings.json.bak` first
    /// is what a careful hand-editor does. A refusal that then replaced that
    /// file destroyed the one copy of their config that still parsed, while the
    /// original it was protecting was never at risk — every caller leaves it
    /// untouched. The message must not claim the occupant as the deck's backup
    /// either.
    #[test]
    fn backup_malformed_never_replaces_a_file_already_at_the_backup_name() {
        let dir = crate::test_temp::tempdir().expect("backup tempdir");
        let dest = dir.path().join("settings.json");
        std::fs::write(&dest, b"{ \"model\": \"opus\",, }").expect("seed destination");
        let users_own = dir.path().join("settings.json.bak");
        std::fs::write(&users_own, b"{ \"model\": \"opus\" }").expect("seed the user's backup");

        let outcome = backup_malformed(&dest, b"{ \"model\": \"opus\",, }");

        assert_eq!(
            std::fs::read(&users_own).expect("read the user's backup"),
            b"{ \"model\": \"opus\" }",
            "the copy aside replaced a backup the user made themselves"
        );
        assert_eq!(outcome, Backup::Occupied(users_own.clone()));
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("list dir").count(),
            2,
            "the copy that lost to the occupant left its temp behind"
        );
        let phrase = preserved_phrase(&outcome);
        assert!(
            !phrase.starts_with("preserved at"),
            "the message claimed the user's own file as the deck's backup: {phrase}"
        );
    }

    /// A copy that cannot be made at all is reported as such, not as preserved.
    #[test]
    fn backup_malformed_reports_a_copy_it_could_not_make() {
        let dir = crate::test_temp::tempdir().expect("backup tempdir");
        let dest = dir.path().join("missing-dir").join("settings.json");
        assert_eq!(backup_malformed(&dest, b"{ not json"), Backup::Failed);
    }

    /// The plain path, and the control for the test above: with nothing at the
    /// name the bytes land at `<name>.bak`; the same bytes refused again are
    /// recognised as already preserved; different bytes leave the first copy
    /// alone and say so. Nothing accumulates and no temp is left behind.
    ///
    /// Not accumulating matters more than it looks: `hooks_manage::auto_install`
    /// runs on every deck start, so a config that stays malformed reaches this
    /// on every launch. A collision-safe *new* name each time would grow one
    /// file per launch in the user's config directory.
    #[test]
    fn backup_malformed_reuses_its_own_backup_and_never_accumulates() {
        let dir = crate::test_temp::tempdir().expect("backup tempdir");
        let dest = dir.path().join("settings.json");
        let bak = dir.path().join("settings.json.bak");
        std::fs::write(&dest, b"first malformed").expect("seed destination");

        assert_eq!(
            backup_malformed(&dest, b"first malformed"),
            Backup::Preserved(bak.clone())
        );
        assert_eq!(
            backup_malformed(&dest, b"first malformed"),
            Backup::Preserved(bak.clone()),
            "the same bytes refused again are already preserved"
        );
        assert_eq!(
            backup_malformed(&dest, b"second malformed"),
            Backup::Occupied(bak.clone()),
            "different bytes must not replace the first copy"
        );
        assert_eq!(
            std::fs::read(&bak).expect("read backup"),
            b"first malformed"
        );

        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("list dir")
            .map(|e| e.expect("dir entry").file_name())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                std::ffi::OsString::from("settings.json"),
                std::ffi::OsString::from("settings.json.bak")
            ],
            "the copy left a stray temp or a second backup behind"
        );
    }

    /// A planted symlink must not get to CHOOSE the backup's mode either.
    ///
    /// Greptile's P1 on PR #855, and a real second mouth of the same trap: the
    /// publish is safe because `rename` replaces the link rather than following
    /// it, but the mode it lands came from `std::fs::metadata(dest)` — `stat(2)`,
    /// which DOES follow — so a link pointing at a world-readable file published
    /// the user's config bytes 0666. Not following a symlink for the write and
    /// then taking the symlink target's permissions for it is worse than either
    /// half sounds. Since #537 nothing is written at a name a link already
    /// holds, so neither the bytes nor a mode reach what it points at.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_backup_path_cannot_choose_the_backups_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::test_temp::tempdir().expect("backup tempdir");
        let dest = dir.path().join("config.json");

        // The attacker's target, deliberately wide open.
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"victim bytes").expect("seed victim");
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o666))
            .expect("widen victim");
        std::os::unix::fs::symlink(&victim, dir.path().join("config.json.bak"))
            .expect("plant symlink");

        assert_eq!(
            backup_malformed(&dest, b"{ not json"),
            Backup::Occupied(dir.path().join("config.json.bak")),
            "nothing may be published at a name a link already holds"
        );
        assert_eq!(
            std::fs::metadata(&victim)
                .expect("stat victim")
                .permissions()
                .mode()
                & 0o777,
            0o666,
            "the victim's mode must be left as it was"
        );
        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"victim bytes",
            "the victim must still be untouched"
        );
    }

    /// A backup is a byte-for-byte copy of the config, so it must not be readable
    /// by accounts the config was not. `std::fs::write` created it at
    /// `0666 & !umask` — 0644 typically — beside a Devin config that ships 0600
    /// and holds `devin.org_id` (#360, #382).
    #[cfg(unix)]
    #[test]
    fn backup_malformed_creates_an_owner_only_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::test_temp::tempdir().expect("backup tempdir");
        let dest = dir.path().join("config.json");
        let Backup::Preserved(backup) = backup_malformed(&dest, b"{ not json") else {
            panic!("a free backup name must take the copy");
        };

        assert_eq!(
            std::fs::metadata(&backup)
                .expect("stat backup")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "a fresh backup must be owner-only"
        );
    }

    // ---------------------------------------------------------------------
    // The shared ownership predicates (issue #730).
    //
    // These gate a privilege GRANT for Codex (`trust_deck_hooks_in` builds on
    // `command_executable`) and DELETION for all four adapters, so they are
    // tested here directly rather than only through whichever adapter happens
    // to exercise them. A regression then localises to this module instead of
    // surfacing as a puzzling failure in one adapter's suite.
    // ---------------------------------------------------------------------

    const CODEX: &str = "hook --agent codex";
    const DEVIN: &str = "hook --agent devin";
    const CLAUDE: &str = "hook --agent claude";

    /// The suffix is a PARAMETER, and it is the whole of ownership at this
    /// layer. One adapter must not claim another's command — that is what keeps
    /// `hooks uninstall --agent codex` from deleting the Claude adapter's rules
    /// out of a file both happen to write.
    #[test]
    fn command_executable_claims_only_its_own_agents_suffix() {
        assert_eq!(
            command_executable("/abs/dot-agent-deck hook --agent codex", CODEX),
            Some("/abs/dot-agent-deck")
        );
        assert_eq!(
            command_executable("/abs/dot-agent-deck hook --agent codex", CLAUDE),
            None,
            "a Claude suffix must not claim a Codex command"
        );
        assert_eq!(
            command_executable("/abs/dot-agent-deck hook --agent claude", CODEX),
            None,
            "a Codex suffix must not claim a Claude command"
        );
        assert_eq!(
            command_executable("/abs/dot-agent-deck hook --agent devin", DEVIN),
            Some("/abs/dot-agent-deck")
        );
        assert_eq!(
            command_executable("/abs/dot-agent-deck hook --agent devin", CODEX),
            None,
            "`--agent devin` and `--agent codex` are different agents"
        );
    }

    /// A command that is NOTHING BUT the suffix names some program called
    /// `hook` on the agent's own `$PATH`. Neither of this project's two command
    /// builders can produce one: [`build_command`] (the Codex, Devin and
    /// OpenCode installers) and `hooks_manage::make_rule` (Claude's) both
    /// prefix the executable token, and the quoters behind them both quote the
    /// empty string rather than emitting nothing. So claiming it would be
    /// claiming a stranger's command — and at the Codex trust seam, handing it
    /// a grant. (That is the checkable form of what this used to say, "this
    /// project has never written one", which is a claim over the whole history
    /// of the repository and not one anybody can verify.)
    #[test]
    fn command_executable_rejects_a_command_that_is_only_the_suffix() {
        assert_eq!(command_executable("hook --agent codex", CODEX), None);
        assert_eq!(command_executable(" hook --agent codex", CODEX), None);
        assert_eq!(command_executable("hook --agent codex   ", CODEX), None);
    }

    /// Trailing whitespace is trimmed before the suffix test, so a config
    /// hand-edited into `"… hook --agent codex "` is still recognised as the
    /// deck's own on the way back in. Note the deliberate asymmetry with the
    /// Codex TRUST predicate, which compares byte-exactly and does NOT trim:
    /// Codex 0.149.0 echoes a trailing space verbatim and hashes the trimmed
    /// and untrimmed forms differently, so an entry differing only by
    /// whitespace is a genuinely different definition, not a spelling of ours.
    /// Liberal for a mutation, exact for a grant, on purpose (issue #730).
    #[test]
    fn command_executable_trims_trailing_whitespace_but_not_leading() {
        assert_eq!(
            command_executable("/abs/dot-agent-deck hook --agent codex  \t\n", CODEX),
            Some("/abs/dot-agent-deck")
        );
        assert_eq!(
            command_executable("  /abs/dot-agent-deck hook --agent codex", CODEX),
            Some("  /abs/dot-agent-deck"),
            "leading whitespace belongs to the executable token and is left for \
             `unquote_if_needed` and the path comparison to deal with"
        );
        assert_eq!(
            command_executable("/abs/dot-agent-deckhook --agent codex", CODEX),
            None,
            "the space before the suffix is required — no substring match"
        );
    }

    /// Issue #537 item 4.2: both quoting forms are undone on every platform, and
    /// so is the escape inside each. A path holding the quote character is where
    /// the escape matters — the POSIX writer spells `'` as `'\''`, the `cmd.exe`
    /// writer spells `"` as `\"` — and no fixture elsewhere carries one, so
    /// deleting either `replace` passed every other test.
    #[test]
    fn unquote_if_needed_undoes_both_quoting_forms_and_their_escapes() {
        assert_eq!(
            unquote_if_needed("'/opt/My Deck/dot-agent-deck'"),
            "/opt/My Deck/dot-agent-deck"
        );
        assert_eq!(
            unquote_if_needed("\"/opt/My Deck/dot-agent-deck\""),
            "/opt/My Deck/dot-agent-deck"
        );
        assert_eq!(
            unquote_if_needed(r"'/opt/Bob'\''s Deck/dot-agent-deck'"),
            "/opt/Bob's Deck/dot-agent-deck"
        );
        assert_eq!(
            unquote_if_needed(r#""/opt/The \"Deck\"/dot-agent-deck""#),
            r#"/opt/The "Deck"/dot-agent-deck"#
        );
        assert_eq!(
            unquote_if_needed("/opt/deck/dot-agent-deck"),
            "/opt/deck/dot-agent-deck",
            "an unquoted token is returned as it is"
        );
    }

    /// Both JSON shapes an agent config can carry, so `strip_deck_commands`
    /// sees every command a rule actually holds.
    #[test]
    fn rule_commands_reads_the_nested_and_the_legacy_flat_shape() {
        let nested = json!({
            "matcher": "Bash",
            "hooks": [ { "command": "a" }, { "command": "b" }, { "type": "command" } ]
        });
        assert_eq!(rule_commands(&nested).collect::<Vec<_>>(), vec!["a", "b"]);

        let flat = json!({ "command": "c" });
        assert_eq!(rule_commands(&flat).collect::<Vec<_>>(), vec!["c"]);

        let both = json!({ "command": "c", "hooks": [ { "command": "a" } ] });
        assert_eq!(rule_commands(&both).collect::<Vec<_>>(), vec!["a", "c"]);

        assert_eq!(rule_commands(&json!({})).count(), 0);
        assert_eq!(rule_commands(&json!("not an object")).count(), 0);
    }

    /// Issue #535/#730: a rule is a list of commands sharing one matcher, so
    /// removal is per COMMAND. The user's sibling handler survives, and so does
    /// the matcher they wrote it under.
    #[test]
    fn strip_deck_commands_keeps_a_sibling_handler_and_its_matcher() {
        let mut rules = vec![json!({
            "matcher": "Bash",
            "hooks": [
                { "type": "command", "command": "/abs/dot-agent-deck hook --agent codex" },
                { "type": "command", "command": "/usr/local/bin/my-critical-audit.sh" }
            ]
        })];

        let removed =
            strip_deck_commands(&mut rules, |cmd| command_executable(cmd, CODEX).is_some());

        assert_eq!(removed, 1);
        assert_eq!(rules.len(), 1, "the rule object must survive: {rules:?}");
        assert_eq!(rules[0]["matcher"], json!("Bash"));
        assert_eq!(
            rule_commands(&rules[0]).collect::<Vec<_>>(),
            vec!["/usr/local/bin/my-critical-audit.sh"]
        );
    }

    /// A rule is dropped only once NOTHING is left in it, in either shape — and
    /// a rule the predicate matched nothing in is returned untouched, so an
    /// already-empty or command-less rule object is never tidied away as a side
    /// effect of installing.
    #[test]
    fn strip_deck_commands_drops_a_rule_only_when_no_command_is_left() {
        let mut rules = vec![
            json!({ "hooks": [ { "command": "/abs/dot-agent-deck hook --agent codex" } ] }),
            json!({ "matcher": "Bash", "hooks": [] }),
            json!({}),
        ];

        let removed =
            strip_deck_commands(&mut rules, |cmd| command_executable(cmd, CODEX).is_some());

        assert_eq!(removed, 1);
        assert_eq!(
            rules,
            vec![json!({ "matcher": "Bash", "hooks": [] }), json!({})],
            "only the emptied rule goes; untouched rules stay as the user wrote them"
        );
    }

    /// Greptile P1 on PR #1029: the emptiness test used to be "does any COMMAND
    /// remain", which is narrower than "does any HANDLER remain". A handler
    /// object with no string `command` survives the per-command `retain` but was
    /// invisible to that test, so the rule was dropped — deleting the user's
    /// handler and the `matcher` they wrote it under, which is exactly the harm
    /// #535 and #730 exist to prevent, one door along. The helper is shared, so
    /// this covers all four adapters.
    ///
    /// Both shapes a command-less handler can take are here: no `command` key at
    /// all, and a `command` whose value is not a string.
    #[test]
    fn strip_deck_commands_keeps_a_handler_that_carries_no_string_command() {
        let mut rules = vec![json!({
            "matcher": "Bash",
            "hooks": [
                { "type": "command", "command": "/abs/dot-agent-deck hook --agent codex" },
                { "type": "audit", "script": "/usr/local/bin/my-critical-audit.sh" },
                { "type": "command", "command": { "argv": ["/usr/local/bin/other"] } }
            ]
        })];

        let removed =
            strip_deck_commands(&mut rules, |cmd| command_executable(cmd, CODEX).is_some());

        assert_eq!(removed, 1, "only the deck's own command is removed");
        assert_eq!(
            rules.len(),
            1,
            "the rule object must survive on its command-less handlers alone: {rules:?}"
        );
        assert_eq!(
            rules[0]["matcher"],
            json!("Bash"),
            "the user's matcher survives with it"
        );
        assert_eq!(
            rules[0]["hooks"],
            json!([
                { "type": "audit", "script": "/usr/local/bin/my-critical-audit.sh" },
                { "type": "command", "command": { "argv": ["/usr/local/bin/other"] } }
            ]),
            "both command-less handlers survive byte-for-byte"
        );
        assert_eq!(
            rule_commands(&rules[0]).count(),
            0,
            "and none of them is a command — which is why the old \
             `rule_commands(rule).next().is_some()` test dropped this rule"
        );
    }

    /// The legacy FLAT shape: the command IS the rule, so there is nothing
    /// smaller to remove — but the key is taken out and the rule kept whenever
    /// it still carries a command in the other shape, rather than the whole
    /// object being assumed to hold nothing else.
    #[test]
    fn strip_deck_commands_handles_the_legacy_flat_shape() {
        let mut lone = vec![json!({ "command": "/abs/dot-agent-deck hook --agent codex" })];
        assert_eq!(
            strip_deck_commands(&mut lone, |cmd| command_executable(cmd, CODEX).is_some()),
            1
        );
        assert!(
            lone.is_empty(),
            "a flat rule with nothing left goes: {lone:?}"
        );

        let mut mixed = vec![json!({
            "command": "/abs/dot-agent-deck hook --agent codex",
            "hooks": [ { "command": "/usr/local/bin/my-critical-audit.sh" } ]
        })];
        assert_eq!(
            strip_deck_commands(&mut mixed, |cmd| command_executable(cmd, CODEX).is_some()),
            1
        );
        assert_eq!(mixed.len(), 1, "a command survives in the nested shape");
        assert!(mixed[0].get("command").is_none(), "{mixed:?}");
        assert_eq!(
            rule_commands(&mixed[0]).collect::<Vec<_>>(),
            vec!["/usr/local/bin/my-critical-audit.sh"]
        );
    }

    /// Two spellings of one binary are the same binary — the real case being a
    /// `dot-agent-deck` symlink pointing at a renamed build, which must collapse
    /// to one rule rather than accumulate a second every launch.
    #[cfg(unix)]
    #[test]
    fn executables_match_resolves_symlinks_and_falls_back_to_string_equality() {
        let dir = crate::test_temp::tempdir().expect("exe tempdir");
        let real = dir.path().join("worker-agent-deck");
        std::fs::write(&real, b"#!/bin/sh\nexit 0\n").expect("seed binary");
        let link = dir.path().join("dot-agent-deck");
        std::os::unix::fs::symlink(&real, &link).expect("plant symlink");

        assert!(executables_match(
            link.to_str().expect("utf-8"),
            real.to_str().expect("utf-8")
        ));
        assert!(
            executables_match("/nowhere/dot-agent-deck", "/nowhere/dot-agent-deck"),
            "neither path resolves, so the comparison falls back to the strings"
        );
        assert!(!executables_match(
            "/nowhere/dot-agent-deck",
            "/elsewhere/dot-agent-deck"
        ));
    }

    /// Fail-safe branch 1: the INSTALLING path has no basename to compare
    /// against (empty, `..`-terminated, or non-UTF-8). Prune nothing.
    #[test]
    fn pin_is_dead_sibling_prunes_nothing_without_a_basename_to_compare() {
        assert!(!pin_is_dead_sibling("/nowhere/dot-agent-deck", ""));
        assert!(!pin_is_dead_sibling("/nowhere/dot-agent-deck", "/opt/.."));
        assert!(
            !pin_is_dead_sibling("/opt/..", "/abs/dot-agent-deck"),
            "a pin with no basename is not a sibling of anything either"
        );
    }

    /// Fail-safe branch 2 (the basename half): a deck pin naming a
    /// DIFFERENT-looking binary is left alone however absent it is. Most hook
    /// fixtures name fictional paths that were never on disk, and they must not
    /// be swept up just for not existing.
    #[test]
    fn pin_is_dead_sibling_needs_the_installing_binarys_own_basename() {
        assert!(!pin_is_dead_sibling(
            "/nowhere/some-other-tool",
            "/abs/dot-agent-deck"
        ));
    }

    /// Branch 3 — the one of this function's four branches that prunes, here
    /// exercised with a pin the OS positively reports missing, under this
    /// binary's own basename: the pruned-worktree residue PRD #381's repair gate
    /// exists for.
    ///
    /// "The branch that prunes" is about `pin_is_dead_sibling`, not about the
    /// pruned set: [`crate::platform::paths::pin_is_repairable`] behind it
    /// returns `true` on four conditions, and a positively-absent path is only
    /// one of them — a bare or relative pin, a non-executable file and a
    /// `target/{debug,release}` path are equally replaceable, two of them while
    /// naming a file that exists. Those are covered by that function's own tests
    /// in `platform::paths`; this one is about the basename conjunct.
    #[test]
    fn pin_is_dead_sibling_repairs_a_positively_absent_sibling() {
        let dir = crate::test_temp::tempdir().expect("pin tempdir");
        let gone = dir.path().join("pruned").join("dot-agent-deck");
        assert!(!gone.exists(), "the dead path must genuinely not exist");
        assert!(pin_is_dead_sibling(
            gone.to_str().expect("utf-8"),
            "/abs/dot-agent-deck"
        ));
    }

    /// Fail-safe branch 4: a pin this process cannot STAT — permission denied,
    /// an unmounted or stale mount — is well-formed and might well be a working
    /// binary, so it is left alone. Deleting a working user's hook is worse than
    /// leaving a stale rule (PRD #381 Open Question 3).
    #[cfg(unix)]
    #[test]
    fn pin_is_dead_sibling_leaves_a_pin_it_cannot_stat_alone() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::test_temp::tempdir().expect("pin tempdir");
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("create locked dir");
        let hidden = locked.join("dot-agent-deck");
        let hidden = hidden.to_str().expect("utf-8").to_string();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("close the directory");

        let unstatable = Path::new(&hidden).try_exists().is_err();
        let verdict = pin_is_dead_sibling(&hidden, "/abs/dot-agent-deck");

        // Reopen before asserting, so a failure does not also leave the
        // tempdir undeletable.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))
            .expect("reopen the directory");

        if !unstatable {
            println!(
                "SKIP: this user can stat through a 0o000 directory (root?), so the \
                 unstatable-pin branch is unreachable here"
            );
            return;
        }
        assert!(
            !verdict,
            "a pin we cannot stat must be left alone, not repaired away"
        );
    }

    /// The message a user reads must not name a file that was never written.
    /// The `let _ = std::fs::write(…)` this replaced always claimed one, and
    /// until #537 a backup name already in use was claimed as well.
    #[test]
    fn preserved_phrase_names_a_backup_only_when_there_is_one() {
        let path = PathBuf::from("/agent/config/hooks.json.bak");
        assert_eq!(
            preserved_phrase(&Backup::Preserved(path.clone())),
            "preserved at /agent/config/hooks.json.bak"
        );
        let occupied = preserved_phrase(&Backup::Occupied(path));
        assert!(
            !occupied.contains("preserved at"),
            "a name someone else holds must not be claimed as the backup: {occupied}"
        );
        assert!(occupied.contains("already exists"), "{occupied}");
        let failed = preserved_phrase(&Backup::Failed);
        assert!(
            !failed.contains(".bak"),
            "a failed copy must not name a backup path: {failed}"
        );
        assert!(failed.contains("not preserved"), "{failed}");
    }
}
