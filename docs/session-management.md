---
sidebar_position: 4
title: Session Management
---

# Session Management

## Session Statuses

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

## Resuming Sessions

The dashboard automatically saves your open panes (directories, names, and commands) when you exit. To restore them next time:

```bash
dot-agent-deck --continue
```

Without `--continue`, the dashboard starts with a blank slate. If a saved directory no longer exists, that pane is skipped with a warning.

Session data is stored in `~/.config/dot-agent-deck/session.toml`.
