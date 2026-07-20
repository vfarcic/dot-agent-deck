# PRD #211: Gemini CLI adapter (wrapper strategy)

**Status**: Draft
**Priority**: Low
**Created**: 2026-07-14
**GitHub Issue**: [#211](https://github.com/vfarcic/dot-agent-deck/issues/211)
**Origin**: Follow-up to [PRD #20](20-multi-agent-support.md) (multi-agent machinery + Codex adapter), created as one of its final tasks. This PRD reuses the wrapper strategy PRD #20 shipped and proved with Codex.

## Problem Statement

PRD #20 built the multi-agent machinery — the compiled-in agent registry + integration-strategy seam, the `dot-agent-deck wrap` stdout-wrapper strategy, per-agent badges and the `type:` filter, and the `live_target`/`send_result` protocol — and proved it end to end with **Codex** as the first wrapper-strategy agent. Google's **Gemini CLI** is another interactive coding CLI developers run in their own terminal with no unified view, and it fits the *same* wrapper mechanism Codex uses: it emits activity to stdout and has no native hook/plugin/extension surface the deck can install into.

Because the wrapper strategy already exists, adding Gemini is deliberately small: it is a registry entry, a Gemini-specific line-classification rule set, detection, and tests — **no new integration mechanism**. This PRD exists to land that thin adapter and to be the second proof that "reuse a shipped strategy" is genuinely a registry-entry-plus-release change, exactly as the adapter authoring guide (`docs/develop/agent-adapters.md`) claims.

## Solution Overview

Add **Gemini as a first-class, status-tracked agent** using the shipped `IntegrationStrategy::Wrapper`. A Gemini pane launches under `dot-agent-deck wrap --agent gemini -- gemini …` (the launch-command rewrite already fires for any Wrapper-strategy agent), its stdout/stderr are teed through a Gemini-specific `RuleSet` into `AgentEvent`s on the existing hook socket, and the dashboard renders it with a Gemini badge, a `type:gemini` filter, and the history-only liveness a wrapped session carries. Nothing about the wire, the daemon, or the wrapper runtime changes — Gemini plugs in as **data**.

### Architecture

```
Codex CLI    →  wrapper strategy  (PRD #20)   →  AgentEvent  →  daemon
Gemini CLI   →  wrapper strategy  (THIS PRD)  →  AgentEvent  →  daemon
```

## Scope

### In Scope
- **`AgentType::Gemini`** in `src/event.rs` (the enum variant; `from_command` keeps delegating basename→type to the registry).
- **A registry `AgentSpec` entry** in `src/agent_registry.rs`: label `Gemini`, detection basename `gemini`, default command `gemini`, `strategy: Some(IntegrationStrategy::Wrapper)`, and a distinct named-ANSI badge colour (not reused by Claude/OpenCode/Pi/Codex, never the neutral `DarkGray`).
- **A Gemini-specific `classify_line` `RuleSet`** in `src/wrap.rs`, selected by `ruleset_for` when the resolved agent is `Gemini`, mapping Gemini CLI output lines to Working / Error / Idle. Falls back to the `GENERIC` rules for anything unmatched.
- **`live_target = history-only`** for a wrapped Gemini session (`kind: Process`), same as Codex — the dashboard cannot inject live input into a wrapped child.
- **Tests** mirroring the Codex ladder: fast-tier registry/detection + wrapper rule-set classification, a synthetic PTY-attached e2e, a real-agent e2e on a **cheap Gemini model** against a uniquely-named fixture sentinel, and a `check_gemini_available` skip harness (plus credential import if needed).

### Out of Scope
- **Any new integration mechanism.** Gemini reuses `dot-agent-deck wrap` unchanged; if it turns out to need a non-wrapper mechanism, that is a different PRD.
- **Live input into a wrapped Gemini session** (it is history-only by construction).
- Feature parity with Claude/Pi cards — a wrapper card is intentionally sparser (basic Working/Idle/Error status).
- Installing, authenticating, or managing the Gemini CLI itself.
- Gemini-specific UI panels or detail views.

## Technical Approach

- **Registry entry drives everything derived.** Adding the `AgentSpec` + the `ALL` slice entry + the `spec()` arm makes detection, the `Display` label, the badge colour, the `type:gemini` filter alias (`resolve_type_alias`), the per-agent default command, and the launch-command wrapping (`wrap::wrap_launch_command`, which fires for any Wrapper-strategy agent) all light up with no further edits.
- **Rule set is the only per-agent code.** In `src/wrap.rs`, add `pub static GEMINI: RuleSet { error_markers, idle_markers }` and a `AgentType::Gemini => &GEMINI` arm in `ruleset_for`. Keep markers narrow and format-specific (matching structured discriminators where Gemini emits them, as the Codex JSONL rule set does) so incidental words never flip the card. The `Detector` debounce and the wrapper runtime are untouched.
- **Cross-version safety.** Gemini rides the existing raw-`AgentEvent` wire, so per [CLAUDE.md rule 12](../CLAUDE.md) / `docs/develop/versioning.md` there is **no `PROTOCOL_VERSION` bump and no `.breaking.md`** — a new `AgentType` variant is covered by the `#[serde(other)]` forward-compat catch-all, and no field meaning changes.

## Success Criteria

- A Gemini CLI session can be monitored in the dashboard end-to-end (detection → wrapped events → card status) via `dot-agent-deck wrap`.
- The Gemini pane shows a distinct coloured `Gemini` badge and is filterable with `type:gemini`.
- A wrapped Gemini session renders history-only (no live-input affordance), consistent with Codex.
- Claude Code, OpenCode, Pi, and Codex integrations continue to work unchanged — the existing test suite passes without edits.
- A real Gemini agent on a cheap model lists a directory and reports a fixture sentinel in a wrapped, PTY-attached e2e.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test-fast` pass; `cargo test-e2e` passes before the PR.
- The change is a registry entry + a rule set + detection + tests — no new mechanism — confirming the wrapper strategy is reusable as PRD #20 designed.

## Milestones

- [ ] `AgentType::Gemini` variant + registry `AgentSpec` entry (label, detection, default command, Wrapper strategy, badge colour); fast-tier registry/detection tests (`src/event.rs`, `src/agent_registry.rs`)
- [ ] Gemini `classify_line` `RuleSet` + `ruleset_for` arm, with fast-tier classification tests over realistic Gemini output (`src/wrap.rs`)
- [ ] `live_target = history-only` declared for wrapped Gemini sessions; badge + `type:gemini` filter verified (comes from the registry)
- [ ] Synthetic PTY-attached e2e (`e2e_*.rs`, `#[cfg(feature = "e2e")]`): a deterministic stand-in emits Gemini-shaped output; assert the event stream and the visible dashboard card
- [ ] Real-agent e2e on a cheap Gemini model against a fixture sentinel; `check_gemini_available` skip harness in `tests/common/mod.rs`
- [ ] All existing tests pass unchanged; docs/changelog note the new agent

## Key Files

- `src/event.rs` — `AgentType::Gemini` variant
- `src/agent_registry.rs` — the Gemini `AgentSpec` entry + `ALL` / `spec()`
- `src/wrap.rs` — the Gemini `RuleSet` + `ruleset_for` arm
- `tests/gemini_adapter.rs` (new) — fast-tier registry/detection + rule-set classification
- `tests/e2e_gemini_wrapper.rs` (new) — synthetic + real-agent PTY-attached e2e
- `tests/common/mod.rs` — `check_gemini_available` + optional credential import
- `docs/develop/agent-adapters.md` — the authoring guide this PRD follows

## Risks

- **Pattern-detection fragility.** Wrapper adapters parse stdout, which can break if Gemini changes its output format. Mitigated by keeping the rule set narrow and format-specific and relying on the `GENERIC` fallback (any non-blank line is activity) for unmatched output — basic status still flows.
- **Gemini output shape mismatch.** If Gemini's default output is not line-oriented enough for the tee to classify usefully, prefer a structured/JSON output mode (as Codex uses `--json`) so the rule set keys off stable discriminators.
- **CLI availability / auth.** Gemini has its own install and API-key requirements; the deck monitors but does not manage them, and the real-agent e2e skips cleanly when the CLI is absent or unauthenticated.
- **Scope creep.** The temptation to add Gemini-specific panels or richer parsing. Mitigated by the "thin adapter" framing — this PRD is a registry entry + rule set + tests, and anything larger is a separate PRD.
