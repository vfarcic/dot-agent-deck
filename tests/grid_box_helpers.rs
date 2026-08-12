//! Fast-tier guards for the harness's shared box-drawing grid helpers
//! (`common::BORDER_WEIGHTS` and the predicates built on it).
//!
//! Both helpers guarded here exist because a LOOSER form of them shipped first
//! and was satisfied by the wrong thing:
//!
//! - `label_in_box_top_border` replaced
//!   `grid.lines().any(|l| l.contains('┌') && l.contains(label))`, which an
//!   AGENT could satisfy — on an Orchestration tab the sidebar's deck cards and
//!   the focused role's live terminal share rows, so a row holding a UUID-titled
//!   card's top-left corner also holds whatever that terminal printed to its
//!   right, including every role name.
//! - `orchestration_pane_column` replaced a whole-grid search whose row-join
//!   spliced sidebar card text BETWEEN adjacent wrapped pane rows, breaking any
//!   needle that straddled the wrap — the root cause of `idle_worker_011` going
//!   red (issue #460).
//!
//! They live in the FAST tier, not beside their e2e callers, deliberately. Pure
//! string logic in a `tests/e2e_*.rs` file is gated `#![cfg(feature = "e2e")]`
//! and so runs only in the pre-PR tier a human triggers by hand (CLAUDE.md rule
//! 5) — the one guard against a regression here would fire nowhere in CI.
//! Nothing in this file spawns a PTY, a binary or a daemon, so there is no
//! reason for it to be gated at all (review of #465, S5).

mod common;

/// Every fixture below is a real rectangle: each row's right-hand glyph sits at
/// the same column as the top row's. `label_in_box_top_border` reads only the
/// top row and would not notice, but these constants read as documentation of a
/// rendered card, and a column-exact parser (`try_first_card` in
/// `tests/e2e_card_layout.rs`) requires alignment — so a fixture the renderer
/// could never produce would hand its next reader a confusing failure. Asserted
/// rather than merely eyeballed because two of them were silently 3 columns
/// adrift when written (review of #465, N1).
fn assert_is_rectangle(name: &str, grid: &str) {
    /// Column every fixture card below closes at.
    const RIGHT_EDGE: usize = 31;

    for (index, line) in grid.lines().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let opening = chars[0];
        let weight = common::BORDER_WEIGHTS
            .iter()
            .find(|w| [w.top_left, w.vertical, w.bottom_left].contains(&opening))
            .unwrap_or_else(|| {
                panic!("{name} row {index} does not open with a box glyph ({opening:?}):\n{grid}")
            });
        let closing = *chars.get(RIGHT_EDGE).unwrap_or_else(|| {
            panic!(
                "{name} row {index} is only {} scalars wide, so it cannot close its box at \
                 column {RIGHT_EDGE}:\n{grid}",
                chars.len()
            )
        });
        assert!(
            [weight.top_right, weight.vertical, weight.bottom_right].contains(&closing),
            "{name} row {index} closes at column {RIGHT_EDGE} with {closing:?}, which is not a \
             right-hand glyph of the weight it opened with ({opening:?}):\n{grid}"
        );
    }
}

/// Scenario: Run the shared card-title predicate against three hand-built grids
/// that reproduce what an Orchestration tab actually paints. The adversarial one
/// puts every role name in the live agent pane to the right of two UUID-titled
/// sidebar cards, and must NOT match `coder`; the plain, thick and double-weight
/// single-card grids each carry `coder` inside their own top border, and must.
#[test]
fn card_title_predicate_rejects_labels_outside_the_card_span() {
    const CODER_LABEL: &str = "ClaudeCode · coder";

    // The historical false pass: UUID-titled sidebar cards have a top-left
    // corner on rows where the adjacent live pane prints every role. The role
    // text is present on each row, but outside both cards' right edge.
    const FALSE_PASS_GRID: &str = "\
┌─ ClaudeCode · 6134822e-f2 ─┐  ┃ ClaudeCode · orchestrator ClaudeCode · coder ClaudeCode · reviewer
│ waiting                    │  ┃
└────────────────────────────┘  ┃
┏━ ClaudeCode · c15a2be1-77 ━┓  ┃ ClaudeCode · orchestrator ClaudeCode · coder ClaudeCode · reviewer
┃ working                    ┃  ┃
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛  ┃";
    assert!(
        !common::label_in_box_top_border(FALSE_PASS_GRID, CODER_LABEL),
        "a role label in the agent pane must not make a UUID-titled sidebar card match\n\
         Adversarial grid:\n{FALSE_PASS_GRID}"
    );
    // The two cards' OWN titles still match, so the rejection above is the
    // span crop doing its job rather than the predicate failing outright.
    assert!(
        common::label_in_box_top_border(FALSE_PASS_GRID, "ClaudeCode · 6134822e-f2"),
        "the plain card's own title must still match:\n{FALSE_PASS_GRID}"
    );
    assert!(
        common::label_in_box_top_border(FALSE_PASS_GRID, "ClaudeCode · c15a2be1-77"),
        "the thick card's own title must still match:\n{FALSE_PASS_GRID}"
    );

    const PLAIN_CARD_GRID: &str = "\
┌─ ClaudeCode · coder ─────────┐  ┃ live agent output
│                              │  ┃
└──────────────────────────────┘  ┃";
    const THICK_CARD_GRID: &str = "\
┏━ ClaudeCode · coder ━━━━━━━━━┓  ┃ live agent output
┃                              ┃  ┃
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛  ┃";
    // `BorderType::Double` is unreachable in `src/` today. Covered anyway
    // because `common::BORDER_WEIGHTS` deliberately lists it as forward
    // tolerance, and an untested table row is how the plain-vs-thick split that
    // caused #460 went unnoticed.
    const DOUBLE_CARD_GRID: &str = "\
╔═ ClaudeCode · coder ═════════╗  ┃ live agent output
║                              ║  ┃
╚══════════════════════════════╝  ┃";

    for (name, grid) in [
        ("PLAIN_CARD_GRID", PLAIN_CARD_GRID),
        ("THICK_CARD_GRID", THICK_CARD_GRID),
        ("DOUBLE_CARD_GRID", DOUBLE_CARD_GRID),
    ] {
        assert_is_rectangle(name, grid);
        assert!(
            common::label_in_box_top_border(grid, CODER_LABEL),
            "{name} carries {CODER_LABEL:?} inside its own top border:\n{grid}"
        );
    }
}

/// Scenario: Run the shared pane-column crop against three hand-built
/// Orchestration-tab grids. On an expanded grid of each border weight it finds
/// the `┌orchestrator` column and crops the sidebar away, so a needle that wraps
/// across two pane rows matches once squeezed — while the SAME needle does not
/// match the uncropped grid, because the row-join splices sidebar card text
/// between the two halves. On a grid whose panes are all collapsed it returns
/// `None` rather than a wrong column.
#[test]
fn orchestration_pane_crop_drops_the_sidebar_and_reports_a_missing_anchor() {
    // The wrap trap in miniature: the needle's two halves are on consecutive
    // PANE rows, and each of those rows also carries a sidebar card whose text
    // lands between them once the rows are joined.
    const NEEDLE: &str = "daemon report, not a message from a person";
    let expanded = |tl: char, v: char| {
        format!(
            "\
┌─ 1 ClaudeCode · orch… ─┐ {tl}orchestrator──────────────
│ working                │ {v} daemon report, not a
┌─ 2 ClaudeCode · worker ┐ {v} message from a person
│ idle                   │ {v}
"
        )
    };

    for weight in common::BORDER_WEIGHTS {
        let grid = expanded(weight.top_left, weight.vertical);
        let edge = common::orchestration_pane_left_edge(&grid).unwrap_or_else(|| {
            panic!(
                "no pane-column anchor found for the {:?} weight:\n{grid}",
                weight.top_left
            )
        });
        assert_eq!(
            edge, 27,
            "the {:?}-weight anchor is at column {edge}, not the sidebar boundary 27:\n{grid}",
            weight.top_left
        );

        let pane =
            common::orchestration_pane_column(&grid).expect("a located edge always yields a crop");
        assert!(
            !pane.contains("ClaudeCode"),
            "the crop must drop the sidebar's card text entirely:\n{pane}"
        );
        assert!(
            common::squeeze_wrapped_text(&pane).contains(&common::squeeze_wrapped_text(NEEDLE)),
            "the wrapped needle must be contiguous once the CROPPED rows are squeezed and \
             joined:\n{pane}"
        );
        // The regression this crop exists for: without it the sidebar's own text
        // is spliced between the needle's two halves, so the whole-grid search
        // misses a needle that is fully on screen.
        assert!(
            !common::squeeze_wrapped_text(&grid).contains(&common::squeeze_wrapped_text(NEEDLE)),
            "the UNCROPPED grid must still miss the needle — otherwise this fixture no longer \
             reproduces the splice that made the crop necessary:\n{grid}"
        );
    }

    // Every pane collapsed: a collapsed `Stacked` pane draws `Borders::TOP` with
    // a padded title and NO corner glyph, so there is no anchor to crop on. The
    // helper must say so rather than guess a column — the whole point of S4's
    // split failure reporting is that this state is NOT "the needle is absent".
    const COLLAPSED_GRID: &str = "\
┌─ 1 ClaudeCode · orch… ─┐ orchestrator ─────────────
│ working                │ worker ───────────────────
└────────────────────────┘";
    assert!(
        common::orchestration_pane_left_edge(COLLAPSED_GRID).is_none(),
        "a grid whose orchestrator pane is COLLAPSED has no corner anchor, so the edge must be \
         None rather than a wrong column:\n{COLLAPSED_GRID}"
    );
    assert!(
        common::orchestration_pane_column(COLLAPSED_GRID).is_none(),
        "no anchor means no crop:\n{COLLAPSED_GRID}"
    );
}
