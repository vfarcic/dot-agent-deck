# Interrupted Work

When the deck daemon restarts, the agents it was running stop. The units a dispatcher started, the orchestrations, the scheduled issue dispatches and the agents you opened yourself leave their branches, worktrees and reports behind, and the restarted deck no longer runs any of them. **Interrupted work** is the list of what a restart left behind, so you can pick up where it stopped, clean up the worktrees you no longer need, and read the reports nobody received. With experimental features on, you can also bring a stopped agent back with its conversation ([Restore an agent](#restore-an-agent)). The TUI, the desktop app and the `dot-agent-deck interrupted` command show the same list with the same words.

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

After a restart, the list of stopped agents that the restart notice opens with `a` ([The deck restarted and its agents disappeared](troubleshooting.md#the-deck-restarted-and-its-agents-disappeared)) also says how many interrupted items there are, or that some may not be listed when the daemon could not read its whole work journal, and `I` opens their list from there.

The list shows its keys along its bottom. In the list:

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

You can also press `I` on the dashboard. It asks the daemons for their interrupted work again before deciding, so it finds work left since the banner was last updated. With one daemon connected, it opens that daemon's dialog; with several, it opens the dialog of the one that has interrupted work, or, when several have or may have it, the dashboard says which ones and you open the one you want from its banner; a banner you hid comes back for this. If a daemon could not be asked, the dashboard says so rather than that there is nothing. `X` hides the banner, as its **Hide** button does; with several banners on the dashboard, `X` hides none and you use the **Hide** of the one you mean.

In the dialog, click an item to select it, or use the same keys as the TUI: `j` / `k`, `Enter`, `n` / `p`, `r`, `d`, `D`, `y` / `n`, and `Esc`, `q` or `I` as in the TUI: `I` closes from the item list, and `Esc` or `q` return from an item's details or a question, then close from the list. `X` hides the daemon's banner behind the dialog and leaves the dialog open, as the TUI's `X` hides its row. In an item's details, its reports and a question, `j` / `k` scroll the dialog. The buttons along the dialog's bottom are **Details**, **Show reports**, **Next reports**, **Previous reports**, **Reclaim**, **Dismiss**, **Dismiss all** and **Back**, and with experimental features on, **Restore** ([Restore an agent](#restore-an-agent)), whose keys `s`, `c`, `o` and `w` are the TUI's too.

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
| 2 | The running daemon does not support interrupted work, or, for `restore` and `start-over`, restoring |
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

## Restore an agent

> **Experimental, off unless you turn it on.** Restoring is behind the `experimental` flag. Set `experimental = true` under `[features]` in `.dot-agent-deck.toml`; for the desktop app, turn it on before you start the app ([Configuration → Turn on experimental features](configuration.md#turn-on-experimental-features) has both). With the flag off, the `s` key and the **Restore** button are not there, `interrupted restore` and `interrupted start-over` refuse and say the flag is off, and a restarted agent's conversation can still be picked up by hand, as [The deck restarted and its agents disappeared](troubleshooting.md#the-deck-restarted-and-its-agents-disappeared) describes.

**Restore** brings a stopped agent back with its conversation: the deck starts the agent again with the command it was started with, in the folder it was working in, and the agent continues the same conversation instead of starting a blank one. It is offered for an item that is one agent: an agent you started yourself, or a unit or issue dispatch run by a single agent. An orchestration's agents are listed in its details with what they were (name, role, folder), and say that orchestrations come back in a later version; reclaim or dismiss the orchestration instead.

The TUI and the desktop app ask before restoring, and from a shell you choose with an option. You choose between:

- **Restore and continue**: the agent comes back and picks up where it stopped. If its last turn was cut off, it first checks what already happened, then carries on without doing it again.
- **Restore only**: the agent comes back and waits until you tell it to continue.
- **Start over in this worktree**: offered instead when the agent cannot be restored with its conversation (below). The deck starts the agent's command again in the same folder as a new agent with no memory of the earlier conversation. A dispatched unit or issue dispatch is given its saved task again as its first message; an agent you started yourself is started with no task. The work already committed in the worktree is still there, so read the branch before you let it repeat anything.

The restored agent appears as a new agent on the dashboard, a card in the TUI and a row on the desktop app's dashboard, under the same name, and the item leaves the list. A started-over agent appears the same way.

### What the restored agent is told

As soon as the agent is back in its conversation, the deck sends it one message saying that the deck restarted and resumed its session:

- After **Restore and continue**, the message tells the agent that its previous turn may have been cut off. It asks the agent to check the current state first (`git status`, `git log`, and whether anything was already pushed, opened or sent), then continue from where it stopped without repeating what already happened, and to do nothing and wait for the next message if its previous turn had finished.
- After **Restore only**, the message tells the agent the same, but asks it to do nothing until you tell it to continue, and to check the current state when you do.

A started-over agent gets no such message: it has no earlier conversation to be told about.

### TUI and desktop app

The TUI and the desktop app use the same words and the same keys. Open the **Interrupted work** list (`I`), select the item, and:

| Key | Action |
|---|---|
| `s` | **Restore** the selected item, in the list or in its details: opens the question |
| `c` | In the question, **Restore and continue** |
| `o` | In the question, **Restore only** |
| `w` | In the question, **Start over in this worktree**, when it is offered |
| `Esc`, `q` or `n` | Leave the question without restoring anything |

The question names the agent, and for an agent that can only start over it says why. An item's details list each of its agents with whether it can be restored, and the question also says `Its branch has an open pull request, which the agent may change.` when the list found an open pull request on the item's branch. While a restore is starting the agent, the question stays on screen and takes no other choice. When the restore works, the list closes and the restored agent is on the dashboard; when it does not, the list says why. If the daemon is from a version that cannot restore agents, the TUI or the desktop app says so and restores nothing.

In the desktop app the dialog also has a **Restore** button, in the list and in an item's details, and the question shows **Restore and continue**, **Restore only**, **Start over in this worktree** (when it is offered) and **Back** as buttons. With several daemons, the agent comes back on the daemon whose dialog you used.

By voice ([Voice Control](desktop/voice.md#interrupted-work)), say **"restore the agent"**, **"bring back fix-login"** or **"resume fix-login's conversation"** to open the question for that item. Choosing **Restore and continue**, **Restore only** or **Start over in this worktree** is done on screen, since each starts an agent that spends credits and may push or send.

### From a shell

```bash
dot-agent-deck interrupted show <id>                              # the item's agents, and whether each can be restored
dot-agent-deck interrupted restore <id> --continue                # restore, and let it carry on
dot-agent-deck interrupted restore <id> --only                    # restore, and let it wait for you
dot-agent-deck interrupted start-over <id> --agent <attempt>      # start the agent's command again, without its conversation
```

`interrupted show` lists each of the item's agents with the id to pass to `--agent`, and says whether it can be restored, can only be started over, or neither, and why.

| Option | Command | What it does |
|---|---|---|
| `--continue` | `restore` | Restore and continue. `restore` needs exactly one of `--continue` and `--only` |
| `--only` | `restore` | Restore only |
| `--agent <attempt>` | `restore`, `start-over` | The agent to act on. `restore` acts on every agent of the item that can be restored when you leave it out; `start-over` needs it |
| `--dry-run` | `restore`, `start-over` | Say what would happen, and start nothing. When the item's branch has an open pull request, it also says `Its branch has an open pull request, which the agent may change.` |
| `--yes` | `restore`, `start-over` | Act without asking. Without a terminal to ask on, it is needed. On a terminal, the question names an open pull request on the item's branch, as `--dry-run` does |

### Which agents can be restored

| Agent | Restored with its conversation | What is missing |
|---|---|---|
| Claude Code | Yes | — |
| Codex | Not yet | Codex does not tell the deck which conversation it reopened until it is sent a message, so the deck cannot confirm it came back. Start over is offered |
| OpenCode | Not yet | The deck does not yet learn reliably which conversation the agent was in, so it could bring back an older conversation or a blank one. Start over is offered |
| Pi | Not yet | The deck does not yet learn which conversation the agent was in. Start over is offered |
| Devin | Not yet | It is not yet confirmed that the conversation the deck hears about is the one Devin can resume. Start over is offered |
| Antigravity | Not yet | The deck knows no way to resume an Antigravity conversation by its id. Start over is offered |
| A pane started without an agent, such as a plain shell | No | There is no conversation to restore. Start over is offered |

Some ways of starting an agent cannot be restored, even for Claude Code. The deck then stops the agent it started before sending it anything, and the item stays listed with Start over offered:

- a command that calls the agent by its full path, such as `/usr/local/bin/claude`, rather than by its name;
- a script that sets its own `PATH` before it starts the agent, or a wrapper that drops the arguments it is given;
- a wrapper that starts the agent with a different home or configuration folder than it had, so the agent cannot find its conversation;
- native Windows, which the deck does not run on.

### Things to know

- **A restored unit no longer reports to its dispatcher.** A dispatched unit or issue dispatch comes back as an agent of its own: the dispatcher that started it is not told when it finishes. Read what it did in its pane, and tell the dispatcher yourself if it needs to know.
- **Claude Code may ask for permissions it did not ask for before.** An agent started to skip permission prompts can ask for them again after it is restored. Answer them in its pane.
- **A prompt in the agent's command is sent again.** If the agent was started with a prompt on its command line, such as `claude "fix the login tests"`, Claude Code receives that prompt again as a new message when it is restored. Agents the deck starts normally take their task as a message instead, so this applies only to a command you wrote that way.
- **Starting the TUI again does not start a restorable agent blank.** When the TUI starts with a daemon that runs no agents, it normally starts the panes of your last session again. With restore on, a saved pane whose agent can be restored with its conversation, in the same folder with the same command, is not started again: restore it from **Interrupted work** instead, so you do not get a blank copy beside the restored one. A saved pane whose agent can only start over starts as before.
- **The old agent must be gone.** The deck restores an agent only when it can confirm the agent it stopped is no longer running, so that two copies of it do not write to one conversation. An agent an older version of the deck started cannot be checked that way, so it is offered Start over instead.
- **The deck checks again when you choose.** Restore and Start over each check the item again at the moment they act, not only when the list was read: the worktree must still be there, and no newer agent may be working in the same folder, however that folder is spelled (a link to it, or a path through `..`). Start over is also refused while the item's branch has an open pull request, since the new agent would repeat its task beside it; Restore only mentions the pull request, since the restored agent may be the one working on it.
- **Start over starts a new conversation.** For Claude Code, Codex, OpenCode, Pi and Devin, Start over is refused when the command the deck starts, or the options the agent's program found on `PATH` is started with, continue, resume, fork or open an earlier conversation (such as `--continue`, `--resume`, or `codex resume`), including options a script of yours passes to the agent when it calls it by name; the agent is stopped before it is sent anything. A launch that never starts the agent's program through `PATH` (it calls the agent by its full path, or replaces `PATH`) is refused too. Claude Code is also started under a new conversation the deck chooses, and the deck sends the saved task only once the first conversation Claude Code reports is that one, so a Claude Code whose first report names any other conversation is stopped. Codex, OpenCode, Pi and Devin report nothing the deck can check that way: a script of your own named after one of them that picks a conversation itself is not checked. For every agent, the deck checks the first start: a program your own command or script starts after it, such as a second Claude Code it runs in a conversation you chose, is not checked, just as if you had started it yourself. Antigravity and a plain shell are started over without this check.
- **One restore at a time, even with two decks.** Restore and Start over act one at a time for each state folder: while one is starting its agent, another one, from any client or from another deck that shares the state folder, is refused with `Refused: another restore or start over is in progress. …` and starts nothing. Asking what a restore would do is not refused because another restore is in progress, though it can be refused for other reasons: for example, when it asks about the agent that restore is bringing back, or about an agent of the same kind while that restore is starting its agent.
- **One agent per conversation, even with two decks.** Restoring the same agent, or two agents that were in one conversation, from two clients or from two decks that share a state folder, at once or one after the other, brings back one of them; the other is refused while the restored agent runs. If the deck could not confirm that an agent it stopped is gone, no agent of any item is restored into a conversation the deck knows that agent may be in until it is gone, and when the deck does not know which conversation that is, no agent of the same kind is restored until then. The deck writes this down in its state folder before it starts the agent, and starts nothing when it cannot (`Refused: the deck could not start the agent.`), so it holds for other decks sharing that folder and after a restart.
- **If the restart note cannot be sent, nothing is restored.** The deck checks that the restored agent's message can be delivered before it starts the agent. If the message still cannot be sent once the agent is back, the deck stops the agent again, and the item stays listed so you can try again.

### When an agent is not restored

The deck checks again when you confirm. When a check fails it starts nothing, and when the agent it started did not come back in its conversation it stops that agent before sending it anything. The answer says why, in words such as these:

| What it says | What it means | What to do |
|---|---|---|
| The agent cannot be restored with its conversation yet | The agent is one the table above marks "Not yet", or a plain shell. | Use **Start over in this worktree**. |
| The deck does not know which conversation the agent was in | The agent stopped before it reported its conversation. | Use Start over. |
| The deck has no record of how the agent was started | The agent was started by an earlier version of the deck, which did not record its command. | Dismiss the item, and start the agent yourself in the folder it was working in. |
| An older version of the deck started this agent | That version did not record what the deck needs to check that the old agent has stopped, so it is not brought back with its conversation. | Use Start over, or dismiss the item. |
| The deck cannot confirm the old agent has stopped | The agent was started by an earlier version of the deck, which did not record what the deck needs to check, or your system did not let the deck look at the old agent's processes. | Use Start over, or dismiss the item. |
| The old agent is still running | A process of the stopped agent is still running, and a second copy would write to the same conversation. | Stop that process, then restore again. |
| The agent's folder was moved, is gone, or others can write to it | The deck starts an agent only in the folder it was working in, and only when no other user can change that folder. | Put the folder back, or remove the write access others have, then restore again. Otherwise dismiss the item. |
| The deck does not know which folder the agent ran in | The record of how the agent was started names no folder, so the deck has nowhere safe to start it. | Dismiss the item, and start the agent yourself in the right folder. |
| The restart did not stop this agent | The item is listed for a report nobody received, but its agent finished on its own or is still running, so there is nothing to bring back. | Read the report, then dismiss the item. |
| The deck tried to restore the agent's conversation and could not | An earlier restore of this agent did not bring its conversation back, so only Start over is offered now. | Use Start over, or dismiss the item. |
| This agent is already being restored or started over | Another restore or start-over of the same agent, from this or another client, has not finished yet. | Wait for it, then list the interrupted work again. |
| Another agent is in this conversation now | An agent the deck runs, such as a copy of this one you restored from another item, is in the same conversation, or is being restored into it, or an agent the deck could not confirm it stopped may still be. When the deck does not know which conversation that stopped agent is in, every restore of an agent of the same kind is refused this way until it is gone. | Stop that agent first, or use Start over. If it keeps happening once you stopped it, the [deck log](troubleshooting.md#enabling-debug-logs) names the pane and process the deck is still waiting on. |
| Too many restores are waiting | Many restores are running at once, or are waiting for their agents to start. | Try again in a minute. |
| The deck could not start the agent | The agent's command could not be started, or the deck could not write to its state folder the note it keeps before starting an agent, so nothing was started. | Try again. If it keeps happening, check that the deck's state folder has free space and can be written to, and read the [deck log](troubleshooting.md#enabling-debug-logs). |
| Another restore or start over is in progress | Another restore or start over, from this or another client, or from another deck sharing the state folder, is starting its agent now. One acts at a time. | Wait for that restore to finish, then try again. A restore holds this turn only while it runs, which can take as long as its agent takes to come back. |
| The unit's saved task is not there | Start over gives a dispatched unit its saved task again, and the file in the worktree's `.dot-agent-deck` folder is gone. | Start the agent yourself and give it the task, or dismiss the item. |
| The deck could not confirm that the agent started a new conversation | Start over started the agent, but the deck could not check how it was started: its command calls it by full path, a script replaced `PATH`, or Claude Code came back in another conversation than the new one, or reported none in time. The deck stopped it before sending it anything. | Start the agent yourself, or dismiss the item. |
| The agent's command reopens an earlier conversation | The agent's command, or a script it runs, starts it with an option that continues, resumes or opens a conversation, such as `claude --continue` or `codex resume --last`, so starting it again would not start fresh. Any agent it started is stopped before it is sent anything. | Start the agent yourself without that option, or dismiss the item. |
| The deck could not confirm that all of it stopped | The deck stopped an agent it had just started (a restore that did not take, or a start over in the old conversation), and part of it may still be running. | Find and stop that agent's processes, then try again. Until they are gone, the deck refuses to restore or start over this agent, and to restore any agent into a conversation that agent may be in. |
| Newer work is running in this agent's folder | Another agent the deck runs now was started in the same folder. | Stop that agent first, so two agents do not work in one folder, then try again. |
| Its branch has an open pull request | Start over would start the agent's task again beside the pull request it already opened. | Restore the agent with its conversation instead, or close the pull request first. |
| The deck's inbox cannot take the restart note now | The deck's message queue is full or its size limit is smaller than the restart note, so it started nothing. | Try again in a minute. |
| The agent came back, but the deck could not give it its restart note | The agent did not get the message telling it the deck restarted, so the deck stopped it again. | Try again. The item is still listed. |
| The agent started, but the deck could not record it | The deck could not write down which agent took the stopped one's place, so it stopped the new agent again. | Try again. The item is still listed. If it keeps happening, check that the deck's state folder has free space. |
| The request named an agent twice, named too many agents, or used an id that is too long | The `--agent` options repeat an agent, or name more agents than an item has. | Name each agent once. |
| The deck daemon was restarted or replaced while you were answering | From a shell, the deck that answered the question is not the one running now, so nothing was started. | Run the command again. |
| The agent came back without its conversation | The agent started, but not in its earlier conversation: its command calls it by full path, a script replaced `PATH` or dropped the arguments, the agent could not find the conversation, or it exited. The deck stopped it before sending it anything. | See the list of ways of starting an agent that cannot be restored, above. Use Start over. If none of those applies and the [deck log](troubleshooting.md#enabling-debug-logs) was on, it says why the agent did not come back; include it when you report the problem. |
| The worktree is no longer there | Start over runs in the item's worktree, which is gone. | Dismiss the item. |
| The saved task cannot be read safely | A dispatched unit's saved task file was replaced by a link, or others can read or change it. | Check the file in the worktree's `.dot-agent-deck` folder, or start the agent yourself. |
| Orchestrations come back in a later version | The item is an orchestration, or a unit run by more than one agent. | Reclaim or dismiss it. |
| The deck could not read its whole work journal or all its records of how agents were started | Part of the record restoring is checked against could not be read, or was written by a newer version of the deck. | Try again later. After going back to an older version of the deck, upgrade again to restore. |

## Agents can list, not change

An agent running in one of the deck's panes can run `interrupted list` and `interrupted show`, so a dispatcher can tell you what a restart left behind. Reclaiming, dismissing, restoring and starting over are left to you: from an agent's pane they are refused with `Refused: this request came from an agent in a deck pane, …`. Do them from the TUI, the desktop app or a terminal that is not one of the deck's panes.

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
