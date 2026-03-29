# PRD #12: Dashboard UI — Dir Truncation & Plain Digit Keybindings

**Status**: Complete (2026-03-29)
**Priority**: Medium
**GitHub Issue**: [#12](https://github.com/vfarcic/dot-agent-deck/issues/12)

## Problem

Two usability issues in the dashboard:

1. **Dir overflow**: When a session's working directory name is long, the `Dir` field overlaps with `Last` and `Tools` on the same row in the session card. The `padded_line()` helper uses `saturating_sub`, which silently produces zero gap instead of truncating.

2. **Deck selection ergonomics**: Selecting a deck requires Alt+N even when the dashboard pane is focused. In Normal mode, plain number keys (1-9) are unbound and could provide a faster, more discoverable shortcut.

## Solution

### 1. Truncate Dir to fit available width

In the wide layout branch of `render_session_card()`, calculate the width consumed by the right-side spans (`Last`, `Tools`) first, then truncate `cwd_display` with an ellipsis (`…`) if it would cause overlap. This follows the existing truncation pattern used for the prompt field.

### 2. Plain digit keys select decks in Normal mode

Add a match arm in `handle_normal_key()` for `KeyCode::Char('1'..='9')` that triggers the same deck-focus behavior as Alt+N. Extract the shared focus logic into a helper function to avoid duplication. Alt+N continues to work from any mode as before.

## Non-Goals (v1)

- Responsive multi-row layout that moves `Last`/`Tools` to a second row when narrow
- Rebindable or configurable keybindings
- Plain digit keys working outside Normal mode (would conflict with Filter/Rename input)

## Milestones

- [x] Dir field truncates with ellipsis when it would overlap Last/Tools in wide layout
- [x] Plain digit keys (1-9) select and focus deck in Normal mode
- [x] Shared focus logic extracted to avoid duplication between Alt+N and plain digit paths
- [x] All existing tests pass; no regressions

## Success Criteria

- Session cards render cleanly at any terminal width — Dir never overlaps Last/Tools
- Pressing `3` in the dashboard selects deck 3, same as Alt+3
- Alt+N continues to work from all modes (Filter, Help, Rename, etc.)
- `cargo test` passes
