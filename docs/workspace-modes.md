---
sidebar_position: 5
title: Workspace Modes
---

# Workspace Modes

Modes are config-driven workspaces that pair an AI agent with live command output in side panes. Each mode activation creates a new tab — a self-contained workspace with the agent pane on the left (50%) and side panes stacked on the right (50%). Modes are defined per-project in a `.dot-agent-deck.toml` file at the project root.

## Concepts

### Persistent Panes

Defined in `[[modes.panes]]`. These run immediately when the mode activates and stay alive for the lifetime of the tab.

By default (`watch = true`), persistent pane commands are re-executed every 10 seconds via the built-in `dot-agent-deck watch` subcommand. Write plain commands without watch/follow flags — the system handles refresh automatically. Set `watch = false` for commands that stream on their own (e.g., `kubectl get pods -w`, `tail -f`).

### Reactive Panes

Driven by `[[modes.rules]]`. Reactive panes start empty and populate when the agent executes a command matching a rule's regex `pattern`.

- `watch = false` (default) — command runs once and the output stays visible.
- `watch = true` with optional `interval` — command re-runs on a timer (in seconds) via the built-in `dot-agent-deck watch` subcommand, producing clean output without shell prompt artifacts.

### Circular Pane Pool

Persistent panes claim the first slots and are never overwritten. Reactive commands cycle through the remaining slots. When all reactive slots are occupied, the oldest is reused. This keeps the workspace bounded while surfacing the most recent output.

## Configuration Reference

### `[[modes]]`

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `name` | string | yes | — | Display name shown in the tab bar |
| `init_command` | string | no | — | Setup command run once in every pane before its own command (e.g., `devbox shell`) |
| `panes` | array | no | `[]` | Persistent pane definitions |
| `rules` | array | no | `[]` | Reactive command-routing rules |
| `reactive_panes` | integer | no | `2` | Number of reactive pane slots for command routing |

### `[[modes.panes]]`

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `command` | string | yes | — | Shell command to run |
| `name` | string | no | command string | Display label for the pane |
| `watch` | bool | no | `true` | Re-execute command every 10s via built-in watcher. Set to `false` for commands with built-in streaming (e.g., `-w`, `tail -f`) |

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

## Side Pane Interaction

Side panes in a mode tab support focus, selection, and direct interaction.

### Focus & Navigation

A visual indicator highlights the currently selected side pane. Use `j`/`k` (or `Down`/`Up`) to move the selection between panes. Press `Esc` to deselect and return focus to the agent pane. You can also click a side pane to select it, or click the agent pane to deselect.

### PaneInput Mode

Press `Enter` on a selected side pane to enter PaneInput mode — this lets you type directly into the pane's shell (run commands, send input, interact with running processes). `Ctrl+c` sends SIGINT to the pane's process. Press `Ctrl+d` to exit PaneInput mode and return to Normal mode.

If no side pane is selected, `Enter` focuses the agent pane instead.

## Scaffolding

### `dot-agent-deck init`

Run `dot-agent-deck init` inside a project directory to generate a `.dot-agent-deck.toml` template:

```bash
cd your-project
dot-agent-deck init
```

The generated file contains a commented example you can edit. It will not overwrite an existing config.

### Agent-Assisted Config Generation

When an agent session is running in a directory without a `.dot-agent-deck.toml`, its dashboard card shows a yellow hint: **`g: generate .dot-agent-deck.toml`**.

Press `g` on the card to open a dialog with three options (navigate with arrow keys, confirm with Enter):

- **Yes** — sends a prompt to the agent asking it to analyze the project, propose a config, and write it after your approval.
- **No** — dismisses the dialog; the hint stays on the card.
- **Never** — suppresses the hint permanently for this directory.

After the agent creates the file, press `Ctrl+w` to close the current pane, then `Ctrl+n` to create a new one and select your mode.

To disable the hint globally: `dot-agent-deck config set auto_config_prompt false`.

### Config Validation

Run `dot-agent-deck validate` to check your config for issues:

```bash
cd your-project
dot-agent-deck validate
```

This checks regex syntax, duplicate mode names, and mismatched watch/interval settings.

### `dot-agent-deck watch`

A built-in command that re-executes a shell command at a fixed interval with clean terminal output, similar to the Linux `watch` utility. This is used internally by reactive watch rules but can also be run standalone:

```bash
dot-agent-deck watch --interval 2 "kubectl get pods"
```

The command clears the screen between executions and displays a header line showing the interval and command. Press `Ctrl+C` to stop.
