#![cfg(feature = "e2e")]

//! PRD #113 SC1 — L2 (real-binary PTY) coverage for the dashboard selection
//! highlight clearing on a tab round-trip.
//!
//! This is the real-binary fidelity the L1 tests cannot provide: their mocks
//! never replicate the embedded controller restoring focus to a Mode tab AGENT
//! pane on tab return. The manual-test bug was exactly that — a Mode tab agent
//! pane is ALSO a dashboard card (only its side panes are excluded from the
//! card list), it stays focused when the Dashboard restores nothing on return,
//! and the per-frame reconcile re-armed the highlight from that restored
//! steady-state focus. The fix made reactivation require a genuine focus
//! TRANSITION (`reconcile_dashboard_selection`:
//! `focus_reactivates = focus_maps_to_card && focus_changed`).
//!
//! Selection cue note: PRD #13 Option A cues the highlighted card with a `▸`
//! title prefix (and a cyan-bold border), not an absolute background fill, and
//! `▸` is used ONLY for that selection marker (src/ui.rs `render_session_card`).
//! So "no card carries the highlight" is asserted as the absence of `▸` from the
//! rendered grid — the same observable the L1 selection tests use.
//!
//! Decision 6: gated behind the `e2e` feature so `cargo test-fast` never
//! compiles it.

mod common;

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

/// Write a fixture "agent" for the Mode tab: it self-posts a `SessionStart`
/// via the real `dot-agent-deck hook` path (using the per-pane
/// `DOT_AGENT_DECK_PANE_ID` injected by the daemon), so the Mode AGENT pane is
/// registered as a Dashboard card mapped to that pane id. It then writes a
/// `started-card.log` marker and sleeps, keeping the pane alive (focusable) for
/// the whole test. Mirrors the agent-script pattern in `e2e_mode_seed_prompt.rs`.
fn write_card_agent(work: &std::path::Path) -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let body = format!(
        "#!/bin/sh\n\
         printf '%s' '{{\"hook_event_name\":\"SessionStart\",\"session_id\":\"modecard\"}}' \
         | \"{bin}\" hook claude-code >/dev/null 2>&1\n\
         echo ready > started-card.log\n\
         sleep 600\n"
    );
    let path = work.join("card-agent.sh");
    std::fs::write(&path, body).expect("write mode agent script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod mode agent script");
    }
    "./card-agent.sh".to_string()
}

/// Drive the new-pane dialog to spawn the `[[modes]]` entry at `mode_index`
/// (1-based) with the agent `command`. Mirrors `e2e_mode_seed_prompt.rs`:
/// Ctrl+n → dir-picker (Space confirms cwd) → form (Right ×mode_index selects
/// the mode, Enter → Name, Enter → Command, type command, Enter submits).
fn spawn_mode(deck: &TuiDeck, mode_index: usize, command: &str) {
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.send_keys(b" "); // Space → confirm current dir → new-pane form
    deck.wait_for_string("No mode"); // form up, Mode field focused at "No mode"
    let mut mode_keys = Vec::new();
    for _ in 0..mode_index {
        mode_keys.extend_from_slice(b"\x1b[C"); // Right → next mode option
    }
    deck.send_keys(&mode_keys);
    deck.send_keys(b"\r"); // Mode → Name
    deck.send_keys(b"\r"); // Name (default) → Command
    deck.send_keys(command.as_bytes());
    deck.send_keys(b"\r"); // submit
}

/// Scenario: SC1 against the real binary. Spawn a Mode tab whose agent pane
/// self-posts `SessionStart` (so that agent pane is ALSO a Dashboard card),
/// switch to the Dashboard and arm the highlight with `j` (a `▸` marker
/// appears), then drive the real tab-switch keys to switch away to the Mode tab
/// and back to the Dashboard. On return the Mode agent pane is still the focused
/// pane (the Dashboard restores nothing), so under the old behavior the
/// per-frame reconcile re-armed the highlight from that steady-state focus; the
/// fix requires a genuine focus transition, so NO card may carry the `▸`
/// selection marker on return. Asserts the `▸` is present after arming and
/// absent after the round-trip.
#[spec("dashboard/selection/015")]
#[test]
fn selection_015_tab_round_trip_clears_highlight_real_binary() {
    // `modes` fixture defines a single mode ("demo") so the new-pane form
    // exposes a Mode field and selecting it opens a Mode tab (Dashboard + Mode
    // = the ≥2 tabs SC1 needs).
    let deck = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("modes");
    deck.wait_for_string("No active sessions");
    let work = deck.workdir().to_path_buf();
    let agent = write_card_agent(&work);

    // Spawn the "demo" mode (index 1). Its agent pane posts SessionStart, so the
    // Mode AGENT pane becomes a Dashboard card mapped to that pane id — the bug's
    // precondition (the agent pane is NOT excluded from the card list).
    spawn_mode(&deck, 1, &agent);
    assert!(
        common::wait_for_path(&work.join("started-card.log"), Duration::from_secs(20)),
        "the mode agent pane must spawn, post SessionStart, and run"
    );

    // Detach to Normal mode on the Mode tab, then switch to the Dashboard
    // (Left → CycleTabPrev → tab 0). The Mode agent pane stays focused. The
    // dashboard is identified by its stable `session(s)` content header (the
    // card title is the random temp-dir basename).
    deck.send_bytes(b"\x04"); // Ctrl+D → Normal mode (still on the Mode tab)
    deck.send_bytes(b"\x1b[D"); // Left → previous tab → Dashboard
    deck.wait_for_string("session(s)"); // the mode-agent card is on the Dashboard

    // Arm the highlight on the Dashboard: `j` activates the selection on the
    // first card, painting the `▸` marker (and mirroring focus to that card's
    // pane — the mode agent pane).
    deck.send_bytes(b"j");
    deck.wait_for_string("\u{25b8}"); // ▸ — a card is now highlighted

    // SC1 round-trip: switch away to the Mode tab (Right → CycleTabNext) and
    // back to the Dashboard (Left → CycleTabPrev). The Mode agent pane remains
    // the focused pane on return (steady state — no focus transition).
    deck.send_bytes(b"\x1b[C"); // Right → next tab → Mode tab
    deck.wait_for_absence("session(s)"); // left the Dashboard (Mode tab shown)
    deck.send_bytes(b"\x1b[D"); // Left → previous tab → Dashboard
    deck.wait_for_string("session(s)"); // confirm we are back on the Dashboard

    // SC1: no card may carry the `▸` selection highlight after the round-trip.
    // Under the pre-fix behavior the restored steady-state focus re-armed the
    // highlight, so `▸` would reappear and this would time out.
    deck.wait_for_absence("\u{25b8}");
}

/// Drive the new-pane dialog to open the (single) orchestration in the fixture.
/// With no `[[modes]]` defined the Mode chip row is `[No mode] [Orch: …]
/// [schedule]`, so ONE Right selects the orchestration; selecting an
/// orchestration HIDES the Command field, so a second Enter submits the form.
fn open_orchestration(deck: &TuiDeck) {
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.send_keys(b" "); // Space → confirm current dir → new-pane form
    deck.wait_for_string("No mode"); // form up, Mode field focused at "No mode"
    deck.send_keys(b"\x1b[C"); // Right → [Orch: demo-orch]
    deck.send_keys(b"\r"); // Mode → Name
    deck.send_keys(b"\r"); // submit (Command hidden for an orchestration)
}

/// Scenario: PR #151 manual-test regression (Issue 1) against the real binary —
/// Enter must PAINT the selection highlight on the Orchestration deck after a
/// tab round-trip. Open an orchestration (two `cat` role cards), detach to
/// Normal mode, arm a role with `j` (a `▸` marker appears), then round-trip
/// Orchestration → Dashboard → Orchestration (the highlight clears, SC1). On
/// return the orchestration re-focuses its remembered role pane, so pressing
/// Enter re-focuses the SAME pane — not a focus transition — and under the
/// pre-fix behavior the reconcile never re-arms the highlight, so `▸` never
/// reappears (this `wait_for_string` times out). The unified fix makes
/// Action::Focus set `selected_index` directly, so Enter paints the highlight on
/// the orchestration deck just like the Dashboard. This is the higher-risk
/// VISUAL behavior the L1 mocks repeatedly missed (they never run the real
/// reconcile + focus-restore on an orchestration tab), hence the L2 coverage.
#[spec("dashboard/selection/019")]
#[test]
fn selection_019_enter_paints_highlight_on_orchestration_real_binary() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("orch-deck");
    deck.wait_for_string("No active sessions");

    // Open the orchestration tab (Dashboard + Orchestration = the ≥2 tabs the
    // round-trip needs). Its two `cat` role panes render as deck cards.
    open_orchestration(&deck);
    deck.wait_for_string("worker"); // the 2nd role card → orchestration deck is up

    // Detach to Normal mode on the Orchestration tab so `j` / Enter drive the
    // deck (Enter would otherwise type into the focused role pane).
    deck.send_bytes(b"\x04"); // Ctrl+D → Normal mode
    // Arm a role: `j` activates the deck selection, painting the `▸` marker.
    deck.send_bytes(b"j");
    deck.wait_for_string("\u{25b8}"); // ▸ — a role card is now highlighted

    // Round-trip: Orchestration → Dashboard → Orchestration. The highlight
    // clears on the tab switch (SC1 / orchestration_003).
    deck.send_bytes(b"\x1b[D"); // Left → previous tab → Dashboard
    deck.wait_for_absence("worker"); // left the orchestration (role cards gone; role panes are excluded from the Dashboard)
    deck.wait_for_absence("\u{25b8}"); // highlight cleared
    deck.send_bytes(b"\x1b[C"); // Right → next tab → Orchestration
    deck.wait_for_string("worker"); // back on the Orchestration deck

    // THE REGRESSION: Enter on the Orchestration deck must RESTORE the previous
    // role AND paint its highlight. Pre-fix the role pane is already focused, so
    // Enter is not a focus transition and the highlight never reappears — this
    // wait times out. Post-fix Action::Focus sets `selected_index` directly.
    deck.send_bytes(b"\r"); // Enter → Action::Focus (restore + paint)
    deck.wait_for_string("\u{25b8}"); // ▸ must reappear on the restored role
}
