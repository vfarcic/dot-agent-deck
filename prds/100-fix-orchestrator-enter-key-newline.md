# PRD #100: Fix Orchestrator Spawn-Time Role Prompt Sometimes Not Submitting

**Status**: Planning (second attempt — see "Prior attempt" section)
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

- [ ] **M1.1** — Re-add minimal `RUST_LOG=trace`-gated instrumentation on the pane write path if not already present. PR #122's branch has a working version under `target: "pane_write"` that can be cherry-picked.
- [ ] **M1.2** — Capture a byte-level trace of a failing spawn-time client → orchestrator submit. Repeat orchestration starts as needed until a failure is captured.
- [ ] **M1.3** — Capture a byte-level trace of a working orchestrator → worker delegation submit for comparison.
- [ ] **M1.4** — Diff the two traces. Document the byte-level difference. That difference IS the bug.

### Phase 2: Minimal fix

- [ ] **M2.1** — Implement the smallest change at the spawn-time call site that makes its byte stream match the working delegation path's. No protocol bumps, no new validation, no cross-cutting behavior changes.
- [ ] **M2.2** — Add a regression test that exercises the spawn-time path specifically. Test must fail before M2.1's change and pass after — toggle-verify.

### Phase 3: Validation and release

- [ ] **M3.1** — Manual smoke: 10 consecutive orchestration starts, each role prompt arrives and submits on its own. Document the pass.
- [ ] **M3.2** — Run the existing test suite (`cargo test`); confirm no regression for other agents (Claude Code, OpenCode) or other pane write paths.
- [ ] **M3.3** — Changelog fragment via `dot-ai-changelog-fragment`. Frame as a bug fix.
- [ ] **M3.4** — PR, review, audit, merge, close.

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
