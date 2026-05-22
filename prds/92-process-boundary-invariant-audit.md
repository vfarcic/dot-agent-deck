# PRD #92: Pre-daemon parity audit

**Status**: Planning
**Priority**: Medium
**Created**: 2026-05-17
**Last updated**: 2026-05-22
**GitHub Issue**: [#92](https://github.com/vfarcic/dot-agent-deck/issues/92)
**Depends on**: PRD #76 (shipped, `prds/done/76-remote-agent-environments.md`) and PRD #93 Phases 1–3 (shipped — commits `48b9180`, `3d2b2db`). Baseline for the audit is commit `2fc39c3` — the last commit before PRD #76 merged.

## Problem Statement

The deck went through two back-to-back architectural pivots:

- **PRD #76** introduced the daemon as a separate process and remote environments as a first-class concept. The in-process arm stayed for local use, with the daemon-attach arm added alongside it for remote.
- **PRD #93** deleted the in-process arm. The daemon is now always external, lazy-spawned per user; every `dot-agent-deck` invocation attaches to it.

Both pivots were tested by re-implementing the architecture — confirming the new path works, fixing bugs that surfaced under it, then moving on. Neither pivot was tested by enumerating the pre-pivot user-visible features and confirming each one survived intact. Bugs that did surface (PRD #76 M2.11–M2.20; PRD #93 round 5+ in its implementation notes) were caught reactively — a user hit the regression on a real session and reported it.

There is no reason to believe every regression has been caught. The features that were used during the pivots got covered; features the maintainers happen not to use day-to-day could be silently broken or silently changed. The first known example, carried over from earlier audit attempts, is the **force-shutdown gap**: at the baseline you quit the deck and the agents died with it; in current main the daemon persists across deck exits and there is no in-product command to stop it (`pkill` is the only option). PRD #93 line 39 explicitly anticipated needing one but never shipped the command. Neither `DaemonCmd::Stop` nor `RemoteCmd::Stop` exists in current code; `remote remove` only deregisters a local entry.

The audit's job is to enumerate the rest of these. Baseline-versus-current. Each user-visible feature that existed at `2fc39c3` — is it still there, is it still doing the same thing, or did it quietly change shape? Anything missing, different, or silently regressed gets flagged. Anything that changed deliberately gets documented so future re-audits do not re-litigate it.

## Solution Overview

A parity audit between the pre-daemon baseline (`2fc39c3`) and current main. Read baseline code, docs, and tests at that commit. Enumerate user-visible features and behaviors. For each, locate the current implementation and compare against the baseline. Triage into one of three buckets:

- **Preserved** — feature works identically in current code. Evidence required: current code path plus at least one test that exercises it.
- **Regressed** — feature is missing, incomplete, or behaves differently than baseline. Drafted as a follow-up milestone in the audit document; not filed as a GitHub issue until the user reviews and authorizes.
- **Intentional change** — feature changed, but the change was a deliberate design decision. Cite the PRD or commit that justifies it so a future re-audit does not re-flag it.

Output goes to `audit/pre-daemon-parity-audit.md` (new file).

The audit is *baseline-versus-current parity*, not a forward-looking review of current code. Current-code-only issues — bugs that have no baseline equivalent — are out of scope.

## Scope

### In Scope

- **Every user-visible feature or behavior that shipped at `2fc39c3`**. Read baseline `src/`, `tests/`, `docs/`, and any closed PRDs in `prds/done/` whose work landed before `2fc39c3`. Build the feature list from baseline, not from current main.
- **For each feature, locate the current implementation** and compare against baseline:
  - Same UX (commands, flags, output shapes, dialog prompts)?
  - Same lifecycle (when it starts, when it stops, what survives a restart)?
  - Same edge-case handling (failure modes, error messages, validation)?
- **Triage** every row into Preserved / Regressed / Intentional change.
- **Worked example — force-shutdown gap**: pre-daemon, quitting the deck killed every agent. Post-daemon the daemon persists across exits and there is no in-product stop command. PRD #93 line 39 anticipated one (`"dot-agent-deck remote stop (or equivalent local command) to force shutdown"`); neither form shipped. The audit must include this as a Regressed row anchored to PRD #93 line 39.
- **Historical anchors**: M2.11 / M2.12 / M2.13 / M2.17 / M2.19 / M2.20 from PRD #76, plus the round 5+ rounds in PRD #93's implementation notes. Each was a regression caught reactively; confirm the corresponding feature is now Preserved in current main, and use the anchor as a spot-check on the methodology (if the audit's pre-existing list does not catch one of these, the methodology is too narrow).
- **Follow-up milestones for every Regressed row**, drafted in the audit document under a "Follow-up milestones to file" section. The user reviews drafts before any GitHub issue is filed.

### Out of Scope

- **Hypothetical bugs in current code that have no baseline equivalent.** The v1 audit attempt drifted into this and surfaced findings (notably one about remote-network-attach assumptions) against an architecture this codebase does not have — TUI and daemon are always co-located, see `docs/remote-environments.md:8` and `:52–67`. Parity only.
- **Performance, security, or any other axis.** Behavioral parity only.
- **Pre-PRD-#76 bugs that the daemon transition incidentally fixed.** Those are improvements, not regressions.
- **Features that genuinely did not exist at baseline** (the `remote add/list/remove/upgrade` family, daemon idle-shutdown, daemon log destination, lazy-spawn semantics, attach protocol Hello handshake, KIND_EVENT plumbing, etc.). These are post-baseline additions, not parity concerns. List them in an appendix to the audit doc so a future re-audit knows what was deliberately added.
- **Fixing any of the findings.** Each Regressed row is drafted as a follow-up milestone in the audit document; nothing is filed as a GitHub issue until the user reviews and authorizes, and fixes are scoped separately.

## Success Criteria

- `audit/pre-daemon-parity-audit.md` exists.
- Every user-visible feature present at `2fc39c3` has a row in the document with a triage column (Preserved / Regressed / Intentional change), a one-sentence rationale, and an evidence pointer (file:line in current code plus a baseline reference where useful).
- The force-shutdown gap appears as a Regressed row anchored to PRD #93 line 39.
- Every Regressed row has a corresponding 2–3 sentence follow-up milestone draft in the deliverable's "Follow-up milestones to file" section.
- The audit document opens with a coverage statement: which baseline feature categories were checked, which were deferred and why. A future re-audit can extend the statement rather than redo the work.
- No numeric floor on findings. Count is not the goal; honest coverage is.

## Milestones

### Phase 1: Baseline enumeration

- [ ] **M1.1** — Read baseline state at `2fc39c3`. Use `git show 2fc39c3:<path>` for individual files or check out a temporary worktree at the baseline. Cover baseline `src/`, baseline `tests/`, baseline `docs/`, and any closed PRDs in `prds/done/` that shipped before `2fc39c3`. Build a feature/behavior list. The list comes from baseline, not from current code.
- [ ] **M1.2** — Map each historical anchor (M2.11, M2.12, M2.13, M2.17, M2.19, M2.20, plus PRD #93 implementation-notes rounds) onto one or more rows in the list. Confirm the methodology would have caught each anchor if it had not already been fixed.

### Phase 2: Current-state verification

- [ ] **M2.1** — For each baseline feature, locate the current implementation in main and decide the triage bucket. Use `Explore` agents for breadth where the surface is wide (event delivery, daemon lifecycle, attach protocol, orchestration dispatch).
- [ ] **M2.2** — For each Preserved candidate, require at least one current test that exercises the daemon path. If no test, demote to Regressed — untested parity is unverified parity.
- [ ] **M2.3** — For each Intentional change, record the PRD or commit that justifies the change (so future re-audits do not re-flag).

### Phase 3: Writeup and follow-up

- [ ] **M3.1** — Finalize `audit/pre-daemon-parity-audit.md` with: coverage statement, findings table, worked example (force-shutdown gap), historical-anchor appendix, post-baseline-additions appendix.
- [ ] **M3.2** — Draft a 2–3 sentence follow-up milestone for each Regressed finding. Do **not** file GitHub issues — the user reviews drafts before any filing.
- [ ] **M3.3** — Brief writeup in the PR description summarizing counts per triage bucket.

## Key Files

Baseline reading targets (at `2fc39c3` — use `git show 2fc39c3:<path>` or a temporary worktree):

- `src/main.rs` — baseline CLI surface, dashboard entry point.
- `src/embedded_pane.rs` — baseline pane I/O and lifecycle.
- `src/state.rs`, `src/ui.rs`, `src/tab.rs` — baseline `AppState` shape and TUI behaviors.
- `src/hook.rs` — baseline hook ingestion.
- `tests/` — baseline integration test coverage (every test that exists at baseline is a baseline-feature assertion worth checking against current main).
- `docs/` — baseline user-facing documentation, especially `getting-started.mdx` and `installation.md`.

Current-code read targets (for the parity check):

- `src/state.rs` — `AppState`, target-pane resolution, session lifecycle.
- `src/daemon.rs` — daemon startup and idle-shutdown.
- `src/daemon_protocol.rs` — attach protocol wire format.
- `src/daemon_client.rs` — TUI-side attach protocol client.
- `src/agent_pty.rs` — `AgentPtyRegistry`, daemon-side PTY ownership, `write_to_pane`.
- `src/pane_input.rs` — `encode_pane_payload`, `SUBMIT_DELAY`, bracketed-paste handling.
- `src/embedded_pane.rs` — `EmbeddedPaneController`, pane read/write paths.
- `src/main.rs` — auto-spawn, lock contention, CLI surface.
- `src/ui.rs`, `src/hook.rs` — TUI-side event consumers.
- `tests/rehydration.rs`, `tests/event_forwarding.rs`, `tests/daemon_integration.rs`, `tests/orchestration_delegate.rs`, `tests/local_attach.rs` — real-daemon integration coverage.

Audit deliverable:

- `audit/pre-daemon-parity-audit.md` — new file.

## Design Decisions

### 2026-05-22: Parity audit framing

The audit's axis is baseline-versus-current behavioral parity, not a forward-looking review of current code. The deck shipped two architectural pivots back-to-back (PRD #76 daemon-as-separate-process, PRD #93 daemon-as-only-process) and each was tested by re-implementing the architecture, not by enumerating pre-pivot features and confirming they survived. The "what changed silently" question is the right one to ask of the resulting code, and the answer requires looking at what was there before — hence the parity framing.

The baseline is `2fc39c3`, the last commit before PRD #76 merged. That is "the deck as it was before the two pivots." Newer baselines drift into the architectures being audited.

A v1 attempt at this PRD ran in a different direction — a forward-looking behavior audit of the current codebase. That attempt confirmed the force-shutdown gap is real (carried forward as the worked example in this PRD). The v1 attempt's other findings do not carry forward: they either target a hypothetical remote-network-attach architecture (laptop-TUI ↔ remote-daemon over a network) that this codebase does not implement, or they concern post-baseline behavior (detach windows, daemon-side event application during disconnect) that has no parity analog because the baseline had no daemon. Those questions belong to a separate post-baseline behavior audit, not this one.

### 2026-05-22: Three-bucket taxonomy

Preserved / Regressed / Intentional change. The v1 attempt had four buckets including "Local-attach assumption" — scoped against a network-attach architecture that the deck does not implement. `docs/remote-environments.md:52–67` is explicit: `connect` runs the TUI on the remote alongside the daemon, the laptop is just a terminal. Same host, same filesystem, same user, same process tree. The Local-attach bucket has no referent in this codebase and is dropped.

### 2026-05-17: Audit, not refactor

Retained from the original PRD. The audit explicitly does not fix anything. Mixing audit and fix work obscures the audit's scope — readers cannot tell whether a clean area was checked or simply not visited. Each Regressed row is drafted as a follow-up milestone in the audit document; not filed as a GitHub issue until the user reviews and authorizes, and fixes are scoped separately.
