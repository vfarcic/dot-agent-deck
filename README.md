# dot-agent-deck

A terminal dashboard for monitoring and controlling multiple AI coding agent sessions.

[![CI](https://github.com/vfarcic/dot-agent-deck/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/vfarcic/dot-agent-deck/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/vfarcic/dot-agent-deck)](https://github.com/vfarcic/dot-agent-deck/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

<!-- TODO: Add terminal recording (asciinema or GIF) showing the dashboard in action -->

## Features

- **Real-time session monitoring** — see status, active tool, working directory, and last prompt for every agent session
- **Pane control** — create, focus, close, and rename agent panes without leaving the dashboard
- **Keyboard-driven interface** — vim-style navigation with single-key actions
- **Auto-installed hooks** — one command registers all required hooks

Currently supports **Claude Code** and **OpenCode**. Want support for your favorite TUI agent? [Open an issue](https://github.com/vfarcic/dot-agent-deck/issues/new) and let us know!

## Quick Start

```bash
# 1. Install Zellij (terminal multiplexer for pane control)
brew install zellij

# 2. Install dot-agent-deck
brew tap vfarcic/tap && brew install dot-agent-deck

# 3. Register agent hooks
dot-agent-deck hooks install                    # Claude Code
dot-agent-deck hooks install --agent opencode   # OpenCode

# 4. Launch the dashboard
dot-agent-deck
```

## Installation

### Zellij

dot-agent-deck uses [Zellij](https://zellij.dev/) for pane control. Install it first:

```bash
brew install zellij
```

See [Zellij installation docs](https://zellij.dev/documentation/installation) for other methods.

### Homebrew (macOS / Linux)

```bash
brew tap vfarcic/tap
brew install dot-agent-deck
```

### Download Binary

Download the latest binary for your platform from the [Releases](https://github.com/vfarcic/dot-agent-deck/releases/latest) page. Binaries are available for Linux (amd64, arm64) and macOS (amd64, arm64).

### Verify

```bash
dot-agent-deck --help
```

## Getting Started

### Hook Setup

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

### Launching

Running `dot-agent-deck` auto-launches Zellij with a two-column layout:

- **Left (1/3)** — the dashboard, displaying a card grid of agent sessions
- **Right (2/3)** — agent panes where Claude Code or OpenCode instances run (stacked by default, toggle to tiled with `t`)

The Zellij session is named `dot-agent-deck`. If the session already exists, it reattaches.

### Session Statuses

Each session card shows the agent's current state:

| Status | Meaning |
|---|---|
| **Thinking** | Agent is reasoning before acting |
| **Working** | Agent is executing a tool (tool name shown) |
| **Compacting** | Context window is being compressed |
| **WaitingForInput** | Agent needs user approval or input |
| **Idle** | Agent is between tasks |
| **Error** | Something went wrong |

Cards also display: session ID, agent type, working directory, tool count, and last user prompt.

### Basic Workflow

1. Launch the dashboard with `dot-agent-deck`
2. Press `n` to open a new pane (pick a directory, name, and command)
3. Run Claude Code in the new pane
4. Watch session statuses update in real-time on the dashboard
5. Press `Enter` on a card to jump to that agent's pane

> **Tip:** Press `Alt+d` (`Opt+d` on macOS) from any pane to jump back to the dashboard.

## Keyboard Shortcuts

### Dashboard Pane (active when the dashboard is focused)

| Key | Action |
|---|---|
| `j` / `Down` | Move down |
| `k` / `Up` | Move up |
| `h` / `Left` | Move left |
| `l` / `Right` | Move right |
| `Alt+1`–`9` | Jump to card N |
| `/` | Filter sessions |
| `r` | Rename session |
| `?` | Toggle help overlay |
| `Esc` | Clear filter |
| `q` / `Ctrl+c` | Quit |

### Pane Control

| Key | Action |
|---|---|
| `Enter` | Focus selected agent pane |
| `n` | New pane (directory picker, then name + command form) |
| `d` | Close selected agent pane |
| `t` | Toggle stacked / tiled layout |

### Directory Picker

| Key | Action |
|---|---|
| `j` / `Down` | Select next directory |
| `k` / `Up` | Select previous directory |
| `l` / `Right` / `Enter` | Enter directory (or confirm if no subdirs) |
| `h` / `Left` / `Backspace` | Go up one level |
| `Space` | Confirm current directory |
| `Esc` / `q` | Cancel |

### New Pane Form

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Switch between Name and Command fields |
| `Enter` | Confirm field / submit form |
| `Esc` | Cancel |

### Zellij Shortcuts (work from any pane)

| Key | Action |
|---|---|
| `Alt+d` / `Alt+h` / `Alt+Left` | Go to dashboard pane |
| `Alt+j` / `Alt+Down` | Navigate down in stacked panes |
| `Alt+k` / `Alt+Up` | Navigate up in stacked panes |
| `Alt+t` | Toggle stacked / tiled layout |
| `Alt+w` | Close current pane |
| `Alt+q` | Quit all (exit Zellij) |

> On macOS, `Alt` is the `Opt` key.

## Configuration

```bash
# Set the default command pre-filled in the new-pane form
dot-agent-deck config set default_command "claude"

# Read the current value
dot-agent-deck config get default_command
```

### Environment Variables

| Variable | Default | Description |
|---|---|---|
| `DOT_AGENT_DECK_SOCKET` | `$XDG_RUNTIME_DIR/dot-agent-deck.sock` or `/tmp/dot-agent-deck.sock` | Unix socket path for daemon IPC |
| `DOT_AGENT_DECK_CONFIG` | `~/.config/dot-agent-deck/config.toml` | Config file path |
| `DOT_AGENT_DECK_LOG` | *(unset)* | Set to any value to enable tracing logs on stderr |

## License

[MIT](LICENSE)
