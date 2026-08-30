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

// ---------------------------------------------------------------------------
// PRD #313 — `Ctrl+Z` in command mode zooms the focused role pane.
// ---------------------------------------------------------------------------

/// The zoom indicator fused into the focused pane's border title while zoomed —
/// a bracketed `Z`, mirroring tmux's status-line `Z`. Bracketed rather than a
/// bare letter so it cannot collide with a role name, an agent's own output, or
/// a sidebar card label anywhere on the settled grid.
///
/// Bracketing narrows the accidental collisions, not the deliberate ones: a
/// display name is agent-reachable and `sanitize_display_name` does not strip
/// brackets. So every POSITIVE assertion below goes through
/// [`role_border_title_marked`] rather than searching the whole grid; the
/// negatives stay broad, which is the stronger direction for them.
const ZOOM_MARKER: &str = "[Z]";

/// Column of the box drawn for the role pane named `role`, or `None` when no
/// such expanded box is on the grid. [`orchestrator_box_edge`] is this with
/// `"orchestrator"`; under `PaneLayout::Stacked` only the FOCUSED role's pane is
/// drawn, so a test that has jumped focus to a non-start role (as the real-agent
/// zoom test below does) has to name the role it focused. The scan itself lives
/// in the harness, for the same one-copy reason recorded there.
fn role_box_edge(grid: &str, role: &str) -> Option<u16> {
    common::role_pane_left_edge(grid, role).map(|column| column as u16)
}

/// Whether the zoom marker rides on the BORDER TITLE of the expanded box drawn
/// for `role` — the positional form of `grid.contains(ZOOM_MARKER)`.
///
/// The bare `contains` is not safe to assert on: a pane title is a *display
/// name*, display names arrive over the hook socket, and
/// `sanitize_display_name` strips control characters and bidi overrides but not
/// brackets — so an agent that calls itself `worker [Z]` paints that token onto
/// an UNZOOMED pane's border (or a sidebar card) and satisfies a whole-grid
/// search with nothing zoomed at all. Requiring the marker at the END of the
/// title of the box the geometry actually expanded is what a text-only vt100
/// grid can still prove; the styled-span half — the real marker is drawn
/// REVERSED, which plain title text never is — is asserted by
/// `render/layout/006`, whose `Buffer` keeps the cells' attributes.
///
/// `ends_with` rather than an exact title match on purpose: a real agent may
/// rename itself mid-run, and this must pin WHERE the marker is, not what the
/// role happens to be called at that moment.
fn role_border_title_marked(grid: &str, role: &str) -> bool {
    common::role_pane_border_title(grid, role).is_some_and(|title| title.ends_with(ZOOM_MARKER))
}

/// Scenario: Open the `orch-focus-lifecycle` fixture's 3-role orchestration on a
/// 120-column PTY and confirm `Ctrl+Z` zooms only where it should. First press
/// `Ctrl+Z` while still in PaneInput and confirm the layout does NOT change —
/// there it is `0x1a`, job control belonging to the agent. Then `Ctrl+d` to
/// command mode and press `Ctrl+Z`: the sidebar disappears, the box moves to
/// column 0 and its border title gains the `[Z]` marker, while every role's
/// agent is still registered and running behind the zoom. A second press restores
/// the 34/66 split with the other roles' sidebar cards — and `beta`'s live
/// hook-driven `Working` status — visible again.
#[spec("tabs/orchestration/011")]
#[test]
fn orchestration_011_z_zooms_the_focused_role_pane_in_command_mode() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("orch-focus-lifecycle");
    write_beta_agent(&deck);
    deck.wait_for_string("No active sessions");

    open_orchestration(&deck);
    deck.wait_for_string("alpha");
    deck.wait_for_string("beta");

    // `beta` reaches `Working` from its own self-posted hook events. Waiting for
    // it BEFORE zooming is what lets the post-unzoom check below mean "the
    // status survived the round trip" rather than "it happened to arrive late".
    assert!(
        common::wait_until(Duration::from_secs(15), || {
            has_role_status(&deck.snapshot_grid(), "beta", "Working")
        }),
        "precondition: beta's sidebar status never reached Working:\n{}",
        deck.snapshot_grid()
    );

    // Baseline: the default 34/66 split puts the pane column's left edge at 34%
    // of the 120-col frame (col 40 or 41, depending on Percentage rounding).
    let default_edge = pane_column_left_edge(&deck.snapshot_grid());
    assert!(
        (40..=41).contains(&default_edge),
        "expected the default 34/66 split's pane-column edge near col 40/41, \
         got {default_edge}\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // (1) In PaneInput `Ctrl+Z` is JOB CONTROL — the tty's SUSP character,
    // `0x1a` — and it belongs to whatever runs in the role pane, so it must not
    // zoom. Asserted as a predicate that is expected to TIME OUT (the
    // `orchestration_007` shape).
    deck.send_bytes(b"\x1a"); // Ctrl+Z == 0x1a
    // Deliberately the BROAD `contains` here, unlike the positive assertions
    // below: this predicate is expected to TIME OUT, so anything that trips it
    // fails the test. Narrowing it to the border title would make it harder to
    // trip and so WEAKEN the check; a spoofed display name could only ever
    // cause a false FAILURE here, never a false pass.
    let zoomed_in_pane_input = deck
        .wait_for_grid_predicate_within(Duration::from_secs(2), |grid| {
            orchestrator_box_edge(grid).is_some_and(|e| e <= 1) || grid.contains(ZOOM_MARKER)
        });
    assert!(
        !zoomed_in_pane_input,
        "`Ctrl+Z` must NOT zoom while in PaneInput — there it is job control \
         the agent is entitled to receive. PRD #313 scopes the toggle to \
         command mode.\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // (2) Ctrl+d -> command mode, where `Ctrl+Z` DOES resolve. The sidebar goes and
    // the focused pane's box moves to column 0 — but it KEEPS its border (the
    // corner glyph the edge scan anchors on is that border) and the border
    // title gains the zoom marker.
    deck.send_bytes(b"\x04"); // Ctrl+d -> command mode
    deck.send_bytes(b"\x1a"); // Ctrl+Z == 0x1a
    let zoomed = deck.wait_for_grid_predicate_within(Duration::from_secs(5), |grid| {
        orchestrator_box_edge(grid) == Some(0) && role_border_title_marked(grid, "orchestrator")
    });
    assert!(
        zoomed,
        "`Ctrl+Z` in command mode did not zoom the focused role pane within 5s — \
         expected the pane box at column 0 with a {ZOOM_MARKER} marker at the \
         end of THAT box's border title (not merely somewhere on the grid, \
         which an agent-supplied display name can spell), got edge {:?} and \
         title {:?}\nGrid:\n{}",
        orchestrator_box_edge(&deck.snapshot_grid()),
        common::role_pane_border_title(&deck.snapshot_grid(), "orchestrator"),
        deck.snapshot_grid()
    );

    // The non-focused roles' sidebar cards are gone — that is the point of the
    // feature, and it is the half a "widen the pane column a bit more"
    // implementation would fail.
    let zoomed_grid = deck.snapshot_grid();
    assert!(
        !has_role_status(&zoomed_grid, "beta", "Working"),
        "while zoomed, the sidebar (and beta's status card with it) must not be \
         drawn\nGrid:\n{zoomed_grid}"
    );

    // (3) Every non-focused agent KEEPS RUNNING while zoomed — the daemon's
    // live agent registry still holds all three roles. Hiding a pane is a
    // presentation change; it must not touch a single agent's lifecycle.
    let live: Vec<String> = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .filter_map(|r| r.display_name)
        .collect();
    for role in ["orchestrator", "alpha", "beta"] {
        assert!(
            live.iter().any(|n| n == role),
            "role `{role}`'s agent must still be live while the tab is zoomed \
             (zoom hides panes, it does not stop agents); live roles: {live:?}"
        );
    }

    // (4) A second `Ctrl+Z` restores the previous view exactly.
    deck.send_bytes(b"\x1a"); // Ctrl+Z == 0x1a
    // Whole-grid NEGATIVE, kept broad for the same reason as the PaneInput
    // check above: "the marker is nowhere" is strictly stronger than "the
    // marker is not on this one border", and unspoofable in the pass direction.
    let restored = deck.wait_for_grid_predicate_within(Duration::from_secs(5), |grid| {
        orchestrator_box_edge(grid).is_some_and(|e| (40..=41).contains(&e))
            && !grid.contains(ZOOM_MARKER)
    });
    assert!(
        restored,
        "a second `Ctrl+Z` did not restore the 34/66 split within 5s — pane-column \
         edge stayed at {:?}\nGrid:\n{}",
        orchestrator_box_edge(&deck.snapshot_grid()),
        deck.snapshot_grid()
    );
    let restored_grid = deck.snapshot_grid();
    assert!(
        has_role_status(&restored_grid, "beta", "Working"),
        "after unzooming, beta's sidebar status card must be back and still \
         reading Working — the zoom round trip must not cost the live status of \
         the roles it hid\nGrid:\n{restored_grid}"
    );
}

/// Sentinel files the REAL agent is asked to name. Neither token appears in the
/// directive that asks for it, so a match on the grid can only come from the
/// agent having actually run `ls` and printed what it found — an echo of the
/// user's own typing can never satisfy it.
const ZOOM_LIVE_SENTINEL: &str = "zoomlive_x7q2m.txt";
const ZOOM_LIVE_SENTINEL_TOKEN: &str = "x7q2m";
const ZOOM_LIVE_SENTINEL_2: &str = "zoomlive_k4v9p.log";
const ZOOM_LIVE_SENTINEL_2_TOKEN: &str = "k4v9p";

/// Whether `needle` is visible inside the pane column of the role pane `role`,
/// wrap-insensitively. Cropping to the pane column first drops the sidebar,
/// whose card text would otherwise splice between two wrapped rows of agent
/// output and break a needle that straddles the wrap column.
fn pane_column_shows(deck: &TuiDeck, role: &str, needle: &str) -> bool {
    let grid = deck.snapshot_grid();
    common::role_pane_column(&grid, role)
        .is_some_and(|column| common::squeeze_wrapped_text(&column).contains(needle))
}

/// Type `directive` at the focused REAL agent's pane and wait until `token`
/// appears in `role`'s pane column, re-pressing Enter on a cadence until it
/// does. Returns whether the token arrived inside `budget`.
///
/// **The Enter nudge is load-bearing, not defensive.** Claude Code accepts
/// characters into its composer from the moment it first draws, but an Enter
/// that arrives in the first seconds of that boot is DROPPED — the text stays in
/// the input box and nothing is ever submitted. Measured on this test's first
/// run against the implementation: the whole focus -> zoom -> type sequence had
/// completed by t=5.0s of the cast, and the directive then sat unsubmitted in
/// the composer, verbatim, for the remaining 175s of the budget until the test
/// failed. `orchestration/lock/012` types a directive exactly the same way and
/// does not hit this only because its own 20s locked-directive wait sits in
/// front of it as an accidental readiness gate.
///
/// A re-pressed Enter after the agent HAS submitted is harmless: Claude Code
/// ignores a submit on an empty composer. The nudge is therefore bounded
/// recovery for a lost keystroke, never a way to make an agent that answered
/// wrongly eventually answer right — only `token` ends the wait.
fn directive_is_answered(
    deck: &TuiDeck,
    role: &str,
    directive: &[u8],
    token: &str,
    budget: Duration,
) -> bool {
    deck.send_keys(directive);
    let deadline = std::time::Instant::now() + budget;
    loop {
        if common::wait_until(Duration::from_secs(15), || {
            pane_column_shows(deck, role, token)
        }) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        deck.send_bytes(b"\r");
    }
}

/// Scenario: Open the `orch-lock-live` fixture's orchestration (a `cat`
/// orchestrator plus a REAL fully interactive Claude Haiku worker), jump to the
/// worker with `2` so the live agent is the focused pane, wait for its pane to
/// go quiet so the agent is genuinely ready for input, and zoom it with `Ctrl+Z`.
/// While zoomed, type a directive asking the agent to `ls` and name the only
/// `.txt` file — its answer painting inside the full-width pane is proof the
/// real agent reflowed to the new PTY size and kept working. Then unzoom and
/// repeat with a `.log` file, proving it reflows back down just as well. Uses a
/// cheap model and two short turns; self-skips where the CLI or credentials are
/// absent.
#[spec("tabs/orchestration/012")]
#[test]
fn orchestration_012_real_agent_reflows_across_a_zoom_round_trip() {
    // A missing CLI or credentials is an environmental condition, not a broken
    // test (Decision 26).
    skip_unless!(common::check_claude_available());

    let deck = TuiDeck::builder()
        .with_pty_size(120, 40)
        .with_imported_claude_credentials()
        // The worker's cwd is the deck's own workdir (the copied
        // `orch-lock-live` fixture root); pre-trust it so the real claude's
        // first-run onboarding/trust gates clear with no keystroke and the
        // directives below are not swallowed answering them.
        .with_claude_trust_workdir()
        .launch_with_fixture("orch-lock-live");
    deck.wait_for_string("No active sessions");

    // Uniquely-named fixture sentinels the agent has to DISCOVER. Written into
    // the agents' cwd before the orchestration opens.
    for name in [ZOOM_LIVE_SENTINEL, ZOOM_LIVE_SENTINEL_2] {
        std::fs::write(deck.workdir().join(name), b"").expect("write zoom sentinel file");
    }

    open_orchestration(&deck);
    deck.wait_for_string("worker"); // 2nd role card -> orchestration tab is up

    // Focus the REAL worker role (Ctrl+d then `2` -> Jump2 -> FocusCard(1)).
    // `focus_deck` re-enters PaneInput on success, so no separate Enter.
    deck.send_bytes(b"\x04");
    deck.send_keys(b"2");
    let worker_pane_up = deck.wait_for_grid_predicate_within(Duration::from_secs(60), |grid| {
        role_box_edge(grid, "worker").is_some_and(|e| (40..=41).contains(&e))
    });
    assert!(
        worker_pane_up,
        "the real worker role's pane never became the focused/expanded pane at \
         the default 34/66 split\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // The deck drawing the worker's box says nothing about the REAL agent inside
    // it being ready for input, and typing at one that is not costs the whole
    // run: an Enter pressed during Claude Code's first seconds is dropped and
    // the directive sits in the composer forever (see `directive_is_answered`).
    // Wait for the pane's own byte stream to go quiet, the same primitive
    // `orchestration/lock/012` gets for free from the wait that precedes it.
    let worker_id = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| record.display_name.as_deref() == Some("worker"))
        .map(|record| record.id)
        .expect("the worker role's agent is registered with the daemon");
    assert!(
        common::wait_until_panes_settled(
            deck.attach_socket_path(),
            std::slice::from_ref(&worker_id),
            Duration::from_millis(1500),
            Duration::from_secs(3),
            Duration::from_secs(90),
        ),
        "the real worker agent's pane never settled within 90s, so a directive \
         typed at it now would race its boot\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // Zoom the LIVE agent's pane.
    deck.send_bytes(b"\x04"); // Ctrl+d -> command mode
    deck.send_bytes(b"\x1a"); // Ctrl+Z == 0x1a
    let zoomed = deck.wait_for_grid_predicate_within(Duration::from_secs(5), |grid| {
        role_box_edge(grid, "worker") == Some(0) && role_border_title_marked(grid, "worker")
    });
    assert!(
        zoomed,
        "`Ctrl+Z` did not zoom the real agent's pane within 5s — expected its box at \
         column 0 with a {ZOOM_MARKER} marker at the end of THAT box's border \
         title (not merely somewhere on the grid, which the agent's own display \
         name can spell), got edge {:?} and title {:?}\nGrid:\n{}",
        role_box_edge(&deck.snapshot_grid(), "worker"),
        common::role_pane_border_title(&deck.snapshot_grid(), "worker"),
        deck.snapshot_grid()
    );

    // The PRD's "resize churn" risk, verified with a real agent rather than a
    // stand-in: the agent must still be working AND painting at the new width.
    // The directive never names the sentinel token, so only a genuine `ls` can
    // put it on the grid.
    deck.send_bytes(b"\x04"); // Ctrl+d -> PaneInput on the zoomed pane
    let painted_zoomed = directive_is_answered(
        &deck,
        "worker",
        b"Use the Bash tool to run ls in the current directory, then reply with \
          the full name of the only file whose name ends in .txt, and nothing \
          else.\r",
        ZOOM_LIVE_SENTINEL_TOKEN,
        Duration::from_secs(180),
    );
    assert!(
        painted_zoomed,
        "the real agent never painted {ZOOM_LIVE_SENTINEL} inside the ZOOMED \
         pane — after the zoom resize it either stopped working or stopped \
         rendering at the new width\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // Unzoom and prove it reflows back down just as well.
    deck.send_bytes(b"\x04"); // Ctrl+d -> command mode
    deck.send_bytes(b"\x1a"); // Ctrl+Z == 0x1a
    // Whole-grid NEGATIVE, kept broad: "the marker is nowhere" is the stronger
    // form and cannot be satisfied by a spoof.
    let restored = deck.wait_for_grid_predicate_within(Duration::from_secs(5), |grid| {
        role_box_edge(grid, "worker").is_some_and(|e| (40..=41).contains(&e))
            && !grid.contains(ZOOM_MARKER)
    });
    assert!(
        restored,
        "a second `Ctrl+Z` did not restore the 34/66 split around the real agent's \
         pane within 5s — edge stayed at {:?}\nGrid:\n{}",
        role_box_edge(&deck.snapshot_grid(), "worker"),
        deck.snapshot_grid()
    );

    deck.send_bytes(b"\x04"); // Ctrl+d -> PaneInput
    let painted_unzoomed = directive_is_answered(
        &deck,
        "worker",
        b"Use the Bash tool to run ls again, then reply with the full name of \
          the only file whose name ends in .log, and nothing else.\r",
        ZOOM_LIVE_SENTINEL_2_TOKEN,
        Duration::from_secs(180),
    );
    assert!(
        painted_unzoomed,
        "the real agent never painted {ZOOM_LIVE_SENTINEL_2} after UNZOOMING — \
         the second resize left it not working or not rendering\nGrid:\n{}",
        deck.snapshot_grid()
    );
}
