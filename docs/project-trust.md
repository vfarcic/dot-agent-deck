# Approving Project Commands

A project's `.dot-agent-deck.toml` decides which commands the deck runs for an orchestration and which instructions it gives the agents in it. A repository you clone, or a change someone pushes to one you already use, can change that file, so the deck shows you what an orchestration would run before it runs it for the first time, and again whenever that changes. You approve it once, and the deck remembers the approval until the text changes.

This page covers both clients and the command line: what the review shows, how to answer it, what is remembered, what happens to work that runs while you are away, and what the review does not cover.

## When the deck asks

The deck asks before it starts an orchestration that you have never approved, or whose text changed since you approved it:

- **TUI:** when you pick an orchestration in the New Agent form (`Ctrl+n`) and press `Enter`, and when the TUI recreates an orchestration tab from your saved workspace (see [Restoring a saved workspace](#restoring-a-saved-workspace)).
- **Desktop app:** when you press **Activate orchestration** in the [New agent](desktop/new-agent.md) dialog, or start an orchestration from the experimental deck screen.

An orchestration you approved, whose text has not changed, starts with no question, as it did before. Nothing from the orchestration starts while the review is open.

## What the review shows

The review is titled **Review project commands** and shows:

- **Project:** the project's directory. For a linked git worktree this is the matching directory of the repository's main checkout, and a **Config in:** line names the worktree directory whose `.dot-agent-deck.toml` is being read.
- **Orchestration:** its name.
- **State:** `not approved before`, or `changed since you approved it`.
- For each role: its name, then `command`, `agent` (when the role sets one), `description` and `prompt` (the role's `prompt_template`), in full. A role whose text changed since your approval is marked `changed`, and one your approval did not have is marked `new`.
- **Removed since you approved it:** the roles your approval had that the file no longer defines.

Read the commands and prompts as what they are: commands that will run as you, and instructions your agents will follow. Characters that could hide or disguise text, such as terminal control sequences, are shown as escapes (for example `\u{1b}`) rather than obeyed, in both clients and on the command line.

## Answer it

**TUI:**

| Key | What it does |
|---|---|
| `a` | Approve exactly the text shown, then start the orchestration (in a restore the footer reads `a approve and restore`). |
| `Esc` | Cancel. Nothing starts and nothing is remembered. |
| `↑` / `↓`, `PgUp` / `PgDn` | Scroll a review taller than the window. `(more above)` and `(more below)` say there is more. |

`Enter` does not approve, and a key you pressed before the prompt appeared does not answer it.

**Desktop app:** the **Review project commands** dialog shows the same text. Press **Approve and start** to approve it and start the orchestration, or **Cancel** (or `Esc`) to start and remember nothing. `Enter` does not approve here either. With [voice control](desktop/voice.md#approving-project-commands) on, say "approve and start" (any request that says *approve*, such as "approve the commands"); "cancel" or "close" cancels.

If the file changes while the review is open, approving does not approve the new text: the review shows the new text instead, and you answer again.

## What is remembered

An approval covers one orchestration of one project, and the exact text you were shown. These fields of every role are part of it: `name`, `command`, `agent`, `start`, `clear`, `description` and `prompt_template`, as they read after any [`extends`](orchestration.md#sharing-a-workflow-with-extends). Changing any of them, by even one character, adding a role, removing one or reordering the roles brings the review back for that orchestration. Editing comments, keys outside the roles, or a different orchestration in the same file does not.

The project is the directory holding `.dot-agent-deck.toml`. In a git repository, a linked worktree counts as the main checkout: the same directory in any worktree of the repository is one project. A linked worktree, such as the one each [dispatched unit](dispatcher-mode.md) works in, therefore uses the main checkout's approval: a unit started from an approved orchestration starts without asking, as long as the worktree's file says the same thing.

**Give your orchestrations a `name`.** An orchestration without one takes the directory's name, which is different in every worktree, so each worktree asks for its own approval and none of them shares the main checkout's.

Approvals are kept per user, on the machine the daemon runs on, in `project-trust/approvals.json` in the deck's state directory (`~/.local/state/dot-agent-deck/` unless `XDG_STATE_HOME` or `DOT_AGENT_DECK_STATE_DIR` moves it; see [Configuration → Environment variables](configuration.md#paths-and-endpoints)). For a [remote](remote-environments.md) daemon, that is the remote host.

## Work that runs while you are away

Some work starts an orchestration, or a role of one, without you in front of a client. That work never shows the review; if the orchestration is not approved as it now reads, it is refused and nothing from it starts:

- a schedule firing, and **Run now**, for a schedule that opens an orchestration ([Schedules](scheduled-tasks.md));
- a scheduled issue dispatch, before the issue's worktree runs any setup;
- `dot-agent-deck dispatch` when it starts an orchestration, before the unit's worktree runs any setup ([Dispatcher Mode](dispatcher-mode.md));
- an orchestrator's `delegate`, `pane restart` and `pane spawn` ([Orchestration](orchestration.md#commands)). An orchestration you started from a current client was approved then, so for it these are refused when its file changed since.

Each refusal says which orchestration and project, whether it was never approved or what changed, and the command that approves it:

```text
orchestration 'team' of project /home/you/code/demo was never approved, so nothing from it was started. Review and approve it from a terminal outside the deck with: dot-agent-deck trust approve /home/you/code/demo --orchestration team
```

Where you see it:

- **An orchestrator's command** (`delegate`, `pane restart`, `pane spawn`, `dispatch`): the command fails with that message, so the orchestrator agent reads it and can tell you.
- **A schedule:** `dot-agent-deck schedule list` prints `last fire refused at <time>: <message>` beneath the schedule, and the TUI's Schedules manager shows its status as `refused` and the message beneath the list while the schedule is selected ([The TUI's Schedules manager](scheduled-tasks.md#the-tuis-schedules-manager)).
- **The daemon log**, at warning level ([Troubleshooting → Logs](troubleshooting.md#logs-and-diagnostics)).

Run the command it names from a terminal outside the deck, check the text it prints, and approve. The next fire, or the orchestrator's next try, then runs.

## Manage approvals from the command line

Run these from a terminal **outside** the deck, on the machine the daemon runs on.

| Command | What it does |
|---|---|
| `dot-agent-deck trust list` | One line per approval: project, orchestration, when it was approved, and `current` (the file still says what you approved), `changed` (it says something else) or `missing` (the file or the orchestration is gone). Prints `No approved orchestrations.` when there are none. |
| `dot-agent-deck trust approve [<dir>] [--orchestration <name>] [--yes]` | Prints every role's text, as the review does (with `[changed]` and `[new]` marks), and asks `Approve these commands and prompts? [y/N]`. Only `y` or `yes` approves. `<dir>` is the directory holding `.dot-agent-deck.toml` (default: the current directory); without `--orchestration` it covers every orchestration in that file. |
| `dot-agent-deck trust revoke [<dir>] [--orchestration <name>]` | Removes the approvals of that project, or of one orchestration. The next start asks again, and unattended work for it is refused. Exits non-zero when there was nothing to revoke. |

`--yes` approves without asking, for scripts, and still prints what it approved. Without a terminal to ask on and without `--yes`, `approve` approves nothing and exits non-zero. If the file changes between printing and your answer, nothing is approved and you are told to run it again.

`trust approve` refuses to run inside a pane the deck started, `--yes` included, and says `run it from a terminal outside the deck`, so that an agent working in a deck pane does not approve its own commands with it ([the limits](#what-the-review-does-not-cover) say what this does not stop). Approve from the review in the TUI or the desktop app, or from a terminal of your own. `list` and `revoke` work anywhere.

## Restoring a saved workspace

When the TUI starts and recreates orchestration tabs from your saved workspace because their agents are gone ([Session Management → Resuming Sessions](session-management.md#resuming-sessions)), it shows the review for each saved orchestration that is not approved as its file now reads, one after the other, with the footer `a approve and restore · Esc cancel · ↑↓ scroll`. The rest of the workspace is restored first. Approving restores that tab; cancelling leaves it out. An orchestration that is approved and unchanged comes back without asking.

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
- **What the approved commands run next.** The review shows a role's `command`, such as `devbox run agent-coder`. What that runs is decided by other files in the project, such as `devbox.json`, scripts and build files, and the deck does not review those. A dispatched unit's worktree setup (its `devbox` environment) is prepared only after the orchestration's approval is checked, but `devbox.json` itself is not reviewed.
- **Commands you start yourself.** A single agent from the New Agent form runs the command you typed; `dot-agent-deck dispatch --command` runs the command it is given; a schedule's own `command` comes from your schedules file. None of these comes from a project file, so none is reviewed.
- **The desktop app's own agent profiles.** Commands you set in the app's experimental agent profiles are your own settings and are not reviewed; the project prompts an orchestration started from the deck screen hands its agents are.
- **A saved orchestrator prompt.** When the TUI restores an orchestration tab, the orchestrator's saved prompt is used again as long as the orchestration is approved as its file reads now.

## When it does not work

| What you see | Why | What to do |
|---|---|---|
| The review comes back every time you open one orchestration from a worktree | The orchestration has no `name`, so each worktree is its own project. | Add `name = "<name>"` to the `[[orchestrations]]` table, then approve it once. |
| A schedule's status is `refused`, or `schedule list` says `last fire refused` | The orchestration was never approved, or its file changed. | Run the command in the message from a terminal outside the deck, then `schedule run-now`. |
| An orchestrator says `delegate` or `pane restart` was refused as not approved | The orchestration's file changed after you started it. | Review the change, then approve with the command in the message, or revert the file. |
| `trust approve` says `run it from a terminal outside the deck` | You ran it in a deck pane. | Run it in a terminal of your own, or approve from the review in a client. |
| `trust approve` says `there is no terminal to ask on` | It was run without a terminal (from a script). | Read the printed text, then run it again with `--yes`. |
| The TUI says `The deck daemon is older than this TUI and cannot show the review prompt` | The daemon is from an older release. | Approve with the command in the message, or upgrade and restart the daemon ([Troubleshooting → Recycling the daemon](troubleshooting.md#recycling-the-daemon)). |
| The desktop app says the deck's daemon is older than this app | The same, for the app. | Upgrade that daemon, or approve on its host and start from the TUI there. |

See also [Orchestration](orchestration.md), [Configuration → `.dot-agent-deck.toml` reference](configuration.md#dot-agent-decktoml-reference) and [Troubleshooting](troubleshooting.md#an-orchestration-is-not-approved).
