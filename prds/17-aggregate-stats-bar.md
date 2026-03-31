# PRD #17: Aggregate Stats Bar

**Status**: Draft
**Priority**: Medium
**Created**: 2026-03-31
**GitHub Issue**: [#17](https://github.com/vfarcic/dot-agent-deck/issues/17)

## Problem Statement

When running many agents simultaneously, there's no quick way to see overall status at a glance. Users must visually scan all cards and mentally tally how many agents are active, waiting for input, erroring, or idle. This becomes increasingly difficult as the number of sessions grows.

## Solution Overview

Add a persistent status bar at the bottom of the dashboard that shows real-time aggregate metrics across all sessions. The bar updates automatically as events arrive — no user interaction required.

**Example rendering:**

```
 5 active  │  12 working  │  2 waiting  │  1 error  │  347 tools  │  3 idle
```

## Scope

### In Scope
- Bottom status bar widget rendered below the card grid
- Real-time counts: active sessions, working, thinking, waiting for input, errors, idle
- Total tool call count across all sessions
- Automatic updates as session state changes
- Adaptive styling: highlight "waiting" and "error" counts with distinct colors

### Out of Scope
- Historical metrics or graphs
- Per-session stats in the bar (that's what cards are for)
- Clickable/interactive elements in the status bar
- Token usage or cost tracking (no hook data available)

## Technical Approach

### Data Aggregation (`src/state.rs`)
- Add method `fn aggregate_stats(&self) -> DashboardStats` to `AppState`
- `DashboardStats` struct with fields: `active`, `working`, `thinking`, `waiting`, `errors`, `idle`, `total_tools`, `compacting`
- Iterate over `self.sessions` and count by status, sum tool counts

### Status Bar Widget (`src/ui.rs`)
- Add `render_stats_bar()` function that draws a single-line bar at the bottom
- Reserve 1 row at the bottom of the main layout (adjust vertical split)
- Use colored spans: green for working, yellow for waiting, red for errors, dim for idle
- Separate segments with `│` dividers
- Only show non-zero counts to save space (except active total)

### Layout Adjustment (`src/ui.rs`)
- Modify the main vertical layout to include a 1-row bottom chunk for the stats bar
- Ensure adaptive card density calculation accounts for the lost row

## Success Criteria

- Status bar visible at bottom of dashboard at all times
- Counts update in real-time as agent events arrive
- "Waiting" count highlighted in yellow, "Error" in red
- Zero-count categories hidden (except total active)
- All existing tests pass
- Card grid still renders correctly with one fewer row available

## Milestones

- [ ] `DashboardStats` struct and `aggregate_stats()` method added to `AppState` (`src/state.rs`)
- [ ] `render_stats_bar()` widget implemented with colored segments (`src/ui.rs`)
- [ ] Main layout adjusted to reserve bottom row for stats bar (`src/ui.rs`)
- [ ] Adaptive card density accounts for stats bar height (`src/ui.rs`)
- [ ] Tests for `aggregate_stats()` covering mixed session states (`src/state.rs`)
- [ ] All existing tests passing

## Key Files

- `src/state.rs` — DashboardStats struct and aggregation method
- `src/ui.rs` — Stats bar rendering, layout adjustment
