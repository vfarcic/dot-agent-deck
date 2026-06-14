# PRD #144: Give cramped UI surfaces more room (button-bar wrapping + wider modals)

**Status**: Complete
**Priority**: Medium
**Created**: 2026-06-12
**Completed**: 2026-06-14
**GitHub Issue**: [#144](https://github.com/vfarcic/dot-agent-deck/issues/144)
**Related**: [#127](https://github.com/vfarcic/dot-agent-deck/issues/127) (scheduler — surfaced the button-bar overflow), `src/ui.rs` (`render_button_bar`, `dashboard_context_buttons`, modal rendering, `layout_mode_chips`)

## Problem Statement

As features are added, the deck's **fixed-footprint UI surfaces** get cramped:

- **Bottom button bar.** It spans the full terminal width and, in full-label form, is now ~138 cols once the always-shown `[Scheduled Tasks s]` button is included. At modest/windowed widths (≈120 — a real split-screen width, and the pinned e2e reference width) the whole bar collapses to short key-chips (`[Ctrl+N] [Ctrl+W] … [Scheduled Tasks s]`). The degradation is also all-or-nothing and inconsistent: the scheduler button stays labeled while the rest chip. The scheduler branch's e2e gate surfaced this (11 mouse tests at 120 cols).
- **Modal dialogs.** Several get tight as content grows — the Scheduled Tasks manager, the new-pane/new-deck form, confirmations. This session band-aided a few (Schedule-field overflow, delete-confirm `wrap_to_width`, mode-chip wrap) without addressing modal sizing properly.

Common thread: content keeps growing on surfaces with fixed footprints, so they cram instead of being given more room.

## Implementation

### Button bar — wrap to second row

The button bar now wraps to a second row (or further) when full labels don't fit on one row at the current terminal width, keeping FULL labels for every button. Degradation is uniform — no single button (including `[Scheduled Tasks s]`) keeps its label while others chip. The bar no longer collapses to shortcut-only chips at the 120-col reference width.

The layout now reserves the bar's actual height (1 or 2+ rows) so panes/dashboard cede exactly the wrapped rows. The bar height is capped so at least 1 content row always remains regardless of terminal size.

### Modals — content-driven auto-size via shared helper

A single shared `modal_rect` helper sizes all modals: given content width/height + terminal dims, it returns the modal rect clamped to `≤90% of terminal`, centered, and never exceeding terminal bounds. All modal dialogs now route through this helper:
- Scheduled Tasks manager
- New-pane / new-deck form
- Confirmations

The per-finding band-aids were removed and superseded: `wrap_to_width`, `truncate_cell`, `layout_mode_chips`.

### No experimental flag

Ships visible by default — no `features::show_*` wrapper, no flag wiring, no experimental-flag docs.

## Gate Results

- `cargo test-fast`: 756 passed / 0 failed
- `cargo test-e2e`: 1118/1118 (including scheduler/manager/007, prompt/new-pane/007 & 009, mouse/buttonbar/003 & 004, mouse/modal/001)
- `cargo clippy -- -D warnings`: clean
- `cargo fmt --check`: clean

## Known Deferred Item

With an unusually long mode/orchestration list on a narrow/windowed terminal, the new-pane Mode-chip row can clip the trailing chip. This is the intended consequence of the approved 90%-width-clip modal design (the `[schedule]`-visibility guard `prompt/new-pane/009` passes at realistic configs). It is deliberately not fixed in this PR; a possible follow-up issue can be filed if needed.

## Out of Scope

- The scheduler behavior itself (#127).
- Supporting sub-80-col terminals as a first-class target.

## Notes

- Surfaced by PRD #127 scheduler manual-validation fixes (button-bar overflow at the 120-col e2e reference width).
- Existing band-aids folded in / superseded: Scheduled Tasks dialog field overflow, delete-confirm `wrap_to_width`, mode-chip wrap (`layout_mode_chips`).
