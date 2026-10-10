//! Upgrading the copies of dot-agent-deck installed on this machine (issue
//! #1635): the CLI/TUI binary and the desktop app, each through the mechanism it
//! was installed with.
//!
//! One module for both clients, so the TUI and the desktop app say the same
//! thing in the same words (CLAUDE.md rule 22). The desktop crate reaches it
//! through its path dependency on this package. The pieces, in the order a
//! client uses them:
//!
//! 1. [`detect`] — how a copy was installed. [`detect::detect`] is a pure
//!    function of [`detect::DetectInputs`]; [`detect::inspect`] gathers the real
//!    inputs through a [`Host`].
//! 2. [`plan`] — what to offer for that install and a newer release, and the
//!    user-facing text for it. Every wording a client shows lives there.
//! 3. [`discover`] — the OTHER copy on the machine (the desktop app from the
//!    CLI, the CLI from the desktop app), which gets its own detection and its
//!    own plan.
//! 4. [`verify`] — the release checksum manifest, build provenance through
//!    `gh attestation verify`, the new binary's `--version`, and the re-hash
//!    right before a staged file is installed.
//! 5. [`execute`] — downloading the release assets and carrying a confirmed
//!    plan out.
//!
//! Nothing here upgrades anything on its own: a plan is something a client
//! shows and the user confirms ([`cli`] is the `dot-agent-deck upgrade`
//! subcommand, which asks on a terminal and takes `--yes` otherwise).
//!
//! Every subprocess and every filesystem question goes through [`Host`], so a
//! test fakes the machine instead of needing one; [`SystemHost`] is the real
//! one. The staged downloads themselves are real files in a private directory
//! [`execute`] creates per upgrade. `docs/develop/self-upgrade.md` describes
//! the design and the trust boundary.

pub mod cli;
pub mod detect;
pub mod discover;
pub mod execute;
pub mod plan;
pub mod verify;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub use detect::{CopyKind, HomebrewFormula, InstallMethod, Installation, Platform, SourceReason};
pub use discover::OtherCopy;
pub use execute::{Outcome, ReleaseSource};
pub use plan::PlanLine;
pub use plan::{
    Found, InstallTarget, Interruption, PlanAction, PlanOptions, Releases, UpgradePlan,
};
pub use verify::{Provenance, ProvenanceCheck};

pub use crate::version::ReleaseChannel;

/// The CLI binary's file name, inside every install and every release asset
/// name.
pub const CLI_BINARY: &str = "dot-agent-deck";

/// How often a running client asks again whether a newer release exists, after
/// the check it makes at start. The desktop app uses it; one value so both
/// clients notice a release equally soon (CLAUDE.md rule 22). Six hours keeps a
/// client that runs for days current while staying far inside GitHub's
/// unauthenticated limit of 60 API requests an hour.
pub const UPDATE_RECHECK_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(6 * 60 * 60);

/// The command a client shows where it cannot carry an upgrade out itself.
pub const UPGRADE_COMMAND: &str = "dot-agent-deck upgrade";

/// The release's checksum manifest for the CLI assets (`task checksums` in
/// `Taskfile.yml`, uploaded by `release.yml`).
pub const CLI_MANIFEST: &str = "checksums.txt";

/// The release's checksum manifest for the desktop assets (`release.yml`'s
/// "Generate desktop checksums" step).
pub const DESKTOP_MANIFEST: &str = "checksums-desktop-alpha.txt";

/// The Debian package the desktop `.deb` installs (Tauri derives it from
/// `productName` in `desktop/src-tauri/tauri.conf.json`; `docs/installation.md`
/// names it). It ships `/usr/bin/dot-agent-deck-desktop` and the CLI at
/// [`DEB_BUNDLED_CLI`].
pub const DESKTOP_DEB_PACKAGE: &str = "agent-deck";

/// The CLI the desktop `.deb` installs beside the app. It upgrades with the
/// package and is not a separate copy.
pub const DEB_BUNDLED_CLI: &str = "/usr/bin/dot-agent-deck";

/// The macOS app bundle's directory name (`productName` plus `.app`).
pub const DESKTOP_APP_BUNDLE: &str = "Agent Deck.app";

/// What a subprocess produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// Whether it exited with status 0.
    pub success: bool,
    /// Its exit code, when it exited rather than being killed.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// How long a probe may take: a question asked of the machine while looking
/// at an install or checking a download (`--version`, `dpkg-query`,
/// `brew --prefix`, `gh auth status`, `codesign -dv`, `hdiutil detach`). Each
/// answers in well under a second when it works; one that hangs (a wrapper
/// script waiting on something) must not hold a release check or a dialog.
pub const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long a check of a download may take when it asks a service: `gh
/// attestation verify` (GitHub's attestation API and Sigstore), and the new
/// app's `codesign --verify --deep` and `spctl --assess`, which can consult
/// Apple's notarization service.
pub const VERIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a command that carries an upgrade out may take: `brew upgrade`,
/// `pkexec … install` and `pkexec … apt-get install` (which wait for the user
/// to answer the password prompt), `hdiutil attach` and `ditto`. Long enough
/// for a slow download inside `brew` or a user who steps away from the
/// prompt, and still an end: a stuck command is stopped and named rather
/// than leaving the dialog on Upgrading for good.
pub const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// The most of each output stream a command's capture keeps: the head is
/// kept and the rest is read and dropped, so a command (or a descendant still
/// holding its pipes) that writes without end cannot grow the capture.
pub const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

/// The machine, as far as upgrading needs to ask about it. [`SystemHost`] is
/// the real one; tests implement it to fake an install.
pub trait Host: Send + Sync {
    /// Run `program` with `args` as a probe, bounded by [`PROBE_TIMEOUT`].
    fn run(&self, program: &Path, args: &[&OsStr]) -> std::io::Result<CommandOutput> {
        self.run_within(program, args, PROBE_TIMEOUT)
    }
    /// Run `program` with `args` and wait for it for at most `timeout`. One
    /// still running then is stopped, and the error says so
    /// ([`std::io::ErrorKind::TimedOut`]).
    fn run_within(
        &self,
        program: &Path,
        args: &[&OsStr],
        timeout: std::time::Duration,
    ) -> std::io::Result<CommandOutput>;
    /// The first executable called `name` on `PATH`.
    fn find_program(&self, name: &str) -> Option<PathBuf>;
    /// Whether `path` is an executable regular file.
    fn is_executable(&self, path: &Path) -> bool;
    /// Whether `path` exists.
    fn exists(&self, path: &Path) -> bool;
    /// `path` with every symlink resolved, when it exists.
    fn canonicalize(&self, path: &Path) -> Option<PathBuf>;
    /// Whether the current user can create files in `dir`.
    fn dir_writable(&self, dir: &Path) -> bool;
    /// The user's home directory.
    fn home(&self) -> Option<PathBuf>;
    /// Whether this is Linux running under WSL.
    fn is_wsl(&self) -> bool;
}

/// [`Host`] for the machine this process runs on.
#[derive(Debug, Clone, Default)]
pub struct SystemHost {
    /// The `PATH` to search, when it is not this process's own (the desktop
    /// app passes its login shell's).
    pub path: Option<std::ffi::OsString>,
}

impl SystemHost {
    /// [`Host::run_within`], also ended early once `cancelled` answers true:
    /// the command is then stopped as one past its bound is, and the error
    /// says it was cancelled ([`std::io::ErrorKind::Interrupted`]) and
    /// whether it was stopped ([`unfinished_stopped`]). `cancelled` is asked
    /// every few milliseconds while the command runs, so it must be cheap.
    /// For a client that forwards its own interruption to the command (the
    /// CLI's Ctrl+C, [`cli`]); nothing here touches a signal disposition.
    pub fn run_within_cancellable(
        &self,
        program: &Path,
        args: &[&OsStr],
        timeout: std::time::Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> std::io::Result<CommandOutput> {
        let mut command = std::process::Command::new(program);
        command.args(args);
        if let Some(path) = &self.path {
            command.env("PATH", path);
        }
        run_bounded(&mut command, timeout, cancelled, &stop_child)
    }
}

impl Host for SystemHost {
    fn run_within(
        &self,
        program: &Path,
        args: &[&OsStr],
        timeout: std::time::Duration,
    ) -> std::io::Result<CommandOutput> {
        let mut command = std::process::Command::new(program);
        command.args(args);
        self.run_within_cancellable(program, args, timeout, &|| false)
    }

    fn find_program(&self, name: &str) -> Option<PathBuf> {
        let path = self.path.clone().or_else(|| std::env::var_os("PATH"))?;
        std::env::split_paths(&path)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(name))
            .find(|candidate| self.is_executable(candidate))
    }

    fn is_executable(&self, path: &Path) -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok()
    }

    fn dir_writable(&self, dir: &Path) -> bool {
        // Creating a file is the only answer that accounts for ACLs, a
        // read-only mount and root-owned directories alike.
        let probe = dir.join(format!(
            ".dot-agent-deck-write-probe-{}",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                true
            }
            Err(_) => false,
        }
    }

    fn home(&self) -> Option<PathBuf> {
        Some(crate::platform::paths::home_dir())
    }

    fn is_wsl(&self) -> bool {
        if !cfg!(target_os = "linux") {
            return false;
        }
        std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
    }
}

/// How long the output of a command that exited is still read, so a
/// descendant that inherited its pipes and keeps them open cannot hold the
/// call until its bound.
const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How long a command signalled to stop is given to exit before it is taken
/// to be one that cannot be stopped.
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How often a reader looks at whether it has been told to stop.
#[cfg(unix)]
const READ_POLL_MS: i32 = 50;

/// Run `command` (stdin closed, stdout and stderr captured) for at most
/// `timeout`, or until `cancelled` answers true.
///
/// On Unix the command runs in a session of its own (`setsid`), so it has no
/// controlling terminal: one that opens `/dev/tty` fails at once with
/// `ENXIO` instead of being stopped by `SIGTTIN` as a background job of the
/// user's terminal, and the terminal's `^C` does not reach it (the CLI
/// forwards its own as cancellation, [`cli`]). The session's id is its
/// process group's, and one still running at the bound, or when cancelled,
/// is stopped by `SIGKILL` to that group alone, so what it started dies with
/// it; a descendant that moved to another group or session escapes it. The
/// error says how long the command was given, or that it was cancelled, and
/// whether it was stopped (`stop` returning whether it exited): a command
/// `pkexec` started runs as root, which this user cannot signal, so it may
/// still be running, and a thread waits for it so it does not stay a zombie.
///
/// The output is read on two threads, so a chatty command cannot fill a pipe
/// and stall. Each keeps at most [`MAX_CAPTURE_BYTES`] of its stream and
/// reads and drops the rest. They stop at the bound, or [`EXIT_GRACE`] after
/// the command exits, even when a descendant still holds the pipes: on Unix
/// they wait for data with `poll` and look at a stop flag between waits, and
/// once they stop the read ends are closed, so a descendant still writing
/// gets `EPIPE`. Elsewhere a reader stops at the end of its stream only.
///
/// Signalling the command's pid and group is safe only while this runner has
/// not reaped it, because until then the pid cannot be reissued. That rests
/// on an assumption: nothing else in the process reaps these children (no
/// `waitpid(-1)` elsewhere, no inherited `SIGCHLD=SIG_IGN`, under which the
/// kernel reaps them itself). Where a wait fails anyway, ownership is taken
/// to be lost: no signal is sent, and the error says the command may still
/// be running ([`unfinished_stopped`] reads `Some(false)`).
fn run_bounded(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
    cancelled: &dyn Fn() -> bool,
    stop: &dyn Fn(&mut std::process::Child) -> bool,
) -> std::io::Result<CommandOutput> {
    use std::process::Stdio;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure runs in the forked child before `exec`, and
        // calls only setsid(2), which is async-signal-safe and allocates
        // nothing. A forked child is never a process group leader, so it
        // cannot fail with EPERM; any failure aborts the spawn.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stop_reading = Arc::new(AtomicBool::new(false));
    let readers = [
        drain(child.stdout.take(), stop_reading.clone()),
        drain(child.stderr.take(), stop_reading.clone()),
    ];
    let finish = |readers: [std::thread::JoinHandle<Vec<u8>>; 2]| {
        stop_reading.store(true, Ordering::SeqCst);
        readers.map(|reader| reader.join().unwrap_or_default())
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        let now = Instant::now();
        let exited = match child.try_wait() {
            Ok(exited) => exited,
            Err(e) => {
                // Not ours to signal any more: see the ownership note above.
                finish(readers);
                return Err(ownership_lost(&e));
            }
        };
        if let Some(status) = exited {
            break status;
        }
        let was_cancelled = cancelled();
        if was_cancelled || now >= deadline {
            let stopped = stop(&mut child);
            if stopped {
                let _ = child.wait();
            } else {
                reap_later(child);
            }
            finish(readers);
            return Err(if was_cancelled {
                cancelled_error(stopped)
            } else {
                timed_out(timeout, stopped)
            });
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
    };
    let until = deadline.min(Instant::now() + EXIT_GRACE);
    while !readers.iter().all(std::thread::JoinHandle::is_finished) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    let [stdout, stderr] = finish(readers);
    Ok(CommandOutput {
        success: status.success(),
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// A child's output pipe, as [`drain`] reads it.
#[cfg(unix)]
trait Pipe: std::io::Read + std::os::fd::AsRawFd + Send + 'static {}
#[cfg(unix)]
impl<T: std::io::Read + std::os::fd::AsRawFd + Send + 'static> Pipe for T {}
#[cfg(not(unix))]
trait Pipe: std::io::Read + Send + 'static {}
#[cfg(not(unix))]
impl<T: std::io::Read + Send + 'static> Pipe for T {}

/// Whether `pipe` has something to read (its end included), waiting at most
/// [`READ_POLL_MS`].
#[cfg(unix)]
fn readable(pipe: &impl Pipe) -> std::io::Result<bool> {
    let mut fd = libc::pollfd {
        fd: pipe.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid `pollfd` for a descriptor `pipe` owns and keeps open
    // for the duration of the call.
    match unsafe { libc::poll(&mut fd, 1, READ_POLL_MS) } {
        -1 => {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error)
            }
        }
        0 => Ok(false),
        _ => Ok(true),
    }
}

#[cfg(not(unix))]
fn readable(_pipe: &impl Pipe) -> std::io::Result<bool> {
    Ok(true)
}

/// Read `pipe` on a thread of its own until its end or until `stop` is set,
/// keeping the first [`MAX_CAPTURE_BYTES`]. The pipe is closed when the
/// thread ends.
fn drain(
    pipe: Option<impl Pipe>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let Some(mut pipe) = pipe else {
            return kept;
        };
        let mut buffer = [0u8; 8192];
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            match readable(&pipe) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break,
            }
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let room = MAX_CAPTURE_BYTES - kept.len();
                    kept.extend_from_slice(&buffer[..n.min(room)]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        kept
    })
}

/// Stop a command that outlived its bound: `SIGKILL` its process group (on
/// Unix) and the command itself, then wait up to [`STOP_GRACE`] for it to
/// exit. Returns whether it did, which a command running as root does not.
fn stop_child(child: &mut std::process::Child) -> bool {
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: kill(2) with a negative pid signals the process group
        // `run_bounded` gave the command, whose id is the command's pid. The
        // command is not reaped yet (`run_bounded` calls this only before
        // its own first successful wait, and nothing else reaps it — the
        // assumption it documents), so that id cannot have been reused for
        // somebody else's group. A failure (EPERM for a group of root's,
        // ESRCH for one already gone) leaves the exit check below to decide.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let deadline = Instant::now() + STOP_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) | Err(_) => return false,
        }
    }
}

/// Wait for `child`, which could not be stopped, on a thread of its own, so
/// it is reaped whenever it exits instead of staying a zombie.
fn reap_later(mut child: std::process::Child) {
    let _ = std::thread::Builder::new()
        .name("self-upgrade-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

/// A command that did not run to its end under the runner's watch, as the
/// error [`Host::run_within`] returns: it outlived its bound
/// ([`std::io::ErrorKind::TimedOut`]), it was cancelled
/// ([`std::io::ErrorKind::Interrupted`]), or its exit could not be read
/// ([`ownership_lost`]). The message is for the user; `stopped` is whether
/// the command is known to have stopped, which [`unfinished_stopped`] reads
/// back, and `why` which of the three it was.
#[derive(Debug)]
struct Unfinished {
    why: plan::Interruption,
    stopped: bool,
    message: String,
}

impl std::fmt::Display for Unfinished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Unfinished {}

/// A command that outlived `timeout`, in words for the user. `stopped` is
/// whether it was stopped; one that was not may still be running.
pub fn timed_out(timeout: std::time::Duration, stopped: bool) -> std::io::Error {
    let within = describe_duration(timeout);
    let message = if stopped {
        format!("it did not finish within {within} and was stopped")
    } else {
        format!(
            "it did not finish within {within} and could not be stopped, so it may still be running"
        )
    };
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        Unfinished {
            why: plan::Interruption::TimedOut,
            stopped,
            message,
        },
    )
}

/// A command ended early because the client cancelled it, in words for the
/// user. `stopped` is whether it was stopped; one that was not may still be
/// running.
pub fn cancelled_error(stopped: bool) -> std::io::Error {
    let message = if stopped {
        "it was cancelled and stopped".to_string()
    } else {
        "it was cancelled and could not be stopped, so it may still be running".to_string()
    };
    std::io::Error::new(
        std::io::ErrorKind::Interrupted,
        Unfinished {
            why: plan::Interruption::Cancelled,
            stopped,
            message,
        },
    )
}

/// A command whose wait failed with `error`: it may have been reaped by
/// something else, so its pid is no longer known to be its own and it is not
/// signalled. Whether it finished is not known.
fn ownership_lost(error: &std::io::Error) -> std::io::Error {
    std::io::Error::other(Unfinished {
        why: plan::Interruption::ExitUnread,
        stopped: false,
        message: format!("its exit could not be read ({error}), so it may still be running"),
    })
}

/// For an error [`timed_out`] or [`cancelled_error`] made, or one from a
/// command whose exit could not be read, whether the command is known to
/// have stopped; `None` for any other error.
pub fn unfinished_stopped(error: &std::io::Error) -> Option<bool> {
    unfinished(error).map(|unfinished| unfinished.stopped)
}

fn unfinished(error: &std::io::Error) -> Option<&Unfinished> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Unfinished>())
}

/// `duration` in the largest whole unit that fits: "15 minutes", "60
/// seconds", "200 milliseconds".
fn describe_duration(duration: std::time::Duration) -> String {
    let plural = |n: u128, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    let ms = duration.as_millis();
    if ms.is_multiple_of(60_000) && ms > 0 {
        plural(ms / 60_000, "minute")
    } else if ms.is_multiple_of(1000) && ms > 0 {
        plural(ms / 1000, "second")
    } else {
        plural(ms, "millisecond")
    }
}

/// Why an upgrade did not happen. Every message is written for the user.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UpgradeError {
    #[error("Cannot find the running dot-agent-deck executable.")]
    NoExecutable,
    #[error("Cannot check for a newer release: {0}")]
    ReleaseLookup(String),
    #[error("Cannot download {url}: {detail}")]
    Download { url: String, detail: String },
    #[error("{manifest} has no entry for {asset}, so it cannot be checked. Nothing was changed.")]
    ChecksumMissing { asset: String, manifest: String },
    #[error(
        "{manifest} lists {asset} more than once with different checksums, so it cannot be checked. Nothing was changed."
    )]
    ChecksumAmbiguous { asset: String, manifest: String },
    #[error(
        "{asset} does not match {manifest} (expected {expected}, got {actual}). Nothing was changed."
    )]
    ChecksumMismatch {
        asset: String,
        manifest: String,
        expected: String,
        actual: String,
    },
    #[error(
        "The build provenance of {manifest} could not be verified, so nothing was changed. `gh attestation verify` said: {detail}"
    )]
    ProvenanceFailed { manifest: String, detail: String },
    #[error(
        "The upgrade plan said build provenance would be checked, but it cannot be now: {reason}. Nothing was changed."
    )]
    ProvenanceUnavailable { reason: String },
    #[error(
        "{path} changed after it was checked (expected {expected}, got {actual}), so it was not installed. Nothing was changed."
    )]
    StagedChanged {
        path: String,
        expected: String,
        actual: String,
    },
    #[error(
        "WARNING: {target} was installed, but it is NOT the verified build (expected {expected}, got {actual}). It was not run. Do not use it: reinstall dot-agent-deck from {release}."
    )]
    InstalledMismatch {
        target: String,
        expected: String,
        actual: String,
        release: String,
    },
    #[error(
        "The download folder {root} is not safe to use: {why}. Nothing was changed. Remove it or make it private to you, then try again."
    )]
    StagingUnsafe { root: String, why: String },
    /// The privilege prompt (`pkexec`) did not install a file that was
    /// downloaded and checked. `install` is the command that installs the
    /// staged file instead, when one can be shown safely.
    #[error("`{command}` failed: {detail}")]
    PrivilegeFailed {
        command: String,
        detail: String,
        install: Option<String>,
        version: String,
    },
    /// A privileged install started and did not complete
    /// ([`UnfinishedInstall`]).
    #[error("`{}` failed: {}", .0.command, .0.detail)]
    InstallUnfinished(Box<UnfinishedInstall>),
    #[error(
        "The downloaded binary reports {actual} instead of dot-agent-deck {expected}. Nothing was changed."
    )]
    VersionMismatch { expected: String, actual: String },
    #[error("The new Agent Deck app failed a check, so nothing was changed: {0}")]
    AppCheckFailed(String),
    #[error("`{command}` failed: {detail}")]
    CommandFailed { command: String, detail: String },
    #[error("{0}")]
    Io(String),
    #[error("This install is not upgraded from here: {0}")]
    NotActionable(String),
    /// `error` stopped the app swap, and the release's disk image could not
    /// be detached afterwards: it is still attached at `mount`.
    #[error("{error}")]
    StillMounted {
        error: Box<UpgradeError>,
        mount: PathBuf,
    },
}

/// A privileged install that started and did not complete: it failed once
/// past the prompt, or did not finish within its bound. Whether the new
/// version is installed is not known from that alone, so `found` is what was
/// found at `target` afterwards — not looked at while the install
/// `may_still_be_running` (`Some`, saying why it was not seen to finish). `install` is the command that installs the
/// verified file again, offered only when `found` shows the target is not the
/// new version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnfinishedInstall {
    pub command: String,
    pub detail: String,
    pub may_still_be_running: Option<plan::Interruption>,
    pub target: plan::InstallTarget,
    pub found: plan::Found,
    pub install: Option<String>,
    pub version: String,
}

impl UpgradeError {
    /// What the user can do instead, shown after the error itself: for a
    /// failed privilege prompt, the command that installs the file that was
    /// already downloaded and checked, or how to upgrade manually when that
    /// command cannot be shown safely; for an install that did not complete,
    /// what was found and how to check it.
    pub fn fallback(&self) -> Vec<PlanLine> {
        match self {
            Self::StillMounted { error, mount } => {
                let mut lines = error.fallback();
                lines.extend(execute::still_mounted(mount));
                lines
            }
            Self::PrivilegeFailed {
                install: Some(command),
                ..
            } => vec![
                PlanLine::Text(plan::PROMPT_FAILED.to_string()),
                PlanLine::Command(command.clone()),
            ],
            Self::PrivilegeFailed {
                install: None,
                version,
                ..
            } => vec![PlanLine::Text(plan::manual_upgrade_line(version))],
            Self::InstallUnfinished(unfinished) => plan::install_unfinished_lines(
                &unfinished.target,
                &unfinished.found,
                unfinished.may_still_be_running,
                unfinished.install.as_deref(),
                &unfinished.version,
            ),
            _ => Vec::new(),
        }
    }
}

impl From<std::io::Error> for UpgradeError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// Run `program args` through `host` for at most `timeout`, mapping a
/// failure, a timeout included, to [`UpgradeError::CommandFailed`] named
/// after the command line.
pub(crate) fn run_checked(
    host: &dyn Host,
    program: &Path,
    args: &[&OsStr],
    timeout: std::time::Duration,
) -> Result<CommandOutput, UpgradeError> {
    let command = display_command(program, args);
    let output =
        host.run_within(program, args, timeout)
            .map_err(|e| UpgradeError::CommandFailed {
                command: command.clone(),
                detail: e.to_string(),
            })?;
    if output.success {
        Ok(output)
    } else {
        let detail = match output.stderr.trim() {
            "" => match output.code {
                Some(code) => format!("exit {code}"),
                None => "killed by a signal".to_string(),
            },
            stderr => stderr.to_string(),
        };
        Err(UpgradeError::CommandFailed { command, detail })
    }
}

/// A command line as a user would type it into a POSIX shell.
pub(crate) fn display_command(program: &Path, args: &[&OsStr]) -> String {
    std::iter::once(program.as_os_str())
        .chain(args.iter().copied())
        .map(|word| shell_word(&word.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `word` quoted for a POSIX shell when it needs to be.
pub(crate) fn shell_word(word: &str) -> String {
    crate::platform::paths::shell_quote_if_needed(word)
}

/// The version `binary --version` reports, without a leading `v`, or `None`
/// when it does not answer as dot-agent-deck.
pub fn reported_version(host: &dyn Host, binary: &Path) -> Option<String> {
    let output = host.run(binary, &[OsStr::new("--version")]).ok()?;
    if !output.success {
        return None;
    }
    let version = crate::version::parse_version_output(&output.stdout)?;
    Some(version.strip_prefix('v').unwrap_or(&version).to_string())
}

/// The release channel `installation` follows: `Prerelease` for the
/// `dot-agent-deck-beta` Homebrew formula, which only ever receives
/// prereleases, or for any copy whose own version is a prerelease; `Stable`
/// otherwise. Each copy on the machine is planned on its own channel
/// ([`plan::plan`]), so a stable copy beside a prerelease one is never offered
/// a prerelease, and a copy on the beta formula is offered only a prerelease
/// ([`plan::Releases`]).
pub fn release_channel(installation: &Installation) -> ReleaseChannel {
    match installation.method {
        InstallMethod::Homebrew {
            formula: HomebrewFormula::Beta,
            ..
        } => ReleaseChannel::Prerelease,
        _ => ReleaseChannel::of_version(&installation.version),
    }
}

/// Whether `latest` is a newer release than `current` (both with or without a
/// leading `v`). An unparseable version is never newer.
pub fn is_newer(current: &str, latest: &str) -> bool {
    let Ok(current) = semver::Version::parse(current.strip_prefix('v').unwrap_or(current)) else {
        return false;
    };
    crate::version::should_notify(&current, latest).is_some()
}

// Real subprocesses through `/bin/sh`: native Windows is unsupported (#164).
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const SH: &str = "/bin/sh";

    #[test]
    fn host_001_a_command_past_its_bound_is_stopped_and_named() {
        let started = Instant::now();
        let err = SystemHost::default()
            .run_within(
                Path::new(SH),
                &[OsStr::new("-c"), OsStr::new("sleep 30")],
                Duration::from_millis(200),
            )
            .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(
            err.to_string(),
            "it did not finish within 200 milliseconds and was stopped"
        );

        // As an upgrade step, the error names the command that timed out.
        let err = run_checked(
            &SystemHost::default(),
            Path::new(SH),
            &[OsStr::new("-c"), OsStr::new("sleep 30")],
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "`/bin/sh -c 'sleep 30'` failed: it did not finish within 200 milliseconds and was stopped"
        );
    }

    #[test]
    fn host_002_a_command_inside_its_bound_answers_in_full() {
        let output = SystemHost::default()
            .run_within(
                Path::new(SH),
                &[
                    OsStr::new("-c"),
                    OsStr::new("echo out; echo err >&2; exit 3"),
                ],
                Duration::from_secs(10),
            )
            .unwrap();
        assert_eq!(
            output,
            CommandOutput {
                success: false,
                code: Some(3),
                stdout: "out\n".into(),
                stderr: "err\n".into(),
            }
        );
    }

    #[test]
    fn host_003_the_bounds_are_ordered_and_described() {
        assert!(PROBE_TIMEOUT < VERIFY_TIMEOUT && VERIFY_TIMEOUT < INSTALL_TIMEOUT);
        assert_eq!(describe_duration(INSTALL_TIMEOUT), "15 minutes");
        assert_eq!(describe_duration(VERIFY_TIMEOUT), "1 minute");
        assert_eq!(describe_duration(PROBE_TIMEOUT), "15 seconds");
        assert_eq!(
            timed_out(INSTALL_TIMEOUT, false).to_string(),
            "it did not finish within 15 minutes and could not be stopped, so it may still be running"
        );
    }

    /// Whether `pid` has been reaped: `kill(pid, 0)` answers `ESRCH` only
    /// once nothing holds the pid, a zombie included.
    fn reaped(pid: i32) -> bool {
        // SAFETY: signal 0 checks only that the pid exists; nothing is sent.
        let rc = unsafe { libc::kill(pid, 0) };
        rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    /// Whether `pid` has exited: reaped, or a zombie waiting for its parent.
    fn exited(pid: i32) -> bool {
        reaped(pid)
            || std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    let (_, rest) = stat.rsplit_once(')')?;
                    rest.split_whitespace().next().map(|state| state == "Z")
                })
                .unwrap_or(false)
    }

    fn eventually(what: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if what() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        what()
    }

    fn pid_in(file: &Path) -> i32 {
        assert!(
            eventually(|| std::fs::read_to_string(file).is_ok_and(|s| s.ends_with('\n'))),
            "{} was never written",
            file.display()
        );
        std::fs::read_to_string(file)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    /// Scenario: a command starts a descendant that keeps writing to the
    /// inherited stdout without end, then exits. The call returns soon after
    /// the command exits, well inside its bound, with the head of the output
    /// capped at `MAX_CAPTURE_BYTES`, and the descendant, whose pipe is
    /// closed, dies of it.
    #[test]
    fn host_004_a_descendant_holding_the_pipes_neither_holds_the_call_nor_grows_the_capture() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("writer.pid");
        let script = format!(
            "yes aaaaaaaaaaaaaaa & echo $! > '{}'; sleep 1; exit 0",
            pid_file.display()
        );
        let started = Instant::now();
        let output = SystemHost::default()
            .run_within(
                Path::new(SH),
                &[OsStr::new("-c"), OsStr::new(&script)],
                Duration::from_secs(30),
            )
            .unwrap();
        let writer = pid_in(&pid_file);
        let elapsed = started.elapsed();
        let writer_died = eventually(|| exited(writer));
        if !writer_died {
            // SAFETY: the pid was just read from the writer itself.
            unsafe { libc::kill(writer, libc::SIGKILL) };
        }
        assert!(elapsed < Duration::from_secs(15), "{elapsed:?}");
        assert!(output.success);
        assert_eq!(output.stdout.len(), MAX_CAPTURE_BYTES);
        assert!(output.stdout.starts_with("aaaaaaaaaaaaaaa\n"));
        assert!(writer_died, "the descendant kept writing to a closed pipe");
    }

    /// Scenario: a command that starts a descendant and then hangs is run
    /// past its bound. The whole process group is stopped, so the descendant
    /// dies with the command rather than running on.
    #[test]
    fn host_005_a_timed_out_command_is_stopped_with_its_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("descendant.pid");
        let script = format!("sleep 60 & echo $! > '{}'; sleep 60", pid_file.display());
        let err = SystemHost::default()
            .run_within(
                Path::new(SH),
                &[OsStr::new("-c"), OsStr::new(&script)],
                Duration::from_millis(500),
            )
            .unwrap_err();
        let descendant = pid_in(&pid_file);
        let died = eventually(|| exited(descendant));
        if !died {
            // SAFETY: the pid was just read from the descendant itself.
            unsafe { libc::kill(descendant, libc::SIGKILL) };
        }
        assert_eq!(
            err.to_string(),
            "it did not finish within 500 milliseconds and was stopped"
        );
        assert!(died, "the descendant outlived its timed-out command");
    }

    /// Scenario: a command outlives its bound and cannot be stopped, as a
    /// command `pkexec` runs as root cannot be. The kill is faked to be
    /// refused (a real root child is impractical in a test); the error says
    /// it may still be running, and once it exits on its own it is reaped
    /// rather than left a zombie.
    #[test]
    fn host_006_a_child_whose_kill_is_refused_is_reaped_later() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        let script = format!("echo $$ > '{}'; sleep 1", pid_file.display());
        let mut command = std::process::Command::new(SH);
        command.args(["-c", &script]);
        let refuse = |_: &mut std::process::Child| false;
        let err =
            run_bounded(&mut command, Duration::from_millis(300), &|| false, &refuse).unwrap_err();
        assert_eq!(
            err.to_string(),
            "it did not finish within 300 milliseconds and could not be stopped, so it may still be running"
        );
        let child = pid_in(&pid_file);
        assert!(!reaped(child), "the child was not yet due to exit");
        assert!(
            eventually(|| reaped(child)),
            "the child that could not be stopped was never reaped"
        );
    }

    /// Marks a process as the re-exec'd half of a test: the test runs its
    /// child body instead of re-executing itself again.
    const REEXEC_CHILD: &str = "DOT_AGENT_DECK_SELF_UPGRADE_REEXEC_CHILD";

    /// Run the test at `path` (as libtest names it) again in a process of its
    /// own, marked with [`REEXEC_CHILD`], and return its output once it
    /// exits. For a test that changes something process-wide, or needs a
    /// process with a controlling terminal of its own.
    fn reexec(path: &str) -> std::process::Output {
        let exe = std::env::current_exe().expect("current_exe: this is a test binary");
        let output = std::process::Command::new(exe)
            .args(["--exact", path, "--nocapture", "--test-threads=1"])
            .env(REEXEC_CHILD, "1")
            .output()
            .expect("re-exec this test binary");
        assert_child_passed(
            &output.status,
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        );
        output
    }

    /// That the re-exec'd half exited 0 having run exactly one test: a filter
    /// that matched nothing exits 0 too.
    fn assert_child_passed(status: &std::process::ExitStatus, stdout: &str, stderr: &str) {
        assert!(
            status.success() && stdout.contains("1 passed"),
            "the re-exec'd test failed or did not run\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );
    }

    fn is_reexec_child() -> bool {
        std::env::var_os(REEXEC_CHILD).is_some()
    }

    /// Run the test at `path` again as [`reexec`] does, but as the leader of
    /// a session of its own whose controlling terminal is a fresh PTY, so it
    /// stands where the CLI stands on a user's terminal: the terminal's
    /// foreground process group.
    fn reexec_on_a_terminal(path: &str) {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        use std::io::Read;

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open a PTY");
        let exe = std::env::current_exe().expect("current_exe: this is a test binary");
        let mut command = CommandBuilder::new(exe);
        command.args(["--exact", path, "--nocapture", "--test-threads=1"]);
        command.env(REEXEC_CHILD, "1");
        let mut child = pair
            .slave
            .spawn_command(command)
            .expect("re-exec this test binary on the PTY");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("PTY reader");
        let output = std::thread::spawn(move || {
            let mut output = Vec::new();
            let mut buffer = [0u8; 4096];
            // The read fails (EIO) once the child's side is closed.
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                output.extend_from_slice(&buffer[..n]);
            }
            String::from_utf8_lossy(&output).into_owned()
        });
        let status = child.wait().expect("wait for the re-exec'd test");
        drop(pair.master);
        let output = output.join().unwrap_or_default();
        assert!(
            status.success() && output.contains("1 passed"),
            "the re-exec'd test failed or did not run\n--- terminal ---\n{output}"
        );
    }

    /// Scenario: run from a process whose controlling terminal is a PTY, as
    /// the CLI runs on a user's terminal, a command opens `/dev/tty` and
    /// reads from it. It has no controlling terminal, so the open fails at
    /// once and the command exits with an error, well inside its 10 s bound,
    /// rather than being stopped by `SIGTTIN` as a background job and looking
    /// hung until the bound.
    #[test]
    fn host_008_a_command_that_opens_the_terminal_fails_at_once() {
        if !is_reexec_child() {
            reexec_on_a_terminal(
                "self_upgrade::tests::host_008_a_command_that_opens_the_terminal_fails_at_once",
            );
            return;
        }
        assert!(
            std::fs::File::open("/dev/tty").is_ok(),
            "the re-exec'd half must have a controlling terminal, or this proves nothing"
        );
        let started = Instant::now();
        let result = SystemHost::default().run_within(
            Path::new(SH),
            &[OsStr::new("-c"), OsStr::new("read line < /dev/tty")],
            Duration::from_secs(10),
        );
        let elapsed = started.elapsed();
        let output = result.expect("the command ends on its own, inside its bound");
        assert!(!output.success, "{output:?}");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    }

    /// Scenario: a command that started a descendant and then waits on it is
    /// run through the cancellable path with a 60 s bound, and the client
    /// cancels it once both are running. The call returns "cancelled" within
    /// `STOP_GRACE` of the cancellation, and the command, its descendant and
    /// its whole process group are gone.
    #[test]
    fn host_009_a_cancelled_command_is_stopped_with_its_group() {
        let dir = tempfile::tempdir().unwrap();
        let command_pid = dir.path().join("command.pid");
        let descendant_pid = dir.path().join("descendant.pid");
        let script = format!(
            "sleep 60 & echo $! > '{}'; echo $$ > '{}'; wait",
            descendant_pid.display(),
            command_pid.display()
        );
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_at = std::sync::Arc::new(std::sync::Mutex::new(None));
        let canceller = {
            let (cancel, fired_at) = (cancel.clone(), fired_at.clone());
            let (command_pid, descendant_pid) = (command_pid.clone(), descendant_pid.clone());
            std::thread::spawn(move || {
                let (command, descendant) = (pid_in(&command_pid), pid_in(&descendant_pid));
                *fired_at.lock().unwrap() = Some(Instant::now());
                cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                (command, descendant)
            })
        };
        let flag = cancel.clone();
        let result = SystemHost::default().run_within_cancellable(
            Path::new(SH),
            &[OsStr::new("-c"), OsStr::new(&script)],
            Duration::from_secs(60),
            &move || flag.load(std::sync::atomic::Ordering::SeqCst),
        );
        let returned_at = Instant::now();
        let (command, descendant) = canceller.join().unwrap();
        let descendant_died = eventually(|| exited(descendant));
        if !descendant_died {
            // SAFETY: the pid was just read from the descendant itself.
            unsafe { libc::kill(descendant, libc::SIGKILL) };
        }
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Interrupted, "{err:?}");
        assert_eq!(err.to_string(), "it was cancelled and stopped");
        assert_eq!(unfinished_stopped(&err), Some(true));
        let fired_at = fired_at.lock().unwrap().expect("the cancellation fired");
        assert!(
            returned_at.duration_since(fired_at) < STOP_GRACE,
            "{:?}",
            returned_at.duration_since(fired_at)
        );
        assert!(reaped(command), "the cancelled command was not reaped");
        assert!(
            descendant_died,
            "the descendant outlived its cancelled command"
        );
        // SAFETY: signal 0 to the command's group checks only that the group
        // exists; nothing is sent.
        let group_left = unsafe { libc::kill(-command, 0) } == 0;
        assert!(
            !group_left,
            "something in the command's group is still running"
        );
    }

    /// Scenario: something else in the process reaps the command first, as
    /// the kernel does for every child under an inherited `SIGCHLD=SIG_IGN`,
    /// so the runner's own wait fails. The call returns an error and sends no
    /// signal: the pid may already be somebody else's.
    #[test]
    fn host_007_a_child_reaped_elsewhere_is_never_signalled() {
        if !is_reexec_child() {
            reexec("self_upgrade::tests::host_007_a_child_reaped_elsewhere_is_never_signalled");
            return;
        }
        // SAFETY: this process is the re-exec'd half running this one test
        // on one thread, so the disposition changes nothing else.
        unsafe {
            libc::signal(libc::SIGCHLD, libc::SIG_IGN);
        }
        let signalled = std::sync::atomic::AtomicBool::new(false);
        let stop = |_: &mut std::process::Child| {
            signalled.store(true, std::sync::atomic::Ordering::SeqCst);
            true
        };
        let mut command = std::process::Command::new(SH);
        command.args(["-c", "exit 0"]);
        let err = run_bounded(&mut command, Duration::from_secs(10), &|| false, &stop).unwrap_err();
        assert!(
            !signalled.load(std::sync::atomic::Ordering::SeqCst),
            "a child the runner did not reap itself was signalled: {err}"
        );
        assert_eq!(unfinished_stopped(&err), Some(false), "{err:?}");
        assert!(
            err.to_string().ends_with("so it may still be running"),
            "{err}"
        );
    }
}

#[cfg(test)]
pub(crate) mod test_host {
    //! A fake [`Host`]: a set of files, a table of command answers, and a log
    //! of what ran.

    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    type Handler = Box<dyn Fn(&[String]) -> std::io::Result<CommandOutput> + Send + Sync>;

    #[derive(Default)]
    pub struct FakeHost {
        pub executables: HashSet<PathBuf>,
        pub files: HashSet<PathBuf>,
        pub links: HashMap<PathBuf, PathBuf>,
        pub writable: HashSet<PathBuf>,
        pub path: Vec<PathBuf>,
        pub home: Option<PathBuf>,
        pub wsl: bool,
        pub answers: HashMap<String, CommandOutput>,
        pub handlers: HashMap<String, Handler>,
        pub log: Mutex<Vec<String>>,
        /// Each command line run, with the bound it was given.
        pub bounds: Mutex<Vec<(String, std::time::Duration)>>,
    }

    pub fn ok(stdout: &str) -> CommandOutput {
        CommandOutput {
            success: true,
            code: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    pub fn fail(stderr: &str) -> CommandOutput {
        CommandOutput {
            success: false,
            code: Some(1),
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    impl FakeHost {
        pub fn new() -> Self {
            Self::default()
        }

        /// An executable at `path`.
        pub fn exe(mut self, path: &str) -> Self {
            self.executables.insert(PathBuf::from(path));
            self.files.insert(PathBuf::from(path));
            self
        }

        /// A deck binary at `path` that answers `--version` with `version`.
        pub fn deck(self, path: &str, version: &str) -> Self {
            self.exe(path).answer(
                &format!("{path} --version"),
                ok(&format!("dot-agent-deck {version}\n")),
            )
        }

        // Used only by Unix-gated tests: native Windows is unsupported (#164).
        #[cfg(unix)]
        pub fn link(mut self, from: &str, to: &str) -> Self {
            self.links.insert(PathBuf::from(from), PathBuf::from(to));
            self
        }

        pub fn writable(mut self, dir: &str) -> Self {
            self.writable.insert(PathBuf::from(dir));
            self
        }

        // Used only by Unix-gated tests: native Windows is unsupported (#164).
        #[cfg(unix)]
        pub fn on_path(mut self, dir: &str) -> Self {
            self.path.push(PathBuf::from(dir));
            self
        }

        // Used only by Unix-gated tests: native Windows is unsupported (#164).
        #[cfg(unix)]
        pub fn home(mut self, dir: &str) -> Self {
            self.home = Some(PathBuf::from(dir));
            self
        }

        // Used only by Unix-gated tests: native Windows is unsupported (#164).
        #[cfg(unix)]
        pub fn wsl(mut self) -> Self {
            self.wsl = true;
            self
        }

        /// What `command_line` (program and arguments joined by spaces)
        /// answers.
        pub fn answer(mut self, command_line: &str, output: CommandOutput) -> Self {
            self.answers.insert(command_line.to_string(), output);
            self
        }

        /// Run `handler` for every invocation of `program`.
        pub fn handle(
            self,
            program: &str,
            handler: impl Fn(&[String]) -> CommandOutput + Send + Sync + 'static,
        ) -> Self {
            self.handle_io(program, move |args| Ok(handler(args)))
        }

        /// [`Self::handle`] for a handler that can fail to run the command,
        /// a timeout ([`super::timed_out`]) included.
        pub fn handle_io(
            mut self,
            program: &str,
            handler: impl Fn(&[String]) -> std::io::Result<CommandOutput> + Send + Sync + 'static,
        ) -> Self {
            self.handlers.insert(program.to_string(), Box::new(handler));
            self
        }

        pub fn ran(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        /// The bound the command line starting with `prefix` was run with.
        pub fn bound_of(&self, prefix: &str) -> Option<std::time::Duration> {
            self.bounds
                .lock()
                .unwrap()
                .iter()
                .find(|(line, _)| line.starts_with(prefix))
                .map(|(_, bound)| *bound)
        }
    }

    impl Host for FakeHost {
        fn run_within(
            &self,
            program: &Path,
            args: &[&OsStr],
            timeout: std::time::Duration,
        ) -> std::io::Result<CommandOutput> {
            let words: Vec<String> = std::iter::once(program.as_os_str())
                .chain(args.iter().copied())
                .map(|w| w.to_string_lossy().into_owned())
                .collect();
            let line = words.join(" ");
            self.log.lock().unwrap().push(line.clone());
            self.bounds.lock().unwrap().push((line.clone(), timeout));
            if let Some(handler) = self.handlers.get(&words[0]) {
                return handler(&words[1..]);
            }
            match self.answers.get(&line) {
                Some(output) => Ok(output.clone()),
                None if self.executables.contains(program) => Ok(fail("no answer configured")),
                None => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            }
        }

        fn find_program(&self, name: &str) -> Option<PathBuf> {
            self.path
                .iter()
                .map(|dir| dir.join(name))
                .find(|candidate| self.is_executable(candidate))
        }

        fn is_executable(&self, path: &Path) -> bool {
            let path = self.links.get(path).map_or(path, PathBuf::as_path);
            self.executables.contains(path)
        }

        fn exists(&self, path: &Path) -> bool {
            self.files.contains(path)
                || self.links.contains_key(path)
                || self.files.iter().any(|file| file.starts_with(path))
        }

        fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
            if let Some(target) = self.links.get(path) {
                return Some(target.clone());
            }
            self.exists(path).then(|| path.to_path_buf())
        }

        fn dir_writable(&self, dir: &Path) -> bool {
            self.writable.contains(dir)
        }

        fn home(&self) -> Option<PathBuf> {
            self.home.clone()
        }

        fn is_wsl(&self) -> bool {
            self.wsl
        }
    }
}
