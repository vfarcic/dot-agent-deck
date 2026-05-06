# PRD #74: Restore Orchestration Tabs With `--continue`

**Status**: Not started
**Priority**: Medium
**Created**: 2026-05-05
**GitHub Issue**: https://github.com/vfarcic/dot-agent-deck/issues/74
**Parent context**: split out of PRD #69 (mode-tab restore). See PRD #69 Decision Log entry 2026-05-05 for the rationale.

## Problem

`dot-agent-deck --continue` restores plain dashboard panes and (as of PRD #69) mode tabs, but **orchestration tabs are not restored**. Users who exit while a multi-role orchestration tab is open lose the entire workspace structure on relaunch — orchestrator pane, role panes, prompts, and role ordering all have to be recreated by hand.

This is the orchestration-tab half of the original PRD #69 scope. The shared save-side bug (teardown-before-save at `src/ui.rs:3414-3451`) was fixed in commits `048f28f` and `7710b77`, so the persistence machinery is now in place. What remains is orchestration-specific: a schema, capture in the snapshot, and a restore branch.

## Solution

Capture enough orchestration metadata in `SavedPane` to recreate the tab on relaunch, then add an orchestration branch to the restore loop in `src/ui.rs` that mirrors the mode-tab restore flow.

### Schema

Extend `config::SavedPane` with:

```rust
pub orchestration: Option<OrchestrationSnapshot>,
```

`OrchestrationSnapshot` carries:

- Role order (`Vec<String>` of role names in display order)
- `start_role_index: usize`
- `orchestrator_prompt: String`
- A reference to the resolved `OrchestrationConfig` (e.g. config name + project path so it can be re-resolved on restore)
- Persistent fields of `OrchestrationStatus` worth restoring (e.g. which roles have been started)

Field is `Option<...>` with `#[serde(default)]` so existing `session.toml` files (no `orchestration` key) still load. Designed as a versioned/extensible struct from day one — include a `version: u32` field so future schema changes can be migrated rather than dropped.

### Save

Already-shipped pre-teardown snapshot path (PRD #69, commits `048f28f` and `7710b77`) covers ordering. Only change needed: when populating `SavedPane` from `pane_metadata`, attach the `OrchestrationSnapshot` for orchestration tabs (parallel to how `mode = Some(...)` is populated for mode tabs).

### Restore

Add an orchestration branch in `src/ui.rs` parallel to the mode-tab restore at `src/ui.rs:1899-2033`. The branch must:

1. Re-resolve `OrchestrationConfig` from project + name. If config is missing or the named orchestration was renamed, surface a clear warning via `ui.session_warnings` and fall back to a plain dashboard pane (same pattern as mode-tab Path D/E from PRD #69).
2. Recreate the orchestrator pane. Re-issue its command with the saved `orchestrator_prompt`.
3. Recreate role panes in the saved order. Re-issue each role's configured command.
4. Restore `start_role_index` so the "next role to start" cursor matches what the user had.
5. Surface a clear warning (not silent fallback) if role definitions changed between exit and restore.

## Acceptance Criteria

### Orchestration tabs

- [ ] Open `dot-agent-deck`, create an orchestration tab via the orchestration entry point with a `.dot-agent-deck.toml`-backed orchestration, exit with `Ctrl+c`. Re-launch with `dot-agent-deck --continue`.
- [ ] The orchestration tab reappears in the tab bar with the original tab name.
- [ ] The orchestrator pane is present and re-ran its command with the original `orchestrator_prompt`.
- [ ] All role panes are present in their original order, each running its configured command.
- [ ] `start_role_index` is preserved across restore.
- [ ] If the project's `.dot-agent-deck.toml` was deleted, the orchestration was renamed, or a role was removed between exit and restore, a clear warning is shown to the user (not silently swallowed) and the pane falls back to a plain dashboard pane in a non-broken state.

### Cross-cutting

- [ ] Add a regression test that exercises save → restore for at least one orchestration tab, asserting the orchestrator pane and all role panes are recreated with correct commands and order.
- [ ] Extend the existing old-format `session.toml` parse test to confirm a missing `orchestration` field still parses without error.
- [ ] Add a parallel paragraph for orchestration-tab restoration in `docs/session-management.md`, mirroring the mode-tab paragraph added in PRD #69.

## Out of Scope

- Restoring the orchestrator agent's own conversation/session state (Claude Code, OpenCode internal session). Agent Deck only restores the workspace structure.
- Reordering tabs across exit/restore — current behavior of always opening on the dashboard tab is acceptable.
- Adding new orchestration-config features.
- Mode-tab restore — shipped in PRD #69.

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Schema change breaks older saved sessions | `orchestration` field is `Option<...>` with `#[serde(default)]`; regression test loads an old-format `session.toml`. Include a `version: u32` on `OrchestrationSnapshot` from day one. |
| Orchestration config drift between exit and restore (renamed, removed, role list changed) leaves user in a half-broken state | Always surface drift via `ui.session_warnings`; on any drift, fall back to plain dashboard pane rather than partially-restored orchestration tab. Mirror the Path D/E fallback pattern from PRD #69. |
| Role command re-issue races the PTY resize, leading to garbled output | Reuse the resize-then-command sequencing already proven for mode-tab restore. |

## Implementation Notes

- Save-side fix from PRD #69 already in place — focus is purely on orchestration-specific schema + restore branch.
- Restore branch should live next to the mode-tab restore code so the two stay easy to compare. Extracting a shared helper for "register pane + queue command + handle failure with warning + fallback" may be worth considering once both branches are in place, but is not required up front.
- Watch for the same teardown-before-save trap that bit PRD #69 — verify by inspection that the orchestration branch of `pane_metadata` population happens before the close_tab teardown loop runs.

## References

- PRD #69 (mode-tab restore, shipped) — `prds/done/69-restore-mode-tabs-on-continue.md` after close-out
- PRD #58 (multi-role agent orchestration) — `prds/58-multi-role-agent-orchestration.md`
- PRD #59 (orchestration documentation) — `prds/59-orchestration-documentation.md`
- Save-side fix commits: `048f28f`, `7710b77`
- Mode-tab restore reference path: `src/ui.rs:1899-2033`
- Save/teardown ordering: `src/ui.rs:3414-3451`
