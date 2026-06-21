//! PRD #103 Phase 2 / PRD #161 Part A — local TUI build-version handshake
//! against the external daemon.
//!
//! After [`crate::daemon_attach::ensure_external_daemon_or_die`] returns
//! Ok, the TUI opens one short-lived attach connection and sends a
//! [`crate::daemon_protocol::AttachRequest::Hello`] carrying its own
//! `client_build_version`. The daemon replies with its compiled-in
//! `build_version` plus a `running_agents` summary (count + display
//! names; PRD #161 M1.1). If the two build-ids differ — or the daemon
//! omits the field entirely (a pre-PRD-103 binary) — PRD #161 D2's
//! consent-based always-restart (option A) takes over. The decision is
//! driven by whether agents are running and whether stdout is a TTY:
//!
//! - **No agents running** (regardless of TTY): nothing to lose, so the
//!   old daemon is restarted SILENTLY — no prompt, no consent. The caller
//!   re-runs `ensure_external_daemon_or_die`, which lazy-spawns a fresh
//!   daemon at the current build ([`HandshakeOutcome::Recovered`]).
//! - **Agents running + TTY**: render an interactive prompt that NAMES the
//!   live agents (their display names, from the hello reply's
//!   `running_agents`) and states that restarting stops them. A single
//!   `s`/`S` consents to the restart ([`HandshakeOutcome::Recovered`]);
//!   any dismiss key (`Esc`/`q`/`n`/`Enter`/`Ctrl+C`/`Ctrl+D`) DECLINES,
//!   keeping the existing daemon and attaching to it unchanged
//!   ([`HandshakeOutcome::ProceedOnExisting`]) so the user never loses
//!   their running agents (D4 never-strand).
//! - **Agents running + Non-TTY** (CI, piped stdout): the restart is
//!   mandatory but can't get consent on a pipe, so print a daemon-recovery
//!   hint to stderr and exit non-zero. No prompt is rendered; the
//!   documented recovery is `dot-agent-deck daemon stop`. This is the only
//!   non-zero-exit path.
//!
//! The handshake runs unconditionally — even when
//! [`ensure_external_daemon_or_die`] just lazy-spawned the daemon and
//! the build-ids are necessarily equal (PRD M2.3). The cost is one
//! extra Unix-socket round-trip on cold start; the upside is a smoke
//! test of the handshake on every launch, which catches regressions
//! in `ensure_external_daemon_or_die` itself or in the wire format.
//!
//! The `SIGTERM` + poll-socket-disappearance helper
//! ([`terminate_daemon_graceful`]) is factored out so PRD #103 Phase 3
//! (`dot-agent-deck daemon stop`) can reuse it — Phase 3 adds an
//! optional `SIGKILL` escalation on top, but the graceful path is
//! identical.

use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use tokio::net::UnixStream;

use crate::build_id::local_build_id;
use crate::daemon_attach::peer_pid;
use crate::daemon_client::issue_command;
use crate::daemon_protocol::{AttachRequest, AttachResponse, PROTOCOL_VERSION};

/// How long to wait for the daemon's attach socket to disappear after a
/// `SIGTERM` before reporting failure. The daemon's own teardown runs a
/// 3-second SIGTERM grace on its agents, so 5 s is comfortable headroom.
pub const TERMINATE_POLL_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of [`ensure_compatible_daemon_or_die`] when the call resolves
/// successfully. Callers proceed to attach in either case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeOutcome {
    /// Daemon's `build_version` matched the TUI's compiled-in
    /// `DAD_BUILD_ID`. No user interaction occurred.
    Match,
    /// Build-ids differed and the old daemon was terminated — either
    /// silently because no agents were running, or after the user pressed
    /// `s` on the TTY consent prompt. The socket disappeared within the
    /// timeout. The caller should re-run
    /// [`crate::daemon_attach::ensure_external_daemon_or_die`] before
    /// attaching — the next TUI startup lazy-spawns a fresh daemon at the
    /// current build.
    Recovered,
    /// Build-ids differed and agents were running, but the user DECLINED
    /// the restart on a TTY (any dismiss key). The existing (older) daemon
    /// is kept and the caller proceeds to attach to it UNCHANGED — the user
    /// keeps their running agents (PRD #161 D4 never-strand). The caller
    /// must NOT re-spawn the daemon.
    ProceedOnExisting,
}

/// Errors that abort startup. The non-TTY fallback prints to stderr
/// inside [`ensure_compatible_daemon_or_die`] before returning the
/// `MismatchAborted` variant, and the interactive `Quit` path is also
/// already user-visible — callers translate any error into
/// [`std::process::ExitCode::FAILURE`] without rendering anything else.
#[derive(Debug)]
pub enum HandshakeError {
    /// `UnixStream::connect` or the Hello round-trip failed. Indicates
    /// the daemon socket is reachable (it exists per
    /// `ensure_external_daemon_or_die`) but the daemon itself is not
    /// answering — a near-miss with a crashing daemon, or a stale
    /// socket inode that survived a kill -9 + reboot. Treated as a
    /// pre-flight failure.
    Probe(io::Error),
    /// `peer_pid()` on the connected socket failed. macOS/Linux both
    /// support the getsockopt this calls, so this is exceptional and
    /// indicates a fundamentally broken host.
    PeerPid(io::Error),
    /// The build-id mismatch was reported and the user aborted (TTY
    /// `Q`/`Ctrl+C`/`Ctrl+D`, or the non-TTY fallback).
    MismatchAborted,
    /// The user agreed to terminate the daemon but the SIGTERM didn't
    /// take effect within [`TERMINATE_POLL_TIMEOUT`] (socket still
    /// present). The user-facing remediation is
    /// `dot-agent-deck daemon stop --force`.
    TerminateTimedOut,
    /// `libc::kill` itself failed (EPERM if the daemon belongs to a
    /// different user — shouldn't happen because the attach socket is
    /// already trust-checked uid-equal — or ESRCH if the daemon died
    /// between probe and kill).
    TerminateFailed(io::Error),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Probe(e) => write!(f, "build-version handshake probe failed: {e}"),
            Self::PeerPid(e) => write!(f, "build-version handshake peer-pid lookup failed: {e}"),
            Self::MismatchAborted => write!(f, "build-version handshake aborted by user"),
            Self::TerminateTimedOut => write!(
                f,
                "daemon did not exit within {}s after SIGTERM; try `dot-agent-deck daemon stop --force`",
                TERMINATE_POLL_TIMEOUT.as_secs()
            ),
            Self::TerminateFailed(e) => write!(f, "SIGTERM to daemon failed: {e}"),
        }
    }
}

impl std::error::Error for HandshakeError {}

/// Public entry point used by `run_tui_session`. Performs the handshake
/// and runs PRD #161 Part A's consent-based recovery on a build-id
/// mismatch, returning once the laptop can safely attach — either to a
/// freshly-restarted daemon ([`HandshakeOutcome::Recovered`]) or to the
/// existing one when the user declined the restart
/// ([`HandshakeOutcome::ProceedOnExisting`]). The only `Err` return is the
/// agents-running non-TTY mandatory-restart path
/// ([`HandshakeError::MismatchAborted`], after a stderr hint) and genuine
/// pre-flight failures (probe / peer-pid / SIGTERM).
///
/// PRD M2.3 — runs unconditionally, even when
/// [`crate::daemon_attach::ensure_external_daemon_or_die`] just lazy-spawned
/// the daemon. The cost is one extra Unix-socket round-trip on cold start;
/// the upside is that the handshake itself gets a smoke test on every
/// launch, which catches regressions in either `ensure_external_daemon_or_die`
/// (wrong socket, wrong daemon binary) or the wire-format encoding of the
/// `build_version` field.
pub async fn ensure_compatible_daemon_or_die(
    attach_path: &Path,
) -> Result<HandshakeOutcome, HandshakeError> {
    // `local_build_id()` returns the compile-time `env!("DAD_BUILD_ID")`
    // in production; the `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` env var
    // (test-only, honoured by both this comparison and
    // `AttachResponse::hello`) lets PRD #103 M4.2 integration tests
    // simulate skew without rebuilding the binary.
    let local_build = local_build_id();
    let probe = probe_daemon(attach_path).await?;

    let daemon_build = probe.response.build_version.clone();
    if daemon_build.as_deref() == Some(local_build.as_str()) {
        tracing::debug!(
            target: "build_version_handshake",
            local_build,
            daemon_build = ?daemon_build,
            "local daemon build_version handshake: match"
        );
        return Ok(HandshakeOutcome::Match);
    }

    // Mismatch path (PRD #161 D2, option A — consent-based always-restart).
    // Read the running-agent summary straight from the hello reply
    // (`running_agents`, PRD #161 M1.1) so the prompt can name the live
    // agents by their DISPLAY names — not a `list_agents()` round-trip,
    // which yields generated ids. A pre-PRD-161 daemon omits the field, in
    // which case we have no agents to enumerate and treat it as "no agents".
    let agents: Vec<String> = probe
        .response
        .running_agents
        .as_ref()
        .map(|s| s.names.clone())
        .unwrap_or_default();
    tracing::debug!(
        target: "build_version_handshake",
        local_build,
        daemon_build = ?daemon_build,
        live_agents = agents.len(),
        "local daemon build_version handshake: mismatch"
    );

    // (a) No agents running: nothing to lose, so restart SILENTLY —
    // regardless of TTY. No prompt, no consent. The caller re-runs
    // `ensure_external_daemon_or_die` after `Recovered`, lazy-spawning a
    // fresh daemon at the current build.
    if agents.is_empty() {
        return terminate_and_recover(&probe, attach_path).await;
    }

    // (c) Agents running + non-TTY: the restart is mandatory but can't get
    // consent on a pipe, so print the daemon-recovery hint to stderr and
    // exit non-zero. This is the only non-zero-exit path.
    if !std::io::stdout().is_terminal() {
        let msg = render_non_tty_error(daemon_build.as_deref(), &local_build);
        eprint!("{msg}");
        return Err(HandshakeError::MismatchAborted);
    }

    // (b) Agents running + TTY: ask the user whether to restart. The prompt
    // is rendered in raw mode on a blocking thread (crossterm's
    // event::read is synchronous; doing it directly would block the
    // tokio worker).
    let agents_for_prompt = agents.clone();
    let daemon_build_for_prompt = daemon_build.clone();
    let local_build_for_prompt = local_build.clone();
    let decision = tokio::task::spawn_blocking(move || {
        interactive_prompt(
            daemon_build_for_prompt.as_deref(),
            &local_build_for_prompt,
            &agents_for_prompt,
        )
    })
    .await
    .map_err(|e| HandshakeError::Probe(io::Error::other(format!("prompt task join: {e}"))))?;

    match decision {
        // Decline keeps the existing (older) daemon: proceed to attach to
        // it unchanged so the user's running agents stay reachable (PRD
        // #161 D4 never-strand). The caller must NOT re-spawn.
        InteractiveDecision::Decline => Ok(HandshakeOutcome::ProceedOnExisting),
        InteractiveDecision::Restart => terminate_and_recover(&probe, attach_path).await,
    }
}

/// SIGTERM the probed daemon and report [`HandshakeOutcome::Recovered`] so
/// the caller re-runs `ensure_external_daemon_or_die` to lazy-spawn a fresh
/// daemon at the current build. Shared by the silent no-agents restart (a)
/// and the consented restart (b).
async fn terminate_and_recover(
    probe: &ProbeOutcome,
    attach_path: &Path,
) -> Result<HandshakeOutcome, HandshakeError> {
    // PRD #103 PID-reuse mitigation: re-resolve `peer_pid()` on the SAME
    // `UnixStream` we kept open across the (possible) interactive prompt.
    // Holding the stream open prevents the kernel from tearing down the
    // socket pairing in the window where the user was deciding. If
    // `peer_pid()` now fails the daemon has already exited on its own —
    // there's nothing to terminate, so short-circuit to success rather
    // than signalling an arbitrary same-UID PID.
    let resolved_pid = match peer_pid(&probe.stream) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(
                target: "build_version_handshake",
                error = %e,
                original_pid = probe.peer_pid,
                "re-resolved peer_pid failed before restart; treating daemon as already gone"
            );
            return Ok(HandshakeOutcome::Recovered);
        }
    };
    // Part A doesn't escalate to SIGKILL — the user can re-run
    // `dot-agent-deck daemon stop --force` for that. The graceful-SIGTERM
    // outcome is the only success variant we care about here; treat both
    // Stopped and Killed (the latter is unreachable with `None`) as
    // Recovered.
    terminate_daemon_graceful(resolved_pid, attach_path, TERMINATE_POLL_TIMEOUT, None).await?;
    Ok(HandshakeOutcome::Recovered)
}

struct ProbeOutcome {
    response: AttachResponse,
    peer_pid: u32,
    /// The original connected `UnixStream` is held alive across the
    /// interactive prompt so the kernel can't recycle the daemon's PID
    /// while the user is deciding S/Q (PRD #103 PID-reuse mitigation —
    /// see [`ensure_compatible_daemon_or_die`]). Used for the
    /// re-resolved `peer_pid()` call right before SIGTERM.
    stream: UnixStream,
}

async fn probe_daemon(attach_path: &Path) -> Result<ProbeOutcome, HandshakeError> {
    let mut stream = UnixStream::connect(attach_path)
        .await
        .map_err(HandshakeError::Probe)?;
    let pid = peer_pid(&stream).map_err(HandshakeError::PeerPid)?;
    // Borrow-split so the original `stream` survives the Hello exchange
    // and can be held across the interactive prompt. `into_split()`
    // (which consumes the stream) would force us to reunite the halves
    // afterwards; the borrow-split is simpler.
    let resp = {
        let (mut rd, mut wr) = stream.split();
        let req = AttachRequest::Hello {
            client_version: PROTOCOL_VERSION,
            // Same `local_build_id()` the comparison uses — keeps the
            // wire-advertised `client_build_version` consistent with the
            // value we're matching against, even under the test-only
            // `DOT_AGENT_DECK_BUILD_ID_OVERRIDE`.
            client_build_version: Some(local_build_id()),
        };
        issue_command(&mut rd, &mut wr, &req)
            .await
            .map_err(|e| HandshakeError::Probe(io::Error::other(e.to_string())))?
    };
    Ok(ProbeOutcome {
        response: resp,
        peer_pid: pid,
        stream,
    })
}

/// What [`terminate_daemon_graceful`] actually did when it succeeded.
/// Phase 2 ignores the variant; Phase 3 (`daemon stop`) surfaces the
/// distinction to the user (`stopped` vs `force-killed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminateOutcome {
    /// SIGTERM was sufficient — the daemon went away within
    /// `grace_timeout`.
    Stopped,
    /// SIGTERM timed out and the caller had passed
    /// `force_kill_after = Some(...)`, so SIGKILL was sent and the
    /// daemon went away within that extra window.
    Killed,
}

/// SIGTERM the daemon (by PID, obtained from `peer_pid()` on the attach
/// socket) and poll for the daemon to actually go away. `daemon-is-gone`
/// is detected via `UnixStream::connect` failure (or the socket file
/// being unlinked), polled every 100 ms up to `grace_timeout`. We don't
/// rely on the socket *file* disappearing because the daemon doesn't
/// unlink its socket inode on exit — only `bind(2)` cleanup on the
/// next start does — so file-existence polling would unconditionally
/// time out against a real production daemon.
///
/// Phase 2 (TUI prompt) calls this with `force_kill_after = None` and
/// surfaces a timeout as [`HandshakeError::TerminateTimedOut`]. Phase 3
/// (`daemon stop --force`) passes a non-`None` second window: on
/// SIGTERM timeout we escalate to SIGKILL and poll for an additional
/// `force_kill_after` before giving up. The graceful path is shared so
/// the two flows can't diverge.
///
/// Returns:
/// - `Ok(TerminateOutcome::Stopped)` — SIGTERM took effect within
///   `grace_timeout`.
/// - `Ok(TerminateOutcome::Killed)` — SIGTERM timed out, SIGKILL was
///   delivered, and the daemon went away within `force_kill_after`.
///   Only reachable when `force_kill_after.is_some()`.
/// - `Err(HandshakeError::TerminateTimedOut)` — daemon still alive after
///   both timeouts (or just `grace_timeout` if no escalation requested).
/// - `Err(HandshakeError::TerminateFailed(_))` — `libc::kill` itself
///   failed (typically ESRCH if the daemon already exited between
///   probe and kill, or EINVAL if the caller passed pid 0 — refused up
///   front).
pub async fn terminate_daemon_graceful(
    pid: u32,
    attach_path: &Path,
    grace_timeout: Duration,
    force_kill_after: Option<Duration>,
) -> Result<TerminateOutcome, HandshakeError> {
    let signal_pid = checked_signal_pid(pid)?;
    // TOCTOU residual risk: between `peer_pid()` (on the connected
    // attach socket) reading this PID and the SIGTERM below, the
    // daemon could exit and the kernel could recycle the PID for an
    // unrelated process. We accept the window because:
    //   1. The caller (handshake / `daemon stop`) holds the attach
    //      socket connection open across this call. While the kernel
    //      considers our peer alive on that fd, it won't reuse the
    //      PID for *another* process — recycling only happens after
    //      the original process is fully reaped. So a same-UID PID
    //      collision requires the daemon to die, reap, and a new
    //      same-UID process to claim the recycled PID, all within
    //      one syscall worth of latency. Vanishingly small in
    //      practice.
    //   2. If we do hit that window, the worst case is a SIGTERM
    //      delivered to an unrelated same-UID process — uid-equality
    //      is already enforced by the daemon's attach-socket trust
    //      check upstream, so we can't cross a security boundary.
    // Documenting rather than mitigating further: the `kill(pid, 0)`
    // double-check the auditor suggested would only narrow the window,
    // not close it (the recycle could still happen between the
    // `kill(_, 0)` and the `kill(_, SIGTERM)`).
    // SAFETY: `libc::kill` is async-signal-safe and has no in-process
    // side effects beyond delivering the signal to the target PID.
    let rc = unsafe { libc::kill(signal_pid, libc::SIGTERM) };
    if rc != 0 {
        return Err(HandshakeError::TerminateFailed(io::Error::last_os_error()));
    }
    if poll_daemon_gone(attach_path, grace_timeout).await {
        return Ok(TerminateOutcome::Stopped);
    }
    let Some(kill_grace) = force_kill_after else {
        return Err(HandshakeError::TerminateTimedOut);
    };
    // SAFETY: same as above; SIGKILL is uncatchable but the syscall
    // itself is async-signal-safe.
    let rc = unsafe { libc::kill(signal_pid, libc::SIGKILL) };
    if rc != 0 {
        return Err(HandshakeError::TerminateFailed(io::Error::last_os_error()));
    }
    if poll_daemon_gone(attach_path, kill_grace).await {
        Ok(TerminateOutcome::Killed)
    } else {
        Err(HandshakeError::TerminateTimedOut)
    }
}

/// Convert a `u32` PID (as returned by `peer_pid()`) into the `pid_t`
/// (`i32`) shape `libc::kill` wants, refusing values that would
/// dangerously change the syscall's meaning:
/// - `pid == 0`: `kill(0, sig)` broadcasts to every process in the
///   calling process group — would take down the parent shell.
/// - `pid > i32::MAX`: the `as i32` cast would wrap to a negative
///   value. `kill(-pgid, sig)` means "signal every process in process
///   group `pgid`" — a wildcard kill. Refuse rather than send.
/// - resulting `i32 <= 0` after the cast: defense-in-depth for any
///   path that bypasses the explicit checks above.
fn checked_signal_pid(pid: u32) -> Result<libc::pid_t, HandshakeError> {
    if pid == 0 {
        return Err(HandshakeError::TerminateFailed(io::Error::new(
            io::ErrorKind::InvalidInput,
            "peer pid is 0; refusing to kill(0, SIGTERM) (would broadcast to process group)",
        )));
    }
    if pid > i32::MAX as u32 {
        return Err(HandshakeError::TerminateFailed(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "peer pid {pid} does not fit in pid_t; refusing kill() (negative i32 would target a process group)"
            ),
        )));
    }
    let signed = pid as libc::pid_t;
    if signed <= 0 {
        return Err(HandshakeError::TerminateFailed(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("peer pid {pid} resolves to non-positive pid_t {signed}; refusing kill()"),
        )));
    }
    Ok(signed)
}

/// Poll until the daemon stops answering connects on `attach_path`, or
/// `budget` elapses. Returns `true` on the former. Used by
/// [`terminate_daemon_graceful`]'s wait loops.
///
/// "Daemon is gone" is `UnixStream::connect` failure (typically
/// `ECONNREFUSED` against a stale inode after the listener fd was
/// closed, or `ENOENT` if something out-of-band unlinked the file).
/// File-existence alone is not a reliable signal — the daemon process
/// does not unlink its socket on exit; only the next `daemon serve`'s
/// `bind(2)` cleanup does — so a file-existence poll would unconditionally
/// time out against a real production daemon that just died.
async fn poll_daemon_gone(attach_path: &Path, budget: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < budget {
        if !attach_path.exists() {
            return true;
        }
        if tokio::net::UnixStream::connect(attach_path).await.is_err() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

// ---------------------------------------------------------------------------
// Rendering / interactive prompt
// ---------------------------------------------------------------------------

/// Render the non-TTY (CI / piped-stdout) error message. Trailing
/// newline included so the caller can write it directly.
fn render_non_tty_error(daemon_build: Option<&str>, local_build: &str) -> String {
    let daemon_display = daemon_build.unwrap_or("<unknown>");
    format!(
        "error: local daemon is build {daemon_display} but this TUI is build {local_build}\n\
         recover with: dot-agent-deck daemon stop\n"
    )
}

/// Render the interactive mismatch prompt as plain newline-separated
/// text. Raw-mode display converts `\n` to `\r\n` at write time so
/// tests can assert on the canonical form. Trailing newline omitted so
/// the cursor sits one line below the prompt body.
///
/// PRD #161 Part A: this prompt is only ever shown when agents are
/// running (the no-agents path restarts silently with no prompt), so it
/// always names the live agents (their display names, from the hello
/// reply's `running_agents`) and offers a single-key consent: `s` to
/// restart (stopping the agents), any other key to keep the existing
/// daemon and continue.
fn render_mismatch_prompt(
    daemon_build: Option<&str>,
    local_build: &str,
    agents: &[String],
) -> String {
    let daemon_display = daemon_build.unwrap_or("<unknown>");
    let mut out = String::new();
    out.push_str(&format!(
        "⚠  Daemon version mismatch  ({n} agent(s) running)\n",
        n = agents.len()
    ));
    out.push_str(&format!("   running daemon:  {daemon_display}\n"));
    out.push_str(&format!("   this binary:     {local_build}\n"));
    out.push('\n');
    out.push_str("   Restarting to upgrade will stop these agents:\n");
    for name in agents {
        out.push_str(&format!("   {name}\n"));
    }
    out.push('\n');
    out.push_str("   [S] restart daemon and continue   [any other key] keep current daemon\n");
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractiveDecision {
    /// Restart the daemon (stopping its agents) and continue on a fresh one.
    Restart,
    /// Keep the existing daemon and continue attached to it (agents intact).
    Decline,
}

/// Render the prompt in raw mode and block on `crossterm::event::read`
/// until the user makes a choice. PRD #161 Part A: a single `s`/`S`
/// consents to the restart; every dismiss key
/// (`Esc`/`q`/`n`/`Enter`/`Ctrl+C`/`Ctrl+D`) DECLINES, which keeps the
/// existing daemon (D4 never-strand). The old two-`S` double-confirm is
/// gone — there is no longer an exit path on a TTY, so a second
/// confirmation buys nothing.
///
/// Runs on a blocking thread because `crossterm::event::read` is
/// synchronous; the caller spawn_blocking()s this function.
fn interactive_prompt(
    daemon_build: Option<&str>,
    local_build: &str,
    agents: &[String],
) -> InteractiveDecision {
    use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
        }
    }

    let prompt = render_mismatch_prompt(daemon_build, local_build, agents);
    let raw_ok = enable_raw_mode().is_ok();
    let _guard = raw_ok.then_some(RawModeGuard);

    let render = |body: &str| {
        let mut out = std::io::stdout().lock();
        // Raw mode disables cooked LF→CRLF translation, so each `\n`
        // must become `\r\n` for the prompt to land on consecutive
        // left-aligned lines.
        let with_cr = body.replace('\n', "\r\n");
        let _ = out.write_all(with_cr.as_bytes());
        let _ = out.flush();
    };
    render(&prompt);

    loop {
        let ev = match event::read() {
            Ok(ev) => ev,
            // A read error in raw mode (terminal yanked, TTY closed) is
            // functionally equivalent to the user dismissing. Decline
            // safely (keeping the existing daemon) rather than looping on
            // a broken fd.
            Err(_) => return InteractiveDecision::Decline,
        };
        let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        else {
            continue;
        };
        match (code, modifiers) {
            // The ONLY affirmative key: a single `s`/`S` restarts.
            (KeyCode::Char('s' | 'S'), m) if !m.contains(KeyModifiers::CONTROL) => {
                return InteractiveDecision::Restart;
            }
            // Explicit dismiss keys all decline → keep the existing daemon.
            (KeyCode::Char('q' | 'Q' | 'n' | 'N'), m) if !m.contains(KeyModifiers::CONTROL) => {
                return InteractiveDecision::Decline;
            }
            (KeyCode::Char('c' | 'd'), m) if m.contains(KeyModifiers::CONTROL) => {
                return InteractiveDecision::Decline;
            }
            (KeyCode::Esc, _) | (KeyCode::Enter, _) => return InteractiveDecision::Decline,
            _ => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_tty_error_message_names_both_build_ids() {
        let msg = render_non_tty_error(Some("0.25.0-gabc1234"), "0.25.0-gdeadbee-dirty");
        assert_eq!(
            msg,
            "error: local daemon is build 0.25.0-gabc1234 but this TUI is build 0.25.0-gdeadbee-dirty\n\
             recover with: dot-agent-deck daemon stop\n"
        );
    }

    #[test]
    fn non_tty_error_message_renders_unknown_daemon_build() {
        // PRD #103 M2.1: a pre-PRD-103 daemon omits `build_version` on
        // the wire; the prompt still needs to surface a sensible
        // placeholder rather than `error: local daemon is build  but
        // this TUI is build ...` (note the empty span).
        let msg = render_non_tty_error(None, "0.25.0-gabc1234");
        assert!(
            msg.contains(
                "error: local daemon is build <unknown> but this TUI is build 0.25.0-gabc1234"
            ),
            "missing daemon build_version must surface <unknown> placeholder, got: {msg:?}"
        );
        assert!(msg.ends_with("recover with: dot-agent-deck daemon stop\n"));
    }

    #[test]
    fn mismatch_prompt_names_agents_and_offers_single_consent() {
        // PRD #161 Part A: the prompt is only shown when agents are
        // running (the no-agents path restarts silently with no prompt),
        // so it always names each live agent by its display name, states
        // that restarting stops them, and offers a single-key consent —
        // `s` to restart, any other key to keep the existing daemon. There
        // is no longer a "[Q] quit"/abort path or a two-`S` double-confirm.
        let out = render_mismatch_prompt(
            Some("0.31.0-g0000old"),
            "0.31.1-g1111new",
            &["zeta-live-77".into(), "alpha-2".into()],
        );
        let expected = "⚠  Daemon version mismatch  (2 agent(s) running)\n\
             \x20  running daemon:  0.31.0-g0000old\n\
             \x20  this binary:     0.31.1-g1111new\n\
             \n\
             \x20  Restarting to upgrade will stop these agents:\n\
             \x20  zeta-live-77\n\
             \x20  alpha-2\n\
             \n\
             \x20  [S] restart daemon and continue   [any other key] keep current daemon\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn mismatch_prompt_conveys_restart_vs_keep_choice() {
        // The new single-consent model: `s` restarts (stopping agents); any
        // other key keeps the current daemon (decline = proceed-on-existing,
        // D4 never-strand). No abort/quit wording survives.
        let out = render_mismatch_prompt(Some("old"), "new", &["only".into()]);
        assert!(
            out.contains("[S] restart daemon and continue"),
            "must offer the single `s` restart consent, got: {out:?}"
        );
        assert!(
            out.contains("keep current daemon"),
            "must offer keeping the existing daemon as the decline path, got: {out:?}"
        );
        assert!(
            out.to_lowercase().contains("stop"),
            "must state that restarting stops the agents, got: {out:?}"
        );
        assert!(
            !out.to_lowercase().contains("quit"),
            "the abort/quit path is gone under Part A, got: {out:?}"
        );
    }

    #[test]
    fn mismatch_prompt_single_agent_pluralization() {
        // The header form is "(N agent(s) running)" with a literal "(s)" —
        // no clever singular/plural switching. Pin it so a future "be
        // helpful" cleanup doesn't drift the string.
        let single = render_mismatch_prompt(Some("a"), "b", &["only".into()]);
        assert!(
            single.contains("(1 agent(s) running)"),
            "single-agent header must keep the literal '(s)', got: {single:?}"
        );
    }

    #[test]
    fn mismatch_prompt_renders_unknown_daemon_build() {
        // A pre-PRD-103 daemon omits `build_version`; the prompt surfaces
        // the `<unknown>` placeholder rather than an empty span.
        let out = render_mismatch_prompt(None, "0.31.1-g1111new", &["only".into()]);
        assert!(
            out.contains("running daemon:  <unknown>"),
            "missing daemon build_version must surface <unknown>, got: {out:?}"
        );
    }
}
