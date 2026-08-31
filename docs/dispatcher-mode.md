---
sidebar_position: 5.6
title: Dispatcher Mode
---

# Dispatcher Mode

## What it is

Dispatcher mode lets you **start work in the background, just by asking for it**.

You open a pane, tell it what you want started — "work on the search bug" — and it sets up a separate, isolated copy of your repository and puts an agent (or a whole team of agents) to work there. You stay where you are. Nothing it does touches the files you have open.

Start as many as you like. Each one gets its own copy of the repo, so three units working on three things never collide with each other or with you.

The pane you are talking to is an **ordinary conversational agent**. It answers questions and does work like any other pane; starting a background unit is simply one more thing you can ask it for. You do not have to phrase anything specially, and you do not lose it as a chat partner once you have used it.

Without this, starting parallel work means doing it yourself: create a Git worktree, open a pane in it, launch an agent, paste the task in, and repeat for each line of work. Dispatcher mode is that chore, asked for in a sentence.

## When to reach for it

- You are mid-conversation and want something else started **without derailing what you are doing**.
- You want several things worked on **at the same time** — three PRDs, three PRs to verify, three bugs.
- You want work done on a copy of the repo, so a half-finished change **cannot disturb your working tree**.

If you just want an agent to do something for you right now, in front of you, you do not need this — open an ordinary pane.

## Starting a dispatcher pane

1. Press `Ctrl+n`
2. Navigate to the project directory and confirm it
3. Cycle the **Mode** field to `dispatcher`
4. Press `Enter`

Then talk to it: *"Start work on the login timeout bug."*

## One agent, or a team?

Each unit can start as a **single agent** or as a **full multi-role orchestration** — a team of agents with an orchestrator delegating to workers, as configured in your project's `.dot-agent-deck.toml`.

Which one is your call, not the agent's, so it asks you before the first unit rather than guessing. The same request can want either shape:

- *"work on these three features"* → usually a team per feature
- *"verify these three PRs"* → usually one agent each

If you name an orchestration your project does not define, that is an error telling you what *is* available — not a silent fall back to something you did not choose. Nothing is created when that happens.

## Watching the work

Each dispatched unit appears on your deck like any other work: a card for a single agent, a tab for a team. Open it to watch, type into it, or take over.

The unit works in `../<your-repo>-dispatch-<name>` — a sibling directory of your project, never inside it.

## Pointing a unit at the right thing

A dispatched unit gets a **copy of your repository**, so it already has your code, your docs, and any instructions you keep in the repo. Ask for work by referring to what is in there — *"execute the release checklist in docs/release.md"* — rather than pasting the contents of those files into the request. Pasted text can go stale against the copy the unit is actually holding.

Refer to files by their path **relative to the repo root**. An absolute path pointing back into your own working directory defeats the isolation and puts two agents on the same files.

One thing worth knowing: a unit's copy is made from your **last commit**. Uncommitted edits, untracked files, and ignored files are not in it. If a unit needs a change you have not committed yet, commit it first — otherwise the unit quietly works from the older version.

## Finishing up

Closing a unit's tab removes that unit's copy of the repo. Your own repository is never touched. Closing the dispatcher pane itself removes nothing — it never owned a copy.

If a unit still has **uncommitted changes**, closing it leaves its directory on disk instead of deleting it, so the work is recoverable. A leftover directory costs disk space; a deleted one costs work.

The close confirmation tells you when that is about to happen, and where: before you answer it, the dialog names the directory the work would be kept in. That warning is a forecast — the unit is still running while you read it, so it can commit its work between the dialog and the close — so the deck checks again once the unit has actually stopped, and the status line afterwards reports what really happened. A unit whose copy turned out to be clean is simply removed and nothing is said, which is why the message appearing is worth reading. If you dismiss the status line and want the path back, `dot-agent-deck worktree list` reports every worktree the deck knows about.

The unit's branch (`agent/dispatch-<name>`) always survives, since it may hold committed work. Dispatching the *same name* again is therefore refused, telling you the branch is there — delete it with `git branch -D agent/dispatch-<name>` when you are done, or use a different name.

## Current limitations

**You are not notified when a unit finishes.** The dispatcher tells you where each unit is running, but nothing reports back to it when the work is done — you check on the units yourself. Sending results back is planned.

## See also

- [Orchestration](orchestration.md) — configuring the multi-role teams a unit can start as
- [Workspace Modes](workspace-modes.md) — the other built-in and project-defined modes on the `Ctrl+n` cycler
