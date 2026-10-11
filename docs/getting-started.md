# Getting Started

This page takes you from nothing to one agent running in the deck, then points to the features you set up next. The steps say how to check that they worked.

## How the pieces fit

- **The daemon** runs in the background and owns the agents: their processes, terminals and statuses. The first `dot-agent-deck` run starts it; you do not start it yourself.
- **The TUI** (`dot-agent-deck`) is a terminal client of that daemon. Quitting it can leave the agents running.
- **The desktop app** (alpha) is a second client of the same daemon. An agent started from either client shows up in both. It covers the dashboard, starting agents, several daemons at once, settings and voice control; see [Desktop app](desktop/index.md). It can start a daemon itself; this guide starts with the TUI.

The deck tracks the status of five agents: [Claude Code](https://www.anthropic.com/claude-code) (`claude`), [OpenCode](https://opencode.ai) (`opencode`), [Pi](https://github.com/earendil-works/pi) (`pi`), [Codex](https://github.com/openai/codex) (`codex`) and [Devin](https://devin.ai) (`devin`). A pane can run any other command too, without status tracking.

## Step 1: Install

**macOS or Linux with Homebrew:**

```bash
brew tap vfarcic/tap && brew install dot-agent-deck
```

**Linux without Homebrew** (use `arm64` in place of `amd64` on ARM):

```bash
mkdir -p ~/.local/bin
curl -fsSL -o ~/.local/bin/dot-agent-deck \
  https://github.com/vfarcic/dot-agent-deck/releases/latest/download/dot-agent-deck-linux-amd64
chmod +x ~/.local/bin/dot-agent-deck
```

**macOS without Homebrew:** download `dot-agent-deck-darwin-arm64` (Apple silicon) or `dot-agent-deck-darwin-amd64` (Intel) the same way; see [Download Binary](installation.md#download-binary).

**Windows:** native Windows is not supported ([#164](https://github.com/vfarcic/dot-agent-deck/issues/164)). Install [WSL](https://learn.microsoft.com/en-us/windows/wsl/install) and follow the Linux steps inside it.

[Installation](installation.md) has the other methods (Nix, building from source) and the desktop app.

**Check:**

```bash
dot-agent-deck --version    # prints: dot-agent-deck <version>
```

If this says `command not found` after the Linux download, add `export PATH="$HOME/.local/bin:$PATH"` to your shell's rc file and open a new shell.

`dot-agent-deck docs` lists the documentation built into this binary, and `dot-agent-deck docs <topic>` prints a page, for example `dot-agent-deck docs orchestration`. That copy matches the installed version. If `dot-agent-deck docs` reports an unrecognized subcommand, the installed version predates it: read the documentation at [agent-deck.devopstoolkit.ai/llms.txt](https://agent-deck.devopstoolkit.ai/llms.txt) instead, which follows the latest release rather than your installed version.

## Step 2: Launch the deck

```bash
dot-agent-deck
```

On startup the deck installs its status hooks for the agents it detects (see [Installation → Agent hooks](installation.md#agent-hooks)) and restores your previous workspace, if you had one (see [Resuming Sessions](session-management.md#resuming-sessions)).

**Check:** on a first run the TUI shows an empty dashboard reading `No active agents. Press Ctrl+n to create an agent.`, with a row of buttons along the bottom and a ` COMMAND ` chip at its left.

![The TUI with no agents: “No active agents. Press Ctrl+n to create an agent.” above the command bar, which starts with a COMMAND chip](/img/dashboard-empty-tui.png)

**Desktop app instead:** install it ([Installation → Desktop app](installation.md#desktop-app)), keep a daemon running (the TUI above is enough; see [How the desktop app gets a daemon](installation.md#how-the-desktop-app-gets-a-daemon)) and open **Agent Deck**. **Check:** the Dashboard says **No agents are running yet** and offers **New agent**. If it says **Daemon disconnected** with **Start daemon**, no daemon is running; press **Start daemon** and confirm.

![The desktop app's dashboard with no agents: “No agents are running yet” and a New agent button](/img/dashboard-empty-desktop.png)

## Step 3: Start an agent

**TUI:**

1. Press `Ctrl+n`. A directory picker opens.
2. Move with `j`/`k` (or the arrow keys), open the highlighted directory with `l` or `Enter`, and go up with `h`. When you are inside the directory the agent should work in, press `Space` to choose it. (`Enter` on a directory with no subdirectories also chooses it.)
3. In the **New Agent** form, leave **Mode** on `No mode` and press `Enter` to move to **Name** (pre-filled from the directory), then `Enter` again to move to **Command**. Type the command, for example `claude`, and press `Enter` to submit. `Tab` / `Shift+Tab` also move between fields, and `Esc` cancels.

![The TUI's New Agent form over the dashboard: the chosen directory at the top, a Mode row with No mode selected and an orchestration, schedule and dispatcher as the other choices, then the Name field pre-filled from the directory, an empty Command field, and Submit and Cancel](/img/new-agent-tui.png)

**Desktop:**

1. Click **New agent** (or press `Ctrl+N` / `⌘N` on the Dashboard).
2. Choose the daemon, browse to a directory and press **Use this directory**.
3. Leave **Mode** on **No mode**, check **Name** and **Command** (pre-filled from `default_command` or your last command), and press **Create agent**. See [Desktop app → New agent](desktop/new-agent.md).

![The desktop app's New agent dialog over the Dashboard: the Local daemon chosen under Daemon, a directory chosen in the browser, the Mode chips with No mode selected, the Name pre-filled from the directory, an empty Command field, and Discard and Create agent](/img/new-agent-desktop.png)

**Check:** a card (TUI) or row (desktop) appears for the agent. After you give the agent a prompt, its status moves from **Idle** to **Thinking** or **Working** (desktop: **idle** to **running**). If the status never changes while the agent visibly works, its hooks are not reaching the daemon; see [Troubleshooting → Hooks](troubleshooting.md#hooks). If the pane shows a `command not found` error for a bare `claude`, `codex` and so on, see [Troubleshooting](troubleshooting.md#a-bare-command-like-claude-opencode-pi-codex-or-devin-fails-to-spawn).

**TUI:**

![The TUI with four agents: a column of agent cards on the left, each with its status (Idle, Working, Needs Input), directory and last prompt, and the focused agent's terminal pane on the right](/img/dashboard-tui.png)

**Desktop:**

![The desktop app's dashboard with four agents in one daemon section, each row showing its status (running or waiting), name and uptime, and New agent, Columns and Refresh at the top](/img/dashboard-desktop.png)

## Step 4: Type into the agent, and come back

**TUI:** the deck has two modes. In **command mode** (the bottom-left chip reads ` COMMAND `) keys drive the deck; in the pane (the chip reads ` TYPING `) keys go to the agent.

- `Ctrl+d` switches between the two.
- In command mode, `j`/`k` select a card, `Enter` or `1`–`9` focus a pane, and `?` shows every shortcut.
- `Ctrl+t` switches between showing only the focused pane (stacked, the default) and showing every pane (tiled).
- Everything is clickable too: the buttons along the bottom carry their shortcuts.

**Desktop:** click an agent's row to open its terminal full-window; `Escape` or **Back to dashboard** returns.

[Keyboard Shortcuts](keyboard-shortcuts.md) lists every key and mouse action.

## Step 5: Close an agent, or quit

- **Close one agent (TUI):** in command mode, select its card and press `Ctrl+w`, then choose **Close**. Inside a pane `Ctrl+w` is the shell's delete-word and closes nothing.
- **Close one agent (desktop):** use the stop control on its row (`Close <name> agent`) and confirm with **Close agent**.
- **Quit the TUI:** in command mode press `Ctrl+c` and choose **Detach** (agents keep running; the next `dot-agent-deck` shows them again) or **Stop** (stops the agents and the daemon).

**Check:** after **Detach**, `dot-agent-deck daemon status` lists your agents with their status. After **Stop**, it prints `daemon status: unavailable (…)` and exits 1.

## How it runs

The daemon outlives the TUI: detach, close the terminal or lose an ssh connection, and the agents keep running. Running `dot-agent-deck` again reattaches. Agents keep running until they exit by themselves or something stops them, for example closing them, quitting with **Stop**, `dot-agent-deck daemon stop --force`, restarting the daemon onto a new binary (the TUI asks first when agents are running; see [Upgrading](installation.md#upgrading)), or the machine shutting down.

About 30 seconds after the last client disconnects, if no agent is running and no enabled schedule is registered, the daemon exits by itself. `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS` sets that window in seconds (`0` keeps the daemon up until it is stopped). See [Configuration](configuration.md) for the other settings.

## Next steps

This page stops at one running agent. Most setups want more than that, so if the goal is larger, continue with the page for it: several agents working together (for example a coder, a reviewer and an orchestrator that hands them work) is [Orchestration](orchestration.md).

| Goal | Where |
|---|---|
| Run a team of agents where one coordinates the others | [Orchestration](orchestration.md). In the TUI, select an agent's card in command mode and press `g` to have that agent draft a `.dot-agent-deck.toml` for its directory. |
| Start isolated background work by asking a dispatcher pane | [Dispatcher Mode](dispatcher-mode.md) |
| Run a prompt on a cron schedule | [Schedules](scheduled-tasks.md) |
| Run agents on another machine over ssh | [Remote Environments](remote-environments.md) |
| Understand what each status means, and resume after a reboot | [Session Management](session-management.md) |
| Set a default command, bells, or other settings | [Configuration](configuration.md) |
| Fix something that is not working | [Troubleshooting](troubleshooting.md) |
