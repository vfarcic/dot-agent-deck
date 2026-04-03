# PRD #34: Extensible Modes System

**Status**: Draft
**Priority**: High
**Created**: 2026-04-03

## Problem

When AI agents execute commands (e.g., `kubectl get pods`, `cargo test`), the output is buried in the agent's conversation and scrolls away. DevOps/SREs, backend developers, and other practitioners want to see live, relevant command output in dedicated side panes alongside the agent — not just a snapshot in the chat.

## Solution

A fully config-driven **modes system** where projects define workspace layouts via a per-project `.dot-agent-deck.toml` file. Each mode creates a **Zellij tab** containing an agent pane and configurable side panes. Side panes come in two types:

- **Persistent panes** — predefined commands that run immediately on mode activation (e.g., `cargo watch -x test`, `kubectl get pods -w`)
- **Reactive panes** — populated when the agent executes commands matching user-defined regex rules (e.g., `kubectl describe`, `terraform plan`)

The code is a generic engine — all behavior is defined in config. Users can create any mode for any workflow (Kubernetes ops, Rust TDD, Node.js dev, AWS operations, observability, etc.). We ship one builtin mode (`kubernetes-operations`) as a starting point.

## UX Flow

1. User presses `n` → dir picker (existing)
2. User selects dir → app checks for `.dot-agent-deck.toml` in that dir
3. **If no config found** → straight to NewPaneForm (existing behavior, zero friction)
4. **If config found** → show mode selector: "New agent pane" (default) + one entry per mode
5. If "New agent pane" selected → existing NewPaneForm flow (unchanged)
6. If mode selected → new Zellij tab created with agent pane + side panes
7. Persistent panes start their commands immediately
8. Reactive panes start empty, populate as agent executes matching commands
9. Circular pane pool for reactive panes: commands cycle through available slots
10. Dashboard tab still shows a card for the session (status, tools, etc.)

## Config Design

Per-project file: `.dot-agent-deck.toml` in project root.

### Example: Kubernetes Operations

```toml
[[modes]]
name = "kubernetes-operations"
source = "builtin"

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
- `pane_count` can be set explicitly or auto-calculated from persistent + reactive needs
- If omitted: persistent count + 1 reactive slot (if rules exist)

### Config Auto-Update (Builtin Modes)

- `source = "builtin"` — app auto-updates this mode when new versions ship
- `source = "custom"` (default if omitted) — user-owned, never auto-updated
- On load: add missing builtins, update existing builtins, leave custom modes untouched
- Users who modify a builtin mode change `source` to `"custom"` to prevent overwriting

## Technical Design

### Config (`src/config.rs`)

New structs: `ModeRule`, `ModePersistentPane`, `ModeConfig`, `ProjectConfig`. Separate from existing `DashboardConfig` (global settings). `ProjectConfig` loads from `.dot-agent-deck.toml` in the selected directory.

### Hook Changes (`src/hook.rs`)

Store full bash command in `event.metadata["bash_command"]` for ToolStart events. Current `tool_detail` truncates to 120 chars — insufficient for re-execution. Display behavior unchanged.

### PaneController Extensions (`src/pane.rs`)

New trait methods: `create_tab`, `go_to_tab`, `write_to_pane`, `send_ctrl_c`. Implemented via Zellij CLI actions (`new-tab`, `go-to-tab-name`, `write-chars`, `write` with control bytes).

### Mode Manager (`src/mode_manager.rs` — new)

Core engine: compiles regex rules, manages pane pool, routes matching commands. Handles tab creation, persistent pane activation, circular reactive slot allocation, and cleanup.

### UI Changes (`src/ui.rs`)

New `ModeSelector` UI mode inserted between DirPicker and NewPaneForm (only when project config exists). Event processing wired into TUI main loop. Status indicator in stats bar.

## Edge Cases

- No config file in dir → skip mode selector, existing behavior unchanged
- User closes mode tab manually → ModeManager detects failure, resets state
- Agent session ends → Ctrl+C reactive panes, leave persistent panes running
- Invalid regex in config → log warning, skip rule, don't crash
- All persistent panes (no rules) → valid, purely predefined workspace
- All reactive panes (no persistent) → valid, purely agent-driven
- Multiple modes simultaneously → v1 supports one active mode; multi-mode is future work
- Builtin auto-update overwrites edits → only if `source = "builtin"`, documented

## Milestones

- [ ] Project config loading — `ProjectConfig` struct, `.dot-agent-deck.toml` parsing, builtin mode definitions, auto-update logic
- [ ] Full command capture — store complete bash command in hook event metadata for re-execution
- [ ] PaneController extensions — tab creation, tab switching, pane write, Ctrl+C via Zellij CLI
- [ ] Mode manager core — regex compilation, circular pane pool, command routing, tab/pane lifecycle
- [ ] Mode selector UI — new modal in `n` dialog flow, project config detection, mode list rendering
- [ ] Mode tab activation — create Zellij tab with agent + side panes, start persistent commands, wire reactive event processing
- [ ] Status indicator and help — mode status in stats bar, help overlay updates
- [ ] Unit tests — config parsing, rule matching, slot allocation, builtin merge
- [ ] Manual integration testing — end-to-end flow with kubernetes-operations mode
- [ ] Future analysis — evaluate global config modes as fallback (documented, not implemented)

## Out of Scope (v1)

- Global config modes (future — evaluate after v1)
- Multiple simultaneous active modes
- `dot-agent-deck init` command for scaffolding project configs
- Mode-specific keybindings or custom layouts beyond side panes
- Sharing modes across projects (library/registry)
