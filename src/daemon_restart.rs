//! PRD #1487: the daemon side of [`AttachRequest::RestartDaemon`] — the pieces
//! the handler arm in `daemon_protocol.rs` composes, kept here so each one is
//! unit-testable without a socket.
//!
//! - [`InstallRecord`] is what the daemon remembers about its own binary,
//!   captured once at `daemon serve` start.
//! - [`resolve_restart_target`] turns that into "the binary now installed at my
//!   own path" (PRD D2). The client never sends a path.
//! - [`verify_restart_target`] checks that binary answers `--version` with the
//!   expected version before anything is stopped.
//! - [`stop_set`] and [`restart_decision`] are the policy (PRD D6): an idle
//!   daemon restarts without asking; a live one names everything at stake and
//!   waits for that exact set to come back as `confirm`.
//! - [`RestartControl`] serialises requests and carries the accepted target
//!   to `run_daemon_with`, which spawns it once the sockets are released.
//!
//! It also holds the two JSON shapes the remote plumbing subcommands print
//! (`daemon probe --json`, `daemon restart-installed --json`), so the laptop
//! side parses exactly what the remote side wrote.
//!
//! [`AttachRequest::RestartDaemon`]: crate::daemon_protocol::AttachRequest::RestartDaemon

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::agent_pty::AgentRecord;
use crate::daemon_protocol::{
    AttachResponse, RestartAgent, RestartDaemonReply, RestartRefusalReason, RestartStopSet,
};
use crate::state::OrchestrationRoleRecord;

/// How long `<target> --version` may take before the daemon gives up on it
/// and refuses the restart. Generous, because a cold first exec of a freshly
/// downloaded binary can be slow on a loaded host, and a refusal here costs
/// nothing but a retry.
pub const RESTART_VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// The most of `<target> --version`'s stdout the daemon keeps. One short line
/// is the honest answer; this bounds a binary that never stops printing.
const VERSION_OUTPUT_CAP: u64 = 8 * 1024;

/// What the daemon recorded about its own binary when it started.
///
/// Captured once, at `daemon serve` start, because after an upgrade replaced
/// the file `current_exe()` may report a path with a ` (deleted)` suffix or a
/// Homebrew keg that no longer exists — the startup path is the stable fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRecord {
    pub startup_exe: PathBuf,
    /// Who restarts this process when it exits — see [`Supervisor`].
    pub supervisor: Supervisor,
}

impl InstallRecord {
    /// This process's own executable, as `daemon serve` sees it at startup.
    /// An unreadable `current_exe()` yields an empty path, which
    /// [`resolve_restart_target`] answers with `TargetUnresolvable`.
    pub fn capture() -> Self {
        Self {
            startup_exe: std::env::current_exe().unwrap_or_default(),
            supervisor: detect_supervisor(&SupervisionFacts::capture()),
        }
    }

    /// A record that resolves to nothing — the default for an in-process or
    /// test daemon, which must never spawn its own test harness as a
    /// "successor".
    pub fn unresolved() -> Self {
        Self {
            startup_exe: PathBuf::new(),
            supervisor: Supervisor::None,
        }
    }
}

/// Who restarts this daemon when it exits, decided once at `daemon serve`
/// start.
///
/// It matters to an accepted [`RestartSuccessor::Installed`] restart. With no
/// supervisor the daemon starts its successor itself, detached, once its
/// sockets are released. Under a service manager that does not work: systemd
/// treats the exit of a service's main process as the end of the service and
/// kills everything left in its cgroup — the detached successor included — and
/// the documented unit's `Restart=on-failure` does not restart after a clean
/// exit. So a supervised daemon starts no successor and instead exits with
/// [`SUPERVISED_RESTART_EXIT`], which `Restart=on-failure` (and launchd's
/// `KeepAlive`) answers by running the unit's own command again: the build now
/// installed at that path.
///
/// [`RestartSuccessor::Installed`]: crate::daemon_protocol::RestartSuccessor::Installed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Supervisor {
    /// Nothing restarts this process; the daemon starts its own successor.
    None,
    /// The main process of a systemd service.
    Systemd,
    /// A launchd job.
    Launchd,
}

/// The exit status of a supervised daemon that accepted a restart: non-zero so
/// systemd's `Restart=on-failure` restarts the unit, and `EX_TEMPFAIL` because
/// the exit asks to be run again.
pub const SUPERVISED_RESTART_EXIT: u8 = 75;

/// What [`detect_supervisor`] decides from, gathered by
/// [`SupervisionFacts::capture`] and spelled out so the decision is testable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupervisionFacts {
    /// This process's pid.
    pub pid: u32,
    /// Its parent's pid at startup.
    pub ppid: u32,
    /// The parent's command name (`/proc/<ppid>/comm`), where readable.
    pub parent_comm: Option<String>,
    /// `INVOCATION_ID`, which systemd sets for every process it starts.
    pub invocation_id: Option<String>,
    /// `SYSTEMD_EXEC_PID` (systemd 248+): the pid systemd started.
    pub systemd_exec_pid: Option<String>,
    /// `XPC_SERVICE_NAME`, which launchd sets to a job's label.
    pub xpc_service_name: Option<String>,
}

impl SupervisionFacts {
    /// This process's facts, read now. `daemon serve` reads them before
    /// anything else could reparent it.
    pub fn capture() -> Self {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        // SAFETY: getppid has no preconditions and cannot fail.
        #[cfg(unix)]
        let ppid = unsafe { libc::getppid() } as u32;
        #[cfg(not(unix))]
        let ppid = 0;
        #[cfg(target_os = "linux")]
        let parent_comm = std::fs::read_to_string(format!("/proc/{ppid}/comm"))
            .ok()
            .map(|c| c.trim().to_string());
        #[cfg(not(target_os = "linux"))]
        let parent_comm = None;
        Self {
            pid: std::process::id(),
            ppid,
            parent_comm,
            invocation_id: env("INVOCATION_ID"),
            systemd_exec_pid: env("SYSTEMD_EXEC_PID"),
            xpc_service_name: env("XPC_SERVICE_NAME"),
        }
    }
}

/// Decide whether a service manager started THIS process, pure.
///
/// The environment variables alone are not enough: every child of a service —
/// a shell in a terminal started by a user service, a deck started from an
/// agent's pane under a supervised daemon — inherits them. Treating such a
/// daemon as supervised would make it exit for a restart nobody performs, so
/// each rule also requires evidence that this process is the one the manager
/// started:
///
/// - **systemd**: `INVOCATION_ID` is set, and `SYSTEMD_EXEC_PID` names this
///   pid. Where systemd predates `SYSTEMD_EXEC_PID` (before 248), the parent
///   must be the service manager itself (`systemd`).
/// - **launchd**: `XPC_SERVICE_NAME` names a job (a shell under Terminal has
///   `0`), and the parent is launchd (pid 1).
pub fn detect_supervisor(facts: &SupervisionFacts) -> Supervisor {
    if facts.invocation_id.is_some() {
        let started_us = match facts.systemd_exec_pid.as_deref() {
            Some(pid) => pid.trim().parse::<u32>().ok() == Some(facts.pid),
            None => facts.parent_comm.as_deref() == Some("systemd"),
        };
        if started_us {
            return Supervisor::Systemd;
        }
    }
    if let Some(name) = facts.xpc_service_name.as_deref()
        && name != "0"
        && facts.ppid == 1
    {
        return Supervisor::Launchd;
    }
    Supervisor::None
}

/// What `run_daemon_with` does with an accepted restart once its sockets are
/// released.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuccessorPlan {
    /// No restart was accepted, or the client starts its own build.
    Nothing,
    /// Start this binary, detached.
    Spawn(PathBuf),
    /// Start nothing and exit with [`SUPERVISED_RESTART_EXIT`], so the service
    /// manager starts the installed build.
    LeaveToSupervisor(Supervisor),
}

/// [`SuccessorPlan`] from the accepted target and the supervisor, pure.
pub fn successor_plan(target: Option<PathBuf>, supervisor: Supervisor) -> SuccessorPlan {
    match (target, supervisor) {
        (None, _) => SuccessorPlan::Nothing,
        (Some(target), Supervisor::None) => SuccessorPlan::Spawn(target),
        (Some(_), supervisor) => SuccessorPlan::LeaveToSupervisor(supervisor),
    }
}

/// "The binary now installed at my own path" (PRD D2), from the path the
/// daemon started from. Pure.
///
/// 1. A trailing ` (deleted)` is stripped — Linux `/proc/self/exe` reports it
///    once an atomic `mv` replaced the inode or `brew cleanup` removed the keg.
/// 2. `<prefix>/Cellar/<formula>/<ver>/bin/<name>` becomes
///    `<prefix>/bin/<name>`, the link `brew upgrade` repoints (Linux resolves
///    symlinks in `current_exe`; on macOS it is already the link path).
/// 3. Otherwise the stripped path itself.
/// 4. A non-absolute or empty path is `TargetUnresolvable`.
pub fn resolve_restart_target(startup_exe: &Path) -> Result<PathBuf, RestartRefusalReason> {
    let raw = startup_exe.to_string_lossy();
    let stripped = raw.strip_suffix(" (deleted)").unwrap_or(&raw);
    if stripped.is_empty() {
        return Err(RestartRefusalReason::TargetUnresolvable);
    }
    let path = PathBuf::from(stripped);
    if !path.is_absolute() {
        return Err(RestartRefusalReason::TargetUnresolvable);
    }
    if let Some(linked) = homebrew_link_for(&path) {
        return Ok(linked);
    }
    Ok(path)
}

/// `<prefix>/Cellar/<formula>/<ver>/bin/<name>` → `<prefix>/bin/<name>`.
fn homebrew_link_for(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    let bin = path.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    let version_dir = bin.parent()?;
    let formula_dir = version_dir.parent()?;
    let cellar = formula_dir.parent()?;
    if cellar.file_name()? != "Cellar" {
        return None;
    }
    let prefix = cellar.parent()?;
    Some(prefix.join("bin").join(name))
}

/// Check the restart target before anything is stopped: a regular file with an
/// exec bit, whose `--version` exits 0 within `timeout` and prints
/// `dot-agent-deck X.Y.Z` — matching `expected` when one is given (a leading
/// `v` on either side is ignored). Returns the reported version.
///
/// Catches a wrong-architecture build, a half-written file, and a brew upgrade
/// that unlinked the old binary without linking the new one. Blocking — the
/// handler runs it in `spawn_blocking`.
pub fn verify_restart_target(
    target: &Path,
    expected: Option<&str>,
    timeout: Duration,
) -> Result<String, (RestartRefusalReason, String)> {
    verify_restart_target_pinned(target, expected, timeout).map(|verified| verified.version)
}

/// Which file a path named when it was checked: device and inode where the
/// platform has them, plus size and modification time (PRD #1487 audit A5).
/// Read through symlinks, so a repointed Homebrew link reads as a different
/// file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    dev: u64,
    ino: u64,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl FileIdentity {
    /// The identity of what `meta` describes.
    pub fn of(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        let (dev, ino) = {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino())
        };
        #[cfg(not(unix))]
        let (dev, ino) = (0, 0);
        Self {
            dev,
            ino,
            len: meta.len(),
            modified: meta.modified().ok(),
        }
    }

    /// The identity of the file at `path` now, following symlinks.
    pub fn read(path: &Path) -> std::io::Result<Self> {
        std::fs::metadata(path).map(|meta| Self::of(&meta))
    }
}

/// A restart target that passed [`verify_restart_target`]: where it is, what
/// it reported, and which file it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedTarget {
    pub path: PathBuf,
    pub version: String,
    pub identity: FileIdentity,
}

/// [`verify_restart_target`], keeping the identity of the file it checked so
/// the successor spawn can tell whether the path still names it.
///
/// The identity is read before `--version` runs and again after; a file that
/// changed in between is refused, so the identity kept is the one whose
/// answer was read.
pub fn verify_restart_target_pinned(
    target: &Path,
    expected: Option<&str>,
    timeout: Duration,
) -> Result<VerifiedTarget, (RestartRefusalReason, String)> {
    let shown = target.display();
    let meta = std::fs::metadata(target).map_err(|e| {
        (
            RestartRefusalReason::TargetMissing,
            format!("the installed build at {shown} is not there ({e})"),
        )
    })?;
    if !meta.is_file() {
        return Err((
            RestartRefusalReason::TargetMissing,
            format!("the installed build at {shown} is not a regular file"),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err((
                RestartRefusalReason::TargetMissing,
                format!("the installed build at {shown} is not executable"),
            ));
        }
    }

    let did_not_answer = |why: String| {
        (
            RestartRefusalReason::TargetDidNotAnswer,
            format!("the installed build at {shown} did not answer `--version`: {why}"),
        )
    };
    let stdout = run_version_bounded(target, timeout).map_err(did_not_answer)?;
    let version = crate::version::parse_version_output(&stdout).ok_or_else(|| {
        did_not_answer(format!(
            "it printed {:?}, which is not `dot-agent-deck X.Y.Z`",
            stdout.trim()
        ))
    })?;
    if let Some(expected) = expected
        && strip_v(expected) != strip_v(&version)
    {
        return Err((
            RestartRefusalReason::VersionMismatch,
            format!(
                "the installed build at {shown} reports {version}, not the expected {expected}"
            ),
        ));
    }
    let identity = FileIdentity::of(&meta);
    if FileIdentity::read(target).ok() != Some(identity) {
        return Err((
            RestartRefusalReason::TargetMissing,
            format!("the installed build at {shown} changed while it was being checked"),
        ));
    }
    Ok(VerifiedTarget {
        path: target.to_path_buf(),
        version,
        identity,
    })
}

/// What [`RestartControl::recheck_successor`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuccessorCheck {
    /// Nothing was verified for that path, so there is nothing to compare.
    NotPinned,
    /// The path still names the file that was verified.
    Unchanged,
    /// The path names a different file now, and it passed verification
    /// against the version the first check reported.
    Reverified(String),
}

/// The successor re-check, pure apart from the filesystem: the identity of
/// `verified.path` now against the one recorded, and on a difference a fresh
/// [`verify_restart_target`] expecting `verified.version`.
///
/// What remains is the window between this check and the spawn itself, and
/// under a service manager the manager starts whatever its unit names — this
/// check never runs there (see [`SuccessorPlan::LeaveToSupervisor`]).
pub fn recheck_verified_target(
    verified: &VerifiedTarget,
    timeout: Duration,
) -> Result<SuccessorCheck, String> {
    if FileIdentity::read(&verified.path).ok() == Some(verified.identity) {
        return Ok(SuccessorCheck::Unchanged);
    }
    verify_restart_target(&verified.path, Some(&verified.version), timeout)
        .map(SuccessorCheck::Reverified)
        .map_err(|(_, message)| {
            format!("the installed build changed after it was verified, and {message}")
        })
}

fn strip_v(v: &str) -> &str {
    v.strip_prefix('v').unwrap_or(v)
}

/// Run `<target> --version` with a wall-clock bound and a byte cap on stdout.
fn run_version_bounded(target: &Path, timeout: Duration) -> Result<String, String> {
    use std::process::{Command, Stdio};
    let deadline = Instant::now() + timeout;
    // A file written moments ago can still be held open for writing by a
    // concurrent fork elsewhere in this process (ETXTBSY); that clears in
    // milliseconds, so retry briefly rather than refuse a restart over it.
    let mut child = loop {
        match Command::new(target)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => break child,
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("it could not be started ({e})")),
        }
    };
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(out) = stdout {
            let _ = out.take(VERSION_OUTPUT_CAP).read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("it did not exit within {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("waiting for it failed ({e})")),
        }
    };
    if !status.success() {
        return Err(format!("it exited with {status}"));
    }
    // A grandchild holding the pipe open must not hang the daemon: wait for the
    // reader only until the same deadline (plus a moment for the final read).
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(200));
    let buf = rx
        .recv_timeout(remaining)
        .map_err(|_| "its output did not close".to_string())?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// What a restart would stop, from the same two sources the `StopDaemon` and
/// `ListAgents` arms read (`AppState::live_orchestration_roles` and
/// `AgentPtyRegistry::agent_records`).
pub fn stop_set(roles: &[OrchestrationRoleRecord], agents: &[AgentRecord]) -> RestartStopSet {
    RestartStopSet {
        agents: agents
            .iter()
            .map(|r| RestartAgent {
                id: r.id.clone(),
                label: r.display_name.clone().unwrap_or_else(|| r.id.clone()),
                pane_id: r.pane_id_env.clone(),
                cwd: r.cwd.clone(),
            })
            .collect(),
        roles: roles.to_vec(),
    }
}

/// The D6 policy, pure. `None` means "go ahead"; `Some` is the
/// [`RestartDaemonReply::NeedsConfirmation`] to answer with.
///
/// - Nothing at stake: go ahead, whatever `confirm` says — there is no question
///   to answer, which is what lets an idle daemon restart with no TTY.
/// - Something at stake and no `confirm`: ask (`stale = false`).
/// - `confirm` names the same targets: go ahead.
/// - `confirm` names different targets: ask again with the current set
///   (`stale = true`) — the user agreed to stop something else.
pub fn restart_decision(
    at_stake: &RestartStopSet,
    confirm: Option<&RestartStopSet>,
) -> Option<RestartDaemonReply> {
    if at_stake.is_empty() {
        return None;
    }
    match confirm {
        Some(confirmed) if confirmed.same_targets(at_stake) => None,
        Some(_) => Some(RestartDaemonReply::NeedsConfirmation {
            at_stake: at_stake.clone(),
            stale: true,
        }),
        None => Some(RestartDaemonReply::NeedsConfirmation {
            at_stake: at_stake.clone(),
            stale: false,
        }),
    }
}

/// Per-daemon restart state, shared by every connection.
///
/// - `lock` serialises handlers: it is held from the first check to the
///   moment the request is accepted, so a second client gets an immediate
///   `InProgress` rather than a queue.
/// - `accepted` latches: once one request is accepted, every later one is
///   `InProgress`.
/// - `successor` is the verified target, consumed by `run_daemon_with` after
///   the sockets are released.
/// - `stopped` is set by every stop path (`StopDaemon`, `KIND_SHUTDOWN`, a
///   termination signal) through [`Self::request_stop`]. It is written and
///   read under `successor`'s lock, together with the successor itself, so a
///   stop and an acceptance cannot interleave: a stop clears a successor
///   already latched, and an acceptance after a stop latches nothing
///   (PRD #1487, Qodo 4201244680).
/// - `committed` is the one point after which a stop no longer wins: set,
///   under the same lock, when `run_daemon_with` takes a plan that starts a
///   successor ([`Self::take_successor_plan`]). Before it a stop always wins;
///   after it the restart is irrevocable, and a stop is not acknowledged as
///   one — `StopDaemon` refuses, saying the daemon is already restarting —
///   so the exit status and the spawn decided there never change
///   underneath it (Qodo 4201481137, 4201540983).
#[derive(Debug)]
pub struct RestartControl {
    lock: tokio::sync::Mutex<()>,
    accepted: AtomicBool,
    successor: StdMutex<Option<PathBuf>>,
    stopped: AtomicBool,
    committed: AtomicBool,
    handed_off: AtomicBool,
    install: InstallRecord,
    /// The identity of the verified successor, for the re-check just before it
    /// is spawned (audit A5). `None` when nothing was verified.
    pinned: StdMutex<Option<VerifiedTarget>>,
    /// A pause the restart handler takes at one [`RestartPause`] point, so a
    /// unit test can interleave work with a reservation it holds.
    #[cfg(test)]
    checkpoint: StdMutex<Option<(RestartPause, RestartCheckpoint)>>,
}

/// Where the restart handler pauses for an armed [`RestartCheckpoint`] (test
/// only).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartPause {
    /// Every check passed; `Accepted` is not written yet.
    BeforeAccepting,
    /// `Accepted` is written; the successor is not latched yet.
    AfterAccepting,
}

/// One pause in the restart handler (test only): it signals `reached`, then
/// waits for `resume`.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub(crate) struct RestartCheckpoint {
    pub reached: std::sync::Arc<tokio::sync::Notify>,
    pub resume: std::sync::Arc<tokio::sync::Notify>,
}

impl RestartControl {
    pub fn new(install: InstallRecord) -> Self {
        Self {
            lock: tokio::sync::Mutex::new(()),
            accepted: AtomicBool::new(false),
            successor: StdMutex::new(None),
            stopped: AtomicBool::new(false),
            committed: AtomicBool::new(false),
            handed_off: AtomicBool::new(false),
            install,
            pinned: StdMutex::new(None),
            #[cfg(test)]
            checkpoint: StdMutex::new(None),
        }
    }

    /// Arm a one-shot pause before the next acceptance is written (test only).
    #[cfg(test)]
    pub(crate) fn pause_before_accepting(&self) -> RestartCheckpoint {
        self.pause_at(RestartPause::BeforeAccepting)
    }

    /// Arm a one-shot pause at `at` (test only).
    #[cfg(test)]
    pub(crate) fn pause_at(&self, at: RestartPause) -> RestartCheckpoint {
        let checkpoint = RestartCheckpoint::default();
        *self.checkpoint.lock().unwrap() = Some((at, checkpoint.clone()));
        checkpoint
    }

    /// Whether a restart handler holds the lock (test only).
    #[cfg(test)]
    pub(crate) fn handler_busy(&self) -> bool {
        self.lock.try_lock().is_err()
    }

    /// Take the pause armed for `at`, if any (test only).
    #[cfg(test)]
    pub(crate) async fn checkpoint(&self, at: RestartPause) {
        let armed = {
            let mut slot = self.checkpoint.lock().unwrap();
            match slot.as_ref() {
                Some((armed_at, _)) if *armed_at == at => slot.take().map(|(_, c)| c),
                _ => None,
            }
        };
        if let Some(checkpoint) = armed {
            checkpoint.reached.notify_one();
            checkpoint.resume.notified().await;
        }
    }

    /// What the daemon recorded about its own binary at startup.
    pub fn install(&self) -> &InstallRecord {
        &self.install
    }

    /// Take the handler lock, or `None` when another restart holds it or one
    /// was already accepted.
    pub fn try_begin(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        if self.accepted.load(Ordering::SeqCst) || self.is_stop_requested() {
            return None;
        }
        let guard = self.lock.try_lock().ok()?;
        // Re-check under the lock: a request accepted between the load above
        // and the lock must still win.
        if self.accepted.load(Ordering::SeqCst) || self.is_stop_requested() {
            return None;
        }
        Some(guard)
    }

    /// Latch the acceptance and record the successor to spawn (`None` in
    /// `ClientSpawns` mode, where the client starts its own build). Returns
    /// `false`, latching nothing, when a stop was requested first: the daemon
    /// then just stops.
    pub fn mark_accepted(&self, successor: Option<PathBuf>) -> bool {
        self.latch(successor, None)
    }

    /// [`Self::mark_accepted`] for a successor that was verified, keeping what
    /// was verified so [`Self::recheck_successor`] can tell whether the file at
    /// that path is still the one checked (audit A5).
    pub fn mark_accepted_verified(&self, verified: VerifiedTarget) -> bool {
        let path = verified.path.clone();
        self.latch(Some(path), Some(verified))
    }

    fn latch(&self, successor: Option<PathBuf>, verified: Option<VerifiedTarget>) -> bool {
        let mut slot = self.successor.lock().unwrap_or_else(|p| p.into_inner());
        if self.stopped.load(Ordering::SeqCst) {
            return false;
        }
        if verified.is_some() {
            *self.pinned.lock().unwrap_or_else(|p| p.into_inner()) = verified;
        }
        *slot = successor;
        self.accepted.store(true, Ordering::SeqCst);
        true
    }

    /// A stop path is tearing this daemon down: no successor may start after
    /// it. Clears a successor already latched and makes every later
    /// [`Self::mark_accepted`] latch nothing — unless the restart was already
    /// committed ([`Self::take_successor_plan`]), which a stop no longer
    /// changes.
    pub fn request_stop(&self) -> StopClaim {
        let mut slot = self.successor.lock().unwrap_or_else(|p| p.into_inner());
        if self.committed.load(Ordering::SeqCst) {
            return StopClaim::RestartCommitted;
        }
        self.stopped.store(true, Ordering::SeqCst);
        *slot = None;
        *self.pinned.lock().unwrap_or_else(|p| p.into_inner()) = None;
        if self.accepted.load(Ordering::SeqCst) {
            StopClaim::OverrodeRestart
        } else {
            StopClaim::Stopped
        }
    }

    /// [`Self::request_stop`] for the stop path named by `via`, logging what
    /// it did to a restart. `false` when the restart was already committed:
    /// the daemon is exiting for it and its successor starts regardless.
    pub fn stop_wins(&self, via: &str) -> bool {
        match self.request_stop() {
            StopClaim::Stopped => true,
            StopClaim::OverrodeRestart => {
                tracing::warn!(
                    via,
                    "a stop arrived after a restart was accepted; the daemon stops without \
                     starting a successor"
                );
                true
            }
            StopClaim::RestartCommitted => {
                tracing::warn!(
                    via,
                    "a stop arrived after this daemon committed to its restart; it is already \
                     exiting and its successor starts regardless"
                );
                false
            }
        }
    }

    /// Undo [`Self::request_stop`] for a stop that did not go ahead
    /// (`StopDaemon` whose acknowledgement could not be written), so later
    /// restarts are not refused forever. A successor it cleared is not
    /// restored: a restart accepted before it then ends with no successor,
    /// and the client's check reports that none answered.
    pub fn withdraw_stop(&self) {
        let _slot = self.successor.lock().unwrap_or_else(|p| p.into_inner());
        self.stopped.store(false, Ordering::SeqCst);
    }

    /// Whether a stop path has called [`Self::request_stop`].
    pub fn is_stop_requested(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Just before `target` is spawned: whether it may be. Passes when nothing
    /// was pinned for that path, or the file there is still the verified one
    /// ([`FileIdentity`]); otherwise the new file is verified again, against
    /// the version the first check reported, and refused if that fails.
    pub fn recheck_successor(&self, target: &Path) -> Result<SuccessorCheck, String> {
        let pinned = self
            .pinned
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        match pinned {
            Some(verified) if verified.path == target => {
                recheck_verified_target(&verified, RESTART_VERIFY_TIMEOUT)
            }
            _ => Ok(SuccessorCheck::NotPinned),
        }
    }

    /// Whether a restart has been accepted.
    pub fn is_accepted(&self) -> bool {
        self.accepted.load(Ordering::SeqCst)
    }

    /// The accepted successor, once. `run_daemon_with` calls this after its
    /// serve loop has returned and its sockets are released.
    pub fn take_successor(&self) -> Option<PathBuf> {
        self.successor
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    /// What to do with the accepted successor, once — the latched successor
    /// decided against the recorded [`Supervisor`], under the lock a stop
    /// takes. A plan that starts a successor commits the restart: from here
    /// on [`Self::request_stop`] changes nothing, so neither this plan nor
    /// [`Self::handed_to_supervisor`], which `daemon serve` reads for its exit
    /// status, can change after it was decided.
    pub fn take_successor_plan(&self) -> SuccessorPlan {
        let mut slot = self.successor.lock().unwrap_or_else(|p| p.into_inner());
        let plan = successor_plan(slot.take(), self.install.supervisor);
        if !matches!(plan, SuccessorPlan::Nothing) {
            self.committed.store(true, Ordering::SeqCst);
        }
        if matches!(plan, SuccessorPlan::LeaveToSupervisor(_)) {
            self.handed_off.store(true, Ordering::SeqCst);
        }
        plan
    }

    /// Whether an accepted restart was left to the service manager, so the
    /// daemon must exit with [`SUPERVISED_RESTART_EXIT`]. Decided once, by
    /// [`Self::take_successor_plan`].
    pub fn handed_to_supervisor(&self) -> bool {
        self.handed_off.load(Ordering::SeqCst)
    }
}

/// What [`RestartControl::request_stop`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopClaim {
    /// The stop is recorded; no restart had been accepted.
    Stopped,
    /// The stop is recorded and cancelled an accepted restart's successor.
    OverrodeRestart,
    /// The restart was already committed; the stop changed nothing.
    RestartCommitted,
}

impl Default for RestartControl {
    fn default() -> Self {
        Self::new(InstallRecord::unresolved())
    }
}

/// What `dot-agent-deck daemon probe --json` prints: whether a daemon is
/// running at this host's endpoint, and its `Hello` reply when one is. Never
/// lazy-spawns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonProbe {
    pub running: bool,
    #[serde(default)]
    pub hello: Option<AttachResponse>,
}

/// What `dot-agent-deck daemon restart-installed --json` prints.
///
/// - `running = false`: no daemon at this host's endpoint; nothing to restart.
/// - `unsupported = true`: the running daemon does not advertise
///   `restart-daemon`, so nothing was sent.
/// - otherwise `reply` is the daemon's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRestartReport {
    pub running: bool,
    #[serde(default)]
    pub reply: Option<RestartDaemonReply>,
    #[serde(default)]
    pub unsupported: bool,
}

/// `restart-installed` exits with this when it failed before the restart
/// request was sent: the daemon was not asked, so the caller can say so.
pub const RESTART_NOT_SENT_EXIT: u8 = 1;

/// `restart-installed` exits with this when the restart request was sent and
/// then no answer was read ([`crate::daemon_client::ClientError::Unanswered`]):
/// the daemon may be restarting, so the caller checks before it says which.
/// The caller treats every non-zero exit other than [`RESTART_NOT_SENT_EXIT`]
/// (and clap's usage error) the same way, a crash after the send included.
pub const RESTART_UNANSWERED_EXIT: u8 = 3;

/// Hex-encode a [`RestartStopSet`]'s JSON for `restart-installed
/// --confirm-hex`. Hex keeps the argument free of shell metacharacters,
/// because the ssh route takes one command string and no stdin.
pub fn encode_stop_set_hex(set: &RestartStopSet) -> String {
    let json = serde_json::to_vec(set).unwrap_or_default();
    let mut out = String::with_capacity(json.len() * 2);
    for b in json {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The inverse of [`encode_stop_set_hex`].
pub fn decode_stop_set_hex(hex: &str) -> Result<RestartStopSet, String> {
    let hex = hex.trim();
    if !hex.len().is_multiple_of(2) {
        return Err("odd number of hex digits".into());
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| {
            hex.get(i..i + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| format!("not a hex digit pair at offset {i}"))
        })
        .collect::<Result<Vec<u8>, String>>()?;
    serde_json::from_slice(&bytes).map_err(|e| format!("not a stop set: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_protocol::{
        AttachRequest, CAP_RESTART_DAEMON, DAEMON_CAPABILITIES, RestartSuccessor,
    };

    fn agent(id: &str) -> RestartAgent {
        RestartAgent {
            id: id.into(),
            label: format!("label-{id}"),
            pane_id: Some(format!("p-{id}")),
            cwd: Some("/w".into()),
        }
    }

    fn role(pane: &str, role: &str, orch: &str) -> OrchestrationRoleRecord {
        OrchestrationRoleRecord {
            pane_id: pane.into(),
            role: role.into(),
            orchestration: orch.into(),
            is_orchestrator: role == "orchestrator",
        }
    }

    fn set(agents: &[&str], roles: &[(&str, &str, &str)]) -> RestartStopSet {
        RestartStopSet {
            agents: agents.iter().map(|a| agent(a)).collect(),
            roles: roles.iter().map(|(p, r, o)| role(p, r, o)).collect(),
        }
    }

    // ---- wire ----

    #[test]
    fn restart_request_round_trips_and_defaults_its_fields() {
        let req = AttachRequest::RestartDaemon {
            confirm: Some(set(&["a1"], &[("p1", "coder", "o")])),
            expected_version: Some("0.46.0".into()),
            successor: RestartSuccessor::ClientSpawns,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["op"], "restart-daemon");
        assert_eq!(json["successor"], "client-spawns");
        match serde_json::from_value::<AttachRequest>(json).unwrap() {
            AttachRequest::RestartDaemon {
                confirm,
                expected_version,
                successor,
            } => {
                assert_eq!(confirm, Some(set(&["a1"], &[("p1", "coder", "o")])));
                assert_eq!(expected_version.as_deref(), Some("0.46.0"));
                assert_eq!(successor, RestartSuccessor::ClientSpawns);
            }
            other => panic!("decoded as {other:?}"),
        }

        // A bare frame: every field optional, successor defaults to Installed.
        let bare: AttachRequest =
            serde_json::from_str(r#"{"op":"restart-daemon"}"#).expect("bare frame decodes");
        assert!(matches!(
            bare,
            AttachRequest::RestartDaemon {
                confirm: None,
                expected_version: None,
                successor: RestartSuccessor::Installed,
            }
        ));
    }

    #[test]
    fn restart_replies_round_trip_with_their_outcome_tags() {
        let replies = [
            RestartDaemonReply::Accepted {
                from_version: "0.45.0".into(),
                to_version: Some("0.46.0".into()),
                successor: RestartSuccessor::Installed,
                stopping: set(&["a1"], &[]),
            },
            RestartDaemonReply::NeedsConfirmation {
                at_stake: set(&["a1", "a2"], &[("p1", "orchestrator", "o")]),
                stale: true,
            },
            RestartDaemonReply::Refused {
                reason: RestartRefusalReason::VersionMismatch,
                message: "m".into(),
            },
        ];
        let tags = ["accepted", "needs-confirmation", "refused"];
        for (reply, tag) in replies.into_iter().zip(tags) {
            let mut resp = AttachResponse::ok();
            resp.restart = Some(reply.clone());
            let json = serde_json::to_value(&resp).unwrap();
            assert_eq!(json["restart"]["outcome"], tag);
            let back: AttachResponse = serde_json::from_value(json).unwrap();
            assert_eq!(back.restart, Some(reply));
        }
        // An unrelated response omits the field entirely.
        let json = serde_json::to_value(AttachResponse::ok()).unwrap();
        assert!(json.get("restart").is_none());
    }

    #[test]
    fn an_unknown_refusal_reason_decodes_as_unknown() {
        let reply: RestartDaemonReply = serde_json::from_str(
            r#"{"outcome":"refused","reason":"some-future-reason","message":"x"}"#,
        )
        .unwrap();
        assert_eq!(
            reply,
            RestartDaemonReply::Refused {
                reason: RestartRefusalReason::Unknown,
                message: "x".into()
            }
        );
    }

    #[test]
    fn the_restart_capability_is_advertised() {
        assert_eq!(CAP_RESTART_DAEMON, "restart-daemon");
        assert!(DAEMON_CAPABILITIES.contains(&CAP_RESTART_DAEMON));
        let hello =
            AttachResponse::hello(crate::daemon_protocol::PROTOCOL_VERSION).with_capabilities();
        assert!(
            hello
                .capabilities
                .unwrap()
                .iter()
                .any(|c| c == CAP_RESTART_DAEMON)
        );
    }

    #[test]
    fn confirm_hex_round_trips_and_rejects_garbage() {
        let s = set(&["a1"], &[("p1", "coder", "o; rm -rf /")]);
        let hex = encode_stop_set_hex(&s);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(decode_stop_set_hex(&hex).unwrap(), s);
        assert!(decode_stop_set_hex("abc").is_err());
        assert!(decode_stop_set_hex("zz").is_err());
        assert!(decode_stop_set_hex("7b").is_err());
    }

    // ---- resolve_restart_target ----

    #[cfg(unix)]
    #[test]
    fn resolve_strips_the_deleted_suffix() {
        assert_eq!(
            resolve_restart_target(Path::new("/home/u/.local/bin/dot-agent-deck (deleted)")),
            Ok(PathBuf::from("/home/u/.local/bin/dot-agent-deck"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_maps_a_homebrew_keg_to_the_prefix_link() {
        assert_eq!(
            resolve_restart_target(Path::new(
                "/home/linuxbrew/.linuxbrew/Cellar/dot-agent-deck/0.45.0/bin/dot-agent-deck"
            )),
            Ok(PathBuf::from(
                "/home/linuxbrew/.linuxbrew/bin/dot-agent-deck"
            ))
        );
        // …including a keg `brew cleanup` already removed.
        assert_eq!(
            resolve_restart_target(Path::new(
                "/opt/homebrew/Cellar/dot-agent-deck/0.45.0/bin/dot-agent-deck (deleted)"
            )),
            Ok(PathBuf::from("/opt/homebrew/bin/dot-agent-deck"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_keeps_a_plain_path() {
        for p in [
            "/home/u/.local/bin/dot-agent-deck",
            "/opt/homebrew/bin/dot-agent-deck",
            "/w/target/debug/dot-agent-deck",
            // A `bin` under something that is not a Cellar keg stays as it is.
            "/x/notcellar/f/1.0/bin/dot-agent-deck",
        ] {
            assert_eq!(resolve_restart_target(Path::new(p)), Ok(PathBuf::from(p)));
        }
    }

    #[test]
    fn resolve_refuses_a_relative_or_empty_path() {
        for p in ["", "dot-agent-deck", "bin/dot-agent-deck", " (deleted)"] {
            assert_eq!(
                resolve_restart_target(Path::new(p)),
                Err(RestartRefusalReason::TargetUnresolvable),
                "{p:?}"
            );
        }
        assert_eq!(
            resolve_restart_target(&InstallRecord::unresolved().startup_exe),
            Err(RestartRefusalReason::TargetUnresolvable)
        );
    }

    // ---- verify_restart_target ----

    #[cfg(unix)]
    fn script(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "#!/bin/sh\n{body}").unwrap();
            f.sync_all().unwrap();
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn verify_refuses_a_missing_target() {
        let dir = tempfile::tempdir().unwrap();
        let err = verify_restart_target(&dir.path().join("absent"), None, RESTART_VERIFY_TIMEOUT)
            .unwrap_err();
        assert_eq!(err.0, RestartRefusalReason::TargetMissing);
        // A directory is not a regular file either.
        let err = verify_restart_target(dir.path(), None, RESTART_VERIFY_TIMEOUT).unwrap_err();
        assert_eq!(err.0, RestartRefusalReason::TargetMissing);
    }

    #[cfg(unix)]
    #[test]
    fn verify_refuses_a_non_executable_target() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "dad", "echo 'dot-agent-deck 0.46.0'", 0o644);
        let err = verify_restart_target(&p, None, RESTART_VERIFY_TIMEOUT).unwrap_err();
        assert_eq!(err.0, RestartRefusalReason::TargetMissing);
        assert!(err.1.contains("not executable"), "{}", err.1);
    }

    #[cfg(unix)]
    #[test]
    fn verify_refuses_a_target_that_does_not_answer_version() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("fails", "exit 3"),
            ("garbage", "echo 'hello world'"),
            ("other-program", "echo 'nano 7.0'"),
        ] {
            let p = script(dir.path(), name, body, 0o755);
            let err = verify_restart_target(&p, None, RESTART_VERIFY_TIMEOUT).unwrap_err();
            assert_eq!(err.0, RestartRefusalReason::TargetDidNotAnswer, "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn verify_times_out_on_a_target_that_hangs() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "hangs", "exec sleep 30", 0o755);
        let started = Instant::now();
        let err = verify_restart_target(&p, None, Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.0, RestartRefusalReason::TargetDidNotAnswer);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the bound must hold: {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_refuses_a_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "dad", "echo 'dot-agent-deck 0.45.0'", 0o755);
        let err = verify_restart_target(&p, Some("0.46.0"), RESTART_VERIFY_TIMEOUT).unwrap_err();
        assert_eq!(err.0, RestartRefusalReason::VersionMismatch);
        assert!(
            err.1.contains("0.45.0") && err.1.contains("0.46.0"),
            "{}",
            err.1
        );
    }

    /// Replace the file at `path` atomically, as an installer does: write a
    /// sibling, then rename it over.
    #[cfg(unix)]
    fn replace(dir: &Path, path: &Path, body: &str) {
        let staged = script(dir, "staged", body, 0o755);
        std::fs::rename(staged, path).unwrap();
    }

    /// Scenario: the successor is re-checked just before it is spawned (PRD
    /// #1487 audit A5). The same file passes untouched; a replacement of the
    /// same version passes after verifying again; a replacement reporting
    /// another version, or one that is gone, is refused — so the daemon starts
    /// nothing rather than a build nobody verified.
    #[cfg(unix)]
    #[test]
    fn the_successor_is_rechecked_against_the_verified_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "dad", "echo 'dot-agent-deck 0.46.0'", 0o755);
        let verified =
            verify_restart_target_pinned(&p, Some("0.46.0"), RESTART_VERIFY_TIMEOUT).unwrap();
        assert_eq!(verified.version, "0.46.0");
        assert_eq!(verified.identity, FileIdentity::read(&p).unwrap());
        assert_eq!(
            recheck_verified_target(&verified, RESTART_VERIFY_TIMEOUT),
            Ok(SuccessorCheck::Unchanged)
        );

        replace(dir.path(), &p, "echo 'dot-agent-deck 0.46.0' # rebuilt");
        assert_ne!(FileIdentity::read(&p).unwrap(), verified.identity);
        assert_eq!(
            recheck_verified_target(&verified, RESTART_VERIFY_TIMEOUT),
            Ok(SuccessorCheck::Reverified("0.46.0".into()))
        );

        replace(dir.path(), &p, "echo 'dot-agent-deck 0.47.0'");
        let err = recheck_verified_target(&verified, RESTART_VERIFY_TIMEOUT).unwrap_err();
        assert!(
            err.contains("changed after it was verified") && err.contains("0.47.0"),
            "{err}"
        );

        std::fs::remove_file(&p).unwrap();
        assert!(recheck_verified_target(&verified, RESTART_VERIFY_TIMEOUT).is_err());
    }

    /// Scenario: the control only re-checks the path it pinned; a successor
    /// accepted without verification (or another path) has nothing to compare.
    #[cfg(unix)]
    #[test]
    fn the_control_rechecks_only_a_pinned_successor() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "dad", "echo 'dot-agent-deck 0.46.0'", 0o755);
        let control = RestartControl::default();
        assert_eq!(control.recheck_successor(&p), Ok(SuccessorCheck::NotPinned));
        control.mark_accepted_verified(
            verify_restart_target_pinned(&p, None, RESTART_VERIFY_TIMEOUT).unwrap(),
        );
        assert!(control.is_accepted());
        assert_eq!(control.recheck_successor(&p), Ok(SuccessorCheck::Unchanged));
        assert_eq!(
            control.recheck_successor(&dir.path().join("other")),
            Ok(SuccessorCheck::NotPinned)
        );
        replace(dir.path(), &p, "echo 'dot-agent-deck 0.45.0'");
        assert!(control.recheck_successor(&p).is_err());
        assert_eq!(control.take_successor(), Some(p));
    }

    #[cfg(unix)]
    #[test]
    fn verify_accepts_a_matching_target() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "dad", "echo 'dot-agent-deck 0.46.0'", 0o755);
        assert_eq!(
            verify_restart_target(&p, Some("0.46.0"), RESTART_VERIFY_TIMEOUT),
            Ok("0.46.0".to_string())
        );
        // A leading `v` on either side is not a mismatch.
        assert_eq!(
            verify_restart_target(&p, Some("v0.46.0"), RESTART_VERIFY_TIMEOUT),
            Ok("0.46.0".to_string())
        );
        // No expectation: any parsable version is accepted.
        assert_eq!(
            verify_restart_target(&p, None, RESTART_VERIFY_TIMEOUT),
            Ok("0.46.0".to_string())
        );
    }

    // ---- restart_decision ----

    #[test]
    fn an_idle_daemon_needs_no_confirmation() {
        let empty = RestartStopSet::default();
        assert_eq!(restart_decision(&empty, None), None);
        // A stale confirm against an idle daemon is no reason to ask.
        assert_eq!(restart_decision(&empty, Some(&set(&["a1"], &[]))), None);
    }

    #[test]
    fn a_live_daemon_without_confirm_asks() {
        let live = set(&["a1"], &[("p1", "orchestrator", "o")]);
        assert_eq!(
            restart_decision(&live, None),
            Some(RestartDaemonReply::NeedsConfirmation {
                at_stake: live.clone(),
                stale: false
            })
        );
        // Roles alone count too.
        let roles_only = set(&[], &[("p1", "coder", "o")]);
        assert!(matches!(
            restart_decision(&roles_only, None),
            Some(RestartDaemonReply::NeedsConfirmation { stale: false, .. })
        ));
    }

    #[test]
    fn a_matching_confirm_goes_ahead() {
        let live = set(&["a1"], &[("p1", "orchestrator", "o")]);
        assert_eq!(restart_decision(&live, Some(&live.clone())), None);
    }

    #[test]
    fn a_mismatching_confirm_asks_again_as_stale() {
        let live = set(&["a1", "a2"], &[("p1", "orchestrator", "o")]);
        for stale_confirm in [
            set(&["a1"], &[("p1", "orchestrator", "o")]),
            set(&["a1", "a2"], &[]),
            set(&["a1", "a2"], &[("p1", "coder", "o")]),
            set(&["a1", "a2", "a3"], &[("p1", "orchestrator", "o")]),
        ] {
            assert_eq!(
                restart_decision(&live, Some(&stale_confirm)),
                Some(RestartDaemonReply::NeedsConfirmation {
                    at_stake: live.clone(),
                    stale: true
                }),
                "{stale_confirm:?}"
            );
        }
    }

    #[test]
    fn confirmation_is_compared_order_insensitively_on_identity_only() {
        let live = set(
            &["a1", "a2"],
            &[("p1", "orchestrator", "o"), ("p2", "coder", "o")],
        );
        let mut reordered = set(
            &["a2", "a1"],
            &[("p2", "coder", "o"), ("p1", "orchestrator", "o")],
        );
        // Display-only fields differ too.
        for a in &mut reordered.agents {
            a.label = "renamed".into();
            a.cwd = None;
            a.pane_id = None;
        }
        reordered.roles[0].is_orchestrator = true;
        assert!(live.same_targets(&reordered));
        assert_eq!(restart_decision(&live, Some(&reordered)), None);
    }

    #[test]
    fn stop_set_labels_agents_like_the_running_summary() {
        let json = serde_json::json!({
            "id": "a1", "pane_id_env": "p1", "display_name": null, "cwd": "/w",
            "tab_membership": null, "agent_type": null, "rows": 24, "cols": 80,
        });
        let unnamed: AgentRecord = serde_json::from_value(json.clone()).unwrap();
        let mut named = unnamed.clone();
        named.id = "a2".into();
        named.display_name = Some("Coder".into());
        let roles = vec![role("p1", "coder", "o")];
        let s = stop_set(&roles, &[unnamed, named]);
        assert_eq!(s.roles, roles);
        assert_eq!(s.agents[0].label, "a1");
        assert_eq!(s.agents[0].pane_id.as_deref(), Some("p1"));
        assert_eq!(s.agents[0].cwd.as_deref(), Some("/w"));
        assert_eq!(s.agents[1].label, "Coder");
    }

    // ---- RestartControl ----

    #[test]
    fn restart_control_serialises_and_latches() {
        let control = RestartControl::default();
        let first = control.try_begin().expect("first request takes the lock");
        assert!(
            control.try_begin().is_none(),
            "a second is refused while held"
        );
        drop(first);
        let again = control.try_begin().expect("released when not accepted");
        control.mark_accepted(Some(PathBuf::from("/x/dot-agent-deck")));
        drop(again);
        assert!(control.is_accepted());
        assert!(control.try_begin().is_none(), "acceptance latches");
        assert_eq!(
            control.take_successor(),
            Some(PathBuf::from("/x/dot-agent-deck"))
        );
        assert_eq!(control.take_successor(), None, "consumed once");
    }

    /// Scenario: a restart is accepted with a successor latched, then a stop
    /// arrives before the daemon exits. The stop clears the successor, so the
    /// daemon just stops — under no supervisor it spawns nothing, and under a
    /// service manager it exits cleanly instead of asking for a restart (PRD
    /// #1487, Qodo 4201244680).
    #[test]
    fn a_stop_after_acceptance_clears_the_latched_successor() {
        for supervisor in [Supervisor::None, Supervisor::Systemd] {
            let control = RestartControl::new(InstallRecord {
                startup_exe: PathBuf::from("/x/dot-agent-deck"),
                supervisor,
            });
            assert!(control.mark_accepted(Some(PathBuf::from("/x/dot-agent-deck"))));
            assert_eq!(
                control.request_stop(),
                StopClaim::OverrodeRestart,
                "the stop reports it overrode a restart"
            );
            assert!(control.is_stop_requested());
            assert_eq!(control.take_successor_plan(), SuccessorPlan::Nothing);
            assert!(!control.handed_to_supervisor());
        }
    }

    /// Scenario: the daemon has already taken a plan that starts its
    /// successor when a connection still finishing handles a stop. The restart
    /// was committed there, so the stop changes neither the plan nor the exit
    /// status and says so; a stop before that point still wins (PRD #1487,
    /// Qodo 4201481137, 4201540983).
    #[test]
    fn a_stop_after_the_restart_is_committed_changes_nothing() {
        for supervisor in [Supervisor::None, Supervisor::Systemd] {
            let control = RestartControl::new(InstallRecord {
                startup_exe: PathBuf::from("/x/dot-agent-deck"),
                supervisor,
            });
            assert!(control.mark_accepted(Some(PathBuf::from("/x/dot-agent-deck"))));
            assert_ne!(control.take_successor_plan(), SuccessorPlan::Nothing);
            let supervised = control.handed_to_supervisor();
            assert_eq!(supervised, supervisor == Supervisor::Systemd);
            assert_eq!(control.request_stop(), StopClaim::RestartCommitted);
            assert!(
                !control.stop_wins("test"),
                "a stop after the commit does not win"
            );
            assert!(!control.is_stop_requested());
            assert_eq!(
                control.handed_to_supervisor(),
                supervised,
                "the exit status decided at the commit does not change"
            );
        }

        // No restart accepted: taking the (empty) plan commits nothing, and a
        // late stop is recorded as usual.
        let control = RestartControl::default();
        assert_eq!(control.take_successor_plan(), SuccessorPlan::Nothing);
        assert!(control.stop_wins("test"));
    }

    /// Scenario: a wire stop is recorded but its acknowledgement cannot be
    /// written, so the stop does not go ahead. Withdrawing it lets a later
    /// restart begin instead of being refused forever (PRD #1487).
    #[test]
    fn a_withdrawn_stop_lets_restarts_begin_again() {
        let control = RestartControl::default();
        assert!(control.stop_wins("test"));
        assert!(control.try_begin().is_none());
        control.withdraw_stop();
        assert!(!control.is_stop_requested());
        assert!(control.try_begin().is_some());
    }

    /// Scenario: a stop arrives while a restart is still being checked. The
    /// restart's later acceptance latches nothing, and no new restart may
    /// begin (PRD #1487, Qodo 4201244680).
    #[cfg(unix)]
    #[test]
    fn an_acceptance_after_a_stop_latches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "dad", "echo 'dot-agent-deck 0.46.0'", 0o755);
        let control = RestartControl::default();
        let held = control.try_begin().expect("the restart takes the lock");
        assert_eq!(
            control.request_stop(),
            StopClaim::Stopped,
            "nothing was accepted yet"
        );
        assert!(!control.mark_accepted_verified(
            verify_restart_target_pinned(&p, None, RESTART_VERIFY_TIMEOUT).unwrap(),
        ));
        drop(held);
        assert!(!control.is_accepted());
        assert_eq!(control.recheck_successor(&p), Ok(SuccessorCheck::NotPinned));
        assert_eq!(control.take_successor_plan(), SuccessorPlan::Nothing);
        assert!(
            control.try_begin().is_none(),
            "no restart begins once a stop was requested"
        );
    }

    // ---- supervision ----

    /// A systemd service's main process: `INVOCATION_ID` set and
    /// `SYSTEMD_EXEC_PID` naming this pid.
    fn systemd_main() -> SupervisionFacts {
        SupervisionFacts {
            pid: 4242,
            ppid: 1700,
            parent_comm: Some("systemd".into()),
            invocation_id: Some("0f3c9a".into()),
            systemd_exec_pid: Some("4242".into()),
            xpc_service_name: None,
        }
    }

    #[test]
    fn a_systemd_main_process_is_supervised() {
        assert_eq!(detect_supervisor(&systemd_main()), Supervisor::Systemd);
        // The system manager is pid 1 and the parent check is not consulted
        // while SYSTEMD_EXEC_PID answers.
        let system = SupervisionFacts {
            ppid: 1,
            parent_comm: None,
            ..systemd_main()
        };
        assert_eq!(detect_supervisor(&system), Supervisor::Systemd);
        // Before systemd 248 there is no SYSTEMD_EXEC_PID: the parent being the
        // manager is the evidence.
        let old = SupervisionFacts {
            systemd_exec_pid: None,
            ..systemd_main()
        };
        assert_eq!(detect_supervisor(&old), Supervisor::Systemd);
    }

    /// The case that must not be mistaken for a service: a daemon started by a
    /// TUI whose shell inherited a service's environment. Treating it as
    /// supervised would make it exit for a restart nobody performs.
    #[test]
    fn a_child_that_inherited_a_services_environment_is_not_supervised() {
        let inherited = SupervisionFacts {
            pid: 5000,
            ppid: 4999,
            parent_comm: Some("dot-agent-deck".into()),
            ..systemd_main()
        };
        assert_eq!(detect_supervisor(&inherited), Supervisor::None);
        // Older systemd: no exec pid, and the parent is not the manager.
        let inherited_old = SupervisionFacts {
            systemd_exec_pid: None,
            ..inherited.clone()
        };
        assert_eq!(detect_supervisor(&inherited_old), Supervisor::None);
        // An unparseable exec pid proves nothing.
        let garbled = SupervisionFacts {
            systemd_exec_pid: Some("not-a-pid".into()),
            ..systemd_main()
        };
        assert_eq!(detect_supervisor(&garbled), Supervisor::None);
        // SYSTEMD_EXEC_PID without INVOCATION_ID is not systemd's doing.
        let no_invocation = SupervisionFacts {
            invocation_id: None,
            ..systemd_main()
        };
        assert_eq!(detect_supervisor(&no_invocation), Supervisor::None);
        // Nothing set at all: a plain lazily spawned daemon.
        assert_eq!(
            detect_supervisor(&SupervisionFacts {
                pid: 10,
                ppid: 9,
                ..SupervisionFacts::default()
            }),
            Supervisor::None
        );
    }

    #[test]
    fn a_launchd_job_is_supervised_and_a_terminal_shell_is_not() {
        let job = SupervisionFacts {
            pid: 700,
            ppid: 1,
            xpc_service_name: Some("ai.devopstoolkit.dot-agent-deck".into()),
            ..SupervisionFacts::default()
        };
        assert_eq!(detect_supervisor(&job), Supervisor::Launchd);
        // Terminal.app's shells carry XPC_SERVICE_NAME=0.
        let terminal = SupervisionFacts {
            xpc_service_name: Some("0".into()),
            ..job.clone()
        };
        assert_eq!(detect_supervisor(&terminal), Supervisor::None);
        // A job's label inherited by a process launchd did not start.
        let child = SupervisionFacts { ppid: 650, ..job };
        assert_eq!(detect_supervisor(&child), Supervisor::None);
    }

    #[test]
    fn a_supervised_daemon_leaves_the_successor_to_its_manager() {
        let target = PathBuf::from("/home/u/.local/bin/dot-agent-deck");
        assert_eq!(
            successor_plan(Some(target.clone()), Supervisor::None),
            SuccessorPlan::Spawn(target.clone()),
            "unsupervised: unchanged, the daemon starts its successor"
        );
        for supervisor in [Supervisor::Systemd, Supervisor::Launchd] {
            assert_eq!(
                successor_plan(Some(target.clone()), supervisor),
                SuccessorPlan::LeaveToSupervisor(supervisor)
            );
            // ClientSpawns (or no restart at all) records no target: nothing
            // to start and nothing to leave to anyone.
            assert_eq!(successor_plan(None, supervisor), SuccessorPlan::Nothing);
        }
        assert_eq!(
            successor_plan(None, Supervisor::None),
            SuccessorPlan::Nothing
        );
    }

    #[test]
    fn restart_control_latches_a_hand_off_only_for_an_accepted_supervised_restart() {
        let supervised = |supervisor| {
            RestartControl::new(InstallRecord {
                startup_exe: PathBuf::from("/x/dot-agent-deck"),
                supervisor,
            })
        };

        let control = supervised(Supervisor::Systemd);
        assert_eq!(control.take_successor_plan(), SuccessorPlan::Nothing);
        assert!(!control.handed_to_supervisor(), "no restart was accepted");

        let control = supervised(Supervisor::Systemd);
        control.mark_accepted(Some(PathBuf::from("/x/dot-agent-deck")));
        assert_eq!(
            control.take_successor_plan(),
            SuccessorPlan::LeaveToSupervisor(Supervisor::Systemd)
        );
        assert!(control.handed_to_supervisor());

        let control = supervised(Supervisor::None);
        control.mark_accepted(Some(PathBuf::from("/x/dot-agent-deck")));
        assert_eq!(
            control.take_successor_plan(),
            SuccessorPlan::Spawn(PathBuf::from("/x/dot-agent-deck"))
        );
        assert!(!control.handed_to_supervisor());

        // ClientSpawns under a supervisor: the client starts its own build, so
        // the daemon exits cleanly as before.
        let control = supervised(Supervisor::Systemd);
        control.mark_accepted(None);
        assert_eq!(control.take_successor_plan(), SuccessorPlan::Nothing);
        assert!(!control.handed_to_supervisor());
    }

    #[test]
    fn the_supervised_exit_status_is_a_failure_to_systemd() {
        // `Restart=on-failure` restarts on any non-zero status outside
        // `SuccessExitStatus`, which by default is 0 and the clean signals.
        assert_ne!(SUPERVISED_RESTART_EXIT, 0);
        assert_eq!(InstallRecord::unresolved().supervisor, Supervisor::None);
    }
}
