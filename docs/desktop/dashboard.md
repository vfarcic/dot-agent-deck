# Dashboard

The **Agent dashboard** is the desktop app's main screen and the one it opens on: every agent on the daemons you are watching, one row each. It is the desktop counterpart of the TUI's dashboard, which shows the same agents as cards (see [Session Management](../session-management.md)).

![The desktop app's dashboard with four agents in one daemon section, each row showing its status, name and uptime](/img/dashboard-desktop.png)

## Check what is running

1. Choose what to watch with the **Daemon** selector under the title: **All daemons**, **This machine** (the default on a fresh install), or one remote daemon. Remote daemons are added in [Settings → Daemons](daemons.md).
2. Read the counters at the top. **AGENTS**, **RUNNING**, **WAITING**, **FAILED** and **GROUPS** count the agents on the daemons that answered. **DAEMONS** reads `<answered>/<shown>`, for example `1/1` for one healthy daemon. When a shown daemon has not answered, **DAEMONS** turns red and the other counters describe only the daemons that did. When no daemon answered, the other counters read `—`.
3. Read the rows. Each daemon has its own section (below); press **Refresh** to reconnect to the daemons and read their agent lists again. When there are more daemons and agents than fit the window, a scrollbar stays on the right edge: its length shows how much of the dashboard is on screen and its position shows where you are. Scroll with the mouse, the trackpad or the keyboard, or, with voice on, [by voice](voice.md#scrolling-the-dashboard).

## One section per daemon

Each daemon being shown gets its own section, headed with its name (**Local daemon** for this machine, the ssh user and host for a remote one) and its own **New agent** button, which creates the agent on that daemon. A remote daemon that runs an older release than the app also gets an **Upgrade** button there ([Daemons → Upgrade a remote daemon](daemons.md#upgrade-a-remote-daemon)). Inside a section, agents are grouped:

- **Standalone agents** (kicker **NO TAB**): agents that belong to no orchestration and no workspace mode.
- One group per orchestration (kicker **ORCHESTRATION**), named after the run. Its rows are numbered in role order, and the start role, the one you message, carries an **ORCHESTRATOR** badge.
- One group per workspace mode (kicker **MODE TAB**), named after the mode. You see one only while an agent started in a workspace mode by a release before 0.44.0 is still running; workspace modes were removed in 0.44.0, so neither client starts one.

Under the header, a **Host of this deck** panel shows free disk space, load per core, memory and the age of those figures for the machine that daemon runs on. A daemon from an older release says **Host metrics are not available from this deck** there instead. The TUI shows the same figures when you press `m`. [Session Management → Check the machine a deck runs on](../session-management.md#check-the-machine-a-deck-runs-on) explains each row.

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

## Filter the dashboard

To show only the agents you care about right now, use the filter at the top of the Dashboard:

1. Type into **Filter agents** to show only agents whose name, orchestration role, orchestration, working directory or last prompt contains what you typed. Case does not matter.
2. Press **Filter** to choose from four lists: **Kind** (orchestration roles, single agents, dispatchers, schedule agents or schedule: issues agents), **Status** (Working, Thinking, Waiting for input, Idle, Blocked, Error), **Agent type** (Claude Code, Codex, OpenCode, Pi, Devin) and **Daemon**. Tick as many entries as you like.

Dispatchers, schedule agents and schedule: issues agents are the agents started with the **dispatcher**, **schedule** and **schedule: issues** modes of [New agent](new-agent.md), in the desktop app or the TUI. An agent started that way by an earlier release of the deck is listed under single agents.

An agent is shown when it matches everything you chose: the text, and one of the ticked entries in each list you ticked something in. For example, ticking **Working** and **Thinking** under Status and **Codex** under Agent type shows the Codex agents that are working or thinking.

While a filter is set, a line under the header says how much of the fleet you are looking at, for example `Showing 4 of 11 agents · Working · Dispatchers`. Each part of the filter has its own remove control on that line, and **Show all** clears the whole filter in one click. The counters in the header (**AGENTS**, **RUNNING**, **WAITING**, **FAILED**, **GROUPS** and **DAEMONS**) always count the whole fleet, not only the agents the filter shows.

A daemon none of whose agents match shows one line in its place, for example `build-box: no matching agents`, and so does an orchestration or other group with no matching agents, so nothing disappears without a word.

The filter stays while you move between screens and lasts until you clear it or quit the app; the next time the app starts, the Dashboard shows every agent. You can also set and clear it [by voice](voice.md#filtering-the-dashboard). The TUI's dashboard has no filter.

![The Dashboard filtered to working agents: two daemons filtered to working agents: the line under the header reads Showing 3 of 6 agents · Working, with a remove control beside Working and a Show all button, while the counters above still count all six agents](/img/dashboard-filter-desktop.png)

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
2. Type into the terminal as you would into the TUI's pane. [Keys typed into an agent's terminal](settings.md#keys-typed-into-an-agents-terminal) says which keys reach the agent, including your platform's editing and paste shortcuts.
3. When the agent's output is longer than the terminal, a scrollbar stays on the terminal's right edge, so you can see that there is more above and how far back you are. Scroll up to read earlier output.
4. To copy the agent's output, select it with the mouse and press `Ctrl+Shift+C` (`⌘C` on macOS). Plain `Ctrl+C` still goes to the agent as an interrupt, even while text is selected.
5. Press **Back to dashboard**, or `Escape` when you are not typing in the terminal, to close the pane. The agent keeps running.

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
| **Daemon disconnected** | The app is not connected to that daemon, and the sentence under the title says why: no daemon is running there, a daemon is running but the app is not connected to it, or the app cannot tell, for example because the host cannot be reached. | Press the one button the section offers: **Start daemon** when no daemon is running, which asks first and names the machine it starts on, or **Reconnect** otherwise. See [Daemons → Start a daemon from the app](daemons.md#start-a-daemon-from-the-app), which also lists what a failed start says. |
| **Waiting for this daemon** | The app is still connecting to it. | Nothing: its agents appear when it answers. If it never does, press **Test connection** for it in [Settings → Daemons](daemons.md#test-connection). |
| **Establishing control channel** | The app is reading the daemon's agent list. | Nothing. |
| **Daemon not configured** | A remote daemon has no socket path yet. | Open [Settings → Daemons](daemons.md), choose it and press **Test connection**, which finds the path. |
| **Incompatible daemon** | A daemon answered, but it and the app are different versions, so the app will not use it. The message says which of the two is older, and either that the app has not connected because it could misread what the daemon reports, or that the two cannot work together at all. | Update the older of the two (see [Installation → Keep the app and the daemon on the same release](../installation.md#keep-the-app-and-the-daemon-on-the-same-release)). The note says what each of its buttons does: **Upgrade**, offered when a remote daemon is older than the app, installs the app's version there and restarts the daemon, asking before it stops any running agent ([Daemons → Upgrade a remote daemon](daemons.md#upgrade-a-remote-daemon)); **Open daemons** goes to the Daemons screen, where the daemon on this machine can be replaced, asking before it stops any running agent; **Connect anyway**, offered only when the message says the app could misread the daemon (never when the two cannot work together), uses the daemon as it is until you quit the app, though some of what it shows may be wrong; **Reconnect** tries again. **Technical details** under the message shows the exact versions, for a bug report. |
| **Desktop bridge error** | The app could not read the daemon's agents, and no daemon answered for it to judge. This is a problem on the app's side, not a version difference. | Press **Reconnect**. If it keeps happening, restart the app. |
| **No agents are running yet** | The daemon is healthy and runs no agents. This is what a fresh install looks like. | Press **New agent**. |

![The desktop app's dashboard with a healthy daemon and no agents: “No agents are running yet”](/img/dashboard-empty-desktop.png)
