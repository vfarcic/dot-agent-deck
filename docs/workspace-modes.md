---
sidebar_position: 5
title: Workspace Modes
---

# Workspace Modes

Modes are config-driven workspaces that pair an AI agent with live command output in side panes. Each mode activation creates a new tab — a self-contained workspace with the agent pane on the left (50%) and side panes stacked on the right (50%). Modes are defined per-project in a `.dot-agent-deck.toml` file at the project root.

## Concepts

### Persistent Panes

Defined in `[[modes.panes]]`. These run immediately when the mode activates and stay alive for the lifetime of the tab.

Use persistent panes for continuous monitoring — `cargo watch -x test`, `kubectl get pods -w`, or a log tail.

### Reactive Panes

Driven by `[[modes.rules]]`. Reactive panes start empty and populate when the agent executes a command matching a rule's regex `pattern`.

- `watch = false` (default) — command runs once and the output stays visible.
- `watch = true` with optional `interval` — command re-runs on a timer (in seconds).

### Circular Pane Pool

Persistent panes claim the first slots and are never overwritten. Reactive commands cycle through the remaining slots. When all reactive slots are occupied, the oldest is reused. This keeps the workspace bounded while surfacing the most recent output.

## Configuration Reference

### `[[modes]]`

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `name` | string | yes | — | Display name shown in the tab bar |
| `panes` | array | no | `[]` | Persistent pane definitions |
| `rules` | array | no | `[]` | Reactive command-routing rules |

### `[[modes.panes]]`

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `command` | string | yes | — | Shell command to run |
| `name` | string | no | command string | Display label for the pane |

### `[[modes.rules]]`

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `pattern` | string (regex) | yes | — | Regex matched against agent bash commands |
| `watch` | bool | no | `false` | Re-run on interval when `true` |
| `interval` | integer (seconds) | no | — | Refresh interval (only used when `watch = true`) |

## Examples

### Kubernetes Operations

A mode for working with Kubernetes clusters. The persistent pane watches pod status continuously, while rules capture kubectl and Helm output as the agent investigates.

```toml
[[modes]]
name = "kubernetes-operations"

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

### Rust TDD

A mode for test-driven development in Rust. Two persistent panes run continuously — one for tests and one for linting. A reactive rule captures build output.

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

## Tab Lifecycle

### Creating a Mode Tab

1. Press `Ctrl+n` to start the new-pane flow.
2. Select a directory that contains a `.dot-agent-deck.toml`.
3. In the unified form, use `Left`/`Right` (or `h`/`l`) to cycle the **Mode** field to your desired mode.
4. Fill in the agent name and command, then press `Enter`.
5. A new tab opens with the agent on the left and side panes on the right.

### Switching Tabs

The tab bar appears at the top when more than one tab is open. Use `Tab`/`Shift+Tab` or arrow keys to cycle between tabs. See [Keyboard Shortcuts](keyboard-shortcuts.md) for all keybindings.

### Closing a Mode Tab

Press `Ctrl+w` on a mode tab to tear down the entire workspace — the agent and all side panes are stopped. The dashboard tab cannot be closed.

### Dashboard Card Navigation

Press `Enter` on an agent's card in the dashboard to jump directly to that agent's mode tab (if it has one).

## Scaffolding

### `dot-agent-deck init`

Run `dot-agent-deck init` inside a project directory to generate a `.dot-agent-deck.toml` template:

```bash
cd your-project
dot-agent-deck init
```

The generated file contains a commented example you can edit. It will not overwrite an existing config.

### Agent-Assisted Config Generation

When creating a new pane in a directory that has no `.dot-agent-deck.toml`, the mode selector offers a **Generate config** option. Selecting it invokes the agent to analyze the project and create a config tailored to the detected toolchain.
