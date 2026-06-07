# PRD #127: Cron-scheduled prompt dispatch (general scheduler primitive)

**Status**: Planning
**Priority**: Medium
**Created**: 2026-05-25
**Updated**: 2026-06-07 (revised after architecture/design discussion — see Design Decisions log)
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

Add a **scheduler subsystem that lives inside the daemon** (not the TUI) with two small, orthogonal primitives:

1. **Cron primitive**: `register(cron_expr, callback) → handle`. Evaluates cron expressions, fires callbacks while the daemon is running. Includes a skip-if-prior-run-still-active rule (the next tick is skipped if the previous callback for the same task hasn't returned). Includes a manual "run now" trigger so a scheduled task can be invoked on demand without waiting for the next tick.

2. **Spawn primitive**: `spawn(working_dir, prompt) → handle`. Reads the *target directory's* `.dot-agent-deck.toml`:
   - If it defines an `[[orchestrations]]` block → opens an **orchestration tab** rooted at that directory and delivers the prompt to the `orchestrator` role.
   - Otherwise → opens a **single agent card** rooted at that directory and delivers the prompt to that agent.
   - If the directory does not exist, the scheduler creates it (`mkdir -p`). If creation fails, the failure is surfaced via the notification channel from PRD #126.

A scheduled prompt is just "a cron whose callback calls `spawn` once." PRD #120's issue-dispatch is "a cron whose callback enumerates issues and calls `spawn` N times." The complexity that's specific to #120 — worktrees, GitHub API enumeration, per-issue identity, dedup against existing PRs — lives in #120, not here. The scheduler PRD ships only the two primitives plus configuration and management UX.

### Why the daemon, not the TUI (architecture)

The deck runs as a **detached, long-lived daemon** plus one or more **stateless TUI clients** that attach to it. Agents (PTYs) are owned by the daemon and survive the TUI closing (`hydrate_from_daemon()` re-attaches on reconnect). The scheduler MUST live in the daemon so that scheduled fires keep happening after the user detaches the TUI — putting it in the TUI would tie autonomous scheduling to a human keeping a terminal open, which defeats the purpose.

Three daemon-lifecycle facts shape the rest of this PRD:

1. **Idle shutdown.** The daemon auto-exits ~30s after `clients == 0 && live_count == 0` (`DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS`, default 30; `0` disables). `live_count()` counts only agents whose PTY process is still alive (`!exited`) — *not* registry entries (`len()`), so an open-but-finished pane does **not** pin the daemon. **Implication:** a registered scheduled task must become a *third keep-alive condition* — the idle gate becomes `clients == 0 && live_count == 0 && no_pending_schedules`. Otherwise the daemon GCs itself between fires (e.g. before the first fire of a freshly-added daily task, or after a reused agent exits) and the cron loop dies silently.

2. **Interactive agents stay alive.** The deck spawns *interactive* PTY sessions (a shell, or `claude` interactive) and **injects the prompt into the PTY's stdin** via `write_to_pane_and_submit` — there is no `claude -p` one-shot anywhere. So a scheduled spawn normally leaves a live session sitting at its prompt (`live_count > 0`), which keeps the daemon up on its own. The idle carve-out above is the safety net for the windows where no live agent exists.

3. **No persistence across daemon restart.** Stopping the daemon (upgrade, `daemon stop`, `daemon restart`, crash) SIGTERM→SIGKILLs every agent; the registry is in-memory and is rebuilt empty on the next `daemon serve`. **Nothing auto-respawns the daemon** — it is lazy-spawned by the next TUI invocation. Schedule *definitions* survive a restart because they are reloaded from the global config file (below), but **in-flight runs are lost, the in-memory tab-reuse registry is wiped, and fires that come due while the daemon is down are not replayed** (no catch-up). Genuinely unattended scheduling across reboots/upgrades therefore depends on daemon supervision — see Open Questions and Risks.

### Configuration: global, daemon-owned

Schedule **definitions** live in a single **global, per-user** config file, owned and read by the daemon at startup — **not** in the per-project `.dot-agent-deck.toml`. The daemon is global (one per user); its schedule source must be global too. A per-project file would make "which schedules are active" depend on which directory the deck was last launched from, which is incoherent for a global daemon.

```toml
# ~/.config/dot-agent-deck/schedules.toml   (XDG_CONFIG_HOME; persistent, survives reboot)

[[scheduled_tasks]]
name = "morning-digest"
cron = "0 9 * * MON-FRI"
working_dir = "~/scheduled/morning-digest"
command = "claude"         # agent command for the single-agent card (mirrors the new-deck dialog); optional, falls back to $SHELL
prompt = """
Generate a brief: Barcelona weather forecast for today, plus the list of
GitHub issues opened in the last 24h across vfarcic/dot-ai and
vfarcic/dot-agent-deck. Notify when done.
"""
new_tab_per_fire = false   # optional, default false (reuse)
enabled = true             # optional, default true
```

`command` is the agent command for the **single-agent** spawn, exactly as in the new-deck dialog. When the target dir has an `[[orchestrations]]` block, the orchestration's own role commands win and `command` is ignored (same as the dialog when an orchestration is selected). When omitted it falls back to `$SHELL` — consistent with the dialog — so the creation agent/CLI prompts for it since a bare shell can't act on a prompt.

Two distinct configs, and only the first moves to global:

| Config | Scope | Role |
|---|---|---|
| **Where a schedule is *defined*** (`name`, `cron`, `working_dir`, `command`, `prompt`, `new_tab_per_fire`, `enabled`) | **Global** — `~/.config/dot-agent-deck/schedules.toml` | The daemon's job list |
| **What a fire *spawns into*** — the target `working_dir`'s `.dot-agent-deck.toml` | **Per-dir** (unchanged) | Decides orchestration-tab vs single-agent-card at fire time |

The per-project `.dot-agent-deck.toml` does not change role: it still defines `[[modes]]`/`[[orchestrations]]` and supplies the **spawn target's** orchestration shape. It simply stops being where schedules are declared.

The config file is the source of truth. The daemon reads it on startup and runs the cron loop from it. Edits (from any of the three doors below) trigger a daemon reload via a `ReloadSchedules` control message over the existing daemon socket (consistent with `daemon stop` / `ListAgents` / `Delegate`); a reload on next `daemon serve` is the fallback. Cron fires while the daemon is closed are **not** replayed — documented behavior, no catch-up logic.

### Three doors to edit the config

All mutation funnels through one validated path; users pick the ergonomics they want:

1. **Agent-driven (primary UI door).** Converse with an agent that constructs the entry and calls the CLI. Best for the prompt-heavy authoring case.
2. **CLI directly.** `dot-agent-deck schedule add|update|remove|list|disable|enable|run-now|reload`. Scriptable; the fast path for trivial edits (e.g. `schedule disable morning-digest`).
3. **Hand-edit the file.** The TOML is human-readable; `schedule reload` (or a daemon restart) picks it up.

The CLI is the single validated writer (cron validation, `~`/`$VAR` expansion, atomic write to the fixed global path **regardless of cwd**, daemon reload). The agent never freehand-edits TOML — it calls the CLI, so an LLM can't silently produce a malformed cron or an unescaped multi-line prompt.

### Creation UX: agent-driven via a "schedule" mode

Creation reuses the **existing new-deck/new-pane dialog** (`NewPaneFormState`). A new selectable **"schedule" mode** spawns an agent **pre-seeded with instructions** on how to add a schedule entry; the user converses to construct the prompt and other fields, and the agent calls `schedule add` once the user confirms.

This reuses the **orchestration seed-prompt-on-spawn pattern that already exists** (orchestrations write a context file and auto-deliver a seed prompt to the spawned agent once it's ready, via `write_and_submit_to_pane` with an agent-ready gate + buffer delay). Plain `[[modes]]` don't yet auto-deliver a seed prompt to the *agent* pane (only `init_command` to side panes); adding that is a small, well-precedented change (`seed_prompt: Option<String>` on `ModeConfig`, threaded through `NewPaneRequest`, delivered like orchestrations do).

Why an agent instead of a form: the hardest field is the **prompt**, which is exactly what conversing with an agent excels at — and the user can **test the prompt in the same session** ("run it now, show me") before committing it. This avoids building bespoke TUI form widgets (multi-line editor, live cron validator). The seed prompt must be crisp: it carries the field list, the `schedule add` invocation, the validation rules, and an explicit "confirm the full entry with me before writing."

Note: "schedule" is semantically different from other dialog options — it creates a **throwaway authoring session** that writes config and is then done, not a long-lived workload. A subtle visual separation in the dialog should signal this.

### Management UX: the "Scheduled Tasks" dialog

A single **keybinding** opens a **"Scheduled Tasks" manager dialog** — the canonical home for the concept. It is **read-only-plus-actions** (no in-place field editing, to keep a terminal dialog simple):

- **List** schedules with a live / idle / **disabled** status indicator and the next-fire time.
- **`a` / on no row → add**, **`Enter` / `e` on a row → edit**: both spawn the seeded agent (edit pre-fills current values; agent calls `schedule add` / `schedule update`).
- **`d` → delete**: confirmation, then removes the **definition only** — it does **not** kill an open/running tab for that schedule (deleting a schedule must not nuke a conversation the user is reading). No agent involved.
- **`r` → run-now**.

Deliberately **no inline enable/disable toggle**: it would add per-row edit state and keybindings to a terminal dialog for one field. `enabled` remains a config field that the dialog **displays** but does not edit in place; pausing is done via the agent, `schedule disable <name>` (CLI), or a file edit. Accepted trade-off: pausing is multi-step rather than one keypress. (If pausing proves to be a daily action, a single dedicated keypress that shells out to `schedule disable` — no edit-mode widget — is a cheap later addition.)

**Rename is forbidden via the agent/edit path** (or must be handled explicitly): `name` is the reuse-tab registry key, so renaming would orphan an open reused tab and the next fire would spawn a fresh one. Treat rename as remove + add if ever needed.

### Tab lifecycle: reuse-by-default

Users primarily learn about scheduled task fires through notifications (PRD #126). They open the deck to dig into a result *if* they choose to. Once they do, they read, interact, and close. That access pattern means **reuse** is the dominant default, not new-tab-per-fire:

- **Default**: a scheduled task reuses the same tab/card each fire. Yesterday's weather output is replaced by today's. One weather tab, ever.
- **Opt-in**: `new_tab_per_fire = true` per task, for cases where the user actually wants per-fire history (audit-style scheduled tasks). Documented up-front, not the default.

The reuse registry is keyed by **scheduled task name**, lives **in memory in the daemon**, and is therefore **wiped on daemon restart** — the first post-restart fire creates a fresh tab even under reuse. Documented limitation; tied to the no-persistence fact above.

**Mid-interaction reuse semantics**: if the user is actively typing into the tab when the next fire occurs, the new prompt is queued until idle; if the user is idle, the new prompt is delivered immediately. ("Idle" defined in M2 — working assumption: last-keystroke timestamp + ~5s debounce.)

## Scope

### In Scope

- **Cron primitive (in the daemon)**:
  - Load `[[scheduled_tasks]]` from the global `~/.config/dot-agent-deck/schedules.toml` at `daemon serve` startup.
  - Evaluate cron expressions and fire callbacks on schedule (in-process in the daemon).
  - Skip-if-prior-run-still-active concurrency rule, with a log entry surfaced via notifications.
  - Manual "run now" trigger per task.
  - **Idle-shutdown carve-out**: a registered (enabled) scheduled task counts as "work present" so the daemon does not idle-GC between fires (`clients == 0 && live_count == 0 && no_pending_schedules`).
  - **Reload**: a `ReloadSchedules` control message re-reads the global config without restarting the daemon.
- **Spawn primitive**:
  - `spawn(working_dir, prompt) → handle` that branches on the *target* directory's `.dot-agent-deck.toml`.
  - Auto-create `working_dir` if missing (`mkdir -p`); fail loud (surface via notifications) on creation errors.
  - When the target directory has no `.dot-agent-deck.toml`, default to a single-agent card spawned with the schedule's `command` (mirroring the new-deck dialog; falls back to `$SHELL` if omitted).
  - Return type designed with one eye on #120: stable handle + "tab closed" callback registration.
- **Global config**:
  - `[[scheduled_tasks]]` blocks in `~/.config/dot-agent-deck/schedules.toml` (global, per-user, XDG_CONFIG_HOME).
  - Fields: `name`, `cron`, `working_dir`, `command` (optional; single-agent command, mirrors the dialog, falls back to `$SHELL`), `prompt`, `new_tab_per_fire` (default false), `enabled` (default true).
  - Load errors surfaced via notifications (PRD #126); a malformed entry does not crash the daemon or block other entries.
- **CLI surface**: `dot-agent-deck schedule add|update|remove|list|enable|disable|run-now|reload`. Single validated writer (cron validation, path expansion, atomic write to the global path regardless of cwd, daemon reload).
- **Creation UX**: a "schedule" mode in the new-deck dialog that spawns a seed-prompted agent which calls `schedule add`. Requires adding `seed_prompt: Option<String>` to `ModeConfig` and reusing the orchestration delivery path.
- **Management UX**: a keybinding-opened "Scheduled Tasks" dialog (list + add/edit via agent + delete-with-confirm + run-now), read-only-plus-actions.
- **Tab lifecycle**: reuse-by-default per scheduled task (registry keyed by name, in-memory); `new_tab_per_fire = true` opt-in; mid-interaction deliver-on-idle.
- **Failure surfacing**: mkdir errors, spawn errors, missing role in `[[orchestrations]]`, skip-if-running, config load/parse errors — all flow into PRD #126. No silent swallowing.
- **Documentation** under `site/` covering the global config, the three edit doors, the creation/management UX, the daemon-must-be-running caveat (incl. restart/upgrade/reboot behavior and no catch-up), the reuse model, and a complete example.

### Out of Scope (this PRD)

- **GitHub API calls, issue enumeration, repo cloning/pulling, worktrees.** All belong to #120, which composes the primitives here. The scheduler API must be sufficient for #120 to build on, but no GitHub-aware code lives in this PRD.
- **N-spawn-per-fire as a typed feature.** The primitive is one spawn per call; a callback wanting N spawns loops over `spawn()`. Per-spawn identity, dedup keys, idempotency, batch cleanup hooks — all #120's problem.
- **Catch-up / missed-fire replay.** Cron fires while the daemon is down are not replayed. No persistent queue, no last-fire-timestamp catch-up. (Revisit only if daemon supervision lands and unattended-across-reboot becomes a hard requirement.)
- **Daemon supervision (systemd/launchd).** Tracked as an Open Question / Risk, not implemented here. Without it, unattended scheduling holds only while the daemon happens to be up.
- **Persisting the in-memory tab-reuse registry across daemon restarts.** First post-restart fire creates a fresh tab. Deferred to PRD #74 / #89 tab-restoration work.
- **Inline field editing in the management dialog** (toggle/edit-in-place). Mutation goes through agent/CLI/file.
- **Renaming a schedule** as a first-class edit (orphans the reuse tab). Treat as remove + add.
- **Remote `/schedule` integration.** Claude Code's `/schedule` is a separate cloud-side cron; we are not bridging. (See "Relationship to Claude Code" below.)
- **Per-task per-event notification routing.** Failures and "task completed" go through whatever PRD #126 provides — no per-task channel overrides.
- **Concurrency across distinct scheduled tasks.** Two different tasks firing at once run in parallel; only the same-task-overlap case is serialized.

### Relationship to Claude Code's `/schedule` and `/loop`

For positioning (not a dependency): Claude Code already has cloud-side **`/schedule`** routines (run remotely, survive everything, 1-hour minimum cron) and session-scoped **`/loop`** (runs in the open CLI session, dies when it closes, hard 7-day auto-expiry). This PRD is closest to `/loop` *architecturally* — in-process, local, host-must-be-running — but hosted in the **daemon** rather than a foreground session, so it survives the TUI closing and has **no 7-day cap**. Its distinguishing value over both is the **spawn primitive**: firing a prompt into a specific working directory and routing it to an orchestration role or single-agent card with deck-native tab lifecycle. Neither Claude Code feature knows about deck tabs/orchestrations. We are not bridging to either.

## Success Criteria

- A user can create a `[[scheduled_tasks]]` entry via any of the three doors (agent in the "schedule" mode, `schedule add` CLI, or hand-editing the global file); the daemon loads it at startup and reloads it live on edit without a restart.
- When the cron fires (or the user invokes run-now), the daemon executes end-to-end: working_dir auto-created if missing, target `.dot-agent-deck.toml` consulted, orchestration tab or single-agent card spawned, prompt delivered.
- For a target dir with an `[[orchestrations]]` block → orchestration tab, `orchestrator` role receives the prompt. Without one → single agent card receives the prompt.
- With `new_tab_per_fire = false` (default), the *same* tab is reused each fire (within a daemon lifetime). With `true`, each fire opens a fresh tab.
- The daemon does **not** idle-shut-down while an enabled scheduled task is registered, even with zero clients and zero live agents — so a daily task survives the gap between fires and the gap before its first fire.
- Skip-if-prior-run-still-active: an overrunning callback causes the next tick to be skipped and a notification to fire (PRD #126).
- A `working_dir` creation error does not crash the daemon; it fires a notification and other scheduled tasks keep running.
- The "Scheduled Tasks" dialog lists schedules with status + next-fire, supports add/edit (via the seeded agent), delete-with-confirm (definition only, leaves open tabs), and run-now.
- The cron primitive is exposed internally such that a future PRD #120 can register a callback that, on fire, calls `spawn` N times with different per-issue working dirs and prompts — without modifying the scheduler module.
- Documented, honest behavior on daemon stop/restart/upgrade/reboot: in-flight runs lost, reuse registry reset, no catch-up; definitions reload from the global file when the daemon next starts.
- `cargo fmt --check` and `cargo clippy -- -D warnings` pass. `cargo test` passes including new tests.
- Documentation under `site/` covers the global config, three edit doors, creation/management UX, the daemon-must-be-running caveat, and the reuse model.

## Open Questions (resolve during M1)

1. **Cron crate choice.** `cron` (small, pure-Rust) vs `tokio-cron-scheduler` (deeper async integration, more deps). Working assumption: `cron` for expression parsing plus a thin daemon-side tokio loop we own; revisit if awkward. Decide on `cargo tree` weight.
2. **Timezone for cron evaluation.** `0 9 * * MON-FRI` — in whose zone? Options: always local, always UTC, or a per-schedule `timezone` field. Working assumption: **local time**, documented; add a `timezone` field only if demand appears. Decide in M1.
3. **Single-agent command.** *Resolved:* each schedule carries a per-schedule **`command`** field (e.g. `claude`), mirroring the new-deck dialog's command field — no deck-wide "default agent" concept. Used for the single-agent card; ignored when the target dir defines an `[[orchestrations]]` block (the orchestration's role commands win). Falls back to `$SHELL` when omitted, so the creation agent/CLI prompts for it. Remaining detail for M2.1: confirm the fallback/empty-command behavior matches the dialog exactly.
4. **Daemon supervision for true unattended scheduling.** The daemon is lazy-spawned by the TUI and not respawned after stop/crash/reboot. Should this PRD ship an *optional* systemd/launchd unit (or document how to add one) so "fires at 09:00 unattended" is actually true? Working assumption: document the limitation in v1; offer an optional supervision recipe in docs; treat a built-in `daemon install`/service as a follow-up. Decide scope in M1.
5. **Reload mechanism.** `ReloadSchedules` control message over the daemon socket (preferred, consistent with existing protocol) vs file-watch vs reload-only-on-`daemon serve`. Working assumption: control message + reload-on-startup; file-watch optional.
6. **Mid-interaction "idle" definition for reuse.** (a) no PTY input for N seconds; (b) tab not focused; (c) explicit agent "ready" state. Working assumption: (a), ~5s debounce. M2 confirms.
7. **Working directory path expansion.** Working assumption: `~` and `$VAR` expanded by the CLI at write/load time; relative paths resolved against the user's home or an explicit base (not the authoring agent's cwd, which is irrelevant). Confirm in M1.
8. **Spawn handle reuse identity.** Reuse registry keyed by scheduled task name; M2 confirms the registry shape and its in-memory lifecycle.

## Milestones

### Phase 1: Cron primitive + global config + daemon lifecycle

- [ ] **M1.1** — Pick the cron crate (`cargo tree` weight). Implement cron-expression evaluation and a daemon-side tokio loop firing registered callbacks. Decide timezone (Q2).
- [ ] **M1.2** — Add the global `~/.config/dot-agent-deck/schedules.toml` schema (`name`, `cron`, `working_dir`, `prompt`, `new_tab_per_fire`, `enabled`). Load at `daemon serve` startup; surface load/parse errors via notifications (PRD #126); a bad entry doesn't block others.
- [ ] **M1.3** — Skip-if-prior-run-still-active. `ReloadSchedules` control message + reload-on-startup.
- [ ] **M1.4** — **Idle-shutdown carve-out**: extend the idle gate so a registered enabled scheduled task keeps the daemon alive (`clients == 0 && live_count == 0 && no_pending_schedules`). Test the before-first-fire and after-agent-exit gaps.
- [ ] **M1.5** — CLI: `schedule add|update|remove|list|enable|disable|run-now|reload` — single validated writer (cron validation, path expansion, atomic global-path write regardless of cwd, triggers reload).

### Phase 2: Spawn primitive + lifecycle

- [ ] **M2.1** — `spawn(working_dir, prompt) → handle`. Auto-create working_dir; fail-loud. Branch on target `.dot-agent-deck.toml` (orchestration tab vs single-agent card). Single-agent card uses the schedule's `command` field, mirroring the dialog (Q3 resolved). Isolate the config lookup behind `load_config_for_dir(path) → Config`.
- [ ] **M2.2** — Tab lifecycle: reuse-by-default. Named-tab registry (in-memory, keyed by task name) + mid-interaction deliver-on-idle. Document the wipe-on-restart behavior.
- [ ] **M2.3** — Wire the cron callback for the static-prompt case: a fire calls `spawn(working_dir, prompt)` exactly once with the configured values.

### Phase 3: Creation + management UX

- [ ] **M3.1** — Add `seed_prompt: Option<String>` to `ModeConfig`; thread through `NewPaneRequest`; deliver to the spawned agent via the orchestration delivery path (agent-ready gate + buffer delay).
- [ ] **M3.2** — Add the "schedule" mode to the new-deck dialog with its authoring seed prompt (field list, `schedule add` invocation, validation rules, confirm-before-write). Subtle visual separation marking it as an authoring session.
- [ ] **M3.3** — "Scheduled Tasks" management dialog behind a keybinding: list (status + next-fire), add/edit (spawn seeded agent), delete-with-confirm (definition only), run-now. Forbid rename via the edit path. TUI tests (L1; L2 where the spawned binary/daemon is involved).

### Phase 4: Tests + integration with notifications

- [ ] **M4.1** — Unit tests: cron evaluation, skip-if-running, idle carve-out, working_dir create success/failure, spawn branching, tab reuse vs new_tab_per_fire, CLI validation, config load with a malformed entry.
- [ ] **M4.2** — Integration: fixture global config with a fast-cron task; assert spawn fires and prompt is delivered. Trigger a working_dir creation failure; assert a notification fires (PRD #126) and other tasks keep running. Assert the daemon stays up with a registered schedule and zero clients/agents.
- [ ] **M4.3** — Manual validation: a real task firing every ~2 min — spawn happens, prompt delivered, notification arrives. Toggle `new_tab_per_fire`. Trigger a typo'd working_dir on a nonexistent mount; verify notification + no crash. Stop/restart the daemon mid-day; verify definitions reload on next start and behavior matches the documented caveat.

### Phase 5: Docs and ship

- [ ] **M5.1** — Documentation under `site/`: global `schedules.toml` reference; the three edit doors; the "schedule" mode + management dialog; the daemon-must-be-running caveat (restart/upgrade/reboot, no catch-up); reuse model; mid-interaction queue; an optional daemon-supervision recipe (Q4); a complete example with a scheduled task plus orchestrations.
- [ ] **M5.2** — Changelog fragment via `dot-ai-changelog-fragment`. Frame as "you can now schedule prompts to run on a cron and land in the deck."
- [ ] **M5.3** — `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` green (incl. `cargo test-e2e` pre-PR). PR, review, audit, merge.
- [ ] **M5.4** — Open the follow-up PRD #120 update: revise its Phase 1 scope to consume this scheduler's primitives instead of duplicating them.

## Key Files

- `src/scheduler.rs` (new) — daemon-side cron loop, registered tasks, run-now, skip-if-running, reload.
- `src/spawn.rs` (new) — `spawn(working_dir, prompt) → handle`, branching on target `.dot-agent-deck.toml`, mkdir+fail-loud, `load_config_for_dir` helper.
- `src/daemon.rs` — idle-shutdown carve-out (third keep-alive condition); `ReloadSchedules` handling; scheduler startup wiring.
- `src/config.rs` — global `schedules.toml` schema + loader (XDG_CONFIG_HOME), alongside the existing `SavedSession`.
- `src/project_config.rs` — add `seed_prompt: Option<String>` to `ModeConfig`.
- `src/ui.rs` — "schedule" mode in the new-deck dialog; "Scheduled Tasks" management dialog; seed-prompt delivery for modes (reuse orchestration path).
- `src/cli.rs` (or equivalent) — `dot-agent-deck schedule add|update|remove|list|enable|disable|run-now|reload`.
- `tests/scheduler.rs` (new) — unit + integration tests.
- `site/content/docs/scheduled-tasks.md` (new) — user-facing documentation.

## Risks and Mitigations

- **Risk**: The daemon idle-GCs itself between fires, silently killing the scheduler.
  - *Mitigation*: M1.4 idle-shutdown carve-out — a registered enabled schedule is a keep-alive condition. Explicit tests for the before-first-fire and after-agent-exit gaps.

- **Risk**: A daemon stop/upgrade/reboot drops fires while it's down, with no catch-up, so an "every 09:00" task silently misses days.
  - *Mitigation*: v1 documents this honestly (deck-must-be-running, no catch-up). For genuinely unattended scheduling, Q4 evaluates an optional systemd/launchd supervision recipe so the daemon is always-on and self-restarts. A fixed global config path means lazy-spawn and supervised modes read the same file with no migration.

- **Risk**: An agent freehand-editing TOML produces a malformed cron or unescaped multi-line prompt that fails silently at the next fire.
  - *Mitigation*: the agent never writes TOML directly — it calls the validated `schedule add` CLI. The daemon also validates on load/reload and surfaces errors via #126.

- **Risk**: A runaway cron (every minute) floods the deck.
  - *Mitigation*: every fire emits a notification (#126), so a runaway is immediately visible; reuse-by-default means high-frequency fires don't accumulate tabs.

- **Risk**: Renaming a schedule orphans its reused tab (name is the registry key).
  - *Mitigation*: rename is forbidden via the edit path; treat as remove + add. Documented.

- **Risk**: Mid-interaction deliver-on-idle races user typing.
  - *Mitigation*: simple last-keystroke + ~5s debounce; documented; refine in a follow-up if wrong in practice.

- **Risk**: The single-agent fallback spawns a bare `$SHELL` and the prompt goes nowhere useful.
  - *Mitigation*: each schedule carries a `command` field (mirrors the dialog); the creation agent/CLI prompts for it. `$SHELL` fallback is documented, consistent with the dialog.

- **Risk**: Notifications dependency (#126) slips.
  - *Mitigation*: ship behind a stub that logs to stderr for development; #126 is a hard dependency only for shipping.

- **Risk**: PRD #120 finds the spawn signature insufficient (more metadata, on-close hook for worktree cleanup).
  - *Mitigation*: M2.1 designs the spawn return type with #120 in mind (stable handle + tab-closed callback). #120 should require *additions*, not breaking changes.

- **Risk**: The spawn primitive couples too tightly to config-loading internals.
  - *Mitigation*: M2.1 isolates the lookup behind `load_config_for_dir(path) → Config`; no reaching into config internals from the scheduler.

## Dependencies

- **PRD #126** (agent-driven notifications) — surfaces scheduler failures (load/parse errors, mkdir errors, spawn errors, skip-if-running). Hard dependency for shipping; soft for development (stub if needed).
- Existing daemon, attach/hydration, and idle-monitor code (`src/daemon.rs`, `src/agent_pty.rs`) — extended with the scheduler and the idle carve-out.
- Existing config-loading code — extended with the global `schedules.toml` (and `seed_prompt` on `ModeConfig`).
- Existing new-deck dialog and the orchestration seed-prompt delivery path (`src/ui.rs`) — reused for the "schedule" mode and management dialog.
- Existing tab/orchestration spawn code paths — M2.1 calls into them rather than reimplementing.
- A maintained cron crate (`cron` or equivalent) — decided in M1.1.
- No external services, no new third-party credentials.

## Validation Strategy

- **Unit**: cron evaluation, skip-if-running, idle carve-out, working_dir mkdir success/failure, spawn branching, tab reuse registry, new_tab_per_fire, mid-interaction deliver-on-idle, CLI validation, config load with malformed entry.
- **Integration**: fast-cron fixture against a global config → real spawn into a fixture `.dot-agent-deck.toml`, assert tab created with the right contents and prompt delivered. Failure injection: read-only working_dir → notification fires (#126), other tasks continue. Daemon-stays-up assertion with a registered schedule and zero clients/agents.
- **TUI** (per CLAUDE.md rule 4): L1 for the management dialog rendering and the new "schedule" mode option; L2 (`e2e_*`) where the spawned binary/daemon, seed-prompt delivery, or real agent are involved.
- **Manual** (per `feedback_validate_pre_pr`): real task every ~2 min — spawn, prompt delivery, notification. Toggle `new_tab_per_fire`. Typo'd working_dir → notification + no crash. Daemon stop/restart mid-day → definitions reload on next start; behavior matches documented caveat.
- **Regression**: existing modes, orchestrations, dashboard, tab-lifecycle, idle-shutdown, and config-loading tests continue to pass. The scheduler is additive — it should not change the shape of any existing test (except the idle gate, which gains the third keep-alive condition).

## CLAUDE.md Compliance

- `cargo fmt --check` and `cargo clippy -- -D warnings` before every commit (rule #2).
- No `m*_*` / `prd*_*` prefixes in source/test filenames (rule #3): `src/scheduler.rs`, `src/spawn.rs`, `tests/scheduler.rs`.
- Add/update TUI tests for the new dialog and "schedule" mode (rule #4); `cargo test-fast` per task, `cargo test-e2e` pre-PR (rule #5).
- Ask before creating branches or worktrees (rule #1). `/prd-start` will prompt accordingly.
- `#[spec]` tests carry a `/// Scenario:` comment (rule #7).

## Design Decisions log (2026-06-07)

Captured from the design discussion that produced this revision:

1. **Scheduler lives in the daemon, not the TUI** — so fires continue after the TUI detaches. (Was ambiguously "in-process inside the deck.")
2. **Idle-shutdown carve-out is required** — a registered enabled schedule must keep the daemon alive (`clients == 0 && live_count == 0 && no_pending_schedules`), or the daemon GCs itself between fires. Hinges on `live_count()` (live PTYs) vs `len()` (registry entries).
3. **Interactive agents, not `claude -p`** — prompts are injected into a live PTY; agents linger, which usually keeps the daemon up. The carve-out covers the no-live-agent windows.
4. **No persistence across daemon restart** — stop/upgrade/reboot kills agents and wipes the reuse registry; definitions reload from the global file; **no catch-up** for fires due while down. True unattended operation needs daemon supervision (Open Q4).
5. **Global, daemon-owned config** at `~/.config/dot-agent-deck/schedules.toml` (XDG_CONFIG_HOME) — *not* per-project. Per-project `.dot-agent-deck.toml` keeps its role only as the **spawn target's** orchestration shape.
6. **Three edit doors, one validated writer** — agent→CLI (primary UI), CLI directly, hand-edit file. The agent never freehand-edits TOML; the CLI validates and writes to the global path regardless of cwd.
7. **Creation via an agent-seeded "schedule" mode** in the existing new-deck dialog — reuses the orchestration seed-prompt-on-spawn pattern; lets the user craft (and test) the prompt conversationally; needs `seed_prompt` on `ModeConfig`.
8. **Management via a read-only-plus-actions "Scheduled Tasks" dialog** — list + add/edit (agent) + delete-with-confirm (definition only) + run-now. **No inline toggle/field-edit** (keeps the terminal dialog simple); pausing is via agent/CLI/file. **Rename forbidden** via the edit path (reuse-key orphaning).
9. **Per-schedule `command` field** — each schedule specifies its single-agent command, mirroring the new-deck dialog, instead of a deck-wide "default agent." Consistent UX; no hidden global; works for users without a default agent. Ignored for the orchestration branch; `$SHELL` fallback when omitted.
