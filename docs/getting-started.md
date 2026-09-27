---
sidebar_position: 2
title: Getting Started
---

import Tabs from '@theme/Tabs';
import TabItem from '@theme/TabItem';

# Getting Started

Agent Deck has two clients: a terminal UI (the TUI, `dot-agent-deck`) and a desktop app. Both are clients of the same background daemon, which owns the agents, so an agent started from one shows up in the other. The desktop app is an alpha; it covers the Dashboard, New agent, several daemons at once, Settings and voice control (see [Desktop app](desktop/index.md)). The desktop app needs a running daemon, and the simplest way to get one is to install and run the TUI, so this page starts there.

## Quick Start

### macOS

```bash
# 1. Install via Homebrew
brew tap vfarcic/tap && brew install dot-agent-deck

# 2. Launch the dashboard (hooks are auto-installed for detected agents)
# Your previous workspace is restored automatically
dot-agent-deck
```

No Homebrew? Download the binary instead — `dot-agent-deck-darwin-arm64` for Apple Silicon, `dot-agent-deck-darwin-amd64` for Intel. See [Download Binary](installation.md#download-binary).

### Linux

```bash
# 1. Download the binary. Swap `amd64` for `arm64` on ARM machines.
mkdir -p ~/.local/bin
curl -fsSL -o ~/.local/bin/dot-agent-deck \
  https://github.com/vfarcic/dot-agent-deck/releases/latest/download/dot-agent-deck-linux-amd64
chmod +x ~/.local/bin/dot-agent-deck

# 2. Launch the dashboard (hooks are auto-installed for detected agents)
# Your previous workspace is restored automatically
dot-agent-deck
```

If `dot-agent-deck` comes back "command not found", `~/.local/bin` is not on your `PATH` — add `export PATH="$HOME/.local/bin:$PATH"` to your shell rc. [Homebrew](installation.md#homebrew-macos--linux) and [Nix](installation.md#nix) work on Linux too if you already use either.

### Desktop app

The desktop app is a separate download: a signed `.dmg` for Apple silicon Macs and a `.deb` for Linux amd64. See [Installation → Desktop app](installation.md#desktop-app) for which file to pick, how to install it, and how it gets a daemon to connect to.

### Windows

Native Windows is [not supported yet](https://github.com/vfarcic/dot-agent-deck/issues/164). For now, install [WSL](https://learn.microsoft.com/en-us/windows/wsl/install) and follow the Linux instructions inside your WSL shell.

> **Tip:** [Installation](installation.md) has every option side by side — Homebrew, a downloaded binary, Nix, and building from source — with what each one suits.

Once the dashboard is running, press `?` inside the app to see all shortcuts. The dashboard is also fully mouse-clickable: a button bar along the bottom exposes the main commands (each labelled with its keyboard shortcut), and cards, tab headers, dialogs, the directory picker, and forms all respond to clicks. See [Keyboard Shortcuts → Mouse](keyboard-shortcuts.md#mouse).

> On launch, dot-agent-deck automatically sets up live status, tool, and prompt tracking for the agents it detects — [Claude Code](https://www.anthropic.com/claude-code), [OpenCode](https://opencode.ai), [Pi](https://github.com/earendil-works/pi), [Codex](https://github.com/openai/codex), and [Devin](https://devin.ai). No configuration is needed. See [Troubleshooting](troubleshooting.md#hooks) if you want to manage this manually.

## Launching

<Tabs groupId="client">
<TabItem value="tui" label="TUI">

Running `dot-agent-deck` opens a two-column layout with native embedded terminal panes:

- **Left (1/3)** — the dashboard, displaying a card grid of agents
- **Right (2/3)** — agent panes where Claude Code, OpenCode, Pi, Codex, or Devin instances run (stacked by default — only the focused pane is shown, at full height; toggle to tiled with `Ctrl+t` to see every pane at once)

![The TUI with four agents: a column of agent cards on the left, each with its status (Idle, Working, Needs Input), directory and last prompt, and the focused agent's terminal pane on the right](/img/dashboard-tui.png)

On a first run there are no agents yet, and the dashboard says so:

![The TUI with no agents: “No active sessions. Press Ctrl+n to create a pane.” above the command bar](/img/dashboard-empty-tui.png)

</TabItem>
<TabItem value="desktop" label="Desktop">

Opening the desktop app shows the **Agent dashboard**: every agent as a row, with its status, grouped by daemon and by orchestration. It connects to the daemon on this machine and does not start one; if none is running, the Dashboard says **Daemon disconnected** (see [Installation → How the desktop app gets a daemon](installation.md#how-the-desktop-app-gets-a-daemon)).

![The desktop app's dashboard with four agents in one daemon section, each row showing its status (running or waiting), name and uptime, and New agent, Columns and Refresh at the top](/img/dashboard-desktop.png)

On a first run the daemon is healthy and owns no agents, and the Dashboard says **No agents are running yet**, with a **New agent** button:

![The desktop app's dashboard with no agents: “No agents are running yet” and a New agent button](/img/dashboard-empty-desktop.png)

Clicking a row opens that agent's terminal in a full-window pane; see [Desktop app → Dashboard](desktop/dashboard.md).

</TabItem>
</Tabs>

## How it runs

The deck is a small background daemon with clients on top of it: the TUI and the desktop app. The first `dot-agent-deck` invocation auto-spawns the daemon and connects to it over a per-user Unix socket — you don't have to start anything manually, and you don't have to clean anything up. The same daemon backs both local runs and `dot-agent-deck connect` (remote) sessions; there is no separate "local mode".

The daemon owns the agent processes, which has one user-facing consequence: closing the TUI is a *detach*, not a kill. Your agents keep running. Reattach with `dot-agent-deck` later and the dashboard rehydrates with the agents still in their previous state. Detach, sleep, a network drop, or switching machines — none of them stop your agents, because the daemon outlives the TUI in every case. The only thing that stops a running agent is *you* choosing to upgrade-and-restart the daemon onto a new binary version, and even then you are asked first (see [Upgrading](installation.md#upgrading)).

The desktop app attaches to that same daemon as a second client; it does not start one itself (see [Installation → How the desktop app gets a daemon](installation.md#how-the-desktop-app-gets-a-daemon)).

About 30 seconds after every client (the TUI, the desktop app) has disconnected and every managed agent is gone, the daemon exits on its own and the socket is cleaned up. Override the window with `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS` (in seconds; set `0` to disable idle shutdown and keep the daemon up indefinitely).

## Basic Workflow

<Tabs groupId="client">
<TabItem value="tui" label="TUI">

1. Launch the dashboard with `dot-agent-deck`
2. Press `Ctrl+n` to open the **New Agent** form — pick a directory, give the agent a name, and enter the command to run (typically `claude`, `opencode`, `pi`, `codex`, or `devin`)
3. Watch the agent's status, tool calls, and prompts update on the dashboard in real-time
4. To type into an agent, move keyboard focus into its pane: press `Ctrl+d` to enter command mode, then either `j`/`k` (or `Down`/`Up`) to cycle through cards or `1`–`9` to jump directly to a card
5. To close a pane, press `Ctrl+d` to leave it, then `Ctrl+w` on the selected card and choose **Close** in the confirmation. While you're typing inside a pane, `Ctrl+w` is the shell's ordinary delete-previous-word — it never closes anything. The dashboard tab itself can't be closed.

> **Tip:** The command can be any shell command, but real-time status, tool, and prompt tracking on the dashboard work for `claude`, `opencode`, `pi`, `codex`, and `devin`.

> **Tip:** `Ctrl+d` toggles: press it in a pane to enter command / navigation mode, press it again to go back to the pane.

</TabItem>
<TabItem value="desktop" label="Desktop">

1. Open the app. The **Agent dashboard** lists the agents of the daemon it is connected to.
2. Click **New agent** (or press `Ctrl+N` / `⌘N` on the Dashboard). Choose a daemon, browse to a directory and press **Use this directory**, leave **Mode** on **No mode**, then give the agent a **Name** and a **Command** (typically `claude`, `opencode`, `pi`, `codex`, or `devin`; it is pre-filled from your `default_command` or last command) and press **Create agent**. See [Desktop app → New agent](desktop/new-agent.md).
3. Watch the agent's row: its status, and whichever columns you chose with **Columns**, update as the agent works.
4. To type into an agent, click its row. Its terminal opens in a full-window pane over the Dashboard; press `Escape` or **Back to dashboard** to return.
5. To close an agent, use the stop control on its row (`Close <name> agent`) and confirm with **Close agent**.

</TabItem>
</Tabs>

## Orchestration

Orchestrations let you run a pipeline of AI agents where a designated orchestrator coordinates work across specialist workers — a coder, a reviewer, an auditor, a release agent, or any roles that fit your workflow. Each worker runs in its own pane with its own model and instructions, working independently and reporting back when done. You set the pipeline up once in `.dot-agent-deck.toml` and the deck handles the rest.

The fastest way to get the config is to let an agent generate it: press `Ctrl+d` then `g` on the dashboard, choose **Yes**, and the agent analyzes your project and proposes a config with suitable roles. Treat the result as a starting point and tune it as you learn what works for your project.

Once you have a config, starting an orchestration is the same as starting any other agent. In the TUI it opens an orchestration tab:

1. Press `Ctrl+n`.
2. Navigate to the project directory that contains `.dot-agent-deck.toml` with `[[orchestrations]]`.
3. Cycle the **Mode** field (`Left`/`Right` or `h`/`l`) until the orchestration name appears.
4. Press `Enter` — the deck opens a tab with a pane for every role.

![Orchestration tab on launch — five role cards stacked in the sidebar (orchestrator working, coder, reviewer, auditor and release idle), with the focused orchestrator pane filling the right-hand side](./img/orchestration-start.png)

In the desktop app, choose the project directory in **New agent**, pick its `Orch: <name>` chip under **Mode**, and press **Activate orchestration**; the Dashboard then shows the run as one group. Generating the config with `g` is TUI-only.

For the full reference, examples, and configuration options, see [Orchestration](orchestration.md).

## Working with Modes

Modes are a TUI feature. They let you pair an agent with live command output in a tabbed workspace — useful for keeping test runners, log streams, or kubectl output visible alongside your agent. They are defined per-project in `.dot-agent-deck.toml`.

![A mode tab in action — agent pane on the left, with live Git status, kubectl pods, and kubectl events stacked on the right](./img/modes.png)

To set one up, let an agent generate the config (`Ctrl+d` then `g`), run `dot-agent-deck init` for a starter template, or write `[[modes]]` blocks manually. Then press `Ctrl+n`, navigate to the project directory, cycle the **Mode** field to your mode name, and press `Enter`.

For the full configuration reference and more examples, see [Workspace Modes](workspace-modes.md).

## Dispatching Work in the Background

Dispatcher mode lets you start work without stopping what you are doing. Tell a dispatcher pane what you want started — "work on the search bug" — and it sets up a separate, isolated copy of your repository and puts an agent, or a whole team, to work there. Start several and they run in parallel without colliding with each other or with your working tree.

Press `Ctrl+n`, navigate to the project directory, cycle the **Mode** field to **dispatcher**, and press `Enter`. Then just ask. In the desktop app, pick the **dispatcher** chip under **Mode** in **New agent**.

For the full reference — choosing one agent or a team, watching the units, and cleanup, see [Dispatcher Mode](dispatcher-mode.md).

## Schedules

Schedules let the daemon spawn an agent (or run a command) on a cron schedule — a nightly review, a recurring digest, a periodic health check — without you being at the keyboard. They are defined globally, so they apply across every project.

The fastest way to create one is to let an agent author it: press `Ctrl+n`, cycle the **Mode** field to **schedule**, and the throwaway pane walks you through building the entry. Or press `s` on the dashboard to open the **Schedules** manager and choose `[Add a]`. The desktop app's **New agent** offers the same **schedule** chip, and has no Schedules manager. Every schedule needs a command that launches a `claude`, `opencode`, `pi`, `codex`, or `devin` agent — directly (`claude`, `opencode`, `pi`, `codex`, `devin`) or via a wrapper like `devbox run agent-new` — which is what gives the run full status tracking.

For the full reference — cron syntax, the global config file, tab reuse, and supervisor recipes — see [Schedules](scheduled-tasks.md).
