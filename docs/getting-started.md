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

> **Tip:** Press `Ctrl+d` from any pane to jump back to the dashboard.
