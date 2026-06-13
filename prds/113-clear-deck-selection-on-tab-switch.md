# PRD #113: Clear deck selection highlight when switching tabs

**Status**: Implemented
**Priority**: Medium
**Created**: 2026-05-24
**GitHub Issue**: [#113](https://github.com/vfarcic/dot-agent-deck/issues/113)

## Problem Statement

On the Dashboard tab, the blue background highlight on a deck card persists across tab switches. Visually it looks like the card is "selected" and ready to act on, but functionally nothing happens until the user presses Enter or `1`-`9`. The visual state and the functional state are out of sync — the blue highlight is stale.

Root cause: `ui.selected_index: usize` (src/ui.rs:499) is a single global index. The renderer at src/ui.rs:5182 paints `selected_bg` purely from this index, and tab-switch handlers (src/ui.rs:4024, 4038) never clear it. Dashboard `j`/`k` only mutates the index without calling `pane.focus_pane`, so the highlight can also drift away from the actually-focused embedded pane.

Mode tab side panes don't have this problem: their `focused_side_pane_index: Option<usize>` is per-tab and starts at `None`, and Mode tab `j`/`k` calls `pane.focus_pane` immediately, keeping visual and functional state in sync.

## Solution Overview

Introduce a notion of "active selection" on the Dashboard. The blue highlight is painted only when a selection is active. Tab-switching deactivates it; explicit user input reactivates it.

Behavior:

- **After tab switch away from Dashboard and back**: no card has the blue background (selection inactive).
- **`1`-`9`**: selects (and focuses) that numbered card — unchanged from today, also activates the highlight.
- **Enter**: when selection is inactive, **restores the previously-selected deck** (re-selects, highlights, and focuses whichever card was selected before the tab switch). When active, focuses the highlighted card (current behavior). See Design Revision below.
- **`j`**: jumps to the first card and activates the highlight.
- **`k`**: jumps to the last card and activates the highlight.
- Once active, `j`/`k` navigate normally and the highlight persists until the next tab-switch away.

Mode tab side-pane behavior is unchanged.

### Design Revision — 2026-06-13 (post-implementation manual-test feedback)

After the initial implementation, manual testing surfaced two behaviors the user expected that the original PRD did not specify. Both are now in scope for this PRD/PR:

1. **Symmetric clearing across decks.** Switching tabs must clear the active selection highlight on **both** the Dashboard **and** the Orchestration deck, so the two behave consistently. (Originally the Orchestration tab's role selection was declared out of scope; it is now in scope for the clear-on-tab-switch behavior.)
2. **Enter restores the previously-selected deck.** When a deck has no active highlight (e.g. just after returning from another tab), pressing Enter must re-select, highlight, and focus **whichever card/role was selected before the tab switch** — not jump to the first card. This requires *remembering* the prior selection rather than discarding it (the initial implementation cleared it). This applies to both the Dashboard and the Orchestration deck.

`j`/`k`/`1`-`9` behavior is unchanged by this revision (they still jump-to-first/last/N and activate). The focus-transition reactivation guard added during implementation (highlight reactivates only on a genuine focus change, not on focus restored by the tab switch) remains in force; remembering the prior selection must not re-introduce the auto-reactivation it prevents.

## Scope

### In Scope

- Change `UiState.selected_index: usize` to `Option<usize>` (or add a sibling `selection_active: bool`) in `src/ui.rs`.
- Clear the active flag in the Tab / BackTab / Left / Right / `h` / `l` handlers (src/ui.rs:4024, 4038).
- Update the Dashboard renderer (src/ui.rs:5182) so `selected_bg` is only painted when active.
- Update Dashboard key handlers (`j`, `k`, `1`-`9`, Enter) per the rules above (src/ui.rs:1987–1996, 2014, src/ui.rs:3760-ish number-key path).
- Update the existing focused-pane sync (src/ui.rs:3231–3241) so the highlight activates when a dashboard session's pane becomes focused via other means.
- Unit tests in `tests/` covering: highlight cleared on tab switch, Enter → first card when inactive, `j`/`k` jump-to-first/last and activate, 1-9 always works.

### Out of Scope

- Mode tab side-pane focus model — already correct.
- ~~Orchestration tab role selection — already separate state.~~ **Now in scope** (Design Revision 2026-06-13): the clear-on-tab-switch and Enter-restores-previous behaviors apply to the Orchestration deck too.
- Any change to PaneInput-mode behavior or `focus_deck` semantics.
- Mouse selection behavior.

## Key Files

| File | Change |
|------|--------|
| `src/ui.rs` | `UiState` field type, tab-switch handlers, Dashboard renderer, Dashboard key handlers |
| `tests/` | Selection-state tests (likely a new `tests/dashboard_selection.rs`) |

## Milestones

- [x] **M1 — Selection becomes optional**: `UiState` carries an "inactive" state for the dashboard selection; renderer paints blue bg only when active. Default state on startup remains "active at index 0" (no behavior change on first launch).
- [x] **M2 — Tab switch clears highlight**: Tab / BackTab / Left / Right / `h` / `l` deactivate the selection. Switching back to the Dashboard shows no blue card until the user acts.
- [x] **M3 — Key handlers updated**: `j` → first + activate; `k` → last + activate; Enter → first when inactive, otherwise focuses highlighted card; `1`-`9` unchanged but explicitly activates the highlight.
- [x] **M4 — Focused-pane sync still works**: When the embedded controller's focused pane corresponds to a dashboard session, the highlight reactivates on that card.
- [x] **M5 — Tests pass**: Unit tests cover the inactive-on-tab-switch, jump-to-first/last, Enter-fallback, and 1-9 paths. Existing dashboard tests still pass.

## Success Criteria

1. Switch from Dashboard to any other tab and back — no card has a blue background.
2. Press `j` after returning to Dashboard — highlight appears on card 1.
3. Press `k` after returning to Dashboard — highlight appears on the last card.
4. Press Enter after returning to Dashboard with no highlight — card 1 is focused.
5. Press `3` after returning to Dashboard with no highlight — card 3 is focused.
6. While the highlight is active, `j`/`k` continue to cycle through cards as today.
7. No regression in Mode tab side-pane focus behavior.

## Notes

- The cyan border on embedded panes (which marks the actually-focused pane in the EmbeddedPaneController) is unaffected.
- The startup behavior (highlight on card 0) is preserved so first-launch UX is unchanged.
- This addresses the visual/functional mismatch without introducing the PaneInput-mode side effect that would come from making `j`/`k` immediately call `focus_pane`.
