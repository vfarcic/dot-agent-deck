# PRD #168: Wrap the new-pane Mode-chip row instead of clipping it

**Status**: Planning
**Priority**: Low
**Created**: 2026-06-15
**GitHub Issue**: [#168](https://github.com/vfarcic/dot-agent-deck/issues/168)
**Related**: [#144](https://github.com/vfarcic/dot-agent-deck/issues/144) (PRD that introduced the content-sized modal + button-bar wrap, PR #166), `prds/144-ui-surface-roominess.md`, regression guard `prompt/new-pane/009`, `src/ui.rs` (`render_new_pane_form`, `modal_rect`)

## Problem Statement

The new-pane / new-deck form renders its **Mode-chip row** (`Mode: [No mode] [build] [Orch: ci-deployment] [schedule] …`) on a **single row** inside a content-sized modal whose width PRD #144 clamps to **≤90% of the terminal**. When a user has **many or long modes/orchestrations** *and* a **narrow/windowed terminal**, that row exceeds the width cap and the **trailing chip is clipped** at the modal's right border. The trailing chip is the always-appended built-in **`[schedule]`** option — so the clip can hide a functional, selectable affordance.

PRD #144 deliberately accepted this clip as the consequence of the approved "content-size, clamp to 90%, clip beyond" model and deferred the proper fix to this PRD. The `[schedule]`-visibility guard `prompt/new-pane/009` passes at realistic configs, so this is an **edge case, not a shipped regression**. But clipping a selectable option is exactly the "cram instead of give room" anti-pattern PRD #144 eliminated for the button bar — the Mode-chip row should get the same treatment.

## Proposed Scope (to refine)

1. **Wrap, don't clip.** When the Mode-chip row would exceed the modal's inner width, **wrap it onto additional rows** inside the content-sized modal so the modal grows in **height** (content-driven, consistent with PRD #144's modal sizing) rather than clipping horizontally. This mirrors the button-bar wrap decision shipped in PRD #144 — uniform across all chips (`[No mode]`, workload modes, `[Orch: …]`, `[schedule]`).
2. **Keep every chip visible and usable.** The trailing built-in `[schedule]` chip must stay fully visible at narrow widths with a large mode list. The Mode cycler (`Left`/`Right`/`h`/`l`) and the selected-chip highlight must remain correct across wrapped rows.
3. **Stay panic-safe and bounded.** The modal's height is content-driven and clamped to the terminal; on very short terminals the existing PRD #144 overlay-bounds guard (the A1 fix that clamps overlay rows to the popup) keeps rendering panic-free. The fields below the Mode row (Command, etc.) must re-flow without overlap.

## Open Questions

- **Wrap vs. window/scroll the chip row?** Wrap is preferred (consistency with PRD #144, all chips visible at once); a horizontal scroll/window hides chips. Confirm wrap is the intended approach.
- **Re-flow vs. fixed positions:** does the taller Mode row push the remaining form fields down (modal grows in height) — and how does that interact with the terminal-height clamp on short terminals?
- **Very narrow extremes:** at a width where even a single chip exceeds the inner modal width, what is the graceful behavior (clip the one over-wide chip vs. allow the modal to use full width)? Sub-80-col is not a first-class target.

## Out of Scope

- Re-introducing the removed `layout_mode_chips` band-aid as-is — PRD #144 superseded it; the wrap must be part of the content-sized modal, not a separate helper.
- Other modals (Scheduled Tasks manager, confirmations) — already content-sized by PRD #144; this PRD is scoped to the new-pane/new-deck **Mode-chip row**.
- Supporting sub-80-col terminals as a first-class target (carried over from PRD #144).

## Implementation Milestones

- [ ] **Mode-chip row wraps** onto additional rows inside the content-sized new-pane/new-deck modal — no horizontal clip.
- [ ] **Trailing `[schedule]` chip stays fully visible and selectable** at narrow widths with a large mode/orchestration list.
- [ ] **Cycler + highlight + field re-flow correct** across wrapped rows (Mode cycling, selected-chip highlight, no overlap with fields below).
- [ ] **Tests** cover the overflow case — extend `prompt/new-pane/009` (and/or add a sibling test) at a width where the single-row layout would clip, proving every chip is visible after wrap; degenerate-size guard stays green.
- [ ] **Docs/changelog** updated if user-visible — changelog fragment; **no experimental flag** (visible by default, consistent with PRD #144).

## Success Criteria

- With a large mode/orchestration list on an ~80-col window, **every Mode chip (including `[schedule]`) renders fully within the modal border** on one of the wrapped rows; nothing is clipped.
- No panic at degenerate terminal sizes; fast tier + e2e green; `prompt/new-pane/009` strengthened to exercise the overflow case rather than only realistic-width configs.

## Notes

- Deferred follow-up from **PRD #144** (PR #166), accepted at the merge gate as the known consequence of the 90%-clip modal design.
- The PRD #144 A1 fix already clamps `render_new_pane_form` overlay rows to the popup bounds — the wrap should **build on** that bounding (grow height within the clamp), not fight it.
