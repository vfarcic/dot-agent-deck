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
