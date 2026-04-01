# PRD #30: OpenCode Agent Support

**Status**: Draft
**Priority**: Medium
**Created**: 2026-04-01
**GitHub Issue**: [#30](https://github.com/vfarcic/dot-agent-deck/issues/30)
**Related**: [#20 Multi-Agent Support](https://github.com/vfarcic/dot-agent-deck/issues/20)

## Problem Statement

The dashboard only monitors Claude Code sessions. Developers using OpenCode (opencode.ai) as their AI coding assistant cannot see those sessions in the same unified dashboard. This forces them to context-switch between terminals to track what each agent is doing.

## Solution Overview

Add OpenCode as the first non-Claude agent monitored by the dashboard. OpenCode has a native JS/TS plugin system with 20+ event types — enabling the same push-based adapter pattern used for Claude Code. A thin JS plugin installed in `~/.opencode/plugin/dot-agent-deck/` will shell out to `dot-agent-deck hook --agent opencode` on each event, keeping all event-mapping logic in Rust.

### Architecture

```
Claude Code  →  settings.json hooks   →  dot-agent-deck hook                   →  daemon
OpenCode     →  JS plugin hooks       →  dot-agent-deck hook --agent opencode   →  daemon
```

The daemon, state machine, and UI are already agent-agnostic — they operate on `AgentEvent` structs regardless of source. The changes are confined to: event enum, CLI flag, hook parsing, plugin installer, and display formatting.

## Scope

### In Scope
- `OpenCode` variant in `AgentType` enum
- `--agent` CLI flag on `hook` subcommand to dispatch by agent type
- OpenCode event parsing and mapping to `EventType`
- JS plugin installer/uninstaller via `dot-agent-deck hooks install --agent opencode`
- OpenCode sessions displayed in dashboard with "OpenCode" label
- `--agent` flag on `hooks install` / `hooks uninstall` subcommands

### Out of Scope
- Full multi-agent framework from PRD #20 (generic wrapper, agent registry with colors, type filtering)
- OpenCode-specific UI panels or detail views
- Managing OpenCode installation or configuration
- Permission control for OpenCode sessions (PRD #18 is Claude-specific)
- Sending prompts to OpenCode (PRD #24 scope)

## Technical Approach

### 1. Extend `AgentType` enum (`src/event.rs`)

Add `OpenCode` variant:

```rust
pub enum AgentType {
    ClaudeCode,
    OpenCode,
}
```

Serializes as `"open_code"` via `serde(rename_all = "snake_case")`. Update existing tests.

### 2. Add `--agent` CLI flag (`src/main.rs`)

Add an `--agent` option to the `Hook` subcommand and `HooksAction` subcommands:

```rust
Hook {
    #[arg(long, default_value = "claude-code")]
    agent: String,
},
```

```rust
HooksAction::Install {
    #[arg(long, default_value = "claude-code")]
    agent: String,
},
HooksAction::Uninstall {
    #[arg(long, default_value = "claude-code")]
    agent: String,
},
```

Default to `"claude-code"` so existing behavior is unchanged.

### 3. OpenCode event parsing (`src/hook.rs`)

Add a new input struct for OpenCode plugin events:

```rust
struct OpenCodeHookInput {
    session_id: String,
    event: String,              // e.g. "tool.execute.before"
    tool_name: Option<String>,
    tool_input: Option<Value>,
    status: Option<String>,     // for session.status.updated
    cwd: Option<String>,
    prompt: Option<String>,
    #[serde(flatten)]
    _extra: HashMap<String, Value>,
}
```

Event mapping:

| OpenCode Event             | EventType          |
|---------------------------|--------------------|
| `session.created`         | `SessionStart`     |
| `session.deleted`         | `SessionEnd`       |
| `session.idle`            | `Idle`             |
| `session.error`           | `Error`            |
| `session.status.updated`  | `Thinking` (default), `Idle` if status="idle", `Error` if status="error" |
| `tool.execute.before`     | `ToolStart`        |
| `tool.execute.after`      | `ToolEnd`          |
| `permission.asked`        | `WaitingForInput`  |

Refactor `handle_hook()` to accept an `agent` parameter and dispatch to the appropriate parser.

### 4. OpenCode plugin installer (`src/hooks_manage.rs`)

Add `install_opencode()` and `uninstall_opencode()` functions.

**Install** writes a JS plugin to `~/.opencode/plugin/dot-agent-deck/index.js`:

```javascript
module.exports = {
  name: "dot-agent-deck",
  hooks: {
    "session.created": (ctx) => sendEvent("session.created", ctx),
    "session.deleted": (ctx) => sendEvent("session.deleted", ctx),
    "session.idle": (ctx) => sendEvent("session.idle", ctx),
    "session.error": (ctx) => sendEvent("session.error", ctx),
    "session.status.updated": (ctx) => sendEvent("session.status.updated", ctx),
    "tool.execute.before": (ctx) => sendEvent("tool.execute.before", ctx),
    "tool.execute.after": (ctx) => sendEvent("tool.execute.after", ctx),
    "permission.asked": (ctx) => sendEvent("permission.asked", ctx),
  }
};

function sendEvent(eventName, ctx) {
  const { execSync } = require("child_process");
  const payload = JSON.stringify({
    session_id: ctx.sessionId || "unknown",
    event: eventName,
    tool_name: ctx.toolName,
    tool_input: ctx.toolInput,
    status: ctx.status,
    cwd: ctx.cwd || process.cwd(),
    prompt: ctx.prompt,
  });
  try {
    execSync("BINARY_PATH hook --agent opencode", {
      input: payload,
      timeout: 5000,
      stdio: ["pipe", "ignore", "ignore"],
    });
  } catch (_) {}
}
```

`BINARY_PATH` is replaced at install time with the resolved path to the `dot-agent-deck` binary (same pattern as the Claude Code installer).

**Uninstall** removes the `~/.opencode/plugin/dot-agent-deck/` directory.

### 5. Display formatting (`src/ui.rs`)

Add `OpenCode` arm to `Display` impl for `AgentType`:

```rust
AgentType::OpenCode => write!(f, "OpenCode"),
```

This is already used in the card title rendering — no other UI change required for basic support.

## Risks

- **Plugin context shape uncertainty**: The exact fields on the OpenCode plugin callback argument need empirical verification. Mitigated by `serde(flatten)` catch-all so unexpected fields don't break deserialization.
- **OpenCode plugin API stability**: OpenCode is actively developed; plugin API may change. Mitigated by keeping the JS plugin thin and all logic in Rust.
- **Session ID format**: OpenCode's session ID format needs verification — it's an opaque string to our system, so any unique ID works.

## Success Criteria

- `dot-agent-deck hooks install --agent opencode` creates a working plugin
- OpenCode sessions appear in the dashboard with "OpenCode" label
- Events from OpenCode correctly map to session statuses (Thinking, Working, Idle, etc.)
- Claude Code integration continues to work unchanged (default behavior preserved)
- `dot-agent-deck hooks uninstall --agent opencode` cleanly removes the plugin
- All existing tests pass, new tests cover OpenCode event parsing

## Milestones

- [ ] `AgentType::OpenCode` variant added to enum with serialization tests (`src/event.rs`)
- [ ] `--agent` CLI flag on `hook` and `hooks install/uninstall` subcommands (`src/main.rs`)
- [ ] OpenCode event parsing and mapping implemented with tests (`src/hook.rs`)
- [ ] OpenCode JS plugin installer and uninstaller (`src/hooks_manage.rs`)
- [ ] Display formatting for OpenCode agent type (`src/ui.rs`)
- [ ] End-to-end verification: OpenCode sessions visible in dashboard
- [ ] All existing tests passing, new tests for OpenCode parsing and installation

## Key Files

- `src/event.rs` — AgentType enum extension
- `src/hook.rs` — OpenCode input parsing and event mapping
- `src/hooks_manage.rs` — Plugin installer/uninstaller
- `src/main.rs` — CLI flag additions
- `src/ui.rs` — Display impl update
