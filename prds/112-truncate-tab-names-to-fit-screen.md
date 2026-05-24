# PRD #112: Truncate Tab Names When They Don't Fit on Screen

**Status**: Not started
**Priority**: Medium
**Created**: 2026-05-24

## Problem

Tab labels are constructed at `src/ui.rs:3278-3298` and rendered through `ratatui`'s `Tabs` widget at `src/ui.rs:4935-4945` using their full names — Dashboard, the mode name, or the orchestration tab name (often a branch-style string like `dot-agent-deck-prd-110-fix-session-reuse-conflicting-with-f9-cleartrue-resume-mode`).

When many tabs are open, or one or more tab names are long, the total label width exceeds the terminal width. `Tabs` truncates by clipping — later tabs disappear off the right edge of the bar and are not reachable by clicking, even though they remain reachable via `Tab` / `Shift+Tab`. Users lose situational awareness of which tabs exist.

## Solution

Apply tab-label truncation *before* handing labels to the `Tabs` widget, using an **equal-cap** strategy:

1. Compute the total rendered width of all labels (including padding and dividers).
2. If `total ≤ available_width`, render labels in full. No change in behavior.
3. Otherwise, compute `cap = floor(available_width / tab_count)` (after subtracting separator overhead).
   - Any label whose rendered width is `≤ cap` renders in full.
   - Any label longer than `cap` is truncated to `cap` characters with a trailing ellipsis (`…`).

Recompute on every frame — the inputs (terminal width, tab list, tab names) are all already available at the call site, so there is no resize-listener plumbing to add. A resize, tab open, tab close, or tab rename naturally produces correct widths on the next render.

### Design decisions (from discussion)

- **Equal cap, not proportional shrink.** When tabs overflow, every long tab is capped at the same width. Short tab names (e.g. "Dashboard", "Modes") stay in full because they're already under the cap. The alternative — proportionally shrinking the longest tabs first — saves more space but produces visually uneven widths and harder-to-predict labels. Uniform width wins on legibility.
- **Truncate, don't scroll.** No horizontal scroll, no overflow indicator (`›`). Every tab remains at least partially visible, which is the property that actually matters for click-to-switch and at-a-glance awareness.
- **Trailing ellipsis.** PRD/branch tab names share a common prefix (`dot-agent-deck-prd-…`) but diverge at the end (issue number, slug). End-truncation arguably loses the distinguishing part. We accept this in v1 because the active tab is highlighted and users typically know which PRD they're on; middle-ellipsis (`prd-110…resume-mode`) is a possible v2 if end-truncation proves confusing.
- **No minimum width floor.** With extreme tab counts on narrow terminals the cap can collapse to 2-3 characters. That's degraded but still better than tabs disappearing off-screen. We do not enforce a minimum — let it degrade gracefully.
- **Cap counts the ellipsis.** A cap of 10 means the truncated label is at most 10 cells wide *including* the `…`. Renderer must not produce a label wider than the cap.

## Acceptance Criteria

### Fit detection
- [ ] When the sum of all full tab labels (with their `" {name} "` padding and dividers) fits within the tab bar area, every tab renders with its full name unchanged.
- [ ] When the sum exceeds the available width, the equal-cap truncation applies.

### Truncation behavior
- [ ] The cap is computed as available tab-bar width divided by the number of tabs (after accounting for the per-tab padding and divider characters used in `src/ui.rs:4924-4945`).
- [ ] A tab whose rendered width is at or below the cap renders in full.
- [ ] A tab whose rendered width exceeds the cap renders as `prefix…` such that the total rendered width (including the ellipsis) is at most the cap.
- [ ] After truncation, the total rendered width of all tabs is `≤` the available width — no clipping by `Tabs`.

### Reactivity
- [ ] Resizing the terminal recomputes truncation on the next render (no manual refresh required).
- [ ] Adding a tab (new mode, new orchestration) recomputes on the next render.
- [ ] Closing a tab recomputes on the next render — if remaining tabs now fit in full, full names return.
- [ ] Renaming a tab (e.g. orchestration status change in #78) recomputes on the next render.

### Visual / interaction
- [ ] Active-tab highlight style (`src/ui.rs:4938-4943`) still applies to the truncated label.
- [ ] Truncated labels do not break the divider rendering between tabs.
- [ ] Mouse click on a truncated tab still selects it (existing tab-bar mouse routing should not depend on label content; verify it doesn't).

## Out of Scope

- Horizontal scrolling of the tab bar, overflow indicators (`›` `‹`), or a "more tabs" dropdown.
- Middle-ellipsis truncation (`prd-110…resume-mode`). End-ellipsis only in v1.
- Per-tab-type weighting (e.g. always show "Dashboard" in full at the expense of others). Uniform cap applies to all tabs.
- Wrapping the tab bar across multiple rows.
- Tooltips or hover-to-reveal-full-name (no tooltip primitive exists in the TUI).
- Changing the underlying tab data model — names remain stored in full; truncation is a render-only concern.

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Truncated PRD tab names lose their distinguishing suffix (issue numbers, slug differ at the end). | Active tab highlight + tab order generally suffice for identification. Revisit with middle-ellipsis in v2 if users report confusion. |
| Interaction with #78 tab-level status badges — badges add ~11 chars of width that must be counted. | Width measurement happens after the badge has been appended (the labels passed in at `src/ui.rs:4924` are the final strings). Equal-cap then applies uniformly. No special-case needed. |
| Unicode width — emoji, CJK, combining characters miscount as bytes instead of cells. | Use `unicode-width` (already a transitive dep via ratatui) for measurement. Don't slice by byte index — find the char boundary that fits the cap minus 1 cell (for `…`). |
| Tabs widget's internal layout may not match our pre-computed width if its padding/divider rules differ from our assumptions. | Mirror exactly the same `" {l} "` padding and `│` divider already in `src/ui.rs:4927,4944`; write a unit test that confirms the computed total equals the rendered total. |
| Cap collapses to `<3` cells on extreme inputs (50+ tabs on a narrow terminal). | Accept degraded rendering — even a 1-character label is more informative than no label. Document as known limitation. |

## Implementation Notes

- All changes localize to the tab-label rendering site at `src/ui.rs:4914-4945` and the label construction at `src/ui.rs:3278-3298`. No state-model changes.
- Add a helper, e.g. `fn fit_tab_labels(labels: &[String], available_width: u16) -> Vec<String>`, that returns truncated labels. Place it near the other UI helpers in `src/ui.rs` (or extract to a small `tab_layout.rs` if the function grows).
- The available width is `chunks[0].width` at `src/ui.rs:4921` — pass it to the helper before building the `titles: Vec<Line>` at `src/ui.rs:4924-4928`.
- Account for per-tab decoration: `" {l} "` adds 2 cells; the `│` divider between adjacent tabs adds 1 cell × (n - 1). Build the formula explicitly so the unit test can mirror it.
- Use `unicode_width::UnicodeWidthStr::width` for cell-width measurement. The crate is already in the dependency tree via `ratatui`; add it to `Cargo.toml` if not directly listed.
- Truncation function: walk the string char-by-char, sum widths, stop when the next char would push the cumulative width past `cap - 1` (reserve 1 cell for `…`), append `…`.
- Tests live in a colocated `#[cfg(test)] mod tests` block — cover: all-fit case, single overflow tab, all-overflow case, unicode label, single-tab case, zero-width edge case.

## References

- `src/ui.rs:3278-3298` — `tab_bar_labels` construction
- `src/ui.rs:4914-4945` — tab bar layout and `Tabs` widget rendering
- `src/ui.rs:4924-4928` — current `Line` construction with `" {l} "` padding
- `src/ui.rs:4944` — divider character `│`
- `prds/78-tab-level-status-indicators.md` — related work that also affects tab label width
