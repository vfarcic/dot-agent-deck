# Desktop App

The desktop app is a graphical client of the same daemon the TUI talks to. The daemon owns every agent, whichever client started it: an agent started in the TUI appears on the desktop Dashboard, and one started from the desktop app appears as a card in the TUI. Quitting the desktop app leaves your agents running, as detaching the TUI does.

The desktop app is an **alpha**. Its downloads are named `dot-agent-deck-desktop-alpha-*` and are not covered by the support expectations of the CLI. Builds exist for macOS on Apple Silicon (`.dmg`) and Linux amd64 (`.deb`) only. [Installation → Desktop app](../installation.md#desktop-app) says what to download and how to install it.

## Before you start: the app needs a running daemon

The desktop app connects to a daemon; it does not start one. Before you open it, start a daemon on this machine in either of these ways:

- Run the TUI, `dot-agent-deck`. It starts a daemon if none is running.
- Run `dot-agent-deck daemon serve` in a terminal. It runs in the foreground, and exits on its own about 30 seconds after it last had a client, an agent or an enabled schedule, so open the app within that window. `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0 dot-agent-deck daemon serve` keeps it up until you stop it.

**Check it worked:** open the app. The Dashboard shows a section named **Local daemon**, and the **DAEMONS** counter at the top reads `1/1`. If the section says **Daemon disconnected** instead, no daemon is answering at the default address: start one, then press **Reconnect**. See [Dashboard → When a daemon is not there](dashboard.md#when-a-daemon-is-not-there) for the other states.

While the desktop app is connected it counts as a client, so the daemon stays up under it. [Installation → How the desktop app gets a daemon](../installation.md#how-the-desktop-app-gets-a-daemon) has the details.

The app looks for the daemon at the same default address the TUI uses. If you run the TUI with `DOT_AGENT_DECK_ATTACH_SOCKET` pointing at another socket, launch the app with the same variable in its environment.

## What you can do in it

The rail on the left has two entries, **Dashboard** and **Settings**, and the **Voice** button sits at the bottom of the window.

| Task | Where | Page |
| --- | --- | --- |
| See every agent on one or more daemons, with its status | **Dashboard** | [Dashboard](dashboard.md) |
| Open an agent's live terminal and type into it | Click its row on the Dashboard | [Dashboard → The agent pane](dashboard.md#the-agent-pane) |
| Start an agent, an orchestration, a dispatcher or a schedule-authoring agent | **New agent** on the Dashboard, or `Ctrl+N` / `⌘N` | [New Agent](new-agent.md) |
| Stop an agent, or every role of an orchestration | The stop control on a row, **Close** on an orchestration | [Dashboard → Closing agents and orchestrations](dashboard.md#closing-agents-and-orchestrations) |
| Watch a daemon on another machine over ssh | **Settings → Daemons** | [Daemons](daemons.md) |
| Upgrade a remote daemon that runs an older release than the app | **Upgrade** on its section of the Dashboard | [Daemons → Upgrade a remote daemon](daemons.md#upgrade-a-remote-daemon) |
| Change the appearance or the zoom level | **Settings → Appearance**, **Settings → Zoom** | [Settings](settings.md) |
| Drive the app by voice | **Voice** button, **Settings → Voice** | [Voice Control](voice.md) |

## What the TUI has and the desktop app does not

These are TUI-only:

- The [Schedules manager](../scheduled-tasks.md). The desktop app can start a schedule-authoring agent from **New agent**, but cannot list, edit or run schedules. Use the TUI or the `dot-agent-deck schedule` CLI.
- Generating `.dot-agent-deck.toml` with `g`, filtering agents with `/`, renaming an agent, and [customising keybindings](../keyboard-shortcuts.md).
- Saving and restoring the workspace, and the quit dialog's **Detach** / **Stop**.
- Installing the deck on a remote machine and starting a daemon there (`dot-agent-deck remote add`, `dot-agent-deck connect`). The desktop app connects to a remote daemon that is already running, and can [upgrade it](daemons.md#upgrade-a-remote-daemon) to the app's version once it does; see [Daemons](daemons.md).

The desktop app has two things the TUI does not: one Dashboard over several daemons at once, and voice control.

## Features behind the `experimental` flag

Some desktop surfaces are hidden unless the `experimental` flag is on, and these pages do not describe them. If you see one of these on someone else's screen and not on yours, that is why:

- The **Daemons** entry in the rail and the Dashboard's **Open daemons** button, which lead to a multi-pane screen. The **Start daemon**, **Stop** and **Replace daemon** controls, renaming an agent, the command palette (`Ctrl+K` / `⌘K`), the shortcut sheet (`?`) and the output **Reader** are on that screen, so they are hidden with it.
- The **Projects**, **Prompts**, **Orchestrations** and **Agent Profiles** entries in the rail.

**How the desktop app reads the flag.** From its own environment, once, when it starts: set `DOT_AGENT_DECK_EXPERIMENTAL=1` in the environment the app is launched from, or set `DOT_AGENT_DECK_FEATURES_CONFIG` to the path of a `.dot-agent-deck.toml` whose `[features]` table sets `experimental = true`. When both are set, `DOT_AGENT_DECK_EXPERIMENTAL` wins. Unlike the TUI and the daemon, the desktop app does not look for a `.dot-agent-deck.toml` in any directory, and it does not notice a change until it is restarted. An app launched from the macOS Dock or Finder, or from a Linux desktop menu, does not inherit variables exported in your shell profile; on Linux, start it from a terminal instead: `DOT_AGENT_DECK_EXPERIMENTAL=1 dot-agent-deck-desktop`.

One flag reaches the desktop app from the daemon instead: the **schedule: issues** chip in **New agent** appears only when the **daemon** you are creating the agent on has `experimental` on, as the TUI's does.

## When something goes wrong

- **Nothing on the Dashboard, or a daemon section with a title instead of agents:** see [Dashboard → When a daemon is not there](dashboard.md#when-a-daemon-is-not-there).
- **A remote daemon will not connect:** press **Test connection** in **Settings → Daemons** and follow the sentence it prints; [Daemons → Test connection](daemons.md#test-connection) lists every result.
- **The app says it cannot read its settings file:** see [Settings → The settings file](settings.md#the-settings-file).
- **Messages from the app itself:** the desktop app prints its diagnostics to standard error. On Linux, run `dot-agent-deck-desktop` from a terminal to see them.
- **Collecting a log for a bug report:** the app writes no log file of its own, so start the daemon it connects to with logging turned on. See [Troubleshooting → With the desktop app](../troubleshooting.md#with-the-desktop-app).
