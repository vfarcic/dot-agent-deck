//! Which `dot-agent-deck` the agents' hooks run, and whether it is older than
//! this deck (issue #1637), plus the macOS locations a deck cannot pin from
//! (issue #1157).
//!
//! An agent's hook config names one deck binary. After an upgrade that binary
//! can be an older copy that still exists — a Homebrew install beside a newer
//! `~/.local/bin` one, a CLI beside a newer desktop app — and the hooks then
//! send what that older copy knows how to send. This module does three things
//! about it:
//!
//! - **Takeover** ([`Takeover`]): an automatic install by a newer copy that IS
//!   an install ([`crate::platform::paths::ResolutionArm::Running`], not
//!   somewhere [`crate::platform::paths::is_known_ephemeral`]) replaces a pin
//!   whose `--version` reports an older release. Ties, newer pins and pins
//!   whose version cannot be read are kept, so two copies never trade the pin
//!   back and forth.
//! - **Detection** ([`HookBinaryState`]): the daemon learns which binary each
//!   agent's hooks run, from the installers' pins at startup (probed with
//!   `--version`) and from the `deck_build` / `deck_exe` keys every hook line
//!   carries ([`stamp_hook_line`]). An older hook binary sends no keys at all,
//!   which is itself the signal ([`HookBinaryReason::Unreported`]).
//! - **Notice** ([`HookBinaryNotice`]): what both clients show, with the
//!   remedy composed here so the TUI and the desktop say the same words.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::event::AgentType;

/// How long the `--version` probe of a pinned hook binary may take. It sits on
/// the TUI's and the daemon's startup path, so it is much shorter than a
/// restart's verification bound.
pub const HOOK_BINARY_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How much probe time one startup may spend across every distinct pinned
/// binary, rather than [`HOOK_BINARY_PROBE_TIMEOUT`] for each. A probe that
/// would start after the budget is spent fails as unprobeable without running,
/// which keeps the pin. The budget refills [`PROBE_BUDGET_WINDOW`] after the
/// first probe of a window, so a long-running TUI that installs again later is
/// not left without one.
pub const HOOK_BINARY_PROBE_BUDGET: Duration = Duration::from_secs(5);

/// How long one [`HOOK_BINARY_PROBE_BUDGET`] lasts.
const PROBE_BUDGET_WINDOW: Duration = Duration::from_secs(60);

/// The most bytes a hook line's [`DECK_BUILD_LINE_KEY`] may carry. A build id
/// is `<version>-g<sha>[-dirty]`, a few dozen bytes; anything longer is not one.
pub const MAX_DECK_BUILD_BYTES: usize = 128;

/// The most bytes a hook line's [`DECK_EXE_LINE_KEY`] may carry, `PATH_MAX` on
/// Linux. A longer value is not a path this daemon's notice repeats.
pub const MAX_DECK_EXE_BYTES: usize = 4096;

/// How many distinct `(binary, reason)` warnings the daemon logs before it
/// stops logging new ones. A sender can name a new path on every hook line, so
/// the history is capped rather than kept for the daemon's lifetime.
pub const MAX_WARNED_BINARIES: usize = 64;

/// The most notices one reply or broadcast carries. Notices are grouped per
/// agent type, so a real deck has a handful; this bounds the payload whatever
/// the state holds.
pub const MAX_NOTICES: usize = 16;

/// The hook-line key carrying the sending deck's build id.
pub const DECK_BUILD_LINE_KEY: &str = "deck_build";
/// The hook-line key carrying the sending deck's own executable path.
pub const DECK_EXE_LINE_KEY: &str = "deck_exe";

/// Why a notice is raised. `#[serde(other)]` keeps a client decoding a reason
/// a newer daemon adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookBinaryReason {
    /// The hook binary reports an older release than the daemon.
    Older,
    /// The hook binary sends no version at all: it predates version reporting,
    /// so it is older than the release that added it.
    Unreported,
    /// The pinned hook binary did not answer `--version` within
    /// [`HOOK_BINARY_PROBE_TIMEOUT`], or answered something unreadable.
    Unprobeable,
    /// This deck runs from a mounted disk image or a translocated location and
    /// found no installed copy to pin instead, so no hooks were installed
    /// (issue #1157).
    EphemeralLocation,
    #[serde(other)]
    Unknown,
}

/// A hook binary the user should know about, as both clients show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookBinaryNotice {
    /// The binary the agents' hooks run (or, for
    /// [`HookBinaryReason::EphemeralLocation`], this deck's own path).
    pub binary: String,
    /// The agents whose hooks run it, by display name.
    pub agents: Vec<String>,
    /// The release it reports, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The daemon's own release.
    pub daemon_version: String,
    pub reason: HookBinaryReason,
    /// What to do about it, in words, composed by the daemon from its own
    /// constants. It never carries a path or anything else a hook line sent.
    /// When [`Self::command`] is set this is the lead-in to it (`Run:`), and a
    /// client shows the two together: `Run: brew upgrade dot-agent-deck`.
    pub remedy: String,
    /// The command that applies the fix, when there is one: composed by the
    /// daemon only from trusted data (its own install path, shell-quoted, or
    /// `brew upgrade dot-agent-deck`) and free of control characters. It is
    /// what the desktop's Copy button copies, and the button is absent when
    /// this is. Additive: an older client ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// The payload of [`crate::event::BroadcastMsg::HookBinaryNotice`]: the
/// daemon's whole current list of notices, which replaces what a client holds.
/// On the wire, `{"kind":"hook_binary_notice","notices":[…]}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookBinaryNotices {
    #[serde(default)]
    pub notices: Vec<HookBinaryNotice>,
}

/// The most bytes [`HookBinaryNotice::sanitized`] keeps of an agent name.
const MAX_NOTICE_AGENT_BYTES: usize = 64;

/// The most agent names [`HookBinaryNotice::sanitized`] keeps in one notice.
const MAX_NOTICE_AGENTS: usize = 16;

/// The most bytes [`HookBinaryNotice::sanitized`] keeps of a remedy.
const MAX_NOTICE_REMEDY_BYTES: usize = 1024;

/// The longest [`HookBinaryNotice::command`]: [`remedy_for`] offers no command
/// rather than a longer one, and a client drops a longer one it is sent.
pub const MAX_NOTICE_COMMAND_BYTES: usize = 8 * 1024;

/// `raw` as one line of display text: [`crate::untrusted_text::display_line`],
/// which strips control and bidi characters, after the Unicode line and
/// paragraph separators it keeps.
fn notice_field(raw: &str, max: usize) -> String {
    crate::untrusted_text::display_line(&raw.replace(['\u{2028}', '\u{2029}'], ""), max)
}

impl HookBinaryNotice {
    /// This notice as it is safe to put on one terminal line: every string
    /// stripped of control and bidi characters (newlines included) and clamped,
    /// the agent list bounded, and a command dropped rather than altered when
    /// sanitizing would change it, since a command is meant to be copied
    /// exactly as shown. A remedy that led in to a dropped command becomes
    /// [`REMEDY_UPGRADE_OR_REINSTALL`].
    ///
    /// The daemon applies it to what it sends, and the TUI again to what it
    /// receives and renders, because a TUI can be attached to a remote or older
    /// daemon.
    pub fn sanitized(&self) -> Self {
        let command = self.command.clone().filter(|command| {
            !command.is_empty() && notice_field(command, MAX_NOTICE_COMMAND_BYTES) == *command
        });
        let mut remedy = notice_field(&self.remedy, MAX_NOTICE_REMEDY_BYTES);
        if command.is_none() && remedy == REMEDY_RUN {
            remedy = REMEDY_UPGRADE_OR_REINSTALL.to_string();
        }
        Self {
            binary: notice_field(&self.binary, MAX_DECK_EXE_BYTES),
            agents: self
                .agents
                .iter()
                .take(MAX_NOTICE_AGENTS)
                .map(|agent| notice_field(agent, MAX_NOTICE_AGENT_BYTES))
                .collect(),
            version: self
                .version
                .as_deref()
                .map(|version| notice_field(version, MAX_DECK_BUILD_BYTES)),
            daemon_version: notice_field(&self.daemon_version, MAX_DECK_BUILD_BYTES),
            reason: self.reason,
            remedy,
            command,
        }
    }
}

/// [`HookBinaryNotice::sanitized`] over a list a daemon sent, at most
/// [`MAX_NOTICES`] of them.
pub fn sanitize_notices(notices: &[HookBinaryNotice]) -> Vec<HookBinaryNotice> {
    notices
        .iter()
        .take(MAX_NOTICES)
        .map(HookBinaryNotice::sanitized)
        .collect()
}

/// What one automatic installer left an agent's hooks pinned to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookPin {
    pub agent: AgentType,
    /// The config file the pin lives in.
    pub config: PathBuf,
    /// The binary the deck's entries there name.
    pub binary: String,
}

/// The name a notice uses for an agent.
pub fn agent_display_name(agent: &AgentType) -> &'static str {
    match agent {
        AgentType::ClaudeCode => "Claude Code",
        AgentType::OpenCode => "OpenCode",
        AgentType::Codex => "Codex",
        AgentType::Devin => "Devin",
        AgentType::Pi => "Pi",
        _ => "an agent",
    }
}

/// The `hooks install --agent` value for an agent.
fn agent_cli_name(agent: &AgentType) -> Option<&'static str> {
    match agent {
        AgentType::ClaudeCode => Some("claude-code"),
        AgentType::OpenCode => Some("opencode"),
        AgentType::Codex => Some("codex"),
        AgentType::Devin => Some("devin"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Version probe and comparison
// ---------------------------------------------------------------------------

type ProbeKey = (PathBuf, Option<crate::daemon_restart::FileIdentity>);

fn probe_cache() -> &'static Mutex<HashMap<ProbeKey, Result<String, String>>> {
    static CACHE: OnceLock<Mutex<HashMap<ProbeKey, Result<String, String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The probe time one window may still spend (see
/// [`HOOK_BINARY_PROBE_BUDGET`]).
#[derive(Debug, Clone, Copy)]
struct ProbeBudget {
    window_start: Option<Instant>,
    spent: Duration,
}

impl ProbeBudget {
    const fn new() -> Self {
        Self {
            window_start: None,
            spent: Duration::ZERO,
        }
    }

    /// How long a probe starting at `now` may run: at most
    /// [`HOOK_BINARY_PROBE_TIMEOUT`], and no more than the window has left.
    /// Zero when the window's budget is spent.
    fn allowance(&mut self, now: Instant) -> Duration {
        match self.window_start {
            Some(start) if now.saturating_duration_since(start) < PROBE_BUDGET_WINDOW => {}
            _ => {
                self.window_start = Some(now);
                self.spent = Duration::ZERO;
            }
        }
        HOOK_BINARY_PROBE_BUDGET
            .saturating_sub(self.spent)
            .min(HOOK_BINARY_PROBE_TIMEOUT)
    }

    fn charge(&mut self, elapsed: Duration) {
        self.spent = self.spent.saturating_add(elapsed);
    }
}

fn probe_budget() -> &'static Mutex<ProbeBudget> {
    static BUDGET: Mutex<ProbeBudget> = Mutex::new(ProbeBudget::new());
    &BUDGET
}

/// Resolve `path` through the filesystem. Every filesystem resolution this
/// module makes goes through here, so a test can assert that ingesting a hook
/// line and composing notices make none (issue #1637 audit A2).
fn resolve(path: &Path) -> Option<PathBuf> {
    #[cfg(test)]
    FS_RESOLUTIONS.with(|count| count.set(count.get() + 1));
    std::fs::canonicalize(path).ok()
}

#[cfg(test)]
thread_local! {
    static FS_RESOLUTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The release `binary --version` reports, run at most once per process for
/// the same file: bounded by [`HOOK_BINARY_PROBE_TIMEOUT`] and by the window's
/// [`HOOK_BINARY_PROBE_BUDGET`], stdout capped, and keyed by the canonical path
/// and the file's identity so the four installers probe a shared pin once and a
/// replaced file is probed again.
///
/// The bound covers the run of the binary ([`run_probe`]). Resolving the path,
/// reading its identity and the `spawn` itself are filesystem calls made before
/// the clock starts; they are not separately bounded. Only startup and the
/// installers call this, never a hook line's ingest.
pub fn probe_version(binary: &Path) -> Result<String, String> {
    let canonical = resolve(binary).unwrap_or_else(|| binary.to_path_buf());
    let key = (
        canonical.clone(),
        crate::daemon_restart::FileIdentity::read(&canonical).ok(),
    );
    if let Some(hit) = probe_cache()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&key)
    {
        return hit.clone();
    }
    let started = Instant::now();
    let allowance = probe_budget()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .allowance(started);
    if allowance.is_zero() {
        // Not cached: the next window may probe it.
        return Err("the startup probe budget was spent on other binaries".to_string());
    }
    let result = run_probe(&canonical, allowance).and_then(|stdout| {
        crate::version::parse_version_output(&stdout)
            .ok_or_else(|| "its `--version` output is not a dot-agent-deck version".to_string())
    });
    probe_budget()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .charge(started.elapsed());
    probe_cache()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key, result.clone());
    result
}

/// The most `--version` output a probe reads.
#[cfg(unix)]
const PROBE_OUTPUT_CAP: usize = 8 * 1024;

/// Run `target --version` for at most `timeout` and return its stdout.
///
/// On Unix the binary runs in a process group of its own, and the whole group
/// is `SIGKILL`ed when the binary exits or the deadline passes, so a
/// descendant it left behind does not outlive the probe. The group is only
/// ever signalled while its leader, the binary, is unreaped ([`ProbeGroup`]):
/// an unreaped leader keeps its pid, and so the group's id, from being reused,
/// and once it is reaped, or its state cannot be observed (`ECHILD` from a
/// leader the kernel reaped itself), the probe sends no signal at all (issue
/// #1637 audit A6). Stdout is read non-blocking from this thread, so a descendant that
/// escaped the group (a `setsid`) and still holds the pipe pins no thread: the
/// read gives up at the deadline. A binary killed at the deadline is reaped by
/// a detached thread, as `daemon_restart` does for a child stuck in
/// uninterruptible I/O, or inline when no thread can be made (audit A8).
///
/// Elsewhere (Windows) this is the restart path's bounded runner unchanged: it
/// kills only the binary on timeout and reads stdout on a helper thread.
#[cfg(unix)]
fn run_probe(target: &Path, timeout: Duration) -> Result<String, String> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let deadline = Instant::now() + timeout;
    let mut child = loop {
        match Command::new(target)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
        {
            Ok(child) => break child,
            // A file written moments ago can still be open for writing in a
            // concurrent fork (ETXTBSY); that clears in milliseconds.
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("it could not be started ({e})")),
        }
    };
    let mut stdout = child.stdout.take().expect("stdout is piped");
    // From here every return path, including an early one, leaves cleanup to
    // `ProbeGroup`: dropped with the leader unreaped and still observable, it
    // kills the group and reaps on a detached thread.
    let mut group = ProbeGroup::new(child);
    // A blocking read would precede both the exit check and the deadline, so a
    // pipe that cannot be made non-blocking is not read at all (audit A7).
    if let Err(e) = set_nonblocking(stdout.as_raw_fd()) {
        return Err(format!("its output could not be read ({e})"));
    }
    let mut out = Vec::new();
    let mut chunk = [0u8; 1024];
    let mut closed = false;
    let mut status = None;
    loop {
        while !closed {
            match stdout.read(&mut chunk) {
                Ok(0) => closed = true,
                Ok(n) => {
                    let room = PROBE_OUTPUT_CAP.saturating_sub(out.len());
                    out.extend_from_slice(&chunk[..n.min(room)]);
                    if out.len() >= PROBE_OUTPUT_CAP {
                        closed = true;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => closed = true,
            }
        }
        if status.is_none() {
            match group.leader_exited() {
                Ok(true) => {
                    // Whatever it left in the group goes with it, so the pipe
                    // closes; the group is killed before the leader is reaped.
                    group.kill();
                    match group.reap() {
                        Ok(exited) => status = Some(exited),
                        Err(e) => return Err(format!("waiting for it failed ({e})")),
                    }
                }
                Ok(false) => {}
                Err(e) => return Err(format!("waiting for it failed ({e})")),
            }
        }
        if status.is_some() && closed {
            break;
        }
        if Instant::now() >= deadline {
            if status.is_none() {
                return Err(format!("it did not exit within {}ms", timeout.as_millis()));
            }
            // The leader is reaped, so its group id is no longer this probe's
            // to signal: a descendant that escaped the group holds the pipe.
            // Stop reading and close our end.
            return Err("its output did not close".to_string());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = status.expect("the loop exits only with a status");
    if !status.success() {
        return Err(format!("it exited with {status}"));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Make `fd` non-blocking, retrying an interrupted `fcntl(2)`.
#[cfg(unix)]
fn set_nonblocking(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_NONBLOCKING.with(std::cell::Cell::get) {
        return Err(std::io::Error::from_raw_os_error(libc::EBADF));
    }
    let fcntl = |cmd: libc::c_int, arg: libc::c_int| loop {
        // SAFETY: `fcntl(2)` on a pipe descriptor the caller owns.
        let rc = unsafe { libc::fcntl(fd, cmd, arg) };
        if rc >= 0 {
            return Ok(rc);
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    let flags = fcntl(libc::F_GETFL, 0)?;
    fcntl(libc::F_SETFL, flags | libc::O_NONBLOCK).map(drop)
}

/// A probed binary and the process group it leads (`process_group(0)`, so the
/// group's id is its pid).
///
/// The leader is kept unreaped for as long as the group may be signalled: a
/// zombie still holds its pid, so the numeric group id cannot name another
/// process's group. Once [`reap`](Self::reap) or [`abandon`](Self::abandon)
/// has taken the leader, [`kill`](Self::kill) does nothing (audit A6). So does
/// a failed [`leader_exited`](Self::leader_exited): `ECHILD` can mean the
/// kernel already reaped the leader (an inherited ignored `SIGCHLD`, or
/// `SA_NOCLDWAIT`), and then its pid is free for reuse, so the group is
/// released without a signal, as `remote.rs`'s `Leader::abandon` does.
#[cfg(unix)]
struct ProbeGroup {
    /// `Some` until the leader is reaped, handed to the reaping thread, or
    /// released because its state could not be observed.
    leader: Option<std::process::Child>,
    id: libc::pid_t,
}

#[cfg(unix)]
impl ProbeGroup {
    fn new(leader: std::process::Child) -> Self {
        let id = leader.id() as libc::pid_t;
        Self {
            leader: Some(leader),
            id,
        }
    }

    /// Whether the leader has exited, without reaping it (`WNOWAIT`).
    ///
    /// A failure other than an interruption releases the leader without a
    /// signal before it is returned: the probe no longer knows the pid is its
    /// own (audit A6).
    fn leader_exited(&mut self) -> std::io::Result<bool> {
        if self.leader.is_none() {
            return Ok(true);
        }
        loop {
            #[cfg(test)]
            if let Some(errno) = WAITID_ERROR.with(std::cell::Cell::get) {
                let e = std::io::Error::from_raw_os_error(errno);
                self.release();
                return Err(e);
            }
            // SAFETY: an all-zero `siginfo_t` is valid, and `waitid` with
            // `WNOHANG` leaves `si_pid` zero when the child has not exited.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: `waitid(2)` on this function's own unreaped child, with
            // `WNOWAIT` so the child stays a zombie.
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.id as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc == 0 {
                // SAFETY: `waitid` filled `info` for a child-state report.
                return Ok(unsafe { info.si_pid() } != 0);
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                self.release();
                return Err(e);
            }
        }
    }

    /// Give up the leader without signalling or waiting for it. After
    /// `ECHILD` there is nothing left to reap, and its pid and group id may
    /// already belong to someone else. Any other `waitid` failure is one the
    /// probe cannot interpret, so it is treated the same way: no signal, and
    /// no wait that an unkilled binary could hold open.
    fn release(&mut self) {
        if self.leader.take().is_some() {
            #[cfg(test)]
            PROBE_EVENTS.with(|events| events.borrow_mut().push(ProbeEvent::Release));
        }
    }

    /// `SIGKILL` the group, but only while the leader is unreaped.
    fn kill(&self) {
        if self.leader.is_none() {
            return;
        }
        #[cfg(test)]
        PROBE_EVENTS.with(|events| events.borrow_mut().push(ProbeEvent::Kill));
        // SAFETY: `killpg(2)` on the group this probe's unreaped child leads; a
        // group whose only member is the zombie leader takes the signal
        // harmlessly.
        unsafe { libc::killpg(self.id, libc::SIGKILL) };
    }

    /// Reap a leader [`leader_exited`](Self::leader_exited) reported, which
    /// does not block. The group is not signalled again afterwards.
    fn reap(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let mut leader = self.leader.take().expect("the leader is reaped once");
        #[cfg(test)]
        PROBE_EVENTS.with(|events| events.borrow_mut().push(ProbeEvent::Reap));
        leader.wait()
    }

    /// Kill the group and reap the leader on a detached thread, which a child
    /// stuck in uninterruptible I/O cannot pin this one on.
    ///
    /// The leader's state is observed first, so a leader the kernel already
    /// reaped behind the probe's back (an early return before the exit poll,
    /// or a panic) is released instead of signalled. This runs from `Drop`,
    /// so it never panics on its own account.
    fn abandon(&mut self) {
        if self.leader.is_none() || self.leader_exited().is_err() {
            return;
        }
        self.kill();
        if let Some(leader) = self.leader.take() {
            #[cfg(test)]
            PROBE_EVENTS.with(|events| events.borrow_mut().push(ProbeEvent::Reap));
            reap_detached(leader);
        }
    }
}

#[cfg(unix)]
impl Drop for ProbeGroup {
    fn drop(&mut self) {
        self.abandon();
    }
}

/// Wait for a killed probe leader on a thread of its own (audit A8).
///
/// The child reaches the thread through a channel, so this one keeps it when
/// the thread cannot be created, and then waits for it here instead of
/// dropping it unreaped or panicking (a panic in `Drop` during an unwind
/// aborts the process). That wait is bounded in practice: the group was
/// `SIGKILL`ed while the leader was still unreaped, and a `SIGKILL` is acted on
/// as soon as the leader next runs. The one residual case is a leader in
/// uninterruptible sleep (a hung network filesystem read, say), which holds
/// this thread until that I/O completes; that is the case the detached thread
/// exists for, and it is reached only when no thread can be made at all.
#[cfg(unix)]
fn reap_detached(leader: std::process::Child) {
    let (tx, rx) = std::sync::mpsc::channel::<std::process::Child>();
    let spawned = {
        #[cfg(test)]
        let refused = FAIL_REAPER_SPAWN.with(std::cell::Cell::get);
        #[cfg(not(test))]
        let refused = false;
        if refused {
            Err(std::io::Error::from_raw_os_error(libc::EAGAIN))
        } else {
            std::thread::Builder::new()
                .name("hook-probe-reaper".to_string())
                .spawn(move || {
                    if let Ok(mut leader) = rx.recv() {
                        let _ = leader.wait();
                    }
                })
                .map(drop)
        }
    };
    let mut leader = match spawned {
        Ok(()) => match tx.send(leader) {
            Ok(()) => return,
            // The thread is gone without receiving it; reap it here.
            Err(std::sync::mpsc::SendError(leader)) => leader,
        },
        Err(e) => {
            tracing::warn!(error = %e, "no thread for the hook probe's reaper; reaping inline");
            leader
        }
    };
    let _ = leader.wait();
}

/// What a [`ProbeGroup`] did, in order, on this thread (audit A6's ordering).
#[cfg(all(test, unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeEvent {
    Kill,
    Reap,
    /// The leader was given up without a signal or a wait.
    Release,
}

#[cfg(all(test, unix))]
thread_local! {
    static PROBE_EVENTS: std::cell::RefCell<Vec<ProbeEvent>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Makes [`set_nonblocking`] fail, for audit A7's path.
    static FAIL_NONBLOCKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Makes [`ProbeGroup::leader_exited`] fail with this errno, for audit A6's
    /// `ECHILD` path.
    static WAITID_ERROR: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
    /// Makes [`reap_detached`] behave as if no thread could be created, for
    /// audit A8.
    static FAIL_REAPER_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// See the Unix [`run_probe`]. No process groups here, so this keeps the
/// restart path's behaviour: the binary alone is killed on timeout.
#[cfg(not(unix))]
fn run_probe(target: &Path, timeout: Duration) -> Result<String, String> {
    crate::daemon_restart::run_version_bounded(target, timeout)
}

/// Whether release `candidate` is strictly newer than release `than`. A leading
/// `v` and a `-g<sha>[-dirty]` build stamp are ignored; a version either side
/// cannot parse is never newer.
pub fn is_newer_release(candidate: &str, than: &str) -> bool {
    crate::daemon_upgrade::client_is_newer(candidate, than)
}

/// The release a build id (`<version>-g<sha>[-dirty]`, or `<version>-unknown`
/// when git was unavailable at build time) was built from.
pub fn release_of_build(build: &str) -> &str {
    let build = build.trim();
    let build = build.strip_suffix("-dirty").unwrap_or(build);
    if let Some(version) = build.strip_suffix("-unknown") {
        return version;
    }
    match build.rsplit_once("-g") {
        Some((version, sha)) if !sha.is_empty() && sha.chars().all(|c| c.is_ascii_hexdigit()) => {
            version
        }
        _ => build,
    }
}

// ---------------------------------------------------------------------------
// Takeover
// ---------------------------------------------------------------------------

/// Whether, and as which release, this process may take another install's
/// hook pin over in an automatic install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Takeover {
    /// This copy's release when it is takeover-eligible, else `None`.
    own_version: Option<String>,
}

thread_local! {
    static TAKEOVER_OVERRIDE: std::cell::RefCell<Option<Takeover>> =
        const { std::cell::RefCell::new(None) };
}

impl Takeover {
    /// A copy that never takes over.
    pub fn ineligible() -> Self {
        Self { own_version: None }
    }

    /// An eligible copy at release `own_version`.
    pub fn eligible(own_version: impl Into<String>) -> Self {
        Self {
            own_version: Some(own_version.into()),
        }
    }

    /// This process's policy: eligible, at its own release, only when the
    /// resolver names the running binary itself
    /// ([`crate::platform::paths::ResolutionArm::Running`]), and neither that
    /// installed name nor the file the running binary resolves to is
    /// [`known ephemeral`](crate::platform::paths::is_known_ephemeral). See
    /// [`takeover_eligible`].
    ///
    /// A test can pin the answer for its own thread with [`with_takeover`].
    pub fn current() -> Self {
        if let Some(pinned) = TAKEOVER_OVERRIDE.with(|o| o.borrow().clone()) {
            return pinned;
        }
        #[cfg(test)]
        {
            Self::ineligible()
        }
        #[cfg(not(test))]
        {
            static PROCESS: OnceLock<Takeover> = OnceLock::new();
            PROCESS
                .get_or_init(|| {
                    let running = crate::platform::paths::running_binary_target();
                    match (process_resolution(), running) {
                        (Ok(resolved), Some(running))
                            if takeover_eligible(
                                resolved.arm,
                                Path::new(&resolved.path),
                                &running,
                            ) =>
                        {
                            Self::eligible(env!("DAD_VERSION"))
                        }
                        _ => Self::ineligible(),
                    }
                })
                .clone()
        }
    }

    /// Whether an automatic install by this copy replaces a deck entry pinned
    /// to `exe`: only when this copy is eligible and `exe --version` reports a
    /// strictly older release. A tie, a newer pin, and a pin whose version
    /// cannot be read all keep the pin. The running binary is never asked —
    /// callers check [`crate::agent_hook_config::executables_match`] first.
    pub fn supersedes(&self, exe: &str) -> bool {
        let Some(own) = self.own_version.as_deref() else {
            return false;
        };
        let Ok(pinned) = probe_version(Path::new(exe)) else {
            return false;
        };
        let newer = is_newer_release(own, &pinned);
        if newer {
            log_takeover_once(exe, &pinned, own);
        }
        newer
    }
}

/// This process's [`crate::platform::paths::durable_binary_resolution`],
/// resolved on first use and shared by [`Takeover::current`] and the daemon's
/// startup ([`DeckIdentity::current`], [`refused_ephemeral_location`]), so the
/// resolver — and its last-resort warning — runs once for all of them. The
/// installers still resolve for themselves.
pub fn process_resolution() -> &'static Result<crate::platform::paths::Resolved, String> {
    static PROCESS: OnceLock<Result<crate::platform::paths::Resolved, String>> = OnceLock::new();
    PROCESS.get_or_init(crate::platform::paths::durable_binary_resolution)
}

/// Whether a copy resolved through `arm` to the installed name `installed`,
/// whose running file canonically is `running`, may take another install's pin
/// over (issue #1637). Eligible iff the arm is
/// [`Running`](crate::platform::paths::ResolutionArm::Running) and NEITHER
/// path is [`known ephemeral`](crate::platform::paths::is_known_ephemeral):
/// an installed name that is a link to a scratch copy in `/var/tmp` would pin
/// the hooks to a file that disappears when the scratch directory is cleaned.
/// No location is exempt, `~/.local/bin` included: one under a mounted or
/// translocated home is as temporary as the home.
pub fn takeover_eligible(
    arm: crate::platform::paths::ResolutionArm,
    installed: &Path,
    running: &Path,
) -> bool {
    arm == crate::platform::paths::ResolutionArm::Running
        && !crate::platform::paths::is_known_ephemeral(installed)
        && !crate::platform::paths::is_known_ephemeral(running)
}

/// Run `f` with [`Takeover::current`] answering `takeover` on this thread.
pub fn with_takeover<R>(takeover: Takeover, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Takeover>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            TAKEOVER_OVERRIDE.with(|o| *o.borrow_mut() = previous);
        }
    }
    let _restore = Restore(TAKEOVER_OVERRIDE.with(|o| o.borrow_mut().replace(takeover)));
    f()
}

fn log_takeover_once(exe: &str, pinned: &str, own: &str) {
    static LOGGED: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let first = LOGGED
        .get_or_init(|| Mutex::new(BTreeSet::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(exe.to_string());
    if first {
        tracing::info!(
            pinned = exe,
            pinned_version = pinned,
            own_version = own,
            "takeover: newer copy — the agents' hooks named an older dot-agent-deck \
             ({pinned}); this copy ({own}) replaces that pin in place"
        );
    }
}

// ---------------------------------------------------------------------------
// The hook-line stamp
// ---------------------------------------------------------------------------

/// Add [`DECK_BUILD_LINE_KEY`] and [`DECK_EXE_LINE_KEY`] to a hook line's JSON
/// object. They describe the sender, not the event, so they are not fields of
/// [`crate::event::AgentEvent`]: an older daemon ignores them, as it ignores
/// `token`, and the event a client is sent does not change.
pub fn stamp_hook_line(map: &mut serde_json::Map<String, serde_json::Value>) {
    map.insert(
        DECK_BUILD_LINE_KEY.to_string(),
        serde_json::Value::String(crate::build_id::local_build_id()),
    );
    if let Some(exe) = crate::platform::paths::executable_path() {
        map.insert(
            DECK_EXE_LINE_KEY.to_string(),
            serde_json::Value::String(exe),
        );
    }
}

/// Whether `value` is a stamp field the daemon will repeat: non-empty, at most
/// `max` bytes, and free of control characters, bidi formatting characters and
/// the Unicode line and paragraph separators. A hook line is producer-asserted
/// data, so a field that fails this is never logged, stored or shown.
fn is_clean_stamp_field(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && !value.chars().any(|c| {
            c.is_control()
                || crate::untrusted_text::is_bidi_format_char(c)
                || matches!(c, '\u{2028}' | '\u{2029}')
        })
}

/// Whether a hook line's `deck_build` is one this daemon reads (see
/// [`MAX_DECK_BUILD_BYTES`]).
pub fn is_valid_deck_build(build: &str) -> bool {
    is_clean_stamp_field(build, MAX_DECK_BUILD_BYTES)
}

/// Whether a hook line's `deck_exe` is one this daemon reads: an absolute
/// path within [`MAX_DECK_EXE_BYTES`] with no control or bidi characters.
pub fn is_valid_deck_exe(exe: &str) -> bool {
    is_clean_stamp_field(exe, MAX_DECK_EXE_BYTES) && Path::new(exe).is_absolute()
}

/// The stamp a hook line carries. Both keys absent means an older hook binary.
///
/// [`Self::from_line`] keeps only fields that pass [`is_valid_deck_build`] and
/// [`is_valid_deck_exe`]: a `deck_exe` that fails reads as absent, and a
/// `deck_build` that fails reads as `Some("")` — reported, but unusable, so
/// [`HookBinaryState::observe`] ignores the line for diagnostics while the
/// event itself is still applied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct HookLineSender {
    #[serde(default)]
    pub deck_build: Option<String>,
    #[serde(default)]
    pub deck_exe: Option<String>,
}

impl HookLineSender {
    /// The stamp on a raw hook line. A key that is not a string reads as
    /// absent; a string that fails validation is dropped as described on
    /// [`HookLineSender`], so nothing over budget is copied out of the line.
    pub fn from_line(line: &str) -> Self {
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(line)
        else {
            return Self::default();
        };
        let text = |key: &str| map.get(key).and_then(serde_json::Value::as_str);
        Self {
            deck_build: text(DECK_BUILD_LINE_KEY).map(|build| {
                if is_valid_deck_build(build) {
                    build.to_string()
                } else {
                    String::new()
                }
            }),
            deck_exe: text(DECK_EXE_LINE_KEY)
                .filter(|exe| is_valid_deck_exe(exe))
                .map(str::to_string),
        }
    }

    /// Whether the line carries a stamp at all.
    pub fn is_reported(&self) -> bool {
        self.deck_build.is_some()
    }
}

// ---------------------------------------------------------------------------
// The daemon's view
// ---------------------------------------------------------------------------

/// Who the daemon is, for comparing hook binaries against it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeckIdentity {
    /// The daemon's own executable, canonicalized.
    pub exe: Option<PathBuf>,
    /// The daemon's release (`DAD_VERSION`).
    pub version: String,
    /// The path `hooks install` from this deck would pin, when that is this
    /// deck's own file — so a notice can tell the user to run it.
    pub self_install_path: Option<String>,
}

impl DeckIdentity {
    /// This process, given the startup's one
    /// [`crate::platform::paths::durable_binary_resolution`]. Resolves paths
    /// through the filesystem, so it runs once, at startup, before the daemon
    /// holds any lock.
    pub fn current(resolution: &Result<crate::platform::paths::Resolved, String>) -> Self {
        let exe = std::env::current_exe().ok().and_then(|exe| resolve(&exe));
        let self_install_path = resolution
            .as_ref()
            .ok()
            .map(|resolved| resolved.path.clone())
            .filter(|path| exe.is_some() && resolve(Path::new(path)).as_ref() == exe.as_ref());
        Self {
            exe,
            version: env!("DAD_VERSION").to_string(),
            self_install_path,
        }
    }

    /// Whether `binary` is spelled as this deck's own file, compared without
    /// touching the filesystem.
    fn is_self_spelling(&self, binary: &str) -> bool {
        self.exe.as_deref() == Some(Path::new(binary))
            || self.self_install_path.as_deref() == Some(binary)
    }
}

/// This deck's own path when it runs from a mounted disk image or a
/// translocated location and the resolver therefore refused to pin it (issue
/// #1157), for [`HookBinaryState::from_startup`]. `resolution` is the
/// startup's one [`crate::platform::paths::durable_binary_resolution`], shared
/// with [`DeckIdentity::current`] so the resolver (and its last-resort warning)
/// does not run again for either.
pub fn refused_ephemeral_location(
    resolution: &Result<crate::platform::paths::Resolved, String>,
) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::path::absolute(&exe).unwrap_or(exe);
    if !crate::platform::paths::is_mounted_or_translocated(&exe) {
        return None;
    }
    resolution
        .is_err()
        .then(|| exe.to_string_lossy().into_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentStatus {
    binary: String,
    version: Option<String>,
    reason: Option<HookBinaryReason>,
    /// Whether `binary` is a Homebrew install, decided when the status was
    /// recorded so composing a notice touches nothing.
    homebrew: bool,
}

/// One binary an agent's hooks were seen to run, and when its lines last
/// arrived.
#[derive(Debug, Clone)]
struct Tracked {
    status: AgentStatus,
    last_seen: Instant,
}

/// What startup learned about one trusted pin, through the filesystem and
/// outside any lock, so a hook line naming the same path is classified from
/// this rather than by resolving the path again.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TrustedPin {
    binary: String,
    /// `binary` resolved at startup, which is how the pinned binary spells
    /// itself in its own hook lines' `deck_exe`.
    resolved: Option<String>,
    is_self: bool,
    homebrew: bool,
}

impl TrustedPin {
    /// `binary` from an agent's config, resolved through the filesystem and
    /// compared with this deck.
    fn classify(deck: &DeckIdentity, binary: &str) -> Self {
        let resolved = resolve(Path::new(binary));
        Self {
            binary: binary.to_string(),
            resolved: resolved
                .as_deref()
                .map(|path| path.to_string_lossy().into_owned()),
            is_self: deck.exe.is_some() && resolved.as_ref() == deck.exe.as_ref(),
            homebrew: is_homebrew_spelling(binary)
                || resolved.as_deref().is_some_and(has_cellar_component),
        }
    }

    /// Whether a hook line's `deck_exe` names this pin. Lexical.
    fn names(&self, exe: &str) -> bool {
        self.binary == exe || self.resolved.as_deref() == Some(exe)
    }
}

/// The most binaries the daemon tracks for one agent's hooks, pins included.
/// An agent's hooks can run more than one copy at once (Codex pins per event,
/// OpenCode per root), but a handful; a sender naming a new path on every line
/// replaces the longest-unseen one rather than growing the state.
pub const MAX_BINARIES_PER_AGENT: usize = 8;

/// How long a binary's hook lines may stop before a line for the same agent
/// from another binary clears that binary's notice, for a binary no agent
/// config pins. Lines from two binaries for one agent are normal — Codex's own
/// `wrap` beside its hooks, or hooks pinned to different copies per event — so
/// another binary's line alone never clears a notice; a notice whose unpinned
/// binary has gone quiet this long is one the user has fixed. A pinned
/// binary's notice does not clear by quiet time at all (see
/// [`HOOK_PIN_REFRESH_INTERVAL`]).
pub const HOOK_BINARY_QUIET_AFTER: Duration = Duration::from_secs(5 * 60);

/// How often a steady stream of lines from one binary refreshes when it was
/// last seen, which takes the daemon's write lock; every other line from it is
/// settled under the read lock. A recorded `last_seen` can therefore trail the
/// binary's last line by up to this much, which [`CLEAR_AFTER`] allows for.
const SEEN_REFRESH: Duration = Duration::from_secs(30);

/// How long after a binary's recorded `last_seen` another binary's line clears
/// its notice: [`HOOK_BINARY_QUIET_AFTER`] plus the [`SEEN_REFRESH`] a recorded
/// time can trail the binary's last line by, so a notice never clears before
/// its binary has actually been quiet for [`HOOK_BINARY_QUIET_AFTER`].
const CLEAR_AFTER: Duration = HOOK_BINARY_QUIET_AFTER.saturating_add(SEEN_REFRESH);

/// How often the daemon reads the agents' hook configs again for the binaries
/// they name ([`PinRefresh`]). A notice about a binary a config names clears
/// once a read shows no config names it any more, so this is how long a fixed
/// config can go on showing the old copy's notice, and how long a newly pinned
/// older copy can go unnoticed.
pub const HOOK_PIN_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// One pin a refresh read, classified outside any lock.
#[derive(Debug, Clone)]
struct RefreshedPin {
    trusted: TrustedPin,
    /// `--version`, for a pin that is not this deck.
    probe: Option<Result<String, String>>,
}

/// What one read of the agents' hook configs found (issue #1637): for each
/// agent whose configs were read, the binaries they name, resolved and probed
/// through the filesystem before the daemon takes its state lock, so
/// [`HookBinaryState::apply_refresh`] applies it under that lock without
/// touching the filesystem. An agent whose configs could not be read is
/// absent, which keeps its pins as they were.
#[derive(Debug, Clone, Default)]
pub struct PinRefresh {
    agents: Vec<(AgentType, Vec<RefreshedPin>)>,
}

impl PinRefresh {
    /// Read every agent's hook configs ([`crate::agent_registry::AgentSpec::configured_pins`])
    /// and [`Self::classify`] what they name. Reads only: nothing is installed,
    /// written or taken over. Filesystem work and `--version` probes, so it
    /// runs off the daemon's state lock.
    pub fn collect(deck: &DeckIdentity) -> Self {
        let read = crate::agent_registry::ALL
            .iter()
            .filter_map(|spec| {
                let read = spec.configured_pins?;
                read().map(|pins| (spec.agent_type.clone(), pins))
            })
            .collect();
        Self::classify(deck, read)
    }

    /// `read` holds, for each agent whose configs were read, the pins they
    /// name; an empty list is evidence that none is named. Each pin is
    /// resolved and classified as at startup, and a pin that is not this deck
    /// is probed with `--version` through the shared cache and budget, so a
    /// pin whose file has not changed spawns nothing. A pin that is not an
    /// absolute path is passed over, since probing it would run whatever the
    /// name finds.
    pub(crate) fn classify(deck: &DeckIdentity, read: Vec<(AgentType, Vec<HookPin>)>) -> Self {
        let agents = read
            .into_iter()
            .map(|(agent, pins)| {
                let mut fresh: Vec<RefreshedPin> = Vec::new();
                for pin in pins {
                    if fresh.len() >= MAX_BINARIES_PER_AGENT
                        || !Path::new(&pin.binary).is_absolute()
                        || fresh.iter().any(|known| known.trusted.binary == pin.binary)
                    {
                        continue;
                    }
                    let trusted = TrustedPin::classify(deck, &pin.binary);
                    let probe = (!trusted.is_self).then(|| probe_version(Path::new(&pin.binary)));
                    fresh.push(RefreshedPin { trusted, probe });
                }
                (agent, fresh)
            })
            .collect();
        Self { agents }
    }
}

/// The daemon's record of which binaries each agent's hooks run, and the
/// notices that follow from it.
///
/// Statuses are kept per agent AND binary, so two binaries sending lines for
/// one agent each keep their own status: a line from one never clears the
/// other's notice while the other is still sending (see
/// [`HOOK_BINARY_QUIET_AFTER`]).
///
/// Only [`Self::from_startup`] touches the filesystem, and [`PinRefresh::collect`]
/// outside the state. [`Self::observe`] and [`Self::apply_refresh`] run under
/// the daemon's state lock, [`Self::observe`] on producer-asserted data, and
/// [`Self::notices`] on every `Hello`, so all three are pure: a hook line's path is
/// compared lexically against this deck's own path and the trusted pins, never
/// resolved (issue #1637 audit A2).
#[derive(Debug, Clone, Default)]
pub struct HookBinaryState {
    deck: DeckIdentity,
    pins: HashMap<AgentType, Vec<TrustedPin>>,
    statuses: HashMap<AgentType, Vec<Tracked>>,
    ephemeral: Option<String>,
    warned: BTreeSet<(String, HookBinaryReason)>,
    warnings_suppressed: bool,
}

/// What agents share to be listed in one notice: binary, reason, version.
type NoticeKey = (String, HookBinaryReason, Option<String>);

/// The order agents are listed in a notice.
fn agent_order(agent: &AgentType) -> usize {
    crate::agent_registry::ALL
        .iter()
        .position(|spec| &spec.agent_type == agent)
        .unwrap_or(usize::MAX)
}

impl HookBinaryState {
    /// The state a daemon starts with: the installers' `pins` (every distinct
    /// binary for an agent, up to [`MAX_BINARIES_PER_AGENT`]), each pinned
    /// binary that is not the daemon probed with `--version`, and
    /// `ephemeral_exe` when this deck runs from a location it refused to pin
    /// (issue #1157).
    pub fn from_startup(
        deck: DeckIdentity,
        pins: &[HookPin],
        ephemeral_exe: Option<String>,
    ) -> Self {
        let mut state = Self {
            deck,
            ..Self::default()
        };
        let now = Instant::now();
        for pin in pins {
            let known = state.pins.entry(pin.agent.clone()).or_default();
            if known.len() >= MAX_BINARIES_PER_AGENT
                || known.iter().any(|known| known.binary == pin.binary)
            {
                continue;
            }
            let trusted = TrustedPin::classify(&state.deck, &pin.binary);
            known.push(trusted.clone());
            if trusted.is_self {
                continue;
            }
            let status = state.probed_status(&trusted, probe_version(Path::new(&pin.binary)));
            state.record(&pin.agent, status, now);
        }
        if let Some(exe) = ephemeral_exe {
            state.warn_once(&exe, HookBinaryReason::EphemeralLocation, &[]);
            state.ephemeral = Some(exe);
        }
        state
    }

    /// Who this daemon is, for [`PinRefresh::collect`].
    pub fn deck(&self) -> &DeckIdentity {
        &self.deck
    }

    /// The status a pin's `--version` probe implies.
    fn probed_status(&self, pin: &TrustedPin, probe: Result<String, String>) -> AgentStatus {
        let (version, reason) = match probe {
            Ok(version) if is_newer_release(&self.deck.version, &version) => {
                (Some(version), Some(HookBinaryReason::Older))
            }
            Ok(version) => (Some(version), None),
            Err(_) => (None, Some(HookBinaryReason::Unprobeable)),
        };
        AgentStatus {
            binary: pin.binary.clone(),
            version,
            reason,
            homebrew: pin.homebrew,
        }
    }

    /// Apply a [`PinRefresh`]. Returns whether the notices changed.
    ///
    /// For each agent it read, the agent's pins become the ones its configs
    /// name now. A status for a binary that was pinned and no longer is
    /// clears, and so does one for a pin that is now this deck. A new pin is
    /// recorded from its probe, as at startup, so an older copy pinned
    /// mid-session raises its notice; a pin already known takes its probe
    /// only when the probe reports a different version than the one recorded,
    /// which is a file replaced in place, so a failed probe or an unchanged
    /// file leaves what its own lines said. A line-learned status spelled as a
    /// new pin resolves becomes that pin's. An agent the refresh could not
    /// read is left as it was.
    ///
    /// Pure, like [`Self::observe`]: everything the filesystem had to say was
    /// gathered by [`PinRefresh::collect`].
    pub fn apply_refresh(&mut self, refresh: PinRefresh) -> bool {
        self.apply_refresh_at(refresh, Instant::now())
    }

    fn apply_refresh_at(&mut self, refresh: PinRefresh, now: Instant) -> bool {
        let before = self.notices();
        for (agent, fresh) in refresh.agents {
            let previous = self.pins.remove(&agent).unwrap_or_default();
            let pins: Vec<TrustedPin> = fresh.iter().map(|pin| pin.trusted.clone()).collect();
            if let Some(tracked) = self.statuses.get_mut(&agent) {
                tracked.retain(|tracked| {
                    let binary = &tracked.status.binary;
                    let pinned = pins.iter().find(|pin| &pin.binary == binary);
                    let was_pinned = previous.iter().any(|pin| &pin.binary == binary);
                    !(was_pinned && pinned.is_none()) && !pinned.is_some_and(|pin| pin.is_self)
                });
                for pin in &pins {
                    if tracked
                        .iter()
                        .any(|known| known.status.binary == pin.binary)
                    {
                        continue;
                    }
                    if let Some(learned) = tracked
                        .iter_mut()
                        .find(|known| pin.names(&known.status.binary))
                    {
                        learned.status.binary = pin.binary.clone();
                        learned.status.homebrew = pin.homebrew;
                    }
                }
            }
            for RefreshedPin { trusted, probe } in fresh {
                let Some(probe) = probe else { continue };
                let known = self
                    .statuses
                    .get(&agent)
                    .and_then(|tracked| {
                        tracked
                            .iter()
                            .find(|tracked| tracked.status.binary == trusted.binary)
                    })
                    .map(|tracked| tracked.status.version.clone());
                let apply = match (&known, &probe) {
                    (None, _) => true,
                    (Some(recorded), Ok(version)) => recorded.as_ref() != Some(version),
                    (Some(_), Err(_)) => false,
                };
                if apply {
                    let status = self.probed_status(&trusted, probe);
                    self.record(&agent, status, now);
                }
            }
            self.pins.insert(agent, pins);
        }
        self.notices() != before
    }

    /// Record what a hook line from `agent` says about the binary that sent
    /// it. Returns whether the notices changed.
    ///
    /// Pure: no filesystem call, whatever path the line names. A stamp that
    /// fails validation ([`is_valid_deck_build`], [`is_valid_deck_exe`]) is
    /// ignored — an unusable build changes nothing, an unusable path reads as
    /// absent — and the caller still applies the event. The line updates only
    /// its own binary's status; another binary's notice for the same agent is
    /// cleared only once that binary has sent nothing for
    /// [`HOOK_BINARY_QUIET_AFTER`], and never while an agent's config pins
    /// it ([`Self::apply_refresh`] clears that one).
    pub fn observe(&mut self, agent: &AgentType, sender: &HookLineSender) -> bool {
        self.observe_at(agent, sender, Instant::now())
    }

    /// Whether [`Self::observe`] would record anything for this line, so a
    /// caller can skip taking a write lock for a line that changes nothing —
    /// the steady state, where an agent's hooks keep sending the same stamp.
    pub fn would_change(&self, agent: &AgentType, sender: &HookLineSender) -> bool {
        self.would_change_at(agent, sender, Instant::now())
    }

    fn observe_at(&mut self, agent: &AgentType, sender: &HookLineSender, now: Instant) -> bool {
        let next = self.next_statuses(agent, sender);
        if next.is_empty() {
            return false;
        }
        // Compared as clients receive them, capped at MAX_NOTICES, so a change
        // beyond the cap does not broadcast a list identical to the last one.
        let before = self.notices();
        let seen: Vec<String> = next.iter().map(|status| status.binary.clone()).collect();
        for status in next {
            self.record(agent, status, now);
        }
        let pins = self.pins.get(agent).map_or(&[][..], Vec::as_slice);
        if let Some(tracked) = self.statuses.get_mut(agent) {
            tracked.retain(|tracked| !Self::cleared_by(tracked, &seen, pins, now));
        }
        self.notices() != before
    }

    fn would_change_at(&self, agent: &AgentType, sender: &HookLineSender, now: Instant) -> bool {
        let next = self.next_statuses(agent, sender);
        if next.is_empty() {
            return false;
        }
        let tracked = self.statuses.get(agent).map_or(&[][..], Vec::as_slice);
        let pins = self.pins.get(agent).map_or(&[][..], Vec::as_slice);
        let seen: Vec<String> = next.iter().map(|status| status.binary.clone()).collect();
        next.iter().any(|status| {
            tracked
                .iter()
                .find(|tracked| tracked.status.binary == status.binary)
                .is_none_or(|tracked| {
                    tracked.status != *status
                        || now.saturating_duration_since(tracked.last_seen) >= SEEN_REFRESH
                })
        }) || tracked
            .iter()
            .any(|tracked| Self::cleared_by(tracked, &seen, pins, now))
    }

    /// Whether a line for the same agent from `seen` clears `tracked`: it
    /// raised a notice, is another binary, no config of the agent's pins it,
    /// and it has been quiet for at least [`HOOK_BINARY_QUIET_AFTER`]
    /// (measured as [`CLEAR_AFTER`] from its recorded `last_seen`).
    ///
    /// A pinned binary's quiet proves nothing: a Codex hook pinned to an old
    /// copy for one event sends nothing until that event happens, while the
    /// pane's current `wrap` keeps sending. Its notice clears when its own
    /// line reports a current build, or when [`Self::apply_refresh`] finds no
    /// config naming it. The quiet rule is for a binary only hook lines named
    /// (a `DOT_AGENT_DECK_BIN` override, a project's own settings), which no
    /// config the deck reads can vouch for either way.
    fn cleared_by(tracked: &Tracked, seen: &[String], pins: &[TrustedPin], now: Instant) -> bool {
        tracked.status.reason.is_some()
            && !seen.contains(&tracked.status.binary)
            && !pins.iter().any(|pin| pin.binary == tracked.status.binary)
            && now.saturating_duration_since(tracked.last_seen) >= CLEAR_AFTER
    }

    /// The statuses a hook line from `agent` implies, one per binary it is
    /// about, or none when the line says nothing. Pure.
    fn next_statuses(&self, agent: &AgentType, sender: &HookLineSender) -> Vec<AgentStatus> {
        let pins = self.pins.get(agent).map_or(&[][..], Vec::as_slice);
        let tracked = self.statuses.get(agent).map_or(&[][..], Vec::as_slice);
        if let Some(build) = sender.deck_build.as_deref() {
            if !is_valid_deck_build(build) {
                return Vec::new();
            }
            let exe = sender
                .deck_exe
                .as_deref()
                .filter(|exe| is_valid_deck_exe(exe));
            // A pinned binary's line is recorded under the pin's spelling, so
            // startup's probe and the binary's own lines are one status. A line
            // with no usable path for an agent without exactly one pin names
            // no binary, so like an unusable build it says nothing.
            let binary = match exe {
                Some(exe) => pins
                    .iter()
                    .find(|pin| pin.names(exe))
                    .map_or(exe, |pin| pin.binary.as_str())
                    .to_string(),
                None => match pins {
                    [pin] => pin.binary.clone(),
                    _ => return Vec::new(),
                },
            };
            let version = release_of_build(build).to_string();
            let is_self = exe.is_some_and(|exe| self.is_self_spelling(exe));
            let reason = (!is_self && is_newer_release(&self.deck.version, &version))
                .then_some(HookBinaryReason::Older);
            let homebrew = self.is_homebrew(&binary);
            vec![AgentStatus {
                binary,
                version: Some(version),
                reason,
                homebrew,
            }]
        } else {
            // A stamp-less line names nobody, so it is read against the pins:
            // only an agent pinned to another binary is affected, and a line
            // sent by something other than its hook (a test, a script) leaves a
            // deck-pinned agent alone. With several such pins, the line is
            // from one of them, but which is unknown, so it is every one not
            // already known to be current.
            //
            // This assumes every current sender stamps its lines: `hook`,
            // `agent-event` and `wrap` all build them through
            // `agent_event_line`, which calls [`stamp_hook_line`]. A stamp-less
            // line injected by hand for an agent pinned to a current install at
            // another path would therefore raise a spurious `Unreported`.
            pins.iter()
                .filter(|pin| !pin.is_self)
                .filter_map(|pin| {
                    let known = tracked
                        .iter()
                        .find(|tracked| tracked.status.binary == pin.binary);
                    if known.is_some_and(|tracked| tracked.status.reason.is_none()) {
                        return None;
                    }
                    Some(AgentStatus {
                        binary: pin.binary.clone(),
                        version: known.and_then(|tracked| tracked.status.version.clone()),
                        reason: Some(HookBinaryReason::Unreported),
                        homebrew: pin.homebrew,
                    })
                })
                .collect()
        }
    }

    /// Whether `binary` is this deck: its own spelling, or a trusted pin
    /// startup resolved to it. Lexical.
    fn is_self_spelling(&self, binary: &str) -> bool {
        self.deck.is_self_spelling(binary)
            || self
                .pins
                .values()
                .flatten()
                .any(|pin| pin.is_self && pin.names(binary))
    }

    /// Whether `binary` is a Homebrew install: what startup decided for a
    /// trusted pin of that spelling, else [`is_homebrew_spelling`]. Lexical.
    fn is_homebrew(&self, binary: &str) -> bool {
        self.pins
            .values()
            .flatten()
            .find(|pin| pin.names(binary))
            .map_or_else(|| is_homebrew_spelling(binary), |pin| pin.homebrew)
    }

    /// Record `status` as the status of its binary for `agent`, seen at `now`.
    /// A binary not yet tracked replaces the agent's longest-unseen one once
    /// the agent has [`MAX_BINARIES_PER_AGENT`].
    fn record(&mut self, agent: &AgentType, status: AgentStatus, now: Instant) {
        let tracked = self.statuses.entry(agent.clone()).or_default();
        let warn = match tracked
            .iter_mut()
            .find(|tracked| tracked.status.binary == status.binary)
        {
            Some(existing) => {
                existing.last_seen = now;
                let changed = existing.status != status;
                existing.status = status.clone();
                changed
            }
            None => {
                if tracked.len() >= MAX_BINARIES_PER_AGENT
                    && let Some(oldest) = tracked
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, tracked)| tracked.last_seen)
                        .map(|(index, _)| index)
                {
                    tracked.remove(oldest);
                }
                tracked.push(Tracked {
                    status: status.clone(),
                    last_seen: now,
                });
                true
            }
        };
        if let (true, Some(reason)) = (warn, status.reason) {
            self.warn_once(&status.binary, reason, &[agent_display_name(agent)]);
        }
    }

    fn warn_once(&mut self, binary: &str, reason: HookBinaryReason, agents: &[&str]) {
        let key = (binary.to_string(), reason);
        if self.warned.contains(&key) {
            return;
        }
        if self.warned.len() >= MAX_WARNED_BINARIES {
            if !self.warnings_suppressed {
                self.warnings_suppressed = true;
                tracing::warn!(
                    limit = MAX_WARNED_BINARIES,
                    "agent hooks named more distinct dot-agent-deck binaries than the deck logs; \
                     further ones are not logged, and the dashboard notice still shows the current \
                     ones"
                );
            }
            return;
        }
        self.warned.insert(key);
        tracing::warn!(
            binary,
            reason = ?reason,
            agents = ?agents,
            daemon_version = %self.deck.version,
            "agent hooks run a dot-agent-deck that is not this deck's release; the dashboard \
             shows a notice with what to do"
        );
    }

    /// The statuses that raise a notice, in the order [`Self::notices`] lists
    /// them.
    fn reasoned(&self) -> Vec<(&AgentType, &AgentStatus, HookBinaryReason)> {
        let mut agents: Vec<(&AgentType, &Vec<Tracked>)> = self.statuses.iter().collect();
        agents.sort_by_key(|(agent, _)| agent_order(agent));
        agents
            .into_iter()
            .flat_map(|(agent, tracked)| {
                tracked.iter().filter_map(move |tracked| {
                    tracked
                        .status
                        .reason
                        .map(|reason| (agent, &tracked.status, reason))
                })
            })
            .collect()
    }

    /// The notices both clients show, grouped by binary, reason and version,
    /// at most [`MAX_NOTICES`]. Pure: everything it needs was decided when the
    /// status was recorded.
    pub fn notices(&self) -> Vec<HookBinaryNotice> {
        let mut groups: Vec<(NoticeKey, bool, Vec<AgentType>)> = Vec::new();
        for (agent, status, reason) in self.reasoned() {
            let key = (status.binary.clone(), reason, status.version.clone());
            match groups.iter_mut().find(|(k, _, _)| *k == key) {
                Some((_, homebrew, members)) => {
                    *homebrew |= status.homebrew;
                    if !members.contains(agent) {
                        members.push(agent.clone());
                    }
                }
                None => groups.push((key, status.homebrew, vec![agent.clone()])),
            }
        }
        let mut notices: Vec<HookBinaryNotice> = Vec::new();
        if let Some(exe) = &self.ephemeral {
            notices.push(HookBinaryNotice {
                binary: exe.clone(),
                agents: Vec::new(),
                version: None,
                daemon_version: self.deck.version.clone(),
                reason: HookBinaryReason::EphemeralLocation,
                remedy: crate::platform::paths::MOVE_TO_APPLICATIONS_ADVICE.to_string(),
                command: None,
            });
        }
        notices.extend(
            groups
                .into_iter()
                .map(|((binary, reason, version), homebrew, members)| {
                    let Remedy { text, command } = remedy_for(&self.deck, homebrew, &members);
                    HookBinaryNotice {
                        binary,
                        agents: members
                            .iter()
                            .map(|agent| agent_display_name(agent).to_string())
                            .collect(),
                        version,
                        daemon_version: self.deck.version.clone(),
                        reason,
                        remedy: text,
                        command,
                    }
                }),
        );
        // A pin read from an agent's config, this deck's own path and a probed
        // version are not otherwise bounded or checked, and a notice should be.
        sanitize_notices(&notices)
    }
}

/// Whether `path` has a `Cellar` component, where Homebrew keeps its kegs.
fn has_cellar_component(path: &Path) -> bool {
    path.components()
        .any(|c| c == std::path::Component::Normal("Cellar".as_ref()))
}

/// Whether `binary` is spelled as a Homebrew install, without touching the
/// filesystem: a path into a `Cellar` keg, or under Homebrew's Apple silicon
/// or Linux prefix. A trusted pin is also resolved at startup, which catches a
/// `/usr/local/bin` link into an Intel keg; a hook line's path is not. Wrongly
/// saying Homebrew costs a `brew upgrade` suggestion, a fixed command.
fn is_homebrew_spelling(binary: &str) -> bool {
    let path = Path::new(binary);
    has_cellar_component(path)
        || path.starts_with("/opt/homebrew")
        || path.starts_with("/home/linuxbrew/.linuxbrew")
}

/// What a notice tells the user to do: words, and the command when the fix is
/// one. See [`HookBinaryNotice::remedy`] and [`HookBinaryNotice::command`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remedy {
    pub text: String,
    pub command: Option<String>,
}

/// The lead-in a remedy with a command has.
pub const REMEDY_RUN: &str = "Run:";

/// The remedy when the deck can offer no command.
pub const REMEDY_UPGRADE_OR_REINSTALL: &str = "Upgrade the dot-agent-deck the hooks run, or run \
     `hooks install` from the copy you want the hooks to use.";

/// What the user does about hooks that run another binary (issue #1637 F3):
/// run `hooks install` from this deck when this deck would pin itself, else
/// upgrade the copy the hooks run, through Homebrew when that is where it came
/// from (`homebrew`). The command is composed only from this deck's own
/// install path and fixed text, never from anything a hook line sent, for this
/// platform's default shell ([`Shell::CURRENT`]).
pub fn remedy_for(deck: &DeckIdentity, homebrew: bool, agents: &[AgentType]) -> Remedy {
    remedy_for_shell(deck, homebrew, agents, Shell::CURRENT)
}

fn remedy_for_shell(
    deck: &DeckIdentity,
    homebrew: bool,
    agents: &[AgentType],
    shell: Shell,
) -> Remedy {
    let run = |command: String| Remedy {
        text: REMEDY_RUN.to_string(),
        command: Some(command),
    };
    if let Some(own) = deck
        .self_install_path
        .as_deref()
        .filter(|own| is_valid_deck_exe(own))
    {
        let commands: Vec<String> = agents
            .iter()
            .filter_map(agent_cli_name)
            .map(|agent| format!("{} hooks install --agent {agent}", shell.program(own)))
            .collect();
        let command = commands.join(shell.separator());
        if !command.is_empty() && command.len() <= MAX_NOTICE_COMMAND_BYTES {
            return run(command);
        }
    }
    if homebrew {
        return run("brew upgrade dot-agent-deck".to_string());
    }
    Remedy {
        text: REMEDY_UPGRADE_OR_REINSTALL.to_string(),
        command: None,
    }
}

/// The shell a notice's command is written for: the one a user pastes it into
/// by default on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shell {
    /// `sh` and its relatives: a path in single quotes when it needs any, and
    /// commands chained with `&&`.
    Posix,
    /// Windows PowerShell: a quoted program runs only through the call
    /// operator `&`, a single quote (typographic ones included) inside single
    /// quotes is doubled, and
    /// commands are separated with `;`, since Windows PowerShell 5.1 (the one
    /// Windows ships) has no `&&`.
    PowerShell,
}

impl Shell {
    const CURRENT: Shell = if cfg!(windows) {
        Shell::PowerShell
    } else {
        Shell::Posix
    };

    /// `path` as the program word(s) of a command in this shell.
    fn program(self, path: &str) -> String {
        match self {
            Shell::Posix => posix_word(path),
            Shell::PowerShell => {
                // PowerShell reads the typographic single quotes as `'` too.
                let mut quoted = String::from("& '");
                for c in path.chars() {
                    quoted.push(c);
                    if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
                        quoted.push(c);
                    }
                }
                quoted.push('\'');
                quoted
            }
        }
    }

    /// What joins two commands.
    fn separator(self) -> &'static str {
        match self {
            Shell::Posix => " && ",
            Shell::PowerShell => "; ",
        }
    }
}

/// `path` as one POSIX shell word: unchanged when it needs no quoting, else in
/// single quotes.
fn posix_word(path: &str) -> String {
    if !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+~:@".contains(c))
    {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `path`, written Unix-style, as an absolute path on this platform:
    /// unchanged on Unix, and under `C:\` with backslashes on Windows, where
    /// `/home/u/…` is not absolute and so is not a `deck_exe` the daemon reads.
    fn abs(path: &str) -> String {
        if cfg!(windows) {
            format!("C:{}", path.replace('/', "\\"))
        } else {
            path.to_string()
        }
    }

    #[test]
    fn release_of_build_strips_the_build_stamp() {
        assert_eq!(release_of_build("0.46.0-g1a8e0de3"), "0.46.0");
        assert_eq!(release_of_build("0.46.0-g1a8e0de-dirty"), "0.46.0");
        assert_eq!(release_of_build("0.46.0-unknown"), "0.46.0");
        assert_eq!(release_of_build("0.25.0-alpha.0-gabc123"), "0.25.0-alpha.0");
        assert_eq!(release_of_build("0.46.0"), "0.46.0");
        // A pre-release whose own name starts with `g` is not a sha.
        assert_eq!(release_of_build("1.0.0-gamma"), "1.0.0-gamma");
    }

    /// Scenario: the compare table. A newer triple wins, the build stamp is
    /// ignored, a pre-release is older than its release, an `-unknown` build
    /// compares as its release, and an equal triple with a different build is a
    /// tie (issue #1637 contract decision 3 dropped `DifferentBuild`).
    #[test]
    fn the_compare_table() {
        assert!(is_newer_release("0.47.0", "0.46.0"));
        assert!(!is_newer_release("0.46.0", "0.47.0"));
        assert!(!is_newer_release("0.46.0", "0.46.0"), "a tie is not newer");
        assert!(is_newer_release("v0.46.1", "0.46.0"));
        assert!(!is_newer_release("0.46.0-g1a8e0de3", "0.46.0"));
        assert!(is_newer_release("0.46.0", "0.46.0-rc.1"));
        assert!(!is_newer_release(
            release_of_build("0.46.0-unknown"),
            "0.46.0"
        ));
        assert!(!is_newer_release(
            "0.46.0",
            release_of_build("0.46.0-gdeadbee")
        ));
        assert!(!is_newer_release("garbage", "0.1.0"));
        assert!(!is_newer_release("0.1.0", "garbage"));
    }

    #[test]
    fn a_hook_line_stamp_round_trips_and_is_absent_from_an_old_line() {
        let mut map = serde_json::Map::new();
        map.insert("session_id".into(), "s".into());
        stamp_hook_line(&mut map);
        let line = serde_json::Value::Object(map).to_string();
        let sender = HookLineSender::from_line(&line);
        assert_eq!(
            sender.deck_build.as_deref(),
            Some(crate::build_id::local_build_id().as_str())
        );
        assert!(sender.is_reported());
        let old = HookLineSender::from_line(r#"{"session_id":"s"}"#);
        assert!(!old.is_reported());
        assert_eq!(old, HookLineSender::default());
        let wrong_type = HookLineSender::from_line(r#"{"deck_build":7}"#);
        assert!(!wrong_type.is_reported());
    }

    #[test]
    fn takeover_is_ineligible_by_default_in_unit_tests_and_overridable_per_thread() {
        assert_eq!(Takeover::current(), Takeover::ineligible());
        with_takeover(Takeover::eligible("1.0.0"), || {
            assert_eq!(Takeover::current(), Takeover::eligible("1.0.0"));
        });
        assert_eq!(Takeover::current(), Takeover::ineligible());
    }

    /// Scenario: only a Running copy whose installed name AND running file
    /// are both outside known-ephemeral locations is takeover-eligible.
    #[test]
    fn only_a_running_copy_outside_ephemeral_locations_is_takeover_eligible() {
        use crate::platform::paths::ResolutionArm;
        let durable = Path::new("/opt/homebrew/bin/dot-agent-deck");
        let keg = Path::new("/opt/homebrew/Cellar/dot-agent-deck/0.47.0/bin/dot-agent-deck");
        assert!(takeover_eligible(ResolutionArm::Running, durable, keg));
        assert!(takeover_eligible(ResolutionArm::Running, durable, durable));
        assert!(!takeover_eligible(ResolutionArm::Install, durable, durable));
        assert!(!takeover_eligible(
            ResolutionArm::LastResort,
            durable,
            durable
        ));
        let scratch = Path::new("/var/tmp/dad-branch/bin/dot-agent-deck");
        assert!(!takeover_eligible(ResolutionArm::Running, scratch, scratch));
        let mounted = Path::new("/Volumes/Agent Deck/Agent Deck.app/Contents/MacOS/dot-agent-deck");
        assert!(!takeover_eligible(ResolutionArm::Running, mounted, mounted));
    }

    /// Scenario: `~/.local/bin/dot-agent-deck` is a symlink to a scratch copy
    /// in `/var/tmp`, and that scratch copy is what runs. The installed name
    /// looks durable, but the file the hooks would run disappears with the
    /// scratch directory, so the copy is not takeover-eligible (audit A4).
    #[test]
    fn an_installed_link_to_a_scratch_copy_is_not_takeover_eligible() {
        use crate::platform::paths::ResolutionArm;
        let installed = Path::new("/home/u/.local/bin/dot-agent-deck");
        assert!(takeover_eligible(
            ResolutionArm::Running,
            installed,
            installed
        ));
        for running in [
            "/var/tmp/dad-branch/bin/dot-agent-deck",
            "/tmp/dad-branch/dot-agent-deck",
            "/Volumes/Agent Deck/Agent Deck.app/Contents/MacOS/dot-agent-deck",
        ] {
            assert!(
                !takeover_eligible(ResolutionArm::Running, installed, Path::new(running)),
                "{running}"
            );
        }
        let in_temp = std::env::temp_dir()
            .join("dad-branch")
            .join("dot-agent-deck");
        assert!(!takeover_eligible(
            ResolutionArm::Running,
            installed,
            &in_temp
        ));
    }

    /// Scenario: `~/.local/bin` has no exemption. Under a home on a mounted
    /// volume, a translocated location or the temp directory, an install there
    /// is as temporary as the home, so it is not takeover-eligible (audit A4).
    #[test]
    fn a_local_bin_under_a_mounted_translocated_or_temporary_home_is_not_eligible() {
        use crate::platform::paths::ResolutionArm;
        for home in [
            PathBuf::from("/Volumes/External/Users/u"),
            PathBuf::from("/private/var/folders/xy/T/AppTranslocation/3F2A/d/home"),
            std::env::temp_dir().join("h"),
            PathBuf::from("/var/tmp/h"),
        ] {
            let installed = home.join(".local").join("bin").join("dot-agent-deck");
            assert!(
                !takeover_eligible(ResolutionArm::Running, &installed, &installed),
                "{}",
                installed.display()
            );
        }
    }

    #[test]
    fn the_remedy_depends_on_where_this_deck_and_the_hook_binary_are() {
        let own_path = abs("/home/u/.local/bin/dot-agent-deck");
        let own = DeckIdentity {
            exe: Some(PathBuf::from(&own_path)),
            version: "0.47.0".into(),
            self_install_path: Some(own_path.clone()),
        };
        #[cfg(unix)]
        let expected = "/home/u/.local/bin/dot-agent-deck hooks install --agent claude-code && \
                        /home/u/.local/bin/dot-agent-deck hooks install --agent codex";
        #[cfg(windows)]
        let expected = "& 'C:\\home\\u\\.local\\bin\\dot-agent-deck' hooks install --agent claude-code; \
                        & 'C:\\home\\u\\.local\\bin\\dot-agent-deck' hooks install --agent codex";
        assert_eq!(
            remedy_for(&own, false, &[AgentType::ClaudeCode, AgentType::Codex]),
            Remedy {
                text: "Run:".into(),
                command: Some(expected.into()),
            }
        );
        let other = DeckIdentity {
            self_install_path: None,
            ..own.clone()
        };
        assert_eq!(
            remedy_for(&other, false, &[AgentType::ClaudeCode]),
            Remedy {
                text: "Upgrade the dot-agent-deck the hooks run, or run `hooks install` from \
                       the copy you want the hooks to use."
                    .into(),
                command: None,
            }
        );
        assert_eq!(
            remedy_for(&other, true, &[AgentType::ClaudeCode]),
            Remedy {
                text: "Run:".into(),
                command: Some("brew upgrade dot-agent-deck".into()),
            }
        );
        // This deck's own path is quoted as one shell word.
        let spaced = DeckIdentity {
            self_install_path: Some(abs("/Applications/Agent Deck.app/x/dot-agent-deck")),
            ..own
        };
        #[cfg(unix)]
        let expected =
            "'/Applications/Agent Deck.app/x/dot-agent-deck' hooks install --agent codex";
        #[cfg(windows)]
        let expected =
            "& 'C:\\Applications\\Agent Deck.app\\x\\dot-agent-deck' hooks install --agent codex";
        assert_eq!(
            remedy_for(&spaced, false, &[AgentType::Codex])
                .command
                .as_deref(),
            Some(expected)
        );
    }

    /// Scenario (Greptile on #1656): the command a notice offers is written
    /// for the platform's default shell. In a POSIX shell a path that needs
    /// quoting is single-quoted with `'\''` for a quote, and two commands are
    /// chained with `&&`; in Windows PowerShell the path is single-quoted with
    /// every quote doubled (typographic ones too), run through the call
    /// operator `&`, and two commands are separated with `;`.
    #[test]
    fn the_command_is_quoted_for_the_platform_shell() {
        assert_eq!(Shell::Posix.program("/a/b"), "/a/b");
        assert_eq!(Shell::Posix.program("/a b/x"), "'/a b/x'");
        assert_eq!(Shell::Posix.program("/a'b"), r"'/a'\''b'");
        assert_eq!(Shell::Posix.separator(), " && ");
        assert_eq!(
            Shell::PowerShell.program(r"C:\Program Files\Agent Deck\dot-agent-deck.exe"),
            r"& 'C:\Program Files\Agent Deck\dot-agent-deck.exe'"
        );
        assert_eq!(
            Shell::PowerShell.program(r"C:\Users\o'brien\dot-agent-deck.exe"),
            r"& 'C:\Users\o''brien\dot-agent-deck.exe'"
        );
        assert_eq!(
            Shell::PowerShell.program("C:\\a\u{2019}b\\x.exe"),
            "& 'C:\\a\u{2019}\u{2019}b\\x.exe'"
        );
        assert_eq!(Shell::PowerShell.separator(), "; ");
        assert_eq!(
            Shell::CURRENT,
            if cfg!(windows) {
                Shell::PowerShell
            } else {
                Shell::Posix
            }
        );

        let deck = DeckIdentity {
            exe: None,
            version: "0.47.0".into(),
            self_install_path: Some(abs("/x/dot-agent-deck")),
        };
        let agents = [AgentType::ClaudeCode, AgentType::Codex];
        let own = abs("/x/dot-agent-deck");
        assert_eq!(
            remedy_for_shell(&deck, false, &agents, Shell::PowerShell).command,
            Some(format!(
                "& '{own}' hooks install --agent claude-code; & '{own}' hooks install --agent codex"
            ))
        );
    }

    /// Scenario (Greptile on #1656): on Windows, the command a notice offers
    /// for this deck's own install pastes into PowerShell as is.
    #[cfg(windows)]
    #[test]
    fn a_windows_command_runs_in_powershell() {
        let deck = DeckIdentity {
            exe: None,
            version: "0.47.0".into(),
            self_install_path: Some(r"C:\Users\o'brien\AppData\Local\dot-agent-deck.exe".into()),
        };
        assert_eq!(
            remedy_for(&deck, false, &[AgentType::Codex])
                .command
                .as_deref(),
            Some(
                r"& 'C:\Users\o''brien\AppData\Local\dot-agent-deck.exe' hooks install --agent codex"
            )
        );
    }

    /// Scenario: a hook line claims an old release and a `deck_exe` that tries
    /// to smuggle a second shell command after a newline, or bidi overrides.
    /// The path is dropped at ingest, and no notice's remedy or command ever
    /// carries a hook line's text (audit A3).
    #[test]
    fn a_malicious_deck_exe_never_reaches_a_remedy_or_a_command() {
        let line = serde_json::json!({
            "deck_build": "0.0.1-gabc1234",
            "deck_exe": "/opt/old/dot-agent-deck\n touch /tmp/hook-notice-marker #",
        })
        .to_string();
        let sender = HookLineSender::from_line(&line);
        assert_eq!(sender.deck_exe, None, "a path with a newline is dropped");
        assert_eq!(sender.deck_build.as_deref(), Some("0.0.1-gabc1234"));

        let old = abs("/opt/old");
        for own in [None, Some(abs("/home/u/.local/bin/dot-agent-deck"))] {
            let mut state = HookBinaryState {
                deck: DeckIdentity {
                    self_install_path: own,
                    ..deck("0.46.0")
                },
                ..HookBinaryState::default()
            };
            // Even a sender constructed past `from_line` is validated again.
            // Each is absolute on this platform, so it is refused for what it
            // carries rather than for not being absolute.
            for exe in [
                format!("{old}/dot-agent-deck\n touch /tmp/hook-notice-marker #"),
                format!("{old}/\u{202E}kced-tnega-tod"),
                format!("{old}/\u{2028}dot-agent-deck"),
                "relative/dot-agent-deck".to_string(),
            ] {
                state.observe(
                    &AgentType::ClaudeCode,
                    &HookLineSender {
                        deck_build: Some("0.0.1-gabc1234".into()),
                        deck_exe: Some(exe),
                    },
                );
                for notice in state.notices() {
                    assert!(!notice.binary.contains("touch"), "{notice:?}");
                    assert!(!notice.binary.contains('\u{202E}'), "{notice:?}");
                    assert!(!notice.remedy.contains(&old), "{notice:?}");
                    if let Some(command) = &notice.command {
                        assert!(!command.contains(&old), "{notice:?}");
                        assert!(!command.chars().any(char::is_control), "{notice:?}");
                    }
                }
            }
        }
    }

    /// Scenario: one notice for hooks that run a path a hook line named. The
    /// remedy is the fixed words, with no path, and no command is offered for
    /// a copy this deck knows nothing about.
    #[test]
    fn a_hook_line_path_is_shown_as_the_binary_and_never_in_the_remedy() {
        let mut state = HookBinaryState {
            deck: deck("0.46.0"),
            ..HookBinaryState::default()
        };
        let old = abs("/opt/old/dot-agent-deck");
        state.observe(
            &AgentType::Codex,
            &HookLineSender {
                deck_build: Some("0.45.0-gabc1234".into()),
                deck_exe: Some(old.clone()),
            },
        );
        let notices = state.notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].binary, old);
        assert_eq!(notices[0].remedy, REMEDY_UPGRADE_OR_REINSTALL);
        assert_eq!(notices[0].command, None);

        // A Homebrew spelling offers the fixed `brew` command. Homebrew's
        // prefixes are Unix paths.
        #[cfg(unix)]
        {
            let homebrew = "/opt/homebrew/bin/dot-agent-deck";
            state.observe(
                &AgentType::Codex,
                &HookLineSender {
                    deck_build: Some("0.45.0-gabc1234".into()),
                    deck_exe: Some(homebrew.into()),
                },
            );
            let notices = state.notices();
            let brew = notices
                .iter()
                .find(|notice| notice.binary == homebrew)
                .expect("the Homebrew copy's notice");
            assert_eq!(brew.remedy, "Run:");
            assert_eq!(brew.command.as_deref(), Some("brew upgrade dot-agent-deck"));
        }
    }

    /// Scenario: stamp validation at ingest. A build id or a path over budget,
    /// or carrying a control or bidi character, is not kept; an unusable build
    /// leaves the line ignored for diagnostics, and an unusable path reads as
    /// absent (audit A1).
    #[test]
    fn over_budget_or_malformed_stamps_are_dropped_at_ingest() {
        let huge_path = format!("/{}", "a".repeat(8 * 1024 * 1024));
        let line = serde_json::json!({
            "deck_build": "0.0.1-gabc1234",
            "deck_exe": huge_path,
        })
        .to_string();
        let sender = HookLineSender::from_line(&line);
        assert_eq!(sender.deck_exe, None, "an 8 MiB path is not kept");
        assert!(sender.is_reported());

        let long_build = "0.0.1-g".to_string() + &"a".repeat(MAX_DECK_BUILD_BYTES);
        for build in [long_build.as_str(), "0.0.1\n", "0.0.1\u{202E}", ""] {
            let line = serde_json::json!({ "deck_build": build }).to_string();
            let sender = HookLineSender::from_line(&line);
            assert_eq!(sender.deck_build.as_deref(), Some(""), "{build:?}");
            let mut state = HookBinaryState {
                deck: deck("0.46.0"),
                ..HookBinaryState::default()
            };
            assert!(!state.observe(&AgentType::ClaudeCode, &sender), "{build:?}");
            assert!(state.notices().is_empty());
            assert!(state.warned.is_empty());
            // Constructed directly, the same build is still ignored.
            let direct = HookLineSender {
                deck_build: Some(build.to_string()),
                deck_exe: None,
            };
            assert!(!state.observe(&AgentType::ClaudeCode, &direct), "{build:?}");
        }

        let root = abs("/");
        assert!(is_valid_deck_exe(&format!(
            "{root}{}",
            "a".repeat(MAX_DECK_EXE_BYTES - root.len())
        )));
        assert!(!is_valid_deck_exe(&format!(
            "{root}{}",
            "a".repeat(MAX_DECK_EXE_BYTES - root.len() + 1)
        )));
        assert!(is_valid_deck_build("0.46.0-g1a8e0de3-dirty"));
    }

    /// Scenario: a sender names a new path on every hook line. The warned
    /// history stops at its cap, and the state keeps at most
    /// `MAX_BINARIES_PER_AGENT` statuses for the agent, the latest among them,
    /// however many paths arrive (audit A1).
    #[test]
    fn repeated_unique_sender_paths_stay_bounded() {
        let mut state = HookBinaryState {
            deck: deck("0.46.0"),
            ..HookBinaryState::default()
        };
        for i in 0..(MAX_WARNED_BINARIES * 4) {
            state.observe(
                &AgentType::ClaudeCode,
                &HookLineSender {
                    deck_build: Some("0.0.1-gabc1234".into()),
                    deck_exe: Some(abs(&format!("/opt/old-{i}/dot-agent-deck"))),
                },
            );
        }
        assert_eq!(state.warned.len(), MAX_WARNED_BINARIES);
        assert!(state.warnings_suppressed);
        assert_eq!(state.statuses.len(), 1);
        assert_eq!(
            state.statuses[&AgentType::ClaudeCode].len(),
            MAX_BINARIES_PER_AGENT
        );
        let notices = state.notices();
        assert_eq!(notices.len(), MAX_BINARIES_PER_AGENT);
        let latest = abs(&format!(
            "/opt/old-{}/dot-agent-deck",
            MAX_WARNED_BINARIES * 4 - 1
        ));
        assert!(
            notices.iter().any(|notice| notice.binary == latest),
            "the notices show the latest path"
        );
    }

    /// Scenario: every agent sends the largest path a stamp may carry, each a
    /// different one, plus an ephemeral-location notice and an over-long
    /// trusted pin. The notices stay few, and their encoding stays far below
    /// the attach protocol's 16 MiB frame limit (audit A1).
    #[test]
    fn the_aggregate_notice_payload_is_bounded() {
        let mut state = HookBinaryState::from_startup(
            deck("0.46.0"),
            &[],
            Some(format!("/Volumes/{}", "v".repeat(10_000))),
        );
        state.pins.insert(
            AgentType::Pi,
            vec![TrustedPin {
                binary: format!("/{}", "p".repeat(100_000)),
                resolved: None,
                is_self: false,
                homebrew: false,
            }],
        );
        state.observe(&AgentType::Pi, &HookLineSender::default());
        for (i, agent) in [
            AgentType::ClaudeCode,
            AgentType::OpenCode,
            AgentType::Codex,
            AgentType::Devin,
            AgentType::None,
        ]
        .into_iter()
        .enumerate()
        {
            let root = abs("/");
            let path = format!(
                "{root}{i}{}",
                "x".repeat(MAX_DECK_EXE_BYTES - root.len() - 2)
            );
            assert!(is_valid_deck_exe(&path));
            state.observe(
                &agent,
                &HookLineSender {
                    deck_build: Some(format!("0.0.1-g{}", "a".repeat(MAX_DECK_BUILD_BYTES - 7))),
                    deck_exe: Some(path),
                },
            );
        }
        let notices = state.notices();
        assert!(notices.len() <= MAX_NOTICES);
        assert_eq!(notices.len(), 7, "{}", notices.len());
        for notice in &notices {
            assert!(
                notice.binary.len() <= MAX_DECK_EXE_BYTES,
                "{}",
                notice.binary.len()
            );
        }
        let bytes = serde_json::to_vec(&notices).unwrap().len();
        assert!(bytes < 64 * 1024, "{bytes} bytes");
        assert!(bytes < crate::daemon_protocol::MAX_FRAME_LEN / 100);
    }

    /// Scenario: after startup has resolved its trusted pins, recording a hook
    /// line — whatever path it names, existing or not — and composing the
    /// notices make no filesystem resolution at all (audit A2).
    #[test]
    fn ingest_and_notices_do_not_touch_the_filesystem() {
        let dir = crate::test_temp::tempdir().unwrap();
        let pinned = dir.path().join("pinned").join("dot-agent-deck");
        let mut state = HookBinaryState::from_startup(
            deck("0.46.0"),
            &[HookPin {
                agent: AgentType::Codex,
                config: PathBuf::from("/cfg"),
                binary: pinned.to_string_lossy().into_owned(),
            }],
            None,
        );
        FS_RESOLUTIONS.with(|count| count.set(0));
        // Absolute on this platform, so each reaches the comparison rather than
        // being dropped at validation.
        for exe in [
            dir.path().to_string_lossy().into_owned(),
            abs("/nonexistent/automount/dot-agent-deck"),
            abs("/opt/homebrew/bin/dot-agent-deck"),
        ] {
            state.observe(
                &AgentType::ClaudeCode,
                &HookLineSender {
                    deck_build: Some("0.0.1-gabc1234".into()),
                    deck_exe: Some(exe),
                },
            );
            let _ = state.notices();
        }
        state.observe(&AgentType::Codex, &HookLineSender::default());
        let _ = state.notices();
        assert_eq!(FS_RESOLUTIONS.with(std::cell::Cell::get), 0);
    }

    #[cfg(unix)]
    fn version_stub(dir: &Path, name: &str, script: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name).join("dot-agent-deck");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::test_isolation::write_script(&path, script.as_bytes()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// Scenario: the probe reads what a pinned binary reports, and a binary
    /// that prints nothing, prints something else or fails is unprobeable.
    #[cfg(unix)]
    #[test]
    fn the_probe_reads_a_version_and_refuses_anything_else() {
        let dir = crate::test_temp::tempdir().unwrap();
        let old = version_stub(
            dir.path(),
            "old",
            "#!/bin/sh\necho 'dot-agent-deck 0.0.1'\n",
        );
        assert_eq!(probe_version(Path::new(&old)).as_deref(), Ok("0.0.1"));
        let silent = version_stub(dir.path(), "silent", "#!/bin/sh\nexit 0\n");
        assert!(probe_version(Path::new(&silent)).is_err());
        let other = version_stub(dir.path(), "other", "#!/bin/sh\necho 'hello 1.0'\n");
        assert!(probe_version(Path::new(&other)).is_err());
        let failing = version_stub(dir.path(), "fail", "#!/bin/sh\nexit 3\n");
        assert!(probe_version(Path::new(&failing)).is_err());
        assert!(probe_version(&dir.path().join("missing")).is_err());
    }

    /// Scenario: an eligible newer copy supersedes an older pin; a tie, a newer
    /// pin, an unprobeable pin and an ineligible copy all keep it.
    #[cfg(unix)]
    #[test]
    fn supersedes_only_an_older_probed_pin_for_an_eligible_copy() {
        let dir = crate::test_temp::tempdir().unwrap();
        let old = version_stub(
            dir.path(),
            "old",
            "#!/bin/sh\necho 'dot-agent-deck 0.45.1'\n",
        );
        let same = version_stub(
            dir.path(),
            "same",
            "#!/bin/sh\necho 'dot-agent-deck 0.46.0'\n",
        );
        let silent = version_stub(dir.path(), "silent", "#!/bin/sh\nexit 0\n");
        let eligible = Takeover::eligible("0.46.0");
        assert!(eligible.supersedes(&old));
        assert!(!eligible.supersedes(&same));
        assert!(!Takeover::eligible("0.45.0").supersedes(&old));
        assert!(!eligible.supersedes(&silent));
        assert!(!Takeover::ineligible().supersedes(&old));
    }

    fn deck(version: &str) -> DeckIdentity {
        DeckIdentity {
            exe: None,
            version: version.into(),
            self_install_path: None,
        }
    }

    /// Scenario: at startup an older pin raises `Older`, an unprobeable pin
    /// `Unprobeable`, and an equal pin nothing; a later stamp-less line from an
    /// agent pinned to another binary turns its notice into `Unreported`. A
    /// current line from another binary leaves that notice, and a current line
    /// from the pinned binary itself — upgraded in place — clears it.
    #[cfg(unix)]
    #[test]
    fn notices_follow_the_startup_probe_and_the_hook_line_stamp() {
        let dir = crate::test_temp::tempdir().unwrap();
        let old = version_stub(
            dir.path(),
            "old",
            "#!/bin/sh\necho 'dot-agent-deck 0.0.1'\n",
        );
        let silent = version_stub(dir.path(), "silent", "#!/bin/sh\nexit 0\n");
        let same = version_stub(
            dir.path(),
            "same",
            "#!/bin/sh\necho 'dot-agent-deck 0.46.0'\n",
        );
        let pin = |agent: AgentType, binary: &str| HookPin {
            agent,
            config: PathBuf::from("/cfg"),
            binary: binary.to_string(),
        };
        let mut state = HookBinaryState::from_startup(
            deck("0.46.0"),
            &[
                pin(AgentType::ClaudeCode, &old),
                pin(AgentType::Codex, &old),
                pin(AgentType::OpenCode, &silent),
                pin(AgentType::Devin, &same),
            ],
            None,
        );
        let notices = state.notices();
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert_eq!(notices[0].binary, old);
        assert_eq!(notices[0].agents, vec!["Claude Code", "Codex"]);
        assert_eq!(notices[0].version.as_deref(), Some("0.0.1"));
        assert_eq!(notices[0].reason, HookBinaryReason::Older);
        assert_eq!(notices[0].daemon_version, "0.46.0");
        assert_eq!(notices[1].reason, HookBinaryReason::Unprobeable);
        assert_eq!(notices[1].agents, vec!["OpenCode"]);

        // An older hook binary sends no stamp.
        assert!(state.observe(&AgentType::ClaudeCode, &HookLineSender::default()));
        let notices = state.notices();
        let claude = notices
            .iter()
            .find(|n| n.agents == vec!["Claude Code"])
            .expect("Claude Code split off");
        assert_eq!(claude.reason, HookBinaryReason::Unreported);
        assert_eq!(claude.version.as_deref(), Some("0.0.1"));
        // Repeating it changes nothing.
        assert!(!state.observe(&AgentType::ClaudeCode, &HookLineSender::default()));

        // A current stamp from another binary leaves the agent's notice, since
        // the pinned binary is still sending; one from the pinned binary
        // clears it. An older stamp raises `Older`.
        let elsewhere = HookLineSender {
            deck_build: Some("0.46.0-gabc1234".into()),
            deck_exe: Some("/somewhere/dot-agent-deck".into()),
        };
        assert!(!state.observe(&AgentType::ClaudeCode, &elsewhere));
        assert!(
            state
                .notices()
                .iter()
                .any(|n| n.agents.contains(&"Claude Code".to_string()))
        );
        let current = HookLineSender {
            deck_build: Some("0.46.0-gabc1234".into()),
            deck_exe: Some(old.clone()),
        };
        assert!(state.observe(&AgentType::ClaudeCode, &current));
        assert!(
            state
                .notices()
                .iter()
                .all(|n| !n.agents.contains(&"Claude Code".to_string()))
        );
        let older = HookLineSender {
            deck_build: Some("0.45.1-gabc1234-dirty".into()),
            deck_exe: Some("/somewhere/dot-agent-deck".into()),
        };
        assert!(state.observe(&AgentType::Devin, &older));
        let devin = state
            .notices()
            .into_iter()
            .find(|n| n.agents == vec!["Devin"])
            .expect("Devin notice");
        assert_eq!(devin.binary, "/somewhere/dot-agent-deck");
        assert_eq!(devin.version.as_deref(), Some("0.45.1"));
        assert_eq!(devin.reason, HookBinaryReason::Older);

        // A stamp-less line for an agent with no pin, or one pinned to the
        // daemon, changes nothing.
        assert!(!state.observe(&AgentType::Pi, &HookLineSender::default()));
    }

    fn codex_pinned_to(binary: &str) -> HookBinaryState {
        let mut state = HookBinaryState {
            deck: DeckIdentity {
                exe: Some(PathBuf::from(abs("/home/u/.local/bin/dot-agent-deck"))),
                ..deck("0.46.0")
            },
            ..HookBinaryState::default()
        };
        state.pins.insert(
            AgentType::Codex,
            vec![TrustedPin {
                binary: binary.to_string(),
                resolved: None,
                is_self: false,
                homebrew: false,
            }],
        );
        state
    }

    /// Scenario (Qodo on #1656): Codex's hooks run an older copy while its
    /// pane's own `dot-agent-deck wrap` — this deck — sends lines stamped with
    /// the current build, the two alternating on every event. The notice is
    /// raised once and stays: the wrap's lines never clear it, nothing
    /// changes after the first older line, and the binary is warned about
    /// once per reason.
    #[test]
    fn wrap_lines_beside_an_older_hook_do_not_flicker() {
        let old = abs("/opt/old/dot-agent-deck");
        let wrap = HookLineSender {
            deck_build: Some("0.46.0-gabc1234".into()),
            deck_exe: Some(abs("/home/u/.local/bin/dot-agent-deck")),
        };
        for hook in [
            HookLineSender {
                deck_build: Some("0.45.0-gabc1234".into()),
                deck_exe: Some(old.clone()),
            },
            HookLineSender::default(),
        ] {
            let mut state = codex_pinned_to(&old);
            let start = Instant::now();
            let mut changes = 0;
            let mut shown = None;
            for i in 0..20u64 {
                let now = start + Duration::from_secs(i);
                let sender = if i % 2 == 0 { &hook } else { &wrap };
                if state.observe_at(&AgentType::Codex, sender, now) {
                    changes += 1;
                }
                let notices = state.notices();
                assert_eq!(notices.len(), 1, "{i}: {notices:?}");
                assert_eq!(notices[0].binary, old);
                if let Some(shown) = &shown {
                    assert_eq!(&notices, shown, "{i}");
                }
                shown = Some(notices);
            }
            assert_eq!(changes, 1, "{hook:?}");
            assert_eq!(state.warned.len(), 1, "{:?}", state.warned);
            // The steady state needs no write.
            let later = start + Duration::from_secs(20);
            assert!(!state.would_change_at(&AgentType::Codex, &wrap, later));
            assert!(!state.would_change_at(&AgentType::Codex, &hook, later));
        }
    }

    /// `codex_pinned_to` with no pin, so an older copy is known only from its
    /// own hook lines.
    fn codex_unpinned() -> HookBinaryState {
        let mut state = codex_pinned_to(&abs("/opt/unused/dot-agent-deck"));
        state.pins.clear();
        state
    }

    /// Scenario: an older copy no agent config names — known only from its
    /// own hook lines — stops sending, and only this deck's lines arrive for
    /// Codex from then on. The older copy's notice stays while that copy was
    /// seen recently, and clears with the first line once it has sent nothing
    /// for `CLEAR_AFTER` (`HOOK_BINARY_QUIET_AFTER` plus the refresh interval
    /// a recorded time can trail by).
    #[test]
    fn a_line_learned_notice_clears_once_its_binary_has_gone_quiet() {
        let old = abs("/opt/old/dot-agent-deck");
        let mut state = codex_unpinned();
        let start = Instant::now();
        let older = HookLineSender {
            deck_build: Some("0.45.0-gabc1234".into()),
            deck_exe: Some(old.clone()),
        };
        assert!(state.observe_at(&AgentType::Codex, &older, start));
        let fixed = HookLineSender {
            deck_build: Some("0.46.0-gabc1234".into()),
            deck_exe: Some(abs("/home/u/.local/bin/dot-agent-deck")),
        };
        let almost = start + CLEAR_AFTER - Duration::from_secs(1);
        assert!(!state.observe_at(&AgentType::Codex, &fixed, almost));
        assert_eq!(state.notices().len(), 1);
        let quiet = start + CLEAR_AFTER;
        assert!(state.would_change_at(&AgentType::Codex, &fixed, quiet));
        assert!(state.observe_at(&AgentType::Codex, &fixed, quiet));
        assert!(state.notices().is_empty());
    }

    /// Scenario (auditor on #1656): the daemon's own sequence, which records
    /// a line only when `would_change_at` says so. The older, unpinned copy's
    /// line is recorded, its repeat 29 seconds later is skipped as unchanged,
    /// and this deck's lines then arrive steadily. The notice stays until the
    /// older copy has really sent nothing for `HOOK_BINARY_QUIET_AFTER`,
    /// counted from its skipped last line rather than the recorded one.
    #[test]
    fn the_quiet_time_counts_from_a_line_the_daemon_skipped() {
        let old = abs("/opt/old/dot-agent-deck");
        let mut state = codex_unpinned();
        let older = HookLineSender {
            deck_build: Some("0.45.0-gabc1234".into()),
            deck_exe: Some(old.clone()),
        };
        let fixed = HookLineSender {
            deck_build: Some("0.46.0-gabc1234".into()),
            deck_exe: Some(abs("/home/u/.local/bin/dot-agent-deck")),
        };
        // What `observe_hook_sender` does with one line.
        let deliver = |state: &mut HookBinaryState, sender: &HookLineSender, at: Instant| {
            state.would_change_at(&AgentType::Codex, sender, at)
                && state.observe_at(&AgentType::Codex, sender, at)
        };
        let start = Instant::now();
        assert!(deliver(&mut state, &older, start));
        let last_old = start + SEEN_REFRESH - Duration::from_secs(1);
        assert!(!state.would_change_at(&AgentType::Codex, &older, last_old));
        assert!(!deliver(&mut state, &older, last_old));
        let mut at = last_old;
        while at < last_old + HOOK_BINARY_QUIET_AFTER {
            assert!(
                !deliver(&mut state, &fixed, at),
                "cleared at {:?}",
                at - last_old
            );
            assert_eq!(state.notices().len(), 1, "{:?}", at - last_old);
            at += Duration::from_secs(1);
        }
        let quiet = start + CLEAR_AFTER;
        assert!(quiet >= last_old + HOOK_BINARY_QUIET_AFTER);
        assert!(deliver(&mut state, &fixed, quiet));
        assert!(state.notices().is_empty());
    }

    /// Scenario (reviewer on #1656): a line stamped with a valid build but an
    /// unusable path, for an agent with no pin or with two, names no binary,
    /// so it raises no notice; with exactly one pin it is that pin's line.
    #[test]
    fn a_line_with_no_usable_path_names_no_binary() {
        let line = HookLineSender {
            deck_build: Some("0.45.0-gabc1234".into()),
            deck_exe: Some("relative/dot-agent-deck".into()),
        };
        let now = Instant::now();
        let mut unpinned = codex_pinned_to(&abs("/opt/old/dot-agent-deck"));
        unpinned.pins.clear();
        let mut two = codex_pinned_to(&abs("/opt/old/dot-agent-deck"));
        let second = TrustedPin {
            binary: abs("/opt/other/dot-agent-deck"),
            ..two.pins[&AgentType::Codex][0].clone()
        };
        two.pins.get_mut(&AgentType::Codex).unwrap().push(second);
        for state in [&mut unpinned, &mut two] {
            assert!(!state.would_change_at(&AgentType::Codex, &line, now));
            assert!(!state.observe_at(&AgentType::Codex, &line, now));
            assert!(state.notices().is_empty(), "{:?}", state.notices());
            assert!(state.statuses.values().all(Vec::is_empty));
        }
        let old = abs("/opt/old/dot-agent-deck");
        let mut one = codex_pinned_to(&old);
        assert!(one.observe_at(&AgentType::Codex, &line, now));
        let notices = one.notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].binary, old);
    }

    /// Scenario (reviewer on #1656): with more binaries raising notices than a
    /// client is sent, a change to one beyond the cap leaves the list clients
    /// receive unchanged, so it is not broadcast; a change inside the cap is.
    #[test]
    fn a_change_beyond_the_notice_cap_is_not_broadcast() {
        let mut state = HookBinaryState {
            deck: deck("0.46.0"),
            ..HookBinaryState::default()
        };
        let now = Instant::now();
        let agents = [AgentType::ClaudeCode, AgentType::OpenCode, AgentType::Codex];
        let line = |n: usize, build: &str| HookLineSender {
            deck_build: Some(build.into()),
            deck_exe: Some(abs(&format!("/opt/old{n}/dot-agent-deck"))),
        };
        let total = MAX_NOTICES + 1;
        for n in 0..total {
            let agent = &agents[n / MAX_BINARIES_PER_AGENT];
            state.observe_at(agent, &line(n, "0.45.0-gabc1234"), now);
        }
        assert_eq!(state.reasoned().len(), total);
        let before = state.notices();
        assert_eq!(before.len(), MAX_NOTICES);
        let last = total - 1;
        let beyond = &agents[last / MAX_BINARIES_PER_AGENT];
        assert!(state.would_change_at(beyond, &line(last, "0.44.0-gabc1234"), now));
        assert!(!state.observe_at(beyond, &line(last, "0.44.0-gabc1234"), now));
        assert_eq!(state.notices(), before);
        assert!(state.observe_at(&agents[0], &line(0, "0.44.0-gabc1234"), now));
        assert_ne!(state.notices(), before);
    }

    /// Scenario (Qodo and Greptile on #1656): Codex's hooks are pinned to two
    /// older copies, one per event. Lines from each are tracked apart, so both
    /// notices show however the lines interleave, and a stamp-less line — from
    /// one of the two, but which is unknown — marks both.
    #[test]
    fn two_pins_for_one_agent_are_both_tracked() {
        let first = abs("/opt/first/dot-agent-deck");
        let second = abs("/opt/second/dot-agent-deck");
        let mut state = codex_pinned_to(&first);
        state
            .pins
            .get_mut(&AgentType::Codex)
            .unwrap()
            .push(TrustedPin {
                binary: second.clone(),
                resolved: None,
                is_self: false,
                homebrew: false,
            });
        let line = |build: &str, exe: &str| HookLineSender {
            deck_build: Some(build.into()),
            deck_exe: Some(exe.into()),
        };
        for _ in 0..3 {
            state.observe(&AgentType::Codex, &line("0.44.0-gabc1234", &first));
            state.observe(&AgentType::Codex, &line("0.45.0-gabc1234", &second));
        }
        let mut binaries: Vec<(String, Option<String>)> = state
            .notices()
            .into_iter()
            .map(|n| (n.binary, n.version))
            .collect();
        binaries.sort();
        assert_eq!(
            binaries,
            vec![
                (first.clone(), Some("0.44.0".into())),
                (second.clone(), Some("0.45.0".into())),
            ]
        );
        state.observe(&AgentType::Codex, &HookLineSender::default());
        let notices = state.notices();
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(
            notices
                .iter()
                .all(|n| n.reason == HookBinaryReason::Unreported),
            "{notices:?}"
        );
    }

    /// Scenario (Qodo on #1656): startup sees two Codex pins, one an older
    /// copy and one this deck, in either order. The older copy's notice is the
    /// same both ways, and a stamp-less line marks only the older copy.
    #[cfg(unix)]
    #[test]
    fn startup_notices_do_not_depend_on_pin_order() {
        let dir = crate::test_temp::tempdir().unwrap();
        let old = version_stub(
            dir.path(),
            "old",
            "#!/bin/sh\necho 'dot-agent-deck 0.0.1'\n",
        );
        let me = version_stub(
            dir.path(),
            "me",
            "#!/bin/sh\necho 'dot-agent-deck 0.46.0'\n",
        );
        let identity = DeckIdentity {
            exe: Some(std::fs::canonicalize(&me).unwrap()),
            ..deck("0.46.0")
        };
        let pin = |binary: &str| HookPin {
            agent: AgentType::Codex,
            config: PathBuf::from("/cfg"),
            binary: binary.to_string(),
        };
        let mut seen = Vec::new();
        for order in [[&old, &me], [&me, &old]] {
            let mut state = HookBinaryState::from_startup(
                identity.clone(),
                &[pin(order[0]), pin(order[1])],
                None,
            );
            let notices = state.notices();
            assert_eq!(notices.len(), 1, "{notices:?}");
            assert_eq!(notices[0].binary, old);
            assert_eq!(notices[0].reason, HookBinaryReason::Older);
            seen.push(notices);
            assert!(state.observe(&AgentType::Codex, &HookLineSender::default()));
            let notices = state.notices();
            assert_eq!(notices.len(), 1, "{notices:?}");
            assert_eq!(notices[0].binary, old);
            assert_eq!(notices[0].reason, HookBinaryReason::Unreported);
        }
        assert_eq!(seen[0], seen[1]);
    }

    /// Scenario: one warning per (binary, reason), however many lines say it.
    #[test]
    fn a_warning_is_recorded_once_per_binary_and_reason() {
        let mut state = HookBinaryState {
            deck: deck("0.46.0"),
            ..HookBinaryState::default()
        };
        state.pins.insert(
            AgentType::ClaudeCode,
            vec![TrustedPin {
                binary: "/opt/old".into(),
                resolved: None,
                is_self: false,
                homebrew: false,
            }],
        );
        for _ in 0..3 {
            state.observe(&AgentType::ClaudeCode, &HookLineSender::default());
        }
        assert_eq!(state.warned.len(), 1);
        assert!(
            state
                .warned
                .contains(&("/opt/old".to_string(), HookBinaryReason::Unreported))
        );
    }

    #[test]
    fn an_ephemeral_location_is_a_notice_with_the_move_advice() {
        let state = HookBinaryState::from_startup(
            deck("0.46.0"),
            &[],
            Some("/Volumes/Agent Deck/Agent Deck.app/Contents/MacOS/dot-agent-deck".into()),
        );
        let notices = state.notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].reason, HookBinaryReason::EphemeralLocation);
        assert!(notices[0].remedy.contains("/Applications"));
        assert_eq!(notices[0].command, None);
    }

    /// Scenario: a pinned binary's `--version` prints its version and leaves a
    /// background child holding stdout open. The probe returns the version
    /// well inside its timeout, and the background child is killed with the
    /// binary's process group (audit A5).
    #[cfg(unix)]
    #[test]
    fn the_probe_kills_a_descendant_holding_stdout() {
        let dir = crate::test_temp::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let stub = version_stub(
            dir.path(),
            "forks",
            &format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\necho 'dot-agent-deck 0.0.1'\n",
                pidfile.display()
            ),
        );
        let started = Instant::now();
        let out = run_probe(Path::new(&stub), Duration::from_secs(5)).expect("probe");
        assert!(out.contains("0.0.1"), "{out}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        let pid: libc::pid_t = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let gone = (0..100).any(|_| {
            // SAFETY: signal 0 only checks that the pid exists.
            let alive = unsafe { libc::kill(pid, 0) } == 0
                && std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .map_or(true, |stat| !stat.contains(") Z "));
            if alive {
                std::thread::sleep(Duration::from_millis(20));
            }
            !alive
        });
        assert!(gone, "the background child {pid} outlived the probe");
    }

    /// Scenario: a pinned binary that never answers. The probe gives up at its
    /// timeout and reports it, without waiting for the binary (audit A5).
    #[cfg(unix)]
    #[test]
    fn the_probe_gives_up_on_a_hung_binary() {
        let dir = crate::test_temp::tempdir().unwrap();
        let stub = version_stub(dir.path(), "hung", "#!/bin/sh\nexec sleep 30\n");
        let started = Instant::now();
        let err = run_probe(Path::new(&stub), Duration::from_millis(300)).unwrap_err();
        assert!(err.contains("did not exit"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    /// The [`ProbeGroup`] events this thread recorded since the last call.
    #[cfg(unix)]
    fn take_probe_events() -> Vec<ProbeEvent> {
        PROBE_EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
    }

    /// Scenario: every probe path kills the group at most once and always
    /// before the leader is reaped, never after: an exiting binary, a hung one
    /// and one whose descendant holds stdout (audit A6).
    #[cfg(unix)]
    #[test]
    fn the_probe_never_signals_the_group_after_the_reap() {
        let dir = crate::test_temp::tempdir().unwrap();
        let cases = [
            ("exits", "#!/bin/sh\necho 'dot-agent-deck 0.0.1'\n"),
            ("hung", "#!/bin/sh\nexec sleep 30\n"),
            (
                "forks",
                "#!/bin/sh\nsleep 30 &\necho 'dot-agent-deck 0.0.1'\n",
            ),
            ("fails", "#!/bin/sh\nexit 3\n"),
        ];
        for (name, script) in cases {
            let stub = version_stub(dir.path(), name, script);
            take_probe_events();
            let _ = run_probe(Path::new(&stub), Duration::from_millis(500));
            assert_eq!(
                take_probe_events(),
                [ProbeEvent::Kill, ProbeEvent::Reap],
                "{name}"
            );
        }
    }

    /// Scenario: a pinned binary's `--version` leaves a descendant that leaves
    /// the binary's process group (`setsid`) holding stdout, then exits. The
    /// probe kills the group and reaps the binary once, sends nothing at the
    /// deadline, and returns within its timeout with the pipe still held
    /// (audit A6).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_probe_stops_reading_an_escaped_descendant_without_a_signal() {
        if std::process::Command::new("setsid")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: no setsid(1) on this host");
            return;
        }
        let dir = crate::test_temp::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let stub = version_stub(
            dir.path(),
            "escapes",
            &format!(
                "#!/bin/sh\nsetsid sh -c 'echo $$ > \"{pid}\"; exec sleep 30' &\n\
                 while [ ! -s '{pid}' ]; do sleep 0.05; done\n\
                 echo 'dot-agent-deck 0.0.1'\n",
                pid = pidfile.display()
            ),
        );
        take_probe_events();
        let started = Instant::now();
        let err = run_probe(Path::new(&stub), Duration::from_millis(800)).unwrap_err();
        let elapsed = started.elapsed();
        let events = take_probe_events();
        let escaped: Option<libc::pid_t> = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|pid| pid.trim().parse().ok());
        if let Some(pid) = escaped {
            // SAFETY: the escaped `sleep` this test's stub started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(err.contains("output did not close"), "{err}");
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
        assert_eq!(events, [ProbeEvent::Kill, ProbeEvent::Reap]);
        assert!(escaped.is_some(), "the escaped descendant never started");
    }

    /// Scenario: the probe cannot make the binary's stdout non-blocking. It
    /// reads nothing, kills the group before reaping, and returns an
    /// unprobeable error at once instead of blocking on the read (audit A7).
    #[cfg(unix)]
    #[test]
    fn the_probe_refuses_a_pipe_it_cannot_make_non_blocking() {
        let dir = crate::test_temp::tempdir().unwrap();
        let stub = version_stub(dir.path(), "silent-hung", "#!/bin/sh\nexec sleep 30\n");
        take_probe_events();
        FAIL_NONBLOCKING.with(|fail| fail.set(true));
        let started = Instant::now();
        let result = run_probe(Path::new(&stub), Duration::from_secs(5));
        FAIL_NONBLOCKING.with(|fail| fail.set(false));
        let err = result.unwrap_err();
        assert!(err.contains("could not be read"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(take_probe_events(), [ProbeEvent::Kill, ProbeEvent::Reap]);
    }

    /// The pid a stub that writes `$$` to `pidfile` recorded, waiting for it.
    #[cfg(unix)]
    fn stub_pid(pidfile: &Path) -> libc::pid_t {
        for _ in 0..250 {
            if let Some(pid) = std::fs::read_to_string(pidfile)
                .ok()
                .and_then(|pid| pid.trim().parse().ok())
            {
                return pid;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the stub never wrote {}", pidfile.display());
    }

    /// Whether `pid` is gone, reaped and not merely a zombie.
    #[cfg(unix)]
    fn is_reaped(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only checks that the pid exists.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    /// Scenario: the probe's exit poll fails, with `ECHILD` (the kernel says
    /// the binary is not its child any more) or with an error it cannot
    /// interpret. The probe reports the failure and gives the binary up
    /// without signalling its process group, in the poll or in cleanup
    /// (audit A6).
    #[cfg(unix)]
    #[test]
    fn the_probe_sends_no_signal_after_its_exit_poll_fails() {
        let dir = crate::test_temp::tempdir().unwrap();
        for errno in [libc::ECHILD, libc::EINVAL] {
            let pidfile = dir.path().join(format!("pid-{errno}"));
            let stub = version_stub(
                dir.path(),
                &format!("poll-fails-{errno}"),
                &format!(
                    "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
                    pidfile.display()
                ),
            );
            take_probe_events();
            WAITID_ERROR.with(|seam| seam.set(Some(errno)));
            let result = run_probe(Path::new(&stub), Duration::from_secs(5));
            WAITID_ERROR.with(|seam| seam.set(None));
            let events = take_probe_events();
            // The seam left a real child behind; this test, not the probe,
            // owns cleaning it up.
            let pid = stub_pid(&pidfile);
            // SAFETY: the stub this test started, still unreaped by anyone.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
            let err = result.unwrap_err();
            assert!(err.contains("waiting for it failed"), "{errno}: {err}");
            assert_eq!(events, [ProbeEvent::Release], "{errno}");
        }
    }

    /// Set only in the re-executed copy of this test binary that
    /// [`the_probe_sends_no_signal_when_sigchld_is_ignored`] starts.
    #[cfg(unix)]
    const IGNORED_SIGCHLD_DIR: &str = "DAD_TEST_HOOK_PROBE_IGNORED_SIGCHLD_DIR";

    /// The child half of [`the_probe_sends_no_signal_when_sigchld_is_ignored`],
    /// a no-op unless re-executed with [`IGNORED_SIGCHLD_DIR`] set. It ignores
    /// `SIGCHLD` for its whole process, which is why it runs in one of its own.
    #[cfg(unix)]
    #[test]
    fn ignored_sigchld_probe_child_half() {
        let Ok(dir) = std::env::var(IGNORED_SIGCHLD_DIR) else {
            return;
        };
        // Written first: writing a script waits for a child of its own.
        let stub = version_stub(
            Path::new(&dir),
            "exits",
            "#!/bin/sh\necho 'dot-agent-deck 0.0.1'\n",
        );
        // SAFETY: this process is a re-executed copy running only this test.
        unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
        take_probe_events();
        let result = run_probe(Path::new(&stub), Duration::from_secs(5));
        let events = take_probe_events();
        assert_eq!(events, [ProbeEvent::Release], "{result:?}");
        let err = result.unwrap_err();
        assert!(err.contains("waiting for it failed"), "{err}");
    }

    /// Scenario: the probe runs in a process that ignores `SIGCHLD`, so the
    /// kernel reaps the binary the moment it exits. The probe sees `ECHILD`,
    /// reports the binary unprobeable, and never signals the process group
    /// whose id was the binary's pid (audit A6).
    #[cfg(unix)]
    #[test]
    fn the_probe_sends_no_signal_when_sigchld_is_ignored() {
        let dir = crate::test_temp::tempdir().unwrap();
        let out = std::process::Command::new(
            std::env::current_exe().expect("the test binary has a path"),
        )
        .args([
            "hook_binary::tests::ignored_sigchld_probe_child_half",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(IGNORED_SIGCHLD_DIR, dir.path())
        .output()
        .expect("re-exec this test binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stdout}\n{stderr}");
        assert!(
            stdout.contains("1 passed"),
            "the child half did not run: {stdout}\n{stderr}"
        );
    }

    /// Scenario: a hung binary is killed at the deadline when no thread can be
    /// created for its reaper. The probe returns its timeout error without
    /// panicking, and the binary is already reaped when it does (audit A8).
    #[cfg(unix)]
    #[test]
    fn the_probe_reaps_inline_when_no_reaper_thread_can_start() {
        let dir = crate::test_temp::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let stub = version_stub(
            dir.path(),
            "hung",
            &format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
                pidfile.display()
            ),
        );
        take_probe_events();
        FAIL_REAPER_SPAWN.with(|seam| seam.set(true));
        let result = run_probe(Path::new(&stub), Duration::from_secs(1));
        FAIL_REAPER_SPAWN.with(|seam| seam.set(false));
        let err = result.unwrap_err();
        assert!(err.contains("did not exit"), "{err}");
        assert_eq!(take_probe_events(), [ProbeEvent::Kill, ProbeEvent::Reap]);
        let pid = stub_pid(&pidfile);
        assert!(is_reaped(pid), "the binary {pid} was left unreaped");
    }

    /// Scenario: a probe unwinds from a panic while it owns a running binary,
    /// and no thread can be created for the reaper. The cleanup kills and
    /// reaps the binary without a second panic, so the original panic is
    /// caught rather than aborting the process (audit A8).
    #[cfg(unix)]
    #[test]
    fn probe_cleanup_during_an_unwind_does_not_panic() {
        use std::os::unix::process::CommandExt;
        let child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as libc::pid_t;
        take_probe_events();
        FAIL_REAPER_SPAWN.with(|seam| seam.set(true));
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _group = ProbeGroup::new(child);
            panic!("a probe failure");
        }));
        FAIL_REAPER_SPAWN.with(|seam| seam.set(false));
        assert!(caught.is_err());
        assert_eq!(take_probe_events(), [ProbeEvent::Kill, ProbeEvent::Reap]);
        assert!(is_reaped(pid), "the binary {pid} was left unreaped");
    }

    /// Scenario: the probe cache answers the same file once, and probes again
    /// when the file at that path is replaced by another (audit A5).
    #[cfg(unix)]
    #[test]
    fn the_probe_cache_follows_the_file_identity() {
        let dir = crate::test_temp::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let script = |version: &str| {
            format!(
                "#!/bin/sh\necho run >> '{}'\necho 'dot-agent-deck {version}'\n",
                runs.display()
            )
        };
        let stub = version_stub(dir.path(), "cached", &script("0.0.1"));
        assert_eq!(probe_version(Path::new(&stub)).as_deref(), Ok("0.0.1"));
        assert_eq!(probe_version(Path::new(&stub)).as_deref(), Ok("0.0.1"));
        let count = || std::fs::read_to_string(&runs).unwrap().lines().count();
        assert_eq!(count(), 1, "the second probe is answered from the cache");

        let replacement = version_stub(dir.path(), "next", &script("0.0.2"));
        std::fs::rename(&replacement, &stub).unwrap();
        assert_eq!(probe_version(Path::new(&stub)).as_deref(), Ok("0.0.2"));
        assert_eq!(count(), 2);
    }

    /// Scenario: the startup budget. Each probe gets at most the per-probe
    /// timeout and no more than the window has left; once the window's budget
    /// is spent a probe gets nothing, and a new window refills it (audit A5).
    #[test]
    fn the_probe_budget_spans_every_probe_in_a_window() {
        let mut budget = ProbeBudget::new();
        let start = Instant::now();
        assert_eq!(budget.allowance(start), HOOK_BINARY_PROBE_TIMEOUT);
        budget.charge(HOOK_BINARY_PROBE_TIMEOUT);
        budget.charge(HOOK_BINARY_PROBE_TIMEOUT);
        assert_eq!(
            budget.allowance(start + Duration::from_secs(4)),
            HOOK_BINARY_PROBE_BUDGET - HOOK_BINARY_PROBE_TIMEOUT * 2
        );
        budget.charge(HOOK_BINARY_PROBE_BUDGET);
        assert!(budget.allowance(start + Duration::from_secs(5)).is_zero());
        assert_eq!(
            budget.allowance(start + PROBE_BUDGET_WINDOW),
            HOOK_BINARY_PROBE_TIMEOUT
        );
    }

    #[test]
    fn an_unknown_reason_decodes() {
        let notice: HookBinaryNotice = serde_json::from_str(
            r#"{"binary":"b","agents":[],"daemon_version":"1.0.0","reason":"new_thing","remedy":"r"}"#,
        )
        .unwrap();
        assert_eq!(notice.reason, HookBinaryReason::Unknown);
        assert_eq!(notice.version, None);
    }

    fn is_unsafe(c: char) -> bool {
        c.is_control()
            || crate::untrusted_text::is_bidi_format_char(c)
            || matches!(c, '\u{2028}' | '\u{2029}')
    }

    /// Scenario: a notice whose every string carries an escape sequence, a
    /// carriage return, a newline, a bidi override and a line separator comes
    /// out of `sanitized` with none of them, bounded, and with the altered
    /// command dropped and its `Run:` lead-in replaced (issue #1637 B1).
    #[test]
    fn a_sanitized_notice_has_no_control_or_bidi_characters() {
        let hostile = "\x1b[2J\r\n\u{202E}\u{2028}";
        let notice = HookBinaryNotice {
            binary: format!("/opt/old{hostile}/{}", "x".repeat(10 * MAX_DECK_EXE_BYTES)),
            agents: (0..100).map(|n| format!("Agent{n}{hostile}")).collect(),
            version: Some(format!("0.45.0{hostile}")),
            daemon_version: format!("0.46.0{hostile}"),
            reason: HookBinaryReason::Older,
            remedy: REMEDY_RUN.to_string(),
            command: Some(format!("brew upgrade dot-agent-deck{hostile}")),
        };
        let clean = notice.sanitized();
        let strings = [&clean.binary, &clean.daemon_version, &clean.remedy]
            .into_iter()
            .chain(clean.agents.iter())
            .chain(clean.version.iter());
        for text in strings {
            assert!(!text.chars().any(is_unsafe), "{text:?}");
        }
        assert!(clean.binary.len() <= MAX_DECK_EXE_BYTES);
        assert!(
            clean.binary.starts_with("/opt/old[2J/"),
            "{}",
            &clean.binary[..20]
        );
        assert!(clean.agents.len() <= 16);
        assert_eq!(clean.version.as_deref(), Some("0.45.0[2J"));
        assert_eq!(clean.command, None);
        assert_eq!(clean.remedy, REMEDY_UPGRADE_OR_REINSTALL);

        // A clean notice is unchanged.
        let fine = HookBinaryNotice {
            binary: "/opt/old/dot-agent-deck".into(),
            agents: vec!["Codex".into()],
            version: Some("0.45.0".into()),
            daemon_version: "0.46.0".into(),
            reason: HookBinaryReason::Older,
            remedy: REMEDY_RUN.into(),
            command: Some("brew upgrade dot-agent-deck".into()),
        };
        assert_eq!(fine.sanitized(), fine);
        assert_eq!(sanitize_notices(&vec![fine.clone(); 40]).len(), MAX_NOTICES);
    }

    /// Scenario: a trusted pin read from an agent's config carrying control
    /// and bidi characters reaches the daemon's notices stripped (issue #1637
    /// B1): `notices()` sanitizes what it sends, not only what a hook line set.
    #[test]
    fn the_daemon_sanitizes_a_pin_it_did_not_validate() {
        let mut state = HookBinaryState {
            deck: deck("0.46.0"),
            ..HookBinaryState::default()
        };
        state.pins.insert(
            AgentType::Codex,
            vec![TrustedPin {
                binary: "/opt/\x1b[2Jold\u{202E}/dot-agent-deck".into(),
                resolved: None,
                is_self: false,
                homebrew: false,
            }],
        );
        assert!(state.observe(&AgentType::Codex, &HookLineSender::default()));
        let notices = state.notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].binary, "/opt/[2Jold/dot-agent-deck");
    }

    /// Scenario: the same stamp sent again records nothing and needs no write
    /// (issue #1637 S2): `would_change` is false and `observe` reports no
    /// change, for a current sender and for an older one alike; a different
    /// version from the same path changes the status again.
    #[test]
    fn a_repeated_stamp_changes_nothing() {
        let mut state = HookBinaryState {
            deck: deck("0.46.0"),
            ..HookBinaryState::default()
        };
        let current = HookLineSender {
            deck_build: Some("0.46.0-gabc1234".into()),
            deck_exe: Some(abs("/opt/current/dot-agent-deck")),
        };
        assert!(state.would_change(&AgentType::ClaudeCode, &current));
        // A first status with no reason is in no notice.
        assert!(!state.observe(&AgentType::ClaudeCode, &current));
        assert!(!state.would_change(&AgentType::ClaudeCode, &current));
        assert!(!state.observe(&AgentType::ClaudeCode, &current));

        let older = HookLineSender {
            deck_build: Some("0.45.0-gabc1234".into()),
            deck_exe: Some(abs("/opt/current/dot-agent-deck")),
        };
        assert!(state.would_change(&AgentType::ClaudeCode, &older));
        assert!(state.observe(&AgentType::ClaudeCode, &older));
        assert!(!state.would_change(&AgentType::ClaudeCode, &older));
        assert!(!state.observe(&AgentType::ClaudeCode, &older));

        // Back to current clears the notice.
        assert!(state.observe(&AgentType::ClaudeCode, &current));
        assert!(state.notices().is_empty());

        // An invalid stamp is never a change.
        let invalid = HookLineSender {
            deck_build: Some(String::new()),
            deck_exe: None,
        };
        assert!(!state.would_change(&AgentType::ClaudeCode, &invalid));
    }

    /// A pin a refresh read: this deck's own when `probe` is `None`.
    fn fresh(binary: &str, probe: Option<Result<String, String>>) -> RefreshedPin {
        RefreshedPin {
            trusted: TrustedPin {
                binary: binary.to_string(),
                resolved: None,
                is_self: probe.is_none(),
                homebrew: false,
            },
            probe,
        }
    }

    fn refresh_of(agent: AgentType, pins: Vec<RefreshedPin>) -> PinRefresh {
        PinRefresh {
            agents: vec![(agent, pins)],
        }
    }

    /// `codex_pinned_to(old)` with the notice startup's probe raises for it.
    fn codex_with_old_pin(old: &str, at: Instant) -> HookBinaryState {
        let mut state = codex_pinned_to(old);
        state.record(
            &AgentType::Codex,
            AgentStatus {
                binary: old.to_string(),
                version: Some("0.45.0".into()),
                reason: Some(HookBinaryReason::Older),
                homebrew: false,
            },
            at,
        );
        state
    }

    /// Scenario (Qodo on #1656): Codex's config pins an older copy for an
    /// event that never happens, so that copy sends nothing, while the pane's
    /// own `wrap` — this deck — sends current lines every ten seconds for far
    /// longer than the quiet time. The older copy's notice stays: its quiet
    /// proves nothing while a config still names it.
    #[test]
    fn a_pinned_idle_hook_keeps_its_notice_beside_current_wrap_lines() {
        let old = abs("/opt/old/dot-agent-deck");
        let start = Instant::now();
        let mut state = codex_with_old_pin(&old, start);
        let shown = state.notices();
        assert_eq!(shown.len(), 1);
        let wrap = HookLineSender {
            deck_build: Some("0.46.0-gabc1234".into()),
            deck_exe: Some(abs("/home/u/.local/bin/dot-agent-deck")),
        };
        let mut at = start;
        while at < start + CLEAR_AFTER * 3 {
            if state.would_change_at(&AgentType::Codex, &wrap, at) {
                assert!(!state.observe_at(&AgentType::Codex, &wrap, at));
            }
            assert_eq!(state.notices(), shown, "{:?}", at - start);
            at += Duration::from_secs(10);
        }
    }

    /// Scenario: the user reinstalls Codex's hooks from this deck, so its
    /// config names this deck instead of the older copy. The next refresh
    /// clears the older copy's notice and reports one change, and the same
    /// refresh again reports none, so the daemon broadcasts once.
    #[test]
    fn a_refresh_that_no_longer_names_the_old_pin_clears_its_notice() {
        let old = abs("/opt/old/dot-agent-deck");
        let own = abs("/home/u/.local/bin/dot-agent-deck");
        let now = Instant::now();
        let mut state = codex_with_old_pin(&old, now);
        assert_eq!(state.notices().len(), 1);
        assert!(state.apply_refresh_at(refresh_of(AgentType::Codex, vec![fresh(&own, None)]), now));
        assert!(state.notices().is_empty());
        assert_eq!(state.pins[&AgentType::Codex].len(), 1);
        assert!(state.pins[&AgentType::Codex][0].is_self);
        assert!(
            !state.apply_refresh_at(refresh_of(AgentType::Codex, vec![fresh(&own, None)]), now)
        );

        // A config that names no deck binary at all is evidence too.
        let mut state = codex_with_old_pin(&old, now);
        assert!(state.apply_refresh_at(refresh_of(AgentType::Codex, Vec::new()), now));
        assert!(state.notices().is_empty());
    }

    /// Scenario: mid-session the user points Codex's hooks at an older copy.
    /// The next refresh raises its notice, as startup would have; a refresh
    /// naming a copy of this deck's release raises nothing.
    #[test]
    fn a_refresh_that_adds_an_older_pin_raises_its_notice() {
        let old = abs("/opt/old/dot-agent-deck");
        let same = abs("/opt/same/dot-agent-deck");
        let now = Instant::now();
        let mut state = codex_unpinned();
        assert!(!state.apply_refresh_at(
            refresh_of(
                AgentType::Codex,
                vec![fresh(&same, Some(Ok("0.46.0".into())))]
            ),
            now
        ));
        assert!(state.notices().is_empty());
        assert!(state.apply_refresh_at(
            refresh_of(
                AgentType::Codex,
                vec![
                    fresh(&same, Some(Ok("0.46.0".into()))),
                    fresh(&old, Some(Ok("0.45.0".into()))),
                ]
            ),
            now
        ));
        let notices = state.notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].binary, old);
        assert_eq!(notices[0].reason, HookBinaryReason::Older);
        assert_eq!(notices[0].agents, vec!["Codex"]);
        assert_eq!(notices[0].version.as_deref(), Some("0.45.0"));
    }

    /// Scenario: a refresh cannot read Codex's config (missing, unparseable
    /// or unreadable), so the reader reports no evidence and the agent is
    /// absent from the refresh. The pin and its notice stay exactly as they
    /// were; and an agent the refresh did read leaves other agents alone.
    #[test]
    fn a_refresh_with_an_unreadable_config_changes_nothing() {
        let old = abs("/opt/old/dot-agent-deck");
        let now = Instant::now();
        let mut state = codex_with_old_pin(&old, now);
        let shown = state.notices();
        let pins = state.pins.clone();
        assert!(!state.apply_refresh_at(PinRefresh::default(), now));
        assert!(!state.apply_refresh_at(refresh_of(AgentType::ClaudeCode, Vec::new()), now));
        assert_eq!(state.notices(), shown);
        assert_eq!(state.pins[&AgentType::Codex], pins[&AgentType::Codex]);
    }

    /// Scenario: a pinned copy is upgraded in place, and its hooks are idle.
    /// The refresh's probe reports the new version, which differs from the
    /// one recorded, so the notice clears; a failed probe of a known pin
    /// changes nothing.
    #[test]
    fn a_refresh_reprobes_a_pin_replaced_in_place() {
        let old = abs("/opt/old/dot-agent-deck");
        let now = Instant::now();
        let mut state = codex_with_old_pin(&old, now);
        assert!(!state.apply_refresh_at(
            refresh_of(
                AgentType::Codex,
                vec![fresh(&old, Some(Err("budget".into())))]
            ),
            now
        ));
        assert!(!state.apply_refresh_at(
            refresh_of(
                AgentType::Codex,
                vec![fresh(&old, Some(Ok("0.45.0".into())))]
            ),
            now
        ));
        assert_eq!(state.notices().len(), 1);
        assert!(state.apply_refresh_at(
            refresh_of(
                AgentType::Codex,
                vec![fresh(&old, Some(Ok("0.46.0".into())))]
            ),
            now
        ));
        assert!(state.notices().is_empty());
    }

    /// Scenario: a binary first seen through its own hook lines, under its
    /// resolved spelling, turns up in a refresh under the spelling a config
    /// uses. It keeps one notice, under the config's spelling, and from then
    /// on its quiet does not clear it.
    #[test]
    fn a_line_learned_binary_a_refresh_pins_keeps_one_notice() {
        let link = abs("/opt/link/dot-agent-deck");
        let real = abs("/opt/real/dot-agent-deck");
        let start = Instant::now();
        let mut state = codex_unpinned();
        let older = HookLineSender {
            deck_build: Some("0.45.0-gabc1234".into()),
            deck_exe: Some(real.clone()),
        };
        assert!(state.observe_at(&AgentType::Codex, &older, start));
        let mut pin = fresh(&link, Some(Ok("0.45.0".into())));
        pin.trusted.resolved = Some(real.clone());
        assert!(state.apply_refresh_at(refresh_of(AgentType::Codex, vec![pin]), start));
        let notices = state.notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0].binary, link);
        let wrap = HookLineSender {
            deck_build: Some("0.46.0-gabc1234".into()),
            deck_exe: Some(abs("/home/u/.local/bin/dot-agent-deck")),
        };
        assert!(!state.observe_at(&AgentType::Codex, &wrap, start + CLEAR_AFTER * 2));
        assert_eq!(state.notices().len(), 1);
    }

    /// Scenario (audit A2): a refresh resolves the pins it read through the
    /// filesystem while it is being collected, before the daemon takes its
    /// state lock; applying it under the lock resolves nothing.
    #[test]
    fn a_refresh_touches_the_filesystem_only_before_the_lock() {
        let dir = crate::test_temp::tempdir().unwrap();
        let pinned = dir.path().join("pinned").join("dot-agent-deck");
        let own = dir.path().join("own").join("dot-agent-deck");
        std::fs::create_dir_all(own.parent().unwrap()).unwrap();
        std::fs::write(&own, b"").unwrap();
        let mut state = codex_unpinned();
        state.deck.exe = std::fs::canonicalize(&own).ok();
        FS_RESOLUTIONS.with(|count| count.set(0));
        let refresh = PinRefresh::classify(
            &state.deck,
            vec![(
                AgentType::Codex,
                vec![
                    HookPin {
                        agent: AgentType::Codex,
                        config: PathBuf::from("/cfg"),
                        binary: own.to_string_lossy().into_owned(),
                    },
                    HookPin {
                        agent: AgentType::Codex,
                        config: PathBuf::from("/cfg"),
                        binary: pinned.to_string_lossy().into_owned(),
                    },
                    HookPin {
                        agent: AgentType::Codex,
                        config: PathBuf::from("/cfg"),
                        binary: "relative/dot-agent-deck".into(),
                    },
                ],
            )],
        );
        assert!(FS_RESOLUTIONS.with(std::cell::Cell::get) > 0);
        let pins = &refresh.agents[0].1;
        assert_eq!(pins.len(), 2, "a relative pin is not probed: {pins:?}");
        assert!(pins[0].trusted.is_self && pins[0].probe.is_none());
        assert!(!pins[1].trusted.is_self && pins[1].probe.is_some());
        FS_RESOLUTIONS.with(|count| count.set(0));
        state.apply_refresh(refresh);
        let _ = state.notices();
        assert_eq!(FS_RESOLUTIONS.with(std::cell::Cell::get), 0);
        assert_eq!(state.pins[&AgentType::Codex].len(), 2);
    }
}
