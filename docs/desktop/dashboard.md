---
title: Dashboard
---

# Dashboard

The **Agent dashboard** is the desktop app's main screen: every agent on the daemons you are watching, one row each. It is the desktop counterpart of the TUI's dashboard, which shows the same agents as cards (see [Session Management](../session-management.md)).

![The desktop app's dashboard with four agents in one daemon section, each row showing its status, name and uptime](/img/dashboard-desktop.png)

## The top bar

- The **Daemon** selector, under the title, chooses which daemons the Dashboard shows: **All daemons**, **This machine**, or one remote daemon you added in Settings. See [Daemons](daemons.md).
- The counters: **AGENTS**, **RUNNING**, **WAITING**, **FAILED** and **GROUPS** count the agents on the daemons that answered, and **DAEMONS** shows how many of the daemons being shown answered, out of how many (`1/1` for one healthy daemon). When some daemon has not answered, **DAEMONS** turns red, and the other counters describe only the ones that did.
- **New agent** opens the [New agent](new-agent.md) dialog. `Ctrl+N` or `⌘N` does the same while the Dashboard is showing and no agent pane is open.
- **Columns** chooses which columns the rows show (below).
- **Refresh** reconnects to the daemons and reads their agent lists again.

## One section per daemon

Each daemon being shown gets its own section, headed with its name and its own **New agent** button, which creates the agent on that daemon. Inside a section, agents are grouped:

- **Standalone agents** (kicker **NO TAB**) — agents that belong to no orchestration or mode tab.
- One group per orchestration (kicker **ORCHESTRATION**), named after the run. Its rows are numbered in role order, and the start role, the one you message, carries an **ORCHESTRATOR** badge.
- One group per workspace-mode tab started from the TUI (kicker **MODE TAB**).

Each group header shows how many agents it holds and how many of them are in each status, and the working directory most of its agents share; a row then shows its own directory only when it differs.

![The Dashboard with two daemons, each in its own section with its own New agent button: Local daemon with four agents and a remote daemon, dev@build-box, with two; the DAEMONS counter reads 2/2 and the other counters add up the agents of both](/img/dashboard-fleet-desktop.png)

## Rows and columns

By default a row shows **Status**, **Agent**, **Uptime** and **Working directory**. **Columns** adds or removes any of these:

| Column | What it shows |
| --- | --- |
| **Status** | The agent's status (below). Always shown with its colour mark. |
| **Agent** | The agent's name, and its role in an orchestration. Cannot be removed. |
| **Last activity** | How long ago the daemon last saw the agent do anything. Blank when the daemon has not reported it, for example for agents under a restarted daemon. |
| **Uptime** | How long the agent's process has been running. For an orchestration worker that was restarted, that is the age of its current process. |
| **CLI** | The program the agent runs, as the daemon reports it (`claude`, `codex`, …). |
| **Active tool** | The tool the agent is running now and a short form of its argument, or `no active tool`. |
| **Tools** | How many tool calls the agent has reported. |
| **Working directory** | Where the agent runs. |
| **Last prompt** | The last prompt sent to the agent. |

**Restore defaults** in the Columns menu goes back to the four default columns. Your choice is remembered by the app on this computer; it is not written to the [settings file](settings.md#the-settings-file). The TUI has no column choice: its cards pick their own density from the terminal's size.

## Statuses

The desktop app shows four statuses. Each covers one or more of the words the TUI shows on its cards:

| Desktop | TUI | Meaning |
| --- | --- | --- |
| **RUNNING** | Thinking, Working, Compacting | The agent is busy. |
| **WAITING** | Needs Input, Idle | The agent is waiting: for your approval or input, or for its next task. The desktop app does not tell the two apart. |
| **FAILED** | Error | Something went wrong. |
| **BLOCKED** | Blocked | The agent's provider refused it because a usage limit or credit pool is exhausted. Open the agent's pane to see which limit, and when it resets if the provider says. |

What each status means, and which agents can report Blocked, is on [Session Management → Session Statuses](../session-management.md#session-statuses).

## The agent pane

Click a row, or its open control (`Open <name> agent`), to open that agent's live terminal in a full-window pane over the Dashboard. Type into it as you would into the TUI's pane; [Keys typed into an agent's terminal](settings.md#keys-typed-into-an-agents-terminal) says which keys reach the agent. Press **Back to dashboard**, or `Escape` when you are not typing in the terminal, to close the pane; the agent keeps running.

Besides **Terminal**, the pane has **Diff**, **Checks**, **Delegations** and **Artifacts** tabs. The daemon does not provide that data today, so against a real daemon each of them says so (for example "Diff data is not exposed by the daemon").

The pane's header names the agent by its type (for example Codex), or by its role when it belongs to an orchestration, rather than by the display name its row shows. Beside that is its status, and below it the agent's command and its model, which reads `Unavailable` because the daemon does not report one.

![An agent's full-window pane: the header reads Codex with a RUNNING status and codex · Unavailable below it, with a close control on the right; under it the agent's assignment, a row of time, token, cost and context readings, then the Terminal, Diff, Checks, Delegations and Artifacts tabs, and the Terminal tab showing the agent's live output: the files it read and edited, a test run that passed, and its summary](/img/agent-pane-desktop.png)

## Closing agents and orchestrations

- **An agent:** press the stop control on its row (`Close <name> agent`). The confirmation, `Close <name>?`, says which daemon the stop request goes to; press **Close agent**.
- **An orchestration:** press **Close** on its group header. The confirmation lists every role it will stop; press **Close all N roles** (or **Close 1 role**).

In the TUI the same things are `Ctrl+w` on a card, and closing an orchestration's tab. If the app cannot confirm that some roles stopped, it says how many may still be running on which daemon, in a message on the Dashboard and in the New agent dialog.

## When a daemon is not there

A daemon section shows one of these instead of its agents:

| Title | What it means |
| --- | --- |
| **Daemon disconnected** | Nothing is answering at that daemon's address. Start a daemon (see [Installation → How the desktop app gets a daemon](../installation.md#how-the-desktop-app-gets-a-daemon)), then press **Reconnect**. |
| **Waiting for this daemon** | The app is still connecting to it. Its agents appear when it answers. |
| **Daemon not configured** | A remote daemon in Settings has no socket path yet. Open [Settings → Daemons](daemons.md) and press **Test connection**, which finds it. |
| **Incompatible daemon** | A daemon answered, but the app will not use it: either it speaks another protocol, or a declared compatibility break sits between the two builds. For a declared break, the note names it and **Connect anyway** connects for this session. For a protocol difference nothing overrides it. Either way, the fix is a daemon from the same release as the app (see [Installation → Keep the app and the daemon on the same release](../installation.md#keep-the-app-and-the-daemon-on-the-same-release)). |
| **No agents are running yet** | The daemon is healthy and owns no agents: what a fresh install looks like. Press **New agent**. |

![The desktop app's dashboard with a healthy daemon and no agents: “No agents are running yet”](/img/dashboard-empty-desktop.png)
