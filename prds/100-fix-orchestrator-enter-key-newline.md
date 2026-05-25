# PRD #100: Fix Enter-Key Intermittently Inserting Newline Instead of Submitting to Orchestrator

**Status**: Planning
**Priority**: High
**Created**: 2026-05-21
**GitHub Issue**: [#100](https://github.com/vfarcic/dot-agent-deck/issues/100)
**Related**: PRD #24 (Send Prompt to Agent — uses `zellij action write 10` for newline), PRD #58 / #82 (orchestrator role), PRD #93 (always-external daemon — will collapse the local/remote split that may be hiding the bug)

## Problem Statement

When a user sends a message to the orchestrator agent via the deck's send-prompt path, **pressing Enter sometimes inserts a newline into the orchestrator's input box instead of submitting the message**. The user observes:

- The text content lands in the orchestrator's input correctly — the write path is not dropping characters.
- The trailing Enter does not always trigger submission. When it fails, the user sees the cursor drop to a new line inside the same input box, with the text still un-submitted.
- The failure is **intermittent**. There is no known reproducer; sometimes the same flow works, sometimes it does not. The user has not yet identified a pattern (e.g. message length, timing, orchestrator state).
- The bug has been observed **only with the orchestrator role**, not with other agents (Claude Code, OpenCode) in the dashboard.
- The bug has only been tested **while running through the daemon**. Whether it reproduces in the in-process / local path is unknown (and PRD #93 is on track to eliminate that split, so this distinction may become moot — but it is data worth collecting now).

The practical consequence: orchestration runs are the longest, most expensive interactions the user has with the deck, and the kickoff step — pressing Enter to dispatch the prompt — silently fails some fraction of the time. The user discovers it only by glancing at the orchestrator pane, deleting the stray newline, and pressing Enter again. This is the worst point in the workflow for an intermittent input failure.

## Solution Overview

This is a bug PRD, not a feature PRD. The solution shape depends on what the investigation finds. The PRD is structured around **investigation first, fix second** — explicitly, because guessing at causes for an intermittent bug wastes time.

Three leading hypotheses, listed in rough order of likelihood:

1. **Bracketed-paste framing eats the trailing Enter.** Many modern terminals support bracketed-paste mode (`\e[200~ ... \e[201~`). When an agent's input box is in bracketed-paste mode and the deck sends the prompt as a paste, an Enter inside the paste is treated as a literal newline, not a submit. If the daemon's pane-write path emits the prompt with bracketed-paste sequences (or if it does so under some conditions and not others), this matches the symptom precisely — including the intermittent character.
2. **`\n` vs `\r` framing inconsistency on the daemon write path.** PRD #24 documents using `zellij action write 10` to send a newline. Most TTY apps treat `\r` (CR, 13) as "submit" and `\n` (LF, 10) as "newline". If the daemon path sometimes emits 10 and sometimes 13 (or sometimes both, sometimes neither), Claude Code and OpenCode may be lenient about it while the orchestrator's input mode is strict.
3. **Orchestrator-UI input-mode race.** The orchestrator may have a multi-line input mode it toggles into transiently. If the Enter arrives during a window where the UI is in multi-line mode, the keystroke is interpreted as a newline. This would explain why only the orchestrator is affected (other agents have simpler single-line submit semantics).

These are hypotheses, not conclusions. Investigation in Phase 1 nails down which (if any) is the actual cause, and the fix is sized accordingly.

## Scope

### In Scope

- **Reproduce the bug deterministically.** Build a reproducer that fails reliably enough to bisect. May involve instrumenting the daemon write path to log every byte sent, and the orchestrator pane to log every byte received.
- **Identify root cause.** Confirm one of the three hypotheses (or surface a new one) with evidence.
- **Fix at the lowest correct layer.** If it is bracketed-paste, fix the daemon write path. If it is `\n` vs `\r`, normalize. If it is the orchestrator UI, fix the input-mode logic.
- **Regression test.** Add a test that fails before the fix and passes after, exercising the path through the daemon — not just an in-process unit test, since the bug is daemon-specific so far.
- **Investigate whether the bug exists in the local (in-process) path.** If yes, document; if no, note that PRD #93's unification will need to keep the daemon-only fix in place.

### Out of Scope

- **Broad refactor of the orchestrator UI** beyond what is needed to fix this bug. PRD #82 covers orchestrator role-reinforcement work; this PRD only touches input handling.
- **Adding new keybindings or customization** of submit vs newline. PRD #40 (customizable keybindings) and the broader chat-UI customization conversation handle that.
- **Fixing the same class of bug for non-orchestrator agents.** They have not been reported as broken. If the investigation reveals the same underlying mechanism could affect them, that goes in a follow-up.
- **Pre-emptively switching the entire write path to `expect`-style explicit framing.** If a small targeted fix works, ship it; do not gold-plate.

## Success Criteria

- A reproducer exists and is documented (steps or test) that triggered the bug reliably **before** the fix.
- After the fix, the same reproducer no longer triggers the bug — verified across N runs (N to be set in M2; rough target: 100 consecutive sends with zero newline-instead-of-submit).
- The fix is covered by an automated test in the daemon-path test suite. The test fails on `main` without the fix and passes with the fix applied.
- The root cause is documented in the PRD's "Root cause" section (added during Phase 2) so future investigators have an audit trail.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass. `cargo test` passes.
- Manual smoke: a long-form orchestrator prompt sent through the daemon submits on first Enter, 10 consecutive times.

## Root cause

Phase 1 ruled out the three leading hypotheses in their original framing and surfaced a fourth, which the evidence and the regression test in `tests/client_write_to_pane_atomic.rs` confirm. Summary:

- **PRD #93 Phase 2 collapsed the local/daemon split.** The in-process pane backend was deleted; every pane is now a `StreamBackend` routed through the daemon's attach socket (`src/embedded_pane.rs:74-78`). M1.3 ("does the bug exist on the local path?") is therefore moot — there is no longer a separate local path to compare against.
- **Two write surfaces remain, both daemon-mediated.** (a) Client-initiated via `EmbeddedPaneController::write_to_pane` (`src/embedded_pane.rs:1754` pre-fix) → STREAM_IN frames → daemon's `handle_attach_stream` input loop (`src/daemon_protocol.rs:1148`) → per-agent PTY writer. (b) Daemon-initiated (orchestration dispatch) via `AgentPtyRegistry::write_to_pane_and_submit` (`src/agent_pty.rs:1466`) → directly into the per-agent PTY writer.
- **The daemon-initiated path is atomic; the client-initiated path was not.** PRD #93 round-8 made `write_to_pane_and_submit` hold the per-agent writer mutex across the full payload + `SUBMIT_DELAY` + CR sequence (`src/agent_pty.rs:1455-1465`). The client-initiated path queued *two* STREAM_IN frames separated by a 150 ms `std::thread::sleep` (the pre-fix `src/embedded_pane.rs:1771-1773`), and the daemon's input loop took the writer mutex briefly per frame. So during the 150 ms gap the writer mutex was **free** on the daemon side.
- **A concurrent daemon-initiated write landed in that gap.** If a `write_to_pane_and_submit` (work-done feedback) or `write_to_pane_notice` (respawn notice) fired against the orchestrator pane while the user's send-prompt was mid-flight, the byte order written to the PTY master was `[user payload][daemon payload][daemon \r][user \r]`. After ICRNL and canonical line buffering the slave received one fused line; the daemon's CR submitted the combined `user-prompt + daemon-bytes` to the receiving agent, and the user's trailing CR was a no-op on an empty input box. To the user this manifested as "Enter inserted a newline into the orchestrator's input instead of submitting" — the PRD #100 symptom.
- **The orchestrator pane is uniquely affected** because it is the only pane that simultaneously *receives* daemon-initiated writes (work-done feedback, respawn notices) and *originates* user prompts. Other role panes only receive daemon writes; the dashboard's pure-user-input panes only originate writes. Neither shape produces concurrent same-pane writers.

The regression test toggle in Phase 2 confirmed the byte surface: with the pre-fix `write_to_pane` body, the scrollback shows `USERMSG-PAYLOADBGWRITER-MSG\r\n` (one fused canonical line); with the post-fix RPC body, two distinct `USERMSG-PAYLOAD\r\n` and `BGWRITER-MSG\r\n` lines surface. The fused line is exactly the bug shape the user observes in production.

## Fix scope

The fix routes all client-initiated send-prompt writes through a new `WriteAndSubmit` daemon RPC (`AttachRequest::WriteAndSubmit { pane_id, text }` in `src/daemon_protocol.rs`). On the daemon side the RPC handler calls `AgentPtyRegistry::write_to_pane_and_submit`, which holds the per-agent writer mutex across the entire payload + `SUBMIT_DELAY` + CR sequence — the same atomic contract orchestration dispatch already had since PRD #93 round-8.

This is **not** orchestrator-only. Every caller of `PaneController::write_to_pane` now benefits — the deck's send-prompt dialog (`src/ui.rs:1603`), the spawn-time init-command writes (`src/ui.rs:3264-3267`, `5070-5076`), the start-pane prompt (`src/ui.rs:3703`), the permission y/n forwarding (`src/ui.rs:4750`), the mode-manager init/command writes (`src/mode_manager.rs:308-322`), and the renew-after-respawn path (`src/mode_manager.rs:421-422`) all go through the same atomic RPC. The orchestrator-only symptom in the PRD was an *observation*, not a constraint — it surfaced there because the orchestrator pane is the only pane with concurrent daemon-initiated writes.

`PROTOCOL_VERSION` bumped from 2 to 3 (`src/daemon_protocol.rs:146`). The version handshake in `src/connect.rs` already rejects mismatched daemons with a clear error, so the upgrade boundary is honest.

## Milestones

### Phase 1: Investigation and reproducer

- [x] **M1.1** — Instrument the daemon's pane-write path to log every byte sent to a pane, with a feature flag or `RUST_LOG=trace` gate so the instrumentation can stay in tree. Confirm whether bracketed-paste sequences (`\e[200~ ... \e[201~`) are present and whether the trailing byte is `\n` (10), `\r` (13), or both.
- [x] **M1.2** — Build a deterministic reproducer. Try the obvious axes first: very long messages, messages with embedded newlines, rapid back-to-back sends, sends immediately after the orchestrator pane mounts, sends while another role pane is animating. Document what triggers the bug.
- [x] **M1.3** — Determine whether the bug exists in the local (in-process) path or only the daemon path. This is one data point that helps localize the cause and informs PRD #93's regression surface. *(N/A — PRD #93 Phase 2 deleted the local pane backend; all writes are daemon-mediated. Documented in Root cause.)*

### Phase 2: Root cause and fix

- [x] **M2.1** — Document the root cause in this PRD (add a "Root cause" section). Cite the evidence from M1.
- [x] **M2.2** — Implement the fix at the smallest layer that resolves it. Likely one of: daemon write path normalization, orchestrator UI input-mode guard, or removing bracketed-paste framing on the send-prompt path.
- [x] **M2.3** — Decide and document the fix's scope: orchestrator-only, all-agents, or a layer that incidentally fixes both.

### Phase 3: Tests and validation

- [x] **M3.1** — Add an automated regression test that exercises the daemon write path and asserts the submit actually fires (i.e. the orchestrator processed the message, not just that bytes were written). Test must fail before the fix.
- [ ] **M3.2** — Run the M1.2 reproducer 100 consecutive times against the fixed build; confirm zero failures.
- [ ] **M3.3** — Verify no regression for other agents (Claude Code, OpenCode) — run the existing send-prompt tests and a manual smoke through each agent type.

### Phase 4: Docs and release

- [ ] **M4.1** — Changelog fragment via `dot-ai-changelog-fragment`. Frame as a bug fix — "fix Enter sometimes inserting newline instead of submitting in the orchestrator pane".
- [ ] **M4.2** — Note in the PRD #93 dependency graph if any of this work should be folded into the unification, or if the fix lands on both paths cleanly.
- [ ] **M4.3** — PR, review, audit, merge, close.

## Key Files (preliminary — confirm during M1)

- `src/embedded_pane.rs` — pane write path, framing.
- `src/daemon.rs` — daemon-side handling of write commands.
- `src/orchestrator/` (per PRD #58 / #82) — orchestrator UI input handling, if the bug turns out to live there.
- `src/pane.rs` / send-prompt write path from PRD #24.
- `tests/` — wherever daemon-path send tests live (or a new test module if none).

## Risks and Mitigations

- **Risk**: The bug is genuinely non-deterministic (true race) and the reproducer never gets to 100%. Then the success criterion of "100 consecutive runs without failure" is the wrong bar.
  - *Mitigation*: If M1.2 cannot get above ~90% repro rate, escalate: pair the byte-level instrumentation with the actual fix attempt and verify by inspection of the byte stream rather than by statistical pass count. Update success criteria with reasoning.
- **Risk**: The fix on the daemon path conflicts with PRD #93's restructure.
  - *Mitigation*: Coordinate with PRD #93 — if the structural fix is small, land here first and PRD #93 carries it forward; if it's invasive, fold this PRD's fix into PRD #93's branch.
- **Risk**: Fix turns out to be in the orchestrator agent's *own* input handling (i.e. upstream code we don't own).
  - *Mitigation*: If that is the case, M2.2 becomes "send the byte sequence the upstream agent expects" rather than "patch the upstream". Document the dependency and the workaround.
- **Risk**: Investigation reveals other agents have the same latent bug and just haven't been reported.
  - *Mitigation*: Document and open a follow-up issue; do not expand this PRD's scope unilaterally.
