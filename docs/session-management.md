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

Cards also display:

- **Title row** — card number, the pane's display name (or `agent_type · session_id` if it hasn't been renamed), an animated status dot, and the status label
- **`Dir:`** — the working directory (basename, truncated to fit)
- **`Last:`** — elapsed time since the agent's last activity, alongside **`Tools:`** showing the total tool-call count
- **`Prmt:`** — the most recent user prompt(s)
- **Recent tool calls** — the last commands the agent ran

![Single agent card showing directory, last activity, tool count, recent prompt, and recent tool calls](/img/session-management-card.jpg)

How many prompts and tool calls fit on a card depends on the auto-chosen density, which Agent Deck picks based on how many cards are on the dashboard and how much room is available:

| Density | Prompts shown | Recent tool calls shown |
|---|---|---|
| Spacious | up to 3 | up to 3 |
| Normal | 1 | up to 3 |
| Compact | 1 | 1 |

The more agents you run in parallel, the more cards Agent Deck has to fit on the screen, so each card automatically becomes more compact. This is deliberate — scrolling through cards would defeat the point of having a single dashboard.

![Five agents running in parallel — cards switch to Compact density to fit them all without scrolling](/img/home-hero-dashboard.jpg)

## Resuming Sessions

The dashboard automatically saves your open panes (directories, names, and commands) when you exit. To restore them next time:

```bash
dot-agent-deck --continue
```

Without `--continue`, the dashboard starts with a blank slate. If a saved directory no longer exists, that pane is skipped with a warning.

After restore the dashboard is shown first so you get an overview before switching to a specific tab.

Mode tabs are also restored: each agent pane records which mode it belonged to, and `--continue` reopens the full mode tab — tab name, agent pane and its command, and all side panes with their commands — by looking up the mode config from the project's `.dot-agent-deck.toml`. The agent's internal conversation state is not restored; only the workspace structure is. If `.dot-agent-deck.toml` is missing or the mode was renamed at restore time, a warning is printed to stderr and the pane falls back to a plain dashboard pane.

Session data is stored in `~/.config/dot-agent-deck/session.toml`.
