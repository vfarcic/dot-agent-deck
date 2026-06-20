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

## Command Resolution and the Login-Shell PATH

The daemon spawns every pane's command — dashboard panes, scheduled-task fires, and the schedule-authoring agent alike — and resolves a bare command such as `claude` or `opencode` against its own `PATH`. To make *"a command that resolves for the logged-in user resolves in a dot-agent-deck pane"* reliably true, the daemon captures your **login-shell PATH** once at startup: it runs `$SHELL -lc 'printf %s "$PATH"'` (bounded by a short timeout) and adopts the result as its own `PATH`, which every pane it spawns then inherits. This means an agent installed under `~/.local/bin` (the default location for both Claude Code and opencode) resolves in a deck pane even when the daemon was launched from a context that never loaded your login profile — a non-interactive SSH session, a system service, or a bare launcher.

> **A profile PATH change needs a daemon restart**
>
> The login-shell PATH is captured **once, when the daemon starts**. If you change your shell profile's `PATH` — or install an agent into a directory that wasn't on it before — restart the daemon so it re-captures: `dot-agent-deck daemon restart`. Until then the daemon keeps using the `PATH` it captured at its last start.

If the capture fails for any reason — `$SHELL` is unset, the probe times out, or it returns nothing usable — the daemon keeps the `PATH` it inherited, so this never makes spawning *worse* than before. Only the `PATH` is taken from the login shell; other login-only variables (for example `KUBECONFIG` or cloud credentials) are not captured here — once an agent like Claude Code is running it sources your profile itself for its own shell-outs.

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
