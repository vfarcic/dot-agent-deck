# dot-agent-deck

A terminal dashboard for monitoring and controlling multiple AI coding agent sessions.

[![CI](https://github.com/vfarcic/dot-agent-deck/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/vfarcic/dot-agent-deck/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/vfarcic/dot-agent-deck)](https://github.com/vfarcic/dot-agent-deck/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

<!-- TODO: Add terminal recording (asciinema or GIF) showing the dashboard in action -->

## Features

- **Real-time session monitoring** — see status, active tool, working directory, and last prompt for every agent session
- **Workspace modes** — per-project config files define persistent side panes and reactive command routing (see [Workspace Modes](#workspace-modes))
- **Pane control** — create, focus, close, and rename agent panes without leaving the dashboard
- **Keyboard-driven interface** — vim-style navigation with single-key actions
- **Auto-installed hooks** — one command registers all required hooks

Currently supports **Claude Code** and **OpenCode**. Want support for your favorite TUI agent? [Open an issue](https://github.com/vfarcic/dot-agent-deck/issues/new) and let us know!

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

## Installation

### Platform Support

| Platform | Status |
|---|---|
| macOS (Intel & Apple Silicon) | Supported |
| Linux (amd64 & arm64) | Supported |
| Windows (via WSL) | Supported (runs as Linux) |
| Windows (native) | Coming soon ([#42](https://github.com/vfarcic/dot-agent-deck/issues/42)) — comment on the issue if you need this! |

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

Running `dot-agent-deck` opens a two-column layout with native embedded terminal panes:

- **Left (1/3)** — the dashboard, displaying a card grid of agent sessions
- **Right (2/3)** — agent panes where Claude Code or OpenCode instances run (stacked by default, toggle to tiled with `Ctrl+t`)

No external terminal multiplexer is required — dot-agent-deck is a single binary.

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
2. Press `Ctrl+n` to open a new pane (pick a directory, name, and command)
3. Run Claude Code in the new pane
4. Watch session statuses update in real-time on the dashboard
5. Press `Enter` on a card to jump to that agent's pane

> **Tip:** Press `Ctrl+d` from any pane to jump back to the dashboard.

### Session Management

The dashboard automatically saves your open panes (directories, names, and commands) when you exit. To restore them next time:

```bash
dot-agent-deck --continue
```

Without `--continue`, the dashboard starts with a blank slate. If a saved directory no longer exists, that pane is skipped with a warning.

Session data is stored in `~/.config/dot-agent-deck/session.toml`.

## Keyboard Shortcuts

### Dashboard Pane (active when the dashboard is focused)

| Key | Action |
|---|---|
| `j` / `Down` | Move down |
| `k` / `Up` | Move up |
| `h` / `Left` | Move left |
| `l` / `Right` | Move right |
| `1`–`9` | Jump to card N and focus its pane |
| `/` | Filter sessions |
| `r` | Rename session |
| `?` | Toggle help overlay |
| `Esc` | Clear filter |
| `Enter` | Focus selected agent pane |
| `y` / `n` | Approve / deny pending permission request |

### Directory Picker

| Key | Action |
|---|---|
| `j` / `Down` | Select next directory |
| `k` / `Up` | Select previous directory |
| `l` / `Right` / `Enter` | Enter directory (or confirm if no subdirs) |
| `h` / `Left` / `Backspace` | Go up one level |
| `Space` | Confirm current directory |
| `/` | Enter filter mode; type to narrow directories (case-insensitive) |
| `Esc` | Clear filter (press twice to close) |
| `q` | Cancel |

Directory lists loop end-to-end, so pressing `Up` on the first entry jumps to the last (and vice versa). The `..` parent entry always remains visible even when a filter is active.

### New Pane Form

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Switch between Name and Command fields |
| `Enter` | Confirm field / submit form |
| `Esc` | Cancel |

### Global Shortcuts (work from any mode)

| Key | Action |
|---|---|
| `Ctrl+d` | Return to dashboard (intercepted globally; prevents sending EOF to focused pane) |
| `Ctrl+n` | New pane (directory picker, then name + command form) |
| `Ctrl+w` | Close selected agent pane |
| `Ctrl+t` | Toggle stacked / tiled layout |

In PaneInput mode, `Ctrl+c` is delivered to the terminal as SIGINT (0x03). From the dashboard, pressing `Ctrl+c` twice triggers the quit confirmation dialog.

## Workspace Modes

Modes let you define workspace layouts per project so that relevant command output appears in dedicated side panes alongside your agent — instead of scrolling away in the chat.

### Quick Setup

```bash
# Scaffold a starter config in your project
cd my-project
dot-agent-deck init
```

This creates `.dot-agent-deck.toml` with a commented example. Edit it to match your workflow.

### How It Works

1. Press `Ctrl+n` and pick a directory that contains `.dot-agent-deck.toml`
2. A **mode selector** appears listing "New agent pane" (default) plus each mode defined in the config
3. Select a mode — the dashboard creates an agent pane on the left and side panes on the right in a 50/50 split
4. **Persistent panes** start their commands immediately (e.g., `cargo watch -x test`)
5. **Reactive panes** populate automatically when the agent runs a command matching one of your regex rules (e.g., `kubectl describe`)

If no `.dot-agent-deck.toml` exists in the chosen directory, the existing new-pane flow is used unchanged. The mode selector also offers a "Generate mode config" option that opens an agent pane with a prompt to create a config for you.

### Config Format

Create `.dot-agent-deck.toml` in your project root:

```toml
[[modes]]
name = "kubernetes-operations"
shell_init = "source .env"   # optional: runs in every side pane before its command

# Persistent panes — always visible, start immediately
[[modes.panes]]
command = "kubectl get pods -w"
name = "Pods"                # optional: defaults to the command

# Reactive rules — route agent commands to side panes
[[modes.rules]]
pattern = "kubectl\\s+(describe|explain)"   # regex matched against agent commands
watch = false                               # run once

[[modes.rules]]
pattern = "kubectl\\s+(get|top)"
watch = true                                # re-run periodically
interval = 2                                # refresh every 2 seconds
```

You can define multiple `[[modes]]` in a single file (e.g., one for Kubernetes ops, another for Rust TDD). Each mode has its own persistent panes and reactive rules.

### Mode Selector Shortcuts

| Key | Action |
|---|---|
| `j` / `k` | Navigate modes |
| `Enter` | Select mode |
| `Esc` | Cancel (opens default new-pane form) |

### Reactive Pane Pool

Persistent panes claim the first slots. Reactive rules share a circular pool of remaining slots — when a new command matches, it goes to the next available reactive pane, cycling back to the first when all are used.

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
| `DOT_AGENT_DECK_SESSION` | `~/.config/dot-agent-deck/session.toml` | Session file path |
| `DOT_AGENT_DECK_LOG` | *(unset)* | Set to any value to enable tracing logs on stderr |

## License

[MIT](LICENSE)
