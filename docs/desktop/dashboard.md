# Dashboard

The **Agent dashboard** is the desktop app's main screen and the one it opens on: every agent on the daemons you are watching, one row each. It is the desktop counterpart of the TUI's dashboard, which shows the same agents as cards (see [Session Management](../session-management.md)).

The Dashboard can also show each agent as a card that says what it reported, whether it needs you and what you can do about it: see [Agent Cards](agent-cards.md).

## Check what is running

1. Choose what to watch with the **Daemon** selector under the title: **All daemons**, **This machine** (the default on a fresh install), or one remote daemon. To add a daemon on another machine, choose **Add a daemon…** at the bottom of the selector ([Daemons](daemons.md#watch-a-daemon-on-another-machine)).
2. Read the counters button at the top, beside **New agent**. It shows how many agents the daemons that answered are running, for example `7 agents`, and adds a count only when something needs you: amber `1 needs input`, red `1 error`, and red `2/4 daemons` when some of the daemons shown have not answered. Hover over it, or click it, to see every count: **AGENTS**, **WORKING**, **THINKING**, **COMPACTING**, **NEEDS INPUT**, **ERROR**, **WAITING**, **IDLE** and **GROUPS** count the agents on the daemons that answered, and **DAEMONS** reads `<answered>/<shown>`, for example `1/1` for one healthy daemon. When no daemon answered, the counts read `—`. After a click the list stays open until you click the button again, press `Esc` or click elsewhere; with voice on, "show the counters" opens it ([Voice Control → Checking the counters](voice.md#checking-the-counters)). In a window narrower than 1240 pixels the button shows its numbers without the words. The TUI's dashboard shows the same counts on one line.
3. Read the rows. Each daemon has its own section (below); press **Refresh** to reconnect to the daemons and read their agent lists again. The **⋯** button right of **Refresh** opens a menu with [Needs you only](#show-only-the-agents-that-need-you); while the daemons show agent cards, a small number on it says how many agents need you. When there are more daemons and agents than fit the window, a scrollbar stays on the right edge: its length shows how much of the dashboard is on screen and its position shows where you are. Scroll with the mouse, the trackpad or the keyboard, or, with voice on, [by voice](voice.md#scrolling-the-dashboard).

## One section per daemon

Each daemon being shown gets its own section, headed with its name (**Local daemon** for this machine, the ssh user and host for a remote one), its own **Stop daemon** button, which stops that daemon ([Daemons → Stop a daemon](daemons.md#stop-a-daemon)), and its own **New agent** button, which creates the agent on that daemon. Just left of **New agent**, a **⋯** button opens that daemon's menu; its **Card actions** item opens the editor for the actions that daemon's agents offer on their cards ([Agent Cards → The Card actions editor](agent-cards.md#the-card-actions-editor)). The menu is there while the daemon shows agent cards. A remote daemon that runs an older release than the app also gets an **Upgrade** button there ([Daemons → Upgrade a remote daemon](daemons.md#upgrade-a-remote-daemon)). Inside a section, agents are grouped:

- **Standalone agents** (kicker **NO TAB**): agents that belong to no orchestration and no workspace mode.
- One group per orchestration (kicker **ORCHESTRATION**), named after the run. Its rows are numbered in role order, and the start role, the one you message, carries an **ORCHESTRATOR** badge.
- One group per workspace mode (kicker **MODE TAB**), named after the mode. You see one only while an agent started in a workspace mode by a release before 0.44.0 is still running; workspace modes were removed in 0.44.0, so neither client starts one.

Beside the daemon's name, a small **Host** button shows how the machine that daemon runs on is doing: green when its disk, load and memory are fine, amber with **Host: warning** or red with **Host: critical** when one of them is not. Click it, or `Tab` to it and press `Enter`, to open **Host of this deck**: free disk space, load per core and memory, each coloured the same way, with `(warning)` or `(critical)` after a figure that needs attention. Its **X** button, `Esc` or a click elsewhere closes it. With voice on, saying "show host" opens it too, and "close" or "close host" closes it ([Voice Control → Checking a daemon's machine](voice.md#checking-a-daemons-machine)). A daemon from an older release opens it saying **Host metrics are not available from this deck** instead. The TUI shows the same figures when you press `m`. [Session Management → Check the machine a deck runs on](../session-management.md#check-the-machine-a-deck-runs-on) explains each row and how to change the limits.

Each group header shows how many agents it holds, how many are in each status, and the working directory most of its agents share. A row shows its own directory only when it differs from that.

## Rows and columns

By default a row shows **Status**, **Agent**, **Uptime** and **Working directory**. **Columns** adds or removes any of these:

| Column | What it shows |
| --- | --- |
| **Status** | The agent's status (below), with its colour mark. |
| **Agent** | The agent's name, and its role in an orchestration, followed by its badges: **ORCHESTRATOR**, the pull request, and an amber **Needs you** when its card needs you ([Agent Cards](agent-cards.md#what-a-card-shows); hover it to see why). Cannot be removed, so those badges show whichever columns you hide. |
| **Last activity** | How long ago the daemon last saw the agent do anything. Blank when the daemon has not reported it, for example for agents under a restarted daemon. |
| **Uptime** | How long the agent's process has been running. For an orchestration worker that was restarted, the age of its current process. |
| **CLI** | The program the agent runs, as the daemon reports it (`claude`, `codex`, …). |
| **Active tool** | The tool the agent is running now and a short form of its argument, or `no active tool`. |
| **Tools** | How many tool calls the agent has reported. |
| **Working directory** | Where the agent runs. |
| **Last prompt** | The last prompt sent to the agent. |

At the right end of a row are its **Close** button (`Close <name> agent`) and, when agent cards are offered, its **⋯** menu. They stay in the same place on every row, with or without a **Needs you** badge.

**Restore defaults** in the Columns menu goes back to the four default columns. The choice is remembered by the app on this computer, in the app's own storage, not in the [settings file](settings.md#the-settings-file). The TUI has no column choice: its cards pick their density from the terminal's size.

## Filter the dashboard

To show only the agents you care about right now, use the filter at the top of the Dashboard:

1. Type into **Filter agents** to show only agents whose name, orchestration role, orchestration, working directory or last prompt contains what you typed. Case does not matter.
2. Press **Filter** to choose from four lists: **Kind** (orchestration roles, single agents, dispatchers, schedule agents or schedule: issues agents), **Status** (Working, Thinking, Needs input, Waiting, Idle, Blocked, Error), **Agent type** (Claude Code, Codex, OpenCode, Pi, Devin, Antigravity) and **Daemon**. Tick as many entries as you like.

Dispatchers, schedule agents and schedule: issues agents are the agents started with the **dispatcher**, **schedule** and **schedule: issues** modes of [New agent](new-agent.md), in the desktop app or the TUI. An agent started that way by an earlier release of the deck is listed under single agents.

An agent is shown when it matches everything you chose: the text, and one of the ticked entries in each list you ticked something in. For example, ticking **Working** and **Thinking** under Status and **Codex** under Agent type shows the Codex agents that are working or thinking.

While a filter is set, a line under the header says how much of the fleet you are looking at, for example `Showing 4 of 11 agents · Working · Dispatchers`. Each part of the filter has its own remove control on that line, and **Show all** clears the whole filter in one click. The counters in the header (**AGENTS**, **WORKING**, **THINKING**, **COMPACTING**, **NEEDS INPUT**, **ERROR**, **WAITING**, **IDLE**, **GROUPS** and **DAEMONS**) always count the whole fleet, not only the agents the filter shows.

A daemon none of whose agents match shows one line in its place, for example `build-box: no matching agents`, and so does an orchestration or other group with no matching agents, so nothing disappears without a word.

The filter stays while you move between screens and lasts until you clear it or quit the app; the next time the app starts, the Dashboard shows every agent. You can also set and clear it [by voice](voice.md#filtering-the-dashboard). The TUI's dashboard has no filter.

## Show only the agents that need you

While the daemons show [agent cards](agent-cards.md), open the **⋯** menu right of **Refresh** and choose **Needs you only**:

- In the Table view, the Dashboard lists only the agents that need you, together with the agents above them, such as the orchestrator a role runs under or the dispatcher that started it, so you can see where each one sits. The other rows are hidden. A daemon with nobody needing you shows one line saying so.
- In the Cards view, the cards stay where they are and the ones that neither need you nor lead to one fade ([Agent Cards → Needs you only](agent-cards.md#needs-you-only)).

While it is on, the item is ticked and a **Needs you only ×** chip sits beside **Filter**. Press the chip, or choose the item again, to see every agent. Switching between Table and Cards keeps the setting. It works together with the filter above: you see the agents that match both. A daemon from a release without agent cards marks nobody as needing you, so its rows stay as they are, and the number on **⋯** leaves its agents out. When no card needs you, a line above the daemons says "No card needs you." and names each daemon it cannot speak for: a daemon without cards whose agents the Dashboard is showing, whose rows you read for yourself, and a daemon that is not answering. When there is no such daemon, the line says "Nothing needs you right now." instead. With voice on, say "only the ones that need me" or "show all cards".

The TUI's dashboard has no Needs you only; its cards show each agent's status.

## Statuses

The desktop app shows a status in the same word the TUI's cards use, on the table, the cards, the tiles and the counters:

| Status | Meaning |
| --- | --- |
| **THINKING**, **WORKING**, **COMPACTING** | The agent is busy: reasoning, running a tool, or compacting its conversation. |
| **NEEDS INPUT** | The agent is waiting for you: a permission answer or other input. Answer it in its pane, or from the app or by voice when the deck knows the question; its row then shows `QUESTION` and how long it has waited, and the app sends one notification once it has waited 10 minutes ([When a question waits](../session-management.md#when-a-question-waits)). |
| **WAITING** | The agent's turn ended while a command it started in that turn is still running. A turn Claude Code starts by itself to tell the agent that another background job finished does not count as a new turn, so the agent stays **WAITING** while a command it started before that is still running. Its mark is hollow, in **WORKING**'s colour: its work is not finished, but nothing needs you. It turns **IDLE** when the command ends. Open its pane to see what is still running ([Work left running after a turn](../session-management.md#work-left-running-after-a-turn)). |
| **IDLE** | The agent finished its turn and is waiting for its next prompt. Commands it left running from an earlier turn, and subagents it left running, keep it **IDLE**; open its pane to see what is still running. A status this build does not recognise, from a newer daemon, also shows **IDLE**, as it does in the TUI. |
| **ERROR** | Something went wrong, including a message the deck could not deliver. Read the notice and open the pane. |
| **BLOCKED** | The agent's provider refused it because a usage limit or credit pool is exhausted. Open the agent's pane to see which limit, and when it resets if the provider says. |
| **RUNNING** | The agent has reported no status at all yet, for example a plain shell or an agent whose hooks are not installed. The counters do not count it under a status, only under **AGENTS**. |

What each status means, and which agents can report Blocked, is on [Session Management → Session Statuses](../session-management.md#session-statuses). An agent that stays **RUNNING** when it is clearly idle usually has no hooks reporting to the daemon; see [Troubleshooting → Hooks](../troubleshooting.md#hooks).

## Message delivery and handoffs

Agent tiles show `✉N queued` while messages await acknowledgement, and `✉ delivered` when received and nothing remains pending. If delivery fails, the tile reads **error** and shows `not delivered: …`. When the deck cannot tell whether a message reached a Claude Code, OpenCode or Pi agent (`✉ delivery uncertain`), or a Pi agent has stayed idle without starting on one (`✉ may not have reached the agent`), the tile lists each such message with its explanation and **Retry** and **Cancel** buttons. The TUI cards show the same states, with **Error** as the status and `✉N` on narrow cards, and list the messages when you press `i`. When an orchestration's start role shows why the deck is holding its messages, the top bar of the deck and of the Dashboard says so too, with how many other agents show such a reason, as the TUI's tab bar does; see [Delivery in the TUI and desktop app](../orchestration.md#delivery-in-the-tui-and-desktop-app).

The handoff rail tracks worker handoffs with `delegated · queued`, `delivered · working`, `work-done` and `not delivered`. It is a desktop surface; in the TUI, follow the role cards and the orchestrator's pane. A delivered message is a receipt, so read the worker's report before treating its work as complete.

In both the desktop app and the TUI, a job that arrives while OpenCode or Pi is busy waits for that agent for as long as the agent keeps it queued, without going to another worker during that wait. A Claude Code worker can lose an unclaimed job to the next worker while busy. If the orchestrator was told a task has not reached its worker, check the worker's pane before delegating or reassigning the task again: the worker may pick it up after all. When it does, the orchestrator may be told the task arrived, and for a task reported not delivered the card or tile may leave its delivery error once no other delivery error remains.

See [Orchestration → Delivery](../orchestration.md#delivery-in-the-tui-and-desktop-app) for native wake support, the fallback doorbell and the inbox commands.

## The agent pane

To watch or talk to one agent:

1. Click its row. From the keyboard, press `Tab` until the row is focused (each row is one stop, read out as `Open <name> agent`, followed by its own buttons) and press `Enter` or `Space`. Its live terminal opens in a full-window pane over the Dashboard. Opening an agent also clears the **Needs you** its card raised for a report that it is waiting, done or failed ([Agent Cards](agent-cards.md#what-a-card-shows)). In the Cards view, click the agent's card instead; the row's **⋯** menu has **Show card** too. Every way in opens the pane on its **Terminal** tab.
2. Type into the terminal as you would into the TUI's pane. [Keys typed into an agent's terminal](settings.md#keys-typed-into-an-agents-terminal) says which keys reach the agent, including your platform's editing and paste shortcuts.
3. When the agent's output is longer than the terminal, a scrollbar stays on the terminal's right edge, so you can see that there is more above and how far back you are. Scroll up to read earlier output.
4. To copy the agent's output, drag across it with the mouse. Some agents, such as Codex, OpenCode and Claude Code in its fullscreen mode, handle the mouse themselves: they highlight what you drag across and copy it when you release the button, so the text is on your clipboard with nothing more to press. Otherwise the terminal highlights the text, and you copy it with your system's copy key: `⌘C` on macOS, `Ctrl+Shift+C` on Linux, and `Ctrl+C` or `Ctrl+Shift+C` on Windows. On Windows, `Ctrl+C` copies only while text is highlighted and then clears the highlight, so pressing it again interrupts the agent. On macOS and Linux, plain `Ctrl+C` always goes to the agent as an interrupt, even while text is highlighted.
5. Press **Back to dashboard**, the **×** at the right end of the pane's header, or `Escape` when you are not typing in the terminal, to close the pane. The agent keeps running.

When the agent has a card, the pane has a **Card** tab beside **Terminal**: the agent's report, its questions with their answers, and its actions as icons, shown in the terminal's place ([The Card tab](agent-cards.md#the-card-tab)). Switching tabs never resizes the agent's terminal. While the agent needs you, the **Card** tab shows a bell. The next time you open an agent, its pane starts on **Terminal** again; only a card's **+N** and saying "show the coder's card" open it on **Card**, because they ask for the card itself. In the pane's header, **Previous agent** and **Next agent** open the neighbouring agents' panes in the Cards view's order, and **Next needing you** the next agent that needs you, each on its **Terminal** tab; `[`, `]` and `n` do the same while you are not typing in the terminal. The TUI's agent pane has no Card tab; see [Agent Cards → In the TUI](agent-cards.md#in-the-tui).

To ask the agent to write the project's `.dot-agent-deck.toml`, press **Generate config** in the pane's header, which reads **Update config** when the project already has one ([Generate a config with an agent](#generate-a-config-with-an-agent)).

To end the agent instead, press the red **Close agent** button with the stop icon in the pane's header. The same `Close <name>?` confirmation as on the Dashboard asks before stopping it. **Cancel** (or `Escape`) leaves the agent and its pane open; confirming stops that agent and returns you to the Dashboard. For an orchestration role, the confirmation offers two closes: **Close agent** closes just that role, leaving the orchestration one role short while the other roles keep running, and **Close orchestration** closes every role, as **Close** on the orchestration's group header does. The text says which roles each one closes. On the orchestrator, **Close orchestration** is the highlighted choice, because the other roles would have nothing coordinating them; on any other role, **Close agent** is.

With voice on and typing mode off, say **"stop"**, **"stop it"**, **"stop that"** or **"close this agent"** on its own to open that confirmation; you still confirm by hand. In typing mode, "stop" interrupts the agent's turn instead of ending its session. Say "typing off" before asking to close the agent. See [Voice Control](voice.md#interrupt-clear-and-scratch-that). In the TUI, enter command mode (`Ctrl+D`), select the agent's card on the dashboard and press `Ctrl+W`, then choose **Close**. On an orchestration tab, that key asks to close the whole orchestration instead.

The TUI copies differently: a mouse drag in a pane copies when you release the button, whichever agent is running, and leaves the copied text highlighted; `v` in command mode opens copy mode, for selecting with the keyboard ([Keyboard Shortcuts → Copying text](../keyboard-shortcuts.md#copying-text)). The TUI puts the text on the clipboard of the machine it runs on and also asks your terminal to, and when an agent copies something itself, the TUI passes that on to your terminal too. Over ssh, or wherever the TUI cannot reach a clipboard itself, the copy depends on your terminal, which has to let programs set the clipboard. Some ask first or need it turned on, such as tmux, which needs `set-clipboard on` ([Troubleshooting → Copy and paste](../troubleshooting.md#copy-and-paste)).

In either client, some agents (Codex and Pi among them) copy straight to the clipboard of the machine they run on, and ask the terminal only when that fails or when they run over SSH. When the daemon runs on another machine that has its own clipboard, such an agent's copy lands on that machine's clipboard, not yours.

The pane's header is one line. It names the agent by the name its row shows, which is its role's name for an agent in an orchestration until you rename it, and for an orchestration agent it puts the run's name in front, for example `prd-1401 / coder`. Beside the name, in muted text, is the agent's command, for example `codex`; a renamed orchestration role keeps its role there too, for example `Coder · claude`. Then come its status and badges, and the pencil that renames it. A long name is cut short, and hovering it shows the whole name. The TUI card's title carries the same agent name; the TUI shows an orchestration's agents on that orchestration's own tab, so its cards don't repeat the run's name. When the agent has ended its turn but left commands or subagents running, a line below that says so, for example `Background: 2 commands, 1 subagent · 3h 12m`, in the same words as the TUI card; see [Session Management → Work left running after a turn](../session-management.md#work-left-running-after-a-turn).

Between the header and the terminal, the pane shows the agent's assignment and two readings: **TIME**, how long the agent has been running (hover it for the exact time it started), and **TOOLS**, how many tool calls it has reported. TIME reads `—` when the daemon did not say when it started the agent.

What can go wrong in the pane:

- **"No terminal here: the desktop has no live connection to <daemon>…"**: the agent's daemon stopped answering. The header then shows the last status the app saw. The terminal comes back on its own when the daemon answers again.
- **"Terminal input unavailable: this agent has no live terminal, only its history"**, or **"Terminal input unavailable: this agent has no live terminal"**: the pane shows the agent's output but cannot take typing, so the terminal is read-only and this line stays under it. In the TUI, the same words appear in the status line when you press `Enter` on that agent's card or press that card's number, and the dashboard stays where it is. If the agent's terminal stops being live while you are typing into it, the TUI shows the same words when you next type, drops that typing and goes back to command mode.
- **"[agent-deck] Input not delivered: <why>"**, written into the terminal: the daemon turned down what you just typed, so it did not reach the agent. The reason says why, for example `the agent has exited` or `the agent in this pane was replaced or closed`. In the TUI, the status line shows the same sentence without the `[agent-deck]` prefix, `Input not delivered: <why>`, and the TUI goes back to command mode, so the keys you press next are not sent to the pane.

## Pull requests

When an agent works on a branch that has a pull request on GitHub, the app shows a badge for it on the agent's row on the Dashboard and in the header of its pane. The badge is GitHub's icon for the pull request's state (open, draft, merged or closed), its number, then its CI checks as GitHub's pull request list shows them (a green check when they passed, a red cross when one failed, an amber dot while they run) and its review (approved, changes requested or review required) in a filled circle, all in GitHub's icons and colours. The circle is what tells an approval from passing CI, since both are a check mark. A pull request without CI checks shows no CI icon, and one with no review yet shows no review icon. Hover the badge to read the same in words, with how many checks are in each result, for example `Pull request #1234: open, review required; CI: 1 failing, 24 passing`. The CI result is GitHub's overall one, so a failing check that is not required for merging still shows the red cross.

The TUI shows the same badge on the bottom border of the agent's card, as glyphs in the same order: the number, the state, the CI result (`✓` passed, `✗` failed, `●` running) and the review (`✓` approved, `±` changes requested, `◉` review required) on a filled cell, for example `#1234 ⊙ ✓ ◉`. The TUI has no tooltip, so it shows no counts; see [Session Management → Pull request badge](../session-management.md#pull-request-badge) for every glyph.

The badge appears by itself: you do not link the pull request. An agent gets one when its working directory is a git repository with a GitHub `origin`, it is on a branch other than the repository's default branch, and that branch has a pull request. The badge follows the pull request, so a review, a CI result or a merge shows up on it within about a minute. If you expect a badge and there is none, see [Troubleshooting → An agent has a pull request but no badge](../troubleshooting.md#an-agent-has-a-pull-request-but-no-badge).

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

The TUI shows the same pull request on the agent's card, as `#1234` with a glyph for its state and its review on the card's bottom border (see [Session Management → What a TUI card shows](../session-management.md#what-a-tui-card-shows)), and `o` opens the selected card's pull request in your system browser rather than inside the TUI. The desktop app has no `o` key: a focused row's keys open the agent, so click the badge or say "open the PR for" and the agent's name instead.

## Generate a config with an agent

To have an agent write the project's `.dot-agent-deck.toml` for you, open its pane and press **Generate config** in the pane's header, or, with voice on, say "generate config with" and the agent's name ("generate config" alone in its pane). The **Generate .dot-agent-deck.toml** dialog asks whether to instruct the agent to analyse the project and create one: **Yes** sends the request, **No** sends nothing. After **Yes** the agent's pane opens and shows `Config request sent to the agent's inbox.`, and the agent proposes a config in its pane and writes it once you approve. Pressing **No** while the dialog says `Sending…` closes it without taking the request back, and the app stays where you are. When the request cannot be sent, the Dashboard or the pane says why, for example `This daemon is too old to generate a config. Upgrade the daemon to use Generate config.` In the TUI, select the agent's card in command mode and press `g`.

When the agent's project already has a `.dot-agent-deck.toml`, the button reads **Update config** (`Update config with <name> agent`), the dialog is titled **Update .dot-agent-deck.toml** and asks whether to instruct the agent to analyse the project and update the existing config, and "update config" works by voice as "generate config" does. The agent then reads the existing file and proposes changes to it, keeping what you wrote unless you agree to drop it, and edits it only after you approve. The TUI's footer shows `[Update g]` in the same case. When the deck can't tell whether the file exists, for example because the project is on a network drive that stopped answering, the button reads **Generate config** and the dialog asks whether to instruct the agent to check the project for `.dot-agent-deck.toml` and create it, or propose updates to it if it already exists, in both clients. [Orchestration → Quick setup](../orchestration.md#quick-setup) has the steps, the screenshots and what each message means.

## Closing agents and orchestrations

- **Stop one agent:** press the stop control on its row (`Close <name> agent`), or **Close agent** in its full-window pane. The confirmation, `Close <name>?`, names the daemon the stop request goes to. Press **Close agent**. Closing from the pane returns you to the Dashboard.
- **Stop every role of an orchestration:** press **Close** on its group header. The confirmation lists every role it will stop. Press **Close all N roles** (or **Close 1 role**). You can also do it from any role's own confirmation: for an orchestration role, it offers **Close agent** and **Close orchestration** side by side, names the orchestration and how many roles it has, and says what each choice closes. **Close orchestration** stops the same roles as the group header's **Close**. On the orchestrator it is the highlighted choice; on any other role, **Close agent** is.
- **Stop but keep the worktree:** for a dispatched unit, the confirmation shows a **Keep the worktree** checkbox under the worktree warning. Tick it before you press a close button, and that close leaves the unit's worktree on disk, clean or not, so you can start another agent in it later. On one role of an orchestration, **Close agent** with the box ticked keeps the worktree the roles share even after you close the other roles normally. A notice then reads `Worktree kept on request:` followed by the directory and stays until you dismiss it with its **×**. The checkbox appears only when the daemon can keep a worktree; after an upgrade, restart the daemon to get it.

**Check it worked:** the row, or the whole group, disappears from the section. If the app cannot confirm that some roles stopped, it says how many may still be running on which daemon, in a message on the Dashboard and in the New agent dialog; open the TUI against that daemon, or run `dot-agent-deck daemon status` on its host, to see what is still there.

In the TUI the same things are `Ctrl+w` on a card, and closing an orchestration's tab; its confirmation offers **Close, keep worktree** as a separate choice for a dispatched unit, where the desktop app has the checkbox. The TUI has no single-role close: closing a role's card closes its whole orchestration, so only the desktop app can close one agent of an orchestration and leave the others running.

If a dispatched unit has uncommitted changes, or other worktrees inside its worktree, both clients' close confirmations warn that the worktree will be **KEPT**, naming its directory and any worktrees inside it. After the close, the desktop shows a notice naming the daemon and the directory actually kept; it stays until you dismiss it with its **×**. The TUI reports the kept directory on its status line. A clean worktree is removed without a kept-worktree notice, unless you chose to keep it. See [Dispatcher Mode → Finish up](../dispatcher-mode.md#finish-up) to recover work or clean up leftover worktrees.

## When a daemon is not there

A daemon section shows one of these instead of its agents:

| Title | What it means | What to do |
| --- | --- | --- |
| **Daemon disconnected** | The app is not connected to that daemon, and the sentence under the title says why: no daemon is running there, a daemon is running but the app is not connected to it, or the app cannot tell, for example because the host cannot be reached. | Press the one button the section offers: **Start daemon** when no daemon is running, which asks first and names the machine it starts on, or **Reconnect** otherwise. See [Daemons → Start a daemon from the app](daemons.md#start-a-daemon-from-the-app), which also lists what a failed start says. |
| **Waiting for this daemon** | The app is still connecting to it. | Nothing: its agents appear when it answers. If it never does, press **Test connection** for it in [Settings → Daemons](daemons.md#test-connection). |
| **Establishing control channel** | The app is reading the daemon's agent list. | Nothing. |
| **Daemon not configured** | A remote daemon has no socket path yet. | Open [Settings → Daemons](daemons.md), choose it and press **Test connection**, which finds the path. |
| **Incompatible daemon** | A daemon answered, but it and the app are different versions, so the app will not use it. The message says which of the two is older, and either that the app has not connected because it could misread what the daemon reports, or that the two cannot work together at all. | Update the older of the two (see [Installation → Keep the app and the daemon on the same release](../installation.md#keep-the-app-and-the-daemon-on-the-same-release)). The note says what each of its buttons does: **Upgrade**, offered when a remote daemon is older than the app, installs the app's version there and restarts the daemon, asking before it stops any running agent ([Daemons → Upgrade a remote daemon](daemons.md#upgrade-a-remote-daemon)); **Replace daemon**, offered on the daemon on this machine, stops it and starts the daemon that came with the app, asking before it stops any running agent; **Open daemons** goes to the Daemons screen; **Connect anyway**, offered only when the message says the app could misread the daemon (never when the two cannot work together), uses the daemon as it is until you quit the app, though some of what it shows may be wrong. The choice covers the whole app session, not only this daemon: until you quit, the app also uses any other daemon it would refuse for the same reason, on any deck, including one that replaces this daemon later; **Reconnect** tries again. **Technical details** under the message shows the exact versions, for a bug report. |
| **Desktop bridge error** | The app could not read the daemon's agents, and no daemon answered for it to judge. This is a problem on the app's side, not a version difference. | Press **Reconnect**. If it keeps happening, restart the app. |
| **No agents are running yet** | The daemon is running and has no agents. This is what a fresh install looks like. It is one line across the daemon's section, with a **New agent** button. | Press **New agent**. While this machine is the only daemon you have added, the note also offers **Add a daemon**, which opens the form for adding a daemon on another machine ([Daemons](daemons.md#watch-a-daemon-on-another-machine)). |

When a daemon restarts while the app is connected to it, the agents it was running can no longer be reached, so the app removes their tiles and open panes and shows one notice in the corner naming each of them, with its role and orchestration, such as `The deck restarted; these agents were stopped: coder (worker in team), api-fix.` The notice lists every agent, scrolling when the list is long, and stays until you close it with its `×`; agents a later restart stops join it. Nothing is stopped or deleted to remove them. The TUI shows the same notice above its footer. When the restart left work behind, the notice also says how many interrupted items there are, with a **Show** button, and the daemon's section shows a banner such as `3 interrupted items from before the restart` with **Show** and **Hide**: **Show** opens the **Interrupted work** dialog, where you can reclaim a unit's worktree or dismiss an item ([Interrupted Work](../interrupted-work.md)). See [The deck restarted and its agents disappeared](../troubleshooting.md#the-deck-restarted-and-its-agents-disappeared).
