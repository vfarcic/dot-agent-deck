# PRD #144: Give cramped UI surfaces more room (button-bar wrapping + wider modals)

**Status**: Planning
**Priority**: Medium
**Created**: 2026-06-12
**GitHub Issue**: [#144](https://github.com/vfarcic/dot-agent-deck/issues/144)
**Related**: [#127](https://github.com/vfarcic/dot-agent-deck/issues/127) (scheduler — surfaced the button-bar overflow), `src/ui.rs` (`render_button_bar`, `dashboard_context_buttons`, modal rendering, `layout_mode_chips`)

## Problem Statement

As features are added, the deck's **fixed-footprint UI surfaces** get cramped:

- **Bottom button bar.** It spans the full terminal width and, in full-label form, is now ~138 cols once the always-shown `[Scheduled Tasks s]` button is included. At modest/windowed widths (≈120 — a real split-screen width, and the pinned e2e reference width) the whole bar collapses to short key-chips (`[Ctrl+N] [Ctrl+W] … [Scheduled Tasks s]`). The degradation is also all-or-nothing and inconsistent: the scheduler button stays labeled while the rest chip. The scheduler branch's e2e gate surfaced this (11 mouse tests at 120 cols).
- **Modal dialogs.** Several get tight as content grows — the Scheduled Tasks manager, the new-pane/new-deck form, confirmations. This session band-aided a few (Schedule-field overflow, delete-confirm `wrap_to_width`, mode-chip wrap) without addressing modal sizing properly.

Common thread: content keeps growing on surfaces with fixed footprints, so they cram instead of being given more room.

## Proposed Scope (to refine)

1. **Button bar — wrap instead of cram.** When the full-label bar doesn't fit on one row, **wrap to a second row** (keep full labels, spend vertical space) rather than collapsing to chips. Optionally **priority-based overflow** (chip the least-important buttons first so core actions keep labels as long as possible). Make degradation uniform — no single button keeping its label while others chip.
2. **Modals — give them width.** Audit the dialogs (Scheduled Tasks manager, new-pane/new-deck form, confirmations) and size them to their content (or a larger fraction of the terminal) so fields/labels aren't clipped or awkwardly wrapped. Replace the per-finding band-aids with one consistent sizing approach.
3. **Test the realistic range**, not a single pinned width — labeled/roomy full-screen *and* narrow/windowed degradation.

## Open Questions

- Bar: wrap-to-second-row vs priority-overflow vs both? Does a 2-row bar eat into the dashboard/pane height budget?
- Modals: fixed larger size vs content-driven auto-size? Min/max bounds for very narrow terminals?
- Is there a shared layout helper, or is sizing duplicated per surface?

## Out of Scope

- The scheduler behavior itself (#127).
- Supporting sub-80-col terminals as a first-class target.

## Notes

- Surfaced by PRD #127 scheduler manual-validation fixes (button-bar overflow at the 120-col e2e reference width).
- Existing band-aids to fold in / supersede: Scheduled Tasks dialog field overflow, delete-confirm `wrap_to_width`, mode-chip wrap (`layout_mode_chips`).
