---
sidebar_position: 6
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

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `DOT_AGENT_DECK_SOCKET` | `$XDG_RUNTIME_DIR/dot-agent-deck.sock` or `/tmp/dot-agent-deck.sock` | Unix socket path for daemon IPC |
| `DOT_AGENT_DECK_CONFIG` | `~/.config/dot-agent-deck/config.toml` | Config file path |
| `DOT_AGENT_DECK_SESSION` | `~/.config/dot-agent-deck/session.toml` | Session file path |
| `DOT_AGENT_DECK_LOG` | *(unset)* | Set to any value to enable tracing logs on stderr |

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
