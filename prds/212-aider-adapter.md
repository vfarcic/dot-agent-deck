# PRD #212: Aider adapter (log-watcher strategy)

**Status**: Draft
**Priority**: Low
**Created**: 2026-07-14
**GitHub Issue**: [#212](https://github.com/vfarcic/dot-agent-deck/issues/212)
**Origin**: Follow-up to [PRD #20](20-multi-agent-support.md) (multi-agent machinery + Codex adapter), created as one of its final tasks. Unlike the [Gemini adapter (PRD #211)](211-gemini-adapter.md), which reuses the shipped wrapper strategy, this PRD introduces a **new** integration strategy — so it carries a one-time mechanism implementation.

## Problem Statement

PRD #20 built the multi-agent machinery and shipped **three** integration strategies with agents behind each: native hooks (Claude), a plugin (OpenCode), a bundled extension (Pi), plus the **wrapper** strategy it added and proved with Codex. **Aider** is a widely-used AI pair-programming CLI, but it does not fit any of those: it has no hook/plugin/extension surface to install into, and stdout-wrapping is a poor fit because its terminal output is a rich, redrawing TUI rather than a clean line stream to classify. What Aider *does* have is **structured logs** — it can write a machine-readable record of its activity to a file.

That makes Aider the motivating case for the **fourth-and-a-half** mechanism PRD #20 named but deliberately deferred: a **log-watcher** strategy. This PRD carries the one-time implementation of that strategy (a `dot-agent-deck watch --agent aider --log <path>` command that tails a log and parses entries into `AgentEvent`s), then plugs Aider into it as the first consumer. Per the adapter authoring guide (`docs/develop/agent-adapters.md`), once the strategy exists, every subsequent log-watching agent is back on the cheap "registry entry + release" path.

**Important:** today's `Commands::Watch` (`src/main.rs`) is an **unrelated generic fixed-interval command runner** — it is not a log watcher. The log-watcher introduced here is a **separate new command / mechanism**, not a change to that verb.

## Solution Overview

Introduce a new `IntegrationStrategy` (log-watcher) and the `dot-agent-deck watch --agent <a> --log <path>` command that tails a structured log file, parses each entry, and emits `AgentEvent`s onto the **existing** daemon hook socket — no new wire. Then add **Aider as a first-class, status-tracked agent** whose registry entry names this strategy. The dashboard renders an Aider card with a badge, a `type:aider` filter, and the history-only/view-only liveness appropriate to a log-watched session (the deck observes Aider through its log, not a handle it can write to).

### Architecture

```
Codex CLI  →  wrapper strategy      (PRD #20)   →  AgentEvent  →  daemon
Gemini CLI →  wrapper strategy      (PRD #211)  →  AgentEvent  →  daemon
Aider      →  log-watcher strategy  (THIS PRD)  →  AgentEvent  →  daemon
```

## Scope

### In Scope
- **A new `IntegrationStrategy` (log-watcher)** in `src/agent_registry.rs` and a new module (e.g. `src/watch_agent.rs`) implementing the tail-and-parse runtime. This is the one-time mechanism cost this PRD carries.
- **`dot-agent-deck watch --agent aider --log <path>`** — a **new** CLI subcommand (distinct from the existing generic-interval `Commands::Watch`) that follows the log file, parses structured entries, and emits `AgentEvent`s over the existing hook socket.
- **`AgentType::Aider`** in `src/event.rs` + a registry `AgentSpec` entry: label `Aider`, detection basename `aider`, default command `aider`, `strategy: Some(IntegrationStrategy::LogWatcher)`, a distinct named-ANSI badge colour.
- **Aider log entry → `EventType` parsing** — map Aider's structured records to Working / Idle / Error / tool activity.
- **`live_target`** for a log-watched session: `kind: none` (the deck holds no writable handle) with `writable: history-only` (or `none`/view-only), so the UI never invites input it cannot deliver, and `send_result` returns an honest `no-live-target`/`history-only`.
- **Tests**: fast-tier registry/detection + log-parse unit tests (feed captured log lines, assert `EventType`s), a synthetic e2e feeding a fixture log file through `watch`, a real Aider e2e on a **cheap model** against a uniquely-named fixture sentinel, and a `check_aider_available` skip harness.

### Out of Scope
- **Retrofitting the other agents onto the log-watcher.** It exists for Aider (and future log-emitting agents); Claude/OpenCode/Pi/Codex/Gemini keep their own strategies.
- **Live input into an Aider session** — a log-watched session is observe-only from the deck's side.
- **Changing the existing generic `Commands::Watch` interval-runner** — the log watcher is a separate command.
- Feature parity with Claude/Pi cards — a log-watched card exposes what the log carries, no more.
- Installing, authenticating, or managing Aider itself.

## Technical Approach

- **The log-watcher runtime.** A new module tails the log file (follow/`tail -f` semantics, resilient to rotation/truncation and to the file not existing yet), parses each complete entry, maps it to an `EventType`, and builds an `AgentEvent` (agent type `Aider`, `session_id` derived from the pane or the log path, `live_target` stamped) sent via `crate::hook::send_to_socket` — the same raw-`AgentEvent` path every other producer uses. Send failures are ignored so `watch` stays a transparent observer even with no daemon.
- **Parsing seam kept as data where possible.** Mirror the wrapper's split: a pure, testable `parse_log_line`/`classify_entry` function plus a small per-format rule/parser, so the mechanism is generic and Aider is (as much as possible) a data/format add. How far the parse rules live in data vs. code is the same "open design dial" PRD #20 left to implementation time.
- **Registry entry drives the derived surface.** As with every agent, the `AgentSpec` + `ALL` + `spec()` change lights up detection, the `Display` label, the badge colour, and the `type:aider` filter with no other edits. The startup auto-install dispatch (`src/main.rs`) skips the log-watcher (like Wrapper, it has no install step); the `watch` command is launched explicitly / via the pane command.
- **Cross-version safety.** Events ride the existing raw-`AgentEvent` wire and the new `AgentType` variant is covered by the `#[serde(other)]` forward-compat catch-all, so per [CLAUDE.md rule 12](../CLAUDE.md) / `docs/develop/versioning.md` there is **no `PROTOCOL_VERSION` bump and no `.breaking.md`** unless the mechanism ends up touching the attach protocol (it should not).

## Success Criteria

- The log-watcher strategy exists as a reusable `IntegrationStrategy` + `dot-agent-deck watch --agent <a> --log <path>` command, cleanly separate from the pre-existing generic interval `Commands::Watch`.
- An Aider session can be monitored in the dashboard end-to-end (log entries → parsed events → card status) with no change to Aider itself beyond pointing `watch` at its log.
- The Aider pane shows a distinct coloured `Aider` badge and is filterable with `type:aider`.
- A log-watched Aider session renders view-only / history-only (no live-input affordance), and an input attempt returns an honest `send_result`.
- Claude Code, OpenCode, Pi, Codex, and Gemini integrations continue to work unchanged — the existing test suite passes without edits.
- A real Aider agent on a cheap model performs a directory listing whose structured log the watcher parses, surfacing a fixture sentinel, in a PTY-attached e2e.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test-fast` pass; `cargo test-e2e` passes before the PR.

## Milestones

- [ ] New `IntegrationStrategy::LogWatcher` variant + the log-watcher runtime module (tail, parse, emit) with fast-tier unit tests for tailing and parsing (`src/agent_registry.rs`, new `src/watch_agent.rs`)
- [ ] `dot-agent-deck watch --agent <a> --log <path>` CLI subcommand wired to the runtime, distinct from the generic-interval `Commands::Watch` (`src/main.rs`)
- [ ] `AgentType::Aider` variant + registry `AgentSpec` entry (label, detection, default command, LogWatcher strategy, badge colour); fast-tier registry/detection tests (`src/event.rs`, `src/agent_registry.rs`)
- [ ] Aider log entry → `EventType` parsing with fast-tier tests over captured log samples
- [ ] `live_target` (view-only / history-only) declared for log-watched sessions; badge + `type:aider` filter verified (comes from the registry)
- [ ] Synthetic e2e (`e2e_*.rs`, `#[cfg(feature = "e2e")]`): a fixture log file is tailed through `watch`; assert the event stream and the visible dashboard card
- [ ] Real Aider e2e on a cheap model against a fixture sentinel; `check_aider_available` skip harness in `tests/common/mod.rs`
- [ ] All existing tests pass unchanged; docs/changelog note the new agent and the new strategy

## Key Files

- `src/agent_registry.rs` — `IntegrationStrategy::LogWatcher` variant + the Aider `AgentSpec` entry
- `src/watch_agent.rs` (new) — the log-watcher runtime (tail, parse, emit `AgentEvent`s)
- `src/main.rs` — the new `watch --agent --log` subcommand (separate from the generic `Commands::Watch`)
- `src/event.rs` — `AgentType::Aider` variant
- `tests/aider_adapter.rs` (new) — fast-tier registry/detection + log-parse unit tests
- `tests/e2e_aider_watch.rs` (new) — synthetic + real-agent PTY-attached e2e
- `tests/common/mod.rs` — `check_aider_available` + optional credential import
- `docs/develop/agent-adapters.md` — the authoring guide this PRD follows (and extends with the log-watcher as the "genuinely new mechanism" case)

## Risks

- **Log-format fragility.** Parsing structured logs breaks if Aider changes its schema. Mitigated by pinning the tested Aider version in docs, keeping the parser tolerant (unknown entries → no event, never a crash), and asserting a captured-log corpus in fast tests.
- **Tailing edge cases.** Log rotation, truncation, partial lines, and a not-yet-created file are real failure modes for a file follower. Mitigated by designing the tailer for them from the start and covering them in unit tests.
- **New-mechanism scope.** A log-watcher is a genuinely new strategy, so this PRD is larger than the Gemini adapter. Mitigated by keeping the runtime generic and Aider a format/data add, and by resisting parity features — basic status is the bar.
- **Aider availability / auth.** Aider has its own install and API-key requirements; the deck observes but does not manage them, and the real-agent e2e skips cleanly when Aider is absent or unauthenticated.
- **Structured-log availability.** If Aider's log detail is too sparse to distinguish Working/Idle/Error reliably, the card degrades to coarse active/idle status — acceptable, and consistent with the wrapper agents' sparser cards.
