# Approving Project Commands

A project's `.dot-agent-deck.toml` decides which commands the deck runs for an orchestration and which instructions it gives the agents in it. A repository you clone, or a change someone pushes to one you already use, can change that file, so the deck shows you what an orchestration would run before it runs it for the first time, and again whenever that changes. You approve it once, and the deck remembers the approval until the text changes.

This page covers both clients and the command line: what the review shows, how to answer it, what is remembered, what happens to work that starts an orchestration on its own, and what the review does not cover.

## When the deck asks

The deck asks before it starts an orchestration that you have never approved, or whose text changed since you approved it:

- **TUI:** when you pick an orchestration in the New Agent form (`Ctrl+n`) and press `Enter`, and when the TUI recreates an orchestration tab from your saved workspace (see [Restoring a saved workspace](#restoring-a-saved-workspace)).
- **Desktop app:** when you press **Activate orchestration** in the [New agent](desktop/new-agent.md) dialog, or start an orchestration from the experimental deck screen.

An orchestration you approved, whose text has not changed, starts with no question, as it did before. Nothing from the orchestration starts while the review is open.

The deck also opens the review on its own when something other than you tries to start an orchestration that is not approved, such as an agent's `dispatch` or a schedule firing ([Work that starts an orchestration on its own](#work-that-starts-an-orchestration-on-its-own)).

## What the review shows

The review is titled **Review project commands** and shows:

- **Project:** the project's directory. For a linked git worktree this is the matching directory of the repository's main checkout, and a **Config in:** line names the worktree directory whose `.dot-agent-deck.toml` is being read.
- **Orchestration:** its name.
- **State:** `not approved before`, or `changed since you approved it`.
- For each role: its name, then `command`, `agent` (when the role sets one), `start` (`true` on the role the orchestration starts with), `clear` (`true` when the role's agent is restarted before each task, `false` when it keeps running between tasks), `description` and `prompt` (the role's `prompt_template`), in full. A role whose text changed since your approval is marked `changed`, and one your approval did not have is marked `new`.
- **Daemon:** in the desktop app, the daemon the orchestration would start on. The approval is recorded on that daemon and the orchestration starts there. If you switch to another daemon while the review is open, the review closes and nothing starts. If you switch before pressing **Approve and start**, nothing is approved either. If you switch after pressing it, the approval may already be recorded on the daemon you started from, and the app tells you the `dot-agent-deck trust revoke` command to run on that daemon's host to undo it.
- **Removed since you approved it:** the roles your approval had that the file no longer defines.

Read the commands and prompts as what they are: commands that will run as you, and instructions your agents will follow. Characters that could hide or disguise text, such as terminal control sequences, are shown as escapes (for example `\u{1b}`) rather than obeyed, in both clients, on the command line and in the refusal messages described below.

## Answer it

**TUI:**

| Key | What it does |
|---|---|
| `a` | Approve exactly the text shown, then start the orchestration (in a restore the footer reads `a approve and restore`). |
| `Esc` | Cancel. Nothing starts and nothing is remembered. |
| `↑` / `↓`, `PgUp` / `PgDn` | Scroll a review taller than the window. `(more above)` and `(more below)` say there is more. |

With the mouse, click `a approve and start` (or `a approve and restore`) in the footer to approve, or `Esc cancel` to cancel, and use the wheel over the review to scroll it. A click anywhere else does nothing.

`Enter` does not approve, and a key you pressed before the prompt appeared does not answer it.

**Desktop app:** the **Review project commands** dialog shows the same text. Press **Approve and start** to approve it and start the orchestration, or **Cancel** (or `Esc`) to start and remember nothing. `Enter` does not approve here either. With [voice control](desktop/voice.md#approving-project-commands) on, say "approve and start" (any request that says *approve*, such as "approve the commands"); "cancel" or "close" cancels.

If the file changes while the review is open, approving does not approve the new text: the review shows the new text instead, and you answer again.

In the desktop app, after you press **Approve and start** the dialog says the approval has been sent. **Cancel** still works then: nothing starts, but the deck may already have recorded the approval. Run `dot-agent-deck trust revoke` on the deck's host if you did not mean to approve it. A spoken "approve" applies only to the review that was on screen when you said it: if that review closed or its text changed before the words were understood, nothing is approved, and you are asked to look again.

## What is remembered

An approval covers one orchestration of one project, and the exact text you were shown. These fields of every role are part of it: `name`, `command`, `agent`, `start`, `clear`, `description` and `prompt_template`, as they read after any [`extends`](orchestration.md#sharing-a-workflow-with-extends). Changing any of them, by even one character, adding a role, removing one or reordering the roles brings the review back for that orchestration. Editing comments, keys outside the roles, or a different orchestration in the same file does not.

The project is the directory holding `.dot-agent-deck.toml`. In a git repository, a linked worktree counts as the main checkout: the same directory in any worktree of the repository is one project. A linked worktree, such as the one each [dispatched unit](dispatcher-mode.md) works in, therefore uses the main checkout's approval: a unit started from an approved orchestration starts without asking, as long as the worktree's file says the same thing.

An orchestration without a `name` is remembered under the name of the project's main checkout, so its worktrees share that approval too. The review and every refusal show that name, and it is the name to give `trust approve` and `trust revoke` with `--orchestration`.

Approvals are kept per user, on the machine the daemon runs on, in `project-trust/approvals.json` in the deck's state directory (`~/.local/state/dot-agent-deck/` unless `XDG_STATE_HOME` or `DOT_AGENT_DECK_STATE_DIR` moves it; see [Configuration → Environment variables](configuration.md#paths-and-endpoints)). For a [remote](remote-environments.md) daemon, that is the remote host.

## Work that starts an orchestration on its own

Some work starts an orchestration, or a role of one, without you choosing it in a client:

- a schedule firing, and **Run now**, for a schedule that opens an orchestration ([Schedules](scheduled-tasks.md));
- a scheduled issue dispatch, before the issue's worktree runs any setup;
- `dot-agent-deck dispatch` when it starts an orchestration, before the unit's worktree runs any setup ([Dispatcher Mode](dispatcher-mode.md));
- an orchestrator's `delegate`, `pane restart` and `pane spawn` ([Orchestration](orchestration.md#commands)). An orchestration you started from a current client was approved then, so for it these are refused when its file changed since.

If the orchestration is not approved as its file now reads, the attempt is refused and nothing from it starts. The deck then asks you for the approval in the TUI and the desktop app attached to it, so you can approve without leaving the deck, and the attempt goes through when it is tried again. An agent whose attempt was refused is told when you answer, so it knows when to try again.

### Answering the deck's request

The deck shows the same **Review project commands** review, led by a sentence saying the deck refused to start the orchestration and an **Asked by:** line naming what tried to start it, such as `dispatch`, `delegate`, `pane restart` or `scheduled task 'nightly'`. It waits while another dialog is open, and for a moment after it appears it ignores keys (in the TUI, until you pause typing), so typing you were in the middle of cannot answer it.

- **TUI:** the review opens over the dashboard or the pane you are typing in, with the footer `a approve · Esc deny · ↑↓ scroll`. `a` (or a click on `a approve`) approves exactly the text shown, and the status line says `Approved orchestration 'team'. It starts when dispatch tries again.`. `Esc` (or a click on `Esc deny`) declines it. You return to where you were.
- **Desktop app:** the dialog names the daemon that asked (**Daemon:**) and offers **Deny** and **Approve**. With **All daemons** selected it shows the requests of every daemon the app watches; with one daemon selected, that daemon's. With [voice control](desktop/voice.md#approving-project-commands) on, "approve" approves, and "cancel" or "close" denies.

Approving records the approval exactly as the New Agent form and `trust approve` do. It starts nothing itself: the attempt that was refused starts when it is tried again. Each agent whose `dispatch`, `delegate`, `pane restart` or `pane spawn` was refused gets a notice from the deck in its inbox saying the orchestration was approved and to retry now, and it decides whether it still needs to. A schedule is not told anything: it runs at its next fire, or now with `dot-agent-deck schedule run-now --name <name>`.

**Deny** refuses the attempt, and its retries, with a message saying you declined it in the deck. Each agent whose attempt was refused gets a notice saying you declined it and not to retry, so it stops waiting. The deck does not ask about that exact text again until the daemon restarts, so a schedule that fires every few minutes does not bring the review back each time. You can still approve it from the New Agent form or with `trust approve`, and a change to the file asks again.

- **Once per change:** a retry while the review is waiting for your answer is refused the same way and opens no second review. After you approve, nothing asks again until the text changes.
- **Answered once, closed everywhere:** answer in one client, and the review closes in the others, which say `The approval request for orchestration 'team' was answered elsewhere.`. Approving with `trust approve`, or changing the file, also closes it within a few seconds.
- **Each waiting agent is told once:** whichever client you answer in, or `trust approve` from a terminal, every agent the request refused gets one notice. If the file changes instead, they are told the request closed without an answer, so they can try again and be asked about the new text. An agent whose pane has closed, or now runs another agent, is not told.
- **No client attached:** the request waits, and the review appears the next time a TUI or desktop app attaches to that deck.
- **A restart forgets it:** the deck keeps waiting requests and your declines in memory only. Restarting the daemon drops them; the next attempt asks again. Approvals are kept on disk and survive a restart, as before.

A TUI or desktop app from an older release shows no request; approve with the command the refusal prints.

### The refusal

Each refusal says which orchestration and project, whether it was never approved or what changed, that the approval was requested in the deck, and the command that approves it from a terminal. An agent's refusal also tells it to wait for the deck's notice instead of retrying:

```text
orchestration 'team' of project /home/you/code/demo was never approved, so nothing from it was started. Its approval was requested in the deck: the user can review it in the dot-agent-deck TUI or desktop app attached to this deck. You will get a dot-agent-deck notice in your inbox when the user approves or declines it: wait for that notice rather than retrying, and once it says approved, run the same command again to start it. Or approve it from a terminal outside the deck with: dot-agent-deck trust approve /home/you/code/demo --orchestration team
```

A schedule's refusal says `Once it is approved, the same attempt, retried, starts it.` in place of the sentence about the notice.

The notice an agent gets when you approve reads:

```text
Orchestration 'team' of project /home/you/code/demo was approved. Retry your dispatch now if you still need it: the approval itself started nothing.
```

After you deny it, the refusal says `… was never approved and the user declined it in the deck, so nothing from it was started. To start it, approve it from the deck's New Agent form or from a terminal outside the deck with: …`.

Where you see it:

- **An orchestrator's command** (`delegate`, `pane restart`, `pane spawn`, `dispatch`): the command fails with that message, so the orchestrator agent reads it and can tell you.
- **A schedule:** `dot-agent-deck schedule list` prints `last fire refused at <time>: <message>` beneath the schedule, and the TUI's Schedules manager shows its status as `refused` and the message beneath the list while the schedule is selected ([The TUI's Schedules manager](scheduled-tasks.md#the-tuis-schedules-manager)).
- **The daemon log**, at warning level ([Troubleshooting → Logs](troubleshooting.md#logs-and-diagnostics)).

Approve from the review in the deck, or run the command it names from a terminal outside the deck, check the text it prints, and approve. The next fire, or the orchestrator's next try, then runs.

A scheduled issue dispatch reuses the issue's existing branch, `agent/issue-<n>`, when an earlier dispatch left one in the schedule's clone. If that branch's `.dot-agent-deck.toml` differs from the approved one on the default branch, the issue is refused without an approve command and opens no review, because approving the branch's version would replace the default branch's approval:

```text
orchestration 'team' on the existing issue branch agent/issue-7 in /home/you/work/issues changed since you approved it (roles: coder (changed)): that branch's .dot-agent-deck.toml differs from the approved one, so nothing was started. Approving the branch's version would replace the default branch's approval, so bring the branch's .dot-agent-deck.toml back in line with the default branch, or, if its work is abandoned, delete the branch with: git -C /home/you/work/issues branch -D agent/issue-7 — the next fire then dispatches the issue fresh. Issues on the default branch keep dispatching.
```

Other issues, on the default branch, keep dispatching meanwhile.

### An orchestrator's instructions after compaction or `/clear`

In the TUI, when the orchestrator's conversation is compacted, or you clear it with `/clear`, the deck sends the orchestrator its instructions again (its prompt, the workers and how to delegate). It sends them only when the orchestration is approved as its file now reads. If the file changed since you approved it, or the orchestration was never approved, nothing is sent and the TUI's status line says why, for example:

```text
Orchestration 'team': its instructions were not sent to its coordinator again, because its config changed since you approved it. Review and approve it from a terminal outside the deck with: dot-agent-deck trust approve /home/you/code/demo --orchestration team
```

The orchestrator keeps running, but without its instructions in its conversation. Approve the change with the command in the message, or revert the file; the next compaction or `/clear` then sends them. If you approve a change that only edits the roles' text, that text is what the orchestrator gets. If the change adds, removes, renames or reorders roles, or moves `start`, the status line says `its roles changed since it was opened. Open it again to use the new roles.`: open the orchestration again from the New Agent form.

The instructions are sent again with the task the orchestration was started with, read from that orchestration's own `orchestrator-context-<id>.md`. When the TUI cannot read that file, it sends them without a task and the status line says why, for example:

```text
Orchestration 'team': instructions re-sent to its coordinator without a task (this tab does not know its context file)
```

That happens for an orchestration you opened with `Ctrl+n` and came back to after detaching, or from a second TUI, which never had a task to send; and for any orchestration whose file was deleted, by hand or because it was more than 14 days old. If the orchestrator was working on a task, give it the task again.

The desktop app does not send the orchestrator's instructions again after compaction or `/clear`, so this does not apply there.

## Manage approvals from the command line

Run these from a terminal **outside** the deck, on the machine the daemon runs on.

| Command | What it does |
|---|---|
| `dot-agent-deck trust list` | One line per approval: project, orchestration, when it was approved, and `current` (the file still says what you approved), `changed` (it says something else) or `missing` (the file or the orchestration is gone). Prints `No approved orchestrations.` when there are none. |
| `dot-agent-deck trust approve [<dir>] [--orchestration <name>] [--yes]` | Prints every role's text, as the review does (with `[changed]` and `[new]` marks), and asks `Approve these commands and prompts? [y/N]`. Only `y` or `yes` approves. `<dir>` is the directory holding `.dot-agent-deck.toml` (default: the current directory); without `--orchestration` it covers every orchestration in that file. |
| `dot-agent-deck trust revoke [<dir>] [--orchestration <name>]` | Removes the approvals of that project, or of one orchestration. The next start asks again, and unattended work for it is refused. Exits non-zero when there was nothing to revoke. |

`--yes` approves without asking, for scripts, and still prints what it approved. Without a terminal to ask on and without `--yes`, `approve` approves nothing and exits non-zero. If the file changes between printing and your answer, nothing is approved and you are told to run it again.

`trust approve` refuses to run inside a pane the deck started, `--yes` included, and says `run it from a terminal outside the deck`, so that an agent working in a deck pane does not approve its own commands with it ([the limits](#what-the-review-does-not-cover) say what this does not stop). Approve from the review in the TUI or the desktop app, including the one the deck opens when it refuses work ([Answering the deck's request](#answering-the-decks-request)), or from a terminal of your own. `list` and `revoke` work anywhere.

## Restoring a saved workspace

When the TUI starts and recreates orchestration tabs from your saved workspace because their agents are gone ([Session Management → Resuming Sessions](session-management.md#resuming-sessions)), it shows the review for each saved orchestration that is not approved as its file now reads, one after the other, with the footer `a approve and restore · Esc cancel · ↑↓ scroll`. The rest of the workspace is restored first. Approving restores that tab, starting the same roles a restore that needed no approval would: those that had a pane when the workspace was saved. Cancelling leaves it out. An orchestration that is approved and unchanged comes back without asking.

An orchestration you cancel, or have not answered yet when you quit, stays in your saved workspace and is offered again the next time the TUI restores it. To drop it for good, clear the saved workspace with `dot-agent-deck snapshot clear` ([Session Management → Starting Fresh](session-management.md#starting-fresh)).

A saved orchestration the TUI can no longer find in its project, because the file is gone, the orchestration was renamed or its roles changed, is not restored at all, and nothing from it starts. When you quit the TUI, it prints a warning naming it, such as `Warning: …; orchestration 'team' was not restored`. Open the orchestration again from the New Agent form; it asks for approval if it needs it.

## After upgrading

Nothing is approved yet the first time you run a release with this review, so:

- each orchestration asks once, the first time you open it in either client, including the tabs the TUI recreates from your saved workspace;
- schedules, issue dispatches and `dispatch` runs that open an orchestration are refused until you approve it once, by opening it in a client or with `dot-agent-deck trust approve`. For an issue-dispatch schedule, the project is the schedule's clone, which exists only after its first run, so expect the first fire to be refused and approve the clone with the command that refusal prints. Then `dot-agent-deck schedule run-now --name <name>` runs it again.

### A daemon from an older release

The review needs a daemon from this release or later:

- **TUI:** against an older daemon, the TUI shows no review. It checks the approvals on the machine it runs on itself: an approved, unchanged orchestration starts, and any other is not started, with a message that the daemon is older and the command that approves it. Approve from a terminal with that command, then open the orchestration again.
- **Desktop app:** against an older daemon, the app starts no orchestration on that daemon and says so, naming the command to run on the daemon's host. Upgrade that daemon ([Daemons](desktop/daemons.md)), or approve on its host and start the orchestration from the TUI there.

An older TUI or desktop app against a daemon from this release cannot show the review either: the older desktop app's start fails, and an older TUI opens the orchestration but its agents' `delegate`, `pane restart`, `pane spawn` and `dispatch` are refused. Upgrade the client, or approve once with `dot-agent-deck trust approve`.

## What the review does not cover

The review is about the commands and instructions in a project's `.dot-agent-deck.toml`. It does not stop everything a project can make your machine run:

- **Programs running as you can approve.** The approvals file and the `trust` command are reachable by any program running under your user account, agents included. The deck refuses `trust approve` inside its own panes, but a program that clears the deck's environment, or writes the file itself, is not stopped. Treat an agent with shell access as able to approve.
- **Where approvals are kept follows the daemon's environment.** The daemon reads its approvals from the state directory that `DOT_AGENT_DECK_STATE_DIR` or `XDG_STATE_HOME` points at when it starts. A daemon started from a shell that loaded a project's environment, for example a `devbox shell` whose init hook loads the project's `.env`, can therefore be pointed at an approvals file the project chose. Start the daemon from a shell of your own, not from inside a project's environment.
- **What the approved commands run next.** The review shows a role's `command`, such as `devbox run agent-coder`. What that runs is decided by other files in the project, such as `devbox.json`, scripts and build files, and the deck does not review those. A dispatched unit's worktree setup (its `devbox` environment) is prepared only after the orchestration's approval is checked, but `devbox.json` itself is not reviewed.
- **Commands you start yourself.** A single agent from the New Agent form runs the command you typed; `dot-agent-deck dispatch --command` runs the command it is given; a schedule's own `command` comes from your schedules file. None of these comes from a project file, so none is reviewed.
- **The desktop app's own agent profiles.** Commands you set in the app's experimental agent profiles are your own settings and are not reviewed; the project prompts an orchestration started from the deck screen hands its agents are.
- **A saved orchestrator prompt.** When the TUI restores an orchestration tab, the orchestrator's saved prompt is used again as long as the orchestration is approved as its file reads now.

## When it does not work

| What you see | Why | What to do |
|---|---|---|
| The review comes back every time you open one orchestration from a worktree | The worktree's file says something other than what you approved in the main checkout. | Review the difference, then approve it, or bring the file in line with the main checkout. |
| An issue dispatch is refused because `the existing issue branch agent/issue-<n>` differs from the approved one | An earlier dispatch left that branch, and its `.dot-agent-deck.toml` changed. | Bring that branch's file back in line with the default branch, or delete the branch with the `git` command in the message; the next fire dispatches the issue again. |
| A cancelled restore is offered again every time the TUI starts | An orchestration you did not approve stays in your saved workspace. | Approve it, or run `dot-agent-deck snapshot clear` to drop the saved workspace. |
| A schedule's status is `refused`, or `schedule list` says `last fire refused` | The orchestration was never approved, or its file changed. | Approve from the review the deck opened in a client, or run the command in the message from a terminal outside the deck, then `schedule run-now`. |
| An orchestrator says `delegate` or `pane restart` was refused as not approved | The orchestration's file changed after you started it. | Review the change in the review the deck opened and approve it, or approve with the command in the message, or revert the file. |
| The deck refused work but no review appeared | No TUI or desktop app is attached to that deck, the desktop app is not watching that deck, you declined this text since the daemon started, or the client is from an older release. | Attach a client (the review appears then), select **All decks** or that deck in the desktop app, or approve with the command in the refusal. |
| A refusal says `the user declined it in the deck` | You denied the review for this text. | Approve it from the New Agent form or with `trust approve` if you meant to allow it; restarting the daemon also lets the deck ask again. |
| The TUI's status line says an orchestration's `instructions were not sent to its coordinator again` | The orchestration's file changed after you opened it, and the change is not approved. | Approve with the command in the message, or revert the file. If it says the roles changed, open the orchestration again. |
| A saved orchestration tab did not come back, and the TUI printed `was not restored` when you quit | Its project's file no longer defines it as saved. | Open it again from the New Agent form. |
| `trust approve` says `run it from a terminal outside the deck` | You ran it in a deck pane. | Run it in a terminal of your own, or approve from the review in a client. |
| `trust approve` says `there is no terminal to ask on` | It was run without a terminal (from a script). | Read the printed text, then run it again with `--yes`. |
| The TUI says `The deck daemon is older than this TUI and cannot show the review prompt` | The daemon is from an older release. | Approve with the command in the message, or upgrade and restart the daemon ([Troubleshooting → Recycling the daemon](troubleshooting.md#recycling-the-daemon)). |
| The desktop app says the deck's daemon is older than this app | The same, for the app. | Upgrade that daemon, or approve on its host and start from the TUI there. |

See also [Orchestration](orchestration.md), [Configuration → `.dot-agent-deck.toml` reference](configuration.md#dot-agent-decktoml-reference) and [Troubleshooting](troubleshooting.md#an-orchestration-is-not-approved).
