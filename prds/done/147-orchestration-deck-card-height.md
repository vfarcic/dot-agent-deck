# PRD #147: Right-size orchestration deck card height

**Status**: Complete
**Completed**: 2026-06-12
**Priority**: Medium
**Created**: 2026-06-12
**GitHub Issue**: [#147](https://github.com/vfarcic/dot-agent-deck/issues/147)
**Related**: [#144](https://github.com/vfarcic/dot-agent-deck/issues/144) (UI-surface roominess), `src/ui.rs` (`CardDensity::card_height`, `choose_density`, `render_session_card`), `tests/render_dashboard.rs` (L1 card snapshots)

## Problem Statement

On the **orchestration tab**, deck cards are stacked in a single column (the card area is ~34% of the terminal width, which `grid_columns` maps to 1 column). Each card is given a **fixed height** chosen by `choose_density`, which steps Spacious → Normal → Compact and stops at Compact.

The fixed heights in `CardDensity::card_height` (`src/ui.rs`) are **larger than the lines `render_session_card` actually emits**:

| Tier | Max content lines (+2 border) | Reserved `card_height` (wide) | Wasted rows |
|------|-------------------------------|-------------------------------|-------------|
| Compact  | 3 + 2 = **5**  | 7  | **2** |
| Normal   | 6 + 2 = **8**  | 9  | 1 |
| Spacious | 8 + 2 = **10** | 11 | 1 |

(Narrow layout adds one inline stats line per tier; the same 2 / 1 / 1 waste applies.)

Two user-visible symptoms follow from the Compact waste:

1. **Empty rows.** Each Compact deck shows Dir + last prompt + last execution (3 lines) inside a 7-row card → ~2 blank rows per card.
2. **Decks overflow.** With 7 decks at 7 rows each that is `7 × 7 = 49` rows; in a typical ~48-row card area only `48 / 7 = 6` cards fit, so the 7th requires scrolling. The user expected card heights to shrink so all fit — but density never goes below Compact, so it scrolls instead.

Common thread: reserved card height is decoupled from rendered content, so cards reserve space they never use.

## Solution

Derive `CardDensity::card_height` from the **exact lines `render_session_card` pushes**, instead of hardcoded magic numbers:

```rust
fn card_height(self, wide: bool) -> u16 {
    // Mirror the lines render_session_card emits:
    //   Dir (1) + prompts + [narrow: inline stats line] + [non-compact: blank separator] + tools
    let prompts    = self.max_prompts() as u16;
    let tools      = self.max_tools() as u16;
    let stats_line = if wide { 0 } else { 1 };
    let separator  = if matches!(self, CardDensity::Compact) { 0 } else { 1 };
    (1 + prompts + stats_line + separator + tools) + 2 // +2 top/bottom border
}
```

Resulting heights — wide: Compact **5**, Normal **8**, Spacious **10**; narrow: **6 / 9 / 11**.

Effects:

- **No empty rows** — reserved height equals rendered content on every tier.
- **All 7 decks fit** — `7 × 5 = 35 ≤ ~48`, so the single-column orchestration grid shows all 7 with no scrolling. Scrolling now only engages much later (≈10+ decks on a short terminal), which is genuinely unavoidable.
- **Self-consistent** — height is computed from `max_prompts` / `max_tools` and the same `wide` / non-compact branches the renderer uses, so it cannot drift out of sync when content changes again.

### Design decisions (from discussion)

- **Fix the root, not just Compact.** Tighten all three tiers to content rather than only shaving Compact's 2 rows. The waste is the same bug at smaller magnitude in Normal/Spacious, and deriving height from content fixes all three uniformly and prevents future drift.
- **Keep the density ladder + scrolling as the overflow path.** This PRD does not add a sub-Compact tier or per-card dynamic heights. Tightening Compact to its content height already makes the reported 7-deck case fit; scrolling remains the (now much-later) fallback for genuinely too-many decks.
- **The intentional in-card blank separator stays.** The `if density != Compact` blank line before the tool list is content and remains counted in the height; only trailing/unused rows are removed.

## Acceptance Criteria

### Height matches content
- [x] `CardDensity::card_height` returns Compact=5, Normal=8, Spacious=10 (wide) and 6 / 9 / 11 (narrow).
- [x] A rendered card has no trailing blank rows below its content at any tier (verified by L1 buffer inspection).

### Orchestration fit
- [x] With 7 decks in the single-column orchestration card area at a typical terminal height, all 7 cards render without scrolling.
- [x] Selecting/navigating past the last visible card still scrolls correctly when the deck count genuinely exceeds the now-tighter capacity.

### No regressions
- [x] Dashboard tab card grid (multi-column) renders correctly at the new heights.
- [x] `choose_density` still selects the largest tier that fits; its tier-boundary unit tests are updated for the new heights and pass.
- [x] L1 `insta` snapshots in `tests/render_dashboard.rs` are re-accepted at the new card heights.

## Milestones

- [x] **M1 — Content-derived height.** Replace `CardDensity::card_height` magic numbers with the content-derived computation; confirm the six (tier × wide/narrow) values.
- [x] **M2 — Tests updated.** Update `test_choose_density_wide` / `test_choose_density_narrow` boundary assertions; add/adjust an L1 test asserting no trailing blank rows and that 7 decks fit; re-accept affected `insta` snapshots.
- [x] **M3 — Verified in the running TUI.** Launch the deck with 7 orchestration decks and confirm all fit with no empty rows (per `run-dot-agent-deck`).

## Out of Scope

- A sub-Compact density tier or per-card dynamically-distributed heights (scrolling remains the overflow path).
- Changing the orchestration card-area width split (`ORCHESTRATION_LEFT_PERCENT`) or `grid_columns` thresholds.
- Broader UI-roominess work tracked in #144 (button-bar wrapping, modal sizing).

## Notes

- Root cause: `card_height` constants were set independently of the lines `render_session_card` emits, so each tier reserved 1–2 rows it never fills.
- Surfaced by a user report: "with 7 decks I see only 6 without scrolling, and each deck has 2 empty rows."
