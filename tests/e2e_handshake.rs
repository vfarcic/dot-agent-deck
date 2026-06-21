#![cfg(feature = "e2e")]

//! L2 end-to-end coverage for the PRD #161 Part A shared TUI↔daemon
//! build-version handshake (`src/build_version_handshake.rs`, called from
//! `run_tui_session` in `src/main.rs`).
//!
//! PRD #161 D2 resolved the shared handshake to **option A** — always-restart,
//! *consent-based*. This demotes PRD #103's exact-build-id refusal to a
//! restart that is silent when no agents are running, prompted (and reversible)
//! when agents are, and never strands the user from their running agents (D4).
//! The cases pinned here, mapped to the `lifecycle/handshake/00N` catalog ids:
//!
//!   - 001 — MATCH: equal build-ids attach silently into the dashboard.
//!   - 002 — MISMATCH + no agents: the old daemon is restarted SILENTLY (no
//!     prompt) and the dashboard appears against a fresh daemon at the new
//!     build.
//!   - 003 — MISMATCH + agents + TTY: an interactive prompt NAMES the live
//!     agents and states restarting stops them.
//!   - 004 — MISMATCH + agents + non-TTY: the mandatory-restart path exits
//!     non-zero with a stderr recovery hint and no prompt.
//!   - 005 — MISMATCH + agents + TTY + ACCEPT: a single consent restarts the
//!     daemon; the agents are gone and the dashboard appears (replaces #103's
//!     two-`S` double-confirm).
//!   - 006 — MISMATCH + agents + TTY + DECLINE: declining keeps the EXISTING
//!     daemon — you land in a working dashboard against it with the agents
//!     still reachable (D4 never-strand; the key change from #103, where
//!     declining EXITED).
//!
//! Skew is simulated without rebuilding the binary via
//! `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` (honoured by both the daemon's `hello`
//! reply and the laptop's comparison under `cfg(debug_assertions)`): the
//! external daemon is started at `OLD_BUILD` and the TUI is launched at
//! `NEW_BUILD`, pointed at the daemon's sockets so it reuses that older daemon
//! and the handshake observes a mismatch.
//!
//! Gated behind the `e2e` feature so CI (`cargo test-fast`) never compiles it
//! (PRD #77 Decision 6). All polling lives in `common` helpers so these bodies
//! carry no raw `sleep` (linkage-check Decision 21 / rule 5).

mod common;

use std::time::Duration;

use common::{DaemonProc, TuiDeck, spawn_daemon_serve_with_env, wait_for_agent_display_name};
use dot_agent_deck::daemon_protocol::AttachRequest;
use spec::spec;

/// Synthetic build-id for the still-running OLDER daemon.
const OLD_BUILD: &str = "0.31.0-g0000old";
/// Synthetic build-id for the NEWER laptop TUI that attaches to it.
const NEW_BUILD: &str = "0.31.1-g1111new";
/// Distinctive display name for the live agent — deliberately free of the
/// substrings ("stop"/"restart"/"agent") the prompt-presence checks key on, so
/// asserting it appears proves the prompt surfaces the agent's *display name*
/// specifically (PRD #161 M1.1 `running_agents.names`), not its generated id.
const LIVE_AGENT_NAME: &str = "zeta-live-77";

/// Start an external `daemon serve` pinned to `build_id` via the test-only
/// `DOT_AGENT_DECK_BUILD_ID_OVERRIDE`. Idle shutdown is disabled so the daemon
/// stays put while the TUI attaches.
fn spawn_daemon_at_build(build_id: &str) -> DaemonProc {
    spawn_daemon_serve_with_env(None, "0", &[("DOT_AGENT_DECK_BUILD_ID_OVERRIDE", build_id)])
}

/// Register one long-lived synthetic agent (`sleep`-style stub) on `daemon`
/// with [`LIVE_AGENT_NAME`] as its display name, and block until the daemon's
/// registry reports it. Mirrors the `StartAgent` synthetic-agent pattern used
/// by `e2e_reconnect_agent_type.rs` / `tests/rehydration.rs`.
fn start_live_agent(daemon: &DaemonProc) {
    let resp = daemon
        .send_attach_request(&AttachRequest::StartAgent {
            command: Some("sh -c 'sleep 600'".into()),
            cwd: None,
            rows: 24,
            cols: 80,
            env: vec![("DOT_AGENT_DECK_PANE_ID".into(), "pane-live".into())],
            display_name: Some(LIVE_AGENT_NAME.into()),
            tab_membership: None,
            agent_type: None,
        })
        .expect("StartAgent over the attach socket");
    assert!(
        resp.error.is_none(),
        "StartAgent should succeed, got error: {:?}",
        resp.error
    );
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(5));
    assert_eq!(
        records.len(),
        1,
        "the live agent must be registered before the TUI attaches"
    );
}

/// Launch the real TUI binary in a PTY at `build_id`, pointed at `daemon`'s
/// hook + attach sockets so `ensure_external_daemon_or_die` reuses that
/// already-running daemon and the build-version handshake fires against it.
fn launch_tui_against(daemon: &DaemonProc, build_id: &str) -> TuiDeck {
    TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            daemon.attach_socket.to_string_lossy().to_string(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            daemon.hook_socket.to_string_lossy().to_string(),
        )
        .with_env("DOT_AGENT_DECK_BUILD_ID_OVERRIDE", build_id)
        .launch_with_fixture("minimal")
}

/// Wait until the version-mismatch restart prompt is on screen. Matches on the
/// cross-version restart/stop intent (`stop`/`restart`, case-insensitive) so it
/// is robust to wording changes between #103 and Part A — both phrasings state
/// that continuing stops/restarts the daemon.
fn wait_for_restart_prompt(deck: &TuiDeck) {
    deck.wait_until_grid("version-mismatch restart prompt", |g| {
        let low = g.to_lowercase();
        low.contains("stop") || low.contains("restart")
    });
}

/// Scenario: Launch the TUI normally so it lazy-spawns its own daemon at the
/// same build — a build-version MATCH. The dashboard must appear with no
/// version-mismatch prompt rendered.
#[spec("lifecycle/handshake/001")]
#[test]
fn handshake_001_match_proceeds_silently_into_dashboard() {
    let deck = TuiDeck::launch_with_fixture("minimal");
    // Empty fresh daemon → the dashboard's empty-state line.
    deck.wait_for_string("No active sessions");
    // A matching build must never surface the mismatch prompt.
    let grid = deck.snapshot_grid().to_lowercase();
    assert!(
        !grid.contains("version mismatch") && !grid.contains("stop daemon"),
        "a build-version match must proceed silently, but the grid shows a \
         mismatch prompt:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Start an older daemon (OLD_BUILD) with NO agents, then attach a
/// newer TUI (NEW_BUILD). Part A restarts the daemon SILENTLY — no prompt — so
/// the dashboard appears against a fresh daemon at the new build and the old
/// daemon process exits, all without any keypress.
#[spec("lifecycle/handshake/002")]
#[test]
fn handshake_002_mismatch_no_agents_restarts_silently() {
    let mut daemon = spawn_daemon_at_build(OLD_BUILD);
    let deck = launch_tui_against(&daemon, NEW_BUILD);

    // No key is ever sent. Under Part A the silent restart lands us straight
    // in the empty dashboard. (Today #103 renders the no-agents prompt and
    // blocks on a keypress, so this wait times out — the RED signal.)
    deck.wait_for_string("No active sessions");

    // The silent restart SIGTERM'd the original daemon; its process must be
    // gone (a fresh one was lazy-spawned at the new build).
    assert!(
        daemon.wait_for_exit(Duration::from_secs(10)),
        "the old daemon should have been restarted (terminated) on silent \
         no-agents recovery, but it is still alive"
    );
}

/// Scenario: Start an older daemon with one live agent named `zeta-live-77`,
/// then attach a newer TUI in a TTY. The mismatch prompt must NAME the live
/// agent (its display name) and state that continuing stops/restarts it.
#[spec("lifecycle/handshake/003")]
#[test]
fn handshake_003_agents_running_prompt_names_agents() {
    let daemon = spawn_daemon_at_build(OLD_BUILD);
    start_live_agent(&daemon);
    let deck = launch_tui_against(&daemon, NEW_BUILD);

    // The prompt must surface the live agent's DISPLAY NAME (PRD #161 M1.1
    // `running_agents.names`) together with the stop/restart intent. (Today
    // #103 lists the agent's generated id, not its display name, so this times
    // out — the RED signal.)
    deck.wait_until_grid("prompt names the live agent + stop/restart intent", |g| {
        let low = g.to_lowercase();
        g.contains(LIVE_AGENT_NAME) && (low.contains("stop") || low.contains("restart"))
    });
}

/// Scenario: Start an older daemon with one live agent, then run the newer TUI
/// with stdout redirected to a pipe (non-TTY). The mandatory-restart path must
/// print no prompt, exit non-zero, and write a recovery hint to stderr.
#[spec("lifecycle/handshake/004")]
#[test]
fn handshake_004_mismatch_agents_non_tty_exits_nonzero_with_hint() {
    let daemon = spawn_daemon_at_build(OLD_BUILD);
    start_live_agent(&daemon);

    // Drive the binary directly (not via the PTY harness) so stdout is a pipe
    // and `is_terminal()` is false — the non-TTY branch.
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let mut cmd = std::process::Command::new(bin);
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("HOME", &daemon.home);
    cmd.env("TERM", "xterm-256color");
    cmd.env("DOT_AGENT_DECK_ATTACH_SOCKET", &daemon.attach_socket);
    cmd.env("DOT_AGENT_DECK_SOCKET", &daemon.hook_socket);
    cmd.env("DOT_AGENT_DECK_STATE_DIR", daemon.home.join("tui-state"));
    cmd.env("DOT_AGENT_DECK_BUILD_ID_OVERRIDE", NEW_BUILD);
    // Safety net: if the build ever reaches the success path, exit cleanly
    // after the handshake instead of trying to render a TUI on a non-TTY.
    cmd.env("DOT_AGENT_DECK_EXIT_AFTER_HANDSHAKE", "1");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let out = cmd
        .output()
        .expect("run dot-agent-deck against the old daemon");

    assert!(
        !out.status.success(),
        "the agents-running non-TTY mandatory-restart path must exit non-zero, \
         got status {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        stderr.contains("daemon") && (stderr.contains("stop") || stderr.contains("restart")),
        "stderr must carry a clear daemon recovery hint, got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Scenario: Start an older daemon with one live agent, attach a newer TUI in a
/// TTY, wait for the restart prompt, then press a single `s` to consent. Part A
/// restarts the daemon on that one keypress (the agents are stopped) and the
/// fresh empty dashboard appears — replacing #103's two-`S` double-confirm.
#[spec("lifecycle/handshake/005")]
#[test]
fn handshake_005_agents_running_single_consent_restarts() {
    let mut daemon = spawn_daemon_at_build(OLD_BUILD);
    start_live_agent(&daemon);
    let deck = launch_tui_against(&daemon, NEW_BUILD);

    wait_for_restart_prompt(&deck);
    // One consent keypress. (Today #103 requires two consecutive `S` with live
    // agents, so a single press only re-renders the prompt and the dashboard
    // never appears — the RED signal.)
    deck.send_keys(b"s");

    deck.wait_for_string("No active sessions");
    assert!(
        daemon.wait_for_exit(Duration::from_secs(10)),
        "consenting to the restart should have terminated the old daemon, but \
         it is still alive"
    );
}

/// Scenario: Start an older daemon with one live agent, attach a newer TUI in a
/// TTY, wait for the restart prompt, then DECLINE with `Esc`. Per D4
/// (never-strand), declining must NOT exit: the TUI stays attached to the
/// EXISTING old daemon, so a working dashboard appears and the live agent
/// remains reachable on that daemon.
#[spec("lifecycle/handshake/006")]
#[test]
fn handshake_006_decline_keeps_existing_daemon() {
    let mut daemon = spawn_daemon_at_build(OLD_BUILD);
    start_live_agent(&daemon);
    let deck = launch_tui_against(&daemon, NEW_BUILD);

    wait_for_restart_prompt(&deck);
    // Decline. (Today #103 treats Esc as abort → process exits non-zero, so no
    // dashboard ever appears — the RED signal. Part A keeps the existing
    // daemon and lands in a working session.)
    deck.send_keys(b"\x1b");

    // We land in a working dashboard showing the live session (not the empty
    // state) against the still-running old daemon.
    deck.wait_for_string("session(s)");

    // Never-strand: the old daemon is still alive and still serving the agent.
    assert!(
        daemon.is_alive_public(),
        "declining must keep the existing daemon alive (never-strand, D4)"
    );
    assert!(
        wait_for_agent_display_name(
            &daemon.attach_socket,
            LIVE_AGENT_NAME,
            true,
            Duration::from_secs(5),
        ),
        "the live agent must remain reachable on the existing daemon after \
         declining the restart (never-strand, D4)"
    );
}
