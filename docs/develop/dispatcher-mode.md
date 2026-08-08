# Dispatcher Mode

> **Developer / maintainer reference.** This page documents an internal development mechanism and is intentionally excluded from the published documentation site.

## What it is

Dispatcher mode is a built-in seeded mode for `dot-agent-deck` that teaches an agent one extra effector: the `dispatch` CLI subcommand, which starts an isolated line of work in its own git worktree. A dispatcher pane is otherwise an **ordinary conversational agent** — it does whatever the user asks — and it reaches for `dispatch` when the user says to *start* something as a separate line of work.

The seed is deliberately scoped to **Agent Deck mechanics, not work methodology**: what the verb is, what it does, and the constraints that follow from process isolation. It holds no opinion on how the user should split up their work, matching the two schedule-authoring seeds. (An earlier version cast the pane as a planner that had to decompose a goal into 2–6 independent units and never do work itself; that was cut — see the Design record in [PRD #220](../../prds/220-dispatcher-mode-worktree-dispatch.md).)

## How to activate it

1. Open a new pane with `Ctrl+n`
2. Cycle through the available modes until `dispatcher` appears in the mode selector
3. Select it and confirm

The dispatcher mode is currently gated behind the `experimental` feature flag — see [experimental-flag.md](experimental-flag.md).

## How the agent uses `dispatch`

Once the agent is running inside dispatcher mode, it can execute:

```
dot-agent-deck dispatch <name> --task "..."
```

This creates a dedicated worktree and starts a line of work inside it. The `--task` text becomes the opening prompt.

Several dispatches from one pane are normal, and are not decomposition: working on three PRDs in parallel is three dispatches of three things the user named.

### Reference the repo, don't paste it

"Self-contained" means independent of the **dispatcher's conversation**, not of the repo. The unit works in a copy of the same repo, so it already has the code, the docs, the PRDs and the skills — 59 skill files, `CLAUDE.md` and every PRD are tracked, so `--task "Execute the /prd-full skill for PRD 220"` is complete as it stands. Pasting a skill's contents into `--task` is waste, and it can go stale against the copy the unit actually holds.

Paths must be **relative to the repo root**. An absolute path into the dispatcher's own checkout points the unit back at the directory the dispatcher is in, which destroys the isolation the worktree exists to provide and puts two agents on the same files — the cross-delivery hazard PRD #140 documents.

One sharp edge the seed deliberately does *not* carry, to keep it short — worth knowing as a maintainer. `git worktree add` checks out the **last commit**, so a dispatched worktree contains committed content only. Measured: an uncommitted edit, an untracked file and a gitignored file are all absent. In this repo that matters for `.claude/settings.local.json`, which is untracked (`verify-pr`'s own `setup.sh` copies it by hand for exactly this reason). If a unit needs something uncommitted, commit it first — otherwise the unit silently reads an older version of the file it was pointed at. This is the same working-tree-vs-HEAD divergence that made an earlier `--list-targets` offer targets the spawn could not start.

## Choosing the shape: one agent, or a team

A unit can start as a single agent or as a full multi-role orchestration. **Which one is the user's call, not the agent's** — and that is why it is asked rather than inferred. The two cases look identical from the request:

- *"work on these three features"* → usually a team per feature
- *"verify these three PRs"* → usually one agent each

So the seed tells the dispatcher to enumerate the shapes this repo offers and ask before the first dispatch:

```
dot-agent-deck dispatch --list-targets
```

which prints `single` plus every role-bearing orchestration by name. The answer then rides on each call:

```
dot-agent-deck dispatch <name> --task "..." --single
dot-agent-deck dispatch <name> --task "..." --orchestration <orchestration-name>
```

`--list-targets` is answered by the **daemon**, not computed in the CLI, and that is deliberate: the daemon resolves the pane's own cwd and reads the same config the dispatch will resolve its shape from. An earlier cut read the CLI process's `current_dir()` locally and let the spawn resolve names against the *worktree* dir instead — and because `load_project_config` normalises an unnamed orchestration to its **directory basename**, the same entry was `myrepo` in the listing and `myrepo-dispatch-<slug>` at spawn time. The listing offered a name the spawn could never match. One basis for both sides is the only way that stays true.

Four outcomes, kept distinct because collapsing them makes the agent state something false:

| Situation | What you get |
|---|---|
| Config with orchestrations | Each role-bearing one, by the name the spawn will use |
| No config file | `single` only — the truth |
| Config present but unparseable | The parse error, named, and a non-zero exit |
| Pane's directory unknown | Said plainly, and explicitly *not* "this repo has none" |

Two more details. Naming an orchestration the repo does not define is an **error** listing what is available, not a silent fallback — starting something other than what the user picked is exactly the surprise the selector removes — and it is rejected *before* the worktree is created, so a typo leaves no directory or branch behind. And schedule/authoring modes never appear: a schedule creates a *future* task, so it is not something a dispatch can start.

With neither flag, the shape still falls back to whatever the repo's config implies (its first role-bearing `[[orchestrations]]`, else a single agent), which is the pre-selector behaviour.

### What the unit actually gets

Whichever shape you pick, the unit is started the way the interactive `Ctrl+n` path starts it, plus your prompt:

- **`--single`** runs a real agent — the deck's configured `default_command`, or the Claude default when that is unset. (It must never be `None`: the spawn path reads an absent command as `$SHELL`, which started a bare shell and typed the task into a bash prompt.)
- **`--orchestration`** starts every role, and the orchestrator receives `.dot-agent-deck/orchestrator-context.md` carrying its own `prompt_template`, the available-agents list, the delegation protocol, and your `--task` under `## Your task`. The task rides *inside* the file rather than being appended to the pointer line because a multi-line prompt does not submit reliably through a PTY.

## Worktree isolation

Every `dispatch` call creates its work in a dedicated Git worktree at `../<repo>-dispatch-<slug>`. Each unit is fully isolated from the others — changes to one dispatched worktree never conflict with another or with the main worktree.

## Cleanup

Cleanup is keyed to the **dispatched unit's own tab**, not the dispatcher's. Closing a unit's tab removes that unit's worktree (the repo itself is always preserved). Closing the dispatcher tab removes nothing — it never owned a worktree.

Removal deliberately **refuses to discard uncommitted work**: if the unit's worktree still has uncommitted changes, it is left on disk and a warning is logged, so you can recover the work. A leaked worktree costs disk; a force-removed one costs work.

The unit's branch (`agent/dispatch-<slug>`) always survives removal, because it may hold the unit's committed work. That means dispatching the **same name again** is refused, naming the leftover branch and telling you how to proceed — delete the branch with `git branch -D agent/dispatch-<slug>` once you are done with it, or dispatch under a different name.

## Current limitations

- ~~A dispatched orchestration starts without the delegation protocol.~~ **Fixed.** A dispatched orchestration now receives the same orchestrator context the interactive `Ctrl+n` path writes — its own `prompt_template`, the available-agents list, and the delegation protocol — with the `--task` text folded in under `## Your task`. The composition is now *shared* (`src/orchestrator_context.rs`) rather than duplicated, so scheduled issue-dispatch (#120) can get the same treatment cheaply — but it deliberately has NOT here. Enabling it there changes what lands in a shipped feature's pane (a pointer line instead of the prompt text), which is [#222](https://github.com/vfarcic/dot-agent-deck/issues/222)'s job to do deliberately with its own tests updated. Until then #120 orchestrations keep their existing defect.
- The return edge (the dispatched unit sending results back to the dispatcher) is not yet implemented. The dispatcher reports where each unit is running; it is **not** notified when a unit finishes. This is Phase 2 of [PRD #220](https://github.com/vfarcic/dot-agent-deck/issues/220) itself, deferred rather than dropped. (It is *not* tracked by #174 — that is the separate *Cross-project orchestration dispatch* PRD, which **depends on** this one.)
