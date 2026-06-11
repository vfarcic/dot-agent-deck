#![cfg(feature = "e2e")]

//! L2 live-surfacing tests for the daemon-hosted scheduler (PRD #127 finding
//! #2): a scheduled fire must surface its card to an ALREADY-ATTACHED TUI —
//! without a disconnect/reconnect — and that card must survive being focused.
//!
//! These drive the real `dot-agent-deck` binary inside an isolated PTY (the
//! `TuiDeck` harness) so the assertions land on the RENDERED vt100 grid, which
//! is the only surface where the bug is observable: the daemon's agent registry
//! (`ListAgents`) already holds the spawned agent in both the broken and fixed
//! states — it's the attached TUI that fails to render/keep a card. The
//! lazy-spawned daemon inherits the deck's env, so a fixture global
//! `schedules.toml` supplied via `DOT_AGENT_DECK_SCHEDULES` is loaded by it;
//! fires are triggered with the existing `RunNow` control message over the
//! deck's attach socket (no real LLM, no real-time cron wait).
//!
//! RED today (the symptoms this pins):
//!   - `live/001`: a fire with a NON-hook command (`cat`) registers an agent in
//!     the daemon but NEVER paints a card on the attached dashboard, because the
//!     BroadcastMsg stream carries no "agent-spawned" message and
//!     `hydrate_from_daemon` only wires daemon agents at TUI startup.
//!   - `live/002`: a fire whose agent emits a `SessionStart` hook DOES paint a
//!     card (the hook event reaches the attached TUI over the existing event
//!     stream), but the card is not backed by a local pane, so FOCUSING it makes
//!     `focus_deck` treat it as stale and DELETE it — the card vanishes instead
//!     of becoming usable.

mod common;

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

/// One `[[scheduled_tasks]]` block whose cron never fires on its own during the
/// test window (Jan 1 00:00); the tests trigger fires explicitly via `RunNow`.
fn single_task_toml(name: &str, working_dir: &str, command: &str, prompt: &str) -> String {
    format!(
        "[[scheduled_tasks]]\n\
         name = \"{name}\"\n\
         cron = \"0 0 1 1 *\"\n\
         working_dir = \"{working_dir}\"\n\
         command = \"{command}\"\n\
         prompt = \"{prompt}\"\n\
         enabled = true\n"
    )
}

/// Fire a registered task on the deck's own daemon via the `RunNow` control
/// message (the same socket path the in-TUI manager dialog uses).
fn run_now(deck: &TuiDeck, name: &str) {
    common::attach_request_on(
        deck.attach_socket_path(),
        &dot_agent_deck::daemon_protocol::AttachRequest::RunNow {
            name: name.to_string(),
        },
    )
    .unwrap_or_else(|e| panic!("RunNow {name} over the attach socket failed: {e}"));
}

/// Scenario: Launch the deck attached to a daemon that has one enabled schedule
/// (`livecard`, a plain `cat` command — no hooks), wait for the empty dashboard,
/// then fire the schedule via the `RunNow` control message WITHOUT detaching.
/// First confirm the daemon actually spawned the agent (it appears in the
/// registry under the task's display name), then assert a card for it surfaces
/// LIVE on the rendered dashboard. RED today: the daemon has the agent but the
/// attached TUI never paints a card (it stays on "No active sessions"), because
/// no "agent-spawned" broadcast triggers live hydration.
#[spec("scheduler/live/001")]
#[test]
fn live_001_scheduled_card_surfaces_to_attached_tui() {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    // working_dir basename == task name, so the card renders "livecard" whether
    // its title comes from the display name or the cwd basename.
    let work = scratch.path().join("livecard");
    std::fs::create_dir_all(&work).expect("create work dir");

    let sched_path = scratch.path().join("schedules.toml");
    std::fs::write(
        &sched_path,
        single_task_toml("livecard", &work.to_string_lossy(), "cat", "LIVECARDPROMPT"),
    )
    .expect("write fixture schedules.toml");

    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // Fire the schedule into the SAME daemon this TUI is attached to.
    run_now(&deck, "livecard");

    // The daemon side works: the spawned agent is registered. This isolates the
    // bug to the attached TUI's surfacing — the registry has the agent in both
    // the broken and fixed states.
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "livecard",
            true,
            Duration::from_secs(10),
        ),
        "the daemon must spawn the scheduled agent (precondition for live surfacing)"
    );

    // The bug: a card must appear on the ALREADY-ATTACHED dashboard, live, with
    // no disconnect/reconnect. RED today — the dashboard stays on its empty
    // state and `wait_for_string` times out with the empty grid shown.
    deck.wait_for_string("livecard");
}

/// Scenario: Launch the deck attached to a daemon with one enabled schedule
/// (`schedfocus`, a long-lived `cat`), fire it via `RunNow`, then — mirroring
/// exactly what a real agent's hook does — inject a `SessionStart` hook carrying
/// the daemon-spawned agent's own `DOT_AGENT_DECK_PANE_ID` (read back from the
/// registry). That paints a card on the attached dashboard. Press `1` to focus
/// the card. The card must SURVIVE and become usable (the TUI enters PaneInput
/// mode on the re-hydrated pane). RED today: the card is not backed by a local
/// pane, so `focus_deck` treats it as stale and DELETES it — focus never enters
/// PaneInput mode because the card vanishes.
#[spec("scheduler/live/002")]
#[test]
fn live_002_focusing_scheduled_card_does_not_delete_it() {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let work = scratch.path().join("schedfocus");
    std::fs::create_dir_all(&work).expect("create work dir");

    let sched_path = scratch.path().join("schedules.toml");
    std::fs::write(
        &sched_path,
        single_task_toml(
            "schedfocus",
            &work.to_string_lossy(),
            "cat",
            "SCHEDFOCUSPROMPT",
        ),
    )
    .expect("write fixture schedules.toml");

    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // Fire the schedule; the daemon spawns the agent.
    run_now(&deck, "schedfocus");
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "schedfocus",
            true,
            Duration::from_secs(10),
        ),
        "the daemon must spawn the scheduled agent (precondition)"
    );

    // The daemon assigns the spawned agent a pane id (used for hook routing and
    // startup hydration). A real `claude` agent's SessionStart hook carries this
    // exact id; reproduce that faithfully by reading it back and injecting the
    // same hook. The resulting card is backed by a LIVE daemon agent but NOT by
    // a local TUI pane — the precise orphan-card condition of the bug.
    let records = common::agent_records_on(deck.attach_socket_path());
    let pane_id = records
        .iter()
        .find(|r| r.display_name.as_deref() == Some("schedfocus"))
        .and_then(|r| r.pane_id_env.clone())
        .expect("scheduler-spawned agent must carry a DOT_AGENT_DECK_PANE_ID for hook routing");

    let event = serde_json::json!({
        "session_id": "schedfocus",
        "agent_type": "claude_code",
        "event_type": "session_start",
        "timestamp": "2026-06-08T12:00:00Z",
        "pane_id": pane_id,
        "cwd": work.to_string_lossy(),
    });
    common::write_hook_line(deck.hook_socket_path(), &event.to_string())
        .expect("write SessionStart hook to the deck's hook socket");

    // The hook event reaches the attached TUI over the existing event stream, so
    // the card appears even in the broken state (this is the bug's precondition,
    // not the bug).
    deck.wait_for_string("schedfocus");

    // Focus the (only) card with the `1` jump key → `focus_deck`.
    deck.send_keys(b"1");

    // GREEN-only signal: focusing a card backed by a live daemon agent must
    // re-hydrate its pane and enter PaneInput mode (the card stays usable). RED
    // today: `focus_deck` deletes the orphan card instead, so PaneInput mode is
    // never reached and this times out with the card gone.
    deck.wait_for_string("PaneInput mode");

    drop(scratch);
}

/// Scenario: Launch the deck attached to a daemon with one enabled schedule
/// named `morning-digest` whose `working_dir` basename (`runbox`) is
/// deliberately UNRELATED to the schedule name, then fire it via `RunNow`
/// WITHOUT detaching. After confirming the daemon registered the spawned agent
/// under its friendly name (precondition) and the card surfaced live (its Dir
/// line shows `runbox`), assert the card's TITLE shows the friendly schedule
/// name `morning-digest` — matching what a disconnect/reconnect already renders
/// — and is NOT the truncated pane-id form (`… · sched-morni…`). RED today: the
/// live-surfacing path titles the card from the spawned pane id, so the header
/// reads `No agent · sched-morni` instead of the schedule's name.
#[spec("scheduler/live/003")]
#[test]
fn live_003_scheduled_card_title_shows_friendly_name() {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    // The working-dir basename (`runbox`) is deliberately UNRELATED to the
    // schedule name. That is the whole point: the friendly name `morning-digest`
    // can then reach the rendered grid ONLY through the card TITLE — never via
    // the Dir line's cwd basename (the trap `scheduler/live/001`'s
    // name==basename fixture sidesteps, letting a stray substring match pass).
    let work = scratch.path().join("runbox");
    std::fs::create_dir_all(&work).expect("create work dir");

    let sched_path = scratch.path().join("schedules.toml");
    std::fs::write(
        &sched_path,
        single_task_toml(
            "morning-digest",
            &work.to_string_lossy(),
            "cat",
            "RUNBOXPROMPT",
        ),
    )
    .expect("write fixture schedules.toml");

    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // Fire the schedule into the SAME daemon this TUI is attached to.
    run_now(&deck, "morning-digest");

    // Precondition: the daemon registers the spawned agent under the schedule's
    // FRIENDLY name. So the friendly name IS available daemon-side — the bug is
    // isolated to how the already-attached TUI titles the live-surfaced card
    // (a reconnect reads this same name back via startup hydration and titles
    // the card correctly).
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "morning-digest",
            true,
            Duration::from_secs(10),
        ),
        "the daemon must spawn the scheduled agent under its friendly name (precondition)"
    );

    // Title-independent surfacing signal: the card's Dir line renders the cwd
    // basename (`runbox`) in BOTH the broken and fixed states, so this waits for
    // the card to paint live without depending on the (buggy) title. The whole
    // card — title block + body — is drawn in one render pass, so once `runbox`
    // is on the grid the title is too.
    deck.wait_for_string("runbox");

    let grid = deck.snapshot_grid();

    // DESIRED (matches a reconnect): the live-surfaced card's TITLE shows the
    // friendly schedule name. Because the cwd basename is `runbox` and the
    // placeholder card renders no prompt, `morning-digest` can ONLY appear on
    // the grid via the card header — so this is a title assertion, not a stray
    // substring match.
    assert!(
        grid.contains("morning-digest"),
        "live-surfaced scheduled card TITLE must show the friendly name \
         'morning-digest' (a disconnect/reconnect already titles it so).\nGrid:\n{grid}"
    );

    // ...and must NOT fall back to the truncated pane id (`sched-morning-digest-0`
    // → its 11-char `id_display` prefix `sched-morni`). This is the load-bearing
    // pin: `sched-morni` reaches the grid ONLY through the broken title's
    // pane-id `id_display`, so its presence means the header is showing the
    // pane id instead of the schedule name.
    assert!(
        !grid.contains("sched-morni"),
        "live-surfaced scheduled card TITLE must NOT show the truncated pane-id \
         form ('… · sched-morni…') — it should show the schedule name.\nGrid:\n{grid}"
    );

    drop(scratch);
}
