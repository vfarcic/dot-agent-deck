# Session Management

This page says what each agent status means in the TUI and the desktop app, what a TUI card shows, how to leave agents running and come back to them, and how to start from an empty workspace.

## Session Statuses

The daemon sets each agent's status from the events its hooks report (see [Installation → Agent hooks](installation.md#agent-hooks)). The TUI shows it on the agent's card; the desktop app shows it in the **Status** column of the [Dashboard](desktop/dashboard.md); `dot-agent-deck daemon status` prints the daemon's name for it.

**TUI:**

![The TUI with four agent cards, each with its status in the card's title row: Idle, Working, Working and Needs Input](/img/dashboard-tui.png)

**Desktop:**

![The desktop app's dashboard with four agents, each row starting with its status: idle, running, running and needs input](/img/dashboard-desktop.png)

| TUI card | `daemon status` | Desktop app | Meaning | What to do |
|---|---|---|---|---|
| **Thinking** | `Thinking` | running | The agent is reasoning before it acts. | Nothing. |
| **Working** | `Working` | running | The agent is running a tool; the card shows which. | Nothing. |
| **Compacting** | `Compacting` | running | The agent is compressing its context window. | Nothing. |
| **Needs Input** | `WaitingForInput` | needs input | The agent is waiting for a permission answer or other input. | Answer it in the pane. In the TUI's command mode, `y` / `n` on the selected card sends approve / deny to a pending permission request. |
| **Idle** | `Idle` | idle | The agent finished its turn and is waiting for a prompt. | Give it the next prompt. |
| **Error** | `Error` | failed | The agent reported a failure, including a turn its provider rejected for a reason other than a usage limit (an API error, a model the account cannot use). | Read the pane. |
| **Blocked** | `Blocked` | blocked | The agent's provider refused it because a usage limit or credit pool is exhausted. | Wait for the limit to reset, switch account or provider, or add credit. See [Blocked](#blocked). |
| **No agent** | — | — | The pane is not running an agent the deck recognises (for example a plain shell), or the agent has not reported yet. | If it is an agent, see [Troubleshooting → Hooks](troubleshooting.md#hooks). |

A newer daemon can report a status this build does not know; both the TUI and the desktop app show it as **Idle**.

### Which agents report which status

The first five statuses come from each agent's hooks, plugin or extension, and how finely an agent separates them depends on what it reports: Pi's extension, for example, reports **Thinking**, **Working** while a tool runs, and **Idle**; Pi has no permission prompt for it to report, so a Pi card does not show **Needs Input**. Error and Blocked depend on the agent:

| Status | Claude Code | Codex | OpenCode | Pi | Devin |
|---|---|---|---|---|---|
| **Error** for a failed provider turn | Yes, 2.1.78 or newer | Yes, a few seconds after the failure | Yes | No | No |
| **Blocked** | Yes, 2.1.78 or newer | Yes | Yes, once OpenCode stops retrying (it shows **Thinking** while it retries) | No | No |
| A subagent's permission prompt told apart from the main agent's | Yes | Yes | No | No | No |

- A Codex card follows Codex's hooks the way a Claude Code card does. In the TUI it shows **Idle** from the moment Codex starts until you send a prompt, **Thinking** and **Working** (with the tool) during the turn, **Needs Input** while a permission prompt is on screen, and **Idle** again when the turn ends, however much Codex redraws its screen (in the desktop app: **idle**, **running** while the turn runs, **needs input** at a permission prompt, and **idle** again). That needs the deck's Codex hooks to be trusted. When they are not (see [Troubleshooting → Codex events not showing](troubleshooting.md#codex-events-not-showing)), the card can only tell whether Codex's screen is changing: the TUI shows **Thinking** while Codex is drawing, including while you type into it and for a few seconds after it starts, and **Idle** once the screen has been still for about three seconds. In that case it shows no tool, no prompt and no **Needs Input**, and a turn waiting on a permission prompt reads **Idle**.
- For Claude Code, Error and Blocked need the `StopFailure` hook, which the deck installs only when `claude --version` reports 2.1.78 or newer. After upgrading Claude Code to 2.1.78 or newer, run `dot-agent-deck hooks install` (or restart the daemon) to add it.
- OpenCode reaching Anthropic through an Anthropic "credit balance is too low" error does not turn the card Blocked; that error carries no marker the deck can read.
- Where a subagent is told apart: when a subagent's permission prompt is abandoned (the subagent stops or fails without it being answered), the card goes back to Idle if the main agent's turn had ended, or to Thinking if it is still running.

### Blocked

In the TUI, a line under `Dir:` says which limit was hit and, when the provider says, when it resets. In the desktop app, the reason is shown in the agent's pane. The status stays until the agent shows it is working again (a new prompt, a tool call or a permission prompt from the main agent) or its pane restarts. There is no timer: a spent credit pool does not reset by itself. A subagent hitting a limit does not turn the card Blocked.

When a worker in an [orchestration](orchestration.md) that still owes a `work-done` turns Blocked, the daemon sends its orchestrator a one-time report, so the orchestrator can reassign the task or notify you; see [Idle Workers & Notifications](idle-workers-and-notifications.md).

If Blocked never appears for Claude Code or OpenCode, the hook or plugin that reports it may not be installed. The deck installs them at startup only into configuration directories that already exist (`~/.claude`; `~/.config/opencode`, `$XDG_CONFIG_HOME/opencode` or `~/.opencode`). If the agent had never run on this machine when the deck started, run `dot-agent-deck hooks install` or `dot-agent-deck hooks install --agent opencode`, then restart the agent. On a remote host, run the same commands there.

## What a TUI card shows

*This section is about the TUI. The desktop app's rows and columns are described on [Desktop app → Dashboard](desktop/dashboard.md#rows-and-columns).*

- **Title row**: the card number, the agent type, the pane's display name (or the session id if it has none), and on the right an animated dot and the status.
- **`Dir:`**: the basename of the working directory, shortened with `…` when it does not fit.
- **`Prmt:`**: the most recent prompt or prompts.
- **Recent tool calls**: the last commands the agent ran.
- **`Last:` and `Tools:`**: time since the agent's last activity and its total tool-call count, in the bottom-right border. Narrow cards shorten them to `2m · 14 tools`, then `2m · 14`, and the narrowest omit them.

![Single agent card showing directory, last activity, tool count, recent prompt, and recent tool calls](/img/session-management-card.jpg)

The deck picks a density from how many cards it has to fit and the space available:

| Density | Prompts shown | Recent tool calls shown |
|---|---|---|
| Spacious | up to 3 | up to 3 |
| Normal | 1 | up to 3 |
| Compact | 1 | 1 |
| Minimal | none | none |

Minimal is used only when there are more cards than fit at Compact. Each card is then three rows: the title row, `Dir:`, and the bottom border with `Last:` and `Tools:`, so every card stays on screen instead of some being scrolled off. On a Blocked card the reason takes the place of `Dir:`, and on an orphaned card `Orphaned — delegation unavailable` does. When even Minimal cannot fit every card, the deck goes back to Compact cards and you scroll with the selection keys (`j`/`k` by default); the title row then shows how many cards are above or below the window.

![Five agents running in parallel — cards switch to Compact density to fit them all without scrolling](/img/home-hero-dashboard.jpg)

### Diagnostic markers on a card

| Marker | Where | Meaning | What to do |
|---|---|---|---|
| ` orphaned ` in the title, and `Orphaned — delegation unavailable` under `Dir:` | An orchestration role's card | The daemon that registered this pane's orchestration role was stopped or restarted while the agent kept running. The agent still works and reports status, but `dot-agent-deck delegate` from it is refused with `the daemon holds no orchestration role for pane …`. | Close the orchestration and start it again. See [Troubleshooting](troubleshooting.md#an-orchestration-stops-being-able-to-delegate-the-daemon-holds-no-orchestration-role-for-pane-). To avoid it, let `dot-agent-deck daemon stop` refuse rather than passing `--force` while an orchestration runs. |
| ` history ` in the title | A session the deck shows but does not drive, such as a Codex session run under `dot-agent-deck wrap` in another terminal | The deck shows its status but cannot type into it. | Type into it in the terminal where it runs. |
| ` view-only ` in the title | A session whose input channel this build does not recognise (for example, reported by a newer daemon) | The deck shows it but cannot deliver input to it. | Type into it where it runs, or upgrade this client. |

### Agents the deck did not start

*This section is about the TUI. The desktop app lists the agents the daemon started, so an agent you start yourself has no card there.*

An agent you start yourself, outside the deck, still gets a card when its hooks report to the deck. Such a card is removed when that agent's session ends, rather than being left behind as an empty card. The deck keeps up to 256 of these cards at a time: when another arrives beyond that, the one that has been quiet the longest is removed, and it comes back the next time that agent starts a session. Cards of agents the running daemon started never count toward that limit and are never removed to make room. An agent that kept running across a daemon restart was started by the previous daemon, so in a TUI you open after the restart it counts as one of these cards. A TUI that stayed open through the restart keeps the card it already had for that agent instead: the card keeps updating, does not count toward the limit and is never removed to make room, and when the agent's session ends the card stays and shows Idle, still marked orphaned if the agent belonged to an orchestration, rather than disappearing. A TUI older than the daemon leaves an empty card for the pane where one of these cards is removed.

## Resuming Sessions

*This section is about the TUI. The desktop app keeps no workspace of its own: it shows the agents the daemon has, and closing it leaves them running.*

To leave the TUI, press `Ctrl+c` in command mode (press `Ctrl+d` first if you are typing in a pane). The quit dialog opens:

![The Quit dialog, headed “Quit dot-agent-deck?”, offering three options: Detach, currently selected, described as “leave agents running on the daemon”; Stop, “shut down agents and daemon”; and Cancel, “return to dashboard”. A clickable row of Detach, Stop and Cancel buttons sits below them, above the hint “Up/Down: navigate, Enter: confirm, Esc: cancel”](/img/detach.webp)

- **Detach** (the default): the TUI exits and the agents keep running in the daemon.
- **Stop**: stops the agents and the daemon. While agents are running it asks once more first.
- **Cancel**: back to the dashboard.

The keys are in [Keyboard Shortcuts → Dialogs](keyboard-shortcuts.md#dialogs).

Every `dot-agent-deck` launch (and `dot-agent-deck connect <name>` for a [remote](remote-environments.md)) restores your workspace automatically. What comes back depends on whether the agents are still running:

- **They are still running** (you detached, or the terminal closed): the dashboard shows each agent with its live output and its current status, tool, tool count and recent prompts. An agent that was waiting for you shows **Needs Input** straight away.
- **They are gone** (a reboot, a fresh machine, or the daemon stopped): the deck recreates the workspace (panes, names, directories, commands and tabs) and starts each command again. It restores the layout, not the agents' conversations; use the agent's own resume option in its command, for example `claude --continue`.

If there is nothing to restore, you get an empty dashboard. A pane whose saved directory no longer exists is skipped with a warning.

**Check:** after relaunching, the cards match what you left; `dot-agent-deck daemon status` lists the same agents.

### You come back where you were

The deck remembers which tab you were on and which pane was focused in each tab, including whether you were on the dashboard. When the agents are still running, reattaching puts you back on that tab and pane. A pane you closed in the meantime, or a role whose agent has finished, is not restored; that tab falls back to its start role. With nothing remembered (a first run), you land on the first orchestration tab if there is one, otherwise the dashboard.

When the agents are gone and the panes are recreated, only the tab is restored, not the focused pane.

The position is saved with the rest of the workspace, one per user account on the machine. If you run two TUIs at once, the one you close last decides what the next launch restores.

### Your setup stays up to date

The workspace is saved after every new agent, rename, tab change and agent change, and when you disconnect, so after an unexpected shutdown you come back to your latest setup.

### Orchestration tabs come back too

Orchestration tabs return with the orchestrator and its prompt, the role panes in their original order, and the start-role cursor where you left it. If the project's `.dot-agent-deck.toml` has changed since (the file is missing, the orchestration was renamed or a role was removed), the deck shows a warning and restores that pane as a plain dashboard pane instead.

### Starting Fresh

To start the next launch from an empty dashboard, clear the saved workspace:

```bash
dot-agent-deck snapshot clear
```

It prints ``Cleared the local saved-session snapshot. The next `dot-agent-deck` startup will begin from an empty dashboard.`` It does not stop running agents, and the next launch still shows any agents the daemon is running; close them first, or quit with **Stop**, for a truly empty dashboard.

The saved workspace is `~/.config/dot-agent-deck/session.toml` (`DOT_AGENT_DECK_SESSION` overrides the path). `dot-agent-deck remote remove <name>` does not clear it.
