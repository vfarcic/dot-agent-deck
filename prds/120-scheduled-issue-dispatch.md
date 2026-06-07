# PRD #120: Scheduled agent dispatch on open GitHub issues

**Status**: Planning (blocked on #127)
**Priority**: Medium
**Created**: 2026-05-25
**GitHub Issue**: [#120](https://github.com/vfarcic/dot-agent-deck/issues/120)
**Depends on**: [#127](https://github.com/vfarcic/dot-agent-deck/issues/127) (general scheduler primitive — provides the cron + spawn primitives this PRD composes), which depends on [#126](https://github.com/vfarcic/dot-agent-deck/issues/126) (agent-driven notifications).
**Related**: `src/orchestration/`, `.dot-agent-deck.toml`, `src/worktree.rs`, the existing delegate / agent-card lifecycle

> **Scope note (2026-05-25)**: This PRD's original "Phase 1: Scheduler primitive and config" section will be removed/subsumed once #127 lands. The general scheduler (cron primitive + spawn primitive + reuse-by-default tab lifecycle) belongs in #127. What remains in this PRD's scope is the GitHub-specific layer that composes those primitives: repo provisioning (clone/pull), per-issue worktrees, issue enumeration via `gh`, idempotency (worktree-exists + linked-PR check), per-issue dedup keys, and the tab-close → worktree-cleanup hook. The detailed scope below will be revised after #127 merges.

## Problem Statement

Day-2 work on open GitHub issues is currently a manual, repetitive ritual inside dot-agent-deck:

1. The user opens GitHub, picks an issue, copies the URL.
2. If the target repo isn't on disk yet, they clone it; if it is, they `git pull`.
3. They create a worktree for the issue (`git worktree add`, choose a branch name).
4. They `cd` in, start an agent or orchestration tab, and paste the issue context as the initial prompt.

That sequence is ~5 minutes of friction per issue, has to be repeated daily, and is almost mechanical — but enough manual steps that "do five issues this morning" usually becomes "do one issue this morning, the rest tomorrow."

There is no way to:

- Say "every weekday at 09:00, pull up to N open issues from these repos and spin up agents on them."
- Reliably skip issues that already have an in-flight agent (the user has no automated dedup beyond memory).
- Branch the dispatch shape per repo — some repos have a full orchestration team in their `.dot-agent-deck.toml`, others just need a single coder. Today both cases require the same manual setup steps.

The result is a tool that is excellent for running agents *once you've set them up*, but expensive enough to set up that the user under-utilizes parallel agent work across their portfolio of repos.

## Solution Overview

Add a **scheduler** subsystem to dot-agent-deck that, on a configured cron, runs an **issue-dispatch task**. The dispatch task:

1. Enumerates open issues across a configured list of repos (with filters: max issues per run, optional label gate, optional `gh` query).
2. For each candidate issue, ensures the target repo is cloned under a configured workspace root (clone if missing, fetch+pull if present).
3. Creates a per-issue worktree on a branch like `agent/issue-<n>` inside that repo.
4. Reads the repo's `.dot-agent-deck.toml`:
   - If it defines an `[[orchestrations]]` block: opens an **orchestration tab** for that worktree and sends the issue context to the `orchestrator` role.
   - Otherwise: opens a **single agent card** in the dashboard for that worktree.
5. The tab/card persists until the user closes it. Closing the tab triggers worktree cleanup.

**Idempotency** is provided by the filesystem itself plus a single GitHub check: an issue is skipped if its worktree already exists *or* if `gh pr list` shows an open PR referencing it. No separate state file — the worktree is the ledger.

**Concurrency** falls out of the same mechanism: "max N issues per run" + skip-if-claimed means today's run only picks up new slots that yesterday's run vacated by being closed.

The scheduler is a general primitive — it runs **tasks** on a cron — but this PRD only implements one task type (`issue_dispatch`). Future task types (e.g. "daily briefing," "nightly dependency scan") can plug into the same scheduler without re-litigating the cron infrastructure.

The dispatch flow runs **in-process inside the deck**, not as a remote `/schedule` routine. Rationale: per-issue agents must be local, visible in the deck's UI, killable by the user, and able to write to local worktrees. Remote routines can't satisfy any of those.

## Scope

### In Scope

- **Scheduler subsystem** inside the deck:
  - Cron-style triggers defined in `.dot-agent-deck.toml` (new `[[scheduled_tasks]]` block, or similar — final shape decided in M1).
  - Tasks fire on schedule when the deck is running; missed fires while the deck was closed are *not* replayed (documented behavior).
  - Manual "run now" command so a scheduled task can be triggered on demand without waiting for the next tick.
- **`issue_dispatch` task type**, configurable with:
  - List of target repos (e.g. `["vfarcic/dot-ai", "vfarcic/dot-agent-deck"]`).
  - Max issues to dispatch per run.
  - Optional label filter (e.g. `agent-eligible`).
  - Optional `gh` query override for advanced users.
- **Repo provisioning**: clone-if-missing, fetch+pull-if-present, under a configured workspace root (defaults to the directory the deck was launched from).
- **Per-issue worktree** under `<repo-clone>/.worktrees/issue-<n>` (or equivalent — exact path decided in M1) on branch `agent/issue-<n>`.
- **Tab spawn branching** based on the target repo's `.dot-agent-deck.toml`:
  - Has `[[orchestrations]]` → orchestration tab; initial prompt delivered to the `orchestrator` role.
  - Does not → single agent card in the dashboard; initial prompt delivered to that agent.
- **Initial prompt** for the spawned agent/orchestrator: includes the issue URL, title, body, repo name, and worktree path. Phrased as a task (e.g. "Investigate and implement what is described in this issue …").
- **Idempotency check**: before dispatching, skip if `<repo-clone>/.worktrees/issue-<n>` exists *or* `gh pr list --search "linked:issue-<n>"` (or equivalent) returns an open PR.
- **Tab persistence**: dispatched tabs live until the user closes them. The user is in control of review / additional iteration / discard.
- **Worktree cleanup**: when the user closes the tab/card associated with an issue, the worktree is removed (`git worktree remove`). The clone is *not* removed.
- **Failure visibility**: if any step in the dispatch fails (clone error, no orchestration role despite block existing, GitHub API rate limit), the failure is surfaced to the user as a deck-level notification or a dedicated "scheduler log" view — not swallowed silently.

### Out of Scope (this PRD)

- **Other task types** beyond `issue_dispatch` (daily briefing, dependency scans, status digests, etc.). The scheduler design should not preclude them, but only `issue_dispatch` is implemented here.
- **Cross-machine scheduling.** The scheduler runs in the local deck. If the deck is closed when the cron fires, the run is skipped. We don't implement persistent / catch-up scheduling.
- **Remote `/schedule` integration.** The Claude Code `/schedule` slash command operates on a separate, cloud-side cron system; we are not bridging to it in this PRD.
- **Auto-restoration of dispatched tabs across deck restarts.** If the user quits the deck with N issue-tabs open, on next launch those tabs are not auto-restored; the worktrees on disk continue to "claim" their issues (so the scheduler won't re-dispatch them), but the user must `cd` in manually or `git worktree remove` to release the slots. Auto-restoration is a follow-up PRD (likely related to existing PRD #74 / #89 work on tab restoration).
- **Per-issue priority / smart ordering.** Issues returned by the GitHub query are processed in returned order up to the `max issues per run` cap. No scoring, no recency weighting, no "stale issue" promotion.
- **Auto-merge or auto-PR.** The dispatched agent does whatever its role prompt instructs (which, for orchestrators following the existing template, can include opening a PR). The scheduler does not add new "auto-ship" behavior.
- **Notifications to external systems** (Slack, email) when a dispatch fires or completes. Surfaced in the deck only.
- **Multi-user / shared schedulers.** Single-user, per-machine.

## Success Criteria

- A user can add a `[[scheduled_tasks]]` block to `.dot-agent-deck.toml` declaring an `issue_dispatch` task with a cron expression, a list of repos, and a per-run cap; the deck loads it on startup without further configuration.
- When the cron fires (or the user invokes "run now"), the dispatch executes end-to-end against at least one configured repo: clone/pull, worktree, tab spawn, initial prompt delivered.
- An issue with an existing worktree under `.worktrees/issue-<n>` *or* an open linked PR is skipped on subsequent runs — verified by running the dispatch twice with no intervening close.
- For a repo with an `[[orchestrations]]` block, the dispatch opens an orchestration tab and the `orchestrator` role receives the initial prompt.
- For a repo without an `[[orchestrations]]` block, the dispatch opens a single agent card and that agent receives the initial prompt.
- Closing a dispatched tab/card removes the corresponding worktree from disk (`git worktree list` no longer shows it).
- Scheduler failures (auth, network, missing role, etc.) are surfaced to the user — not silently swallowed.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` all pass on the implementation branch.
- Documentation under `site/` covers: how to configure a scheduled task, how to specify target repos, the idempotency / cleanup model, and the deck-must-be-running caveat.

## Open Questions (resolve during M1)

1. **Config schema location and shape.** Does the scheduler config live in the same `.dot-agent-deck.toml` that the deck is launched from (new `[[scheduled_tasks]]` block alongside `[[modes]]` / `[[orchestrations]]`), or in a separate file (`~/.config/dot-agent-deck/scheduler.toml`)? Working assumption: same file, project-scoped — keeps "this deck instance's configuration" in one place.
2. **Cron parser.** Use a Rust cron crate (e.g. `cron`, `tokio-cron-scheduler`) or hand-roll a simpler "every N minutes / daily at HH:MM" syntax? Working assumption: pick a maintained crate to avoid scope creep; only reach for hand-rolled if dependency footprint is bad.
3. **Workspace root resolution.** Default to "the directory `dot-agent-deck` was launched from" — but is that the *cwd* at launch time, or the directory containing the active `.dot-agent-deck.toml`? Working assumption: directory containing the config. Explicit override via config (`workspace_root = "..."`).
4. **PR-linked check.** What is the most reliable way to detect "there's already an open PR for this issue" via `gh`? Options: search `gh pr list --search "linked:#<n>"`, parse PR bodies for `Closes #<n>` / `Fixes #<n>`, or rely on GitHub's "linked PRs" API. M1 picks one and documents the limitation if any.
5. **Initial prompt format.** What exactly do we send to a single agent vs. an orchestrator? Working assumption: a short template ("Work on issue <url>. Title: <…>. Body: <…>. Worktree: <…>.") that, for orchestrators, is suffixed with "Coordinate the team per the project's orchestration template." Template content finalized in M2.
6. **Tab close → worktree cleanup hook.** Is there already a "tab closed" event we can attach to, or does this require new plumbing? Working assumption: tab-close hooks likely already exist for agent shutdown; reuse them.
7. **Concurrency safety.** If two scheduled task runs overlap (e.g. a slow run still in progress when the next tick fires), do we serialize, skip, or run them in parallel? Working assumption: skip-if-prior-run-still-active, with a log entry.

## Milestones

### Phase 1: Consume #127's scheduler primitives (do NOT rebuild)

> **Revised 2026-06-07 — #127 has landed.** The general scheduler is done and
> this PRD now **composes** it rather than building its own. Do not add a second
> cron engine, config schema, or spawn path. What #127 already provides and #120
> consumes:
> - **Cron primitive** (`src/scheduler.rs`): `Scheduler::register(name, cron, callback)`, `run_now`, `tick_at`, the daemon-side firing loop, skip-if-prior-run-still-active, and `reload_apply`. #120's issue-dispatch is simply *"a registered callback whose body calls `spawn` N times"* — register a callback on this scheduler; do not reimplement cron evaluation.
> - **Spawn primitive** (`src/spawn.rs`): `spawn(SpawnRequest{ task_name, working_dir, command, prompt }) -> SpawnHandle`. It already does `mkdir -p` + fail-loud via the notifier, branches on the target dir's `.dot-agent-deck.toml` (orchestration tab vs single-agent card), delivers the prompt, and reuse-by-default lifecycle. #120 calls this per issue (different `working_dir`/`prompt` per worktree) instead of duplicating the spawn/branch logic.
> - **Tab-closed cleanup seam**: `SpawnHandle.on_tab_closed` (`TabClosedCallback`) is the hook #120 registers for **per-issue worktree cleanup** (M2.4) — its close-detection wiring is the addition #120 makes; the seam exists so #120 needs *additions*, not breaking changes.
> - **Config + CLI**: the global `~/.config/dot-agent-deck/schedules.toml` schema, the `dot-agent-deck schedule …` validated writer, and the "schedule" authoring mode / "Scheduled Tasks" manager UX all exist. #120 does **not** add a competing config or UI for the cron part; it layers its GitHub-specific fields (repo list, label filter, `max_per_run`, query) on top, ideally as a distinct task type that reuses the same machinery.
>
> The remaining #120 scope is purely the **GitHub layer** (Phases 2–4 below): repo provisioning, per-issue worktrees, `gh` enumeration, idempotency/dedup, and the worktree-cleanup hook. The original M1.1–M1.3 below are **subsumed by #127** and kept only for historical reference.

- [x] ~~**M1.1** — Decide config schema.~~ *Subsumed by #127 (`config::ScheduledTask` + the global `schedules.toml`).*
- [x] ~~**M1.2** — Implement the scheduler engine.~~ *Subsumed by #127 (`src/scheduler.rs`: cron loop, `run_now`, skip-if-running, `reload_apply`).*
- [ ] **M1.3** — Surface scheduler state / failure notifications for issue-dispatch runs. Reuse #127's notification seam (PRD #126) rather than adding a parallel one; only the issue-dispatch-specific events (per-repo/per-issue success/skip/failure) are new here.

### Phase 2: Issue-dispatch task type

- [ ] **M2.1** — Implement repo provisioning: clone-if-missing, fetch+pull-if-present, under the resolved workspace root. Exposed as a reusable internal API so future task types can call it.
- [ ] **M2.2** — Implement per-issue worktree creation and the idempotency check (worktree presence + open linked-PR check via `gh`). Skip + log when an issue is already claimed.
- [ ] **M2.3** — Implement the dispatch branching: read the target repo's `.dot-agent-deck.toml`, choose orchestration-tab vs. single-agent-card, spawn accordingly, deliver the initial prompt.
- [ ] **M2.4** — Implement tab-close → worktree cleanup hook. Closing the dispatched tab/card removes the worktree; the clone is preserved.

### Phase 3: Filters, robustness, polish

- [ ] **M3.1** — Implement the optional label filter and the optional `gh` query override. Defaults to "all open issues, up to `max_per_run`."
- [ ] **M3.2** — Failure handling: auth/network/GitHub API errors are caught at task boundaries, logged, and surfaced to the user; one failing repo does not abort the rest of the run.
- [ ] **M3.3** — Tests: unit tests for the scheduler engine, integration tests for the dispatch end-to-end (using a fixture repo or a real test repo per the established testing approach with `dot-ai-infra`).

### Phase 4: Docs and ship

- [ ] **M4.1** — User docs under `site/` covering configuration, the idempotency / cleanup model, the deck-must-be-running caveat, and a complete example `.dot-agent-deck.toml` with a `[[scheduled_tasks]]` block.
- [ ] **M4.2** — Final pass: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`. Open follow-up issues for known out-of-scope items (tab auto-restoration, additional task types, external notifications).

## Risks and Mitigations

- **Risk**: An agent in a dispatched worktree pushes broken code to a branch named `agent/issue-<n>` and opens a PR before the user reviews. **Mitigation**: the spawned agent inherits whatever discipline the target repo's role prompts impose (most orchestrators in practice run `cargo test` / equivalent before PR; this is enforced by the *target repo's* config, not the scheduler). The scheduler does not add auto-merge. Users opting into scheduled dispatch are accepting that their repos' role prompts are the gate.
- **Risk**: The deck accumulates abandoned worktrees because the user never closes tabs from yesterday's dispatch. **Mitigation**: documented behavior — the scheduler explicitly defers cleanup to user action. A future PRD can add an "abandon after N days" sweeper if this becomes a real pain.
- **Risk**: `gh pr list` returns false negatives (open PR exists but the linking is informal — e.g. PR title mentions the issue but no `Closes` keyword), causing duplicate dispatch. **Mitigation**: use the structured "linked PRs" API where possible; document the heuristic; rely on the worktree-presence check as the primary defense (covers 99% of cases since duplicate dispatch implies the prior worktree was *also* cleaned up).
- **Risk**: Cron expression mistakes (every minute instead of every day) cause runaway dispatch. **Mitigation**: log every fire prominently in the deck UI so a runaway is immediately visible; the per-run cap puts a hard ceiling on how many agents can spawn before the user notices.
- **Risk**: The scheduler subsystem becomes a generic plugin framework before it's earned that complexity. **Mitigation**: this PRD implements exactly one task type. The scheduler API is *internal*; future task types are added by editing the deck's code, not via a plugin protocol. We can refactor toward plugins later if a second task type proves it's worth it.
- **Risk**: Repos without a `devbox` setup but with an `[[orchestrations]]` block will fail to spawn (the orchestration roles reference `devbox run agent-*`). **Mitigation**: M2.3 validates that the spawn command is resolvable before committing to orchestration mode; on failure, falls back to single-agent-card with a logged warning. (This needs a small "is the command runnable" check — captured as a sub-task.)

## Dependencies

- The existing orchestration tab and agent-card subsystems must continue to expose stable internal APIs for "create a tab from a working directory + initial prompt." If they don't yet, that gets surfaced during M2.3 and may pull in a small refactor.
- GitHub CLI (`gh`) available on the user's path and authenticated. This is already a project-wide assumption (see existing PRD-creation flow).
- A maintained cron-expression crate in the Rust ecosystem (resolved in M1.2).
- No external services or APIs beyond GitHub.

## Validation Strategy

- **Unit**: scheduler engine tests cover cron evaluation, skip-if-running, manual trigger.
- **Integration**: end-to-end test that points the scheduler at a fixture repo with a known open issue, runs one dispatch, asserts that a worktree exists, that an agent/orchestration tab was created, and that the idempotency check skips a second run. Uses a real `gh`-authenticated test repo where feasible (consistent with the project's preference for real over mocked integration tests per `feedback_always_fix_failures`).
- **Manual**: the user (per `feedback_validate_pre_pr`) validates one end-to-end run against a real repo before merge: scheduler config in `.dot-agent-deck.toml`, cron fires (or "run now" used), at least one issue is dispatched, the spawned agent receives the initial prompt with the right issue context, closing the tab removes the worktree.
- **Regression**: existing modes, orchestrations, dashboard, and worktree tests continue to pass. The scheduler subsystem is additive; it should not change the shape of any existing test.
