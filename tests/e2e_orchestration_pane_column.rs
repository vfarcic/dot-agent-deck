#![cfg(feature = "e2e")]

//! PRD #311 — L2 (real-binary PTY) coverage for the Orchestration tab's
//! `PaneLayout::Stacked` pane column: removing the non-focused roles'
//! collapsed 1-row title frames must not touch agent lifecycle. Every role's
//! PTY stays open, keeps running, and keeps reporting status regardless of
//! whether its pane is currently drawn.

mod common;

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

/// A collapsed `Stacked` pane renders a `Block` with `Borders::TOP` and
/// `.title(format!(" {title} "))` — no other cell content. On the settled
/// grid that row, after trimming ONLY the leading blank columns (the sidebar
/// area to its left), reads exactly `"<role> ─..."` — the title text directly
/// followed by the border-fill dashes, nothing else. A sidebar deck card's
/// title line ("│ N status · role ─── status │") can never match: after
/// trimming leading whitespace it starts with the card's own border glyph
/// (`┌`/`┏`/`│`), not the bare role name. This is what makes the check
/// specific to the collapsed PANE-COLUMN frame rather than any other on-screen
/// occurrence of the role name.
fn has_collapsed_frame(grid: &str, role: &str) -> bool {
    let prefix = format!("{role} \u{2500}");
    grid.lines()
        .any(|line| line.trim_start().starts_with(&prefix))
}

/// A sidebar deck card's title row (`render_session_card` in `src/ui.rs`)
/// carries the role's display name and its live status word on the SAME
/// rendered line, joined by the card's `\u{00b7}` separator (`" \u{00b7} {name} "`
/// on the left, `" {dot} {status} "` on the right of the same `Block`). Search
/// for `"\u{00b7} {role}"` plus `status` together on one line so the check is
/// scoped to `role`'s own card rather than any occurrence of `status`
/// anywhere on the settled grid (e.g. another role's card, or pane content).
fn has_role_status(grid: &str, role: &str, status: &str) -> bool {
    let role_needle = format!("\u{00b7} {role}");
    grid.lines()
        .any(|line| line.contains(&role_needle) && line.contains(status))
}

/// Overwrite the fixture's `beta-agent.sh` placeholder with the ABSOLUTE path
/// of the freshly built test binary baked in (mirrors `write_card_agent` in
/// `e2e_dashboard_selection.rs`), rather than relying on `dot-agent-deck`
/// resolving correctly on PATH — a dev machine may have a separately
/// installed `dot-agent-deck` shadowing the build under test.
fn write_beta_agent(deck: &TuiDeck) {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let body = format!(
        "#!/bin/sh\n\
         printf 'BETA_ROLE_SENTINEL\\n'\n\
         {session_start}\
         sleep 1\n\
         {pre_tool_use}\
         sleep 600\n",
        session_start = common::claude_session_start_line(bin, "beta-sess"),
        pre_tool_use = common::claude_hook_line(
            bin,
            r#"{"hook_event_name":"PreToolUse","session_id":"beta-sess","tool_name":"Bash"}"#,
        ),
    );
    let path = deck.workdir().join("beta-agent.sh");
    std::fs::write(&path, body).expect("overwrite beta-agent.sh with resolved binary path");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod beta-agent.sh");
    }
}

/// Drive the new-pane dialog to open the (single) orchestration in the
/// `orch-focus-lifecycle` fixture. With no `[[modes]]` defined the Mode chip
/// row is `[No mode] [Orch: focus-lifecycle] [schedule]`, so ONE Right
/// selects the orchestration; selecting an orchestration hides the Command
/// field, so a second Enter submits the form.
fn open_orchestration(deck: &TuiDeck) {
    deck.send_bytes(b"\x0e"); // Ctrl+n -> directory picker
    deck.send_bytes(b" "); // Space -> confirm current dir -> new-pane form
    deck.wait_for_string("No mode");
    deck.send_bytes(b"\x1b[C"); // Right -> [Orch: focus-lifecycle]
    deck.send_bytes(b"\r"); // Mode -> Name
    deck.send_bytes(b"\r"); // submit (Command hidden for an orchestration)
}

/// Scenario: Open the `orch-focus-lifecycle` fixture's 3-role orchestration
/// (`orchestrator` start role, plus `alpha` and `beta`) in the deck's default
/// `PaneLayout::Stacked`. (b) Confirm a non-focused role keeps running and its
/// sidebar status transitions live: `beta`'s status card goes from idle to
/// `Working` purely through its own self-posted hook events while its pane is
/// NOT the expanded/focused slot. (a) Assert the settled grid carries NO
/// collapsed title-bar frame for either non-focused role (`alpha`, `beta`) —
/// PRD #311 removes that arm of `PaneLayout::Stacked` entirely. (c) Drive `j`
/// twice (Normal mode) to move the deck's focus orchestrator -> alpha -> beta,
/// then `k` twice back to orchestrator, asserting each role's own sentinel
/// text is visible once it becomes the focused/expanded pane — proving no lost
/// scrollback or stale fragment survives a focus round trip. RED today: (a)
/// fails because `render_terminal_panes`' Stacked else-arm
/// (`src/ui.rs:11890-11908`) still draws a `Borders::TOP` titled block for
/// every non-focused role.
#[spec("tabs/orchestration/006")]
#[test]
fn orchestration_006_stacked_pane_column_hides_collapsed_frames_while_agents_stay_live() {
    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .launch_with_fixture("orch-focus-lifecycle");
    write_beta_agent(&deck);
    deck.wait_for_string("No active sessions");
    open_orchestration(&deck);
    deck.wait_for_string("orchestrator");
    deck.wait_for_string("alpha");
    deck.wait_for_string("beta");

    // (b) The non-focused `beta` role keeps running and its sidebar status
    // transitions live (Idle -> Working) purely from its own self-posted hook
    // events, while its pane is not the expanded/focused slot.
    assert!(
        common::wait_until(Duration::from_secs(15), || {
            has_role_status(&deck.snapshot_grid(), "beta", "Working")
        }),
        "the non-focused beta role's sidebar status never transitioned to \
         Working while its pane was collapsed/not drawn:\n{}",
        deck.snapshot_grid()
    );

    // (a) With `orchestrator` focused/expanded (the start role), neither
    // non-focused role may render a collapsed title-bar frame.
    let grid = deck.snapshot_grid();
    assert!(
        !has_collapsed_frame(&grid, "alpha"),
        "non-focused role 'alpha' must not render a collapsed title-bar frame \
         in PaneLayout::Stacked:\n{grid}"
    );
    assert!(
        !has_collapsed_frame(&grid, "beta"),
        "non-focused role 'beta' must not render a collapsed title-bar frame \
         in PaneLayout::Stacked:\n{grid}"
    );

    // (c) Switching focus between roles preserves each agent's rendered
    // content, with no lost scrollback / stale fragment across the round
    // trip: orchestrator -> alpha -> beta -> alpha -> orchestrator.
    deck.send_bytes(b"\x04"); // Ctrl+D -> Normal mode
    deck.send_bytes(b"j"); // orchestrator -> alpha
    deck.wait_for_string("ALPHA_ROLE_SENTINEL");
    deck.send_bytes(b"j"); // alpha -> beta
    deck.wait_for_string("BETA_ROLE_SENTINEL");
    deck.send_bytes(b"k"); // beta -> alpha
    deck.wait_for_string("ALPHA_ROLE_SENTINEL");
    deck.send_bytes(b"k"); // alpha -> orchestrator
    deck.wait_for_string("ORCH_ROLE_SENTINEL");
}

// ---------------------------------------------------------------------------
// PRD #336 — `Ctrl+l` toggles the orchestration sidebar/pane-column split.
// ---------------------------------------------------------------------------

/// Column index of the orchestration tab's role-pane column's LEFT edge: the
/// role-pane box drawn for the fixture's `start = true` role ("orchestrator")
/// renders its title fused into the top border as `┌orchestrator───…`, so the
/// column of that `┌` is exactly `panes_area.x` — the boundary between the
/// sidebar (role list) and the pane column that `orchestration_split_percents`
/// controls. Distinct from the sidebar's own truncated `orchestrat…` card
/// label, so there is no collision risk.
///
/// The corner glyph is NOT fixed: PRD #341 ("make UI modes unmistakable")
/// renders the focused pane's border heavier in command mode, so the same box
/// reads `┌orchestrator` in PaneInput and `┏orchestrator` in command mode.
/// [`common::orchestration_pane_left_edge`] matches any weight — this helper is
/// about the box's COLUMN, not its styling, and pinning one glyph made the test
/// fail on a mode switch that was working correctly. That scan lives in the
/// harness rather than here because `e2e_idle_worker_detector.rs` needs the same
/// anchor to crop on, and two copies is one copy too many for a scan whose glyph
/// set has already had to change once (review of #465, S1).
fn orchestrator_box_edge(grid: &str) -> Option<u16> {
    common::orchestration_pane_left_edge(grid).map(|column| column as u16)
}

/// Panicking form of [`orchestrator_box_edge`], for use outside a predicate
/// where the box must exist.
fn pane_column_left_edge(grid: &str) -> u16 {
    orchestrator_box_edge(grid).unwrap_or_else(|| {
        panic!("orchestrator role-pane box top border not found in grid:\n{grid}")
    })
}

/// Scenario: Open a real orchestration tab from the `orch-deck` fixture (two
/// stub `cat` roles) on a 120-column PTY at the default 34/66 split, and
/// confirm the pane column's left edge sits at the ~34%-width boundary. Opening
/// the tab leaves the deck in PaneInput, so first press `Ctrl+l` there and
/// confirm the boundary does NOT move — PRD #336 scopes the toggle to command
/// mode so the chord stays available to the role pane's agent. Then `Ctrl+d` to
/// command mode, press `Ctrl+l` and wait for the boundary to move to the
/// narrower-sidebar 25%-width position (sidebar visibly narrows, pane column
/// visibly widens), and press `Ctrl+l` again to confirm it returns to 34%.
#[spec("tabs/orchestration/007")]
#[test]
fn orchestration_007_ctrl_l_toggles_pane_column_split() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("orch-deck");
    deck.wait_for_string("No active sessions");

    // Same keystrokes as the `orch-focus-lifecycle` opener above: with no
    // `[[modes]]` in the fixture, ONE Right selects `[Orch: demo-orch]`, and
    // selecting an orchestration hides the Command field so a second Enter
    // submits the form.
    open_orchestration(&deck);
    deck.wait_for_string("worker"); // 2nd role card -> orchestration tab is up

    // Baseline: the default 34/66 split puts the pane column's left edge at
    // 34% of the 120-col frame (col 40 or 41, depending on Percentage
    // rounding) — well clear of the 25%-split boundary (col 30) asserted next.
    let default_edge = pane_column_left_edge(&deck.snapshot_grid());
    assert!(
        (40..=41).contains(&default_edge),
        "expected the default 34/66 split's pane-column edge near col 40/41, \
         got {default_edge}\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // Ctrl+l: toggle to the narrower-sidebar 25/75 split. The sidebar narrows
    // and the pane column widens, so the boundary column DECREASES (25% of
    // 120 = col 30) — a range tolerates rounding without pinning the constant.
    // Opening the tab leaves the deck in PaneInput (the role pane owns the
    // keyboard). PRD #336 scopes the toggle to COMMAND mode, mirroring
    // `close_pane` (PRD #241 M1), so Ctrl+l here belongs to whatever runs in
    // the role pane. It must NOT move the boundary — asserted as a predicate
    // that is expected to TIME OUT.
    deck.send_bytes(b"\x0c"); // Ctrl+l == 0x0c
    let narrowed_in_pane_input = deck
        .wait_for_grid_predicate_within(Duration::from_secs(2), |grid| {
            orchestrator_box_edge(grid).is_some_and(|e| (29..=30).contains(&e))
        });
    assert!(
        !narrowed_in_pane_input,
        "Ctrl+l must NOT toggle the split while in PaneInput — there the chord \
         belongs to the role pane's agent (readline's clear-screen). PRD #336 \
         scopes the toggle to command mode.\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // Ctrl+d -> command mode, where the toggle DOES resolve.
    deck.send_bytes(b"\x04");

    deck.send_bytes(b"\x0c"); // Ctrl+l == 0x0c
    let narrowed = deck.wait_for_grid_predicate_within(Duration::from_secs(5), |grid| {
        orchestrator_box_edge(grid).is_some_and(|e| (29..=30).contains(&e))
    });
    assert!(
        narrowed,
        "Ctrl+l did not narrow the sidebar to the 25/75 split within 5s — \
         pane-column edge stayed at {}\nGrid:\n{}",
        pane_column_left_edge(&deck.snapshot_grid()),
        deck.snapshot_grid()
    );

    // Second Ctrl+l: back to the 34/66 default.
    deck.send_bytes(b"\x0c");
    let restored = deck.wait_for_grid_predicate_within(Duration::from_secs(5), |grid| {
        orchestrator_box_edge(grid).is_some_and(|e| (40..=41).contains(&e))
    });
    assert!(
        restored,
        "a second Ctrl+l did not restore the 34/66 default split within 5s — \
         pane-column edge stayed at {}\nGrid:\n{}",
        pane_column_left_edge(&deck.snapshot_grid()),
        deck.snapshot_grid()
    );
}

/// Scenario: On the Dashboard (a NON-orchestration tab) with a live `cat -v`
/// pane in PaneInput mode, type a unique sentinel, then press `Ctrl+l`, then
/// Enter. `cat -v` renders the received control byte as the two characters
/// `^L`, so the pane echoes `<sentinel>^L` only if the raw `0x0c` actually
/// reached the PTY. `Action::ToggleOrchestrationSplit` must not claim `Ctrl+l`
/// off an orchestration tab (`scope_orchestration_split`), otherwise the
/// keystroke is swallowed — the dispatcher no-ops there — and the pane never
/// sees the byte. Regression guard for the Greptile P1 on PR #342.
///
/// Deliberately asserts on `cat -v`'s own rendering rather than on a shell's
/// readline `clear-screen` side effect: whether readline redraws depends on the
/// host's terminal setup, which made an earlier version of this test fail on a
/// machine where the forwarding was in fact working correctly.
#[spec("tabs/orchestration/008")]
#[test]
fn orchestration_008_ctrl_l_forwards_to_pty_on_non_orchestration_tab() {
    const SENTINEL: &str = "CTRLL_FWD_9f3c";

    let deck = TuiDeck::builder()
        .with_continue_session("ctrl-l-dashboard-cat", "cat -v")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]"); // live PTY, PaneInput mode

    // Type the sentinel, then the chord under test, then Enter to flush the
    // tty's canonical-mode line buffer through to `cat`.
    deck.send_keys(SENTINEL.as_bytes());
    deck.send_bytes(b"\x0c"); // Ctrl+l == 0x0c
    deck.send_bytes(b"\r");

    let needle = format!("{SENTINEL}^L");
    let forwarded =
        deck.wait_for_grid_predicate_within(Duration::from_secs(5), |grid| grid.contains(&needle));
    assert!(
        forwarded,
        "Ctrl+l never reached the `cat -v` pane on a non-orchestration tab: \
         expected {needle:?} in the grid. The global resolver claimed Ctrl+l as \
         Action::ToggleOrchestrationSplit even though the active tab is a \
         Dashboard tab, so the byte was swallowed instead of forwarded \
         (PRD #336 scope violation).\nGrid:\n{}",
        deck.snapshot_grid()
    );
}
