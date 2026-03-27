# PRD #8: Terminal Bell Notifications

**Status**: Complete
**Priority**: Medium
**GitHub Issue**: [#8](https://github.com/vfarcic/dot-agent-deck/issues/8)

## Problem

Users monitoring multiple Claude Code agent sessions in the dashboard miss important state changes when they're not actively looking at the TUI. An agent may finish its work, hit a permission prompt requiring input, or encounter an error — and the user has no way of knowing without visually scanning the dashboard.

## Solution

Send terminal bell character (`\x07`) when agent sessions transition to attention-requiring states. The bell is a universal, zero-dependency mechanism supported by all terminal emulators. Most terminals can be configured to bounce the dock icon (macOS), flash the taskbar (Linux/Windows), or play a sound.

### Configuration

Global configuration via `config.toml` with per-state toggles:

```toml
[bell]
enabled = true
on_waiting_for_input = true
on_idle = false
on_error = true
```

### Triggering States

| State | Default | Rationale |
|-------|---------|-----------|
| WaitingForInput | On | Agent needs user action (permission prompt) — most actionable |
| Error | On | Something went wrong — likely needs attention |
| Idle | Off | Agent finished — frequent, can be noisy |

### Detection Logic

- Track last-seen `SessionStatus` per session in the TUI layer
- On each poll cycle (~100ms), compare current vs previous status
- Bell only on actual state *transitions* (not repeated same state)
- Coalesce multiple simultaneous transitions into a single bell
- Clean up tracking for removed sessions

### Design Decisions

- **TUI layer, not state layer**: Bell is a terminal/UI concern, so detection and firing happen in `src/ui.rs`, not `src/state.rs`
- **Pure function for testability**: Core logic extracted into `compute_bell_needed()` that returns a bool + updated tracking map, with no side effects
- **Single bell per tick**: Even if 3 sessions transition simultaneously, emit one bell. Multiple rapid bells are annoying and most terminals coalesce them anyway
- **Direct stdout write**: `\x07` written directly to stdout, bypassing ratatui's buffer (which is for visual content only). Works in alternate screen mode on all major terminals

## Success Criteria

- Terminal bell fires when an agent transitions to WaitingForInput or Error
- Bell does NOT fire repeatedly for the same state
- Bell can be disabled via `config.toml`
- Per-state toggles work correctly (e.g., `on_idle = false` suppresses idle bells)
- All existing tests continue to pass
- New unit tests cover bell detection logic

## Milestones

- [x] `BellConfig` struct added to `src/config.rs` with per-state toggles and sensible defaults
- [x] Bell tracking state (`last_bell_status`) added to `UiState` in `src/ui.rs`
- [x] Pure `compute_bell_needed()` function implemented with transition detection logic
- [x] Bell integrated into TUI main loop (fires `\x07` on detected transitions)
- [x] Unit tests for bell detection logic (transitions, duplicates, config toggles, cleanup)
- [ ] Manual verification: bell fires in terminal on agent state changes

## Files to Modify

- `src/config.rs` — Add `BellConfig` struct, wire into `DashboardConfig`
- `src/ui.rs` — Add tracking state, detection logic, main loop integration, tests

## Risks & Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| Terminal doesn't support bell | No notification | Graceful degradation — no crash, just silent. Document terminal config tips |
| Bell too frequent/annoying | User disables entirely | Idle off by default; per-state toggles let users tune |
| Missed transitions between polls | Rare missed notification | WaitingForInput and Error persist until user action, so unlikely to be missed in 100ms window |
