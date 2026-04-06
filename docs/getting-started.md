---
sidebar_position: 2
title: Getting Started
---

# Getting Started

## Quick Start

```bash
# 1. Install dot-agent-deck
brew tap vfarcic/tap && brew install dot-agent-deck

# 2. Register agent hooks
dot-agent-deck hooks install                    # Claude Code
dot-agent-deck hooks install --agent opencode   # OpenCode

# 3. Launch the dashboard
dot-agent-deck

# 4. Or resume your previous session
dot-agent-deck --continue
```

Once the dashboard is running, press `?` inside the app to see all shortcuts.

## Hook Setup

Register hooks so your agents send events to the dashboard:

**Claude Code:**

```bash
dot-agent-deck hooks install
```

This writes entries into `~/.claude/settings.json` for these hook types: SessionStart, SessionEnd, UserPromptSubmit, PreToolUse, PostToolUse, Notification, Stop, PreCompact, SubagentStart, SubagentStop. The command is idempotent — safe to run again.

**OpenCode:**

```bash
dot-agent-deck hooks install --agent opencode
```

This creates a JS plugin at `~/.opencode/plugin/dot-agent-deck/index.js` that forwards session, tool, and permission events to the dashboard.

**To remove hooks:**

```bash
dot-agent-deck hooks uninstall                    # Claude Code
dot-agent-deck hooks uninstall --agent opencode   # OpenCode
```

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
