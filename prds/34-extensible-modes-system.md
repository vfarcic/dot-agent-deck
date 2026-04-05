# PRD #34: Extensible Modes System

**Status**: Draft
**Priority**: High
**Created**: 2026-04-03

## Problem

When AI agents execute commands (e.g., `kubectl get pods`, `cargo test`), the output is buried in the agent's conversation and scrolls away. DevOps/SREs, backend developers, and other practitioners want to see live, relevant command output in dedicated side panes alongside the agent — not just a snapshot in the chat.

## Solution

A fully config-driven **modes system** where projects define workspace layouts via a per-project `.dot-agent-deck.toml` file. Each mode creates a workspace with an agent pane and configurable side panes using the native embedded terminal system (`EmbeddedPaneController`). Side panes come in two types:

- **Persistent panes** — predefined commands that run immediately on mode activation (e.g., `cargo watch -x test`, `kubectl get pods -w`)
- **Reactive panes** — populated when the agent executes commands matching user-defined regex rules (e.g., `kubectl describe`, `terraform plan`)

The code is a generic engine — all behavior is defined in config. Users can create any mode for any workflow (Kubernetes ops, Rust TDD, Node.js dev, AWS operations, observability, etc.). All modes are user-defined in project config.

## Prior Art

An earlier implementation attempt used Zellij as the terminal multiplexer (branch `feature/prd-34-extensible-modes-system`). This was abandoned due to: tab bar visibility issues, keybinding conflicts, external dependency friction, layout control limitations, and pane collapse on command exit. PRD-39 replaced Zellij with native terminal panes (`portable-pty` + `vt100`), which this PRD now targets.

## UX Flow

1. User presses `n` → dir picker (existing)
2. User selects dir → app checks for `.dot-agent-deck.toml` in that dir
3. **If no config found** → straight to NewPaneForm (existing behavior, zero friction)
4. **If config found** → show mode selector: "New agent pane" (default) + one entry per mode
5. If "New agent pane" selected → existing NewPaneForm flow (unchanged)
6. If mode selected → workspace created with agent pane + side panes
7. Persistent panes start their commands immediately
8. Reactive panes start empty, populate as agent executes matching commands
9. Circular pane pool for reactive panes: commands cycle through available slots
10. Dashboard still shows a card for the session (status, tools, etc.)

## Config Design

Per-project file: `.dot-agent-deck.toml` in project root.

### Example: Kubernetes Operations

```toml
[[modes]]
name = "kubernetes-operations"
shell_init = "source .env"

[[modes.panes]]
command = "kubectl get pods -w"
name = "Pods"

[[modes.rules]]
pattern = "kubectl\\s+(describe|explain)"
watch = false

[[modes.rules]]
pattern = "kubectl\\s+(get|top)"
watch = true
interval = 2

[[modes.rules]]
pattern = "kubectl\\s+logs"
watch = true
interval = 5

[[modes.rules]]
pattern = "helm\\s+(status|list)"
watch = false
```

### Example: Rust TDD

```toml
[[modes]]
name = "rust-tdd"

[[modes.panes]]
command = "cargo watch -x test"
name = "Tests"

[[modes.panes]]
command = "cargo watch -x clippy"
name = "Lint"

[[modes.rules]]
pattern = "cargo\\s+build"
watch = false
```

### Pane Slot Allocation

- Persistent panes claim the first N slots (reserved, never overwritten by reactive commands)
- Reactive rules cycle through remaining slots only (circular pool)
- Auto-calculated: persistent count + 1 reactive slot (if rules exist)

## Design Decisions

1. **`shell_init` per mode.** An optional setup command (e.g., `source .env`, `devbox shell`) that runs in every side pane before the pane-specific command. Configured as `shell_init` on the mode.

2. **Shell panes with sent commands.** Side panes are created as shells, not process panes. Commands are sent after creation. This prevents pane death when commands exit with errors — the shell stays alive for re-use.

3. **50/50 split layout.** Agent pane takes the left half. All side panes share the right half, stacked vertically.

4. **All side panes visible simultaneously.** Persistent panes stack vertically in the right column. Reactive panes slot below them.

5. **All modes are user-defined.** No builtin modes or auto-update mechanism. Users own their config entirely.

## Technical Design

### Config (`src/project_config.rs` — new)

New structs: `ModeRule`, `ModePersistentPane`, `ModeConfig`, `ProjectConfig`. Separate from existing `DashboardConfig` (global settings). `ProjectConfig` loads from `.dot-agent-deck.toml` in the selected directory.

### Hook Changes (`src/hook.rs`)

Store full bash command in `event.metadata["bash_command"]` for ToolStart events. Current `tool_detail` truncates to 120 chars — insufficient for re-execution. Display behavior unchanged.

### EmbeddedPaneController Extensions (`src/pane.rs`)

Extend the existing `EmbeddedPaneController` to support mode-driven pane creation: creating multiple panes in a layout group, writing commands to panes, and sending control sequences (e.g., Ctrl+C for cleanup).

### Mode Manager (`src/mode_manager.rs` — new)

Core engine: compiles regex rules, manages pane pool, routes matching commands. Handles persistent pane activation, circular reactive slot allocation, and cleanup. Uses `EmbeddedPaneController` for all pane operations.

### UI Changes (`src/ui.rs`)

New `ModeSelector` UI mode inserted between DirPicker and NewPaneForm (only when project config exists). Event processing wired into TUI main loop. Status indicator in stats bar.

## Edge Cases

- No config file in dir → skip mode selector, existing behavior unchanged
- Agent session ends → Ctrl+C reactive panes, leave persistent panes running
- Invalid regex in config → log warning, skip rule, don't crash
- All persistent panes (no rules) → valid, purely predefined workspace
- All reactive panes (no persistent) → valid, purely agent-driven
- Multiple modes simultaneously → v1 supports one active mode; multi-mode is future work
- Side pane command exits → shell stays alive (shell pane design decision)

## Milestones

- [ ] Project config loading — `ProjectConfig` struct, `.dot-agent-deck.toml` parsing, `resolve_modes()` returns user-defined modes
- [ ] Full command capture — store complete bash command in hook event metadata for re-execution
- [ ] EmbeddedPaneController extensions — mode-driven pane creation, command writing, Ctrl+C via native PTY
- [ ] Mode manager core — regex compilation, circular pane pool, command routing, pane lifecycle, `shell_init` support
- [ ] Mode selector UI — modal in `n` dialog flow, loads config from selected directory, mode list with j/k navigation, "New agent pane" default option
- [ ] Mode activation — create agent + side panes in 50/50 layout, start persistent commands, wire reactive event processing
- [ ] Status indicator and help — mode status in stats bar, help overlay updates
- [ ] Unit tests — config parsing, rule matching, slot allocation, mode selector navigation
- [ ] Manual integration testing — end-to-end flow with a sample mode config
- [ ] Config generation via agent — in mode selector, offer "Generate config for this project" when no `.dot-agent-deck.toml` exists
- [ ] `dot-agent-deck init` CLI command — scaffolding for project configs

## Out of Scope (v1)

- Multiple simultaneous active modes per session
- Mode-specific keybindings or custom layouts beyond side panes
- Sharing modes across projects (library/registry)
- Global config modes (evaluate after v1)
