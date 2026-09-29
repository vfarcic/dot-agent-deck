---
title: Desktop App
---

# Desktop App

The desktop app is a second client of the same daemon the TUI talks to. It does not replace the TUI and runs no agents of its own: the daemon owns every agent, whichever client started it, so an agent started in the TUI appears on the desktop Dashboard and one started from the desktop appears as a card in the TUI. Closing the desktop app leaves your agents running, as detaching the TUI does.

The desktop app is an **alpha**. Its downloads are labelled `desktop-alpha` and are not covered by the support expectations of the CLI. See [Installation → Desktop app](../installation.md#desktop-app) for what to download and how to install it.

## Before you start: the app needs a running daemon

The desktop app connects to a daemon; it does not start one. If no daemon is running on this machine, the Dashboard shows **Daemon disconnected** and a **Reconnect** button. Start a daemon with the TUI (`dot-agent-deck`) or with `dot-agent-deck daemon serve`, then press **Reconnect**. [Installation → How the desktop app gets a daemon](../installation.md#how-the-desktop-app-gets-a-daemon) has the details, including how long a daemon with no clients and no agents stays up.

To collect a log for a bug report, start that daemon with logging turned on. The app itself writes no log. See [Troubleshooting → With the desktop app](../troubleshooting.md#with-the-desktop-app).

## What the app shows

The navigation rail on the left has two entries:

- **Dashboard** — every agent on the daemons you are watching, as rows with a status. Clicking a row opens that agent's terminal. See [Dashboard](dashboard.md).
- **Settings** — the daemons the app knows about, appearance, zoom and voice. See [Daemons](daemons.md), [Settings](settings.md) and [Voice control](voice.md).

From the Dashboard, **New agent** starts an agent, an orchestration, a dispatcher or a schedule-authoring agent on any connected daemon. See [New agent](new-agent.md).

The **Voice** button at the bottom of the window turns voice control on and off. See [Voice control](voice.md).

## What the TUI has and the desktop app does not

The desktop app covers part of what the TUI does. These are TUI-only today:

- The [Schedules manager](../scheduled-tasks.md#the-schedules-dialog). The desktop app can start a schedule-authoring agent from **New agent**, but cannot list, edit or run schedules; use the TUI or the `dot-agent-deck schedule` CLI.
- Generating `.dot-agent-deck.toml` with `g`, filtering agents with `/`, renaming an agent, and [customising keybindings](../keyboard-shortcuts.md).
- Automatic workspace save and restore, and the quit dialog's **Detach** / **Stop**.
- Installing and starting a daemon on a remote machine (`dot-agent-deck remote add` and `connect`). The desktop app connects to a remote daemon that is already running; see [Daemons](daemons.md).

And the desktop app has two things the TUI does not: one Dashboard over several daemons at once, and voice control.

## Features behind the `experimental` flag

Some desktop surfaces are hidden unless the `experimental` flag is on, and these pages do not describe them. If you do not see one of these, that is why:

- The **Daemons** entry in the rail and the Dashboard's **Open daemons** button, which lead to a multi-pane workspace. The **Start daemon**, **Stop** and **Replace daemon** controls, renaming an agent, and the output **Reader** are on that screen too, so they are hidden with it.
- The **Projects**, **Prompts**, **Orchestrations** and **Agent Profiles** entries in the rail.

**How the desktop app reads the flag.** Only from its own environment, once, when it starts: set `DOT_AGENT_DECK_EXPERIMENTAL=1` in the environment the app is launched from, or point `DOT_AGENT_DECK_FEATURES_CONFIG` at a `.dot-agent-deck.toml` whose `[features]` table sets `experimental = true`. Unlike the TUI and the daemon, the desktop app does not look for a `.dot-agent-deck.toml` in any directory, and it does not notice a change until it is restarted. An app launched from the macOS Dock or Finder, or from a Linux desktop menu, does not inherit variables exported in your shell profile.

One flag reaches the desktop app from the daemon instead: the **schedule: issues** chip in **New agent** appears only when the **daemon** it is creating the agent on has `experimental` on, as the TUI's does.
