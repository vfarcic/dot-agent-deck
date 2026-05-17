# PRD #92: Process-boundary invariant audit sweep

**Status**: Planning
**Priority**: Medium
**Created**: 2026-05-17
**GitHub Issue**: [#92](https://github.com/vfarcic/dot-agent-deck/issues/92)
**Depends on**: PRD #76 M2.19 and M2.20 shipped first.

## Problem Statement

The PRD #76 architectural pivot turned the daemon from an in-process subsystem into a separate process. The TUI now talks to it over a Unix socket using the attach protocol. Every place in the codebase that previously relied on shared-memory semantics — direct pane references, immediate `AppState` consistency, TUI-generated identifiers that happened to also be the daemon's identifiers — became a potential bug at that boundary.

The pattern has been visible for months but was caught reactively. Each milestone since M2.11 maps to one such site:

- **M2.11**: pane ids needed to be daemon-stable across reconnect; the TUI was generating fresh ids on hydration → `pane_id_env`, `display_name`, `cwd` added to the daemon registry.
- **M2.12**: tab membership was reconstructed from the wrong source on reconnect → tab restoration milestone.
- **M2.13**: `agent_type` was not in `AgentRecord` so post-reconnect placeholder rendering misclassified live agents → field added.
- **M2.17**: hook events fired in the daemon's process never reached the attached TUI → `tokio::sync::broadcast` fanout + `KIND_EVENT` frames.
- **M2.19** *(in flight)*: `DOT_AGENT_DECK_PANE_ID` env var on the orchestrator goes stale across reconnect, so `state.handle_delegate` silently drops the signal.
- **M2.20** *(reported)*: intermittent need to press Enter after sending a prompt — likely another timing-or-identity boundary issue.

Each of these was discovered the same way: a user tested on a real remote VM, a bug appeared, and we tracked it back to a shared-memory assumption that broke under IPC. **There is no reason to believe we have caught all of them.** The remaining ones are sitting in the codebase, waiting for a user flow to hit them.

## Solution Overview

Run an exploratory survey across the codebase looking for the structural patterns that produce this class of bug. The deliverable is a triaged checklist of suspect call sites, not a code change. Findings get scoped as follow-up milestones under PRD #76 (or a successor PRD) and fixed per-finding.

The audit is *exploratory and pattern-driven*. It is not a diff review and not a bug fix. The goal is coverage — to enumerate the surface area so the project can stop reacting one user-reported bug at a time.

## Scope

### In Scope

- **Code patterns to grep for and inspect**:
  - Sites that read or mutate `AppState` and assume the read reflects post-IPC state.
  - Sites holding `&Pane` or `&Session` references across `.await` points.
  - Identifier flows where the TUI generates an id that is then expected to match a daemon-side id (the M2.11 pattern).
  - Event sources whose listeners assume in-process delivery (the M2.17 pattern).
  - Env-var or per-process state that the orchestrator/agent reads once at spawn time but expects to remain valid across TUI reconnect (the M2.19 pattern).
  - `unwrap()` / `expect()` on lookups that worked locally because state was always populated but can be empty on a fresh reconnect.
  - Any `PaneBackend::Pty` vs `PaneBackend::Stream` divergence where the `Stream` arm is incomplete relative to the `Pty` arm.
- **Triage each finding** into:
  - **Likely broken**: there is a plausible user flow that would hit this. File a milestone.
  - **Theoretically broken**: the pattern matches but no obvious user flow reaches it. Note for later.
  - **Safe on inspection**: pattern matches superficially but the code is correct (e.g. the value is reconstructed elsewhere). Note why so the audit does not flag it again.
- **Use M2.19 and M2.20 root causes as inputs**. The audit should explicitly check whether the same pattern that produced those bugs appears elsewhere.
- **Output a single audit document** at `audit/process-boundary-invariants.md` (new file). Checklist format, one row per finding.

### Out of Scope

- Fixing any of the findings. Fixes are scoped per-finding as follow-up milestones, not in this PRD.
- Refactoring `PaneBackend` or `ControllerMode` to remove the local/remote divergence — that is PRD #93's territory, and doing it before the audit would erase the very patterns the audit is looking for.
- Performance audit, security audit, or any other axis. Process-boundary invariants only.
- Audits of code paths that already have integration test coverage on a real daemon (those are validated). Focus on code that only ran under in-process semantics.

## Success Criteria

- `audit/process-boundary-invariants.md` exists and lists every site the audit flagged, with a triage column.
- At least three "likely broken" findings filed as PRD milestones (a lower count suggests the audit was too shallow; if the codebase really is clean, document the patterns checked and why each was safe).
- The audit document includes a "patterns checked" section so a future re-audit can extend it rather than redo it.
- A retrospective on M2.19 and M2.20: did the audit's pattern list catch them? If not, the patterns get extended before the audit closes.

## Milestones

### Phase 1: Setup

- [ ] **M1.1** — Read M2.19 and M2.20 post-mortems (commit messages + PRD entries). Extract the structural pattern of each.
- [ ] **M1.2** — Draft the initial "patterns to check" list, seeded with the M2.11/M2.12/M2.13/M2.17/M2.19/M2.20 patterns.

### Phase 2: Survey

- [ ] **M2.1** — Grep + read pass across `src/` looking for each pattern. Spawn `Explore` agents per pattern if the scope is wide.
- [ ] **M2.2** — For each hit, read enough surrounding code to triage. Record finding + triage + rationale in `audit/process-boundary-invariants.md`.
- [ ] **M2.3** — Cross-check against `tests/` to see which findings already have integration coverage on a real daemon (those auto-downgrade to "safe").

### Phase 3: Output and follow-up

- [ ] **M3.1** — Finalize `audit/process-boundary-invariants.md` with a summary table and a "patterns checked" appendix.
- [ ] **M3.2** — File each "likely broken" finding as a milestone under PRD #76 (or a successor PRD if #76 has already closed).
- [ ] **M3.3** — Brief writeup in the PR description summarizing coverage and counts per triage bucket.

## Key Files

- `audit/process-boundary-invariants.md` — new file, the audit deliverable.
- `src/state.rs`, `src/embedded_pane.rs`, `src/ui.rs`, `src/daemon.rs`, `src/main.rs`, `src/hook.rs` — primary read targets.
- `prds/76-remote-agent-environments.md` — where most likely-broken findings end up as milestones.

## Design Decisions

### 2026-05-17: Audit, not refactor

The audit explicitly does not fix anything. Two reasons. First, mixing audit and fix work obscures the audit's scope — readers cannot tell whether a clean area was checked or simply not visited. Second, the right fix for some findings may be the unification work in PRD #93 (remove local-vs-remote divergence entirely), which is a bigger architectural decision than the audit should pre-commit to.

### 2026-05-17: Run after M2.19 + M2.20

Both milestones expose patterns the audit needs to look for. Running the audit before they ship risks producing a pattern list that misses their root-cause shape. The cost of waiting is one PRD-#76 cycle; the cost of an incomplete pattern list is that the audit gives false confidence.

### 2026-05-17: Explore agent over reviewer agent

The reviewer agent is wired for diff review against a specific change. This audit has no diff — it is a structural survey of existing code. `Explore` is the right tool: it can read broadly, follow references, and produce a structured report. The reviewer would be miscast.
