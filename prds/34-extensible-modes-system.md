# PRD #34: Extensible Modes System

**Status**: In Progress
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

### Tab Model

The UI uses a tab-based layout. Tab 1 is always the **Dashboard** — the multi-agent overview with session cards and agent panes (unchanged from before this PRD). Each mode activation creates a **new tab** that is a self-contained workspace.

- **Tab bar** — displayed at the top of the screen when more than one tab exists. Shows tab names (e.g., "Dashboard | k8s-ops | rust-tdd"). Tabs are clickable with the mouse. The active tab is highlighted.
- **Dashboard tab** — left 1/3 session cards, right 2/3 agent panes (existing behavior, unchanged).
- **Mode tabs** — dedicated full-screen workspace: agent pane on the left 50%, side panes stacked on the right 50%.

### Mode Activation Flow

1. User presses `Ctrl+n` → dir picker (existing)
2. User selects dir → app checks for `.dot-agent-deck.toml` in that dir
3. **If no config found** → straight to NewPaneForm (existing behavior, zero friction). Pane is created in the dashboard tab.
4. **If config found** → show mode selector: "New agent pane" (default) + one entry per mode
5. If "New agent pane" selected → existing NewPaneForm flow (unchanged, pane in dashboard tab)
6. If mode selected → **new tab** created with agent pane + side panes in 50/50 layout
7. Persistent panes start their commands immediately
8. Reactive panes start empty, populate as agent executes matching commands
9. Circular pane pool for reactive panes: commands cycle through available slots
10. Dashboard still shows a session card for the mode's agent (status, tools, etc.)
11. Pressing `Enter` on that card in the dashboard switches to the mode's tab

### Tab Navigation

| Key | Action |
|---|---|
| `Ctrl+Shift+1`–`9` | Switch to tab N (1 = dashboard, 2+ = mode tabs) |
| Click tab in tab bar | Switch to that tab |
| `Enter` on dashboard card | If agent is in a mode tab, switch to that tab |
| Close mode tab | Tears down the entire workspace (agent + all side panes) |

### Tab Lifecycle

- Mode tabs have no per-pane close — all panes are defined by config, users cannot add or remove individual panes at runtime.
- Closing a mode tab destroys the entire workspace: agent pane + all persistent and reactive side panes.
- The dashboard tab cannot be closed.

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

1. **Tab-based workspaces.** Mode activation creates a new tab rather than injecting panes into the dashboard. This keeps the dashboard clean for multi-agent management and gives each mode a dedicated full-screen workspace. The tab bar is only shown when more than one tab exists (zero visual overhead for users who don't use modes).

2. **`Ctrl+Shift+1-9` for tab switching.** Avoids conflicts with terminal bindings (`Ctrl+Tab` is taken by most terminals). Numbers map directly to tab positions. Needs validation across common terminals (iTerm2, Alacritty, WezTerm, Windows Terminal, GNOME Terminal) — fallback to `Alt+1-9` if support is poor.

3. **No per-pane close in mode tabs.** All panes are defined by config; users cannot add or remove individual panes at runtime. Only the entire tab can be closed, which tears down the agent + all side panes together.

4. **Dashboard card links to mode tab.** When an agent running in a mode tab appears as a session card in the dashboard, pressing `Enter` on that card switches to the mode's tab. This ties the multi-agent overview to the focused workspaces.

5. **`shell_init` per mode.** An optional setup command (e.g., `source .env`, `devbox shell`) that runs in every side pane before the pane-specific command. Configured as `shell_init` on the mode.

6. **Shell panes with sent commands.** Side panes are created as shells, not process panes. Commands are sent after creation. This prevents pane death when commands exit with errors — the shell stays alive for re-use.

7. **50/50 split layout in mode tabs.** Agent pane takes the left half. All side panes share the right half, stacked vertically.

8. **All side panes visible simultaneously.** Persistent panes stack vertically in the right column. Reactive panes slot below them.

9. **All modes are user-defined.** No builtin modes or auto-update mechanism. Users own their config entirely.

10. **Pane `name` is optional.** Persistent panes require `command` but `name` is optional. When omitted, the pane name defaults to the command string itself.

11. **Reactive rules use `watch` and `interval`.** Each rule has a `pattern` (required regex), `watch` (bool, default false), and `interval` (u64 seconds, optional, only meaningful when `watch = true`). Commands like `kubectl get` benefit from periodic re-execution (`watch = true`) to show live state, while point-in-time commands like `kubectl describe` should run once (`watch = false`). Rules do not specify a target pane — matched commands go to the next available reactive pane in the circular pool.

12. **Real test config as canonical example.** `../dot-ai-infra/.dot-agent-deck.toml` contains a `kubernetes-operations` mode exercising persistent panes, watch rules, and one-shot rules. Use this for integration testing.

## Technical Design

### Config (`src/project_config.rs` — new)

New structs, separate from existing `DashboardConfig` (global settings). `ProjectConfig` loads from `.dot-agent-deck.toml` in the selected directory.

- `ProjectConfig` — top-level: `modes: Vec<ModeConfig>`
- `ModeConfig` — `name: String`, `shell_init: Option<String>`, `panes: Vec<ModePersistentPane>`, `rules: Vec<ModeRule>`
- `ModePersistentPane` — `command: String`, `name: Option<String>` (defaults to command)
- `ModeRule` — `pattern: String` (regex), `watch: bool` (default false), `interval: Option<u64>` (seconds, only when watch=true)

### Hook Changes (`src/hook.rs`)

Store full bash command in `event.metadata["bash_command"]` for ToolStart events. Current `tool_detail` truncates to 120 chars — insufficient for re-execution. Display behavior unchanged.

### EmbeddedPaneController Extensions (`src/pane.rs`)

Extend the existing `EmbeddedPaneController` to support mode-driven pane creation: creating multiple panes in a layout group, writing commands to panes, and sending control sequences (e.g., Ctrl+C for cleanup).

### Mode Manager (`src/mode_manager.rs` — new)

Core engine: compiles regex rules, manages pane pool, routes matching commands. Handles persistent pane activation, circular reactive slot allocation, and cleanup. Uses `EmbeddedPaneController` for all pane operations.

### Tab System (`src/ui.rs`)

New tab abstraction layered on top of the existing UI:

- **`Tab` enum** — `Dashboard` (existing layout: cards left, panes right) or `Mode { name, agent_pane_id, mode_manager }` (50/50 agent + side panes).
- **`TabBar`** — rendered at top of screen when `tabs.len() > 1`. Shows tab names, highlights active tab, supports mouse click to switch.
- **Tab switching** — `Ctrl+Shift+1-9` keybindings. Dashboard card `Enter` resolves the agent's tab and switches to it.
- **Tab close** — closing a mode tab calls `mode_manager.deactivate_mode()` to tear down all panes, then removes the tab.
- **Render dispatch** — the main render loop checks `active_tab` and delegates to either the existing dashboard renderer or a mode tab renderer.
- **Mode selector** — existing `ModeSelector` UI mode inserted between DirPicker and NewPaneForm (only when project config exists). When a mode is selected, it creates a new tab instead of injecting panes into the dashboard.
- **Reactive event routing** — the TUI main loop routes bash commands to the correct tab's `ModeManager` based on which tab owns the agent's pane ID.

## Edge Cases

- No config file in dir → skip mode selector, existing behavior unchanged
- Agent session ends → Ctrl+C reactive panes, leave persistent panes running
- Invalid regex in config → log warning, skip rule, don't crash
- All persistent panes (no rules) → valid, purely predefined workspace
- All reactive panes (no persistent) → valid, purely agent-driven
- Side pane command exits → shell stays alive (shell pane design decision)
- Only one tab (dashboard) → tab bar hidden, zero visual overhead
- Multiple mode tabs open simultaneously → each is independent, different configs/projects
- `Ctrl+Shift+N` where N > tab count → ignored
- Close last mode tab → switch back to dashboard
- Terminal doesn't support `Ctrl+Shift+1-9` → mouse click on tab bar still works; consider `Alt+1-9` fallback

## Milestones

### Phase 1: Foundation (complete)
- [x] Project config loading — `ProjectConfig` struct, `.dot-agent-deck.toml` parsing, `resolve_modes()` returns user-defined modes
- [x] Full command capture — store complete bash command in hook event metadata for re-execution
- [x] EmbeddedPaneController extensions — mode-driven pane creation, command writing, Ctrl+C via native PTY
- [x] Mode manager core — regex compilation, circular pane pool, command routing, pane lifecycle, `shell_init` support
- [x] Mode selector UI — modal in `n` dialog flow, loads config from selected directory, mode list with j/k navigation, "New agent pane" default option
- [x] Unit tests — config parsing, rule matching, slot allocation, mode selector navigation
- [x] Config generation via agent — in mode selector, offer "Generate config for this project" when no `.dot-agent-deck.toml` exists
- [x] `dot-agent-deck init` CLI command — scaffolding for project configs

### Phase 2: Tab-based workspaces (in progress)
- [x] Tab data model — `Tab` enum (Dashboard / Mode), tab list, active tab index, tab-to-pane mapping
- [x] Tab bar rendering — rendered at top when >1 tab, shows names, highlights active (mouse click deferred)
- [x] Tab switching — `Ctrl+PageUp/PageDown` keybindings (`Ctrl+Shift+1-9` not feasible in terminals)
- [x] Mode activation creates new tab — refactor mode activation to create a new tab with 50/50 layout instead of injecting panes into the dashboard
- [x] Mode tab rendering — dedicated render path: agent pane left 50%, side panes stacked right 50%
- [ ] Dashboard card → tab navigation — `Enter` on a session card whose agent lives in a mode tab switches to that tab
- [x] Tab close — close mode tab tears down entire workspace (agent + all side panes), switch to dashboard
- [x] Reactive event routing per tab — route bash commands to the correct tab's ModeManager based on agent pane ownership
- [ ] Update help overlay — reflect tab navigation keybindings
- [ ] Update README — document tab-based workflow
- [x] Tests — tab creation, switching, close, card-to-tab navigation

## Out of Scope (v1)

- Mode-specific keybindings or custom layouts beyond side panes
- Sharing modes across projects (library/registry)
- Global config modes (evaluate after v1)
