# Dashboard

The **Agent dashboard** is the desktop app's main screen and the one it opens on: every agent on the daemons you are watching, one row each. It is the desktop counterpart of the TUI's dashboard, which shows the same agents as cards (see [Session Management](../session-management.md)).

![The desktop app's dashboard with four agents in one daemon section, each row showing its status, name and uptime](/img/dashboard-desktop.png)

## Check what is running

1. Choose what to watch with the **Daemon** selector under the title: **All daemons**, **This machine** (the default on a fresh install), or one remote daemon. Remote daemons are added in [Settings → Daemons](daemons.md).
2. Read the counters at the top. **AGENTS**, **RUNNING**, **WAITING**, **FAILED** and **GROUPS** count the agents on the daemons that answered. **DAEMONS** reads `<answered>/<shown>`, for example `1/1` for one healthy daemon. When a shown daemon has not answered, **DAEMONS** turns red and the other counters describe only the daemons that did. When no daemon answered, the other counters read `—`.
3. Read the rows. Each daemon has its own section (below); press **Refresh** to reconnect to the daemons and read their agent lists again.

## One section per daemon

Each daemon being shown gets its own section, headed with its name (**Local daemon** for this machine, the ssh user and host for a remote one) and its own **New agent** button, which creates the agent on that daemon. Inside a section, agents are grouped:

- **Standalone agents** (kicker **NO TAB**): agents that belong to no orchestration and no mode tab.
- One group per orchestration (kicker **ORCHESTRATION**), named after the run. Its rows are numbered in role order, and the start role, the one you message, carries an **ORCHESTRATOR** badge.
- One group per workspace-mode tab started from the TUI (kicker **MODE TAB**).

Each group header shows how many agents it holds, how many are in each status, and the working directory most of its agents share. A row shows its own directory only when it differs from that.

![The Dashboard with two daemons, each in its own section with its own New agent button: Local daemon with four agents and a remote daemon, dev@build-box, with two; the DAEMONS counter reads 2/2 and the other counters add up the agents of both](/img/dashboard-fleet-desktop.png)

## Rows and columns

By default a row shows **Status**, **Agent**, **Uptime** and **Working directory**. **Columns** adds or removes any of these:

| Column | What it shows |
| --- | --- |
| **Status** | The agent's status (below), with its colour mark. |
| **Agent** | The agent's name, and its role in an orchestration. Cannot be removed. |
| **Last activity** | How long ago the daemon last saw the agent do anything. Blank when the daemon has not reported it, for example for agents under a restarted daemon. |
| **Uptime** | How long the agent's process has been running. For an orchestration worker that was restarted, the age of its current process. |
| **CLI** | The program the agent runs, as the daemon reports it (`claude`, `codex`, …). |
| **Active tool** | The tool the agent is running now and a short form of its argument, or `no active tool`. |
| **Tools** | How many tool calls the agent has reported. |
| **Working directory** | Where the agent runs. |
| **Last prompt** | The last prompt sent to the agent. |

**Restore defaults** in the Columns menu goes back to the four default columns. The choice is remembered by the app on this computer, in the app's own storage, not in the [settings file](settings.md#the-settings-file). The TUI has no column choice: its cards pick their density from the terminal's size.

## Statuses

The desktop app shows four statuses. Each covers one or more of the words the TUI shows on its cards:

| Desktop | TUI | Meaning |
| --- | --- | --- |
| **RUNNING** | Thinking, Working, Compacting | The agent is busy. An agent that has reported no status at all yet, for example a plain shell or an agent whose hooks are not installed, also shows **RUNNING**. |
| **WAITING** | Needs Input, Idle | The agent is waiting: for your approval or input, or for its next task. The desktop app does not tell the two apart. A status this build does not recognise, from a newer daemon, also shows **WAITING**. |
| **FAILED** | Error | Something went wrong. |
| **BLOCKED** | Blocked | The agent's provider refused it because a usage limit or credit pool is exhausted. Open the agent's pane to see which limit, and when it resets if the provider says. |

What each status means, and which agents can report Blocked, is on [Session Management → Session Statuses](../session-management.md#session-statuses). An agent that stays **RUNNING** when it is clearly idle usually has no hooks reporting to the daemon; see [Troubleshooting → Hooks](../troubleshooting.md#hooks).

## The agent pane

To watch or talk to one agent:

1. Click its row, or its open control (`Open <name> agent`). Its live terminal opens in a full-window pane over the Dashboard.
2. Type into the terminal as you would into the TUI's pane.
3. To copy the agent's output, select it with the mouse and press `Ctrl+Shift+C` (`⌘C` on macOS). Plain `Ctrl+C` still goes to the agent as an interrupt, even while text is selected.
4. Press `Escape`, or **Back to dashboard**, to close the pane. The agent keeps running.

The TUI copies differently: a mouse drag in a pane copies when you release the button, and whether that reaches your clipboard depends on your terminal.

The pane's header names the agent by its type (for example Codex), or by its role when it belongs to an orchestration, rather than by the display name its row shows. Beside that is its status, and below it the agent's command and its model, which reads `Unavailable` because the daemon does not report one.

Between the header and the terminal, the pane shows the agent's assignment and two readings: **TIME**, how long the agent has been running (hover it for the exact time it started), and **TOOLS**, how many tool calls it has reported. TIME reads `—` when the daemon did not say when it started the agent.

![An agent's full-window pane: the header reads Codex with a RUNNING status and codex · Unavailable below it, with a close control on the right; under it the agent's assignment, a row of time, token, cost and context readings, then the Terminal tab showing the agent's live output: the files it read and edited, a test run that passed, and its summary](/img/agent-pane-desktop.png)

What can go wrong in the pane:

- **"No terminal here: the desktop has no live connection to <daemon>…"**: the agent's daemon stopped answering. The header then shows the last status the app saw. The terminal comes back on its own when the daemon answers again.
- **"Terminal input unavailable — <reason>."**: the daemon refused input to this pane, and the reason says why. What you type is not sent.
- **"Delivery was not confirmed — <reason>."**: the input may or may not have reached the agent. Check the terminal before typing it again.

## Closing agents and orchestrations

- **Stop one agent:** press the stop control on its row (`Close <name> agent`). The confirmation, `Close <name>?`, names the daemon the stop request goes to. Press **Close agent**.
- **Stop every role of an orchestration:** press **Close** on its group header. The confirmation lists every role it will stop. Press **Close all N roles** (or **Close 1 role**).

**Check it worked:** the row, or the whole group, disappears from the section. If the app cannot confirm that some roles stopped, it says how many may still be running on which daemon, in a message on the Dashboard and in the New agent dialog; open the TUI against that daemon, or run `dot-agent-deck daemon status` on its host, to see what is still there.

In the TUI the same things are `Ctrl+w` on a card, and closing an orchestration's tab.

## When a daemon is not there

A daemon section shows one of these instead of its agents:

| Title | What it means | What to do |
| --- | --- | --- |
| **Daemon disconnected** | Nothing is answering at that daemon's address. | Start a daemon (see [Installation → How the desktop app gets a daemon](../installation.md#how-the-desktop-app-gets-a-daemon)), then press **Reconnect**. For a remote daemon, see [Daemons → What a remote daemon must already have](daemons.md#what-a-remote-daemon-must-already-have). |
| **Waiting for this daemon** | The app is still connecting to it. | Nothing: its agents appear when it answers. If it never does, press **Test connection** for it in [Settings → Daemons](daemons.md#test-connection). |
| **Establishing control channel** | The app is reading the daemon's agent list. | Nothing. |
| **Daemon not configured** | A remote daemon has no socket path yet. | Open [Settings → Daemons](daemons.md), choose it and press **Test connection**, which finds the path. |
| **Incompatible daemon** | A daemon answered, but it and the app are different versions, so the app will not use it. The message says which of the two is older, and either that the app has not connected because it could misread what the daemon reports, or that the two cannot work together at all. | Update the older of the two (see [Installation → Keep the app and the daemon on the same release](../installation.md#keep-the-app-and-the-daemon-on-the-same-release)). The note says what each of its buttons does: **Open daemons** goes to the Daemons screen, where a daemon on this machine with no running agents can be replaced; **Connect anyway**, offered only when the message says the app could misread the daemon (never when the two cannot work together), uses the daemon as it is until you quit the app, though some of what it shows may be wrong; **Reconnect** tries again. **Technical details** under the message shows the exact versions, for a bug report. |
| **Desktop bridge error** | The app could not read the daemon's agents, and no daemon answered for it to judge. This is a problem on the app's side, not a version difference. | Press **Reconnect**. If it keeps happening, restart the app. |
| **No agents are running yet** | The daemon is healthy and runs no agents. This is what a fresh install looks like. | Press **New agent**. |

![The desktop app's dashboard with a healthy daemon and no agents: “No agents are running yet”](/img/dashboard-empty-desktop.png)
