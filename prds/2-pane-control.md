# PRD #2: Pane Control via Zellij Integration

**Status**: Complete
**Priority**: High
**GitHub Issue**: [#2](https://github.com/vfarcic/dot-agent-deck/issues/2)
**Depends on**: PRD #1 (Agent Status Dashboard)

## Problem

The Agent Status Dashboard (PRD #1) provides visibility into agent sessions, but users still need to manually switch between terminal panes to interact with agents. The dashboard should allow users to control panes directly — switching to an agent that needs input, creating new agent sessions, and managing the terminal layout — all from within the dashboard.

## Solution

Integrate with zellij's CLI to control terminal panes from the dashboard. The dashboard detects which multiplexer is running and issues pane management commands. Zellij is the first supported multiplexer, with a trait abstraction enabling future support for tmux and others.

### Layout Strategy: Persistent Dashboard + Stacked Agent Panes

The Zellij layout uses a two-column split:
- **Left column (1/3 width)**: Dashboard pane — always visible, acts as the control plane
- **Right column (2/3 width)**: Stacked agent panes — only the active agent is expanded, others show as collapsed title bars

When the user presses `n` to create a new agent or `Enter` to focus an existing one, the right column switches to show that agent. The dashboard remains pinned on the left, providing continuous visibility into all sessions.

This design means users never "leave" the dashboard — they always see both the dashboard and whichever agent they're currently interacting with. Inactive agents are represented as thin title bars in the stacked area, serving as a visual inventory of running sessions.

**Decision date**: 2026-03-23
**Rationale**: A persistent dashboard + focused agent provides a better mental model than full-screen switching. Users can monitor agent status while interacting with an agent, and switching between agents is a single keypress from the dashboard without losing context.

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

### Zellij Layout (Two-Column Stacked)

```kdl
layout {
    default_tab_template {
        children
    }
    tab {
        pane size="33%" borderless=true command="/tmp/dot-agent-deck-shell.sh"
        pane stacked=true size="67%" {
            pane borderless=true  // placeholder or first agent
        }
    }
}
```

The right column uses `stacked=true` so new panes created there stack vertically with only one expanded at a time. The dashboard pane on the left has a fixed 33% width.

### Pane-Session Mapping

The dashboard needs to map agent sessions (identified by `session_id` from hook events) to multiplexer pane IDs. The preferred approach is environment-based: Claude Code hooks include the multiplexer pane ID (e.g., `ZELLIJ_PANE_ID` env var), and the adapter sends it alongside session events.

```bash
PANE_ID=$(printenv ZELLIJ_PANE_ID 2>/dev/null || echo "unknown")
```

### Dashboard Keybindings

#### Navigation (moved from PRD #1)
- `Up`/`Down` arrow keys or `j`/`k`: navigate vertically between agent cards
- `Left`/`Right` arrow keys or `h`/`l`: navigate horizontally across grid columns
- `/`: filter/search sessions by name, directory, or status
- `r`: rename/label the selected session (set a friendly display name)
- `Esc`: clear active filter

#### Pane Control
- `Enter`: expand and focus the selected agent's pane in the stacked area (2/3 right column)
- `n`: open directory picker, then create a new pane in the selected directory
- `d`: close the selected agent's pane

#### Zellij Shortcuts (work from any pane)
- `Alt+h` / `Alt+d`: go to dashboard
- `Alt+j` / `Alt+k`: navigate between stacked panes
- `Alt+l`: go to agent pane area
- `Alt+w`: close current pane
- `Alt+n`: create new pane (via Zellij)
- `Alt+q`: quit all (exit Zellij session)

#### General
- `?`: show/hide keybindings help overlay
- `q`: quit dashboard

### Return-to-Dashboard

`Alt+h` or `Alt+d` moves focus to the dashboard pane from any agent pane. This uses Zellij's `MoveFocus "Left"` action. Note: `pane_frames` must be `true` for focus navigation to work correctly with stacked panes (Zellij bug #4656).

## Technical Details

- **IPC to zellij**: Shell out to `zellij` CLI via `std::process::Command`
- **Pane detection**: Parse `zellij action list-panes` output or use zellij pipe protocol
- **Multiplexer detection**: Check `$ZELLIJ` env var (set when running inside zellij). Future: check `$TMUX` for tmux support.

## Non-Goals (v1)

- tmux support (future — implement `PaneController` trait for tmux)
- Custom layouts / saved workspace configurations
- Auto-spawning agents on startup
- Cross-session pane management (only current zellij session)
- Fullscreen toggle (`f`) and split (`s`) — removed in favor of the stacked layout which handles visibility automatically
- Resize from dashboard — the stacked layout manages pane sizes

## Milestones

- [x] Keyboard navigation: arrow keys + h/j/k/l grid selection, `/` filter, `?` help overlay, status bar
- [x] Session rename: `r` to set a friendly display name for a session
- [x] Multiplexer detection and `PaneController` trait definition
- [x] Pane-session mapping: link session_ids to zellij pane IDs via adapter
- [x] Focus switching: `Enter` to switch to agent pane, return-to-dashboard mechanism
- [x] Two-column stacked layout: dashboard (1/3) + stacked agent panes (2/3) via swap layouts
- [x] Pane creation: `n` opens directory picker, creates pane in selected dir
- [x] Focus switching update: `Enter` expands the selected agent in the stacked area
- [x] Pane management: `d` closes selected pane, `Alt+w` closes current pane from anywhere

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
