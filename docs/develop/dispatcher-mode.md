# Dispatcher Mode — design record

> **Developer / maintainer reference.** This page documents internal rationale and is intentionally excluded from the published documentation site. The user-facing page is [`docs/dispatcher-mode.md`](../dispatcher-mode.md); everything here is the *why*, the sharp edges, and the decisions that are invisible from the outside.

## The seed teaches mechanics, not methodology

Dispatcher mode is a built-in seeded mode whose seed teaches an agent one extra effector: the `dispatch` CLI subcommand. The seed is deliberately scoped to **Agent Deck mechanics, not work methodology** — what the verb is, what it does, and the constraints that follow from process isolation. It holds no opinion on how the user should split up their work, matching the two schedule-authoring seeds.

An earlier version cast the pane as a *planner* that had to decompose a goal into 2–6 independent units and never do work itself. That was cut: it made the pane refuse ordinary requests, and the "don't do the work" clause forbade it from doing anything else the user asked. See the Design record in [PRD #220](https://github.com/vfarcic/dot-agent-deck/blob/main/prds/220-dispatcher-mode-worktree-dispatch.md). Pinned by `dispatcher_seed_teaches_mechanics_not_work_methodology`.

Several dispatches from one pane are normal and are **not** decomposition: working on three PRDs in parallel is three dispatches of three things the user named.

## `--list-targets` is answered by the daemon

```
dot-agent-deck dispatch --list-targets
```

prints `single` plus every role-bearing orchestration by name. It is answered by the **daemon**, not computed in the CLI, and that is deliberate: the daemon resolves the pane's own cwd and reads the same config the dispatch will resolve its shape from.

An earlier cut read the CLI process's `current_dir()` locally and let the spawn resolve names against the *worktree* dir instead — and because `load_project_config` normalises an unnamed orchestration to its **directory basename**, the same entry was `myrepo` in the listing and `myrepo-dispatch-<slug>` at spawn time. The listing offered a name the spawn could never match. One basis for both sides is the only way that stays true.

Four outcomes, kept distinct because collapsing them makes the agent state something false:

| Situation | What you get |
|---|---|
| Config with orchestrations | Each role-bearing one, by the name the spawn will use, with `[default]` on whichever an unnamed dispatch would open |
| No config file | `single` only — the truth |
| Config present but unparseable | The parse error, named, and a non-zero exit |
| Pane's directory unknown | Said plainly, and explicitly *not* "this repo has none" |

Naming an orchestration the repo does not define is an **error** listing what is available, not a silent fallback — and it is rejected *before* the worktree is created, so a typo leaves no directory or branch behind. Schedule/authoring modes never appear: a schedule creates a *future* task, so it is not something a dispatch can start.

With neither flag, the shape falls back to whatever the repo's config implies (its DEFAULT `[[orchestrations]]` — the block carrying `default = true`, else the first one with roles — and a single agent when it defines none) — the pre-selector behaviour, kept so an older CLI keeps working against a newer daemon. When that choice is implicit, the dispatch's reply carries a note naming what was opened and what else was defined; see [Orchestration](../orchestration.md#which-orchestration-a-scheduled-task-opens).

## What the unit actually gets

- **`--single`** runs a real agent — the deck's configured `default_command`, or the Claude default when unset. It must never be `None`: the spawn path reads an absent command as `$SHELL`, which started a bare shell and typed the task into a bash prompt. A worktree appeared, a pane appeared, and the test was green — see `resolve_single_agent_command`.
- **`--orchestration`** starts every role, and the orchestrator receives `.dot-agent-deck/orchestrator-context.md` carrying its own `prompt_template`, the available-agents list, the delegation protocol, and the `--task` under `## Your task`. The task rides *inside* the file rather than being appended to the pointer line because a multi-line prompt does not submit reliably through a PTY.

Each role pane is labelled with its **role name** (not the task name, and not the agent's session id) so a six-role team does not come up as six indistinguishable cards. Both the daemon record (`spawn_one`) and the live card (a per-role synthetic `SessionStart`) carry it, so the label survives a reconnect. See `orchestration/dispatch/002`.

## The uncommitted-content edge

`git worktree add` checks out the **last commit**, so a dispatched worktree contains committed content only. Measured: an uncommitted edit, an untracked file, and a gitignored file are all absent. In this repo that matters for `.claude/settings.local.json`, which is untracked (`verify-pr`'s own `setup.sh` copies it by hand for exactly this reason).

This is the same working-tree-vs-HEAD divergence that made an earlier `--list-targets` offer targets the spawn could not start. The user-facing page states the consequence ("commit it first"); the seed deliberately does not carry it, to stay short.

## Close path

Cleanup is keyed to the dispatched unit's own tab. Three defects lived here, each found only by a reproduction (`dispatch/close/001`):

1. A **daemon-spawned card has no local pane** in the TUI until it is focused, so `close_pane` answered `Pane <id> not found`, PRD #92 F4 preserved the card, and the agent kept running. Focusing the card attached it, which is why a second `Ctrl+W` appeared to work. Fixed by resolving the agent through `list-agents` and issuing the ordinary `stop-agent`.
2. The daemon **awaited worktree cleanup before answering** the close. On a worktree an agent has worked in, `git status --porcelain` is seconds, which blew the TUI's 5s `CTRL_W_STOP_TIMEOUT`. Cleanup now runs detached, after the response.
3. A pane can carry **more than one session** — a placeholder plus the agent's own — and the close removed only the one its card was built from, leaving a ghost card badged `No agent`. Only reproduces when the command is **not inferable** as an agent (a `devbox run agent-<role>` launcher), because such a command is not wrapped and the agent's hooks arrive under an identity the reuse guard does not match. Fixed by `AppState::remove_sessions_for_pane`.

`RemovalPolicy::KeepIfDirty` is why a dirty worktree survives: this sibling's name was chosen by an LLM, so closing must not destroy uncommitted work. Issue-dispatch uses `Force` instead, because its slot-reclaim model depends on the name actually being freed.

## Deferred, and why

- **Scheduled issue-dispatch (#120) does not get the orchestrator context.** The composition is *shared* (`src/orchestrator_context.rs`) rather than duplicated, so enabling it there is cheap — but doing it here would change what lands in a shipped feature's pane (a pointer line instead of the prompt text). That is [#222](https://github.com/vfarcic/dot-agent-deck/issues/222)'s job, with its own tests updated. Until then #120 orchestrations keep their existing defect.
- **The return edge** (a dispatched unit reporting completion back to the dispatcher) is Phase 2 of PRD #220 itself, deferred rather than dropped. It is *not* tracked by #174 — that is the separate *Cross-project orchestration dispatch* PRD, which **depends on** this one. The dependency has been stated backwards more than once.

## Graduation

Shipped behind `features::show_dispatcher()` and graduated out of it before release: the wrapper is deleted and the branch inlined to `true`. See [experimental-flag.md](experimental-flag.md).
