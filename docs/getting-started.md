---
sidebar_position: 2
title: Getting Started
---

# Getting Started

## Quick Start

```bash
# 1. Install dot-agent-deck
brew tap vfarcic/tap && brew install dot-agent-deck

# 2. Launch the dashboard (hooks are auto-installed for detected agents)
dot-agent-deck

# Or resume your previous session
dot-agent-deck --continue
```

Once the dashboard is running, press `?` inside the app to see all shortcuts.

## Hook Setup

Hooks are **auto-installed on every startup**. The CLI detects which agents are present by checking for their configuration directories and installs hooks automatically:

- **Claude Code** (`~/.claude/` detected) — writes entries into `~/.claude/settings.json` for hook types: SessionStart, SessionEnd, UserPromptSubmit, PreToolUse, PostToolUse, Notification, Stop, PreCompact, SubagentStart, SubagentStop.
- **OpenCode** (`~/.opencode/` detected) — creates a JS plugin at `~/.opencode/plugin/dot-agent-deck/index.js` that forwards session, tool, and permission events.

Auto-install is idempotent and best-effort — if an agent directory is missing the step is silently skipped, and errors are logged without blocking startup.

### Manual Management

The `hooks install` and `hooks uninstall` commands are still available for debugging or explicit removal:

```bash
# Install manually
dot-agent-deck hooks install                    # Claude Code
dot-agent-deck hooks install --agent opencode   # OpenCode

# Remove hooks
dot-agent-deck hooks uninstall                    # Claude Code
dot-agent-deck hooks uninstall --agent opencode   # OpenCode
```

> **Note:** If you uninstall hooks manually, the next dashboard launch will re-install them automatically.

## Launching

Running `dot-agent-deck` opens a two-column layout with native embedded terminal panes:

- **Left (1/3)** — the dashboard, displaying a card grid of agent sessions
- **Right (2/3)** — agent panes where Claude Code or OpenCode instances run (stacked by default, toggle to tiled with `Ctrl+t`)

No external terminal multiplexer is required — dot-agent-deck is a single binary.

## Basic Workflow

1. Launch the dashboard with `dot-agent-deck`
2. Press `Ctrl+n` to open a new pane (pick a directory, name, and command)
3. Run Claude Code in the new pane
4. Watch session statuses update in real-time on the dashboard
5. Press `Enter` on a card to jump to that agent's pane

> **Tip:** Press `Ctrl+d` from any pane to enter command / navigation mode.

## Working with Modes

Modes let you pair an agent session with live command output in a tabbed workspace. They are defined per-project in `.dot-agent-deck.toml`.

### Setting Up a Mode Config

Option A — scaffold a template, then edit:

```bash
cd your-project
dot-agent-deck init
```

Option B — create `.dot-agent-deck.toml` manually. Here is a minimal example:

```toml
[[modes]]
name = "dev"

[[modes.panes]]
command = "cargo watch -x test"
name = "Tests"

[[modes.rules]]
pattern = "cargo\\s+build"
watch = false
```

### Activating a Mode

1. Press `Ctrl+n` to start the new-pane flow.
2. Select a directory that contains `.dot-agent-deck.toml`.
3. In the unified form, use `Left`/`Right` (or `h`/`l`) to cycle the **Mode** field to your desired mode.
4. Fill in the agent name and command, then press `Enter`.
5. A new tab opens with the agent on the left and side panes on the right.

### Navigating Mode Tabs

Use `Tab`/`Shift+Tab` to cycle between tabs. The tab bar appears at the top when multiple tabs are open. See [Keyboard Shortcuts](keyboard-shortcuts.md) for all tab navigation keybindings.

> **Tip:** Press `Enter` on a mode agent's card in the dashboard to jump directly to its tab.

For the full configuration reference and more examples, see [Workspace Modes](workspace-modes.md).
