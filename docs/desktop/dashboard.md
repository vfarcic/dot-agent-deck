# Dashboard

The **Agent dashboard** is the desktop app's main screen and the one it opens on: every agent on the daemons you are watching, one row each. It is the desktop counterpart of the TUI's dashboard, which shows the same agents as cards (see [Session Management](../session-management.md)).

![The desktop app's dashboard with four agents in one daemon section, each row showing its status, name and uptime](/img/dashboard-desktop.png)

## Check what is running

1. Choose what to watch with the **Daemon** selector under the title: **All daemons**, **This machine** (the default on a fresh install), or one remote daemon. Remote daemons are added in [Settings → Daemons](daemons.md).
2. Read the counters at the top. **AGENTS**, **RUNNING**, **NEEDS INPUT**, **FAILED**, **IDLE** and **GROUPS** count the agents on the daemons that answered. **DAEMONS** reads `<answered>/<shown>`, for example `1/1` for one healthy daemon. When a shown daemon has not answered, **DAEMONS** turns red and the other counters describe only the daemons that did. When no daemon answered, the other counters read `—`.
3. Read the rows. Each daemon has its own section (below); press **Refresh** to reconnect to the daemons and read their agent lists again. When there are more daemons and agents than fit the window, a scrollbar stays on the right edge: its length shows how much of the dashboard is on screen and its position shows where you are. Scroll with the mouse, the trackpad or the keyboard, or, with voice on, [by voice](voice.md#scrolling-the-dashboard).

## One section per daemon

Each daemon being shown gets its own section, headed with its name (**Local daemon** for this machine, the ssh user and host for a remote one) and its own **New agent** button, which creates the agent on that daemon. A remote daemon that runs an older release than the app also gets an **Upgrade** button there ([Daemons → Upgrade a remote daemon](daemons.md#upgrade-a-remote-daemon)). Inside a section, agents are grouped:

- **Standalone agents** (kicker **NO TAB**): agents that belong to no orchestration and no workspace mode.
- One group per orchestration (kicker **ORCHESTRATION**), named after the run. Its rows are numbered in role order, and the start role, the one you message, carries an **ORCHESTRATOR** badge.
- One group per workspace mode (kicker **MODE TAB**), named after the mode. You see one only while an agent started in a workspace mode by a release before 0.44.0 is still running; workspace modes were removed in 0.44.0, so neither client starts one.

Beside the daemon's name, a small **Host** button shows how the machine that daemon runs on is doing: green when its disk, load and memory are fine, amber with **Host: warning** or red with **Host: critical** when one of them is not. Click it, or `Tab` to it and press `Enter`, to open **Host of this deck**: free disk space, load per core and memory, each coloured the same way, with `(warning)` or `(critical)` after a figure that needs attention. Its **X** button, `Esc` or a click elsewhere closes it. With voice on, saying "show host" opens it too, and "close" or "close host" closes it ([Voice Control → Checking a daemon's machine](voice.md#checking-a-daemons-machine)). A daemon from an older release opens it saying **Host metrics are not available from this deck** instead. The TUI shows the same figures when you press `m`. [Session Management → Check the machine a deck runs on](../session-management.md#check-the-machine-a-deck-runs-on) explains each row and how to change the limits.

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
2. Press **Filter** to choose from four lists: **Kind** (orchestration roles, single agents, dispatchers, schedule agents or schedule: issues agents), **Status** (Working, Thinking, Needs input, Idle, Blocked, Error), **Agent type** (Claude Code, Codex, OpenCode, Pi, Devin) and **Daemon**. Tick as many entries as you like.

Dispatchers, schedule agents and schedule: issues agents are the agents started with the **dispatcher**, **schedule** and **schedule: issues** modes of [New agent](new-agent.md), in the desktop app or the TUI. An agent started that way by an earlier release of the deck is listed under single agents.

An agent is shown when it matches everything you chose: the text, and one of the ticked entries in each list you ticked something in. For example, ticking **Working** and **Thinking** under Status and **Codex** under Agent type shows the Codex agents that are working or thinking.

While a filter is set, a line under the header says how much of the fleet you are looking at, for example `Showing 4 of 11 agents · Working · Dispatchers`. Each part of the filter has its own remove control on that line, and **Show all** clears the whole filter in one click. The counters in the header (**AGENTS**, **RUNNING**, **NEEDS INPUT**, **FAILED**, **IDLE**, **GROUPS** and **DAEMONS**) always count the whole fleet, not only the agents the filter shows.

A daemon none of whose agents match shows one line in its place, for example `build-box: no matching agents`, and so does an orchestration or other group with no matching agents, so nothing disappears without a word.

The filter stays while you move between screens and lasts until you clear it or quit the app; the next time the app starts, the Dashboard shows every agent. You can also set and clear it [by voice](voice.md#filtering-the-dashboard). The TUI's dashboard has no filter.

![The Dashboard filtered to working agents: two daemons filtered to working agents: the line under the header reads Showing 3 of 6 agents · Working, with a remove control beside Working and a Show all button, while the counters above still count all six agents](/img/dashboard-filter-desktop.png)

## Statuses

The desktop app shows five statuses. Each covers one or more of the words the TUI shows on its cards:

| Desktop | TUI | Meaning |
| --- | --- | --- |
| **RUNNING** | Thinking, Working, Compacting | The agent is busy. An agent that has reported no status at all yet, for example a plain shell or an agent whose hooks are not installed, also shows **RUNNING**. |
| **NEEDS INPUT** | Needs Input | The agent is waiting for you: a permission answer or other input. Answer it in its pane. |
| **IDLE** | Idle | The agent finished its turn and is waiting for its next prompt. Commands or subagents it left running keep it **IDLE**; open its pane to see what is still running. A status this build does not recognise, from a newer daemon, also shows **IDLE**, as it does in the TUI. |
| **error** (counted under **FAILED**) | Error | Something went wrong, including a message the deck could not deliver. Read the notice and open the pane. |
| **BLOCKED** | Blocked | The agent's provider refused it because a usage limit or credit pool is exhausted. Open the agent's pane to see which limit, and when it resets if the provider says. |

What each status means, and which agents can report Blocked, is on [Session Management → Session Statuses](../session-management.md#session-statuses). An agent that stays **RUNNING** when it is clearly idle usually has no hooks reporting to the daemon; see [Troubleshooting → Hooks](../troubleshooting.md#hooks).

## Message delivery and handoffs

Agent tiles show `✉N queued` while messages await acknowledgement, and `✉ delivered` when received and nothing remains pending. If delivery fails, the tile reads **error** and shows `not delivered: …`. When the deck cannot tell whether a message reached a Claude Code, OpenCode or Pi agent (`✉ delivery uncertain`), or a Pi agent has stayed idle without starting on one (`✉ may not have reached the agent`), the tile lists each such message with its explanation and **Retry** and **Cancel** buttons. The TUI cards show the same states, with **Error** as the status and `✉N` on narrow cards, and list the messages when you press `i`. When an orchestration's start role shows why the deck is holding its messages, the top bar of the deck and of the Dashboard says so too, with how many other agents show such a reason, as the TUI's tab bar does; see [Delivery in the TUI and desktop app](../orchestration.md#delivery-in-the-tui-and-desktop-app).

The handoff rail tracks worker handoffs with `delegated · queued`, `delivered · working`, `work-done` and `not delivered`. It is a desktop surface; in the TUI, follow the role cards and the orchestrator's pane. A delivered message is a receipt, so read the worker's report before treating its work as complete.

In both the desktop app and the TUI, a job that arrives while OpenCode or Pi is busy waits for that agent for as long as the agent keeps it queued, without going to another worker during that wait. A Claude Code worker can lose an unclaimed job to the next worker while busy. If the orchestrator was told a task has not reached its worker, check the worker's pane before delegating or reassigning the task again: the worker may pick it up after all. When it does, the orchestrator may be told the task arrived, and for a task reported not delivered the card or tile may leave its delivery error once no other delivery error remains.

See [Orchestration → Delivery](../orchestration.md#delivery-in-the-tui-and-desktop-app) for native wake support, the fallback doorbell and the inbox commands.

## The agent pane

To watch or talk to one agent:

1. Click its row, or its open control (`Open <name> agent`). Its live terminal opens in a full-window pane over the Dashboard.
2. Type into the terminal as you would into the TUI's pane. [Keys typed into an agent's terminal](settings.md#keys-typed-into-an-agents-terminal) says which keys reach the agent, including your platform's editing and paste shortcuts.
3. When the agent's output is longer than the terminal, a scrollbar stays on the terminal's right edge, so you can see that there is more above and how far back you are. Scroll up to read earlier output.
4. To copy the agent's output, drag across it with the mouse. Some agents, such as Codex, OpenCode and Claude Code in its fullscreen mode, handle the mouse themselves: they highlight what you drag across and copy it when you release the button, so the text is on your clipboard with nothing more to press. Otherwise the terminal highlights the text, and you copy it with your system's copy key: `⌘C` on macOS, `Ctrl+Shift+C` on Linux, and `Ctrl+C` or `Ctrl+Shift+C` on Windows. On Windows, `Ctrl+C` copies only while text is highlighted and then clears the highlight, so pressing it again interrupts the agent. On macOS and Linux, plain `Ctrl+C` always goes to the agent as an interrupt, even while text is highlighted.
5. Press **Back to dashboard**, or `Escape` when you are not typing in the terminal, to close the pane. The agent keeps running.

The TUI copies differently: a mouse drag in a pane copies when you release the button, whichever agent is running, and when an agent copies something itself, the TUI passes that on to your terminal too. Whether either reaches your clipboard depends on your terminal, which has to let programs set the clipboard. Some ask first or need it turned on, such as tmux, which needs `set-clipboard on`.

In either client, some agents (Codex and Pi among them) copy straight to the clipboard of the machine they run on, and ask the terminal only when that fails or when they run over SSH. When the daemon runs on another machine that has its own clipboard, such an agent's copy lands on that machine's clipboard, not yours.

The pane's header names the agent by the name its row shows, which is its role's name for an agent in an orchestration until you rename it. Beside that is its status, and below it the agent's command, for example `codex`. A renamed orchestration role keeps its role there too, for example `Coder · claude`. The TUI card's title carries the same name. When the agent has ended its turn but left commands or subagents running, a line below that says so, for example `Background: 2 commands, 1 subagent · 3h 12m`, in the same words as the TUI card; see [Session Management → Work left running after a turn](../session-management.md#work-left-running-after-a-turn).

Between the header and the terminal, the pane shows the agent's assignment and two readings: **TIME**, how long the agent has been running (hover it for the exact time it started), and **TOOLS**, how many tool calls it has reported. TIME reads `—` when the daemon did not say when it started the agent.

![An agent's full-window pane: the header reads Desktop implementation with a RUNNING status and codex below it, with a close control on the right; under it the agent's assignment and a row of time, token, cost and context readings, each with its label on the same line, then the Terminal tab showing the agent's live output: the files it read and edited, a test run that passed, and its summary](/img/agent-pane-desktop.png)

What can go wrong in the pane:

- **"No terminal here: the desktop has no live connection to <daemon>…"**: the agent's daemon stopped answering. The header then shows the last status the app saw. The terminal comes back on its own when the daemon answers again.
- **"Terminal input unavailable: this agent has no live terminal, only its history"**, or **"Terminal input unavailable: this agent has no live terminal"**: the pane shows the agent's output but cannot take typing, so the terminal is read-only and this line stays under it. In the TUI, the same words appear in the status line when you press `Enter` or the card's number on that agent's card, and the dashboard stays where it is.
- **"[agent-deck] Input not delivered: <why>"**, written into the terminal: the daemon turned down what you just typed, so it did not reach the agent. The reason says why, for example `the agent has exited` or `the agent in this pane was replaced or closed`. In the TUI, the same words appear in the status line and the TUI goes back to command mode, so the keys you press next are not sent to the pane.

## Pull requests

When an agent works on a branch that has a pull request on GitHub, the app shows a badge for it on the agent's row on the Dashboard and in the header of its pane. The badge is GitHub's icon for the pull request's state (open, draft, merged or closed), its number, then its CI checks as GitHub's pull request list shows them (a green check when they passed, a red cross when one failed, an amber dot while they run) and its review (approved, changes requested or review required) in a filled circle, all in GitHub's icons and colours. The circle is what tells an approval from passing CI, since both are a check mark. A pull request without CI checks shows no CI icon, and one with no review yet shows no review icon. Hover the badge to read the same in words, with how many checks are in each result, for example `Pull request #1234: open, review required; CI: 1 failing, 24 passing`. The CI result is GitHub's overall one, so a failing check that is not required for merging still shows the red cross.

The TUI shows the same badge on the bottom border of the agent's card, as glyphs in the same order: the number, the state, the CI result (`✓` passed, `✗` failed, `●` running) and the review (`✓` approved, `±` changes requested, `◉` review required) on a filled cell, for example `#1234 ⊙ ✓ ◉`. The TUI has no tooltip, so it shows no counts; see [Session Management → Pull request badge](../session-management.md#pull-request-badge) for every glyph.

The badge appears by itself: you do not link the pull request. An agent gets one when its working directory is a git repository with a GitHub `origin`, it is on a branch other than the repository's default branch, and that branch has a pull request. The badge follows the pull request, so a review, a CI result or a merge shows up on it within about a minute. If you expect a badge and there is none, see [Troubleshooting → An agent has a pull request but no badge](../troubleshooting.md#an-agent-has-a-pull-request-but-no-badge).

**TUI:**

![TUI card showing open pull request #1234 with passing CI, awaiting review, beside the implementation agent's pane](/img/pull-request-badge-tui.png)

**Desktop:**

![Dashboard row showing the implementation agent's open pull request #1234 with passing CI, awaiting review](/img/pull-request-badge-desktop.png)

### Read a pull request in the app

1. Click the badge, on the agent's row or in its pane, or with voice on say "open the PR for the coder" on the dashboard, or "open the PR" while the agent's pane is open (see [Voice Control → Pull requests](voice.md#pull-requests)). GitHub's page for the pull request opens inside the app, over the screen you were on.
2. Read, comment and review on the page as you would on github.com.
3. Use the toolbar above the page: **Back** goes back one page, **Open in browser** opens the page you are on in your system browser and closes it in the app, and the close button (✕) in its corner closes it. If your system browser does not open, or the page cannot go back, the page stays in the app and a message under the toolbar says why, so you can try again.
4. Press `Escape` to close the page and return to where you were. When you are typing in the page or a menu or dialog of GitHub's is open, `Escape` closes that first, as it does on github.com.

A link to a site other than GitHub opens in your system browser, and the page stays where it was. While one of the app's own dialogs is open, for example Settings or voice's "What you can say" list, the page is hidden, and it comes back when the dialog closes.

### Signing in to GitHub in the app

The page has its own GitHub sign-in, separate from your system browser's. The first time you open a pull request that needs it, sign in on the page; the app remembers the sign-in, also after you restart it.

A sign-in that leaves GitHub cannot finish in the app: if your organisation signs you in through SAML single sign-on, or you sign in to GitHub with Google or Apple, that step opens in your system browser instead. Use **Open in browser** to work on the pull request there.

To sign out, use [Settings → GitHub → Sign out of GitHub](settings.md#github).

### In the TUI

The TUI shows the same pull request on the agent's card, as `#1234` with a glyph for its state and its review on the card's bottom border (see [Session Management → What a TUI card shows](../session-management.md#what-a-tui-card-shows)), and `o` opens the selected card's pull request in your system browser rather than inside the TUI. The desktop app has no `o` key, because it has no keyboard selection of a row: click the badge or say "open the PR for" and the agent's name instead.

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
| **Incompatible daemon** | A daemon answered, but it and the app are different versions, so the app will not use it. The message says which of the two is older, and either that the app has not connected because it could misread what the daemon reports, or that the two cannot work together at all. | Update the older of the two (see [Installation → Keep the app and the daemon on the same release](../installation.md#keep-the-app-and-the-daemon-on-the-same-release)). The note says what each of its buttons does: **Upgrade**, offered when a remote daemon is older than the app, installs the app's version there and restarts the daemon, asking before it stops any running agent ([Daemons → Upgrade a remote daemon](daemons.md#upgrade-a-remote-daemon)); **Open daemons** goes to the Daemons screen, where the daemon on this machine can be replaced, asking before it stops any running agent; **Connect anyway**, offered only when the message says the app could misread the daemon (never when the two cannot work together), uses the daemon as it is until you quit the app, though some of what it shows may be wrong. The choice covers the whole app session, not only this daemon: until you quit, the app also uses any other daemon it would refuse for the same reason, on any deck, including one that replaces this daemon later; **Reconnect** tries again. **Technical details** under the message shows the exact versions, for a bug report. |
| **Desktop bridge error** | The app could not read the daemon's agents, and no daemon answered for it to judge. This is a problem on the app's side, not a version difference. | Press **Reconnect**. If it keeps happening, restart the app. |
| **No agents are running yet** | The daemon is healthy and runs no agents. This is what a fresh install looks like. | Press **New agent**. |

![The desktop app's dashboard with a healthy daemon and no agents: “No agents are running yet”](/img/dashboard-empty-desktop.png)

When a daemon restarts while the app is connected to it, the agents it was running can no longer be reached, so the app removes their tiles and open panes and shows one notice in the corner naming each of them, with its role and orchestration, such as `The deck restarted; these agents were stopped: coder (worker in team), api-fix.` The notice lists every agent, scrolling when the list is long, and stays until you close it with its `×`; agents a later restart stops join it. Nothing is stopped or deleted to remove them. The TUI shows the same notice above its footer. See [The deck restarted and its agents disappeared](../troubleshooting.md#the-deck-restarted-and-its-agents-disappeared).
