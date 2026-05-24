# PRD #110: Fix session reuse conflicting with F9 clear=true respawn

**Status**: Planning
**Priority**: High
**Created**: 2026-05-24
**GitHub Issue**: [#110](https://github.com/vfarcic/dot-agent-deck/issues/110)
**Related**: PRD #92 (F9 respawn on delegate), commit `781c2aa` (fix: reuse opencode decks)

## Problem Statement

Two features introduced at different times now conflict:

1. **Commit `781c2aa` (Apr 2, 2026) — "fix: reuse opencode decks"**: Added session-reuse logic in `src/state.rs:778-788`. When any `SessionStart` event arrives for a pane that already has an active session, `apply_event` silently remaps the incoming `session_id` to the existing one. This was correct at the time — opencode generates a new session ID on every process start, so without this rule, each opencode restart (e.g., after a crash or config reload) would create a new orphaned dashboard card for the same pane.

2. **PRD #92 F9 (May 24, 2026) — clear=true respawn**: The daemon now correctly kills and respawns the worker agent (e.g., reviewer, auditor) when the orchestrator delegates with `clear=true`. The new process emits `SessionStart` with a fresh `session_id` **and** a new `DOT_AGENT_DECK_AGENT_ID` (injected at spawn time by the daemon).

**The conflict**: After a F9 respawn, the new agent's `SessionStart` hits the reuse code, which sees the same `pane_id` as the old session and maps the new `session_id` back to the old one. The TUI dashboard continues to display the old session's data, making it appear to the user that no new session was started — even though the agent process was correctly replaced on the daemon side.

This is why users observe "delegate doesn't start a new session": the daemon is doing the right thing, but the TUI is hiding the new session behind the old session card.

## Solution Overview

Track `agent_id` inside `SessionState`. Update the session-reuse logic in `apply_event` to apply reuse **only** when the new event's `agent_id` matches the existing session's `agent_id`:

- **Same `agent_id`** (or both absent): same agent process continuing across a natural restart (opencode crash/reload) → reuse the existing session card, preserving the original behaviour.
- **Different `agent_id`**: intentional respawn via clear=true delegate → skip reuse, let the new `session_id` create a fresh session card.

`AgentEvent` already carries an optional `agent_id` field (added by F9 followup-7, commit `98f9a89`). `SessionState` needs a new `agent_id: Option<String>` field and population logic.

## Scope

### In Scope

- Add `agent_id: Option<String>` to `SessionState` in `src/state.rs`.
- Populate it when a session is created or updated from a `SessionStart` event.
- Update the reuse guard in `apply_event` (lines 778-788) to check agent_id equality before remapping `session_id`.
- Unit tests covering:
  - Same pane, same `agent_id` → session reused (existing behaviour preserved).
  - Same pane, different `agent_id` (clear=true respawn) → new session created.
  - Same pane, both `agent_id` absent (pre-F9 events) → session reused (backward-compat).
- Integration test extending `tests/orchestration_delegate.rs`: verify dashboard shows a *new* session card after a clear=true delegate.

### Out of Scope

- Changes to the daemon-side respawn logic (already correct from F9).
- Changes to `AgentEvent.agent_id` propagation (already correct from F9 followup-7).
- Dashboard UI changes beyond what naturally follows from the new session being tracked.

## Key Files

| File | Change |
|------|--------|
| `src/state.rs` | Add `agent_id` to `SessionState`; update `apply_event` reuse guard |
| `src/event.rs` | Read-only — `AgentEvent.agent_id` already exists |
| `tests/orchestration_delegate.rs` | Add session-card verification after clear=true delegate |

## Milestones

- [ ] **M1 — SessionState carries agent_id**: Add `agent_id: Option<String>` to `SessionState`; populate it on session creation/update from `SessionStart` events.
- [ ] **M2 — Reuse guard updated**: `apply_event` skips session reuse when `agent_id` differs; existing same-agent reuse path unchanged.
- [ ] **M3 — Unit tests pass**: Tests for same-agent reuse, different-agent new-session, and absent-agent-id backward-compat cases all pass.
- [ ] **M4 — Integration test**: `orchestration_delegate.rs` verifies a new session card appears in TUI state after clear=true delegate.
- [ ] **M5 — Regression verified**: Existing opencode deck tests still pass (no orphaned-card regression).

## Success Criteria

1. After the orchestrator delegates to a reviewer/auditor with `clear=true`, the TUI dashboard/orchestration tab shows a **new** session card for that role (fresh `started_at`, no old conversation history carried over).
2. When opencode restarts naturally within the same pane (crash, reload) **without** a delegate, the TUI still shows the same session card (no orphaned card regression).
3. All existing session and orchestration tests pass.

## Notes

- The fix is deliberately minimal: one new field, one guard check. No changes to the wire protocol, daemon side, or hook scripts.
- Pre-F9 events (no `agent_id` in the payload) continue to be handled by the existing reuse path, so older hook scripts remain compatible.
