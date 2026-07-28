# PRD #234: Screen-state observation for hookless agents

**Status**: Not started
**Priority**: Medium
**Created**: 2026-07-28
**GitHub Issue**: [#234](https://github.com/vfarcic/dot-agent-deck/issues/234)
**Related**: [PRD #20](20-multi-agent-support.md) (the integration-strategy seam this adds a mechanism to), [PRD #225](225-wrapper-agent-readiness-and-stable-launch-shape.md) (fixes the readiness race for Codex via native hooks; this PRD covers the hookless case #225 defers), [PRD #211](211-gemini-adapter.md) (first consumer — likely replaces its planned per-agent rule set), [PRD #212](212-aider-adapter.md) (whose log-watcher premise this PRD re-tests)
**Feature flag**: None. Rule 9 applies to a new user-visible *surface* (pane, field, command, tab, footer, keybinding); this adds none — it improves the fidelity of status already rendered on shipped cards. A wrapper card that previously sat on a stale status will start reporting `Idle` correctly, which is a defect-class improvement, not a new surface to gate.
**Prior art**: [`coder/agentapi`](https://github.com/coder/agentapi) (MIT), `lib/screentracker` and `lib/msgfmt/agent_readiness.go`. Technique only — see "Relationship to agentapi" for why we are not adopting the binary.

## Problem Statement

The deck learns what an agent is doing through one of four shipped [integration strategies](../docs/develop/agent-adapters.md). Three of them — `NativeHooks`, `Plugin`, `Extension` — install into a surface the agent provides, and deliver the full twelve-variant [`EventType`](../src/event.rs) stream. The fourth, `Wrapper`, is the fallback for agents that provide no such surface, and it works by **classifying stdout lines** (`classify_line` / `RuleSet` in [`src/wrap.rs`](../src/wrap.rs)).

Line classification cannot read a redrawing TUI. There are no meaningful "lines" in a stream of cursor-positioning and repaint escapes, only a wall of redraw text. The code already concedes this: the `GENERIC` rule set ships `idle_markers: &[]` with the comment that mid-session idleness is "left to process-exit quiescence rather than guessed from a single line." The practical consequence is that **a wrapped agent with no agent-specific rule set never reports `Idle` while it is running** — its card holds whatever state the last matched line produced until the process exits. Codex escapes this only because it is a hybrid whose real events come from native hooks; the `CODEX` rule set is explicitly retained as a coarse fallback, not as the mechanism.

Two open PRDs walk straight into this:

- **[PRD #211](211-gemini-adapter.md) (Gemini)** plans a Gemini-specific `classify_line` rule set. Gemini CLI is a redrawing TUI with no hook surface, so that rule set has to guess status from repaint text — the same guess `GENERIC` declines to make. It will be brittle in exactly the way the [adapter guide](../docs/develop/agent-adapters.md) warns about, and it will need re-tuning every time Gemini changes its interface.
- **[PRD #225](225-wrapper-agent-readiness-and-stable-launch-shape.md) (wrapper readiness)** fixes a delegate prompt being injected before the agent exists. Its fix is a *provenance* marker: tag the wrapper's synthetic fork-time `SessionStart` ([`src/wrap.rs:1079`](../src/wrap.rs)) so `wait_for_session_start` ([`src/state.rs:444-471`](../src/state.rs)) skips it and waits for the agent's real one. That works only when a real one exists. #225 states the limitation directly — for a wrapper agent with no native hooks the synthetic event is the only `SessionStart` that will ever arrive, so the skip has to be conditional on `hook_install.is_some()`, and hookless agents keep the racy behaviour or eat the full `SESSION_START_WAIT_TIMEOUT`. #225's Risk #1 ("over-fitting to Codex") is this hole.

Both problems have the same root: **for a hookless agent the deck has no truth source about the agent's state other than the bytes on the screen — and it currently never looks at the screen.** It looks at lines. The `vt100` parser that renders those bytes into a grid runs **client-side**, in the TUI's terminal widget; the daemon and the wrapper only ever handle a byte stream, and `src/wrap.rs` only ever handles lines.

## Solution Overview

Render the screen where the bytes already are, and derive state from **whether the rendered grid is changing** rather than from what any individual line says.

This is a fifth integration mechanism in the sense that matters — a new way for agent state to reach the deck — but it is deliberately *not* a new `AgentEvent` producer competing with the existing ones. It produces the same [`AgentEvent`](../src/event.rs)s on the same socket, exactly as every other strategy does. What it adds is two marker-free predicates over a screen:

- **`is_stable`** — the rendered grid has not changed for a configured quiet period. Drives `Idle`.
- **`is_ready`** — the agent has painted its input box. Drives the readiness gate that #225 needs and cannot supply for hookless agents.

Both are **format-independent**: they need no per-agent markers, no knowledge of the agent's output vocabulary, and no re-tuning when an agent changes its wording. That is the entire point — it is the opposite trade from a `RuleSet`.

### Architecture

```
Claude / OpenCode / Pi   →  hooks / plugin / extension  →  AgentEvent  →  daemon   (rich, 12 EventTypes)
Codex                    →  native hooks under wrapper  →  AgentEvent  →  daemon   (rich; RuleSet as fallback)
Gemini (#211)            →  screen-state observation    →  AgentEvent  →  daemon   (THIS PRD: stable / ready)
Aider  (#212)            →  log-watcher (+ screen?)     →  AgentEvent  →  daemon   (see Open Question 2)
```

## Scope

### In Scope

- **A screen-state observer**, daemon-side, that maintains a `vt100` screen per agent PTY and exposes a cheap content hash plus a `last_changed` timestamp.
- **`is_stable(quiet_period)`** — a stability predicate derived from `last_changed`, with an explicit *initializing* state distinct from *stable* so a not-yet-observed agent is never mistaken for an idle one.
- **`is_ready()`** — an input-box-presence predicate, used strictly as a **gate**, never for content extraction (see Technical Approach).
- **Idle events for hookless wrapper agents** — the observer emits `EventType::Idle` on the existing raw-`AgentEvent` socket when a session goes stable, closing the "never reports Idle mid-session" gap.
- **A readiness source for [PRD #225](225-wrapper-agent-readiness-and-stable-launch-shape.md)'s hookless case** — `wait_for_session_start`'s gate can consult observed readiness for agents whose registry spec has no native hooks, instead of falling through to the timeout.
- **A registry-level way to declare that an agent uses screen observation**, consistent with how `IntegrationStrategy` already drives dispatch from [`src/agent_registry.rs`](../src/agent_registry.rs).
- **Tests per rule 4**: fast-tier unit tests over the predicates (feed captured byte streams, assert stable/ready transitions and the initializing→stable boundary), an L2 PTY-attached e2e, and a real-agent e2e on a cheap model.
- **Rule 12 cross-version contract check** — this touches the daemon and the orchestration readiness gate.
- **Docs** — a new strategy section in [`docs/develop/agent-adapters.md`](../docs/develop/agent-adapters.md), including when an adapter author should reach for screen observation vs. a `RuleSet`.

### Out of Scope

- **Adopting `coder/agentapi` as a dependency.** Technique only. See "Relationship to agentapi".
- **Message extraction / turning the screen into a chat transcript.** Explicitly rejected — see Technical Approach.
- **Replacing hook-based status for Claude, OpenCode, Pi, or Codex.** Hooks give twelve event types with tool-level detail; screen observation gives two predicates. Where hooks exist they win, unconditionally. This mechanism is the *fallback path*, and nothing about the hook path changes.
- **Rich detail from the screen** (tool names, prompts, token counts). A stable/ready pair is the whole contract.
- **Shipping the Gemini adapter itself** — that stays [PRD #211](211-gemini-adapter.md); this PRD makes its approach viable and should land first.
- **Deciding Aider's mechanism** — [PRD #212](212-aider-adapter.md) keeps that call; this PRD only re-tests its premise (Open Question 2).

## Technical Approach

### Where the screen lives

`vt100` is already a dependency, but it is parsed **client-side** in the TUI's terminal widget — the daemon holds PTY bytes and never renders them. This PRD's one genuinely new piece of machinery is a screen rendered **daemon-side**, in [`src/agent_pty.rs`](../src/agent_pty.rs), fed from the same byte stream that already flows to `KIND_STREAM_OUT` subscribers.

Daemon-side is preferred over wrapper-side because it covers **every** strategy from one place — including a future hookless agent that is not wrapped at all — and because the daemon already owns PTY lifecycle, dimensions, and the broadcast bus. Wrapper-side would only ever cover `Wrapper`, and would put a second screen renderer in a second process. This is the main cost of the PRD and should be validated in M1 before the predicates are built on top.

### Stability detection

The reference implementation (agentapi, `lib/screentracker/pty_conversation.go`) polls the screen every `SnapshotInterval = 25ms` into a ring buffer sized `ceil(ScreenStabilityLength / SnapshotInterval) + 1` — 81 entries at their `ScreenStabilityLength = 2s` — and calls the screen stable when the buffer is **full** and every entry is byte-identical to the first:

```go
func (c *PTYConversation) isScreenStableLocked() bool {
	snapshots := c.snapshotBuffer.GetAll()
	if len(snapshots) < c.stableSnapshotsThreshold { return false }
	for i := 1; i < len(snapshots); i++ {
		if snapshots[0].screen != snapshots[i].screen { return false }
	}
	return true
}
```

The design detail worth copying is the **third state**: a partially-filled buffer reports `Initializing`, never `Stable`, so a freshly-spawned agent that has not yet painted anything cannot be read as idle. Our equivalent must preserve that distinction — this is the same class of bug as #225's "forked" being mistaken for "ready".

Two deliberate deviations:

- **Hash, don't store.** They keep N full screen copies per agent. We keep a `u64` content hash plus a `last_changed: Instant`; stable ⟺ `now - last_changed >= quiet_period` and the agent has been observed at least once. This collapses the ring buffer entirely and makes the memory cost per agent constant.
- **Debounce on bytes, don't poll.** They tick at 25ms because at their layer they do not own the byte stream. We do — the daemon is already woken by PTY reads. Re-render and re-hash on byte arrival, then arm a single timer for the quiet period. No per-pane ticker, no idle CPU multiplied across a deck of panes.

The quiet period (their 2s) is a tuning parameter to establish empirically in M2, not a constant to copy. It trades false-idle (too short — a thinking agent between output bursts looks done) against latency (too long — the card lags reality).

### Readiness detection

The reference predicate is the whole of `lib/msgfmt/agent_readiness.go`:

```go
func isGenericAgentReadyForInitialPrompt(message string) bool {
	message = trimEmptyLines(message)
	messageWithoutInputBox := removeMessageBox(message)
	return len(messageWithoutInputBox) != len(message)
}
```

If stripping the composer changes the length, a composer exists, so the agent is up and accepting input. Per-agent variants exist there for Codex, OpenCode, and Amp; everything else uses the generic one.

Crucially, this observes **the agent itself**. It cannot be satisfied by a launcher script — which is precisely #225's failure mode, where the prompt was written into a PTY running only `devbox` (canonical mode, echo on, so the text echoed back and *looked* delivered) four seconds before `node codex` started.

**Box detection is fragile, and that is acceptable here — but only here.** It is per-agent and per-version, the same TUI-scraping treadmill this PRD otherwise avoids. What makes it tolerable for a *gate* is the failure mode: a missed match degrades to the existing `SESSION_START_WAIT_TIMEOUT` fallback ([`src/state.rs:44`](../src/state.rs)), which is exactly today's behaviour — strictly no worse. The asymmetry to hold the line on: **never** use the same detection for content extraction, where a bad strip silently corrupts what the user reads.

Combining both predicates gives a stronger gate than either alone: **ready ⟺ the screen is stable AND an input box is present.** A booting agent mid-repaint is neither.

### What we are deliberately not taking

`screentracker/diff.go` — the message extraction that turns screen diffs into a chat transcript. It finds the first line absent from the previous screen, takes everything after it, and special-cases OpenCode by skipping two header lines because "token count, context percentage, cost… change between screens." That is the treadmill in its pure form, and their commit log is the evidence: paste-echo workarounds pinned to Claude Code 0.2.70, "prevent terminal echo from being captured as agent messages", "make writeStabilize Phase 1 non-fatal when agents don't echo input". We have no use for a transcript — the deck shows the real terminal.

### Relationship to agentapi

[`coder/agentapi`](https://github.com/coder/agentapi) (Go, MIT) solves an adjacent problem — exposing one agent as an HTTP chat API — and was evaluated as a possible dependency. It was rejected: it wants to own the PTY and run its own terminal emulator, which would nest a second emulator under ours and endanger the live pane that is the product; it would add a non-Rust binary per pane with an HTTP port each, against the single-binary model [`src/remote.rs`](../src/remote.rs) ssh-installs; and its two-state `stable`/`running` status is a large downgrade from hook-sourced events for the agents that have hooks. Its screen-observation *technique*, however, is directly applicable to the agents that do not — which is this PRD. MIT licensing makes reading and porting the algorithm unencumbered.

### Cross-version safety

Idle events ride the existing raw-`AgentEvent` wire with no new fields, so per [rule 12](../CLAUDE.md) / [versioning.md](../docs/develop/versioning.md) no `PROTOCOL_VERSION` bump is expected. The readiness gate change is daemon-internal. Both still require the M6 cross-version run to confirm, since this touches the daemon and orchestration.

## Success Criteria

- A hookless wrapper agent reports `Idle` when it stops working, mid-session, with **no agent-specific markers configured** — the gap `GENERIC`'s empty `idle_markers` leaves today.
- A freshly-spawned agent is never reported `Idle` before it has painted anything (the initializing/stable distinction holds under test).
- A delegate to a hookless wrapper agent is gated on observed readiness and delivers its prompt, rather than racing the fork or waiting out `SESSION_START_WAIT_TIMEOUT`.
- Claude, OpenCode, Pi, and Codex status behaviour is **byte-identical** to before — the existing test suite passes unedited, and no hook-fed card changes what it reports.
- Steady-state CPU with N idle panes shows no regression attributable to screen tracking (the event-driven design's justification).
- [PRD #211](211-gemini-adapter.md) can be implemented against this mechanism without a `classify_line` rule set, demonstrated by a real Gemini agent on a cheap model reaching a correct `Idle`.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test-fast` pass; `cargo test-e2e` passes before the PR.

## Milestones

- [ ] **M1 — Daemon-side screen exists.** A `vt100` screen per agent PTY in [`src/agent_pty.rs`](../src/agent_pty.rs), fed from the existing byte path, with a content hash and `last_changed`. Validated for correctness against the client-side render and for cost under a multi-pane deck. This is the load-bearing piece — if daemon-side proves wrong, reconsider before building on it.
- [ ] **M2 — `is_stable` predicate + quiet-period tuning.** Event-driven debounce, explicit initializing state, quiet period established from measured agent behaviour rather than copied. Fast-tier tests feed captured byte streams and assert transitions.
- [ ] **M3 — Idle events for hookless wrapper sessions.** The observer emits `EventType::Idle` on the existing socket; a wrapped agent with no rule set reaches `Idle` mid-session. Hook-fed agents are untouched — asserted, not assumed.
- [ ] **M4 — `is_ready` predicate + readiness gate integration.** Input-box presence, combined with stability, consulted by the delegate gate for agents whose registry spec has no native hooks. Coordinated with [PRD #225](225-wrapper-agent-readiness-and-stable-launch-shape.md)'s discriminator so the two do not implement competing notions of readiness.
- [ ] **M5 — Registry + adapter-guide integration.** An agent declares screen observation through [`src/agent_registry.rs`](../src/agent_registry.rs) the way other strategies are declared; [`docs/develop/agent-adapters.md`](../docs/develop/agent-adapters.md) gains the strategy section and the "screen observation vs. `RuleSet`" guidance.
- [ ] **M6 — Real-agent e2e + cross-version check.** A PTY-attached e2e with a real agent on a cheap model reaching a correct `Idle` through screen observation alone (rule 4's user-visible bar), plus the rule 12 cross-version run with the classification recorded in the PR.
- [ ] **M7 — Changelog fragment.**

## Risks

- **Daemon-side rendering cost.** A `vt100` screen per agent, updated on every byte burst, across a deck of panes. Mitigated by hashing rather than storing, by event-driven updates rather than a 25ms ticker, and by making M1 measure this before anything is built on top. If it does not hold, wrapper-side is the fallback position (narrower coverage, same predicates).
- **Quiet-period tuning is a false-idle/latency trade with no universally right answer.** Too short and a thinking agent between output bursts reads as done; too long and the card lags. Per-agent override may be needed — resist making it the default, since marker-freeness is the point.
- **Input-box detection is per-agent and per-version.** Accepted deliberately, and only for gating, where the failure mode degrades to today's timeout. The standing rule (M5 docs) is that it must never be reused for content.
- **Two notions of readiness.** [PRD #225](225-wrapper-agent-readiness-and-stable-launch-shape.md) ships a provenance-based gate before this lands. If M4 is not coordinated with #225's discriminator choice, the codebase ends up with two competing readiness concepts. Mitigated by #225 choosing its discriminator with this successor in mind — see the note added to its Open Question 2.
- **Screen observation is genuinely weaker than hooks.** Two predicates against twelve event types. The temptation, once this exists, will be to lean on it for agents that have hooks because it is uniform. Do not — it is the fallback path, and the scope section says so for a reason.

## Open Questions

1. **Daemon-side or wrapper-side?** Daemon-side covers every strategy and centralises the renderer; wrapper-side is a smaller change but only ever serves `Wrapper` agents and duplicates rendering into a second process. M1 is scheduled to settle this with measurements rather than argument.
2. **Does this subsume [PRD #212](212-aider-adapter.md)'s log-watcher, or complement it?** #212's motivating premise is that "stdout-wrapping is a poor fit because [Aider's] terminal output is a rich, redrawing TUI rather than a clean line stream to classify" — an objection screen observation answers directly. The likely answer is *complement* (screen gives status with zero per-agent code; the log gives tool-level detail a screen cannot), but the premise should be re-tested before paying for a second mechanism.
3. **Does [PRD #211](211-gemini-adapter.md) still need a `RuleSet` at all?** If screen observation covers Gemini's status, #211 collapses to a registry entry plus detection — a smaller PRD than currently drafted, and a cleaner proof of the "reuse a shipped strategy" claim. #211's scope should be revisited once M3 lands.
4. **Is `is_ready` worth generalising beyond the delegate gate?** The scheduler path ([`crate::spawn::spawn`](../src/state.rs)) has the same prompt-delivery problem, and #225 flags it as needing a decision too. Both PRDs should reach the same answer.

## Work Log

### 2026-07-28 — Created
Originated from an evaluation of [`coder/agentapi`](https://github.com/coder/agentapi) as a possible dependency. Adoption was rejected (nested terminal emulators, a second non-Rust binary per pane, and a two-state status model that would regress every hook-fed agent), but two techniques from its `lib/screentracker` and `lib/msgfmt` were identified as directly applicable to the hookless agents the deck currently serves worst — and as the missing truth source for the readiness case [PRD #225](225-wrapper-agent-readiness-and-stable-launch-shape.md) explicitly defers. No code written.
