# PRD #162: Restore live session status on daemon reconnect

**Status**: In Progress — implementation, tests, review, and e2e gate complete; changelog + PR/merge pending (M4.2–M4.3)
**Priority**: High
**Created**: 2026-06-14
**GitHub Issue**: [#162](https://github.com/vfarcic/dot-agent-deck/issues/162)
**Related**: PRD #93 (always-external daemon — the reason the daemon outlives the TUI and owns the authoritative session state), PRD #148 (remote connect survives sleep/wake — auto-reconnects the ssh session; this PRD makes the *resumed dashboard* correct, not just reconnected), PRD #89 (auto-restore TUI state), PRD #110 (`agent_id` on `SessionState` — the join key this PRD relies on), PRD #76 M2.11–M2.13 (the `display_name` / `cwd` / `agent_type` reconnect-snapshot fields this PRD extends).

## Problem Statement

The daemon and the TUI are separate processes: the daemon owns the agents and outlives the TUI (PRD #93), so a user can disconnect (ssh drop, `Ctrl+C` the local TUI, close the tab) and later reconnect with `dot-agent-deck connect` to the *same* running agents. On reconnect the dashboard rebuilds one card per live agent — but every card shows the **wrong status**: either **"No agent"** or a reset-to-**"Idle"** state, regardless of what the agent is actually doing. The status only becomes correct when (and if) the agent emits its *next* hook event.

The failure is most visible — and never self-heals — for an agent that is genuinely idle or waiting for input at reconnect time: it emits no further events, so the card stays stuck on the wrong label until the user touches it.

This directly undermines the experience PRDs #93 and #148 are built to deliver. #148 auto-reconnects the ssh session after laptop sleep precisely so the user "reopens the laptop and the session is just there" — but if the resumed dashboard reads "No agent" on every card, the session is *not* there from the user's point of view, even though the agents are alive and working underneath.

### Root cause

The daemon **already tracks live status correctly**. Every hook event flows through `state.write().await.apply_event(event)` (`src/daemon.rs:884`), maintaining an authoritative `AppState.sessions` map keyed by `session_id`, where each `SessionState` carries the real `status` (`Idle`/`Working`/`Thinking`/`WaitingForInput`/`Compacting`/`Error`), the event-derived `agent_type`, the `active_tool`, `tool_count`, and `first_prompts` (`src/state.rs:64-92`).

The bug is entirely on the **reconnect snapshot path**, which reads the *wrong store*:

1. On reconnect the TUI calls `ListAgents`. The handler returns `registry.agent_records()` (`src/daemon_protocol.rs:766-768`, `src/agent_pty.rs:2178`).
2. `agent_records()` reads the **PTY registry** (`RunningAgent`), which holds only **spawn-time** metadata — `pane_id_env`, `display_name`, `cwd`, `tab_membership`, `agent_type`, `rows`, `cols`. **There is no `status` field at all** (`AgentRecord`, `src/agent_pty.rs:1087-1121`).
3. The TUI hydrates each record via `hydrate_from_daemon` → `HydratedPane` → `insert_placeholder_session(...)`, which hardcodes `status: SessionStatus::Idle` and takes `agent_type` from the spawn-time record (`src/ui.rs:5538-5543`, `src/state.rs` `insert_placeholder_session`).
4. The TUI then `SubscribeEvents` to **future** events only — there is no initial snapshot of current state.

So two things regress, both from the same cause:

- **Status** is always reset to `Idle` until the next event arrives — the daemon's live status is never read on connect.
- **"No agent"** appears whenever the spawn-time `agent_type` was `None` (e.g. the launch command was a wrapper/script not matched by `AgentType::from_command`). During the live session the card looked correct because `agent_type` was being set *from events*; on reconnect it silently falls back to the stale spawn-time value. The code comment at `src/ui.rs:5521-5529` already documents this fallback as the expected (legacy) behavior — this PRD removes the need for that fallback.

The fix is clean and additive because the data already exists daemon-side and the handler already has access to it: the production attach handler (`handle_connection` → `serve_attach_with_counter`) is already passed `state: SharedState` (`src/daemon_protocol.rs:566,688`); the `ListAgents` arm at line 766 simply does not read it yet. `SessionState` carries both `agent_id` (PRD #110) and `pane_id`, which are the join keys back to `AgentRecord.id` / `AgentRecord.pane_id_env`.

## Solution Overview

On connect, enrich the reconnect snapshot with the daemon's **live, event-derived session state**, and seed each hydrated session from it instead of minting a bare `Idle`/`None` placeholder.

The scope chosen (full session snapshot) restores everything the daemon already knows, so the reconnected dashboard matches the pre-disconnect view:

- **`status`** — the live `SessionStatus`.
- **`agent_type`** — the **event-derived** value (fixes "No agent"); falls back to the spawn-time registry value when the session hasn't emitted events yet.
- **`active_tool`** — name + detail, so a card mid-tool keeps showing it.
- **`tool_count`** — so the running tool tally is preserved.
- **`first_prompts`** / **`last_user_prompt`** — so the card's prompt context survives the reconnect.

### Wire shape (additive, back-compatible)

Follow the established reconnect-field pattern (M2.11–M2.13: every snapshot field is `#[serde(default, skip_serializing_if = "Option::is_none")]`, no `PROTOCOL_VERSION` bump). The live-session fields are carried as an **optional nested snapshot** on `AgentRecord` (e.g. `live: Option<SessionSnapshot>`) rather than scattering individual optionals, so the "no live session" case (older daemon, test/dummy-state path, agent that never emitted an event) is represented by a single `None` and the TUI falls back to today's behavior.

### Join in the `ListAgents` handler

The handler reads the daemon's `AppState` (already in scope as `state`), and for each registry record finds the matching live session by `agent_id` (primary) / `pane_id` (secondary), attaching its `SessionSnapshot`. The test/harness path (`serve_attach`, `src/daemon_protocol.rs:521`) constructs a `dummy_state` with an empty `AppState`, which naturally yields no snapshots → exactly today's behavior, so no test-harness regression.

When more than one historical session maps to the same agent (e.g. a `/clear` restart leaving a stale entry), pick the current one — match on both `agent_id` **and** `pane_id`, breaking ties by most-recent `last_activity` — so the live card reflects the running session, not a dead predecessor.

### Seed on the TUI side

`HydratedPane` carries the optional `SessionSnapshot` through, and the hydration block seeds the session from it (a snapshot-aware insert, or extending `insert_placeholder_session`) — using the snapshot's `status` / `agent_type` / `active_tool` / `tool_count` / `first_prompts` when present, and the current bare-placeholder defaults when absent. The PRD #110 `agent_id` minting on the placeholder is preserved so a post-reconnect `SessionStart` still remaps onto the hydrated card rather than spawning a duplicate.

## Scope

### In Scope

- A `SessionSnapshot` type (serde, additive optional) carrying `status`, event-derived `agent_type`, `active_tool`, `tool_count`, `first_prompts`, `last_user_prompt`.
- Attaching the snapshot to each `AgentRecord` in the `ListAgents` handler by joining the registry snapshot against the daemon's live `AppState.sessions` (on `agent_id` + `pane_id`, newest-wins).
- Threading the snapshot through `HydratedPane` and seeding the hydrated session from it (status, agent_type, active tool, tool count, prompt history), with a graceful fallback to today's bare placeholder when the snapshot is absent.
- Preserving the M2.11–M2.13 reconnect fields and the PRD #110 `agent_id` placeholder minting unchanged.
- Tests (Phase 3) and docs note (Phase 4).

### Out of Scope

- **Backfilling the activity feed / `recent_events`.** The live event stream resumes via `SubscribeEvents`; replaying up to 50 historical events per agent over the wire is a heavier, separable concern. (Tracked as an open question.)
- **A `PROTOCOL_VERSION` bump.** All new fields are additive optionals; an older daemon simply sends `None` and the TUI falls back — same forward-compat posture as M2.11–M2.13.
- **Persisting session state across a daemon restart.** This PRD restores state from a *live* daemon to a *reconnecting TUI*; a daemon that itself restarted has no `AppState` to offer and is governed by the existing spawn-time fallback.
- **Local in-process (`LocalDeck`) path.** `hydrate_from_daemon` is already a no-op there (the in-process daemon shares the TUI's registry directly), so there is nothing to snapshot.
- **Changing how status is *derived*.** `apply_event` is the single source of truth and is unchanged; this PRD only *exposes* what it already computes.

## Success Criteria

- After disconnecting and reconnecting to a daemon with live agents, each card shows the agent's **actual** status (`Working`/`Thinking`/`WaitingForInput`/`Idle`/…) immediately on reconnect — not `Idle` and not "No agent" — without waiting for a new event. Verified by tests.
- An agent whose launch command was **not** matched by `AgentType::from_command` (spawn-time `agent_type = None`) but which has emitted at least one event shows its real agent label (e.g. ClaudeCode/OpenCode) on reconnect instead of "No agent". Verified by tests.
- A card mid-tool keeps its active-tool label and tool count across the reconnect; first-prompt context is preserved. Verified by tests.
- An older daemon (or the test dummy-state path) that supplies no snapshot degrades to today's bare-placeholder behavior with no panic and no duplicate cards. Verified by tests.
- A post-reconnect `SessionStart` from the same agent remaps onto the hydrated card (no duplicate), as today (PRD #110 property preserved). Verified by tests.

## Milestones

### Phase 1: Snapshot on the wire (daemon side)

- [x] **M1.1** — Define `SessionSnapshot` (serde, additive). Add `live: Option<SessionSnapshot>` to `AgentRecord` with `#[serde(default, skip_serializing_if = "Option::is_none")]`; serde round-trip + older-shape-deserializes-to-None tests in `daemon_protocol.rs` / `agent_pty.rs`. — Done (5549f08; `SessionSnapshot` in `crate::state`; test `session/live/001`).
- [x] **M1.2** — In the `ListAgents` handler, join `registry.agent_records()` against the daemon's `AppState.sessions` (on `agent_id` + `pane_id`, newest-`last_activity`-wins) and attach the snapshot. Dummy-state path yields `None` (today's behavior). The join reads only the already-in-scope `state: SharedState`. — Done (5549f08, tiebreak hardened in 61125a9; tests `session/live/002`, `session/live/003`).

### Phase 2: Seed the reconnected session (TUI side)

- [x] **M2.1** — Carry the snapshot through `HydratedPane` (`src/embedded_pane.rs`). — Done (1c6fe58; `HydratedPane.live`, set from `record.live` in `hydrate_from_daemon`).
- [x] **M2.2** — Seed the hydrated session from the snapshot in the hydration block (`src/ui.rs:~5512-5560`): a snapshot-aware insert that sets `status` / `agent_type` / `active_tool` / `tool_count` / `first_prompts` / `last_user_prompt` when present, and the current bare-placeholder defaults when absent. Preserve PRD #110 `agent_id` minting. Drop / supersede the "No agent until SessionStart" fallback comment where the snapshot now covers it. — Done (1c6fe58; `AppState::seed_hydrated_session` delegates to `insert_placeholder_session` then overlays the snapshot; agent_type precedence fixed in 61125a9; tests `session/live/004`, `session/live/008`).

### Phase 3: Tests

- [x] **M3.1** — State/protocol tests: `SessionSnapshot` round-trip + back-compat (`None` from older shape); the `ListAgents` join (status/agent_type/active_tool/tool_count/prompts populated; newest-wins on duplicate agent; dummy-state → `None`). — Done (`session/live/001`–`003`; 31b450e). Plus security-hardening tests from review: wire-boundary sanitize + clamp (`session/live/007`) and agent_type precedence (`session/live/008`) — df94e58.
- [x] **M3.2** — TUI hydration tests: a hydrated session reflects the snapshot status/label instead of `Idle`/"No agent" (L1 render/state); no-snapshot path falls back to bare placeholder; post-reconnect `SessionStart` remaps onto the hydrated card (no duplicate). L2 (`e2e_*`, gated) for the disconnect→reconnect-against-real-daemon path where the spawned binary + attach protocol are exercised. Run `cargo test-e2e` before the PR. — Done (`session/live/004`–`006`; 44ebf2e). E2E gate green: `DOT_AGENT_DECK_RECORD=1 cargo test-e2e` → 1449/1449 passed, `session/live/006` passing under parallel load after a test-sync fix (7246c02).

### Phase 4: Docs and release

- [x] **M4.1** — Docs: note in the daemon/reconnect documentation that a reconnected dashboard restores live status (not just the list of agents). Keep it dual-render (Docusaurus + GitHub) per repo convention. — Done (d789667; `docs/session-management.md` "Resuming Sessions").
- [ ] **M4.2** — Changelog fragment (`dot-ai-changelog-fragment`) on the first push to the PR.
- [ ] **M4.3** — PR, Greptile review, audit, merge, close.

## Key Files

- `src/agent_pty.rs` — `AgentRecord` gains `live: Option<SessionSnapshot>` (M1.1); `agent_records()` is the registry snapshot the join enriches (M1.2).
- `src/daemon_protocol.rs` — the `ListAgents` arm (`:766`) performs the join using the already-passed `state: SharedState` (`:566,:688`); `serve_attach` dummy-state path (`:521`) yields no snapshots (M1.2). `SessionSnapshot` type may live here or in `state.rs`.
- `src/state.rs` — `SessionState` (`:64-92`) is the source of the snapshot fields; `insert_placeholder_session` is extended (or a snapshot-aware sibling added) for seeding (M2.2). `apply_event` (`:795-1059`) unchanged.
- `src/embedded_pane.rs` — `HydratedPane` (`:22-52`) carries the snapshot through `hydrate_from_daemon` (M2.1).
- `src/ui.rs` — the hydration block (`:5512-5560`) seeds the session from the snapshot and supersedes the "No agent until SessionStart" fallback (M2.2).
- `src/daemon.rs` — `apply_event` call site (`:884`) is the (unchanged) reason the daemon already holds the live state; referenced for context.

## Risks and Mitigations

- **Risk**: Stale-session mismatch — a `/clear` restart or respawn can leave more than one `SessionState` mapping to the same agent/pane, so the join could attach a dead session's status.
  - *Mitigation*: Match on `agent_id` **and** `pane_id`, break ties by most-recent `last_activity`; covered by an M3.1 newest-wins test. The PRD #110 `agent_id` reuse guard already keeps live restarts collapsed onto one session in the common case.
- **Risk**: Snapshot vs live-event race — a hook event could arrive between the `ListAgents` response and the `SubscribeEvents` subscription, or just after seeding, briefly showing slightly stale status.
  - *Mitigation*: The seeded value is already correct-as-of-snapshot and the next event reconciles it via the same `apply_event` path; the window is the same one that exists today for the agent list itself. Order the subscribe before/around the list if the race proves observable (open question).
- **Risk**: Wire/version skew — an older daemon doesn't send the snapshot; a newer daemon talking to an older TUI sends a field it ignores.
  - *Mitigation*: Additive optional field with `skip_serializing_if`; `None`/unknown-field both degrade to today's behavior, asserted by an M3.1 back-compat test. No `PROTOCOL_VERSION` bump.
- **Risk**: Duplicate cards — mis-seeding the placeholder identity could break the PRD #110 remap and spawn a second card on the next `SessionStart`.
  - *Mitigation*: Preserve the `agent_id` minting on the seeded session exactly as the current placeholder path does; explicit M3.2 no-duplicate test.

## Open Questions

- **`recent_events` backfill**: should the reconnected card's activity feed be repopulated from the daemon's `recent_events` (up to 50/agent), or is resuming the live stream sufficient? (Out of scope as drafted — heavier wire payload; revisit if the empty feed on reconnect is jarring.)
- **Subscribe/list ordering**: is the list→subscribe gap wide enough to warrant subscribing first (or snapshotting under the same lock) to fully close the race, or is the self-reconciling next-event behavior good enough?
- **Snapshot type location**: `SessionSnapshot` in `daemon_protocol.rs` (next to the wire types) vs `state.rs` (next to `SessionState` it is derived from) — which keeps the dependency direction cleaner?
- **Status mapping completeness**: confirm every `SessionStatus` variant (including `Error` and `Compacting`) round-trips and renders correctly when seeded cold, since some are normally only ever seen transiently mid-stream.
