# PRD #2: Pane Control via Zellij Integration

**Status**: Draft
**Priority**: High
**GitHub Issue**: [#2](https://github.com/vfarcic/dot-agent-deck/issues/2)
**Depends on**: PRD #1 (Agent Status Dashboard)

## Problem

The Agent Status Dashboard (PRD #1) provides visibility into agent sessions, but users still need to manually switch between terminal panes to interact with agents. The dashboard should allow users to control panes directly — switching to an agent that needs input, creating new agent sessions, and managing the terminal layout — all from within the dashboard.

## Solution

Integrate with zellij's CLI to control terminal panes from the dashboard. The dashboard detects which multiplexer is running and issues pane management commands. Zellij is the first supported multiplexer, with a trait abstraction enabling future support for tmux and others.

## Architecture

### Multiplexer Abstraction

```rust
trait PaneController {
    fn focus_pane(&self, pane_id: &str) -> Result<()>;
    fn create_pane(&self, command: Option<&str>) -> Result<String>;  // returns pane_id
    fn close_pane(&self, pane_id: &str) -> Result<()>;
    fn list_panes(&self) -> Result<Vec<PaneInfo>>;
    fn resize_pane(&self, pane_id: &str, direction: Direction, amount: u16) -> Result<()>;
}
```

### Zellij Implementation (First)

Uses zellij CLI commands:

| Dashboard Action | Zellij Command |
|---|---|
| Switch to agent | `zellij action focus-terminal-pane --pane-id X` |
| New agent pane | `zellij action new-pane -- claude` |
| Close pane | `zellij action close-pane --pane-id X` |
| List panes | `zellij action list-panes` |

### Pane-Session Mapping

The dashboard needs to map agent sessions (identified by `session_id` from hook events) to multiplexer pane IDs. The preferred approach is environment-based: Claude Code hooks include the multiplexer pane ID (e.g., `ZELLIJ_PANE_ID` env var), and the adapter sends it alongside session events.

```bash
PANE_ID=$(printenv ZELLIJ_PANE_ID 2>/dev/null || echo "unknown")
```

### Dashboard Keybindings

#### Navigation (moved from PRD #1)
- `Up`/`Down` arrow keys or `j`/`k`: navigate between agent cards
- `/`: filter/search sessions by name, directory, or status
- `r`: rename/label the selected session (set a friendly display name)

#### Pane Control
- `Enter`: focus the selected agent's terminal pane in zellij
- `n`: create a new zellij pane and optionally start `claude` in it
- `d`: close/delete the selected agent's pane (with confirmation)
- `f`: toggle the selected pane to fullscreen
- `s`: split and create a new pane (horizontal/vertical)
- `Esc` or `Tab`: return focus to the dashboard pane

#### General
- `?`: show/hide keybindings help overlay
- `q`: quit dashboard

### Return-to-Dashboard

After switching to an agent pane, the user needs a way to return to the dashboard:

1. **Zellij keybinding**: configure a zellij key combo that focuses the dashboard pane
2. **Claude Code hook**: when an agent goes idle or completes, auto-focus dashboard
3. **Manual**: user switches back via zellij's normal pane navigation

## Technical Details

- **IPC to zellij**: Shell out to `zellij` CLI via `std::process::Command`
- **Pane detection**: Parse `zellij action list-panes` output or use zellij pipe protocol
- **Multiplexer detection**: Check `$ZELLIJ` env var (set when running inside zellij). Future: check `$TMUX` for tmux support.

## Non-Goals (v1)

- tmux support (future — implement `PaneController` trait for tmux)
- Custom layouts / saved workspace configurations
- Auto-spawning agents on startup
- Cross-session pane management (only current zellij session)

## Milestones

- [ ] Keyboard navigation: Up/Down/j/k card selection, `/` filter, `?` help overlay
- [ ] Session rename: `r` to set a friendly display name for a session
- [ ] Multiplexer detection and `PaneController` trait definition
- [ ] Pane-session mapping: link session_ids to zellij pane IDs via adapter
- [ ] Focus switching: `Enter` to switch to agent pane, return-to-dashboard mechanism
- [ ] Pane creation: `n` to create new pane with `claude` (or custom command)
- [ ] Pane management: close, resize, fullscreen from dashboard

## Future: tmux Support

The `PaneController` trait enables adding tmux support later:

| Dashboard Action | tmux Command |
|---|---|
| Switch to agent | `tmux select-pane -t X` |
| New agent pane | `tmux split-window -- claude` |
| Close pane | `tmux kill-pane -t X` |
| List panes | `tmux list-panes -F '#{pane_id} #{pane_current_path}'` |

## Success Criteria

- User can switch to any agent pane from the dashboard with a single keypress
- User can create new agent sessions from the dashboard
- Pane-session mapping is reliable (no stale/wrong mappings)
- Works within any zellij session (no special zellij config required beyond keybindings)
- Adding tmux support later requires only implementing the trait, no dashboard changes
