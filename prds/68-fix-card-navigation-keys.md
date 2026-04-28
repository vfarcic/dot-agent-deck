# PRD #68: Fix Dashboard Card Navigation Keys

**Status**: Draft
**Priority**: Medium
**Created**: 2026-04-28

## Problem

The dashboard claims to support `j`/`k` and `Up`/`Down` for cycling selection through dashboard cards, both in:

- The in-app help overlay (`src/ui.rs:4416`)
- The keyboard-shortcuts documentation (`docs/keyboard-shortcuts.md`)
- The "Basic Workflow" walkthrough in `docs/getting-started.mdx`
- The key handler at `src/ui.rs:1527-1538`, which updates `ui.selected_index` on `j`/`k` / arrow keys

In practice these keys do not appear to move the selection on the dashboard. The only working way for a user to switch which card is focused is to enter command mode (`Ctrl+d`) and press `1`-`9`.

This is a documentation-vs-behavior gap: the keys are in the code path and look correct, but something else is preventing the selection from updating in normal mode.

## Workaround

Until this is fixed, the docs and in-app help should not advertise `j`/`k`/`Up`/`Down` for card navigation. Users should be told to use `Ctrl+d` then `1`-`9` to jump directly to a card.

## Solution

Investigate why the key handler does not produce a visible selection change. Hypotheses to check:

1. **Event interception** — keys may be consumed by an embedded pane or a non-Normal `UiMode` before `handle_normal_key` is called, so the handler at `ui.rs:1527-1538` never runs even though the user is "on the dashboard."
2. **Focus / mode mismatch** — `UiMode::Normal` may not actually be the current mode when a user thinks they are on the dashboard. Possible interaction with `PaneInput` mode or the dashboard pane retaining focus on the embedded terminal.
3. **Rendering** — the `selected_index` may be updating but the card-grid render is not highlighting the new selection (e.g., highlight applied to a different index, or the visible "selected" card is determined by something other than `selected_index`).

Fix so that pressing `j`/`k`/`Up`/`Down` while the dashboard has keyboard focus visibly cycles the selected card, matching what the help overlay and docs describe.

## Acceptance Criteria

- [ ] From a freshly launched dashboard with multiple session cards, pressing `j` or `Down` advances the selection to the next card; the previously-selected card is no longer highlighted.
- [ ] Pressing `k` or `Up` moves the selection back.
- [ ] Selection wraps at the end / start of the list (matches existing `(selected_index + 1) % total` logic).
- [ ] After fix, restore `j`/`k` / `Up`/`Down` rows in:
  - `docs/keyboard-shortcuts.md` Dashboard table
  - `docs/getting-started.mdx` Basic Workflow step on focusing a pane
  - `src/ui.rs` help overlay text

## Out of Scope

- Changing selection visuals (color, indicator) — separate concern.
- Adding new navigation keys beyond what is already documented.

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Fix shifts focus in unexpected ways for users who use `Ctrl+d` workflows | Add a regression test that verifies `Ctrl+d` then `1`-`9` still works after the fix. |
| Root cause is in mode/focus handling and touches multiple modes | Scope the fix narrowly to dashboard card navigation; do not refactor focus model unless required. |
