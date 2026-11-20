# Interrupted Work

When the deck daemon restarts, the agents it was running stop. The units a dispatcher started, the orchestrations, the scheduled issue dispatches and the agents you opened yourself leave their branches, worktrees and reports behind, and the restarted deck no longer runs any of them. **Interrupted work** is the list of what a restart left behind, so you can pick up where it stopped, clean up the worktrees you no longer need, and read the reports nobody received. The TUI, the desktop app and the `dot-agent-deck interrupted` command show the same list with the same words.

## What is listed

Each item is one piece of work that a restart stopped before it finished:

- **Units** that `dot-agent-deck dispatch` started, single agents and orchestrations alike, with the unit's branch (`agent/dispatch-<name>`) and worktree. A unit whose agent had started but had not yet received its task when the daemon stopped is listed too, and its worktree and branch are kept for it.
- **Orchestrations** you started from the TUI or the desktop app, or that a schedule started, as one item for the whole team.
- **Issue dispatches** a schedule started, with the issue they were working on.
- **Other agents**: agents you started yourself, or that a plain schedule started, in a folder of yours rather than in a worktree the deck created. They are listed in a group of their own, under **Other agents**.

Each item shows what the deck knows about it: its name, branch and worktree (a plain agent's folder), the dispatcher or schedule that started it, its last recorded step and when, and how far it got. When the deck can read them, it also shows the worktree's Git facts on one line: whether the worktree is still on disk, whether it has uncommitted changes, how many commits are on no remote, whether the branch still exists, and the state of the branch's pull request (`pull request open`, `merged`, `closed`, `no pull request`, or `pull request unknown` when the GitHub CLI could not tell, for example because it is not installed or signed in, or did not answer in time).

Work that finished, work you closed and work that is still running are listed only when they carry a report nobody received (below).

The list is not a permanent inventory. The deck forgets old work after a while, so older items can leave the list on their own even when you neither reclaimed nor dismissed them, and their worktrees and branches stay on disk when they do. Reclaim what you no longer need while it is listed.

### Reports nobody received

A worker or a unit sends its report to the orchestrator or dispatcher that gave it the task. When that agent is no longer running, the deck keeps the report instead of handing it to whoever took its place, and lists it with the work that sent it. This happens after a restart, and also while the deck keeps running, for example when you closed the dispatcher before its unit reported: the unit is then listed with its report even though no restart happened.

Each kept report is shown with its id and `kept until <date>`, the day the deck deletes it. Open an item's details to read a report's text. Report text is written by an agent, so the deck shows it marked as untrusted, as it does everywhere else.

## Open the list

### TUI

When the daemon the TUI is attached to has interrupted work, a row above the footer says how many items there are, such as `3 interrupted items from before the restart`, with `[Show I]` and `[Hide X]`. The row counts what the daemon listed when the TUI attached, opened the list, or reclaimed or dismissed an item; `I` asks the daemon again, so it also finds a report kept since then. If the daemon could not read its whole work journal and listed nothing, the row says `The deck could not read its whole work journal, so some interrupted work may not be listed.` instead, with the same buttons. In command mode, press `I` (or click `[Show I]`) to open the **Interrupted work** list, and `X` to hide the row. If you are typing in a pane, press `Ctrl+D` first. Hiding the row is not saved: it comes back when the TUI attaches to a restarted daemon or is started again. `I` opens the list whether or not the row is shown.

![The TUI's Interrupted work list, opened with I from the footer row 3 interrupted items from before the restart: the unit fix-login on branch agent/dispatch-fix-login is selected, with its worktree, who started it, its last entry and progress; below it the orchestration release-team, and under Other agents the agent scratch-shell with its folder; the keys j/k select, Enter details, r reclaim, d dismiss, D dismiss all and Esc close are at the bottom](/img/interrupted-work-tui.png)

After a restart, the list of stopped agents that the restart notice opens with `a` ([The deck restarted and its agents disappeared](troubleshooting.md#the-deck-restarted-and-its-agents-disappeared)) also says how many interrupted items there are, or that some may not be listed when the daemon could not read its whole work journal, and `I` opens their list from there.

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
| `Esc`, `q` or `I` | `I` closes from the item list; `Esc` or `q` return from an item's details or a question, then close from the list |
| `X` | Hide the row above the footer; the list stays open and nothing is dismissed |

You can rebind `I` and `X` in `keybindings.toml` (`interrupted_work` and `hide_interrupted_work`, see [Keyboard Shortcuts](keyboard-shortcuts.md#actions-and-defaults)); the keys inside the list are fixed.

### Desktop app

On the agent dashboard, each daemon that has interrupted work shows a banner in its section with the same words, such as `3 interrupted items from before the restart`, and **Show** and **Hide** buttons. **Show** opens that daemon's **Interrupted work** dialog. **Hide** hides the banner until the app connects to a restarted daemon or is started again. A daemon whose list came back empty because it could not read its whole work journal shows a banner too, saying `The deck could not read its whole work journal, so some interrupted work may not be listed.`, with the same buttons. The banner counts what the daemon listed when the app connected to it, opened its dialog, or reclaimed or dismissed an item. After a restart, the notice in the corner that names the stopped agents also says how many interrupted items there are, or that some may not be listed, with a **Show** button that opens the dashboard and, when only one daemon has interrupted work, its dialog.

![The desktop app's Interrupted work dialog for the Local daemon, opened from the banner 3 interrupted items from before the restart: the unit fix-login on branch agent/dispatch-fix-login is selected, with its worktree, who started it, its last entry and progress; below it the orchestration release-team, and under Other agents the agent scratch-shell with its folder; the buttons Details, Reclaim, Dismiss and Dismiss all are at the bottom](/img/interrupted-work-desktop.png)

You can also press `I` on the dashboard. It asks the daemons for their interrupted work again before deciding, so it finds work left since the banner was last updated. With one daemon connected, it opens that daemon's dialog; with several, it opens the dialog of the one that has interrupted work, or, when several have or may have it, the dashboard says which ones and you open the one you want from its banner; a banner you hid comes back for this. If a daemon could not be asked, the dashboard says so rather than that there is nothing. `X` hides the banner, as its **Hide** button does; with several banners on the dashboard, `X` hides none and you use the **Hide** of the one you mean.

In the dialog, click an item to select it, or use the same keys as the TUI: `j` / `k`, `Enter`, `n` / `p`, `r`, `d`, `D`, `y` / `n`, and `Esc`, `q` or `I` as in the TUI: `I` closes from the item list, and `Esc` or `q` return from an item's details or a question, then close from the list. `X` hides the daemon's banner behind the dialog and leaves the dialog open, as the TUI's `X` hides its row. In an item's details, its reports and a question, `j` / `k` scroll the dialog. The buttons are **Details**, **Show reports**, **Next reports**, **Previous reports**, **Reclaim**, **Dismiss**, **Dismiss all** and **Back**.

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

**Reclaim** removes the item's worktree and keeps its branch, so the work you committed is still there and a pull request from it is untouched. It is offered for units, orchestrations and issue dispatches, and it removes a worktree only when the deck created it for that work, as it does for a dispatched unit or an issue dispatch. A team you started in your own folder ran there, not in a worktree the deck created: reclaim refuses it and removes nothing, and you can dismiss it. The same goes for a unit dispatched with `--dir` into a folder that already existed ([Dispatcher Mode → Continue work in an existing directory](dispatcher-mode.md#continue-work-in-an-existing-directory)): that folder was not created for this work, so reclaim leaves it alone. A plain agent's folder is yours too, so there is nothing to reclaim: dismiss it instead.

> **Copy what you want to keep first.** Reclaim deletes the whole worktree folder, including the files Git ignores there, such as build output, local notes or a saved task. The branch keeps only what was committed.

The TUI and the desktop app first check what reclaiming would do and show it, such as `Reclaim would remove its worktree and keep its branch.`, and remove the worktree only when you answer `y` or click **Reclaim** to confirm. From a shell, `reclaim --dry-run` shows the same answer and `reclaim` acts on it. The item then leaves the list: `Reclaimed: its worktree was removed and its branch kept.`

Commits on the branch that are on no remote do not stop a reclaim, since the branch keeps them. The deck counts them and tells you, for example `2 commits on its branch are on no remote; the branch keeps them.`, so you know to push them.

The deck keeps the worktree, removes nothing and says why when:

- it has uncommitted changes: commit or discard them, then reclaim it again;
- other worktrees or Git checkouts lie inside it, of its own repository or of any other, which it names: move or remove those first;
- the deck could not check whether it has uncommitted changes or whether anything like that lies inside it;
- the deck could not get its repository's worktree lock in time, or could not take it at all: reclaim it again;
- it was moved, replaced, created again by a later run or switched to another branch while it was being reclaimed: list the interrupted work again;
- Git could not remove it, or the removal did not finish in time. In that last case it may still be removed, and the deck records it if it is.

It refuses, also without removing anything, when the worktree is no longer on disk, is not a Git worktree, has a different branch checked out than the one the work started on, was not created by the deck, or still has an agent running in it, or when the deck cannot tell whether one is. It also refuses an older item whose work was dispatched again at the same folder, since that worktree now holds the later run's work: `Refused: a later run used this worktree, …`. Dismiss the older item; the later run's item is the one to reclaim if a restart interrupted it too.

It also refuses when it cannot confirm the worktree is still the item's: `Refused: the deck cannot confirm this worktree is still this item's, …`. You see this for a worktree an older version of the deck created, since those do not record which run they were created for, and for one whose record the deck cannot read. Dismiss the item. If you no longer need the worktree, copy out anything you want to keep, including files Git ignores, then run `git worktree remove <worktree>` from the repository, or let `dot-agent-deck worktree reclaim` remove it once its pull request has merged ([Dispatcher Mode → Finish up](dispatcher-mode.md#finish-up)).

The branch is never deleted. A kept branch stops you dispatching a unit of the same name again; delete it with `git branch -D agent/dispatch-<name>` when you no longer need it.

## Dismiss

**Dismiss** takes an item off the list and changes nothing else: its worktree, branch and kept reports stay as they are, and the reports are still deleted on their `kept until` date. In the TUI and the desktop app, `d` and the **Dismiss** button dismiss the selected item at once; asking by voice shows a question first. In both clients **Dismiss all** asks first and dismisses exactly the items its question named, so an item that appears while you are answering is not swept up. From a shell, `dismiss --all` asks on the terminal and then dismisses exactly the items it listed, or dismisses without asking with `--yes`. A dismissal is saved: a dismissed item stays off the list after later restarts, in every client.

A **Dismiss all** of many items, or a `dismiss` naming many, is sent in parts, all to the same daemon. If that daemon is restarted, stops answering or refuses partway through, the rest are not sent, and the answer says how many items were not, or may not have been, dismissed, such as `1 more item may not have been dismissed: the deck daemon was replaced. List the interrupted work again.`; from a shell the command then exits 1. Open the list again to see what is left.

## Agents can list, not change

An agent running in one of the deck's panes can run `interrupted list` and `interrupted show`, so a dispatcher can tell you what a restart left behind. Reclaiming and dismissing are left to you: from an agent's pane they are refused with `Refused: this request came from an agent in a deck pane, …`. Do them from the TUI, the desktop app or a terminal that is not one of the deck's panes.

## Differences between the clients

- The TUI shows the interrupted work of the one daemon it is attached to. The desktop app shows a banner for each daemon on its dashboard, and `I` and "show interrupted work" ask you to pick one when several have interrupted work.
- With several daemons' banners on the dashboard, the desktop app's `X` hides none of them; use the **Hide** button of the one you mean.
- The TUI's `I` and `X` can be rebound in `keybindings.toml`; the desktop app's keys are fixed.
- Voice control is in the desktop app only.

## When something goes wrong

| Symptom | Cause | What to do |
|---|---|---|
| `The running deck daemon does not support interrupted work. …` (exit 2), and the TUI and the desktop app show no row or banner | The daemon was started by an older version of the deck. | Finish or stop the agents it runs, since stopping it stops them, then stop it with `dot-agent-deck daemon restart`. That command only stops the daemon: start the new one by opening the TUI, pressing **Start daemon** in the desktop app, or running `dot-agent-deck daemon serve` where nothing else starts it ([Recycling the daemon](troubleshooting.md#recycling-the-daemon)). |
| `No deck daemon is running.` (exit 3) | Nothing is listening on the deck's socket. | Start the deck, or point `DOT_AGENT_DECK_ATTACH_SOCKET` at the daemon you mean. |
| `The deck could not read its whole work journal, so an item may be missing or out of date.` | Part of the record the list is built from could not be read. | The list shows what could be read. While the deck cannot read its whole work journal, reclaim and dismiss are refused with `Refused: the deck could not read its whole work journal, …` and change nothing. Try again later, since a later read can succeed. If the refusal persists, you can still clean up an item's worktree and branch yourself with Git: first copy out anything you want to keep, including files Git ignores, then run `git worktree remove <worktree>` from the repository, and `git branch -D <branch>` only if you no longer need the branch's commits. This does not take the item off the list. To report the problem, include the [deck log](troubleshooting.md#enabling-debug-logs). |
| `The list could not be refreshed, so it may be out of date.`, followed by why | The TUI or the desktop app asked the daemon for the list again and got no answer, so it still shows the list from the last answer. | Items may since have been reclaimed, dismissed or added. Close the list and open it again to ask once more; the sentence goes away when the daemon answers. If it keeps failing, check that the daemon is running (`dot-agent-deck daemon status`). |
| `pull request unknown` | The GitHub CLI is not installed or signed in, or did not answer in time. | Check `gh auth status`. Reclaim does not depend on it. |
| `No interrupted item has that id, name or branch.` | Nothing listed matches what you typed, or another client has since dismissed or reclaimed it. | Run `interrupted list` and use the id it prints. |
| `More than one interrupted item matches; use the item's id.` | The name or branch you typed belongs to several items. | Use the item's id. |
| `close <name>` answers `unknown-unit` after a restart | Units do not survive a restart, so `close` cannot reach them. | Find the unit in `interrupted list`, and reclaim or dismiss it there. |
