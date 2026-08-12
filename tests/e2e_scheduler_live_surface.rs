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
//! All four tests here are GREEN today, and have been since the fixes below
//! landed — with ONE exception: `78f92b6` widened the retire predicate and made
//! `/004` RED again for the window it was on the tree, until `8f579dd` reverted
//! it (see `/004`'s own note below). Outside that window they have been green,
//! so do NOT read any of them as known-failing. A failure in this file is a real
//! regression at the live-surfacing / supersession seam (issue #284: an earlier
//! stale `RED today:` note here caused exactly that misclassification, so an
//! investigation stopped looking at a genuine break).
//!
//! What each one pins, and the fix it guards:
//!   - `live/001`: a fire with a NON-hook command (`cat`) paints a card on the
//!     ALREADY-ATTACHED dashboard. Was RED when written — the daemon registered
//!     the agent but no broadcast carried it, and `hydrate_from_daemon` wires
//!     daemon agents only at TUI startup. Closed by `ed0c3bf`, which publishes a
//!     synthetic `SessionStart` for single-agent fires (`surface_spawned_pane`)
//!     through the daemon's existing hook-event broadcast.
//!   - `live/002`: focusing such a card KEEPS it and makes it usable. Was RED —
//!     the card is backed by a live daemon agent but not by a local pane, so
//!     `focus_deck` treated it as stale and deleted it. Closed by `ed0c3bf`,
//!     which attaches the daemon's pane on demand and retries `focus_pane`
//!     before writing the card off — at that point via a
//!     `downcast_ref::<EmbeddedPaneController>()` calling the new
//!     `EmbeddedPaneController::hydrate_pane`, and only from `focus_deck`. The
//!     `PaneController::try_hydrate_pane` trait method that carries this today
//!     arrived LATER, in `f1e9f82`, which replaced the downcast (untestable —
//!     it silently no-ops for every controller but the concrete production one)
//!     and extended the same retry to `Action::Focus`.
//!   - `live/003`: the live-surfaced card's TITLE shows the schedule's friendly
//!     name, not the truncated spawn pane id. Was RED; closed by `b0bdc4b`,
//!     which threads the task name onto the synthetic `SessionStart` as
//!     `metadata["display_name"]` and onto `SessionState.display_name`.
//!   - `live/004`: that friendly title SURVIVES the agent's real `SessionStart`
//!     hook superseding the placeholder. Was RED; closed by `ccadbbc`, which
//!     captures the retired session's `display_name` (keyed by the stable pane
//!     id) and seeds the replacement session with it. Shipped green in v0.35.0.

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
/// LIVE on the rendered dashboard. Pins live surfacing for a bare (hookless)
/// command: green since `ed0c3bf` publishes a synthetic `SessionStart` for
/// single-agent fires. When written this was RED — the daemon had the agent but
/// the attached TUI stayed on "No active sessions", because no broadcast
/// triggered live hydration.
#[spec("scheduler/live/001")]
#[test]
fn live_001_scheduled_card_surfaces_to_attached_tui() {
    let scratch = common::harness_tempdir().expect("scratch tempdir");
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

    // The pin: a card must appear on the ALREADY-ATTACHED dashboard, live, with
    // no disconnect/reconnect. If this times out with the empty grid shown, the
    // dashboard is back to its pre-`ed0c3bf` behaviour and live surfacing has
    // regressed.
    deck.wait_for_string("livecard");
}

/// Scenario: Launch the deck attached to a daemon with one enabled schedule
/// (`schedfocus`, a long-lived `cat`), fire it via `RunNow`, then — mirroring
/// exactly what a real agent's hook does — inject a `SessionStart` hook carrying
/// the daemon-spawned agent's own `DOT_AGENT_DECK_PANE_ID` (read back from the
/// registry). That paints a card on the attached dashboard. Press `1` to focus
/// the card. The card must SURVIVE and become usable (the TUI enters PaneInput
/// mode on the re-hydrated pane). Pins on-demand pane hydration on focus: green
/// since `ed0c3bf` retries `focus_pane` after `try_hydrate_pane`. When written
/// this was RED — the card is backed by a live daemon agent but not by a local
/// pane, so `focus_deck` treated it as stale and DELETED it, and focus never
/// reached PaneInput mode because the card vanished.
#[spec("scheduler/live/002")]
#[test]
fn live_002_focusing_scheduled_card_does_not_delete_it() {
    let scratch = common::harness_tempdir().expect("scratch tempdir");
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

    // The pin: focusing a card backed by a live daemon agent must re-hydrate its
    // pane and enter PaneInput mode (the card stays usable). If this times out
    // with the card gone, `focus_deck` is deleting the orphan card again instead
    // of hydrating it.
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
/// — and is NOT the truncated pane-id form (`… · sched-morni…`). Pins title
/// parity between the live path and a reconnect: green since `b0bdc4b` threads
/// the task name onto the synthetic `SessionStart` as `metadata["display_name"]`.
/// When written this was RED — the live-surfacing path titled the card from the
/// spawned pane id, so the header read `No agent · sched-morni`.
#[spec("scheduler/live/003")]
#[test]
fn live_003_scheduled_card_title_shows_friendly_name() {
    let scratch = common::harness_tempdir().expect("scratch tempdir");
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

    // The pin (title parity with a reconnect, which reads the friendly name from
    // the daemon registry's `display_name`): the live-surfaced card's TITLE shows
    // the friendly schedule name. Because the cwd basename is `runbox` and the
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

/// Scenario: Launch the deck attached to a daemon with one enabled schedule
/// `morning-digest` (working_dir basename `runbox`, deliberately unrelated),
/// fire it via `RunNow`, and wait for the synthetic placeholder card to surface
/// LIVE with the friendly title `morning-digest` (the "name shows briefly"
/// state). THEN — mirroring what a real hook-emitting agent (claude/opencode,
/// the primary scheduler case) does — inject the agent's REAL `SessionStart`
/// hook for that pane carrying BOTH the daemon-spawned pane id AND its
/// spawn-injected registry agent id (read back from the registry; a `Some`,
/// distinct from the synthetic placeholder's `None`) and NO display_name
/// metadata. That mismatched agent id supersedes the placeholder (it is retired
/// and a fresh live card replaces it). After the supersession lands (the card
/// turns from a "No agent" placeholder into a live ClaudeCode agent), assert the
/// card TITLE STILL shows `morning-digest` and has NOT reverted to the
/// session-id hash form. Pins the friendly title across supersession: green
/// since `ccadbbc` carries the retired placeholder's `display_name` onto the
/// replacement session, and SHIPPED GREEN in v0.35.0. When written this was RED
/// — the superseding session was created with display_name=None, so its title
/// fell back to the session id. This test is also one of the two guards the
/// reverted `78f92b6` broke, so treat a failure here as a real regression at the
/// retire predicate (issue #284), never as a known-RED expectation.
#[spec("scheduler/live/004")]
#[test]
fn live_004_real_hook_supersession_keeps_friendly_title() {
    let scratch = common::harness_tempdir().expect("scratch tempdir");
    // Same trap as `scheduler/live/003`: the working-dir basename (`runbox`) is
    // deliberately UNRELATED to the schedule name, so `morning-digest` can reach
    // the rendered grid ONLY through the card TITLE — never via the Dir line's
    // cwd basename.
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

    // Precondition: the daemon registers the spawned agent under its FRIENDLY
    // name, so the name is available daemon-side.
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "morning-digest",
            true,
            Duration::from_secs(10),
        ),
        "the daemon must spawn the scheduled agent under its friendly name (precondition)"
    );

    // The synthetic placeholder card surfaces LIVE with the friendly title —
    // the same state `scheduler/live/003` pins. This is the starting point this
    // test then supersedes: the friendly name is present *before* the real hook.
    deck.wait_for_string("morning-digest");
    // The placeholder is a `cat` spawn (no recognized agent type) → its status
    // badge reads "No agent". Confirming it is on-grid now makes the later
    // `wait_for_absence` a genuine present→absent transition (the supersession),
    // not a vacuously-true wait.
    deck.wait_for_string("No agent");

    // Read BOTH the spawned pane's pane id AND its registry id from the daemon.
    // The registry id is the SAME value the daemon injected as
    // `DOT_AGENT_DECK_AGENT_ID` into the spawned child's env (see
    // `AgentPtyRegistry::spawn_agent` — the pre-allocated id is both inserted
    // into the registry and pushed into the child env; `hook.rs` reads it back).
    // A real claude/opencode agent's `SessionStart` hook carries this exact
    // agent id; reproduce that faithfully — `live_002` read the pane id the same
    // way, this also reads the agent id.
    let record = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|r| r.display_name.as_deref() == Some("morning-digest"))
        .expect("the scheduler-spawned agent must be registered under its friendly name");
    let pane_id = record
        .pane_id_env
        .clone()
        .expect("scheduler-spawned agent must carry a DOT_AGENT_DECK_PANE_ID for hook routing");
    let agent_id = record.id.clone();

    // A real hook-emitting agent's `SessionStart` carries a Some(agent_id)
    // distinct from the synthetic placeholder's None, and NO display_name
    // metadata. The agent-id mismatch supersedes the placeholder: it is retired
    // and a fresh session is created from this hook. The session id is a
    // hash-like value DELIBERATELY unrelated to the schedule name, so its 11-char
    // `id_display` title-fallback prefix (`9f8e7d6c-5b`) can be asserted-absent
    // without colliding with `morning-digest`, `runbox`, or `sched-`.
    let real_session_id = "9f8e7d6c-5b4a-4321-aaaa-bbbbccccdddd";
    let event = serde_json::json!({
        "session_id": real_session_id,
        "agent_type": "claude_code",
        "event_type": "session_start",
        "timestamp": "2026-06-08T12:00:00Z",
        "pane_id": pane_id,
        "agent_id": agent_id,
        "cwd": work.to_string_lossy(),
        // NOTE: no `metadata` key → the real hook carries NO display_name (only
        // the daemon's synthetic live-surface SessionStart sets that).
    });
    common::write_hook_line(deck.hook_socket_path(), &event.to_string())
        .expect("write the agent's real SessionStart hook to the deck's hook socket");

    // Sync on the supersession landing: the placeholder ("No agent") is replaced
    // by a live ClaudeCode card (its status becomes "Idle"), so "No agent"
    // disappears from the grid. This present→absent transition happens in BOTH
    // the broken and fixed states — it confirms the real hook was applied and the
    // card re-rendered before we snapshot the title.
    deck.wait_for_absence("No agent");

    let grid = deck.snapshot_grid();

    // The pin (title parity with a reconnect, which reads the friendly name from
    // the daemon registry's `display_name`): the superseding live card's TITLE
    // STILL shows `morning-digest`. Because the cwd basename is `runbox`, the only
    // way `morning-digest` reaches the grid is via the card header. If this fails,
    // the retire path dropped the display_name again and the title reverted to
    // the session-id hash — the pre-`ccadbbc` behaviour.
    assert!(
        grid.contains("morning-digest"),
        "after a real SessionStart hook supersedes the synthetic placeholder, the \
         card TITLE must STILL show the friendly name 'morning-digest' (a reconnect \
         keeps it), but it reverted to the session-id hash.\nGrid:\n{grid}"
    );

    // ...and must NOT fall back to the session-id hash form. `9f8e7d6c-5b` is the
    // session id's 11-char `id_display` prefix — it reaches the grid ONLY through
    // the reverted `ClaudeCode · 9f8e7d6c-5b` title, so its presence is the
    // load-bearing pin that the friendly name was lost on supersession.
    assert!(
        !grid.contains("9f8e7d6c-5b"),
        "after supersession the card TITLE must NOT revert to the session-id hash \
         ('… · 9f8e7d6c-5b…') — it should keep the schedule name.\nGrid:\n{grid}"
    );

    drop(scratch);
}
