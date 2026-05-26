# PRD #128: Move Orchestrator Spawn-Time Role Prompt Injection Daemon-Side

**Status**: In Progress — Phase 1 instrumentation landed; trace capture (M1.2/M1.3) pending against user's broken environment.
**Priority**: High
**Created**: 2026-05-25
**GitHub Issue**: [#128](https://github.com/vfarcic/dot-agent-deck/issues/128)
**Related**: PRD #100 (prior interleave-race fix, shipped in v0.27.0), PRD #93 (always-external daemon; daemon-side `write_to_pane_and_submit`), PRD #92 F9 followup-6 (delegate path's SessionStart hook-broadcast readiness gate)

## Problem Statement

After PRD #100's fix shipped in v0.27.0, the orchestrator's spawn-time role prompt **still** lands in the input box without being submitted. The trailing Enter is interpreted as a literal newline; the user sees the prompt text followed by a blank line, with the orchestrator's input still un-dispatched.

Empirically reproduced 2026-05-25 against a fully-upgraded environment:

- TUI process and daemon process both running the same v0.27.0 on-disk binary, both restarted within minutes of the test.
- Daemon listening on `/run/user/0/dot-agent-deck.sock` (local; no remote-daemon variable).
- Brand-new orchestration kicked off from a clean dashboard — exactly the spawn-time path PRD #100 intended to fix.
- Symptom: `Read .dot-agent-deck/orchestrator-context.md...` arrives in the orchestrator's input box; Enter falls through as a newline; the prompt is not dispatched.
- Orchestrator → worker delegations on the same run submit cleanly every time. Same agent (Claude Code), same daemon primitive (`AgentPtyRegistry::write_to_pane_and_submit`), same submit semantics — only the trigger and ownership differ.

This disproves PRD #100's hypothesis. The fix solved a real race (the per-frame-mutex interleave window between two `STREAM_IN` frames separated by `SUBMIT_DELAY`) but that race was not the cause of the user-reported symptom.

## Known data points (load-bearing)

These constrain the next attempt. Each is observable in the current tree; none rely on memory of the prior PRD's deliberations.

1. **PRD #100's fix is in effect.** `src/ui.rs:3779` calls `pane.write_and_submit_to_pane(...)`, which routes through the `WriteAndSubmit` RPC (`src/daemon_client.rs:270`, `src/daemon_protocol.rs:948`) to the daemon's atomic `AgentPtyRegistry::write_to_pane_and_submit` (`src/agent_pty.rs:1466`). The daemon's per-agent writer mutex is held across `payload → SUBMIT_DELAY → CR`. Toggle-verified by `tests/spawn_time_role_prompt_atomic.rs` against a concurrent daemon-initiated writer. The interleave the fix prevents does not happen in the new path — and the symptom still occurs.

2. **The orchestrator → worker delegate path is structurally different in one key way.** `state.rs::dispatch_one_owned` (called from `handle_delegate`) runs as a dedicated tokio task on the daemon side and uses an explicit hook-broadcast readiness gate:
   - Subscribes to `event_tx` **before** the new agent process is forked (PRD #92 F9 followup-6, `src/state.rs:374-382`).
   - Calls `wait_for_session_start` scoped to the **new agent's id** (`src/state.rs:402-408`).
   - Calls `write_to_pane_and_submit` immediately on wake-up, from the same async task.

3. **The spawn-time path's readiness gate is in the TUI loop.** `src/ui.rs:3768-3779` polls `snapshot.sessions.values().any(|s| s.pane_id.as_deref() == Some(start_pane_id) && s.agent_type != AgentType::None)`. `agent_type` flips on receipt of the broadcast `SessionStart` event via `apply_event`. So both paths nominally wait for the same hook — but the spawn-time path takes additional hops: daemon hook receiver → broadcast → TUI deserialize → `apply_event` on snapshot → next render tick → `agent_ready` check → RPC to daemon. The delegate path waits on the broadcast directly.

4. **The submit primitive is identical for both paths.** Both end up calling `AgentPtyRegistry::write_to_pane_and_submit` with the same payload+SUBMIT_DELAY+CR semantics and the same per-agent writer mutex. If the working path's writes submit and the failing path's writes don't, the difference must lie in **when** the write fires relative to the agent's TUI input readiness — not in **how** the bytes are produced.

5. **Hypothesis (to be validated by trace, not by code).** Claude Code's `SessionStart` hook fires very early in its boot sequence — possibly before its TUI input is in a state where it interprets `\r` as submit. The delegate path appears to tolerate this because the worker pane is background/off-screen at spawn time, or because the orchestrator's own steady-state stdout activity provides a hidden synchronization. The spawn-time path fires the write into a foreground, fresh-init pane and loses the CR.

6. **PR #122's instrumentation exists and is cherry-pickable.** `tracing::trace!` events at `target: "pane_write"` were added across four files in PR #122 (closed) before the regression that closed the PR. They can be re-applied minimally without the rest of PR #122's scope creep.

## Solution Overview

**Phase 1 — byte-trace investigation — is mandatory this time.** PRD #100 skipped it and shipped a fix that did not resolve the bug; the present PRD exists because of that skip. Do not skip again.

1. Cherry-pick the minimal `RUST_LOG=trace`-gated instrumentation from PR #122 onto a working branch. Daemon-side only, on `write_to_pane_and_submit` and the surrounding PTY-write path. Commit it as the first commit on the branch so the trace evidence is auditable.
2. Capture a byte-level trace of **one failing spawn-time submit** against a fresh orchestration. Record what the orchestrator's PTY received in time order: the role-prompt payload, the SUBMIT_DELAY gap, the CR, and any post-CR bytes from Claude Code itself.
3. Capture a byte-level trace of **one working delegate submit** against the same daemon. Same time-ordered record.
4. Diff the two traces. The byte-level or timing-level difference IS the bug.
5. Apply a minimal fix at the spawn-time call site that closes the difference. Two plausible directions exist depending on what the trace shows:
   - **Direction A — structural mirror.** Move the spawn-time injection daemon-side: at orchestrator-pane spawn time, the daemon starts a task that subscribes to its own hook broadcast, waits for the orchestrator agent's first `SessionStart` event (scoped by the new agent's id), then calls `write_to_pane_and_submit`. The TUI's `ui.rs:3768-3779` block goes away; the `WriteAndSubmit` RPC from PRD #100 is no longer called from the spawn-time site (it remains in the codebase as the atomic-write primitive — see "Out of scope" below).
   - **Direction B — adjusted trigger only.** If traces show `SessionStart` arrives well before Claude Code's input is live, add a more specific readiness signal (e.g. wait for a stdout-quiet gap, or for a Claude Code–specific banner-completion marker). Cheaper but more fragile.

The trace evidence decides between A and B. Do not commit to a direction before the trace.

## Scope

### In scope

- **Phase 1 byte-trace investigation.** Cherry-pick the minimal instrumentation from closed PR #122 (daemon-side only, gated behind `RUST_LOG=trace`, `target: "pane_write"`). Commit trace excerpts to the PR for the audit trail.
- **Minimal fix at the spawn-time call site.** Expected one or two files; tens of lines. No fan-out across unrelated callers.
- **Regression test that exercises the spawn-time path specifically** against an agent stub that fires `SessionStart` *before* its input is live (the failure mode the production bug is hypothesized to be). Test fails before the fix; passes after. Toggle-verified.
- **Manual 10-start smoke (NOT skipped this time — see Lessons).**

### Out of scope

- **Refactoring other `write_to_pane` callers.** Seven legacy two-frame-with-gap callers (send-prompt dialog, agent init commands, mode-manager shell setup, permission y/n forwarding, orchestration restoration) remain unchanged. PRD #100 Decision 4 stands.
- **Removing the `WriteAndSubmit` RPC.** Keep PRD #100's atomic-write primitive in the codebase — it protects against the interleave race even if that race was not the user's reported symptom. Direction A would stop calling it from the spawn-time site; that is the only change to its surface.
- **New wire-protocol variants or `PROTOCOL_VERSION` bump.** None expected. If trace evidence forces one, document it as an explicit decision in this PRD's "Implementation decisions" section.
- **Cross-cutting hardening / validation / PII-aware tracing of the kind PR #122 attempted.** Trace instrumentation is daemon-side and `RUST_LOG=trace`-gated only.
- **Refactoring the orchestrator UI.** PRD #82 covers that.

## Success Criteria

- **Byte traces of one failing pre-fix send and one working delegate send are committed to the PR.** No code-side fix lands before this evidence is recorded.
- **The byte-level or timing-level difference between failing and working sends is stated in the PRD or PR description** as the load-bearing finding the fix targets.
- **The fix is minimal at the spawn-time call site.** One or two files; tens of lines. No new protocol variant. No cross-cutting hardening.
- **An automated regression test drives the spawn-time client → orchestrator path specifically against a slow-input-readiness agent stub** and asserts the role prompt submits (not just arrives). Test fails before the fix; passes after. Toggle-verified.
- **Manual smoke: start orchestration 10 consecutive times.** The role prompt arrives in the orchestrator's input AND submits on its own, every time.
- **`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and the full `cargo test` suite pass.**

## Milestones

### Phase 1: Investigation (mandatory — do not skip)

- [x] **M1.1** — Cherry-pick the minimal `RUST_LOG=trace`-gated instrumentation from closed PR #122 onto a working branch. Daemon-side, on `write_to_pane_and_submit` and the PTY-write path. Commit it as the first commit on the branch. *(Done: 8a8a025 cherry-pick + 123cd87 field symmetry/escape fixes + 877838c sentinel distinction & helper visibility. Trace events at `target="pane_write"` cover daemon write_to_pane (payload/submit terminator/notice terminator) and STREAM_IN forwarding, all inside the per-agent writer mutex; both events emit `pane_id` + `agent_id` for cross-path diff.)*
- [ ] **M1.2** — Capture a byte-level trace of a failing spawn-time submit on a fresh orchestration. Commit the trace excerpt to the PR.
- [ ] **M1.3** — Capture a byte-level trace of a working orchestrator → worker delegate submit on the same daemon. Commit the trace excerpt.
- [ ] **M1.4** — Diff the two traces. Record the byte-level or timing-level difference in this PRD ("Investigation findings" section, to be added) before any production-code change.

### Phase 2: Minimal fix

- [ ] **M2.1** — Implement the fix. Direction A (structural mirror — daemon-side spawn-time injection waiting on hook-broadcast `SessionStart`) is the front-runner; Direction B (adjusted trigger only) is the fallback. Trace evidence decides. Cap fan-out: spawn-time call site only.
- [ ] **M2.2** — Regression test at `tests/spawn_time_role_prompt_submit_after_session_start.rs` (or similar) that drives the spawn-time path against an agent stub fielding `SessionStart` *before* its input is ready. Assert the role prompt submits. Toggle-verify: revert the fix → test fails with the un-submitted symptom; restore → passes.

### Phase 3: Validation and release

- [ ] **M3.1** — Manual 10-start smoke against the fix. NOT skipped — the failure mode is intermittent and the prior PRD's skip is exactly why this PRD exists.
- [ ] **M3.2** — Full `cargo test` green; `cargo fmt --check` clean; `cargo clippy --all-targets -- -D warnings` clean.
- [ ] **M3.3** — Changelog fragment via `dot-ai-changelog-fragment`.
- [ ] **M3.4** — PR, review, audit, merge, close.

## Implementation decisions

Recorded up front to prevent the prior PRD's failure modes.

1. **Phase 1 trace investigation is NOT skipped.** PRD #100 skipped it (its Decision 1 explicitly accepted the risk) and shipped a hypothesis-based fix that did not work. The cost of that skip is this PRD. Repeating it would burn another release cycle. The trace evidence is the load-bearing input to the Phase 2 direction choice.

2. **M3.1 10-start manual smoke is NOT skipped.** Same reasoning. The bug is intermittent; a one-shot post-merge validation is insufficient. Run 10 consecutive fresh orchestrations against the fix before declaring it done.

3. **No `PROTOCOL_VERSION` bump expected.** Both Direction A and Direction B can be implemented without new wire frames — Direction A moves the existing daemon-side `write_to_pane_and_submit` call to a new daemon-side task, no new RPC; Direction B is a trigger-only change in the TUI loop or in the existing daemon flow. If the trace forces a protocol change, document the decision here before implementing.

4. **`WriteAndSubmit` RPC is preserved.** PRD #100's atomic-write primitive stays in the codebase. Direction A would stop calling it from `ui.rs:3779` (the only call site) and leave the RPC as a no-caller primitive ready for future use, or — at the implementer's option — remove it as dead code if no future caller is anticipated. Decide during Phase 2 based on whether keeping a one-RPC dead surface is worth the optionality.

5. **Hold the fix to the orchestrator spawn-time call site only.** PRD #100 Decision 4 stands: seven other `write_to_pane` callers retain the legacy pattern. A future "finish PRD #93 — daemon-side TUI writes" PRD is the right vehicle for the broader cleanup.

## Key files (preliminary — confirm during M1)

- `src/ui.rs:3756-3785` — current spawn-time orchestrator role-prompt injection block (the `agent_ready` poll and the `write_and_submit_to_pane` call).
- `src/state.rs::dispatch_one_owned` (`src/state.rs:274` onward) and `wait_for_session_start` (`src/state.rs:178` onward) — the working delegate path's structural baseline.
- `src/agent_pty.rs::write_to_pane_and_submit` (`src/agent_pty.rs:1466`) — the daemon-side atomic primitive, unchanged.
- `src/daemon.rs` orchestrator-pane spawn handler (Direction A target — where a daemon-side spawn-time injection task would be wired).
- Closed PR #122 — source for cherry-picked `RUST_LOG=trace` instrumentation.
- `tests/spawn_time_role_prompt_atomic.rs` — PRD #100's regression test for the interleave race. Stays. New PRD adds a sibling test for the input-readiness race.

## Open questions

- Does Claude Code's `SessionStart` hook fire before its TUI input is in submit-CR-aware mode? If yes, by how much? Trace will tell.
- Is the delegate path's apparent immunity due to the worker pane being background at spawn time, or due to some other hidden synchronization (e.g. the orchestrator's own stdout activity gating the daemon's writer in a way the fresh-orchestrator-pane case lacks)? Trace will tell.
- If Direction A is taken, what does "orchestrator-pane spawn" look like daemon-side? The TUI initiates the spawn via `start_agent`; the daemon would need a flag in that request (or a separate signal) that says "after this agent's first SessionStart, run the spawn-time injection task with the role prompt." Worked out during M2.1 based on M1 evidence.
