# Desktop App

The desktop app is a graphical client of the same daemon the TUI talks to. The daemon owns every agent, whichever client started it: an agent started in the TUI appears on the desktop Dashboard, and one started from the desktop app appears as a card in the TUI. Quitting the desktop app leaves your agents running, as detaching the TUI does.

The desktop app is an **alpha**. Its downloads are named `dot-agent-deck-desktop-alpha-*` and are not covered by the support expectations of the CLI. Builds exist for macOS on Apple Silicon (`.dmg`) and Linux amd64 (`.deb`) only. [Installation → Desktop app](../installation.md#desktop-app) says what to download and how to install it.

## Before you start: a daemon

The desktop app is a client of a daemon. When no daemon is running on this machine, the app can start one: open the app, and the Dashboard's **Local daemon** section says **Daemon disconnected** and `No daemon is running on this machine.` with a **Start daemon** button. Press it and confirm, and the section connects. You can also start a daemon yourself before you open the app:

- Run the TUI, `dot-agent-deck`. It starts a daemon if none is running.
- Run `dot-agent-deck daemon serve` in a terminal. It runs in the foreground, and exits on its own about 30 seconds after it last had a client, an agent or an enabled schedule, so open the app within that window. `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0 dot-agent-deck daemon serve` keeps it up until you stop it.

**Check it worked:** the Dashboard shows a section named **Local daemon** listing its agents (or **No agents are running yet**), and the **DAEMONS** counter at the top reads `1/1`. A section that says **Daemon disconnected** offers one button: **Start daemon** when no daemon is running there, or **Reconnect** when a daemon is running but the app is not connected to it. See [Dashboard → When a daemon is not there](dashboard.md#when-a-daemon-is-not-there) for the other states.

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
| Start a daemon that is not running, on this machine or a remote one | **Start daemon** on its section of the Dashboard | [Daemons → Start a daemon from the app](daemons.md#start-a-daemon-from-the-app) |
| Upgrade a remote daemon that runs an older release than the app | **Upgrade** on its section of the Dashboard | [Daemons → Upgrade a remote daemon](daemons.md#upgrade-a-remote-daemon) |
| Upgrade the app, and the `dot-agent-deck` CLI on this machine, to a newer release | The upgrade button at the bottom of the rail, or **Upgrade…** on the Dashboard's banner | [Upgrade the app](#upgrade-the-app) |
| Change the appearance or the zoom level | **Settings → Appearance**, **Settings → Zoom** | [Settings](settings.md) |
| Drive the app by voice | **Voice** button, **Settings → Voice** | [Voice Control](voice.md) |

## What the TUI has and the desktop app does not

These are TUI-only:

- The [Schedules manager](../scheduled-tasks.md). The desktop app can start a schedule-authoring agent from **New agent**, but cannot list, edit or run schedules. Use the TUI or the `dot-agent-deck schedule` CLI.
- Generating `.dot-agent-deck.toml` with `g`, filtering agents with `/`, renaming an agent, and [customising keybindings](../keyboard-shortcuts.md).
- Saving and restoring the workspace, and the quit dialog's **Detach** / **Stop**.
- Installing the deck on a remote machine (`dot-agent-deck remote add`). Once it is installed, the desktop app can [start a daemon there](daemons.md#start-a-daemon-from-the-app) and [upgrade it](daemons.md#upgrade-a-remote-daemon) to the app's version.

The desktop app has two things the TUI does not: one Dashboard over several daemons at once, and voice control.

## Upgrade the app

The app checks for a newer release when it starts and every 6 hours while it runs. When this machine's app or its `dot-agent-deck` CLI is behind, two things appear:

- an upgrade button, an arrow in a circle, at the bottom of the rail, on every screen. Hovering it shows, for example, `Agent Deck (desktop app): update available: v0.47.0 (current: v0.46.0)`.
- a banner with the same text at the top of the Dashboard, with an **Upgrade…** button and a dismiss button. Dismissing it hides it until a newer release appears or the app restarts; the rail button stays.

Nothing appears while both copies are current, or while the app cannot reach GitHub. The TUI shows the same notice as `Update available: v<new> (current: v<yours>)` in its footer, and upgrades with `dot-agent-deck upgrade` in a terminal ([Installation → Upgrading](../installation.md#upgrading)), which shows the same plan as this dialog.

### The upgrade dialog

The rail button and **Upgrade…** open a dialog titled **Upgrade to v<new>**. It has one section for the app and, when a `dot-agent-deck` CLI is installed on this machine, one for the CLI. Each says which version you have, how that copy was installed, and what upgrading it does, including whether the download's build provenance will be checked (it is when the [GitHub CLI](https://cli.github.com/) is installed and logged in; the checksum is always checked). [Installation → What upgrading does for each install method](../installation.md#what-upgrading-does-for-each-install-method) lists every case.

![The Upgrade to v0.47.0 dialog over the Dashboard, whose banner reads Agent Deck (desktop app): update available: v0.47.0 (current: v0.46.0) beside an Upgrade… button, with the upgrade button lit at the bottom of the rail: one section for the app installed from a .dmg in /Applications, saying what upgrading downloads and checks, one for the dot-agent-deck CLI installed with Homebrew, and below them the question Upgrade Agent Deck (desktop app) to v0.47.0? with Cancel and Upgrade buttons](/img/self-upgrade-desktop.png)

1. The dialog asks about one copy at a time, the app first, for example `Upgrade Agent Deck (desktop app) to v0.47.0?`. Press **Upgrade**, or **Cancel**. Nothing changes until you press **Upgrade**: **Cancel**, `Escape` or a click outside the dialog before then close it having done nothing. After one copy is upgraded, **Cancel** skips the next copy instead.
2. While a copy upgrades, its section says **Upgrading…** and the dialog cannot be closed.
3. The result appears under that copy's section, then the dialog asks about the next copy, if any.
4. When nothing is left to ask, **Close** closes the dialog, and the app checks again.

A copy that cannot be upgraded from the app, such as a CLI installed with Nix or built from source, says what to do instead, and the dialog offers only **Close**. Most commands shown in the dialog have a copy button beside them.

### What you see on each outcome

| Copy | What you see | What to do |
| --- | --- | --- |
| The app, from a `.dmg` in a folder you can write | `Replaced <path> with v<new>. Quit and reopen Agent Deck to run it.`, and a **Relaunch** button | Press **Relaunch** to restart the app on the new version, or quit and reopen it later. Your agents keep running, as when you quit the app. |
| The app, from a `.dmg` in a folder you cannot write, or an unsigned app | Why it cannot be replaced, and `Download <link>, open it, and drag Agent Deck.app into <folder>, replacing the old one.` | Do that, then reopen the app. |
| The app, from the `.deb` | Your system's password prompt, then `Installed v<new>.` | Quit and reopen the app to run the new version. There is no **Relaunch** for the `.deb`. |
| The app, from the `.deb`, when you dismiss the password prompt or it fails | `` `pkexec …` failed: … ``, then `It was downloaded and checked, but not installed. Install it with:` and a `sudo apt install …` command | Run the command in a terminal, then quit and reopen the app. If the system has no graphical password prompt, the dialog shows the command straight away. |
| The CLI, from Homebrew | `` `brew upgrade dot-agent-deck` finished; it now reports v<new>. `` | Nothing. |
| The CLI, a downloaded binary | `Upgraded <path> to v<new>.`, or, in a directory you cannot write, the password prompt on Linux and then `Installed v<new>.` | Nothing. Where the password prompt is dismissed, fails or is not available, run the command the dialog shows. |

If **Relaunch** fails, the dialog shows why under its buttons; quit the app and open it again yourself. For any other failure, the copy's section shows the error and says whether anything changed; [Installation → When an upgrade fails](../installation.md#when-an-upgrade-fails) lists the messages and what to do.

Upgrading does not stop the daemon or its agents. If the Dashboard says **Incompatible daemon** after the app restarts, see [Installation → Keep the app and the daemon on the same release](../installation.md#keep-the-app-and-the-daemon-on-the-same-release). A remote daemon is not upgraded from this dialog; use its **Upgrade** button ([Daemons → Upgrade a remote daemon](daemons.md#upgrade-a-remote-daemon)).

## Features behind the `experimental` flag

Some desktop surfaces are hidden unless the `experimental` flag is on, and these pages do not describe them. If you see one of these on someone else's screen and not on yours, that is why:

- The **Daemons** entry in the rail and the Dashboard's **Open daemons** button, which lead to a multi-pane screen. The **Stop** and **Replace daemon** controls, renaming an agent, the command palette (`Ctrl+K` / `⌘K`), the shortcut sheet (`?`) and the output **Reader** are on that screen, so they are hidden with it.
- The **Projects**, **Prompts**, **Orchestrations** and **Agent Profiles** entries in the rail.

**How the desktop app reads the flag.** From its own environment, once, when it starts: set `DOT_AGENT_DECK_EXPERIMENTAL=1` in the environment the app is launched from, or set `DOT_AGENT_DECK_FEATURES_CONFIG` to the path of a `.dot-agent-deck.toml` whose `[features]` table sets `experimental = true`. When both are set, `DOT_AGENT_DECK_EXPERIMENTAL` wins. Unlike the TUI and the daemon, the desktop app does not look for a `.dot-agent-deck.toml` in any directory, and it does not notice a change until it is restarted. An app launched from the macOS Dock or Finder, or from a Linux desktop menu, does not inherit variables exported in your shell profile; on Linux, start it from a terminal instead: `DOT_AGENT_DECK_EXPERIMENTAL=1 dot-agent-deck-desktop`.

One flag reaches the desktop app from the daemon instead: the **schedule: issues** chip in **New agent** appears only when the **daemon** you are creating the agent on has `experimental` on, as the TUI's does.

## When something goes wrong

- **Nothing on the Dashboard, or a daemon section with a title instead of agents:** see [Dashboard → When a daemon is not there](dashboard.md#when-a-daemon-is-not-there).
- **A remote daemon will not connect:** press **Test connection** in **Settings → Daemons** and follow the sentence it prints; [Daemons → Test connection](daemons.md#test-connection) lists every result.
- **The app says it cannot read its settings file:** see [Settings → The settings file](settings.md#the-settings-file).
- **Messages from the app itself:** the desktop app prints its diagnostics to standard error. On Linux, run `dot-agent-deck-desktop` from a terminal to see them.
- **Collecting a log for a bug report:** the app writes no log file of its own, so start the daemon it connects to with logging turned on. See [Troubleshooting → With the desktop app](../troubleshooting.md#with-the-desktop-app).
