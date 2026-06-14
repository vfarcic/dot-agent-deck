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

Agent Deck restores your workspace automatically. There is no flag to pass and no decision to make: every time you launch the TUI — `dot-agent-deck` locally or `dot-agent-deck connect <name>` against a remote daemon — your previous panes, names, directories, commands, and tabs come back.

> **Breaking change:** Auto-restore is now the default. Bare `dot-agent-deck` restores your previous workspace instead of starting empty, and the old `--continue` flag has been removed. If you have wrapper scripts or aliases that pass `--continue`, drop it — running `dot-agent-deck --continue` now prints a short message explaining that auto-restore is the default. To start from an empty dashboard on purpose, see [Starting Fresh](#starting-fresh) below.

Restore happens in two layers, tried in order:

1. **Daemon hydration first.** The background daemon owns your running agents, so on attach the dashboard rehydrates whatever the daemon currently holds — agents still in their previous state, with live output. This is the common case when you detach (close the TUI, or disconnect from a remote) and reattach later.
2. **On-disk snapshot fallback.** If the daemon is empty — a fresh machine, the first launch after a reboot, or recovery after a daemon crash — Agent Deck falls back to the saved snapshot on disk and recreates the workspace structure: panes, names, directories, commands, and tabs. Agent processes are respawned fresh; each agent's own conversation state is restored by its own command line (for example, `claude --continue`), not by Agent Deck.

If both the daemon and the snapshot are empty, you land on a clean, empty dashboard. When a snapshot restore rebuilds one or more orchestration tabs, Agent Deck opens the first restored orchestration tab so you land where you left off; otherwise — and after a daemon hydration with no orchestration to land on — you start on the dashboard for an overview. If a saved directory no longer exists, that pane is skipped with a warning.

### The snapshot stays fresh

The on-disk snapshot is written continuously, not only when you quit cleanly. Agent Deck saves it on every meaningful change to your workspace — a new pane, a rename, a mode or orchestration tab opening or closing, an agent stopping or restarting — and again when you detach. Writes are coalesced, so a burst of changes (such as spinning up a full orchestration) collapses to one or two disk writes rather than thrashing the disk on every keystroke. This is what makes crash recovery useful: because the snapshot reflects your latest state at detach time, a respawned-empty daemon falls back to an up-to-date workspace, not a weeks-stale one from your last clean quit.

### Mode and orchestration tabs are restored too

Mode tabs are restored in full: each agent pane records which mode it belonged to, and restore reopens the entire mode tab — tab name, agent pane and its command, and all side panes with their commands — by looking up the mode config from the project's `.dot-agent-deck.toml`.

Orchestration tabs are restored as well. From a warm daemon they come back via hydration; when the daemon is empty they are rebuilt from the snapshot, which records the orchestrator pane and its prompt, the role panes in their saved order, and the start-role cursor. Agent Deck re-resolves the orchestration config from the project's `.dot-agent-deck.toml` to rebuild the tab.

In every case only the workspace structure is restored, not an agent's internal conversation state. If the relevant config has drifted at restore time — `.dot-agent-deck.toml` is missing, the mode or orchestration was renamed, or a role was removed — a clear warning is shown and the affected pane falls back to a plain dashboard pane rather than a half-broken tab.

### Starting Fresh

To discard the saved workspace and start from an empty dashboard next time, clear the snapshot:

```bash
dot-agent-deck snapshot clear
```

This deletes the single global snapshot file. Note that `dot-agent-deck remote remove <name>` is registry-only — it removes a remote from your local registry and intentionally does **not** clear the snapshot, so removing an unrelated remote never wipes your local workspace.

Session data is stored in `~/.config/dot-agent-deck/session.toml`.
