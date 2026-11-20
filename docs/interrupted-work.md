# Interrupted Work

When the deck daemon restarts, the agents it was running stop. The units a dispatcher started, the orchestrations, the scheduled issue dispatches and the agents you opened yourself leave their branches, worktrees and reports behind, and the restarted deck no longer runs any of them. **Interrupted work** is the list of what a restart left behind, so you can pick up where it stopped, clean up the worktrees you no longer need, and read the reports nobody received. The TUI, the desktop app and the `dot-agent-deck interrupted` command show the same list with the same words.

## What is listed

Each item is one piece of work that a restart stopped before it finished:

- **Units** that `dot-agent-deck dispatch` started, single agents and orchestrations alike, with the unit's branch (`agent/dispatch-<name>`) and worktree.
- **Orchestrations** you started from the TUI or the desktop app, or that a schedule started, as one item for the whole team.
- **Issue dispatches** a schedule started, with the issue they were working on.
- **Other agents**: agents you started yourself, or that a plain schedule started, in a folder of yours rather than in a worktree the deck created. They are listed in a group of their own, under **Other agents**.

Each item shows what the deck knows about it: its name, branch and worktree (a plain agent's folder), the dispatcher or schedule that started it, its last recorded step and when, and how far it got. When the deck can read them, it also shows the worktree's Git facts on one line: whether the worktree is still on disk, whether it has uncommitted changes, how many commits are on no remote, whether the branch still exists, and the state of the branch's pull request (`pull request open`, `merged`, `closed`, `no pull request`, or `pull request unknown` when the GitHub CLI could not tell, for example because it is not installed or signed in, or did not answer in time).

Work that finished, work you closed and work that is still running are listed only when they carry a report nobody received (below).

### Reports nobody received

A worker or a unit sends its report to the orchestrator or dispatcher that gave it the task. When that agent is no longer running, the deck keeps the report instead of handing it to whoever took its place, and lists it with the work that sent it. This happens after a restart, and also while the deck keeps running, for example when you closed the dispatcher before its unit reported: the unit is then listed with its report even though no restart happened.

Each kept report is shown with its id and `kept until <date>`, the day the deck deletes it. Open an item's details to read a report's text. Report text is written by an agent, so the deck shows it marked as untrusted, as it does everywhere else.

## Open the list

### TUI

When the daemon the TUI is attached to has interrupted work, a row above the footer says how many items there are, such as `3 interrupted items from before the restart`, with `[Show I]` and `[Hide X]`. In command mode, press `I` (or click `[Show I]`) to open the **Interrupted work** list, and `X` to hide the row. If you are typing in a pane, press `Ctrl+D` first. Hiding the row is not saved: it comes back when the TUI attaches to a restarted daemon or is started again. `I` opens the list whether or not the row is shown.

After a restart, the list of stopped agents that the restart notice opens with `a` ([The deck restarted and its agents disappeared](troubleshooting.md#the-deck-restarted-and-its-agents-disappeared)) also says how many interrupted items there are, and `I` opens their list from there.

In the list:

| Key | Action |
|---|---|
| `j` / `k` or `Down` / `Up` | Select an item |
| `Enter` | Open the item's details; in the details, show the text of its kept reports |
| `n` / `p` | In the details, the next or previous page of reports, when an item has more than one page |
| `r` | **Reclaim** the selected unit, orchestration or issue dispatch (asks first) |
| `d` | **Dismiss** the selected item |
| `D` | **Dismiss all** (asks first) |
| `y` / `n` | Answer a reclaim or dismiss-all question |
| `Esc`, `q` or `I` | Close the list (`Esc` and `q` go back from the details or a question first) |

You can rebind `I` and `X` in `keybindings.toml` (`interrupted_work` and `hide_interrupted_work`, see [Keyboard Shortcuts](keyboard-shortcuts.md#actions-and-defaults)); the keys inside the list are fixed.

### Desktop app

On the agent dashboard, each daemon that has interrupted work shows a banner in its section with the same words, such as `3 interrupted items from before the restart`, and **Show** and **Hide** buttons. **Show** opens that daemon's **Interrupted work** dialog. **Hide** hides the banner until the app connects to a restarted daemon or is started again. After a restart, the notice in the corner that names the stopped agents also says how many interrupted items there are, with a **Show** button that opens the dashboard and, when only one daemon has interrupted work, its dialog.

![The desktop app's Interrupted work dialog for the Local daemon, opened from the banner 3 interrupted items from before the restart: the unit fix-login on branch agent/dispatch-fix-login is selected, with its worktree, who started it, its last entry and progress; below it the orchestration release-team, and under Other agents the agent scratch-shell with its folder; the buttons Details, Reclaim, Dismiss and Dismiss all are at the bottom](/img/interrupted-work-desktop.png)

You can also press `I` on the dashboard. With one daemon listing interrupted work, it opens that daemon's dialog; with several, the dashboard says which ones and you open the one you want from its banner.

In the dialog, click an item to select it, or use the same keys as the TUI: `j` / `k`, `Enter`, `n` / `p`, `r`, `d`, `D`, `y` / `n`, and `Esc`, `q` or `I` to close. The buttons are **Details**, **Show reports**, **Next reports**, **Previous reports**, **Reclaim**, **Dismiss**, **Dismiss all** and **Back**.

By voice ([Voice Control](desktop/voice.md#interrupted-work)), say:

- **"show interrupted work"** (or "what did the restart leave behind?") to open the dialog. With several daemons, name the one you mean, for example "show interrupted work on build box".
- **"reclaim the first one"**, or name the item, such as "reclaim fix-login". The app checks what reclaiming would do and reads the answer back; the reclaim itself waits for you to confirm on screen.
- **"dismiss the second one"**: the dialog asks whether to dismiss that item, and you confirm on screen.
- **"dismiss all"**: the dialog asks whether to dismiss every item, and you confirm on screen.
- **"show the details of the first one"**, **"show its reports"**, **"next reports"** and **"previous reports"** to read an item on screen. The app does not read them aloud.

### From a shell

```bash
dot-agent-deck interrupted list                 # every item, newest first, with its id
dot-agent-deck interrupted show <id>            # one item, with the text of its kept reports
dot-agent-deck interrupted reclaim <id>         # remove its worktree, keep its branch
dot-agent-deck interrupted dismiss <id>...      # stop listing these items
dot-agent-deck interrupted dismiss --all        # stop listing every item (asks first)
```

`<id>` is the id `list` prints, or the item's name or branch when only one item has it, or the end of its id.

| Option | Command | What it does |
|---|---|---|
| `--json` | `list` | Print the list as JSON, for a script |
| `--no-git` | `list` | Leave out the Git and pull request facts, so no `git` or `gh` runs and the list answers sooner |
| `--report <REPORT_ID>` | `show` | Show the text of one of the item's kept reports |
| `--reports-from <N>` | `show` | Show the item's kept reports from the Nth on (counting from 0), a page at a time; each page says where the next one starts |
| `--dry-run` | `reclaim` | Say what reclaiming would do, and change nothing |
| `--yes` | `dismiss --all` | Dismiss every item without asking. Without a terminal to ask on, `--all` needs it |

| Exit status | Meaning |
|---|---|
| 0 | Done |
| 1 | Refused, kept the worktree, named no listed item, or failed |
| 2 | The running daemon does not support interrupted work |
| 3 | No deck daemon is running |

## Reclaim a worktree

**Reclaim** removes the item's worktree and keeps its branch, so the work you committed is still there and a pull request from it is untouched. It is offered for units, orchestrations and issue dispatches, which run in a worktree the deck created. A plain agent's folder is yours, so there is nothing to reclaim: dismiss it instead.

The TUI and the desktop app first check what reclaiming would do and show it, such as `Reclaim would remove its worktree and keep its branch.`, and remove the worktree only when you answer `y` or click **Reclaim** to confirm. From a shell, `reclaim --dry-run` shows the same answer and `reclaim` acts on it. The item then leaves the list: `Reclaimed: its worktree was removed and its branch kept.`

Commits on the branch that are on no remote do not stop a reclaim, since the branch keeps them. The deck counts them and tells you, for example `2 commits on its branch are on no remote; the branch keeps them.`, so you know to push them.

The deck keeps the worktree, removes nothing and says why when:

- it has uncommitted changes: commit or discard them, then reclaim it again;
- other worktrees lie inside it, which it names: remove those first;
- the deck could not check whether it has uncommitted changes;
- the deck could not get its repository's worktree lock in time, or could not take it at all: reclaim it again;
- it was moved, replaced or switched to another branch while it was being reclaimed: list the interrupted work again;
- Git could not remove it, or the removal did not finish in time. In that last case it may still be removed, and the deck records it if it is.

It refuses, also without removing anything, when the worktree is no longer on disk, is not a Git worktree, has a different branch checked out than the one the work started on, was not created by the deck, or still has an agent running in it, or when the deck cannot tell whether one is.

The branch is never deleted. A kept branch stops you dispatching a unit of the same name again; delete it with `git branch -D agent/dispatch-<name>` when you no longer need it.

## Dismiss

**Dismiss** takes an item off the list and changes nothing else: its worktree, branch and kept reports stay as they are, and the reports are still deleted on their `kept until` date. In the TUI and the desktop app, `d` and the **Dismiss** button dismiss the selected item at once; asking by voice shows a question first. In both clients **Dismiss all** asks first and dismisses exactly the items its question named, so an item that appears while you are answering is not swept up. From a shell, `dismiss --all` asks on the terminal, or dismisses without asking with `--yes`. A dismissal is saved: a dismissed item stays off the list after later restarts, in every client.

## Agents can list, not change

An agent running in one of the deck's panes can run `interrupted list` and `interrupted show`, so a dispatcher can tell you what a restart left behind. Reclaiming and dismissing are left to you: from an agent's pane they are refused with `Refused: this request came from an agent in a deck pane, …`. Do them from the TUI, the desktop app or a terminal that is not one of the deck's panes.

## Differences between the clients

- The TUI shows the interrupted work of the one daemon it is attached to. The desktop app shows a banner for each daemon on its dashboard, and `I` and "show interrupted work" ask you to pick one when several have interrupted work.
- The TUI hides its row with `X`; the desktop app hides a banner with its **Hide** button and has no key for it.
- The TUI's `I` and `X` can be rebound in `keybindings.toml`; the desktop app's keys are fixed.
- Voice control is in the desktop app only.

## When something goes wrong

| Symptom | Cause | What to do |
|---|---|---|
| `The running deck daemon does not support interrupted work. …` (exit 2), and the TUI and the desktop app show no row or banner | The daemon was started by an older version of the deck. | Restart it onto this version with `dot-agent-deck daemon restart` ([Recycling the daemon](troubleshooting.md#recycling-the-daemon)). |
| `No deck daemon is running.` (exit 3) | Nothing is listening on the deck's socket. | Start the deck, or point `DOT_AGENT_DECK_ATTACH_SOCKET` at the daemon you mean. |
| `The deck could not read its whole work journal, so an item may be missing or out of date.` | Part of the record the list is built from could not be read. | The list shows what could be read. Reclaim and dismiss refuse an item they cannot be sure of; try again later. |
| `pull request unknown` | The GitHub CLI is not installed or signed in, or did not answer in time. | Check `gh auth status`. Reclaim does not depend on it. |
| `No interrupted item has that id, name or branch.` | Nothing listed matches what you typed, or another client has since dismissed or reclaimed it. | Run `interrupted list` and use the id it prints. |
| `More than one interrupted item matches; use the item's id.` | The name or branch you typed belongs to several items. | Use the item's id. |
| `close <name>` answers `unknown-unit` after a restart | Units do not survive a restart, so `close` cannot reach them. | Find the unit in `interrupted list`, and reclaim or dismiss it there. |
