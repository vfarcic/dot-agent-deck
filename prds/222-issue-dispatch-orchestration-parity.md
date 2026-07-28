# PRD #222: Scheduled issue-dispatch parity with the interactive orchestration-open path

**Status**: Not started
**Priority**: High
**Created**: 2026-07-25
**GitHub Issue**: [#222](https://github.com/vfarcic/dot-agent-deck/issues/222)
**Feature flag**: `experimental` (PRD #139) — this feature stays behind the flag through this PRD; graduation is tracked separately by [#193](https://github.com/vfarcic/dot-agent-deck/issues/193).

## Problem Statement

Scheduled issue-dispatch (PRD #120, built on the #127 scheduler) does not open orchestrations "the same way the user would from the new-deck dialog." It goes through a **separate, daemon-side reimplementation** — `crate::spawn::spawn` (`src/spawn.rs`) — instead of the interactive path `TabManager::open_orchestration_tab` (`src/tab.rs`). The two paths have drifted, and the drift produces user-visible defects that were found by dispatching the repo's own dogfood `dot-agent-deck` orchestration (6 mixed-agent roles) onto three open issues:

- **Only auto-approving roles come up.** Of the six roles (orchestrator + auditor = `pi … --approve`, coder + release = `claude`, reviewer = `opencode`, tester = `codex`), only the two Pi roles became live agents. The `claude`/`opencode`/`codex` roles block on their first-run trust / permission / onboarding prompts in the freshly-cloned worktree, and there is no one to answer "yes" under an unattended scheduler fire. The manual flow works only because a human is present to press "yes." So a mixed orchestration silently degrades to "just the `--approve` agents."
- **Role names are lost.** Dispatched orchestration panes are titled with the schedule task name / pane id (`sched-<task>`) instead of their role (`orchestrator`, `coder`, `auditor`, …). The live surface path carries `role_name`, but after a detach/reconnect the tab is rebuilt from the daemon registry where each pane's `display_name` was stamped with the task name — so the role identity is not reconstructed.
- **Generic-instructions ordering is unclear.** The interactive path threads each role's `prompt_template` and injects the `orchestrator_prompt` into the start role once ready (with Pi-native-seed handling). The scheduler's `RoleSpawn` does not carry `prompt_template`, so it is not established that the orchestrator receives its generic role instructions before the schedule prompt is delivered.

Separately, the guided `schedule: issues` authoring flow has two UX gaps unrelated to the divergence:

- **Hardcoded example repo.** The authoring agent suggests `vfarcic/dot-ai` — a literal example string in the seed prompt (`ISSUE_DISPATCH_AUTHORING_SEED_PROMPT`, `src/ui.rs`) — even when launched from inside a different repo. It does not infer the current repo.
- **No ordering choice.** There is no first-class way to choose which issues get picked. Ordering is whatever `gh issue list` returns (newest-first), reachable only by hand-writing a raw `--search` query into the optional `query` field, which the authoring agent does not surface.

## Solution Overview

Converge scheduled issue-dispatch onto the **same canonical orchestration-open routine** the interactive new-deck dialog uses, so role naming, role-prompt ordering, and startup handling are shared instead of duplicated — and make dispatched agents come up **unattended**. Layer two additive authoring-UX improvements on top: a smart default repo inferred from the current repo, and a first-class issue `sort` field with today's order as the default.

The whole feature stays behind the `experimental` flag through this PRD. After release we re-run the real dispatch test (as done during discovery) and, only if it passes, graduate the feature (remove the flag) via the existing follow-up #193.

### Architecture

Today there are two orchestration-open implementations:

- **Interactive:** `TabManager::open_orchestration_tab(config, cwd, orchestrator_prompt, display_title, dims)` — builds role panes with correct identity/titles, applies each role's `prompt_template`, injects `orchestrator_prompt` into the start role once its agent is ready, and special-cases Pi's native-seed delivery vs. PTY injection.
- **Scheduler:** `crate::spawn::spawn` with `SpawnTarget::Orchestration` — loops all roles, stamps `display_name = task_name`, delivers to the orchestrator via its own readiness gate, surfaces the tab via `OrchestrationSurface`, and does not thread `prompt_template`.

The target architecture is **one** daemon-side canonical open-orchestration routine that both the interactive dialog and the scheduler call. The divergence is largely historical: `spawn.rs` predates the always-external daemon (PRD #93), when `open_orchestration_tab` was TUI-only and could not be driven daemon-side. That constraint is gone.

## Scope

### In Scope

- Unify scheduler/dispatch orchestration spawning onto the interactive `open_orchestration_tab` path (or a shared daemon-side routine both call), eliminating the divergent orchestration branch in `spawn.rs`.
- Make dispatched agents come up **unattended**: seed per-agent trust/onboarding and/or pass per-agent no-prompt flags so non-`--approve` roles (claude/opencode/codex) start without a human answering a prompt. Mirror the test-harness pattern (`prepare_claude_home` + per-folder trust + `--allowedTools`) into the production dispatch path.
- Preserve **role names** on dispatched orchestration panes across live surface AND detach/reconnect rebuild.
- Guarantee the orchestrator receives its **generic role instructions (`prompt_template`) before** the schedule prompt.
- Authoring UX: infer the **current repo** as the default `repo` (via `gh repo view --json nameWithOwner`), falling back to asking; keep it a confirmable default.
- Authoring UX: add a first-class **`sort`** field (structured, discoverable, default = current order); have the authoring agent ask about ordering; define `sort` vs. raw `query` precedence.
- A **real-agent e2e** test (CLAUDE.md rule 4) that dispatches a mixed orchestration on cheap models and asserts all roles come up, named by role, unattended — the exact class of failure a headless test misses.
- Keep the feature behind the `experimental` flag; document the post-release re-test and the graduation gate via #193.

### Out of Scope

- **Selection depth / starvation (ceiling-vs-top-up).** The cap applies to issues *considered*, not *newly dispatched*, so a busy backlog does not "top up" to N new agents. This is a real limitation but a separate concern; it is named here and deferred to a follow-up decision.
- **Non-GitHub forges.** Dispatch stays GitHub-only (built on `gh`).
- **Graduation itself.** Removing the `experimental` flag is #193, gated on this PRD landing and the post-release re-test passing.
- Changes to the single-agent (non-orchestration) dispatch path beyond what unification incidentally touches.

## Technical Approach

### Unify the open path
Replace the `SpawnTarget::Orchestration` branch in `src/spawn.rs` with a call into the canonical routine used by `open_orchestration_tab`. The scheduler's job shrinks to: fetch issues → provision clone/worktree → call the canonical open-orchestration routine with the schedule prompt as `orchestrator_prompt`. Role naming, `prompt_template` application, ready-gated injection, and Pi-native-seed handling then come for free and cannot drift again. Requires making the routine callable daemon-side (no attached-TUI assumption) and surfacing the resulting tab to any attached TUI via the existing surface/rebuild mechanism.

### Unattended startup
Determine, per agent type, what blocks an unattended start in a fresh worktree (claude: folder-trust + tool permissions; opencode: prompt/auth; codex: onboarding/auth) and seed/flag around it. Reuse the harness approach where possible. This is presentation-independent (it is startup correctness), so it is **not** gated on the flag.

### Role-name preservation
Ensure the daemon registry entry for each dispatched role pane carries the role identity (not just `task_name`), so the reconnect/rebuild path titles panes by role. This aligns the rebuild path with the live `OrchestrationSurface` path.

### Authoring UX (repo default + sort)
In the `schedule: issues` authoring seed (`src/ui.rs`): instruct the agent to detect the current repo and offer it as the default `repo`; add a `sort` field that compiles to `gh`'s `--search "sort:<field>-<dir>"`, with `sort` ignored when a raw `query` is provided (query = full-control override). "sort unset ⇒ byte-for-byte today's path/order" is an explicit, tested success criterion.

### Cross-version contract (CLAUDE.md rule 12)
This touches orchestration and the daemon spawn path. Before PR: answer "did the TUI↔daemon contract change?" If the wire shape moved, bump `PROTOCOL_VERSION`; if a same-wire meaning changed, add a `changelog.d/<issue>.breaking.md` fragment. Run the cross-version manual test (branch TUI against previous-release daemon: delegate routes, hooks arrive).

## Success Criteria

- Dispatching the dogfood `dot-agent-deck` orchestration (6 mixed roles) onto real issues brings up **all** roles as live agents, unattended, with **no human answering a prompt**.
- Each dispatched orchestration pane is titled by its **role name** both live and after a detach/reconnect.
- The orchestrator provably receives its `prompt_template` generic instructions **before** the schedule prompt; the schedule prompt is delivered **only** to the orchestrator role.
- The scheduler no longer contains a second orchestration-open implementation — both entry points share one routine.
- The `schedule: issues` authoring flow offers the **current repo** as the default and asks about **sort**; with `sort` unset, issue selection is byte-for-byte identical to today.
- A real-agent e2e test exercises the above on cheap models and is green in the pre-PR e2e tier (not CI).
- The `experimental` flag still gates the authoring UI; docs/changelog reflect the still-experimental status and the #193 graduation gate.

## Milestones

- [ ] **M1 — Unified open path.** Scheduler orchestration dispatch calls the canonical `open_orchestration_tab` routine; the divergent `spawn.rs` orchestration branch is removed. Role names correct live; prompt delivered only to the orchestrator, after `prompt_template`.
- [ ] **M2 — Unattended startup.** Dispatched non-`--approve` agents (claude/opencode/codex) come up without a human answering a prompt, via seeded trust/onboarding + no-prompt flags.
- [ ] **M3 — Role names survive reconnect.** Detach/reconnect rebuilds dispatched orchestration tabs with role-named panes.
- [ ] **M4 — Authoring UX: repo default + sort.** Current-repo default (confirmable) and a first-class `sort` field (current-order default, `query`-wins precedence); authoring agent asks about ordering.
- [ ] **M5 — Tests.** Real-agent mixed-orchestration dispatch e2e (cheap models, sentinel + role-pane assertions, pre-PR tier) plus L1/protocol coverage for naming and sort default-parity.
- [ ] **M6 — Docs + cross-version contract.** Update `docs/scheduled-tasks.md` (repo default, sort, unattended behavior); changelog fragment; run the rule-12 cross-version check and classify per versioning policy.
- [ ] **M7 — Post-release re-test + graduation handoff.** After release, re-run the real dispatch test; on success, hand off to #193 to remove the flag (out of scope to remove it here).

## Key Files

- `src/spawn.rs` — the divergent scheduler orchestration branch to unify away.
- `src/tab.rs` — `open_orchestration_tab` (the canonical interactive routine to converge on).
- `src/issue_dispatch_run.rs`, `src/issue_dispatch.rs` — dispatch fire flow, issue enumeration/selection.
- `src/ui.rs` — `ISSUE_DISPATCH_AUTHORING_SEED_PROMPT` (repo default + sort authoring).
- `src/daemon.rs` — scheduler fire callback (`spawn_or_reuse`).
- `src/features.rs` — `show_issue_dispatch_authoring()` flag seam (unchanged; stays gated).
- `docs/scheduled-tasks.md` — user docs.
- `tests/` — real-agent e2e (`e2e_*.rs`, gated `e2e`) + L1/protocol tests.

## Risks

- **Making the interactive routine daemon-callable** may surface hidden TUI-attachment assumptions in `open_orchestration_tab`. Mitigation: drive it via the existing daemon-side surface/rebuild seam; add a headless test first.
- **Unattended startup seeding** touches per-agent trust/permission state (security-adjacent). Mitigation: mirror the vetted harness pattern; scope no-prompt to the dispatched worktree; do not weaken interactive-session prompts.
- **Cross-version contract (rule 12).** Unifying the spawn path could shift the TUI↔daemon orchestration surface. Mitigation: run the cross-version manual test and bump/version per policy before PR.
- **Sort parity.** Routing "default" through `--search` could subtly change ordering. Mitigation: keep the no-`--search` path when `sort` is unset; assert default-parity in a test.

## Open Questions

- Does the scheduler path currently inject each role's `prompt_template` at all, or are generic instructions dropped today? (Confirm during M1; determines whether M1 is a fix or a regression-guard.)
- Should the `experimental` flag gate more than the authoring UI during the "prove it" window, or is a soft-launch (CLI/config runs; guided door hidden) acceptable? (Decision for M1/M6.)

## Rollout & Graduation

Ship behind `experimental`. After release, re-run the real dispatch test (mixed orchestration on real issues; verify all roles up, unattended, role-named, prompt after instructions). Only on success, proceed to **#193** to remove the flag, inline the `true` branches, and drop the flag's docs/changelog notes. This PRD does **not** remove the flag.
