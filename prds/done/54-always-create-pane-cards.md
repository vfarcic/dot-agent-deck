# PRD #54: Always Create Dashboard Card for Every Pane

**Status**: Complete
**Priority**: High
**Created**: 2026-04-09

## Problem

When a user creates a new pane via `Ctrl+n`, a PTY process is spawned and the user is placed into `PaneInput` mode. However, a dashboard card only appears after an agent inside that pane emits its first hook event (`SessionStart`). If the user presses `Ctrl+d` to return to the dashboard before launching an agent, the pane becomes orphaned:

1. **No card on the dashboard** — `filter_sessions()` iterates `state.sessions`, which has no entry for this pane
2. **No way to switch back** — arrow keys, number keys, and Enter all navigate through cards only
3. **No way to close it** — `Ctrl+w` guards on `session.pane_id` being `Some`, which requires an existing session

The pane's PTY process continues running invisibly until the application exits.

## Solution

Create a placeholder `SessionState` immediately when a pane is created, before any agent starts. This ensures every pane always has a corresponding dashboard card that can be selected, focused, and closed.

Placeholder sessions use:
- A "No agent" status label and a distinct border color to differentiate from active agent cards
- The border color must work well in both light and dark terminal themes
- When an agent later starts in that pane and sends a `SessionStart` event, the existing session-reuse logic in `apply_event()` transitions the placeholder into a real session seamlessly

## Design Details

### Placeholder Session

When `KeyResult::NewPane` is handled, immediately after `register_pane()`, insert a `SessionState` into `state.sessions` with:
- `session_id`: A synthetic ID derived from the pane ID (e.g., `pane-{pane_id}`)
- `pane_id`: `Some(new_id)`
- `status`: A new variant or the existing `Idle` status — rendered as **"No agent"** on the card
- `agent_type`: A neutral default (e.g., `AgentType::Unknown` or a new `AgentType::None`)
- `cwd`: Set from the directory chosen during pane creation
- `started_at` / `last_activity`: Current timestamp

### Card Appearance

- **Border color**: Use a muted/neutral color distinct from active session borders. A gray tone (e.g., `Color::DarkGray`) works in both light and dark modes.
- **Status label**: Display **"No agent"** instead of the usual status (Idle, Working, etc.)
- **Title**: Show the user-provided pane name (or a default like "Pane {id}")
- **Content**: Show `Dir: <directory>` and a hint like "Launch an agent to get started"

### Session Transition

When a `SessionStart` event arrives with a `pane_id` matching an existing placeholder session:
- The existing logic at `apply_event()` lines 115-125 already finds sessions by `pane_id` and reuses the key
- The placeholder session is naturally replaced with real agent data
- The card updates to show the agent's actual status, type, and activity

### Closing Panes

`Ctrl+w` already works for sessions with a `pane_id`. Since placeholder sessions always have `pane_id = Some(...)`, closing works without changes to the close logic.

## Milestones

- [x] Placeholder `SessionState` created at pane creation time with "No agent" status and `pane_id` set
- [x] Dashboard card rendered for placeholder sessions with distinct border color (works in light and dark modes) and "No agent" label
- [x] Seamless transition from placeholder to real session when agent starts (`SessionStart` event received)
- [x] Pane switching (arrow keys, number keys, Enter) works for placeholder cards
- [x] Pane closing (`Ctrl+w`) works for placeholder cards
- [x] Tests covering placeholder creation, card rendering, session transition, and close behavior

## Technical Notes

### Key Files
- `src/state.rs` — `SessionState` struct, `register_pane()`, `apply_event()`
- `src/ui.rs` — `KeyResult::NewPane` handler (~line 1783), `filter_sessions()` (~line 432), `render_session_card()` (~line 2854), `Ctrl+w` handler (~line 1696)

### Risks
- **Session ID collision**: The synthetic `pane-{id}` session ID must not collide with real agent session IDs (which are UUIDs). The `pane-` prefix ensures this.
- **Event routing**: `apply_event()` rejects events from unknown panes but accepts events for registered panes. Since `register_pane()` is already called, agent events will route correctly to the existing placeholder session.
