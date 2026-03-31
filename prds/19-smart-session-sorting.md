# PRD #19: Smart Session Sorting and Pinning

**Status**: Draft
**Priority**: Medium
**Created**: 2026-03-31
**GitHub Issue**: [#19](https://github.com/vfarcic/dot-agent-deck/issues/19)

## Problem Statement

With many agents running, sessions that need attention (waiting for input, errors) get buried among idle or working sessions. The card grid displays sessions in arrival order, forcing users to visually scan every card to find the ones requiring action. This wastes time and risks missing urgent situations like errors or permission prompts.

## Solution Overview

Two complementary features:

1. **Priority-based auto-sorting**: Sessions automatically sort by status priority so the most actionable items appear first. Default priority order: WaitingForInput > Error > Working > Thinking > Compacting > Idle.

2. **Manual pinning**: Users can pin specific sessions to the top of the grid regardless of status, useful for tracking high-priority agents.

## Scope

### In Scope
- Auto-sort sessions by status priority
- Pin/unpin sessions to the top of the grid (`p` key)
- Pinned sessions sort among themselves by status priority
- Visual pin indicator on pinned cards
- Toggle sorting on/off (`S` key) for users who prefer arrival order
- Persist sort preference in config

### Out of Scope
- Custom sort orders or user-defined priority rankings
- Drag-and-drop reordering
- Sort by other criteria (tool count, age, directory)
- Grouping/categories (would be a separate feature)

## Technical Approach

### Session Ordering (`src/state.rs`)
- Add `fn sorted_session_ids(&self, sort_enabled: bool) -> Vec<String>` to `AppState`
- Status priority map: `WaitingForInput=0, Error=1, Working=2, Thinking=3, Compacting=4, Idle=5`
- Pinned sessions always come first, sorted by priority within the pinned group
- Unpinned sessions follow, sorted by priority (if enabled) or arrival order (if disabled)
- Stable sort to avoid jitter when multiple sessions share the same status

### Pin State (`src/state.rs`)
- Add `pinned: bool` field to `SessionState` (default `false`)
- Add `fn toggle_pin(&mut self, session_id: &str)` method

### Sort Toggle (`src/config.rs`)
- Add `sort_by_priority: bool` config option (default `true`)
- Persisted via `dot-agent-deck config set sort_by_priority true/false`

### Dashboard UI (`src/ui.rs`)
- Replace direct session iteration with `sorted_session_ids()` call
- `p` key on selected card → toggle pin
- `S` key → toggle sort on/off with status message
- Pinned cards show a `📌` or `*` prefix on the title
- When sort is off, show "Sort: off" in the stats bar (if PRD #17 is implemented) or in the help overlay

### Index Stability
- When sort reorders cards, the selected card index must follow the previously selected session (not stay at the same grid position)
- Track selected session by ID, not by index

## Success Criteria

- Cards with "WaitingForInput" and "Error" status appear at the top automatically
- Pinned sessions stay at the top regardless of status changes
- Selected card follows its session when sort reorders the grid
- Sort can be toggled off to restore arrival order
- All existing tests pass
- No visual jitter — sort is stable

## Milestones

- [ ] Status priority ordering implemented in `sorted_session_ids()` (`src/state.rs`)
- [ ] Pin/unpin state and toggle method added to `SessionState` (`src/state.rs`)
- [ ] Dashboard uses sorted order for card rendering (`src/ui.rs`)
- [ ] `p` keybinding for pin toggle, `S` for sort toggle with visual indicators (`src/ui.rs`)
- [ ] Selected card tracks by session ID instead of grid index (`src/ui.rs`)
- [ ] Sort preference persisted in config (`src/config.rs`)
- [ ] Tests for sort ordering with mixed statuses and pins (`src/state.rs`)
- [ ] All existing tests passing

## Key Files

- `src/state.rs` — Sort logic, pin state, sorted_session_ids()
- `src/ui.rs` — Keybindings, pin indicator, sort toggle
- `src/config.rs` — Sort preference persistence
