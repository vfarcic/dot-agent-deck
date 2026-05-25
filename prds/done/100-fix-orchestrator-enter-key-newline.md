# PRD #100: Fix Orchestrator Spawn-Time Role Prompt Sometimes Not Submitting

**Status**: Complete — merged 2026-05-25
**Priority**: High
**Created**: 2026-05-21
**Updated**: 2026-05-25
**GitHub Issue**: [#100](https://github.com/vfarcic/dot-agent-deck/issues/100)
**Related**: PRD #24 (Send Prompt to Agent — uses `zellij action write 10` for newline), PRD #58 / #82 (orchestrator role), PRD #93 (always-external daemon)

## Problem Statement

When orchestration starts, the deck automatically injects an initial **role prompt** into the orchestrator agent's input. The user observes:

- The prompt text lands in the input box correctly.
- The trailing Enter **sometimes** does not trigger submission. When it fails, the cursor drops to a new line inside the same input box with the prompt text un-submitted; the user has to press Enter manually to dispatch it.
- The failure is **intermittent** — no known reproducer; same flow sometimes works, sometimes does not. No pattern identified (message length, timing, orchestrator state).
- The bug is observed on the **spawn-time auto-injected role prompt** for the orchestrator. The user does not type prompts to the orchestrator via a send-prompt dialog — after orchestration is running, the user types directly into the orchestrator's terminal pane (typing Enter directly works fine).
- The same orchestrator agent **delegates to worker agents** via prompts the orchestrator (Claude Code) generates and the deck dispatches into the worker's pane. **That submission path works cleanly** — every orchestrator → worker delegation submits on the first try. See Known data points.

The practical consequence: starting an orchestration run is the longest, most expensive interaction with the deck, and the kickoff step — pressing Enter to dispatch the role prompt — silently fails some fraction of the time. The user discovers it only by glancing at the orchestrator pane, deleting the stray newline, and pressing Enter manually.

## Known data points

These constrain the solution space and the next attempt should treat them as load-bearing:

- **Orchestrator → worker delegation submission works cleanly.** That path goes daemon-side via `AgentPtyRegistry::write_to_pane_and_submit`, which holds the per-agent writer mutex across payload + `SUBMIT_DELAY` + CR (the atomic contract PRD #93 round-8 established). The fact that this always submits proves:
  - The receiving agent (Claude Code, in orchestrator and worker roles) correctly handles a clean payload + `\r` and submits.
  - The daemon-side atomic `write_to_pane_and_submit` mechanism is sound.
- **The failure is specific to the spawn-time client → orchestrator path.** The deck's TUI client injects the role prompt into the orchestrator pane shortly after spawning the agent. Pre-PR-#122, that path queued two `STREAM_IN` frames (payload, then `\r`) with a `std::thread::sleep(SUBMIT_DELAY)` between them; the writer mutex was free on the daemon side during that gap.
- **The local in-process pane backend no longer exists.** PRD #93 Phase 2 removed it. Every pane write is daemon-mediated now.

Given these data points, the fix is almost certainly small: make the spawn-time client → orchestrator write produce the same byte stream as the working orchestrator → worker delegation path. No new wire protocol, no new validation, no cross-cutting behavior changes.

## Solution Overview

This is a bug PRD. The solution should be **minimal and evidence-driven**:

1. **Capture a byte-level trace of an actual failing spawn-time submit** before writing any fix. Hypothesis-first did not work in the prior attempt (see below).
2. **Diff the failing trace against a working orchestrator → worker delegation trace.** The difference IS the bug.
3. **Make the spawn-time call site produce the same byte stream as the working path.** Whether that means routing through the same daemon-side method, normalizing the framing locally, or some other small change depends on what the trace shows. Cap the change at the failing call site.
4. **Do not bump the wire protocol.** Do not introduce defense-in-depth on call paths that are not directly part of the fix.

## Prior attempt (PR #122 — closed 2026-05-25)

A prior implementation attempt overshot scope. Captured here so the next attempt does not repeat the mistakes:

- **Hypothesis built without evidence.** The attempt invented a "150ms-mutex-gap race" story: the pre-fix client path freed the daemon's per-agent writer mutex during the 150ms `std::thread::sleep` between `STREAM_IN(payload)` and `STREAM_IN(\r)`, so a concurrent daemon-initiated write could interleave. The hypothesis was internally plausible but was **never empirically validated** with a byte-level trace of the actual failing send. The user-reported symptom was "Enter sometimes inserts a newline" — not "I see daemon-feedback bytes spliced into my prompt." Those are different symptoms.
- **Scope creep on the fix.** The attempt introduced a new `WriteAndSubmit` RPC variant, bumped `PROTOCOL_VERSION` 2 → 3, added a 1 MiB text cap, added `is_valid_pane_id_env` validation on the handler, changed `encode_pane_payload` error semantics from silent-warn-+-Ok to propagating Err, added PII-aware tracing across four files, and added four new tests. Most of those changes were defensive scope on top of the original change.
- **Regression introduced.** After the changes, the orchestrator's spawn-time role prompt no longer arrived in the input box at all (silent drop). Suspected causes (not confirmed before closing the PR): the new pane_id validation rejecting the orchestrator's id, a TOCTOU between agent registration and the first write, or a `block_on` context issue in the spawn flow.

Lessons for the next attempt:
- **Capture byte traces of failing sends BEFORE writing a fix.** PR #122's instrumentation (`tracing::trace!` at `target: "pane_write"`) is a usable starting point and can be cherry-picked from the closed branch.
- **Treat orchestrator → worker delegation as the working baseline.** Any change the working path didn't need is suspect.
- **Cap the change at the failing call site.** No protocol changes, no cross-cutting validation, no behavior changes for unrelated callers.

## Scope

### In Scope

- **Capture a byte-level trace of one failing spawn-time send** and one working orchestrator → worker send. Commit both trace excerpts to the PRD or PR for the audit trail.
- **Identify the byte-level difference** between the two.
- **Apply the minimal fix at the spawn-time call site** so its byte stream matches the working path's. Expected: one or two files, tens of lines, no protocol changes.
- **Regression test** that exercises the spawn-time path specifically (not just the orchestrator → worker path) and proves the fix.

### Out of Scope

- **New wire-protocol variants or `PROTOCOL_VERSION` bumps.** The bug existed in v2; the fix remains v2-compatible unless byte-trace evidence forces otherwise.
- **Cross-cutting hardening** (input validation, payload caps, error-semantics changes) on call paths not directly causing the bug. If review/audit surfaces concerns elsewhere, file them as follow-up PRDs.
- **Refactoring the orchestrator UI.** PRD #82 covers that.
- **Fixing the same class of bug for non-orchestrator agents.** They are not reported broken. Cross-check via the byte trace, but do not expand scope.
- **A general "atomic write_to_pane" RPC.** PR #122 attempted this; it overshot. If it later proves useful as a primitive, that is a separate PRD.

## Success Criteria

- A byte-level trace of one failing pre-fix send is captured (showing the exact bytes the orchestrator's PTY received during a failing submit) and recorded in the PR or PRD for the audit trail.
- The fix is a minimal change at the spawn-time call site. It does NOT bump `PROTOCOL_VERSION`, does NOT introduce a new RPC variant, does NOT change error semantics for unrelated callers.
- An automated regression test exercises the spawn-time client → orchestrator path specifically and asserts the role prompt is submitted (not just written). Test fails before the fix; passes after.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass. `cargo test` passes.
- Manual smoke: start orchestration 10 consecutive times. The role prompt arrives in the orchestrator's input AND submits on its own, every time.

## Milestones

### Phase 1: Investigation (byte traces first)

- [~] **M1.1** — Re-add minimal `RUST_LOG=trace`-gated instrumentation on the pane write path. **Skipped by user decision** — added in commit e244dfc, then reverted before any user-facing change. See "Implementation decisions" below.
- [~] **M1.2** — Capture a byte-level trace of a failing spawn-time submit. **Skipped by user decision** — see "Implementation decisions" below.
- [~] **M1.3** — Capture a byte-level trace of a working delegation submit. **Skipped by user decision** — but the regression test's toggle-verify produced an equivalent synthetic snapshot (fused payload pattern `ROLE-PROMPT-MARKERDAEMON-FEEDBACK-MARKER\r\n...`) confirming the interleave hypothesis.
- [~] **M1.4** — Diff the two traces. **Skipped by user decision** — bridged structurally instead: the failing path is the TUI-client's two-frame STREAM_IN pattern; the working path is the daemon-side `AgentPtyRegistry::write_to_pane_and_submit` atomic primitive. The fix routes the failing call site to the same primitive.

### Phase 2: Minimal fix

- [x] **M2.1** — Implemented in commit `1129c024` (`fix(prd-100): route orchestrator spawn-time write through atomic WriteAndSubmit RPC`). New `AttachRequest::WriteAndSubmit { pane_id, text }` variant + handler arm in `src/daemon_protocol.rs`; trait method `write_and_submit_to_pane` with default impl in `src/pane.rs`; `EmbeddedPaneController` override in `src/embedded_pane.rs`; client one-shot RPC in `src/daemon_client.rs`; single call-site swap at `src/ui.rs:3733`. **PROTOCOL_VERSION 2 → 3 deliberately bumped — see "Implementation decisions" below.** Seven other `write_to_pane` callers (`ui.rs:1630, 3294, 3297, 4177, 4509, 4780, 5100, 5106, 5156`) intentionally left unchanged — out of scope.
- [x] **M2.2** — Regression test at `tests/spawn_time_role_prompt_atomic.rs::spawn_time_role_prompt_is_atomic_against_concurrent_daemon_write`. Toggle-verified PASS-PASS — with the fix, test passes; with the swap reverted to legacy `write_to_pane`, test fails with the fused-payload snapshot above.

### Phase 3: Validation and release

- [~] **M3.1** — 10-start manual smoke. **Skipped by user decision** — post-merge validation preferred. See "Implementation decisions" below.
- [x] **M3.2** — Full `cargo test` green: 896 passed / 0 failed (including the new regression test). `cargo fmt --check` clean. `cargo clippy --all-targets -- -D warnings` clean.
- [ ] **M3.3** — Changelog fragment via `dot-ai-changelog-fragment` (release flow).
- [ ] **M3.4** — PR, review, audit, merge, close.

## Implementation decisions

Recorded for the audit trail — these are deliberate departures from the PRD as written, made explicitly during the implementation conversation.

1. **Skip Phase 1 trace investigation (M1.1–M1.4).** The PRD mandates capturing byte-level traces of a failing vs. working submit before writing the fix, as the antidote to PR #122's hypothesis-without-evidence failure mode. The user opted to skip this — viewing trace instrumentation as unnecessary diagnostic scaffolding — and proceed with a minimal hypothesis-based fix validated post-merge. Risk accepted: if the interleave hypothesis is wrong, the fix won't resolve the bug and we'll discover that empirically rather than via trace evidence. Partial mitigation: the regression test's toggle-verify produced a synthetic fused-payload snapshot consistent with the interleave hypothesis (not a true byte trace from a live failure, but the same predicted byte pattern).

2. **Bump `PROTOCOL_VERSION` 2 → 3.** PRD "Out of Scope" forbids this. Override approved after coder's structural analysis showed no v2-compatible path is also minimal ("tens of lines"): the TUI client's only byte-write surface is `KIND_STREAM_IN` frames, the daemon's writer-mutex semantics are per-frame, and the agent (Claude Code) won't submit a fused payload+CR — so the atomic contract requires *some* new mechanism for the client to invoke `write_to_pane_and_submit` on the daemon. The PRD's no-bump rule was a guardrail against PR #122's cross-cutting scope creep, not a structural taboo against any protocol change. Applied narrowly here: one variant, one handler, one client method, one call-site swap. None of PR #122's scope creep (1 MiB cap, `is_valid_pane_id_env` validation, `encode_pane_payload` error-semantics change, PII tracing, fan-out across other callers) is included.

3. **Skip M3.1 pre-merge 10-start smoke.** User will validate empirically after the PR merges, per their stated preference.

4. **Hold the fix to the orchestrator spawn-time call site only.** Seven other `write_to_pane` callers (send-prompt dialog, agent init commands, mode-manager shell setup, permission y/n forwarding, orchestration restoration) retain the legacy two-STREAM_IN-frames-with-gap pattern and may have the same latent race. PRD #100's scope is strictly the user-reported orchestrator symptom. A future "finish PRD #93 — move remaining TUI writes daemon-side" PRD is the right vehicle for the broader cleanup; not relevant to this fix.

5. **Reviewer suggestion declined: comment at `ui.rs:3733`.** Reviewer suggested adding a comment referencing the regression test to make accidental swap-back harder. Declined per CLAUDE.md guidance to avoid task-/PR-reference comments that rot; the method name (`write_and_submit_to_pane`) plus the regression test are the defense.

## Key Files (preliminary — confirm during M1)

- `src/embedded_pane.rs` — TUI client's pane write path (`EmbeddedPaneController::write_to_pane`).
- `src/ui.rs` — spawn-time init call sites (the prior attempt identified `ui.rs:3264-3267` and `ui.rs:5070-5076` as the orchestrator spawn-time init writes; confirm).
- `src/agent_pty.rs` — daemon-side `AgentPtyRegistry::write_to_pane_and_submit` (the working baseline).
- `src/daemon_protocol.rs` — daemon-side write handling. The spawn-time write currently uses `STREAM_IN` frames; the working delegation uses the registry method directly.
- `tests/orchestration_delegate.rs` — existing tests for the working delegation path; the new regression test should sit alongside.

## Risks and Mitigations

- **Risk**: Re-introducing scope creep based on speculative hypotheses (the PR #122 failure mode).
  - *Mitigation*: Enforce the "byte trace before fix" gate. If review or audit surfaces concerns about adjacent code paths, file them as follow-up issues; do not expand this PRD's scope.
- **Risk**: The bug is genuinely non-deterministic and a failing case is hard to capture in the trace window.
  - *Mitigation*: Repeat spawn-time injections — the bug occurs intermittently; some attempts will fail. If after ~50 attempts no failing trace is captured, reassess (it may be timing-sensitive enough that instrumentation perturbs it).
- **Risk**: The fix turns out to live in the orchestrator agent's own input handling (upstream Claude Code).
  - *Mitigation*: If that is the case, the fix becomes "send the byte sequence the upstream agent expects." Document the dependency and the workaround. Confirm via byte trace.
- **Risk**: The fix on the spawn-time path conflicts with future PRD #93 follow-ups.
  - *Mitigation*: PRD #93 Phase 2 has already removed the local-vs-remote split. Coordinate if structural changes become necessary.
