# PRD #20: Multi-Agent Support (Codex, Gemini, Aider)

**Status**: Draft
**Priority**: Low
**Created**: 2026-03-31
**GitHub Issue**: [#20](https://github.com/vfarcic/dot-agent-deck/issues/20)

## Problem Statement

The dashboard only supports Claude Code. Developers increasingly use multiple AI coding tools — OpenAI Codex CLI, Google Gemini CLI, Aider, and others. Each tool has its own terminal session but no unified view exists to monitor them all. Users running mixed agent setups must mentally track which terminals run which tools.

## Solution Overview

Extend the event protocol and hook system to support additional AI coding agents through a generic adapter pattern. Each agent type gets a thin adapter that translates its native events into dot-agent-deck's `AgentEvent` protocol. The dashboard already handles events generically — the `agent_type` field just needs to be populated correctly and the UI needs per-agent-type visual distinction.

### Architecture

```
Claude Code  →  claude-code adapter (existing hooks)  →  AgentEvent  →  daemon
Codex CLI    →  codex adapter (wrapper script)         →  AgentEvent  →  daemon
Gemini CLI   →  gemini adapter (wrapper script)        →  AgentEvent  →  daemon
Aider        →  aider adapter (log watcher)            →  AgentEvent  →  daemon
```

## Scope

### In Scope
- Define a stable, documented `AgentEvent` JSON protocol
- Generic adapter interface: any process that sends `AgentEvent` JSON to the Unix socket is a valid adapter
- `dot-agent-deck wrap <agent-command>` — generic wrapper that intercepts stdio to generate events
- Agent-type visual distinction in the dashboard (colored badges, icons)
- `dot-agent-deck hooks install --agent <type>` for agents with native hook support
- Codex CLI adapter as the first non-Claude agent (uses wrapper approach)
- A per-session **live-target descriptor** so the dashboard can tell a writable session from a view-only one, plus an honest **send result** status when input is delivered (see Liveness & Write Semantics)

### Out of Scope
- Feature parity across all agents (each agent exposes different levels of detail)
- Agent-specific UI panels or detail views
- Installing or managing the agent tools themselves
- Permission control for non-Claude agents (PRD #18 is Claude-specific)
- **Proof-of-consumption machinery** — runtime "generation" tracking and input/output cursor diffing to *prove* a specific keystroke was consumed by the live process. The lightweight `live_target` + `send_result` model below is enough for this Low-priority PRD; cursor/generation proofs are a future hardening step, not a milestone here.

## Technical Approach

### Event Protocol Stabilization (`src/event.rs`)
- Document the `AgentEvent` JSON schema as a stable public API
- Add `agent_version: Option<String>` field
- Ensure `agent_type` is a free-form string (not an enum) to support future agents without code changes
- Add protocol version field for forward compatibility
- Add `live_target: Option<LiveTarget>` to describe how (and whether) the session can receive input (see Liveness & Write Semantics)

### Liveness & Write Semantics

**Invariant:** a dashboard-visible session is not necessarily a live, writable target. Today's Claude integration delivers input through a PTY/embedded pane and can reasonably assume the session it shows is the session it writes to. Other agents break that assumption — e.g. a Codex session the dashboard knows about from logs may only be *resumable from history*, not driveable live. The adapter contract must carry that distinction so the UI doesn't invite users to type into a card that can't accept input.

Each adapter declares a **live-target descriptor** per session:

- `kind`: `process | pty | tmux | sdk | none` — the concrete handle, if any, that input is delivered through
- `writable`: `live` | `history-only` | `none` — can we deliver input to the running session now, only resume/replay from history, or neither (view-only)?

When the dashboard delivers input to a session, the adapter returns an honest **send result** rather than fire-and-forget:

- `applied` — delivered to the live target
- `queued` — accepted, not yet confirmed applied
- `stale` — target moved on / our view was behind
- `wrong-session` — the handle no longer maps to the session we meant
- `history-only` — no live target; only history resume is possible
- `no-live-target` — nothing to write to

**UI consequence:** non-`live` sessions render visually distinct (e.g. dimmed input affordance / a "view-only" or "history" marker on the badge), and a failed/`stale`/`wrong-session` send surfaces feedback instead of silently dropping. Proving *consumption* of a specific input (generation counters, output-cursor diffing) is explicitly deferred — see Out of Scope.

_Credit: the live-target / send-result distinction was raised by @Snailflyer in [#20](https://github.com/vfarcic/dot-agent-deck/issues/20)._

### Generic Wrapper (`src/main.rs` or new `src/wrap.rs`)
- New CLI subcommand: `dot-agent-deck wrap -- codex <args>`
- Spawns the agent command as a child process
- Intercepts stdout/stderr to detect common patterns:
  - Prompt submission (user input lines)
  - Tool execution (command output patterns)
  - Status changes (thinking indicators, error messages)
- Sends `AgentEvent` messages to the daemon socket
- Passes through all I/O transparently (the agent remains fully interactive)

### Agent Type Registry (`src/config.rs`)
- Configuration for per-agent-type settings:
  ```toml
  [agents.codex]
  color = "green"
  label = "Codex"

  [agents.gemini]
  color = "blue"
  label = "Gemini"
  ```
- Default colors/labels for known agent types
- Unknown types get a neutral default

### Dashboard UI (`src/ui.rs`)
- Show agent type badge on each card (e.g., `[Claude]`, `[Codex]`, `[Gemini]`)
- Badge color from agent type registry
- Filter by agent type: `/` filter supports `type:codex` syntax
- Stats bar (PRD #17) shows breakdown by agent type if multiple types are active

### Adapter Pattern for Specific Agents

**Codex CLI**: Wrapper approach — `dot-agent-deck wrap -- codex`
- Detect tool calls from stdout patterns
- Map to Working/Idle/Error states

**Gemini CLI**: Wrapper approach — same pattern as Codex

**Aider**: Log watcher approach — Aider writes structured logs
- `dot-agent-deck watch --agent aider --log ~/.aider/logs/current.log`
- Tail the log file and parse structured entries into AgentEvent

### Pane Integration
- `dot-agent-deck pane new` gets `--agent <type>` flag
- Default command per agent type from config
- Pane created via Zellij with appropriate wrapper

## Success Criteria

- At least one non-Claude agent (Codex CLI) can be monitored in the dashboard
- Agent type is visually distinguishable on cards
- Events from different agent types coexist in the same dashboard
- Claude Code integration continues to work unchanged
- Filter supports agent type filtering
- `dot-agent-deck wrap` works with arbitrary commands as a basic fallback

## Milestones

- [ ] AgentEvent protocol documented with version field and stable JSON schema (`src/event.rs`)
- [ ] `live_target` descriptor (`kind` + `writable`) on the protocol and `send_result` status returned on input delivery (`src/event.rs`, `src/pane_input.rs`)
- [ ] UI renders view-only / history-only sessions distinctly and surfaces failed/stale sends (`src/ui.rs`)
- [ ] Agent type registry with configurable colors/labels (`src/config.rs`)
- [ ] Agent type badge rendering on cards (`src/ui.rs`)
- [ ] `dot-agent-deck wrap` CLI subcommand with stdout/stderr pattern detection (`src/wrap.rs`)
- [ ] Codex CLI adapter working end-to-end via wrapper
- [ ] `--agent` flag on `pane new` command with per-type default commands
- [ ] Agent type filter support in `/` search (`src/ui.rs`)
- [ ] Documentation: adapter authoring guide for third-party agents
- [ ] All existing tests passing, new tests for wrapper and registry

## Key Files

- `src/event.rs` — Protocol stabilization, version field, `live_target` descriptor, `send_result`
- `src/pane_input.rs` — Return `send_result` status on input delivery instead of fire-and-forget
- `src/wrap.rs` (new) — Generic wrapper command
- `src/config.rs` — Agent type registry
- `src/ui.rs` — Agent badges, type filtering
- `src/main.rs` — CLI subcommand registration
- `src/pane.rs` — Agent-aware pane creation

## Risks

- **Pattern detection fragility**: Wrapper-based adapters rely on parsing stdout, which can break if agent tools change their output format. Mitigated by keeping patterns simple and having a "generic" fallback that shows basic active/idle state.
- **Agent tool availability**: Each agent has its own installation, auth, and API key requirements. We don't manage these — we just monitor.
- **Feature disparity**: Different agents expose very different levels of information. Cards for wrapper-based agents will be sparser than Claude Code cards. This is acceptable — basic status is still valuable.
