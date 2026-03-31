# PRD #18: Permission Prompt Control from Dashboard

**Status**: Draft
**Priority**: High
**Created**: 2026-03-31
**GitHub Issue**: [#18](https://github.com/vfarcic/dot-agent-deck/issues/18)

## Problem Statement

When Claude Code needs permission to execute a tool (e.g., "Run bash command? yes/no"), the user must switch to that specific agent pane to respond. This breaks the dashboard workflow — the user loses their overview, and if multiple agents are waiting simultaneously, they must switch back and forth between panes. The dashboard already shows "WaitingForInput" status but provides no way to act on it.

## Solution Overview

Intercept permission prompts using the `PermissionRequest` hook (currently unused), display the permission details on the session card, and let users approve or deny directly from the dashboard. The hook's response mechanism (`permissionDecision: "allow"|"deny"`) sends the answer back to Claude Code without needing to switch panes.

### User Flow

1. Agent requests permission → `PermissionRequest` hook fires
2. Dashboard card shows the permission details (tool name, command/file)
3. Card highlights with a distinct "needs approval" style
4. User navigates to the card and presses `y` (allow) or `n` (deny)
5. Hook response is sent back → agent continues or gets denial feedback
6. Card returns to normal state

## Scope

### In Scope
- Register `PermissionRequest` hook in hook installation
- Parse permission request data (tool name, tool input summary)
- Display pending permission on the session card with details
- `y`/`n` keybindings on a card with a pending permission
- Hook response mechanism to send allow/deny back to Claude Code
- Visual indicator (color/icon) for cards with pending permissions
- Queue multiple permissions per session if they arrive before response

### Out of Scope
- Responding to general questions (non-permission prompts) — would require pane keystroke injection
- Editing tool input before approving (e.g., modifying a command)
- Bulk approve/deny all pending permissions at once (future enhancement)
- Auto-approve rules from the dashboard

## Technical Approach

### Hook Registration (`src/hooks_manage.rs`)
- Add `PermissionRequest` to the registered hooks list
- No matcher needed — capture all permission requests

### Event Protocol (`src/event.rs`, `src/hook.rs`)
- Add `PermissionRequest` event type
- Parse `tool_name`, `tool_input` summary, and `tool_use_id` from the hook input
- Store `tool_use_id` — needed to correlate the response

### Hook Response Mechanism (`src/daemon.rs`, `src/hook.rs`)
- The `PermissionRequest` hook is synchronous — Claude Code waits for the hook's exit code and stdout
- The hook process must stay alive until the user responds in the dashboard
- Architecture: hook script sends the request to the daemon via the Unix socket, then blocks waiting for a response on a per-request named pipe or secondary socket
- When the user presses `y`/`n`, the daemon writes the decision to the waiting hook process
- Hook process exits with code 0 and outputs `{"hookSpecificOutput": {"permissionDecision": "allow"|"deny"}}`

### Session State (`src/state.rs`)
- Add `pending_permission: Option<PendingPermission>` to `SessionState`
- `PendingPermission` struct: `tool_name`, `tool_detail`, `tool_use_id`, `requested_at`
- Applied on `PermissionRequest` event, cleared on response or timeout

### Dashboard UI (`src/ui.rs`)
- Cards with pending permissions render a permission banner showing tool name and detail
- Distinct border color (e.g., bright magenta) for permission-pending cards
- `y` key on a permission-pending card → send allow decision
- `n` key on a permission-pending card → send deny decision
- Status text changes to "Approve? y/n: [tool summary]"
- Update help overlay with `y`/`n` keybindings

### Communication Flow
```
Claude Code → PermissionRequest hook fires
    → hook script sends request to daemon via Unix socket
    → daemon updates SessionState with pending permission
    → TUI renders permission on card
    → user presses y/n
    → daemon sends decision back to waiting hook script
    → hook script outputs JSON response and exits 0
    → Claude Code receives allow/deny decision
```

## Success Criteria

- Permission prompts appear on dashboard cards within 1 second of being raised
- User can approve with `y` or deny with `n` without leaving the dashboard
- Claude Code receives the decision and continues (allow) or shows feedback (deny)
- Multiple agents can have pending permissions simultaneously
- If user switches to the pane and responds there instead, the dashboard clears the stale permission
- All existing tests pass

## Milestones

- [ ] `PermissionRequest` hook registered in hook installation (`src/hooks_manage.rs`)
- [ ] Event type and parsing for permission requests implemented (`src/event.rs`, `src/hook.rs`)
- [ ] `PendingPermission` state tracking added to `SessionState` (`src/state.rs`)
- [ ] Hook response mechanism: blocking hook script with daemon-mediated response channel (`src/daemon.rs`)
- [ ] Permission banner rendering on cards with `y`/`n` keybindings (`src/ui.rs`)
- [ ] Help overlay updated with permission approval shortcuts (`src/ui.rs`)
- [ ] Integration test: permission request → dashboard approve → agent continues
- [ ] All existing tests passing

## Key Files

- `src/hooks_manage.rs` — Hook registration
- `src/hook.rs` — PermissionRequest event parsing
- `src/event.rs` — New event type
- `src/state.rs` — PendingPermission state
- `src/daemon.rs` — Response channel for blocking hooks
- `src/ui.rs` — Permission card rendering, y/n keybindings

## Risks

- **Hook timeout**: Claude Code may have a timeout on how long it waits for a hook response. Need to verify the default (10 minutes) is sufficient and whether it's configurable.
- **Stale permissions**: If the user responds in the pane directly, the dashboard must detect this (via `PostToolUse` or `Stop` event) and clear the pending state.
- **Concurrency**: Multiple permission requests from the same session could queue up; need to handle sequentially.
