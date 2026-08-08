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

## Design record — intended dispatcher behaviour (2026-08-03)

Captured during review of PR #232 (the implementation). The seed prompt as first shipped read as a **work decomposer**, which is not what this PRD asked for, and the difference was large enough to need settling with the contributor before further changes. Recorded here so the reasoning survives the session.

**RESOLVED 2026-08-05.** The contributor agreed without reservation that the decomposer framing was a mistake, named the same narrower scope described below, and asked the maintainer to push the change. The seed was rewritten accordingly and `dispatcher` was dropped from `is_authoring_selected()`; the two open questions this record raised are now decisions. The reasoning is kept in full because it is the rationale for the seed-scope principle, which applies to every future mode seed.

### Canonical walkthrough

The dispatcher is an **ordinary conversational agent that additionally knows the `dispatch` verb**. It is not a planner.

```
Me:    Fetch all open PRDs.
Agent: (does the work and presents them, like any normal agent)
Me:    I want to work on PRD 220 by executing the /prd-full skill.
Agent: calls `dot-agent-deck dispatch prd-220 --task "Execute the /prd-full skill"`
       → worktree created, orchestration started in it, orchestrator gets the prompt
Me:    (may keep talking and dispatch more, or close the pane with Ctrl+W)
```

**The invariant, and the whole feature:** a worktree is created, an isolated orchestration (or single agent) starts in it, and the orchestrator receives one additional prompt. *What* that prompt says is entirely the user's business — the dispatcher composes it from the conversation. Two users share nothing beyond this invariant.

### Consequences

- **Starting the unit should be mechanically identical to the interactive `Ctrl+n` path, minus the dialog, plus the injected prompt.** The fix for the gap below is therefore **parity**, not a new mechanism: share the interactive composition rather than build a second one. This is also why a `--intent` CLI flag was considered and **rejected** — `--task` already carries the user's intent; nothing new needs to cross the CLI boundary. What is missing is daemon-side access to composition that currently lives TUI-side.
- **"A normal agent in the worktree" is already available and needs no flag.** `decide_target` falls back to `SpawnTarget::SingleAgent` when the target dir has no `[[orchestrations]]`, so the shape is config-driven per repo.
- **The dispatcher is just a pane.** It needs no lifecycle or "done" concept: it owns no worktrees (cleanup is keyed to each dispatched unit's own tab close via `worktree_of_record`), has nothing to summarise without a return edge, and is not in `orchestrator_pane_ids` so `work-done --done` does not apply. A done-signal whose only effect is closing the pane is ceremony — `Ctrl+W` already does it. The seed should say nothing about finishing or being one-off.
- **Several dispatches are normal, and are not decomposition.** Working on 3 PRDs in parallel means 3 dispatches of 3 things the user named. No unit-splitting judgment is involved.

### Seed-scope principle (applies to every mode seed, not just this one)

**A seed teaches Agent Deck mechanics, not work methodology.** Test: *would this sentence still be needed if the user's work were of a completely different kind?* If yes, it is mechanics and belongs; if no, it is the deck being opinionated about someone else's workflow.

Precedent confirms this is the existing convention — both schedule-authoring seeds are purely mechanical (which CLI to use and why it is validated/atomic, which flags do not apply, confirm fields before writing, where results surface). Neither offers any opinion on how to organise work. The dispatcher seed is the outlier.

Measured against it, the shipped seed should **keep**: the `dispatch` syntax; that `--task` text must be self-contained (a consequence of process isolation); the sibling worktree layout; fire-and-forget with no return edge; and (worth adding) that a dispatch name is single-use until its branch is deleted. It should **cut**: "decompose a task into independent units", "Break it into independent, parallel-ready units", "Each unit MUST be independent", "Keep the number of units reasonable (2-6)", and "NEVER do the work yourself. You ONLY decompose and dispatch" — that last one is not merely methodology but actively restrictive, since it forbids the pane from doing anything else the user asks (it would forbid step 1 of the walkthrough above).

On the "2-6 units" line specifically: "too many units overwhelm the system" is a *deck* claim, so if it is real it belongs in code as an enforced limit, not as prose advice to a model. See the open soft-dispatch-cap question.

### Known gap: the orchestrator gets no protocol (→ #222)

A dispatched orchestrator currently receives **only** the `--task` text. It is never told it is an orchestrator, what roles exist, or how to delegate:

- `prepare_orchestrator_prompt` (writes `.dot-agent-deck/orchestrator-context.md` and sends the one-line pointer to it) has exactly **one** caller — `src/ui.rs`, the interactive new-pane path. The daemon spawn path never calls it.
- `prompt_template` appears **nowhere** in `src/spawn.rs`; `RoleSpawn` carries only `role_index`, `role_name`, `command`, `is_start_role`. So per-role prompt templates are dropped on this path too.

`issue_dispatch` (#120) has the identical defect, since `dispatch` reuses that engine — this is exactly #222's "prompt order" parity item. Two implementation notes for whoever does it: nothing here is a *system* prompt (it is all PTY-typed user input, which is why combining protocol + intent into one delivery is easy), and multi-line prompts do not submit reliably through a PTY — so the shape is a combined context **file** plus a one-line pointer, following `compose_worker_task_file`, not string concatenation.

### ~~Open question for the contributor (blocking further seed work)~~ — answered 2026-08-05

Asked on PR #232 whether the decomposer framing was deliberate, rather than resolved unilaterally: the technical shape was a maintainer call, but the contributor's intent was not knowable without asking, and guessing risked building two different features. **Asking was the right call and it settled cleanly** — the framing was not solving for anything the narrower reading loses, so nothing was thrown away.

Both dependent items are now decided:

- **Seed scope → verb-teacher.** Rewritten to mechanics only, per the keep/cut list above, and pinned by `dispatcher_seed_teaches_mechanics_not_work_methodology` so the planner copy cannot drift back in.
- **`is_authoring_selected()` → `dispatcher` removed.** The "↳ authoring (one-off)" hint stays correct for the schedule modes, whose own seeds tell the user the pane existed only to write the schedule. A dispatcher pane has continued purpose, so the hint was telling the user the opposite of the truth.

## Design record — three findings from actually using it (2026-08-08)

Found by running the shipped feature by hand, *after* CI was green, Greptile returned 0 comments, and `prompt/new-pane/016` passed with a real agent. None of the three was caught by any gate, and the reason is instructive: the e2e test asserts the **worktree appears on disk** and explicitly does not assert the dispatched unit's tab or output, so all three sit underneath a passing test. Recorded here with mechanisms and `file:line` because each one looks like it works.

### 1. The dispatcher pane rendered at half width with an empty column — FIXED

`build_new_pane_request` returned `mode_config: Some(build_dispatcher_mode(...))`, and any `mode_config: Some(...)` routes to `open_mode_tab`. `mode_side_pane_dims` computes `half_width = area.width / 2 - 2` **unconditionally**, while `build_dispatcher_mode` declares `panes: []`, `reactive_panes: 0` — so the tab reserved half the width for side panes that do not exist. There is no way to opt out of the split while remaining a mode tab.

PRD #127 hit this first and left the fix in a comment on the `schedule` option: spawn as a dashboard **card** (`mode_config: None`) carrying the seed via `seed_prompt`, "so it routes to the dashboard like any single-agent card instead of through `render_mode_tab`'s 50/50 split." The dispatcher now does the same, keeping the synthetic `ModeConfig` for the cycler's title/chip only. Both branches already funnelled into the same `pending_seed_prompts` queue (`ui.rs:8185` vs `:8271`), so seed delivery was identical either way — the `ModeConfig` was a redundant seed carrier. Pinned by `dispatcher_submits_as_a_dashboard_card_not_a_mode_tab`.

### 2. A dispatched orchestration starts without the delegation protocol → #222

The reported symptom was three dispatches producing tabs where only the first agent in each did anything. Three mechanisms compose:

- `spawn`'s orchestration branch delivers `req.prompt` to exactly one pane, `orchestrator_role_index(&roles)` (`spawn.rs:318`, `:378`).
- `RoleSpawn` carries no prompt field at all (`role_index`, `role_name`, `command`, `is_start_role`), so every role's configured `prompt_template` is dropped on this path.
- `prepare_orchestrator_prompt` — which writes `.dot-agent-deck/orchestrator-context.md` with the roles list and delegation protocol — has exactly **one** non-test caller, `src/ui.rs:7794` (the interactive path). `src/spawn.rs` contains **zero** references to orchestrator context.

So the orchestrator gets the task but is never told it is an orchestrator, what roles exist, or how to `delegate`. Workers idling is *normal* for an orchestration — they wait to be delegated to; the defect is that the orchestrator can never delegate. Net effect in a repo whose first orchestration has six roles: one working agent, five idle.

Two things worth carrying to whoever fixes it. First, `RoleSpawn` does **not** need a prompt field: worker `prompt_template`s are consumed only inside `build_orchestrator_context` (the orchestrator's own template verbatim, plus each worker's `description` in an "Available agents" list), and workers get work at delegation time, not spawn time — so calling the one function covers all of it. Second, both `build_orchestrator_context` and `prepare_orchestrator_prompt` are **pure** (config in, `String`/`fs` out, no UI state), so this is a MOVE to a shared module, never a second implementation — which is what "share the interactive composition" above means concretely.

**Initially deferred, then FIXED here (2026-08-08).** The deferral was the wrong call: without this, "dispatch an orchestration" does not work at all, so the feature's headline case would have shipped non-functional. Reported from real use — an orchestration dispatch produced one working agent and idle workers — which settled it.

Fixed by MOVING both composers to `src/orchestrator_context.rs` and calling them from `spawn`'s orchestration branch. The caller's task is folded into the context file under `## Your task`, and the pointer line then says *carry out that task* rather than *wait for instructions* — leaving the wait wording would have stranded a dispatched unit idle with its task unread on disk. **Scoped to `dispatch` only, and the reason is worth recording.** The first cut enabled the composition for every producer, on the argument that "one fix covers both". The e2e tier immediately caught three #120/#127 failures — `dispatch_001`, `dispatch_004`, `spawn_002` — all asserting that the per-issue prompt text reaches the orchestrator pane VERBATIM. With a context file it arrives as a pointer, and a `cat`-based stub never reads the file. So enabling it there is not a free improvement: it changes what lands in a shipped, non-experimental feature's pane, and three tests encode the old contract.

Gated behind `SpawnRequest::compose_orchestrator_context`, `true` only for dispatch. #120 keeps its existing defect until #222 fixes it deliberately, with those tests updated as part of that work. The *shared module* is the durable win — #222 becomes a one-line flip plus test updates rather than a second implementation.

Cost still accepted: `src/spawn.rs` is shared, so a #120 or #127 regression bisects to this PR even though their behaviour is unchanged.

### 3. Nothing chose between a single agent and an orchestration — FIXED

`decide_target` took `cfg.orchestrations.first()` unconditionally, so in any repo defining `[[orchestrations]]` every dispatch produced a team — which is how finding 2 was reached by default here.

The decisive argument for asking rather than inferring: *"work on these three features"* wants a team per feature, *"verify these three PRs"* wants one agent each, and both arrive as the same words. An earlier objection that `dispatch` "cannot ask" conflated the **daemon** asking mid-dispatch (true, one-way hook socket, no interactive channel) with the **dispatcher agent** asking beforehand — which is free, because that pane is already conversational. The answer then crosses the CLI boundary as a flag.

Shipped as `--single` / `--orchestration [<name>]` (an additive `#[serde(default)]` field on `DispatchSignal`, so `PROTOCOL_VERSION` does not move) plus `--list-targets`. Two decisions inside it: an unknown orchestration name is an **error listing what is available**, never a silent fallback to something the user did not pick; and `--list-targets` is a **local** read of the repo's own config rather than a daemon round-trip, since the dispatched worktree is a copy of this repo — so it adds no wire message and no protocol surface. Distinct from the `--to` dropped in M1.0, which selected the *completion-routing* target rather than the spawn shape.

### What real use taught, and what the tests did not

Every defect in this record was found by a human running the feature, never by a gate. Worth keeping, because the pattern repeated three times:

1. **A green e2e test that asserted the wrong thing.** `prompt/new-pane/016` asserted the worktree appeared on disk, and its CATALOG entry said outright that it did not assert the dispatched unit's tab or output. So the mode-tab split, the `$SHELL` unit, and the protocol-less orchestration all sat underneath a passing real-agent test. It now asserts the card shape, that the seed reached the pane, and that the unit comes up as an AGENT — the last verified by reintroducing `command: None` and watching it fail.
2. **Assertions that could not fail.** Two waits were written against the raw PTY stream, where redrawn dashboard chrome is interleaved with cursor-positioning escapes so the text never appears contiguously; and `deck.wait_until_grid` is hard-capped at the harness `WAIT_TIMEOUT` (10s), silently cutting intended 60s waits. Grid assertions with an explicit `common::wait_until` bound fix both.
3. **A listing that could disagree with the spawn.** `--list-targets` was argued for on the grounds that "the menu is computed by the code that spawns", then implemented as a local CLI read against a different directory, a different git state, and a different name-resolution basis. The property has to be built in, not asserted in a doc comment.

The generalisable rule: **assert the thing the user sees, and prove the assertion can fail.** A test that spins up a real agent is not automatically a test of the feature.

### Also found: three sanitizers for one job

Not a live bug, but a maintenance hazard worth naming. Three functions turn arbitrary text into a safe path segment: `config_validation::sanitize_role_name:9` (strip separators, then `..`), `issue_dispatch::sanitize_clone_segment` (separators → `-`, then `..`, fallback `"issues"`), and `dispatch::sanitize_name` (strip `..`, then map non-alphanumeric → `-`, fallback `"dispatch"`).

The ordering **contradicts**: `sanitize_role_name` documents "path separators are removed first so that inputs like `./.` cannot produce `..` after slash removal", and `sanitize_name` does it in the opposite order. `sanitize_name` is nevertheless safe — it maps every non-alphanumeric including `.` to `-`, so dots cannot survive to be recombined — but it is safe by a *different* mechanism than the documented one, and nothing at that site says so. Anyone later "aligning" it with the documented style could reintroduce the exact bug that comment warns about, and the current tests assert outputs rather than the ordering property. One shared helper with one documented rule would remove the trap.

Related asymmetry: `derive_issue_paths` has a property test (`derive_issue_paths_never_escapes_working_dir`) over `/etc/passwd`, `../../escape`, `a/b/c`, `..\..\windows`; `derive_dispatch_paths` has no equivalent — and it is the one whose name comes from an LLM and which deliberately writes *outside* the repo. The property worth pinning there is "resolves to exactly one new segment in the repo's parent".

## Scope

### In Scope

**The `dispatch` verb (effector):**

- **A new agent-callable CLI subcommand** (working name `dispatch`) alongside `Delegate`/`WorkDone` (`src/main.rs:36`). Shape (to finalize in M1): a unit name/branch, an optional target orchestration selector, and the task text (with the same `--task` / `--task-file` shell-safety discipline as `Delegate`, `src/main.rs:88-118`).
- **Deterministic worktree creation**, reusing `create_worktree` (`src/issue_dispatch_run.rs:607-636`) and a user-driven analogue of `derive_issue_paths` (`src/issue_dispatch.rs:60-86`) for branch/worktree naming when there is no issue number to key on.
- **Spawn the isolated unit** by handing the worktree path to the existing `SpawnRequest { working_dir }` → `spawn` (`src/spawn.rs:228`) path, so every role inherits `cwd = worktree` exactly as issue-dispatch already does (`src/spawn.rs:324-349`). If the target directory defines an orchestration with roles, spawn the orchestration; otherwise spawn a single agent (mirrors #174's spawn table).
- **Return-edge routing** so the dispatched unit's completion reaches the *caller's* pane rather than being resolved by the `(name, cwd)` tuple (which will never match, since the caller lives in a different orchestration/cwd). Register a `dispatch-id → caller pane` callback at dispatch time and resolve it on the unit's terminal `work-done`, reusing the same `write_to_pane_and_submit` injection the local work-done loop uses. This is the one genuinely new wire and is the same mechanism PRD #174 needs.

**The dispatcher mode (trigger):**

- **A built-in dispatcher mode** — a `ModeConfig` with a `seed_prompt` (`src/project_config.rs:30`) — modeled on `build_schedule_authoring_mode` (`src/ui.rs:4271`). Its seed teaches: the `dispatch` verb and its syntax; that `--task` must be self-contained because the unit is a fresh process; the sibling worktree layout, with isolation automatic; that a name is single-use until its branch is deleted; and that it is fire-and-forget with no return edge. (This bullet originally also said "you help the user decompose … do not do the work yourself" — cut per the seed-scope decision; the pane is an ordinary agent that additionally knows the verb.)
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

- [x] **M1.0** — Define the `dispatch` CLI subcommand (`src/main.rs:36`): args (unit/branch name, optional orchestration selector, `--task`/`--task-file`), validation, and the hook-socket round-trip to the daemon. → Shipped WITHOUT the orchestration selector: `--to` was parsed, serialized, and never read, so it would have shipped in `--help` doing nothing. Dropped; #174 adds it when cross-project targeting is real.
- [x] **M1.1** — Worktree creation for a user-driven unit: reuse `create_worktree` (`src/issue_dispatch_run.rs:607`) with a non-issue naming/collision scheme; spawn the isolated unit via `SpawnRequest { working_dir }` → `spawn` (`src/spawn.rs:228`). Single-agent vs orchestration chosen from the target dir's config (mirror #174's table). → Single-agent-vs-orchestration comes free: `spawn` → `decide_target` already branches on the dispatched worktree's `.dot-agent-deck.toml`, so a repo defining `[[orchestrations]]` gets a full multi-role orchestration. The result message reports which, from `SpawnHandle::kind`. **Superseded by M1.3:** deriving the shape from config alone turned out to be the wrong default — see finding 3 in the 2026-08-08 Design record. Config remains the fallback when the caller passes no flag.
- [x] **M1.3** — The shape selector (added 2026-08-08, not in the original plan): `--single` / `--orchestration [<name>]` on the CLI, an additive `shape` field on `DispatchSignal`, and `SpawnShapeOverride` threaded through `SpawnRequest` into `decide_target_with_override`. Plus `--list-targets` as a local config read, and a seed that tells the dispatcher to ask the user before its first dispatch. The scheduler and issue-dispatch producers pass `None` and are behaviourally untouched.
- [x] **M1.2** — Cleanup lifecycle: worktree removal on tab close via the issue-dispatch bookkeeping (`remove_worktree`, `src/issue_dispatch_run.rs:133`), including the shared-worktree accounting for multi-role units. → **Reusing #120's bookkeeping naively regressed #120.** `remove_worktree` is SHARED, and the two producers need opposite dirty-tree policies: dispatch must keep uncommitted work (a sibling of the user's own checkout, LLM-chosen name), while issue-dispatch must force-remove or the reuse-the-vacated-slot model breaks — `dispatch_decision` reads a surviving worktree as "issue already claimed" and skips that issue on every later fire, permanently. Resolved by making the policy travel with the registry entry (`WorktreeEntry { clone_dir, policy }`), because the tab-close handler in `daemon_protocol.rs` serves both producers and only ever sees a path. See also the branch-leftover note under Decisions.

### Phase 2: Return-edge routing — DEFERRED

**Deliberately out of this PRD's shipped scope** (maintainer call, 2026-07-28). The callback maps *dispatched unit → dispatcher pane*, but the dispatcher is the SHORT-LIVED side (an `↳ authoring (one-off)` card) while units are long-lived, and `unregister_pane` drops the callbacks when the dispatcher closes. So even with a healthy daemon, the common case is that the dispatcher is already gone when a unit finishes — which suggests completion should be reported somewhere durable rather than injected into a pane. That is a design decision, not a coding task, and is worth making properly rather than bolting on here. Phases 1/3 ship behind the experimental flag without it; the dispatcher reports where each unit is running and the user watches the tabs appear.

- [ ] **M2.0** — Register a `dispatch-id → caller pane` callback at dispatch time; ride the id into the spawned unit.
- [ ] **M2.1** — On the dispatched unit's terminal `work-done`, resolve `dispatch-id → pane` and inject via `write_to_pane_and_submit`, bypassing the `(name, cwd)` tuple lookup; survives detach/reattach.

> **Tracking:** this is #220's OWN Phase 2, not #174's. #174 (cross-project orchestration dispatch) *depends on* this PRD — see Decisions — so deferring the return edge "to #174" inverts the dependency. Needs its own follow-up issue before #220 can close.

### Phase 3: The dispatcher mode

- [x] **M3.0** — Add the built-in dispatcher mode (`ModeConfig` + `seed_prompt`, `src/project_config.rs:30`) modeled on `build_schedule_authoring_mode` (`src/ui.rs:4271`); author its context file (what/when to call `dispatch`, one-unit-per-worktree, isolation is automatic, don't do the work). → Gated behind `features::show_dispatcher()`. **Seed rewritten to mechanics only** after the 2026-08-05 seed-scope decision — the "don't do the work" clause in this milestone's own wording was cut, since it forbade the pane from doing anything else the user asked. Pinned by `dispatcher_seed_teaches_mechanics_not_work_methodology`.
- [ ] **M3.1** — Verify the seed is delivered only on opening the dispatcher tab (scoped, zero ambient overhead) and reaches Claude/OpenCode/Pi panes uniformly. → **PARTIAL:** verified for Claude only (`prompt/new-pane/016` — the seed text is visible in the pane and the agent acts on it). OpenCode and Pi are unverified.

### Phase 4: Cross-type spawn + tests

- [ ] **M4.0** — Validate and test the mode → orchestration-tab cross-type interaction: a dispatcher *mode* tab causing new *orchestration* tabs to hydrate from daemon records (L1).
- [x] **M4.1** — L2 PTY-attached e2e: real dispatcher agent runs `dispatch`, worktree created, isolated orchestration up in a new tab (`.cast`-recording; model on `scheduler/dispatch/013`). → `prompt/new-pane/016` [reel]. Asserts through the worktree landing on disk; the dispatched unit's own TAB is not asserted (that overlaps M4.0). Two fidelity traps found here, both of which made an earlier version of this test incapable of exercising `dispatch` at all: the agent's `dot-agent-deck` resolved to a host-installed binary predating the verb (fixed by prepending the build-under-test's dir to the deck's PATH), and the harness `git init`s fixtures without committing, so `git worktree add` had no commit to branch from.
- [ ] **M4.2** — Real-agent pre-PR test: Haiku dispatcher runs `dispatch` end to end against a real clone/worktree, asserted via a uniquely-named sentinel file. → Largely subsumed by M4.1, which drives a real agent through the genuine path; a sentinel-file assertion on the dispatched unit's OUTPUT is still missing.

### Phase 5: Docs, cross-version, release

- [x] **M5.0** — Docs: dispatcher-mode + `dispatch` as the recommended parallel-work flow; repoint #140's guard/warning copy at the dispatcher mode. → `docs/develop/dispatcher-mode.md`. #140's guard copy is NOT yet repointed.
- [ ] **M5.1** — Cross-version contract check (CLAUDE.md rule 12): the return-edge callback and any spawn-request field additions are additively compatible; classify per `docs/develop/versioning.md` and add a `.breaking.md` fragment only if the contract shifts. → Not run. Static read is non-breaking, `PROTOCOL_VERSION` unchanged: `DaemonMessage` is `#[serde(tag = "message_type")]` on the hook socket and the loop's `if let Ok(msg)` SKIPS unknown variants, so an older daemon ignores a `dispatch` message rather than closing the connection (same additive shape as the `GetSeed` precedent, which also did not move the version). Classified **feature** → `changelog.d/220.feature.md`, patch bump. The manual matrix is still the stated gate.
- [x] **M5.2** — Changelog fragment (`dot-ai-changelog-fragment`); PR, Greptile review, merge, close #220. → Fragment `changelog.d/220.feature.md`. PR #232 (contributor: @irizzant). Merge + close pending; #220 cannot close while Phase 2 is outstanding.

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

## Decisions (were Open Questions)

- **Verb name and surface → `dispatch`, agent CLI only.** No interactive TUI action; the dispatcher *mode* is the only new UI surface, and it is a Mode-cycler option rather than a new command.
- **Branch/worktree naming → user-supplied name, sibling location.** `../<repo>-dispatch-<slug>` on branch `agent/dispatch-<slug>`, matching `/worktree-prd`'s `create.sh` rather than #120's in-repo `.worktrees/`. Rationale: a nested tree is walked by every `rg`, IDE index and file watcher in the parent, and `git clean -xdff` would remove it along with any uncommitted agent work. #120 legitimately differs because its `.worktrees/` lives inside a daemon-owned `gh repo clone`, never a checkout a human works in — so the rule is "never nest inside a human's checkout", not "always siblings".
- **Existing branch → refuse, and say why.** An existing `agent/dispatch-<slug>` is reported as `WorktreeCreation::BranchExists` (distinct from `AlreadyClaimed`) and refused. Silent resume is riskier here than for #120 because the name is LLM-chosen, so unrelated units colliding on something like `fix-tests` is likely rather than hypothetical. **Consequence to keep in mind:** `git worktree remove` PRESERVES the branch, so a name is single-use until the branch is deleted — the branch is never removed implicitly because it may hold that unit's committed work. The refusal message names the branch and both recovery paths.
- **Worktree removal → fail toward leaking.** Dispatch drops `--force` and gates on `git status --porcelain`; a dirty tree survives with a warning. A leaked worktree costs disk, a force-removed one costs work, and that asymmetry decides it — Ctrl+W reads as "close this view", not "destroy uncommitted work". Issue-dispatch keeps forcing (see M1.2).
- **Standalone vs #174 Phase 1 → ships standalone; #174 depends on THIS.** Recorded explicitly because the dependency has been stated backwards more than once (in `docs/develop/dispatcher-mode.md` and in PR #232 discussion, both of which deferred #220's return edge "to #174"). #174 is *Cross-project orchestration dispatch* — a separate open PRD issue, not a PR, and not a tracker for #220's Phase 2.
- **Experimental flag → yes.** `features::show_dispatcher()`, its own wrapper per CLAUDE.md rule 9 (not a reuse of `show_issue_dispatch_authoring`). Gates ONLY the Mode-cycler option; the `dispatch` verb and its daemon handler are ungated. Graduation tracked as `graduate-dispatcher`, to be filed at merge.
- **Seed scope → verb-teacher, not decomposer.** Settled with the contributor on 2026-08-05 (see the Design record). The seed teaches Agent Deck mechanics only; the planner copy is cut and pinned against by a unit test. **This generalises: a seed teaches Agent Deck mechanics, not work methodology** — the test for any future mode seed is "would this sentence still be needed if the user's work were of a completely different kind?"
- **`is_authoring_selected()` membership → `dispatcher` excluded.** It is a real mode tab with continued purpose, not a throwaway authoring card, so the "↳ authoring (one-off)" hint does not apply.

## Open Questions
- **#140 handoff — prong 1 fate.** Once distinct-cwd worktree dispatch is the norm, #140's per-tab `orchestration_id` only protects the discouraged same-cwd-two-tabs case. Decide (on #140) whether to keep it as belt-and-suspenders or trim #140 to guard + docs. Recorded here as the cross-PRD dependency; the decision lives in #140.
- **Soft dispatch cap.** Should the daemon or the dispatcher seed impose a soft limit on concurrent dispatched worktrees per session? Decide in M1 once the authority model is concrete. Note the shipped seed currently states a 2-6 range as prose advice with nothing enforcing it — if the limit is real it belongs in code.
- ~~**Experimental flag (CLAUDE.md rule 9).**~~ Decided — see Decisions.
