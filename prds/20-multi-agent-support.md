# PRD #20: Multi-agent machinery + Codex adapter

**Status**: Draft (rescoped 2026-06-22 — machinery + Codex only; Gemini/Aider split to follow-up PRDs)
**Priority**: Low
**Created**: 2026-03-31
**GitHub Issue**: [#20](https://github.com/vfarcic/dot-agent-deck/issues/20)

## Rescope (2026-06-22)

This PRD is narrowed to **the multi-agent machinery plus Codex as its first proving agent.** Gemini and Aider are removed from active scope; they become their own follow-up PRDs, created as the final tasks of this one once the machinery and Codex have shipped and proven the pattern.

Rationale: the foundation — the compiled-in agent registry + integration-strategy seam, the `dot-agent-deck wrap` stdout-wrapper, agent badges/filtering, and the `live_target`/`send_result` protocol — is shared by every future agent, but it can only be designed and validated against a concrete first consumer. Codex is the natural one: it is the first *wrapper-based* agent, and a wrapped session is also the first place the `live_target`/`send_result` distinction actually bites (a wrapped Codex session may be history-only, not a live PTY target the way a Claude Code pane is). Building the machinery and Codex together — then spinning Gemini (which reuses the wrapper strategy) and Aider (which needs a new log-watcher strategy) out as their own PRDs — keeps each PRD shippable and honestly scoped, instead of one open-ended "support every agent" epic.

## Validation refresh + decision (2026-06-14)

Re-validated against current code and re-scoped after a design discussion. Corrections to the original draft:

- **OpenCode already shipped (PRD #30) — the dashboard is no longer "Claude only."** Agents are modeled today as a closed enum `AgentType { ClaudeCode, OpenCode, None }` (`src/event.rs`) with command-basename inference (`AgentType::from_command`). The real gap is Codex/Gemini/Aider "and beyond," not multi-agent from zero.
- **Design decided: a curated, compiled-in agent registry + a small set of integration "strategies" — NOT a free-form string for runtime extensibility.** Because every change ships in a release anyway, requiring a recompile to add an agent is acceptable; the goal is **maintainability and separation of concerns for maintainers**, not letting end users add agents without a release. Runtime/user extensibility is an explicit **non-goal**, so the original "make `agent_type` a free-form string so new agents need no code changes" requirement is **dropped** — a typed identity keyed into the registry is fine.
- **What the registry replaces:** each agent's data (label, badge colour, detection pattern, default command, which integration strategy it uses) moves out of scattered `match AgentType` arms (detection in `from_command`, colours/labels in `src/ui.rs`, hook/plugin install dispatch) into **one cohesive registry entry per agent**. The agent identity stays typed; the win is centralisation, not destructuring.
- **Integration strategies are the code seam.** Events reach the deck by different mechanisms per agent: native hooks (Claude Code, `src/hooks_manage.rs`), a plugin (OpenCode, `src/opencode_manage.rs`), stdout-wrapping (`dot-agent-deck wrap` — Codex/Gemini), or log-watching (Aider). The two shipped agents already use two different mechanisms, which is why this layer is inherently code. Define a small finite set of strategies; each registry entry names one. Adding an agent that reuses an existing strategy = a registry entry (+ release); adding a genuinely new mechanism = a one-time strategy implementation, then config thereafter.
- **Open design dial:** how far to push strategy parameters (wrapper regexes, log-parse rules) into registry data vs. code is left as an implementation-time design decision.
- **Still genuinely future work:** `dot-agent-deck wrap` and `watch --agent` do not exist yet (`Commands::Watch` is currently a generic interval-runner, not a log watcher), and none of the `live_target`/`send_result` protocol fields are implemented — consistent with Status: Draft.
- **Testing strategy (the registry move is behaviour-preserving for shipped agents):** the enum→registry refactor must not change observable behaviour for Claude Code or OpenCode, so the **existing test suite must pass unchanged** — that is the regression proof that detection (`from_command`), card rendering, and hook/plugin install for the two shipped agents still work after centralisation. Do **not** edit existing tests to make them pass; if they need changing, the refactor changed behaviour and that's a bug. **New coverage** is additive: L1/unit tests for the registry lookups and strategy dispatch, and **new L2 e2e tests** (`e2e_*.rs`, gated by `#[cfg(feature = "e2e")]`, per CLAUDE.md rules 4–5) for Codex, exercising detection → events → dashboard status end-to-end. Run `cargo test-e2e` before the PR.

## Problem Statement

The dashboard supports Claude Code and OpenCode, but each was wired in by hand: detection, badge colour/label, and the install/event path are scattered across `match AgentType` arms in `src/event.rs`, `src/ui.rs`, `src/hooks_manage.rs`, and `src/opencode_manage.rs`. Adding the next agent means touching all of those sites again, and there is no shared seam for the *kinds* of integration an agent can use. Meanwhile developers increasingly run other AI coding tools — OpenAI Codex CLI, Google Gemini CLI, Aider — each in its own terminal with no unified view.

This PRD builds the **machinery** that makes adding an agent a cohesive, one-place change, and proves it by landing the first new agent — **Codex** — end to end. Gemini and Aider are deliberately out of scope here (see Rescope) and become follow-up PRDs.

## Solution Overview

Centralise per-agent data into a compiled-in **registry**, route events through a small finite set of integration **strategies**, stabilise the `AgentEvent` protocol (including a `live_target`/`send_result` contract for sessions that aren't live PTY targets), give the dashboard per-agent badges and a type filter, and add the **stdout-wrapper strategy** (`dot-agent-deck wrap`) with **Codex** as its first consumer.

### Architecture

```
Claude Code  →  native-hooks strategy (shipped)      →  AgentEvent  →  daemon
OpenCode     →  plugin strategy       (shipped)      →  AgentEvent  →  daemon
Codex CLI    →  wrapper strategy      (THIS PRD)      →  AgentEvent  →  daemon
Gemini CLI   →  wrapper strategy      (future PRD)    →  AgentEvent  →  daemon
Aider        →  log-watcher strategy  (future PRD)    →  AgentEvent  →  daemon
```

## Scope

### In Scope
- **Agent registry + strategy seam.** Move per-agent data (label, badge colour, detection pattern, default command, integration strategy) into one registry entry per agent. Behaviour-preserving for Claude Code and OpenCode.
- **Protocol stabilization (`src/event.rs`).** Document the `AgentEvent` JSON schema as a stable public API; add `agent_version: Option<String>` and a protocol version field; add `live_target: Option<LiveTarget>` (see Liveness & Write Semantics).
- **`dot-agent-deck wrap <agent-command>`** — the generic stdout-wrapper strategy that intercepts stdio to generate events. Codex is its first consumer.
- **Live-target / send-result semantics** — a per-session descriptor of whether/how the session can receive input, and an honest send-result status when input is delivered.
- **Agent-type visual distinction** in the dashboard (coloured badges) and **agent-type filtering** (`type:codex` in `/` search).
- **Codex CLI adapter, end-to-end** — registry entry, stdout pattern detection, detection → events → dashboard status, with new L2 e2e coverage.
- **Adapter authoring guide** — so the follow-up Gemini/Aider PRDs build against a documented seam.

### Out of Scope
- **Gemini adapter** — wrapper strategy, reuses `dot-agent-deck wrap`. Split to its own follow-up PRD (a final task here).
- **Aider adapter** and the **log-watcher strategy** (`watch --agent`, log tailing/parsing) — split to its own follow-up PRD (a final task here).
- Feature parity across agents (each exposes different levels of detail).
- Agent-specific UI panels or detail views.
- Installing or managing the agent tools themselves.
- Permission control for non-Claude agents (PRD #18 is Claude-specific).
- **Proof-of-consumption machinery** — runtime "generation" tracking and input/output cursor diffing to *prove* a specific keystroke was consumed by the live process. The lightweight `live_target` + `send_result` model below is enough; cursor/generation proofs are a future hardening step, not a milestone here.

## Technical Approach

### Event Protocol Stabilization (`src/event.rs`)
- Document the `AgentEvent` JSON schema as a stable public API
- Add `agent_version: Option<String>` field
- ~~Ensure `agent_type` is a free-form string (not an enum)~~ **Superseded (2026-06-14):** runtime extensibility is a non-goal, so `agent_type` need not become a free-form string. Keep a typed identity and drive per-agent behaviour from the curated registry (see the decision above) instead of scattered `match` arms; recompile-per-agent is acceptable since every change ships in a release.
- Add protocol version field for forward compatibility
- Add `live_target: Option<LiveTarget>` to describe how (and whether) the session can receive input (see Liveness & Write Semantics)

### Liveness & Write Semantics

**Invariant:** a dashboard-visible session is not necessarily a live, writable target. Today's Claude integration delivers input through a PTY/embedded pane and can reasonably assume the session it shows is the session it writes to. Other agents break that assumption — e.g. a Codex session the dashboard knows about from a wrapper or logs may only be *resumable from history*, not driveable live. The adapter contract must carry that distinction so the UI doesn't invite users to type into a card that can't accept input.

Each adapter declares a **live-target descriptor** per session:

- `kind`: `process | pty | tmux | sdk | none` — the concrete handle, if any, that input is delivered through
- `writable`: `live` | `history-only` | `none` — can we deliver input to the running session now, only resume/replay from history, or neither (view-only)?

When the dashboard delivers input to a session, the adapter returns an honest **send result** rather than fire-and-forget:

- `applied` — delivered to the live target
- `queued` — accepted, not yet confirmed applied
- `stale` — target moved on / our view was behind
- `wrong-session` — the handle no longer maps to the session we meant
- `history-only` — no live target; only history resume is possible
- `no-live-target` — nothing to write to

**UI consequence:** non-`live` sessions render visually distinct (e.g. dimmed input affordance / a "view-only" or "history" marker on the badge), and a failed/`stale`/`wrong-session` send surfaces feedback instead of silently dropping. Proving *consumption* of a specific input (generation counters, output-cursor diffing) is explicitly deferred — see Out of Scope.

_Credit: the live-target / send-result distinction was raised by @Snailflyer in [#20](https://github.com/vfarcic/dot-agent-deck/issues/20)._

### Generic Wrapper (`src/main.rs` or new `src/wrap.rs`)
- New CLI subcommand: `dot-agent-deck wrap -- codex <args>`
- Spawns the agent command as a child process
- Intercepts stdout/stderr to detect common patterns:
  - Prompt submission (user input lines)
  - Tool execution (command output patterns)
  - Status changes (thinking indicators, error messages)
- Sends `AgentEvent` messages to the daemon socket
- Passes through all I/O transparently (the agent remains fully interactive)

### Agent Registry (`src/config.rs` / new registry module)
- One cohesive entry per agent: label, badge colour, detection pattern, default command, and the integration strategy it uses.
- Default colours/labels for known agent types; unknown types get a neutral default.
- Replaces the scattered `match AgentType` arms; the agent identity stays typed.

### Dashboard UI (`src/ui.rs`)
- Show agent type badge on each card (e.g., `[Claude]`, `[OpenCode]`, `[Codex]`)
- Badge colour from the agent registry
- Filter by agent type: `/` filter supports `type:codex` syntax
- Stats bar (PRD #17) shows breakdown by agent type if multiple types are active

### Codex CLI Adapter

**Codex CLI**: Wrapper approach — `dot-agent-deck wrap -- codex`
- Detect tool calls from stdout patterns
- Map to Working/Idle/Error states
- Declare the correct `live_target` for a wrapped session (it may be `history-only` rather than a live PTY target)

### Future agents (separate PRDs — see Milestones)

Captured here only so the follow-up PRDs have a starting point; **not in this PRD's scope**:

- **Gemini CLI**: wrapper approach — same `dot-agent-deck wrap` pattern as Codex, so it should be a thin registry-entry + patterns PRD once the wrapper strategy ships here.
- **Aider**: log-watcher approach — Aider writes structured logs; `dot-agent-deck watch --agent aider --log ~/.aider/logs/current.log` would tail the log file and parse structured entries into `AgentEvent`. This needs a **new** integration strategy (log-watching), so its PRD carries that strategy implementation.

### Pane Integration
- `dot-agent-deck pane new` gets `--agent <type>` flag
- Default command per agent type from the registry
- For Codex, the pane is created with the wrapper

## Success Criteria

- Codex CLI can be monitored in the dashboard end-to-end (detection → events → status).
- Agent type is visually distinguishable on cards (Claude / OpenCode / Codex badges).
- Events from different agent types coexist in the same dashboard.
- Claude Code and OpenCode integrations continue to work unchanged — the existing test suite passes without edits.
- Filter supports agent type filtering (`type:codex`).
- `dot-agent-deck wrap` works with arbitrary commands as a basic fallback.
- The agent registry is the single place per-agent data lives; adding the next agent that reuses an existing strategy is a registry entry plus release.

## Milestones

- [ ] AgentEvent protocol documented with version field and stable JSON schema (`src/event.rs`)
- [ ] Agent registry + strategy seam: move Claude Code and OpenCode per-agent data into registry entries, behaviour-preserving (existing tests pass unchanged) (`src/event.rs`, `src/config.rs`, `src/ui.rs`)
- [ ] `live_target` descriptor (`kind` + `writable`) on the protocol and `send_result` status returned on input delivery (`src/event.rs`, `src/pane_input.rs`)
- [ ] UI renders view-only / history-only sessions distinctly and surfaces failed/stale sends (`src/ui.rs`)
- [ ] Agent type badge rendering on cards, colour from the registry (`src/ui.rs`)
- [ ] `dot-agent-deck wrap` CLI subcommand with stdout/stderr pattern detection (`src/wrap.rs`)
- [ ] Codex CLI adapter working end-to-end via wrapper, with new L2 e2e coverage
- [ ] `--agent` flag on `pane new` command with per-type default commands from the registry
- [ ] Agent type filter support in `/` search (`src/ui.rs`)
- [x] Documentation: adapter authoring guide for third-party agents (`docs/develop/agent-adapters.md`, linked from `CONTRIBUTING.md`)
- [ ] All existing tests passing unchanged; new tests for the registry, wrapper, and Codex
- [x] **Follow-up PRD: Gemini adapter** — [PRD #211](211-gemini-adapter.md) (wrapper strategy; reuses `dot-agent-deck wrap` from this PRD)
- [x] **Follow-up PRD: Aider adapter** — [PRD #212](212-aider-adapter.md) (introduces the log-watcher strategy + `watch --agent`)

## Key Files

- `src/event.rs` — Protocol stabilization, version field, `live_target` descriptor, `send_result`, registry-backed `AgentType`
- `src/config.rs` — Agent registry
- `src/pane_input.rs` — Return `send_result` status on input delivery instead of fire-and-forget
- `src/wrap.rs` (new) — Generic wrapper command (wrapper strategy)
- `src/ui.rs` — Agent badges, type filtering
- `src/main.rs` — CLI subcommand registration
- `src/pane.rs` — Agent-aware pane creation

## Risks

- **Pattern detection fragility**: Wrapper-based adapters rely on parsing stdout, which can break if agent tools change their output format. Mitigated by keeping patterns simple and having a "generic" fallback that shows basic active/idle state.
- **Agent tool availability**: Each agent has its own installation, auth, and API key requirements. We don't manage these — we just monitor.
- **Feature disparity**: Different agents expose very different levels of information. Cards for wrapper-based agents will be sparser than Claude Code cards. This is acceptable — basic status is still valuable.
- **Registry refactor scope creep**: centralising the scattered `match AgentType` arms could grow into a larger rework of `src/ui.rs`/`src/event.rs`. Mitigated by the behaviour-preserving constraint — existing tests must pass unchanged, which bounds the refactor to a move, not a redesign.
