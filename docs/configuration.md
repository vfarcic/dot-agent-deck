---
sidebar_position: 7
title: Configuration
---

# Configuration

## Default Command

```bash
# Set the default command pre-filled in the new-pane form
dot-agent-deck config set default_command "claude"

# Read the current value
dot-agent-deck config get default_command
```

`default_command` is the agent command pre-filled in the **new-pane form**'s Command field and the value that seeds the **schedule-authoring** agent. Both the new-pane form and the Scheduled Tasks **Add/Edit** flow use the same form — you type the command directly into the **Command** field (it accepts `claude`, `opencode`, a path, or any command), pre-filled from `default_command`. If `default_command` is unset, the schedule-authoring agent falls back to `claude`.

## Mouse

```bash
# Disable mouse capture so your terminal handles text selection / copy
dot-agent-deck config set mouse.enabled false
```

| Key | Default | Description |
|---|---|---|
| `mouse.enabled` | `true` | Capture the mouse for in-TUI interaction (click cards/buttons, click-drag to select within a pane, OSC52 copy-to-clipboard). |

When `mouse.enabled` is `true` (the default) dot-agent-deck puts the terminal into mouse-reporting mode, so click/drag is handled by the TUI. Set it to `false` if you prefer your **terminal's own** text selection and copy (select into the primary buffer, middle-click / your terminal's paste): the deck then leaves mouse reporting off entirely. The tradeoff is that the in-app mouse affordances (clickable cards/buttons and the in-pane mouse selection + its clipboard copy) are unavailable while disabled — every action still has a keyboard shortcut (press `?`). Note: with capture **on**, you can still do a one-off native selection in most terminals by holding **Shift** while dragging.

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `DOT_AGENT_DECK_SOCKET` | `$XDG_RUNTIME_DIR/dot-agent-deck.sock` or `/tmp/dot-agent-deck-{uid}.sock` | Unix socket path for daemon IPC. `{uid}` in the `/tmp` fallback is the user's POSIX uid, included so two users on the same host get disjoint sockets (the XDG path is already per-user since `XDG_RUNTIME_DIR` typically resolves to `/run/user/{uid}`). |
| `DOT_AGENT_DECK_CONFIG` | `~/.config/dot-agent-deck/config.toml` | Config file path |
| `DOT_AGENT_DECK_SESSION` | `~/.config/dot-agent-deck/session.toml` | Session file path |
| `DOT_AGENT_DECK_LOG` | *(unset)* | When set, enables file-based tracing logs. Empty value or `1` writes to `/tmp/dot-agent-deck.log`; any other value is treated as the target log file path. |

## Project Configuration

Per-project workspace modes are defined in `.dot-agent-deck.toml` at the project root. This file is loaded automatically when you select a directory in the new-pane flow.

### Quick Example

```toml
[[modes]]
name = "dev"

[[modes.panes]]
command = "git log --oneline -20"
name = "Recent Commits"

[[modes.rules]]
pattern = "cargo\\s+(build|test|check)"
watch = false
```

### Schema Overview

| Block | Key Fields |
|---|---|
| `[[modes]]` | `name` (required), `init_command` (optional), `panes`, `rules`, `reactive_panes` (default: 2) |
| `[[modes.panes]]` | `command` (required), `name` (optional label), `watch` (default: true) |
| `[[modes.rules]]` | `pattern` (regex, required), `watch` (bool), `interval` (seconds) |

For the full reference and more examples, see [Workspace Modes](workspace-modes.md).

### Scaffolding

Run `dot-agent-deck init` inside a project directory to generate a starter `.dot-agent-deck.toml`.

## Idle ASCII Art

When a session has been idle long enough, the dashboard can generate a short, context-aware ASCII art animation on the card using a lightweight LLM call. The feature is opt-in and disabled by default.

### Enabling

```bash
dot-agent-deck config set idle_art.enabled true
```

Set the API key for your chosen provider as an environment variable:

```bash
export ANTHROPIC_API_KEY=sk-...   # for Anthropic (default)
export OPENAI_API_KEY=sk-...      # for OpenAI
# Ollama requires no API key
```

### Options

| Key | Default | Description |
|-----|---------|-------------|
| `idle_art.enabled` | `false` | Enable idle ASCII art on dashboard cards |
| `idle_art.provider` | `anthropic` | LLM provider: `anthropic`, `openai`, or `ollama` |
| `idle_art.model` | `claude-haiku-4-5` | LLM model to use for generation |
| `idle_art.timeout_secs` | `300` | Seconds a session must be idle before art is triggered |

> **Note:** Idle art only appears in **Spacious** card density. Normal and Compact densities show the standard flashing-dot indicator instead.

### Standalone CLI

You can generate ASCII art outside the dashboard with the `ascii` subcommand:

```bash
dot-agent-deck ascii --input "debug the login flow" --output "fixed auth token refresh"
```

Optional `--provider` and `--model` flags override the configured defaults. The CLI works regardless of the `idle_art.enabled` setting.
