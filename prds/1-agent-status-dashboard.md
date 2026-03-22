# PRD #1: Agent Status Dashboard

**Status**: In Progress
**Priority**: High
**GitHub Issue**: [#1](https://github.com/vfarcic/dot-agent-deck/issues/1)
**Related**: PRD #2 (Pane Control via Zellij Integration)

## Problem

Developers running multiple AI coding agent sessions (Claude Code, and others in the future) have no way to see at a glance what each agent is doing, which ones need input, and which are idle or stuck. Existing tools like cmux and claude-squad show minimal per-session metadata — not enough to make informed decisions about which session to focus on.

## Solution

A standalone ratatui terminal application that renders a rich, real-time dashboard showing the status of all active agent sessions. The dashboard receives structured events from agents via a Unix socket and displays per-agent cards with current activity, progress, and recent output.

## Architecture

### Event Protocol (Agent-Agnostic)

The dashboard consumes events through a generic event protocol over a Unix socket. This protocol is agent-agnostic — each agent type has its own adapter that translates agent-specific events into the common schema.

```
┌──────────────────┐
│ Claude Code hooks │──┐
├──────────────────┤  │   ┌──────────────────┐
│ Future: Codex    │──┼──►│ Event protocol   │──► Dashboard
├──────────────────┤  │   │ (Unix socket)    │
│ Future: Gemini   │──┘   │ {session, tool,  │
│                  │      │  status, output} │
└──────────────────┘      └──────────────────┘
```

### Event Schema (v1)

```json
{
  "session_id": "string",
  "agent_type": "claude_code",
  "event_type": "tool_start | tool_end | waiting_for_input | idle | error | session_start | session_end",
  "tool_name": "Bash | Edit | Read | ...",
  "tool_detail": "npm test | src/foo.ts | ...",
  "cwd": "/path/to/project",
  "timestamp": "ISO-8601",
  "metadata": {}
}
```

### Claude Code Adapter (First Implementation)

Uses Claude Code's hooks system to emit events. Hooks are configured in `~/.claude/settings.json` and fire automatically for every `claude` session:

- `SessionStart` → session_start event
- `PreToolUse` → tool_start event (with tool name and parameters)
- `PostToolUse` → tool_end event (with duration)
- `Notification` (permission_prompt) → waiting_for_input event
- `Stop` → idle event
- `SessionEnd` → session_end event

Hook scripts translate Claude Code's JSON input into the common event schema and send to the Unix socket.

### Dashboard UI

Per-agent cards showing:
- **Session identity**: working directory, branch name, agent type
- **Current activity**: which tool is running and on what (e.g., "Bash: npm test", "Edit: src/routes.ts")
- **Status indicator**: working (yellow), waiting for input (red/flashing), idle (green), error (red)
- **Progress**: tasks completed / total (when available from agent)
- **Recent output**: last 3-5 lines of relevant output
- **Time since last activity**

### Keyboard Navigation

- `j`/`k` or arrow keys: navigate between agent cards
- `Enter`: switch to selected agent's pane (requires PRD #2 — Pane Control)
- `n`: create new agent pane (requires PRD #2)
- `q`: quit dashboard
- `/`: filter/search sessions

## Technical Stack

- **Language**: Rust
- **TUI framework**: ratatui + crossterm
- **IPC**: Unix domain socket (listening for events)
- **Serialization**: serde + serde_json

## Non-Goals (v1)

- Pane management and switching (see PRD #2 — Pane Control)
- Built-in terminal emulation
- Web UI
- Cross-machine monitoring

## Milestones

- [x] Event daemon: Unix socket listener, event schema, in-memory state per session
- [ ] Claude Code adapter: Hook scripts that translate Claude Code events to common protocol
- [x] Basic dashboard rendering: Agent cards with status, current tool, working directory
- [ ] Rich dashboard: Progress bars, recent output lines, time tracking, colored status indicators
- [ ] Keyboard navigation: j/k navigation, filtering, card selection

## Relationship to PRD #2

This dashboard works standalone as a **read-only status viewer**. PRD #2 (Pane Control) adds the ability to switch to agent panes, create new panes, and manage layouts from within the dashboard. The `Enter` and `n` keybindings will be wired up when PRD #2 is implemented.

## Success Criteria

- Dashboard updates in real-time as Claude Code sessions emit events
- User can see at a glance which agents need attention
- Adding support for a new agent type requires only writing a new adapter (no dashboard changes)
- Dashboard renders correctly in standard terminal sizes (80x24 and above)
