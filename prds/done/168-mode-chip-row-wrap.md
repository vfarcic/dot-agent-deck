# PRD #168: Wrap the new-pane Mode-chip row instead of clipping it

**Status**: Complete (2026-10-20)
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

- [x] **Mode-chip row wraps** onto additional rows inside the content-sized new-pane/new-deck modal — no horizontal clip.
- [x] **Trailing `[schedule]` chip stays fully visible and selectable** at narrow widths with a large mode/orchestration list.
- [x] **Cycler + highlight + field re-flow correct** across wrapped rows (Mode cycling, selected-chip highlight, no overlap with fields below).
- [x] **Tests** cover the overflow case — extend `prompt/new-pane/009` (and/or add a sibling test) at a width where the single-row layout would clip, proving every chip is visible after wrap; degenerate-size guard stays green.
- [x] **Docs/changelog** updated if user-visible — changelog fragment; **no experimental flag** (visible by default, consistent with PRD #144).

## Decisions

- **2026-10-20 — Wrap, not window/scroll.** Reading `render_new_pane_form` showed no reason against wrap: every overlay is already placed by line index and bounded by the PRD #144 A1 guard, so extra chip rows slot in the same way. All chips stay visible at once and it matches the button-bar wrap. The clip reproduced at `origin/main` (`1f2ca35b`) before the change: at 80×30 with four long orchestration names the row ended mid-`[Orch: ci-deployment-br` and `[schedule]`/`[dispatcher]` were gone (`render/layout/007`, RED first).
- **2026-10-20 — Wrap only once the modal is capped.** The modal stays content-sized: it first grows in width to hold the whole row on one line (PRD #144), and only when `modal_rect`'s 90% cap stops it does the row wrap and the modal grow in height. Because `modal_rect` clamps each axis independently, the width is settled first and the chip rows (and the Mode block's height) are computed from it. Wrapped rows start under the first chip; a chip wraps when it would end inside the 2-column right margin the content width already reserves. The layout lives inline in `render_new_pane_form`, not in a separate helper, per this PRD's Out of Scope.
- **2026-10-20 — Re-flow, then the existing clamp.** The taller Mode block pushes Name, Command, the hint and the buttons down. On a terminal too short for the result the modal is clamped to 90% of its height as before, and chip rows that fall past the popup are skipped, like every other overlay row under the A1 guard.
- **2026-10-20 — A single over-wide chip is clipped on its own row.** At a width where one chip is wider than the modal's inner width, that chip keeps a row to itself and is clipped there; the chips after it still wrap normally. Sub-80-col remains out of scope; at 80 columns an orchestration name of up to 51 characters fits whole.
- **2026-10-20 — Desktop parity (CLAUDE.md rule 22): already wraps.** `desktop/src/components/NewAgentDialog.tsx` renders its chips in `.new-agent-chips { flex-wrap: wrap }`, and with Voice on it pages them instead. A new Playwright check (`voice-off modes wrap whole in a narrow window` in `desktop/e2e/voice-paged-lists.spec.ts`) pins that, at 420 px with the crowded project's twelve orchestration chips plus No mode and the built-in ones, every chip is whole inside the Mode field and the dialog across several rows; it fails if the wrap is removed. No desktop code changed.
- **2026-10-20 — No experimental flag.** This changes an existing surface rather than adding one (CLAUDE.md rule 9 does not apply).

## Success Criteria

- With a large mode/orchestration list on an ~80-col window, **every Mode chip (including `[schedule]`) renders fully within the modal border** on one of the wrapped rows; nothing is clipped.
- No panic at degenerate terminal sizes; fast tier + e2e green; `prompt/new-pane/009` strengthened to exercise the overflow case rather than only realistic-width configs.

## Notes

- Deferred follow-up from **PRD #144** (PR #166), accepted at the merge gate as the known consequence of the 90%-clip modal design.
- The PRD #144 A1 fix already clamps `render_new_pane_form` overlay rows to the popup bounds — the wrap should **build on** that bounding (grow height within the clamp), not fight it.
