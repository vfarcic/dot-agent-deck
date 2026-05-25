# PRD #127: Cron-scheduled prompt dispatch (general scheduler primitive)

**Status**: Planning
**Priority**: Medium
**Created**: 2026-05-25
**GitHub Issue**: [#127](https://github.com/vfarcic/dot-agent-deck/issues/127)
**Depends on**: [#126](https://github.com/vfarcic/dot-agent-deck/issues/126) (agent-driven notifications — used for failure surfacing)
**Prerequisite for**: [#120](https://github.com/vfarcic/dot-agent-deck/issues/120) (issue-dispatch — composes the primitives here with GitHub-specific logic)
**Related**: PRD #74 / #89 (tab restoration — out-of-scope here, downstream concern)

## Problem Statement

The deck is a great place to run agents *once you've set them up manually*, but there's no way to say "every weekday at 09:00, run this prompt in this directory." Every periodic task is a manual ritual: open a terminal at the right time, `cd` to the right place, paste the prompt, watch.

Two concrete shapes that motivate this:

1. **Static-prompt schedules.** "Every morning 09:00: generate a Barcelona weather forecast plus the list of GitHub issues opened in the last 24h across these repos." Or: "Daily: analyze these stock tickers and suggest buy/hold/sell plus 5 tech-stock candidates worth watching." Plain prompt, fixed phrasing, runs on a cadence, output lands in the deck where the user can read it after a notification.
2. **Dynamic-prompt schedules.** "Every weekday 09:00: enumerate the 5 newest open issues across my repos and spawn an agent on each." This is what PRD #120 (issue-dispatch) wants. It is *not* a different scheduler — it is a scheduled callback whose body happens to call spawn N times instead of once.

Both shapes need the same two primitives: *something fires on a cron* and *something opens a deck tab from a working directory plus a prompt*. The deck has neither. Without them, users either don't bother, or hand-roll cron entries that invoke `claude-code` outside the deck and lose all the UI, history, and orchestration affordances the deck provides.

## Solution Overview

Add an **in-process scheduler subsystem** to the deck with two small, orthogonal primitives:

1. **Cron primitive**: `register(cron_expr, callback) → handle`. Evaluates cron expressions, fires callbacks when the deck is running. Includes a skip-if-prior-run-still-active rule (the next tick is skipped if the previous callback for the same task hasn't returned). Includes a manual "run now" trigger so a scheduled task can be invoked on demand without waiting for the next tick.

2. **Spawn primitive**: `spawn(working_dir, prompt) → handle`. Reads the target directory's `.dot-agent-deck.toml`:
   - If it defines an `[[orchestrations]]` block → opens an **orchestration tab** rooted at that directory and delivers the prompt to the `orchestrator` role.
   - Otherwise → opens a **single agent card** rooted at that directory and delivers the prompt to that agent.
   - If the directory does not exist, the scheduler creates it (`mkdir -p`). If creation fails, the failure is surfaced via the notification channel from PRD #126.

A scheduled prompt is just "a cron whose callback calls `spawn` once." PRD #120's issue-dispatch is "a cron whose callback enumerates issues and calls `spawn` N times." The complexity that's specific to #120 — worktrees, GitHub API enumeration, per-issue identity, dedup against existing PRs — lives in #120, not here. The scheduler PRD ships only the two primitives plus configuration.

### Tab lifecycle: reuse-by-default

Users primarily learn about scheduled task fires through notifications (PRD #126). They open the deck to dig into a result *if* they choose to. Once they do, they read, interact, and close. That access pattern means **reuse** is the dominant default, not new-tab-per-fire:

- **Default**: a scheduled task reuses the same tab/card each fire. Yesterday's weather output is replaced by today's. One weather tab, ever.
- **Opt-in**: `new_tab_per_fire = true` per task, for cases where the user actually wants per-fire history (audit-style scheduled tasks). Documented up-front, not the default.

Mid-interaction reuse semantics are nailed down in M2.

### Configuration shape

A new `[[scheduled_tasks]]` block in `.dot-agent-deck.toml`:

```toml
[[scheduled_tasks]]
name = "morning-digest"
cron = "0 9 * * MON-FRI"
working_dir = "~/scheduled/morning-digest"
prompt = """
Generate a brief: Barcelona weather forecast for today, plus the list of
GitHub issues opened in the last 24h across vfarcic/dot-ai and
vfarcic/dot-agent-deck. Notify when done.
"""
new_tab_per_fire = false   # optional, default false (reuse)
```

The scheduler runs **in-process inside the deck**, not as a remote routine. Reasons: spawned tabs must be local, visible in the deck UI, killable by the user, and able to read/write the local working directory. Cron fires while the deck is closed are *not* replayed — documented behavior, no catch-up logic.

## Scope

### In Scope

- **Cron primitive**:
  - Load `[[scheduled_tasks]]` from `.dot-agent-deck.toml` at deck startup.
  - Evaluate cron expressions and fire callbacks on schedule (in-process).
  - Skip-if-prior-run-still-active concurrency rule, with a log entry surfaced via notifications.
  - Manual "run now" trigger per task (via CLI subcommand or dashboard action — final shape decided in M1).
- **Spawn primitive**:
  - `spawn(working_dir, prompt) → handle` that branches on the target directory's `.dot-agent-deck.toml`.
  - Auto-create `working_dir` if missing (`mkdir -p`); fail loud (surface via notifications) on creation errors (permissions, missing mount, etc.).
  - When the working directory has no `.dot-agent-deck.toml`, default to a single-agent card.
- **Tab lifecycle**:
  - Reuse-by-default per scheduled task.
  - `new_tab_per_fire = true` opt-in flag.
  - Mid-interaction reuse semantics: if the user is actively typing into the tab when the next fire occurs, the new prompt is queued until idle; if the user is idle, the new prompt is delivered immediately. ("Idle" defined in M2.)
- **Configuration**: `[[scheduled_tasks]]` blocks in `.dot-agent-deck.toml` (project-scoped, same file as `[[modes]]` and `[[orchestrations]]`).
- **Cron parsing**: pick a maintained Rust cron crate (`cron` or `tokio-cron-scheduler`) in M1 based on dep weight; do not hand-roll.
- **Failure surfacing**: clone errors, mkdir errors, spawn errors, missing role in `[[orchestrations]]`, etc. all flow into the notification channel from PRD #126. No silent swallowing.
- **Documentation** under `site/` covering `[[scheduled_tasks]]` syntax, the in-process / deck-must-be-running caveat, the reuse-by-default model, and a complete example.

### Out of Scope (this PRD)

- **GitHub API calls, issue enumeration, repo cloning/pulling, worktrees.** All of these belong to #120 (issue-dispatch), which composes the cron and spawn primitives provided here. The scheduler API must be sufficient for #120 to build on, but no GitHub-aware code lives in this PRD.
- **N-spawn-per-fire as a typed feature.** The primitive is one spawn per call; if a callback wants N spawns it loops over spawn() N times. Per-spawn identity, dedup keys, idempotency checks, cleanup hooks for batch spawns — all #120's problem.
- **Other task types** beyond the static-prompt pattern (e.g. "scan dependencies," "post a daily status digest somewhere"). The cron primitive is general enough to support them; this PRD does not implement them.
- **Cross-machine / catch-up scheduling.** Cron fires while the deck is closed are not replayed. No persistent queue, no cloud-side scheduler bridge.
- **Remote `/schedule` integration.** Claude Code's `/schedule` is a separate cloud-side cron; we are not bridging.
- **Auto-restoration of scheduled-task tabs across deck restarts.** If the deck is closed with N scheduled-task tabs open, they don't auto-restore on next launch. Resolution is deferred to the existing PRD #74 / #89 work on tab restoration.
- **Per-task per-event notification routing.** Failures and "task completed" notifications go through whatever PRD #126 provides — no per-task channel overrides here.
- **Concurrency across distinct scheduled tasks.** Two different tasks firing at the same time run in parallel; only the same-task-overlap case is serialized (skip-if-prior-run-active).

## Success Criteria

- A user can add a `[[scheduled_tasks]]` block with a `cron`, `working_dir`, and `prompt` to `.dot-agent-deck.toml`; the deck loads it at startup without further configuration.
- When the cron fires (or the user invokes "run now"), the scheduler executes end-to-end: working_dir auto-created if missing, target `.dot-agent-deck.toml` consulted, orchestration tab or single-agent card spawned, prompt delivered.
- For a target dir with an `[[orchestrations]]` block, the spawn opens an orchestration tab and the `orchestrator` role receives the prompt.
- For a target dir without an `[[orchestrations]]` block, the spawn opens a single agent card and that agent receives the prompt.
- With `new_tab_per_fire = false` (the default), the *same* tab is reused on each fire — no tab accumulation over time.
- With `new_tab_per_fire = true`, each fire opens a fresh tab.
- Skip-if-prior-run-still-active: if a scheduled task's callback hasn't returned by the next tick, that tick is skipped and a notification is fired (via PRD #126).
- A `working_dir` creation error (permissions, etc.) does not crash the deck; it fires a notification and the rest of the scheduled tasks continue to run.
- The cron primitive is exposed internally such that a future PRD #120 implementation can register a callback that, on fire, calls `spawn` N times with different per-issue working directories and prompts — without modifying the scheduler module.
- `cargo fmt --check` and `cargo clippy -- -D warnings` pass. `cargo test` passes including new tests.
- Documentation under `site/` covers configuration, the reuse model, and the deck-must-be-running caveat.

## Open Questions (resolve during M1)

1. **Cron crate choice.** `cron` is small and pure-Rust; `tokio-cron-scheduler` integrates more deeply with async runtimes but pulls more deps. Working assumption: `cron` for expression parsing plus a thin tokio-side loop that we own; revisit if it's awkward.
2. **"Run now" UX surface.** Three options: a CLI subcommand (`dot-agent-deck schedule run-now <name>`), a dashboard action (keypress when focused on a "scheduled tasks" panel), or both. Working assumption: CLI subcommand in M1, dashboard action in a polish milestone if it earns it.
3. **Mid-interaction "idle" definition for reuse.** Options: (a) no PTY input from user for N seconds; (b) tab not focused; (c) explicit "ready" state exposed by the agent. Working assumption: (a) with a short threshold (~5 seconds) — simple and correct enough. M2 confirms.
4. **Working directory path expansion.** `~`, `$VAR`, relative paths — how thoroughly do we expand? Working assumption: `~` and `$VAR` expansion at load time; relative paths resolved against the directory containing `.dot-agent-deck.toml` (matching how `[[orchestrations]]` paths likely behave today).
5. **Spawn handle reuse identity.** When `new_tab_per_fire = false`, the scheduler needs to look up the existing tab for "morning-digest." Likely keyed by scheduled task name. M2 confirms the registry shape.

## Milestones

### Phase 1: Cron primitive + configuration

- [ ] **M1.1** — Pick the cron crate based on `cargo tree` weight. Implement cron-expression evaluation and a tokio-backed loop that fires registered callbacks on schedule.
- [ ] **M1.2** — Add the `[[scheduled_tasks]]` block to the `.dot-agent-deck.toml` schema. Load tasks at deck startup; surface load errors via notifications (PRD #126).
- [ ] **M1.3** — Implement skip-if-prior-run-still-active and the "run now" CLI subcommand (`dot-agent-deck schedule run-now <name>`).

### Phase 2: Spawn primitive + lifecycle

- [ ] **M2.1** — Implement `spawn(working_dir, prompt) → handle`. Auto-create the working_dir; fail-loud on errors. Branch on the target's `.dot-agent-deck.toml` between orchestration tab and single-agent card.
- [ ] **M2.2** — Tab lifecycle: reuse-by-default. Implement the named-tab registry (one tab per scheduled task name) and the mid-interaction queue (deliver-on-idle).
- [ ] **M2.3** — Wire the cron callback for the static-prompt case: a registered scheduled task fires → callback calls `spawn(working_dir, prompt)` exactly once with the configured values.

### Phase 3: Tests + integration with notifications

- [ ] **M3.1** — Unit tests: cron expression evaluation, skip-if-running rule, working_dir creation success/failure paths, spawn branching (orchestration vs. single agent), tab reuse vs. new_tab_per_fire.
- [ ] **M3.2** — Integration test: fixture `.dot-agent-deck.toml` with a `[[scheduled_tasks]]` block on a fast cron (e.g. every second), assert spawn fires and prompt is delivered. Trigger a working_dir creation failure, assert notification fires via PRD #126.
- [ ] **M3.3** — Manual validation: real scheduled task in a real `.dot-agent-deck.toml` fires at 09:00, output appears in the expected tab, notification arrives.

### Phase 4: Docs and ship

- [ ] **M4.1** — Documentation under `site/`: `[[scheduled_tasks]]` reference, the reuse-by-default model, mid-interaction queue semantics, the deck-must-be-running caveat, a complete `.dot-agent-deck.toml` example with a scheduled task plus orchestrations.
- [ ] **M4.2** — Changelog fragment via `dot-ai-changelog-fragment`. Frame as "you can now schedule prompts to run on a cron and land in the deck."
- [ ] **M4.3** — `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` all green. PR, review, audit, merge.
- [ ] **M4.4** — Open the follow-up PRD #120 update: revise its Phase 1 scope to consume this scheduler's primitives instead of duplicating them.

## Key Files

- `src/scheduler.rs` (new) — cron loop, registered tasks, run-now, skip-if-running.
- `src/spawn.rs` (new) — `spawn(working_dir, prompt) → handle`, branching on target `.dot-agent-deck.toml`, mkdir+fail-loud.
- `src/config.rs` — extend with the `[[scheduled_tasks]]` block.
- `src/cli.rs` (or equivalent) — `dot-agent-deck schedule run-now <name>` subcommand.
- `tests/scheduler.rs` (new) — unit + integration tests.
- `site/content/docs/scheduled-tasks.md` (new) — user-facing documentation.

## Risks and Mitigations

- **Risk**: A user writes a runaway cron (every minute instead of every day) and floods the deck with tabs.
  - *Mitigation*: Every fire emits a notification (via PRD #126). A runaway is immediately visible. Reuse-by-default means even high-frequency fires don't accumulate tabs.

- **Risk**: The spawn primitive's `.dot-agent-deck.toml` lookup couples this PRD too tightly to the existing config-loading code, making future config changes hard.
  - *Mitigation*: M2.1 isolates the lookup behind a small helper (`load_config_for_dir(path) → Config`) that other code can also use. No reaching into config internals from the scheduler module.

- **Risk**: The mid-interaction "deliver-on-idle" queue is the most likely source of subtle bugs (race between user typing and timer-based delivery).
  - *Mitigation*: M2.2 implementation uses a simple "last-keystroke timestamp + 5-second debounce" rather than anything more elaborate. Documented behavior; if it's wrong in practice, refine in a follow-up.

- **Risk**: Notifications dependency (PRD #126) slips, blocking this PRD.
  - *Mitigation*: If #126 hasn't landed by M3.2, this PRD can ship with a stub that logs to stderr instead of firing notifications. The dependency is documented but not blocking for development — only for shipping.

- **Risk**: PRD #120 finds that the spawn primitive's signature is insufficient (e.g. needs to return more metadata, or needs an on-close hook for worktree cleanup).
  - *Mitigation*: M2.1 designs the spawn return type with one eye on #120's needs: include a stable handle, expose a "tab closed" callback registration. Implementing #120 will likely require *additions* to the spawn API but should not require breaking changes.

- **Risk**: Cron parsing crates may drift in maintenance; picking a heavy one increases dep weight long-term.
  - *Mitigation*: M1.1 evaluates `cargo tree` weight and maintenance status. If both candidates are bad, hand-roll a minimal evaluator (`every N minutes / daily HH:MM / weekdays HH:MM`) — explicitly accept the loss of full cron expressivity in exchange for zero dep.

## Dependencies

- **PRD #126** (agent-driven notifications) — used for surfacing scheduler failures (load errors, mkdir errors, spawn errors, skip-if-running). Hard dependency for shipping; soft dependency for development (stub if needed).
- Existing config-loading code for `.dot-agent-deck.toml`. Extended with `[[scheduled_tasks]]`.
- Existing tab/orchestration spawn code paths. M2.1 calls into them rather than reimplementing.
- A maintained cron crate (`cron` or equivalent) — decided in M1.1.
- No external services, no new third-party credentials.

## Validation Strategy

- **Unit**: cron expression evaluation, skip-if-running rule, working_dir mkdir success/failure, spawn branching (orchestration block present vs. absent), tab reuse registry, new_tab_per_fire behavior, mid-interaction deliver-on-idle.
- **Integration**: end-to-end test with a fast-cron fixture (`* * * * *` or a custom in-test trigger), real spawn into a fixture `.dot-agent-deck.toml`, assert tab is created with the right contents. Failure injection: read-only `working_dir`, assert notification fires via PRD #126 and the rest of the scheduled tasks still run.
- **Manual** (per `feedback_validate_pre_pr`):
  - Configure a real scheduled task firing every 2 minutes; verify the spawn happens, the prompt is delivered, and the notification (via #126) arrives.
  - Toggle `new_tab_per_fire = true` and verify a new tab opens each fire instead of reuse.
  - Trigger a typo'd `working_dir` (path on nonexistent mount); verify a notification fires and the deck doesn't crash.
- **Regression**: existing modes, orchestrations, dashboard, tab-lifecycle, and config-loading tests continue to pass. The scheduler is additive — it should not change the shape of any existing test.

## CLAUDE.md Compliance

- `cargo fmt --check` and `cargo clippy -- -D warnings` before every commit (project rule #2).
- No `m*_*` or `prd*_*` prefixes in source/test filenames (project rule #3). Use semantic names: `src/scheduler.rs`, `src/spawn.rs`, `tests/scheduler.rs`.
- Ask before creating branches or worktrees (project rule #1). `/prd-start` will prompt accordingly.
