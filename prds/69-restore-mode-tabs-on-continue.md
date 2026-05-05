# PRD #69: Restore Mode Tabs With `--continue`

**Status**: In progress
**Priority**: Medium
**Created**: 2026-04-28
**Updated**: 2026-05-05 — orchestration scope split out to a follow-up PRD

## Problem

`dot-agent-deck --continue` is supposed to restore the entire workspace — plain dashboard panes and mode tabs (agent pane + side panes from `.dot-agent-deck.toml`). In practice, only plain panes come back. Mode tabs are not restored.

The restore code at `src/ui.rs:1882-2026` already attempts the work — reads `saved_pane.mode`, loads project config, opens the mode tab, registers side panes, resizes PTYs, re-issues commands, surfaces warnings on failure — but the restore branch is never entered because the data is missing on disk.

## Root Cause (confirmed)

**Hypothesis #1 (teardown-before-save) confirmed at `src/ui.rs:3414-3451`.**

- `src/ui.rs:3221-3231` correctly populates `SavedPane.mode = Some(mode_name)` into `ui.pane_metadata` when a mode tab is created.
- `src/ui.rs:3414-3421` (the exit loop) calls `tab_manager.close_tab(i)` for every non-dashboard tab, then `state.unregister_pane(&id)` for every returned pane id.
- `src/tab.rs:262-291` (`close_tab`) returns the `agent_pane_id`; all get removed from `state.managed_pane_ids` (`src/state.rs:132-137`).
- `src/ui.rs:3427-3433` then runs `pane_metadata.retain(|id, _| live_panes.contains(id))`, which drops the just-unregistered ids — including the mode-tab agent pane carrying `mode = Some(...)`.
- `src/ui.rs:3434-3451` `SavedSession::save()` writes only the surviving (plain) panes, so `saved_pane.mode` is never on disk.

Empirical confirmation: `~/.config/dot-agent-deck/session.toml` after exiting with a mode tab open contains no `mode` key on any pane.

## Solution

Reorder `src/ui.rs:3414-3451` so the `SavedSession` snapshot is taken from `pane_metadata` **before** the `close_tab` teardown loop runs, while `managed_pane_ids` still contains every live pane. The existing `pane_metadata.retain(...)` step still has a real job (pruning externally-closed panes), but it must operate on the pre-teardown snapshot, not after. No schema change is required — the `mode` field already exists on `SavedPane` and is populated at creation.

After the save fix is in place, run the existing restore path end-to-end and confirm hypotheses #2–#4 are non-issues. Address any that actually surface.

The agent's *internal* state (Claude Code conversation, OpenCode state) is explicitly out of scope — only the *workspace structure* must be restored.

## Acceptance Criteria

### Mode tabs
- [x] Open `dot-agent-deck`, create at least one mode tab via `Ctrl+n` with a `.dot-agent-deck.toml`-backed mode, exit with `Ctrl+c`. Re-launch with `dot-agent-deck --continue`.
- [x] The mode tab reappears in the tab bar with the original tab name.
- [x] The agent pane is present and the agent command (e.g. `claude`) was re-run.
- [x] All side panes from the mode are present and running their configured commands.
- [x] If the project's `.dot-agent-deck.toml` was deleted or the mode name was changed between exit and restore, a clear warning is shown to the user (not silently swallowed) and the pane falls back to a plain dashboard pane.

### Cross-cutting
- [x] Add a regression test that exercises save → restore for at least one mode tab, asserting side panes are recreated.
- [ ] Add a regression test that loads an old-format `session.toml` (no `mode` field on any pane) and confirms it still parses without error.
- [ ] Restore the mode-tab-restoration paragraph in `docs/session-management.md` that was removed when this PRD was filed.

## Out of Scope

- Restoring the agent's own conversation/session state (Claude Code, OpenCode internal session). Agent Deck only restores the workspace structure.
- Reordering tabs across exit/restore — current behavior of always opening on the dashboard tab is acceptable.
- Adding new mode-config features.
- Orchestration tabs — moved to a follow-up PRD (see Decision Log entry 2026-05-05).

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Fix changes the `session.toml` schema in a way that breaks older saved sessions | All new fields optional with sensible defaults; regression test loads an old-format `session.toml`. |
| Warnings remain swallowed in some failure paths | Audit `ui.session_warnings` flush logic so users always see why a restore fell back; verify after the save fix whether hypotheses #2 (silent `load_project_config`), #3 (invisible tab), #4 (resize race) are still relevant and address any that are. |

## Decision Log

- **2026-04-29 — Root cause confirmed**: Hypothesis #1 (teardown-before-save at `src/ui.rs:3414-3451`). Empirical evidence captured from a real `session.toml` (no `mode` key on any pane after exit-with-mode-tab). Hypotheses #2–#4 deferred until save side is fixed; revisit then.
- **2026-04-29 — Fix direction**: Reorder save vs. teardown rather than introduce a separate snapshot data structure. The existing `pane_metadata` already holds all needed state for mode tabs; the bug is purely an ordering issue.
- **2026-04-29 — Scope expansion**: Orchestration tabs added to this PRD. They share the teardown-before-save bug and additionally need a brand-new persistence schema. Bundled because the save-side fix benefits both.
- **2026-05-05 — Scope split**: Orchestration tabs moved to a follow-up PRD. The shared save-side fix has shipped (commits `048f28f` and `7710b77`), so the original bundling rationale no longer applies — mode-tab and orchestration restore paths are now mostly parallel rather than entangled, and the mode-tab work is at a natural shipping boundary. The follow-up PRD will inherit:
  - **Schema**: extend `config::SavedPane` with `orchestration: Option<OrchestrationSnapshot>` carrying role order, `start_role_index`, `orchestrator_prompt`, the resolved `OrchestrationConfig` reference, and any persistent fields of `OrchestrationStatus`. Field optional with sensible default so existing `session.toml` files still load. Designed as a versioned/extensible struct from day one.
  - **Save**: rely on the already-shipped pre-teardown snapshot path; just ensure orchestration metadata is captured.
  - **Restore**: add an orchestration branch in `src/ui.rs` parallel to the mode-tab restore at `src/ui.rs:1899-2033`. Recreate the orchestrator pane and all role panes, re-issue role commands, and surface clear warnings (not silent fallbacks) if the orchestration config changed between exit and restore.
  - **Acceptance criteria**: orchestration tab reappears with original name; orchestrator pane re-runs its command with the original `orchestrator_prompt`; all role panes present in original order with their commands; `start_role_index` preserved; clear warning + non-broken state if config changed.
  - **Tests**: regression test for orchestration save→restore; old-format `session.toml` parse test extended to cover missing `orchestration` field.
  - **Docs**: parallel paragraph for orchestration-tab restoration in `docs/session-management.md`.
