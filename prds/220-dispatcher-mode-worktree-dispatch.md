# PRD #220: Dispatcher mode + worktree dispatch — one-step, agent-callable isolated line of work

**Status**: Planning
**Priority**: Medium
**Created**: 2026-07-19
**GitHub Issue**: [#220](https://github.com/vfarcic/dot-agent-deck/issues/220)
**Related**: PRD #140 (concurrent orchestration safety — establishes worktree-per-orchestration as the supported model and documents it; this PRD makes that model a one-step action but does not depend on #140's routing code); PRD #120 (issue-dispatch — already creates one git worktree per dispatched orchestration and spawns a full orchestration inside it; this PRD lifts that engine out from behind the scheduler and gives it an agent-callable trigger); PRD #174 (cross-project orchestration dispatch — the cross-project superset of this feature; #174 should depend on the `dispatch` verb and return-edge addressing built here); PRD #127 (mode `seed_prompt` + the schedule-authoring mode — the seeded-single-agent precedent the dispatcher mode reuses); PRD #93 (always-external daemon — the single spawn authority and hydration path a dispatched unit rides).

## Problem Statement

Concurrent orchestrations are only safe *across directories*: distinct working directories yield distinct routing identities, distinct on-disk coordination files (`.dot-agent-deck/*-{role}.md`), and distinct working trees. PRD #140 makes this the supported model — "one orchestration per working directory, a worktree per parallel line of work" — and documents it. But #140 stops at making the model *safe and honest*; it does not make it *easy*.

Today, getting an orchestration into a worktree interactively is a multi-step manual chore. The maintainer's real workflow is: spin up a single agent in the dashboard, instruct it to create one or more worktrees, then **manually** open an orchestration in each (the new-pane form's directory field pointed at the worktree). The `/worktree-prd` skill automates only the first half — it creates a worktree in a sibling directory and tells the user to `cd` there manually; it is not wired to the orchestration spawn. So every parallel line of work costs: create worktree → return to the TUI → open a new orchestration → set its directory by hand. The #140 reporter hit the concurrency bug precisely because this friction pushes users toward same-directory concurrency instead.

There is no agent-callable, one-step entrypoint that both creates a worktree and starts an isolated orchestration in it:

- The only agent-callable orchestration verbs are `Delegate` and `WorkDone` (`src/main.rs:88-118`). `handle_delegate` (`src/state.rs:835`) targets **pre-existing** worker panes filtered by role (`src/state.rs:864-874`) and respawns them at the target pane's **frozen** cwd (`src/state.rs:910`); it never creates a pane, let alone a worktree. So an agent cannot start a new isolated line of work — it can only feed panes that already exist in its own orchestration.
- Issue-dispatch (PRD #120) already does exactly the create-worktree-then-spawn dance — `create_worktree` (`src/issue_dispatch_run.rs:607-636`) → `SpawnRequest { working_dir: worktree }` (`src/issue_dispatch_run.rs:348-353`) → `spawn` (`src/spawn.rs:228`), which spawns every role of the target orchestration with `cwd = working_dir` (`src/spawn.rs:324-349`) — but it is **scheduler-triggered only**. The engine exists; the agent-facing trigger does not.

Separately, even if the verb existed, an agent needs to know *what to call and when*. The reported user need is not "detect intent" — in every path the user described ("I want to work on X, start orchestration"; discuss first then start; start several at once) the human **explicitly** says to start. The gap is teaching the agent, agent-agnostically and without polluting unrelated sessions, that the isolated-dispatch verb is the right effector for that stated intent.

## Solution Overview

Two pieces, one principle.

**The effector — a `dispatch` verb.** Add an agent-callable CLI verb that, in one call, (a) creates a git worktree and (b) spawns a fresh, fully-isolated orchestration (or single agent) inside it via the existing daemon `SpawnRequest` + hydration path. It reuses PRD #120's `create_worktree` and `spawn` engine wholesale — the only new surface is the trigger, the branch/worktree naming for a user-driven (non-issue-numbered) unit, the return-edge routing back to the caller, and the cleanup lifecycle.

**The trigger — a dispatcher mode.** Add a built-in "dispatcher" mode: a single seeded agent (`ModeConfig` with a `seed_prompt`, `src/project_config.rs:30`) whose seed teaches it what command to run and when to run it. This reuses the existing seed-prompt delivery mechanism — a scoped prompt pointing the agent at a context file (`prepare_orchestrator_prompt`, `src/ui.rs:1862`; the `seed_prompt` primitive from PRD #127), with `build_schedule_authoring_mode` (`src/ui.rs:4271`) as the direct precedent: a seeded single agent that helps set up and fire off a unit of work rather than doing the work itself. The dispatcher mode is the same family — "issue-dispatch, but the trigger is a seeded conversational agent instead of the scheduler."

Skills are deliberately **not** used to carry this knowledge. This repo does not treat skills as a cross-agent runtime mechanism: the orchestrator protocol is delivered by a file+seed, not a skill, and Pi required a bundled extension (PRD #201) rather than reading a skill. The seed-prompt path works uniformly for Claude, OpenCode, and Pi (every agent can read a file) and — because the seed is delivered only when the dispatcher mode tab is opened — carries **zero overhead** for the sessions that never dispatch.

**The principle — isolation is deterministic, never a judgment call.** The agent declares intent ("this is a line of work; start it"); the verb *always* isolates work in a worktree. Whether-to-isolate is never an LLM decision. The payoff is that a mis-timed or over-eager dispatch produces a redundant, cleanable worktree — never the cross-delivery corruption of #140. Getting the *timing* wrong costs disk, not correctness. Worktree selection is therefore always a **pre-spawn** decision.

## Scope

### In Scope

**The `dispatch` verb (effector):**

- **A new agent-callable CLI subcommand** (working name `dispatch`) alongside `Delegate`/`WorkDone` (`src/main.rs:36`). Shape (to finalize in M1): a unit name/branch, an optional target orchestration selector, and the task text (with the same `--task` / `--task-file` shell-safety discipline as `Delegate`, `src/main.rs:88-118`).
- **Deterministic worktree creation**, reusing `create_worktree` (`src/issue_dispatch_run.rs:607-636`) and a user-driven analogue of `derive_issue_paths` (`src/issue_dispatch.rs:60-86`) for branch/worktree naming when there is no issue number to key on.
- **Spawn the isolated unit** by handing the worktree path to the existing `SpawnRequest { working_dir }` → `spawn` (`src/spawn.rs:228`) path, so every role inherits `cwd = worktree` exactly as issue-dispatch already does (`src/spawn.rs:324-349`). If the target directory defines an orchestration with roles, spawn the orchestration; otherwise spawn a single agent (mirrors #174's spawn table).
- **Return-edge routing** so the dispatched unit's completion reaches the *caller's* pane rather than being resolved by the `(name, cwd)` tuple (which will never match, since the caller lives in a different orchestration/cwd). Register a `dispatch-id → caller pane` callback at dispatch time and resolve it on the unit's terminal `work-done`, reusing the same `write_to_pane_and_submit` injection the local work-done loop uses. This is the one genuinely new wire and is the same mechanism PRD #174 needs.

**The dispatcher mode (trigger):**

- **A built-in dispatcher mode** — a `ModeConfig` with a `seed_prompt` (`src/project_config.rs:30`) — modeled on `build_schedule_authoring_mode` (`src/ui.rs:4271`). Its context file teaches: you help the user decompose and start lines of work; when the user has settled on a unit and says to start it, run the `dispatch` verb once per independent unit; isolation is automatic; do not do the work yourself.
- **Agent-agnostic, zero ambient overhead:** the seed is delivered only when the dispatcher mode tab is opened (the existing scoped seed-delivery path), so no unrelated session pays for it, and it works across Claude/OpenCode/Pi.

**Lifecycle:**

- **Per-unit branch/worktree naming** for user-driven units (no issue number), collision-checked like `/worktree-prd`'s `create.sh`.
- **Cleanup on tab close**, reusing the issue-dispatch bookkeeping (`remove_worktree`, `src/issue_dispatch_run.rs:133-145`; shared-worktree accounting `worktree_still_in_use` / `take_worktree`) so a closed dispatched orchestration's worktree is removed and does not accumulate.

**Tests (CLAUDE.md rule 4):**

- **L1**: the dispatcher mode renders and delivers its seed; the mode → orchestration-tab cross-type spawn produces the expected new tab(s).
- **L2 PTY-attached** (demo-reel-eligible): a real dispatcher agent invokes `dispatch`, a worktree is created, and an isolated orchestration comes up in it in a new tab — modeled on `scheduler/dispatch/013`.
- **Real-agent (pre-PR tier)**: a Haiku dispatcher genuinely runs `dispatch` end to end against a real clone/worktree, asserted via a uniquely-named sentinel file.

**Docs (prong of #140 handoff):**

- Document the dispatcher mode and `dispatch` verb as the recommended one-step way to run parallel lines of work; update #140's guard/warning copy to point at the dispatcher mode instead of the manual `/worktree-prd` flow.

### Out of Scope

- **Cross-project dispatch** (target resolution across sibling repos, the peer-map allowlist, info-vs-work read-only enforcement) — that is PRD #174, which builds on the verb defined here.
- **Same-directory concurrency isolation** (per-instance namespacing of coordination files) — deferred in #140; unchanged here.
- **Autonomous decomposition** — the dispatcher mode helps the *human*-directed decomposition; it does not attempt to decide on its own whether independent-looking work is truly independent (an unreliable LLM judgment). Human states the units; the verb isolates each.
- **The "after-case" (mid-flight worktree adoption)** — explicitly unsupported; see below.

### Explicitly unsupported: mid-flight worktree adoption (the "after-case")

A running orchestrator creating a worktree partway through and expecting its already-running workers to follow **cannot work** and will not be supported, because: worker pane cwds are frozen at spawn (`cmd.cwd`, `src/agent_pty.rs:736`); the orchestrator's cwd is neither movable nor reported to the daemon (an agent's internal `cd` does not relocate the PTY process, and the daemon is never told); and coordination files are pinned to the pane's recorded cwd (`work-done-{role}.md` via `pane_cwd_map`), so a worker that edits files in a new worktree while its handshake lands in the original directory splits brain, with in-flight uncommitted work stranded in the old tree. Worktree is therefore **always a pre-spawn decision** made by `dispatch`, never a runtime relocation. The dispatcher mode makes the correct pre-spawn path the easy one so users never reach for the broken after-case.

## Success Criteria

- A dispatcher-mode agent can, from a single stated user intent, run `dispatch` and have a fully-isolated orchestration come up in a fresh worktree in a new tab — no manual worktree creation, no manual directory selection.
- Every `dispatch`ed unit lands in its own worktree deterministically; two dispatches never share a tree or coordination files.
- The dispatched unit's completion is delivered back to the dispatcher's pane via the `dispatch-id` callback, surviving detach/reattach.
- Closing a dispatched orchestration's tab removes its worktree; worktrees do not accumulate across dispatches.
- The dispatcher mode's seed is delivered only when its tab is opened; unrelated agent sessions incur no added prompt.
- The verb works with a Claude, OpenCode, or Pi dispatcher (agent-agnostic seed + CLI).
- `cargo test-fast` green per task; `cargo test-e2e` green pre-PR, including a PTY-attached L2 test and a real-agent pre-PR test.
- Docs describe the dispatcher-mode + `dispatch` flow; #140's guard copy points at it.

## Milestones

### Phase 1: The `dispatch` verb over the existing engine

- [ ] **M1.0** — Define the `dispatch` CLI subcommand (`src/main.rs:36`): args (unit/branch name, optional orchestration selector, `--task`/`--task-file`), validation, and the hook-socket round-trip to the daemon.
- [ ] **M1.1** — Worktree creation for a user-driven unit: reuse `create_worktree` (`src/issue_dispatch_run.rs:607`) with a non-issue naming/collision scheme; spawn the isolated unit via `SpawnRequest { working_dir }` → `spawn` (`src/spawn.rs:228`). Single-agent vs orchestration chosen from the target dir's config (mirror #174's table).
- [ ] **M1.2** — Cleanup lifecycle: worktree removal on tab close via the issue-dispatch bookkeeping (`remove_worktree`, `src/issue_dispatch_run.rs:133`), including the shared-worktree accounting for multi-role units.

### Phase 2: Return-edge routing

- [ ] **M2.0** — Register a `dispatch-id → caller pane` callback at dispatch time; ride the id into the spawned unit.
- [ ] **M2.1** — On the dispatched unit's terminal `work-done`, resolve `dispatch-id → pane` and inject via `write_to_pane_and_submit`, bypassing the `(name, cwd)` tuple lookup; survives detach/reattach.

### Phase 3: The dispatcher mode

- [ ] **M3.0** — Add the built-in dispatcher mode (`ModeConfig` + `seed_prompt`, `src/project_config.rs:30`) modeled on `build_schedule_authoring_mode` (`src/ui.rs:4271`); author its context file (what/when to call `dispatch`, one-unit-per-worktree, isolation is automatic, don't do the work).
- [ ] **M3.1** — Verify the seed is delivered only on opening the dispatcher tab (scoped, zero ambient overhead) and reaches Claude/OpenCode/Pi panes uniformly.

### Phase 4: Cross-type spawn + tests

- [ ] **M4.0** — Validate and test the mode → orchestration-tab cross-type interaction: a dispatcher *mode* tab causing new *orchestration* tabs to hydrate from daemon records (L1).
- [ ] **M4.1** — L2 PTY-attached e2e: real dispatcher agent runs `dispatch`, worktree created, isolated orchestration up in a new tab (`.cast`-recording; model on `scheduler/dispatch/013`).
- [ ] **M4.2** — Real-agent pre-PR test: Haiku dispatcher runs `dispatch` end to end against a real clone/worktree, asserted via a uniquely-named sentinel file.

### Phase 5: Docs, cross-version, release

- [ ] **M5.0** — Docs: dispatcher-mode + `dispatch` as the recommended parallel-work flow; repoint #140's guard/warning copy at the dispatcher mode.
- [ ] **M5.1** — Cross-version contract check (CLAUDE.md rule 12): the return-edge callback and any spawn-request field additions are additively compatible; classify per `docs/develop/versioning.md` and add a `.breaking.md` fragment only if the contract shifts.
- [ ] **M5.2** — Changelog fragment (`dot-ai-changelog-fragment`); PR, Greptile review, merge, close #220.

## Key Files

- `src/main.rs` — CLI `Commands` enum (`:36`), `Delegate`/`WorkDone` shape to mirror (`:88-118`).
- `src/issue_dispatch_run.rs` — `create_worktree` (`:607`), worktree path → `working_dir` (`:348`), `remove_worktree` (`:133`), shared-worktree accounting (`:110-127`).
- `src/issue_dispatch.rs` — `derive_issue_paths` (`:60-86`) as the naming precedent.
- `src/spawn.rs` — `spawn` (`:228`), `decide_target` (`:251`), orchestration role loop spawning every role at `working_dir` (`:324-349`).
- `src/state.rs` — `handle_delegate` (`:835`) / `handle_work_done` (`:941`) as the injection/routing precedent; `pane_cwd_map` usage (`:910`) that the return-edge must sidestep.
- `src/project_config.rs` — `ModeConfig` + `seed_prompt` (`:30`).
- `src/ui.rs` — `prepare_orchestrator_prompt` / seed-file mechanism (`:1862`), `build_schedule_authoring_mode` precedent (`:4271`).
- `src/agent_pty.rs` — `cmd.cwd` frozen-at-spawn (`:736`), `TabMembership::Orchestration` (`:231`).

## Risks and Mitigations

- **Cross-type spawn regressions.** A *mode* tab causing *orchestration* tabs to appear is a new interaction. Mitigation: it rides the same daemon `SpawnRequest` + hydration path that issue-dispatch (a non-interactive trigger) already exercises; M4.0 tests it explicitly.
- **Spawn authority / runaway.** An agent that can spawn N orchestrations is a new privilege. Mitigation: deterministic isolation + cleanup bound the blast radius to wasted disk, not corruption; the dispatcher mode is opened deliberately (not ambient), and a confused dispatcher creates removable worktrees. Consider a soft per-session dispatch cap in M1 if warranted.
- **Worktree accumulation.** Dispatched worktrees could pile up. Mitigation: reuse #120's remove-on-close bookkeeping (M1.2); document manual pruning as the backstop.
- **Return-edge loss.** If the `dispatch-id` callback is not persisted across detach/reattach, a dispatcher waiting on a unit sleeps forever. Mitigation: store the callback daemon-side (like the local work-done routing) and test the reattach path (M2.1).
- **Teaching drift (agent ignores the seed).** An LLM may not always reach for `dispatch`. Mitigation: this is best-effort by design and safe by construction (isolation is not gated on the agent getting it right); the seed copy is explicit and the verb is the only path that isolates.

## Open Questions

- **Verb name and surface.** `dispatch` vs `start`/`spawn`; does it live only as an agent CLI, or also as an interactive TUI action? Decide in M1 against the existing new-pane UI.
- **Branch/worktree naming for user-driven units.** User-supplied name, derived-from-task slug, or both; where worktrees live (`.worktrees/<name>` like #120, or a sibling like `/worktree-prd`). Decide in M1.1.
- **Standalone vs #174 Phase 1.** This PRD is scoped as the same-project precursor; confirm with the maintainer whether it ships standalone (with #174 depending on it) or is folded in as #174's first milestone.
- **#140 handoff — prong 1 fate.** Once distinct-cwd worktree dispatch is the norm, #140's per-tab `orchestration_id` only protects the discouraged same-cwd-two-tabs case. Decide (on #140) whether to keep it as belt-and-suspenders or trim #140 to guard + docs. Recorded here as the cross-PRD dependency; the decision lives in #140.
- **Soft dispatch cap.** Should the daemon or the dispatcher seed impose a soft limit on concurrent dispatched worktrees per session? Decide in M1 once the authority model is concrete.
- **Experimental flag (CLAUDE.md rule 9).** The dispatcher mode is a new user-visible surface — confirm with the maintainer at `/prd-start` whether it ships behind the `experimental` flag.
