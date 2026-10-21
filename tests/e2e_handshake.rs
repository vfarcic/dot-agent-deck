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
//!   - 007 — MISMATCH + agents + TTY, but the daemon OMITS `running_agents`
//!     (a pre-#161 daemon predating M1.1): the handshake must NOT read the
//!     absent field as "no agents" and silently restart (which would SIGTERM
//!     the live agent unseen); it falls back to `list_agents()`, sees the
//!     agent, and shows the consent prompt. Declining then keeps the existing
//!     daemon with the agent reachable (FIX 1; D2/D4 never-strand).
//!   - 009 — PROTOCOL SKEW + agents + TTY + DECLINE: a daemon advertising
//!     another attach protocol is never attached to (issue #405). Declining
//!     exits non-zero with a refusal naming both protocol numbers, and the
//!     daemon and its agent keep running.
//!   - 010 — PROTOCOL SKEW + MATCHING build id + no agents: the build-id match
//!     does not wave the skew through; the daemon is restarted silently.
//!
//! Skew is simulated without rebuilding the binary via
//! `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` (honoured by both the daemon's `hello`
//! reply and the laptop's comparison under `cfg(debug_assertions)`): the
//! external daemon is started at `OLD_BUILD` and the TUI is launched at
//! `NEW_BUILD`, pointed at the daemon's sockets so it reuses that older daemon
//! and the handshake observes a mismatch. Protocol skew (009/010) is simulated
//! the same way through the test-only (`e2e` feature + debug build)
//! `DOT_AGENT_DECK_TEST_PROTOCOL_VERSION_OVERRIDE`, which changes only the
//! number the daemon's `hello` reply advertises.
//!
//! Gated behind the `e2e` feature so CI (`cargo test-fast`) never compiles it
//! (PRD #77 Decision 6). All polling lives in `common` helpers so these bodies
//! carry no raw `sleep` (linkage-check Decision 21 / rule 5).

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

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

/// Like [`spawn_daemon_at_build`] but ALSO sets the test-only
/// `DOT_AGENT_DECK_TEST_OMIT_RUNNING_AGENTS` on the daemon, so its `Hello`
/// reply leaves `running_agents = None` — exactly how a pre-#161 daemon that
/// predates the M1.1 summary field replies. The newer TUI must then fall back
/// to `list_agents()` (PRD #161 FIX 1) rather than mistaking the absent field
/// for "no agents" and silently restarting over live agents it can't see.
fn spawn_daemon_at_build_omitting_running_agents(build_id: &str) -> DaemonProc {
    spawn_daemon_serve_with_env(
        None,
        "0",
        &[
            ("DOT_AGENT_DECK_BUILD_ID_OVERRIDE", build_id),
            ("DOT_AGENT_DECK_TEST_OMIT_RUNNING_AGENTS", "1"),
        ],
    )
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
            seed: None,
            authoring_kind: None,
            client_seeded_kind: None,
            remember_command: false,
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
    deck.wait_for_string("No active agents");
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
    deck.wait_for_string("No active agents");

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

    deck.wait_for_string("No active agents");
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
    deck.wait_for_string("agent(s)");

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

/// Scenario: Start an older daemon with one live agent, but force its `Hello`
/// reply to OMIT `running_agents` (`DOT_AGENT_DECK_TEST_OMIT_RUNNING_AGENTS`) —
/// exactly how a pre-#161 daemon that predates the M1.1 summary replies. Attach
/// a newer TUI in a TTY: the handshake must NOT read the absent field as "no
/// agents" and silently restart (which would SIGTERM the live agent unseen), but
/// fall back to `list_agents()`, see the agent, and surface the consent prompt.
/// Pressing `Esc` then declines and lands in a working dashboard against the
/// still-running old daemon with the agent reachable (PRD #161 FIX 1; D2/D4
/// never-strand).
#[spec("lifecycle/handshake/007")]
#[test]
fn handshake_007_omitted_running_agents_falls_back_no_silent_kill() {
    let mut daemon = spawn_daemon_at_build_omitting_running_agents(OLD_BUILD);
    start_live_agent(&daemon);
    let deck = launch_tui_against(&daemon, NEW_BUILD);

    // CORE GUARD: the agents-PRESENT consent prompt must appear. Pre-fix, the
    // omitted `running_agents` was read as "no agents" → SILENT restart (NO
    // prompt) → the live agent was killed unseen and we'd land straight in the
    // empty dashboard. The fix falls back to `list_agents()`, which still sees
    // the agent, so the prompt surfaces instead. LOOSE match on the agent
    // reference: when `running_agents` is omitted the name comes from
    // `list_agents()` (display name, falling back to id), so accept either the
    // display name OR a non-zero "(N agent(s) running)" header — anything but
    // the silent no-prompt restart proves the fallback ran.
    deck.wait_until_grid(
        "agents-present restart prompt via list_agents fallback",
        |g| {
            let low = g.to_lowercase();
            let prompt_present = low.contains("stop") || low.contains("restart");
            let names_agent = g.contains(LIVE_AGENT_NAME);
            let nonzero_header =
                low.contains("agent(s) running") && !low.contains("(0 agent(s) running)");
            prompt_present && (names_agent || nonzero_header)
        },
    );

    // Decline with Esc → keep the EXISTING daemon (D4 never-strand), mirroring
    // handshake_006: a working dashboard appears against the still-running old
    // daemon and the live agent stays reachable on it.
    deck.send_keys(b"\x1b");
    deck.wait_for_string("agent(s)");
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
         declining the restart, even though its summary was omitted from the \
         handshake (never-strand, D4)"
    );
}

/// Start an external `daemon serve` pinned to `build_id` whose `Hello` reply
/// advertises attach protocol `protocol` instead of this build's own, via the test-only
/// (`e2e` feature + debug build) `DOT_AGENT_DECK_TEST_PROTOCOL_VERSION_OVERRIDE` seam (issue
/// #405). The daemon and the TUI here are one compiled binary, so this is the
/// only way a PTY test can put a protocol skew between them.
fn spawn_daemon_at_build_and_protocol(build_id: &str, protocol: u32) -> DaemonProc {
    spawn_daemon_serve_with_env(
        None,
        "0",
        &[
            ("DOT_AGENT_DECK_BUILD_ID_OVERRIDE", build_id),
            (
                "DOT_AGENT_DECK_TEST_PROTOCOL_VERSION_OVERRIDE",
                &protocol.to_string(),
            ),
        ],
    )
}

/// Scenario: Start a daemon that speaks a NEWER attach protocol than the TUI,
/// with one live agent, and attach the TUI in a TTY. The restart prompt appears
/// and says this binary cannot attach to that daemon; pressing `Esc` must NOT
/// attach (which would leave a normal-looking dashboard silently dropping every
/// event it cannot decode) but exit non-zero with a refusal naming both protocol
/// numbers and how to keep the agents, and the daemon and its agent stay alive.
#[spec("lifecycle/handshake/009")]
#[test]
fn handshake_009_protocol_skew_decline_refuses_and_keeps_daemon() {
    let newer = dot_agent_deck::daemon_protocol::PROTOCOL_VERSION + 1;
    let mut daemon = spawn_daemon_at_build_and_protocol(OLD_BUILD, newer);
    start_live_agent(&daemon);
    let mut deck = launch_tui_against(&daemon, NEW_BUILD);

    wait_for_restart_prompt(&deck);
    deck.send_keys(b"\x1b");

    let exited = deck.wait_for_exit_within(Duration::from_secs(15));
    assert_eq!(
        exited,
        Some(false),
        "declining the restart of a daemon on another attach protocol must exit non-zero \
         instead of attaching to it (None = still running, i.e. it attached)"
    );
    // The prompt already names the skew, so these two lines are the refusal's
    // own: its header, and the way out that keeps the agents.
    deck.wait_for_string(&format!(
        "error: daemon speaks attach protocol v{newer}, but this binary speaks v{}",
        dot_agent_deck::daemon_protocol::PROTOCOL_VERSION
    ));
    deck.wait_for_string(&format!(
        "To keep them: attach with the daemon's own build ({OLD_BUILD})"
    ));

    assert!(
        daemon.is_alive_public(),
        "the refusal must leave the daemon running, agents and all"
    );
    assert!(
        wait_for_agent_display_name(
            &daemon.attach_socket,
            LIVE_AGENT_NAME,
            true,
            Duration::from_secs(5),
        ),
        "the live agent must remain reachable on the refused daemon"
    );
}

/// Scenario: Start a daemon with the SAME build id as the TUI but a different
/// attach protocol and no agents, then launch the TUI in a TTY. A matching build
/// id must not wave the skew through: with nothing to lose the old daemon is
/// restarted silently and the TUI lands in the empty dashboard on a fresh one.
#[spec("lifecycle/handshake/010")]
#[test]
fn handshake_010_protocol_skew_with_matching_build_restarts_silently() {
    let older = dot_agent_deck::daemon_protocol::PROTOCOL_VERSION - 1;
    let mut daemon = spawn_daemon_at_build_and_protocol(NEW_BUILD, older);
    let deck = launch_tui_against(&daemon, NEW_BUILD);

    deck.wait_for_string("No active agents");
    assert!(
        daemon.wait_for_exit(Duration::from_secs(10)),
        "a daemon on another attach protocol must be restarted, not attached to, even when \
         its build id matches"
    );
}

/// Scenario: Replace the daemon under an open TUI at the same socket. A compatible replacement must display a terminal-only sentinel; an incompatible replacement must admit zero terminal/event streams throughout the attempt and show a persistent refusal associating the daemon and client versions.
#[spec("lifecycle/handshake/011")]
#[test]
fn handshake_011_reconnect_rechecks_protocol_before_reattaching() {
    use inbox::*;
    let own = dot_agent_deck::daemon_protocol::PROTOCOL_VERSION;
    // Exercise the positive reconnect control before reaching the RED case.
    for protocol in [own, own + 1] {
        let mut original = spawn_daemon_at_build_and_protocol(NEW_BUILD, own);
        let old = pane(&original, "before-replacement");
        status(&original, &old, "idle", "claude_code");
        let deck = TuiDeck::builder()
            .with_env(
                "DOT_AGENT_DECK_ATTACH_SOCKET",
                original.attach_socket.to_string_lossy().to_string(),
            )
            .with_env(
                "DOT_AGENT_DECK_SOCKET",
                original.hook_socket.to_string_lossy().to_string(),
            )
            .with_env("DOT_AGENT_DECK_BUILD_ID_OVERRIDE", NEW_BUILD)
            .with_env("RUST_LOG", "dot_agent_deck=debug")
            .launch_with_fixture("minimal");
        deck.wait_for_string("before-replacement");

        let reply = original
            .send_attach_request(&AttachRequest::StopDaemon { force: true })
            .expect("stop only the fixture-owned daemon");
        assert!(reply.ok, "fixture stop: {reply:?}");
        assert!(
            original.wait_for_exit(Duration::from_secs(10)),
            "fixture daemon must stop"
        );
        let replacement = spawn_daemon_serve_with_env(
            None,
            "0",
            &[
                ("DOT_AGENT_DECK_BUILD_ID_OVERRIDE", NEW_BUILD),
                (
                    "DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_DIR",
                    "reconnect-observations",
                ),
                (
                    "DOT_AGENT_DECK_TEST_PROTOCOL_VERSION_OVERRIDE",
                    &protocol.to_string(),
                ),
                (
                    "DOT_AGENT_DECK_ATTACH_SOCKET",
                    original.attach_socket.to_str().unwrap(),
                ),
                (
                    "DOT_AGENT_DECK_SOCKET",
                    original.hook_socket.to_str().unwrap(),
                ),
            ],
        );
        let name = if protocol == own {
            "replacement-peer-a"
        } else {
            "replacement-peer-b"
        };
        let sentinel = format!("PTY-ONLY-REPLACEMENT-9f71-{protocol}");
        // Recreate the pane the user already had open. The stand-in paints
        // its label periodically, so reattachment is visible from live output
        // as well as a replayed snapshot, including when daemon ids restart at 1.
        let new = pane_with(
            &replacement,
            &old.id,
            serde_json::json!({
                "display_name":name, "env":[["DOORBELL_STANDIN_LABEL", sentinel]]
            }),
        );
        status(&replacement, &new, "idle", "claude_code");
        if protocol == own {
            deck.wait_for_string(&sentinel);
            continue;
        }
        let observations = replacement.home.join("reconnect-observations/observations");
        let admission_count = || {
            std::fs::read_to_string(&observations)
                .unwrap_or_default()
                .lines()
                .filter(|s| s.starts_with("attach-stream ") || s.starts_with("subscribe-events"))
                .count()
        };
        let refusal_visible = |grid: &str| {
            let low = grid.to_lowercase();
            let associated = (low.contains(&format!("daemon v{protocol}"))
                && low.contains(&format!("client v{own}")))
                || (low.contains(&format!("daemon speaks attach protocol v{protocol}"))
                    && low.contains(&format!("binary speaks v{own}")));
            low.contains("protocol")
                && associated
                && ["refus", "incompatib", "cannot", "not attaching"]
                    .iter()
                    .any(|word| low.contains(word))
        };
        let refused = deck.wait_for_grid_predicate_within(Duration::from_secs(10), refusal_visible);
        let grid = deck.snapshot_grid();
        assert_eq!(
            admission_count(),
            0,
            "incompatible reconnect admitted terminal/event streams before refusal: {}; refusal_visible={refused}; grid={grid}",
            std::fs::read_to_string(&observations).unwrap_or_default()
        );
        assert!(
            refused && !grid.contains(&sentinel),
            "protocol-skew reconnect must refuse daemon v{protocol}, name client v{own}, and never reattach; replacement_visible={}; grid=\n{grid}",
            grid.contains(&sentinel)
        );
        assert!(
            !common::wait_until(Duration::from_secs(2), || {
                let grid = deck.snapshot_grid();
                admission_count() != 0 || grid.contains(&sentinel) || !refusal_visible(&grid)
            }),
            "refusal must stay visible without any incompatible attach admission across retry window"
        );
    }
}

/// Scenario: Attach a pane in the real TUI, restart its isolated daemon with the same binary, and create a successor agent with the recycled id in a different pane. Ctrl+W on the old pane must display "that agent is gone (the deck restarted)" and leave the successor agent alive and named as before; closing a freshly listed successor card is the control.
#[spec("lifecycle/handshake/013")]
#[test]
fn handshake_013_ctrl_w_after_a_same_build_restart_preserves_the_successor() {
    use inbox::*;
    let mut original = spawn_daemon_at_build(NEW_BUILD);
    let old = pane_with(
        &original,
        "old-restart-pane",
        serde_json::json!({"env":[["DOORBELL_STANDIN_LABEL", "OLD-ATTACHED-PTY-1666"]]}),
    );
    let old_agent = old.agent.clone();
    let deck = launch_tui_against(&original, NEW_BUILD);
    deck.wait_for_string("OLD-ATTACHED-PTY-1666");

    let stopped = original
        .send_attach_request(&AttachRequest::StopDaemon { force: true })
        .expect("stop the fixture-owned daemon");
    assert!(stopped.ok, "{stopped:?}");
    assert!(original.wait_for_exit(Duration::from_secs(10)));
    // The old fixture's Drop carries an unbound cleanup stop; finish it before
    // placing a successor at this socket so cleanup cannot stop a recycled id.
    drop(old);
    let successor = spawn_daemon_serve_with_env(
        None,
        "0",
        &[
            ("DOT_AGENT_DECK_BUILD_ID_OVERRIDE", NEW_BUILD),
            (
                "DOT_AGENT_DECK_ATTACH_SOCKET",
                original.attach_socket.to_str().unwrap(),
            ),
            (
                "DOT_AGENT_DECK_SOCKET",
                original.hook_socket.to_str().unwrap(),
            ),
        ],
    );
    let new = pane(&successor, "successor-other-pane");
    assert_eq!(
        new.agent, old_agent,
        "precondition: daemon recycles the agent id"
    );
    deck.send_keys(b"\x17");
    deck.wait_for_string("Close selected agent?");
    deck.send_keys(b"\x1b[B");
    deck.send_keys(b"\r");
    deck.wait_for_string(dot_agent_deck::daemon_client::AGENT_GONE_AFTER_RESTART);
    assert!(
        wait_for_agent_display_name(
            &successor.attach_socket,
            "successor-other-pane",
            true,
            Duration::from_secs(5),
        ),
        "closing the old pane must leave the successor agent alive and unrenamed"
    );
    let fresh = launch_tui_against(&successor, NEW_BUILD);
    fresh.wait_for_string("successor-other-pane");
    fresh.send_keys(b"\x17");
    fresh.wait_for_string("Close selected agent?");
    fresh.send_keys(b"\x1b[B");
    fresh.send_keys(b"\r");
    fresh.wait_for_string("No active agents");
    assert!(
        wait_for_agent_display_name(
            &successor.attach_socket,
            "successor-other-pane",
            false,
            Duration::from_secs(5),
        ),
        "control: a fresh card closes the successor agent"
    );
}

/// Scenario: Replace the daemon under an open TUI with one on the next attach protocol and wait for the TUI's refusal, then open the New Agent form and submit it. The replacement must receive no start, listing, attach or subscription from the TUI at any point — the form's own hints included — and no new pane may open.
#[spec("lifecycle/handshake/012")]
#[test]
fn handshake_012_refused_daemon_gets_no_new_pane_start_or_listing() {
    use inbox::*;
    let own = dot_agent_deck::daemon_protocol::PROTOCOL_VERSION;
    let newer = own + 1;
    let mut original = spawn_daemon_at_build_and_protocol(NEW_BUILD, own);
    let old = pane(&original, "before-replacement");
    status(&original, &old, "idle", "claude_code");
    let deck = TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            original.attach_socket.to_string_lossy().to_string(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            original.hook_socket.to_string_lossy().to_string(),
        )
        .with_env("DOT_AGENT_DECK_BUILD_ID_OVERRIDE", NEW_BUILD)
        .launch_with_fixture("minimal");
    deck.wait_for_string("before-replacement");

    let reply = original
        .send_attach_request(&AttachRequest::StopDaemon { force: true })
        .expect("stop only the fixture-owned daemon");
    assert!(reply.ok, "fixture stop: {reply:?}");
    assert!(
        original.wait_for_exit(Duration::from_secs(10)),
        "fixture daemon must stop"
    );
    let replacement = spawn_daemon_serve_with_env(
        None,
        "0",
        &[
            ("DOT_AGENT_DECK_BUILD_ID_OVERRIDE", NEW_BUILD),
            (
                "DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_DIR",
                "reconnect-observations",
            ),
            (
                "DOT_AGENT_DECK_TEST_PROTOCOL_VERSION_OVERRIDE",
                &newer.to_string(),
            ),
            (
                "DOT_AGENT_DECK_ATTACH_SOCKET",
                original.attach_socket.to_str().unwrap(),
            ),
            (
                "DOT_AGENT_DECK_SOCKET",
                original.hook_socket.to_str().unwrap(),
            ),
        ],
    );
    let observations = replacement.home.join("reconnect-observations/observations");
    let admitted = || {
        std::fs::read_to_string(&observations)
            .unwrap_or_default()
            .lines()
            .filter(|s| {
                [
                    "attach-stream",
                    "subscribe-events",
                    "list-agents",
                    "start-agent",
                    "run-now",
                ]
                .iter()
                .any(|op| s.starts_with(op))
            })
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    deck.wait_for_string(&format!("daemon speaks attach protocol v{newer}"));
    deck.wait_for_string("Not attaching");
    assert_eq!(
        admitted(),
        Vec::<String>::new(),
        "the refused daemon was admitted a request before the user acted"
    );

    // New agent: the directory picker, then the form, then Submit.
    deck.send_keys(b"\x0e");
    deck.send_keys(b" ");
    deck.wait_for_string("┌ New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(&[0x7f; 96]);
    deck.send_keys(b"refused-new-pane");
    let (col, row) = deck.wait_for_in_grid("[Submit]");
    deck.click(col, row);
    deck.wait_for_absence("[Submit]");
    assert!(
        !common::wait_until(Duration::from_secs(3), || !admitted().is_empty()),
        "the refused daemon was asked to start, list or attach after New Agent: {:?}\n{}",
        admitted(),
        deck.snapshot_grid()
    );
    assert!(
        !deck.snapshot_grid().contains("refused-new-pane"),
        "no pane may open on a refused daemon:\n{}",
        deck.snapshot_grid()
    );
}
