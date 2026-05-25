//! PRD #103 Phase 2 — local TUI build-version handshake against the
//! external daemon.
//!
//! After [`crate::daemon_attach::ensure_external_daemon_or_die`] returns
//! Ok, the TUI opens one short-lived attach connection and sends a
//! [`crate::daemon_protocol::AttachRequest::Hello`] carrying its own
//! `client_build_version`. The daemon replies with its compiled-in
//! `build_version` (PRD #103 M1.1). If the two differ — or the daemon
//! omits the field entirely (a pre-PRD-103 binary) — the handshake
//! enters a recovery flow:
//!
//! - **TTY**: render an interactive prompt naming both build-ids and the
//!   live agent IDs (from a `ListAgents` round-trip). The user can press
//!   `S` to terminate the daemon and continue, or `Q`/`Ctrl+C`/`Ctrl+D`
//!   to abort. When live agents are present, two consecutive `S` presses
//!   are required (data-loss guard).
//! - **Non-TTY** (CI, piped stdout): print the equivalent error to
//!   stderr and exit non-zero. No prompt is rendered; the documented
//!   recovery is `dot-agent-deck daemon stop`.
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
use crate::daemon_client::{DaemonClient, issue_command};
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
    /// Build-ids differed; the user pressed `S`, the old daemon was
    /// terminated, and the socket disappeared within the timeout. The
    /// caller should re-run [`crate::daemon_attach::ensure_external_daemon_or_die`]
    /// before attaching — the next TUI startup lazy-spawns a fresh daemon
    /// at the current build.
    Recovered,
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

/// Public entry point used by `run_tui_session`. Performs the handshake,
/// runs the recovery flow on mismatch, and returns once the laptop can
/// safely attach (or `Err` if the user aborted / something failed).
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

    // Mismatch path. Pull the live-agent list so the prompt can name
    // them; treat a list_agents error as "no agents listed" — the
    // mismatch itself is the important user signal, and a failing
    // list_agents on top would just confuse the picture. (If the
    // daemon is too broken to list agents it almost certainly can't
    // host live ones either.)
    let agents = match DaemonClient::new(attach_path.to_path_buf())
        .list_agents()
        .await
    {
        Ok(rs) => rs.into_iter().map(|r| r.id).collect::<Vec<_>>(),
        Err(e) => {
            tracing::debug!(
                target: "build_version_handshake",
                error = %e,
                "list_agents failed during mismatch flow; treating as no live agents"
            );
            Vec::new()
        }
    };
    tracing::debug!(
        target: "build_version_handshake",
        local_build,
        daemon_build = ?daemon_build,
        live_agents = agents.len(),
        "local daemon build_version handshake: mismatch"
    );

    if !std::io::stdout().is_terminal() {
        let msg = render_non_tty_error(daemon_build.as_deref(), &local_build);
        eprint!("{msg}");
        return Err(HandshakeError::MismatchAborted);
    }

    // TTY recovery flow: ask the user whether to terminate. The prompt
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
        InteractiveDecision::Quit => Err(HandshakeError::MismatchAborted),
        InteractiveDecision::Stop => {
            // Phase 2 doesn't escalate to SIGKILL — the user can re-run
            // `dot-agent-deck daemon stop --force` for that. The
            // graceful-SIGTERM outcome is the only success variant we
            // care about here; treat both Stopped and Killed (the
            // latter is unreachable with `None`) as Recovered.
            terminate_daemon_graceful(probe.peer_pid, attach_path, TERMINATE_POLL_TIMEOUT, None)
                .await?;
            Ok(HandshakeOutcome::Recovered)
        }
    }
}

struct ProbeOutcome {
    response: AttachResponse,
    peer_pid: u32,
}

async fn probe_daemon(attach_path: &Path) -> Result<ProbeOutcome, HandshakeError> {
    let stream = UnixStream::connect(attach_path)
        .await
        .map_err(HandshakeError::Probe)?;
    let pid = peer_pid(&stream).map_err(HandshakeError::PeerPid)?;
    let (mut rd, mut wr) = stream.into_split();
    let req = AttachRequest::Hello {
        client_version: PROTOCOL_VERSION,
        // Same `local_build_id()` the comparison uses — keeps the
        // wire-advertised `client_build_version` consistent with the
        // value we're matching against, even under the test-only
        // `DOT_AGENT_DECK_BUILD_ID_OVERRIDE`.
        client_build_version: Some(local_build_id()),
    };
    let resp = issue_command(&mut rd, &mut wr, &req)
        .await
        .map_err(|e| HandshakeError::Probe(io::Error::other(e.to_string())))?;
    Ok(ProbeOutcome {
        response: resp,
        peer_pid: pid,
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
/// the cursor sits one line below the prompt body — the next keystroke
/// then echoes (well, would echo if raw mode left echo on) under the
/// "[S] / [Q]" line.
fn render_mismatch_prompt(
    daemon_build: Option<&str>,
    local_build: &str,
    agents: &[String],
) -> String {
    let daemon_display = daemon_build.unwrap_or("<unknown>");
    if agents.is_empty() {
        format!(
            "⚠  Daemon version mismatch\n\
             \x20  running daemon:  {daemon_display}\n\
             \x20  this binary:     {local_build}\n\
             \n\
             \x20  [S] stop daemon and continue   [Q] quit\n"
        )
    } else {
        let mut out = String::new();
        out.push_str(&format!(
            "⚠  Daemon version mismatch  ({n} managed agent(s) running)\n",
            n = agents.len()
        ));
        for id in agents {
            out.push_str(&format!("   {id}\n"));
        }
        out.push('\n');
        out.push_str("   Stopping the daemon will end these agents.\n");
        out.push('\n');
        out.push_str("   [S] stop daemon and continue   [Q] quit\n");
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractiveDecision {
    Stop,
    Quit,
}

/// Render the prompt in raw mode and block on `crossterm::event::read`
/// until the user picks `S` or `Q` (or sends `Ctrl+C`/`Ctrl+D`). When
/// `agents` is non-empty, two `S` presses are required — the live-agent
/// data-loss guard (PRD #103 M2.1 / worker-task line 41).
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

    let needs_two_s = !agents.is_empty();
    let mut s_count = 0usize;

    loop {
        let ev = match event::read() {
            Ok(ev) => ev,
            // A read error in raw mode (terminal yanked, TTY closed)
            // is functionally equivalent to the user giving up. Quit
            // safely rather than looping on a broken fd.
            Err(_) => return InteractiveDecision::Quit,
        };
        let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        else {
            continue;
        };
        match (code, modifiers) {
            (KeyCode::Char('s' | 'S'), m) if !m.contains(KeyModifiers::CONTROL) => {
                s_count += 1;
                if !needs_two_s || s_count >= 2 {
                    return InteractiveDecision::Stop;
                }
                // First S with live agents: re-render the prompt so
                // the user sees that the keystroke was received but
                // a second confirmation is still required. The text
                // is unchanged (PRD pins it character-for-character).
                render(&prompt);
            }
            (KeyCode::Char('q' | 'Q'), m) if !m.contains(KeyModifiers::CONTROL) => {
                return InteractiveDecision::Quit;
            }
            (KeyCode::Char('c' | 'd'), m) if m.contains(KeyModifiers::CONTROL) => {
                return InteractiveDecision::Quit;
            }
            (KeyCode::Esc, _) => return InteractiveDecision::Quit,
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
    fn mismatch_prompt_no_agents_matches_prd_form() {
        // PRD #103 M2.1 pins this text character-for-character; the
        // Phase 4 integration tests (M4.2) will assert against the
        // same form, so any drift here fails the future test too.
        let out = render_mismatch_prompt(Some("0.25.0-gabc1234"), "0.25.0-gdeadbee", &[]);
        assert_eq!(
            out,
            "⚠  Daemon version mismatch\n\
             \x20  running daemon:  0.25.0-gabc1234\n\
             \x20  this binary:     0.25.0-gdeadbee\n\
             \n\
             \x20  [S] stop daemon and continue   [Q] quit\n"
        );
    }

    #[test]
    fn mismatch_prompt_with_agents_lists_them_under_header_and_warns_about_data_loss() {
        let out = render_mismatch_prompt(
            Some("0.25.0-gabc1234"),
            "0.25.0-gdeadbee",
            &["agent-1".into(), "agent-2".into()],
        );
        let expected = "⚠  Daemon version mismatch  (2 managed agent(s) running)\n\
             \x20  agent-1\n\
             \x20  agent-2\n\
             \n\
             \x20  Stopping the daemon will end these agents.\n\
             \n\
             \x20  [S] stop daemon and continue   [Q] quit\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn mismatch_prompt_pluralization_pinned_at_n_managed_agents() {
        // The PRD-pinned form is "(N managed agent(s) running)" with a
        // literal "(s)" — no clever singular/plural switching. Pin it
        // so a future "be helpful" cleanup doesn't drift the string
        // out from under the M4.2 assertions.
        let single = render_mismatch_prompt(Some("a"), "b", &["only".into()]);
        assert!(
            single.contains("(1 managed agent(s) running)"),
            "single-agent header must keep the literal '(s)', got: {single:?}"
        );
    }

    #[tokio::test]
    async fn terminate_rejects_pid_zero() {
        // kill(0, SIGTERM) is "send to every process in the calling
        // process's process group" — refusing pid 0 up front prevents
        // a sentinel/uninitialized value from accidentally taking
        // down the parent shell.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("ignored.sock");
        let err = terminate_daemon_graceful(0, &sock, Duration::from_millis(100), None)
            .await
            .expect_err("pid 0 must be rejected");
        match err {
            HandshakeError::TerminateFailed(e) => {
                assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
            }
            other => panic!("expected TerminateFailed(InvalidInput), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn terminate_returns_stopped_when_socket_disappears() {
        // Bind a real Unix socket, spawn a kill+unlink helper, and
        // confirm `terminate_daemon_graceful` returns `Stopped` once
        // the socket vanishes. We can't actually SIGTERM ourselves in
        // a test (test runner dies), so we target a freshly forked
        // child that never receives any visible SIGTERM and instead
        // unlink the socket out-of-band partway through — exercising
        // just the polling half.
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("attach.sock");
        let _listener = UnixListener::bind(&sock).unwrap();

        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();

        let sock_for_drop = sock.clone();
        let unlink_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let _ = std::fs::remove_file(&sock_for_drop);
        });

        let result = terminate_daemon_graceful(pid, &sock, Duration::from_secs(2), None).await;
        unlink_task.await.unwrap();
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            matches!(result, Ok(TerminateOutcome::Stopped)),
            "expected Ok(Stopped) once socket disappears, got {result:?}"
        );
    }

    #[tokio::test]
    async fn terminate_returns_stopped_when_listener_stops_accepting() {
        // Production daemons don't unlink their socket file on exit —
        // the inode lingers and only the next `daemon serve`'s bind()
        // cleanup removes it. The poll loop therefore can't rely on
        // file existence; it must also accept "connect failed" as
        // "daemon is gone". This test pins that behavior: bind a
        // listener, drop it (closes the fd; the socket file lingers
        // but connect returns ECONNREFUSED), and confirm
        // terminate_daemon_graceful exits Stopped.
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("attach.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();

        let listener_holder = std::sync::Mutex::new(Some(listener));
        let drop_task = {
            let listener_holder = &listener_holder;
            async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                let _ = listener_holder.lock().unwrap().take();
            }
        };

        let (result, _) = tokio::join!(
            terminate_daemon_graceful(pid, &sock, Duration::from_secs(2), None),
            drop_task,
        );
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            matches!(result, Ok(TerminateOutcome::Stopped)),
            "expected Ok(Stopped) once the listener stops accepting (socket file may persist), got {result:?}"
        );
        // Sanity: the file IS still present — proves we detected
        // daemon-gone via the connect-fail path, not the file-gone
        // path.
        assert!(
            sock.exists(),
            "regression: this test must exercise the connect-fail path, but the socket file was unlinked"
        );
    }

    #[tokio::test]
    async fn terminate_returns_killed_when_force_escalates() {
        // SIGTERM-ignoring child + force_kill_after = Some: SIGKILL
        // takes the child down (uncatchable), the listener fd closes,
        // the connect-fail poll fires, and we return Killed.
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("attach.sock");
        // Bind the listener inside the child's lifetime so its fd is
        // held by the child (via `nc -lU` or similar). Without a real
        // bound listener inside the child, the test process's
        // listener stays alive across the SIGKILL and connect-fail
        // never trips. Use `sh` to bind via a Python one-liner... too
        // brittle. Cheaper approach: bind in this process and CLOSE
        // it AFTER the SIGTERM grace times out — simulating "daemon
        // process held the listener, SIGKILL closes the daemon's fd
        // → listener dies".
        let listener = UnixListener::bind(&sock).unwrap();
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("trap '' TERM; sleep 30")
            .spawn()
            .expect("spawn ignoring-sigterm child");
        let pid = child.id();

        // After ~250ms (well past SIGTERM grace of 200ms below) drop
        // the listener — that's the moment SIGKILL would close the
        // daemon's listener fd in production.
        let listener_holder = std::sync::Mutex::new(Some(listener));
        let drop_task = {
            let listener_holder = &listener_holder;
            async move {
                tokio::time::sleep(Duration::from_millis(350)).await;
                let _ = listener_holder.lock().unwrap().take();
            }
        };

        let (result, _) = tokio::join!(
            terminate_daemon_graceful(
                pid,
                &sock,
                Duration::from_millis(200),
                Some(Duration::from_secs(2)),
            ),
            drop_task,
        );

        let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        let _ = child.wait();

        assert!(
            matches!(result, Ok(TerminateOutcome::Killed)),
            "expected Ok(Killed) on SIGKILL escalation, got {result:?}"
        );
    }

    #[tokio::test]
    async fn terminate_times_out_when_socket_never_disappears() {
        // The SIGTERM hits a real (sleeping) child that ignores the
        // signal and stays alive; we cap the wait at 200 ms and
        // expect `TerminateTimedOut`. This pins the failure-mode
        // contract: if the daemon doesn't go away, the recovery flow
        // surfaces a clean error instead of hanging.
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("attach.sock");
        let _listener = UnixListener::bind(&sock).unwrap();

        // `trap '' TERM; sleep 5` ignores SIGTERM so the socket
        // (which we don't unlink) stays present. The polling loop
        // must time out.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("trap '' TERM; sleep 5")
            .spawn()
            .expect("spawn ignoring-sigterm child");
        let pid = child.id();

        let result = terminate_daemon_graceful(pid, &sock, Duration::from_millis(200), None).await;

        // Tear down with SIGKILL — the test must not leak the child.
        let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        let _ = child.wait();

        assert!(
            matches!(result, Err(HandshakeError::TerminateTimedOut)),
            "expected TerminateTimedOut when socket lingers, got {result:?}"
        );
    }
}
