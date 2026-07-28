# PRD #225: Reliable prompt delivery to Wrapper-strategy agents on respawn

**Status**: Not started
**Priority**: High
**Created**: 2026-07-26
**GitHub Issue**: [#225](https://github.com/vfarcic/dot-agent-deck/issues/225)
**Related**: [PRD #234](234-screen-state-observation-hookless-agents.md) (screen-state observation — the truth source for the hookless-agent readiness case this PRD defers; see Open Question 2 and Risks)
**Feature flag**: None — this is a defect fix on a shipped surface (the delegate path), not a new user-visible surface. Rule 9 does not apply.

## Problem Statement

A `clear = true` delegate to a **Codex** worker silently loses its prompt. The worker respawns, comes up with an empty composer, and does nothing. The orchestrator believes it delegated; the operator sees a worker that restarted and then sat idle. In the dogfood `dot-agent-deck` orchestration the `tester` role is Codex, so **Codex is effectively unusable as an orchestration worker** — every delegation to it is dropped.

This was diagnosed live against a running daemon (pid 3115616) driving three worktree orchestrations. Two independent defects stack:

### Defect 1 — the readiness gate accepts a signal that does not mean "ready"

`dispatch_one_owned` (`src/state.rs:509-767`) does the right thing in the right order: respawn, wait for readiness, then inject the prompt. Readiness is `wait_for_session_start` (`src/state.rs:444-471`), which returns on the first `SessionStart` matching the pane and the **new** agent id, with a 10s timeout fallback.

For a wrapped agent that signal is a lie. `src/wrap.rs:1079` emits `EventType::SessionStart` immediately after `cmd.spawn()` returns — commented "the session has begun — surface the card immediately". It is emitted for the benefit of the *dashboard card*, at fork time, carrying the correct `pane_id` and `agent_id` from the environment. So it satisfies the gate exactly, and the 10s wait collapses to roughly zero.

Measured on the live system, for the PRD-140 tester pane (pane 21, agent 63):

| time | event |
|---|---|
| 23:42:36.03 | `worker-task-tester.md` written — the delegate reached the daemon |
| 23:42:39 | `dot-agent-deck wrap` forks `devbox run codex-big`; wrapper emits `SessionStart` |
| ~23:42:39.2 | gate satisfied; deck writes the prompt, waits `SUBMIT_DELAY` (150ms), writes CR |
| **23:42:43** | `node codex` finally starts — 4 seconds after the prompt was written |

The prompt lands in a PTY where only `devbox` is running. The line discipline (canonical mode, echo on) **echoes the text back**, which is why the operator reports "I saw it receive the prompt". Codex then boots, enters the alternate screen and clears it — which is what reads as "it restarted right after". There is no second restart; the real one happened earlier and was invisible because the pane still displayed the dead agent's frozen screen.

Codex's *native* `SessionStart` — the one that genuinely means the session is up — arrives seconds later and is ignored, because the gate already returned.

Only Codex is affected today: it is the only shipped Wrapper-strategy agent, so it is the only agent with a synthetic fork-time `SessionStart` racing a real one. Claude and OpenCode are unaffected because their only `SessionStart` comes from an initialized session.

### Defect 2 — the launch shape mutates across respawn

The pane does not start out wrapped. It becomes wrapped on its first delegate:

1. The role command is `devbox run codex-big`. `AgentType::from_command` tokenizes it, resolves basename `devbox`, and returns `None` (`src/event.rs:88-91`). With no agent type, `wrap_launch_command` is a no-op, so the **initial spawn is unwrapped**.
2. Codex's native hooks fire. The daemon learns the real type and records it: `src/daemon.rs:943` → `AgentPtyRegistry::set_agent_type` (`src/agent_pty.rs:2967`), an upgrade-only `None` → `Some(Codex)` write intended purely so `list_agents` reports the right badge after a reconnect.
3. The first `clear = true` delegate calls `respawn_agent_for_pane`, which replays the captured record — including that learned `agent_type: Some(Codex)` (`src/agent_pty.rs:2736`).
4. `spawn_agent_inner` now resolves `Codex` and wraps: `dot-agent-deck wrap --agent codex -- devbox run codex-big`.

So a value recorded for **display** silently changes the **exec line**, and the same pane runs a different process tree before and after its first delegate. This is what arms Defect 1, and it is a correctness problem in its own right — the respawn contract is supposed to reproduce the previous child's environment and geometry, not redefine how it launches.

Confirmed on the live system: pane 21 (post-delegate) runs `dot-agent-deck wrap --agent codex -- devbox run codex-big`, while panes 15 and 27 (never delegated to) still run a bare `devbox run codex-big`.

## Solution Overview

Two changes, both required. Fixing only Defect 1 leaves a pane that silently changes shape; fixing only Defect 2 leaves the race live for any correctly-detected Codex command (`command = "codex"` wraps from the start and hits the identical race).

1. **Make the readiness signal mean "the agent can accept input."** Distinguish the wrapper's fork-time card-surfacing event from a genuine session-ready signal, and have the delegate gate wait for the latter.
2. **Make the wrap decision stable for the life of a pane.** A pane that spawned unwrapped respawns unwrapped. A type learned from hook events updates the badge, never the exec line.

The user-visible outcome: delegating to a Codex worker delivers the prompt and the worker starts working, the same as every other agent.

## Scope

### In Scope

- Readiness: separate "card surfaced" from "session ready" for Wrapper-strategy agents, and gate delegate prompt injection on the latter.
- Wrap stability: the wrap decision is fixed at pane creation and survives respawn; `set_agent_type` no longer influences it.
- The same readiness gate is reused by `crate::spawn::spawn` (`src/state.rs:442`) for scheduled cards — that call site must be fixed or consciously exempted, not left inconsistent.
- Revisit `SESSION_START_WAIT_TIMEOUT` (currently 10s, `src/state.rs:44`) against measured Codex boot. On this machine the wrapper→`node codex` gap alone is 4s before Codex's own initialization; if the fallback ever has to carry a Codex pane, 10s is marginal.
- Tests per CLAUDE.md rule 4, including a **real-agent e2e** on a cheap model (Codex mini) that delegates to a `clear = true` Codex worker and asserts the worker acts on the prompt — a sentinel-file scenario, since a stand-in like `cat` cannot reproduce a race with a real TUI's boot sequence.
- Rule 12 cross-version contract check: this touches hooks, orchestration, and the daemon.

### Out of Scope

- **Whether a script-launched Codex should be wrapped at all.** Codex is a documented *hybrid* (`src/agent_registry.rs:219-239`): the wrapper is its PTY host, but its rich events come from native hooks. Panes 15 and 27 run unwrapped today and report status fine, which suggests the wrapper may be optional for Codex. Deciding that is a larger design question — see Open Questions.
- **Improving `AgentType::from_command` to see through `devbox run <script>`.** Tempting, but it generalizes badly to arbitrary launcher scripts and would only mask Defect 2 by making the initial spawn wrapped too. Explicitly not the fix.
- The phantom dashboard cards observed alongside this (Codex cards that vanish when selected). Not root-caused, not implicated in prompt loss. Should be filed separately.
- The five leaked e2e Codex processes (PPID 1, pointing at dead `/tmp/.tmp*/hook.sock` sockets, carrying a `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS=300` cap that never fired). Separate cleanup bug.

## Technical Approach

### Readiness

The wrapper's `SessionStart` has a legitimate purpose — surfacing the card immediately, so a slow-booting agent is not invisible. It should keep doing that. What must change is that the delegate gate stops treating it as proof of interactivity.

The likely shape is an origin marker on the emitted event (the `Emitter` in `src/wrap.rs` already builds the full `AgentEvent`, and `metadata` is an existing free-form map, so this is additive on the wire), with `wait_for_session_start` skipping marked events.

The skip **must be conditional**. For a Wrapper-strategy agent with no native hooks — Gemini, PRD #211 — the synthetic event is the only `SessionStart` that will ever arrive, and skipping it unconditionally regresses those agents to a full timeout on every delegate. The natural discriminator is whether the agent's registry spec has native hooks (`hook_install.is_some()`), which is true for Codex and false for a pure-wrapper agent. Confirm this against the registry rather than assuming it.

Wire compatibility is additive in both directions: an old wrapper sends no marker and a new daemon treats its event as it does today; a new wrapper's marker is ignored by an old daemon. That is a semantic no-op, not a `PROTOCOL_VERSION` bump — but rule 12 still requires the cross-version run to confirm it.

### Wrap stability

The clean invariant is that the launch shape is decided once, at pane creation, and replayed verbatim. Options to weigh:

- Record the wrap decision (or the resolved launch command) on `RunningAgent` at spawn and replay it on respawn, leaving `agent_type` purely descriptive.
- Split the field: a spawn-time type that drives launch, and an observed type that drives display. `set_agent_type` writes only the latter.

The second is probably cleaner but touches every reader of `RunningAgent.agent_type` (notably `list_agents` → `AgentRecord`). Whichever is chosen, the test that matters is: a pane spawned from a command that does not resolve to an agent type must produce an identical exec line before and after a `clear = true` delegate.

## Milestones

- [ ] **M1 — Failing tests pin both defects.** A test that asserts the delegate prompt reaches a wrapped agent only after it is genuinely ready, and a test that asserts the exec line is identical across a respawn for an undetected-type command. Both RED against current `main`.
- [ ] **M2 — Wrap decision is stable across respawn.** A pane's launch shape is fixed at creation; `set_agent_type` affects the badge only. M1's second test goes GREEN.
- [ ] **M3 — Readiness gate distinguishes card-surfacing from session-ready.** Conditional on the agent having native hooks, so pure-wrapper agents keep working. M1's first test goes GREEN.
- [ ] **M4 — Scheduler call site and timeout reconciled.** `crate::spawn::spawn`'s use of the same gate is fixed or consciously exempted, and `SESSION_START_WAIT_TIMEOUT` is set from measured boot times rather than inherited from the Claude-era tuning.
- [ ] **M5 — Real-agent e2e passes.** A cheap-model Codex worker under a `clear = true` role receives a delegate and demonstrably acts on it (sentinel file), in the pre-PR e2e tier per rule 5.
- [ ] **M6 — Cross-version contract check clean.** Per rule 12: branch TUI against a previous-release daemon, delegate still routes and hooks still arrive. Classification (`PROTOCOL_VERSION` bump vs. `.breaking.md` fragment vs. neither) recorded in the PR.
- [ ] **M7 — Docs and changelog.** `docs/develop/agent-adapters.md` documents the readiness contract a Wrapper-strategy adapter must satisfy — this is the trap the next adapter author would otherwise walk into. Changelog fragment added.

## Success Criteria

- Delegating to a `clear = true` Codex worker delivers the prompt and the worker starts working — verified with a real agent, not a stand-in.
- A pane's exec line is byte-identical before and after its first delegate.
- Claude, OpenCode, and Pi delegate behavior is unchanged (no regression in the existing dispatch tests).
- A Wrapper-strategy agent with no native hooks still receives its prompt without waiting out the full timeout.

## Risks

- **Over-fitting to Codex.** The readiness contract has to hold for the next wrapper agent too, or PRD #211 inherits this bug. Mitigated by making the discriminator a registry property, not a Codex special case, and by documenting the contract in M7. Note this is only *partially* mitigated: a hookless wrapper agent has no real `SessionStart` to wait for, so it keeps today's behaviour (or eats the full timeout) no matter how the discriminator is expressed. That residual gap is [PRD #234](234-screen-state-observation-hookless-agents.md)'s to close, and is a conscious hand-off rather than an oversight — but it means "PRD #211 does not inherit this bug" is not true until #234 lands.
- **The timeout fallback is load-bearing and untested.** If Codex's native hooks are not installed or not trusted, the gate falls through to the timeout. That path must still deliver a usable prompt, which is why M4 revisits the duration.
- **Cross-version behavior behind a stable wire.** The marker is additive, but an old wrapper against a new daemon reverts to today's racy behavior. That is acceptable (no worse than current), but it must be a conscious, documented outcome rather than a surprise.

## Open Questions

1. **Should Codex be wrapped at all when launched via a script?** It is a hybrid whose events come from native hooks, and unwrapped Codex panes work today. If the answer is "no", Defect 2's fix largely subsumes Defect 1 for Codex — but Defect 1 still has to be fixed for `command = "codex"` and for Gemini. Worth settling before M3.
2. **Is `hook_install.is_some()` the right discriminator** for "this agent will emit a real `SessionStart`", or does that need to be an explicit field on `AgentSpec`? An explicit field is more honest about intent and survives an agent that installs hooks for other reasons.

   **Choose this with [PRD #234](234-screen-state-observation-hookless-agents.md) in mind.** This PRD's fix is *provenance*-based — ignore the synthetic event, wait for the real one — which structurally requires a real one to exist, hence the conditional skip and the hookless hole below. #234 supplies the missing truth source for exactly that case (readiness observed from the screen: the agent has painted its input box, which a launcher script cannot fake). The discriminator chosen here should therefore express "**where does this agent's readiness signal come from**" rather than "does it install hooks", so #234's observed-readiness source can be added as a third answer without re-migrating every call site. An explicit `AgentSpec` field is the shape that survives that; `hook_install.is_some()` is the shape that does not.
3. **Does the scheduler path want the same semantics?** A scheduled card's prompt has the same delivery problem, but the failure mode and acceptable latency may differ from an interactive delegate.

## Work Log

### 2026-07-28 — Line references refreshed against `main`
[PRD #140](140-orchestration-session-partitioning.md) (#228, merged as `cb307ca`) rewrote ~450 lines of `src/state.rs`, invalidating most of the code pointers in the diagnosis below. Refreshed against `main`: `dispatch_one_owned` 562-710 → **509-767**, `wait_for_session_start` 389-415 → **444-471**, the `crate::spawn::spawn` gate 386-388 → **442**, `from_command` 148-222 → **88-91** (it now delegates to the registry, hence the collapse), `AgentPtyRegistry::set_agent_type` 2828 → **2967**, the respawn `agent_type` replay 2566 → **2736**, the Codex hybrid spec 226-238 → **219-239**. Verified unchanged: `SESSION_START_WAIT_TIMEOUT` (`src/state.rs:44`), the `set_agent_type` call site (`src/daemon.rs:943`), the synthetic emit (`src/wrap.rs:1079`).

Only the pointers moved — **both defects are unrevalidated against post-#140 `main`**. The diagnosis below stands as written for the code as of 2026-07-26; #140 touched `dispatch_one_owned` directly, so re-confirm the race still reproduces before starting M1 rather than assuming it.

### 2026-07-26 — Diagnosis
Root-caused live against daemon pid 3115616 without restarting it (three orchestrations were mid-flight). Evidence: process tree showing pane 21 wrapped vs. panes 15/27 unwrapped; process start times establishing the 4s fork→Codex gap; `worker-task-tester.md` mtime establishing that the delegate reached the daemon and only the injection was lost. No daemon restart, no code changes.
