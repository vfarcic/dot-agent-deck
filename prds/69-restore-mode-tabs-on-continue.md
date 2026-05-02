# PRD #69: Restore Mode and Orchestration Tabs With `--continue`

**Status**: Draft
**Priority**: Medium
**Created**: 2026-04-28
**Updated**: 2026-04-29 — root cause confirmed, scope expanded to orchestration tabs

## Problem

`dot-agent-deck --continue` is supposed to restore the entire workspace — plain dashboard panes, mode tabs (agent pane + side panes from `.dot-agent-deck.toml`), and orchestration tabs (orchestrator pane + role panes). In practice, only plain panes come back. Mode tabs and orchestration tabs are not restored.

For mode tabs, the restore code at `src/ui.rs:1882-2026` already attempts the work — reads `saved_pane.mode`, loads project config, opens the mode tab, registers side panes, resizes PTYs, re-issues commands, surfaces warnings on failure — but the restore branch is never entered because the data is missing on disk.

For orchestration tabs, no persistence schema or restore path exists at all.

## Root Cause (confirmed)

**Mode tabs — Hypothesis #1 (teardown-before-save) confirmed at `src/ui.rs:3414-3451`.**

- `src/ui.rs:3221-3231` correctly populates `SavedPane.mode = Some(mode_name)` into `ui.pane_metadata` when a mode tab is created.
- `src/ui.rs:3414-3421` (the exit loop) calls `tab_manager.close_tab(i)` for every non-dashboard tab, then `state.unregister_pane(&id)` for every returned pane id.
- `src/tab.rs:262-291` (`close_tab`) returns the `agent_pane_id` (`Tab::Mode`) and `role_pane_ids` (`Tab::Orchestration`); all get removed from `state.managed_pane_ids` (`src/state.rs:132-137`).
- `src/ui.rs:3427-3433` then runs `pane_metadata.retain(|id, _| live_panes.contains(id))`, which drops the just-unregistered ids — including the mode-tab agent pane carrying `mode = Some(...)`.
- `src/ui.rs:3434-3451` `SavedSession::save()` writes only the surviving (plain) panes, so `saved_pane.mode` is never on disk.

Empirical confirmation: `~/.config/dot-agent-deck/session.toml` after exiting with a mode tab open contains no `mode` key on any pane.

Because the save side strips the data, the previously suspected hypotheses #2 (silent `load_project_config`), #3 (invisible tab), and #4 (resize race) are unreachable until the save side is fixed. They may need re-investigation after the save fix lands, but no evidence currently points to them.

**Orchestration tabs — same teardown bug AND missing schema.**

Orchestration tabs share the teardown-before-save problem (their pane ids are unregistered before save runs), and additionally have no persistence schema at all:

- `config::SavedPane` (`src/config.rs:267-276`) has no orchestration field.
- `Tab::Orchestration` (`src/tab.rs:67-84`) state — role order, `start_role_index`, `orchestrator_prompt`, `OrchestrationConfig`, `OrchestrationStatus` — is not serialized anywhere.
- `src/ui.rs:1899-2033` has no orchestration restore branch.

So orchestration tabs require both a save-side fix and a new schema + new restore path.

## Workaround

Until this is fixed, the docs should not promise mode-tab or orchestration-tab restoration on `--continue`. Users can recreate mode tabs manually after restore by pressing `Ctrl+n` and selecting the project's mode in the new-pane form. Orchestration tabs must be re-created from scratch.

## Solution

### Mode tabs

Reorder `src/ui.rs:3414-3451` so the `SavedSession` snapshot is taken from `pane_metadata` **before** the `close_tab` teardown loop runs, while `managed_pane_ids` still contains every live pane. The existing `pane_metadata.retain(...)` step still has a real job (pruning externally-closed panes), but it must operate on the pre-teardown snapshot, not after. No schema change is required for mode tabs — the `mode` field already exists on `SavedPane` and is populated at creation.

After the save fix is in place, run the existing restore path end-to-end and confirm hypotheses #2–#4 are non-issues. Address any that actually surface.

### Orchestration tabs

1. **Schema**: extend `config::SavedPane` with an optional orchestration descriptor. Mirror the `mode: Option<String>` pattern — likely `orchestration: Option<OrchestrationSnapshot>` carrying enough state to reconstruct the tab (role order, `start_role_index`, `orchestrator_prompt`, the resolved `OrchestrationConfig` reference, and any persistent fields of `OrchestrationStatus`). The field must be optional with a sensible default so old `session.toml` files still load.
2. **Save**: ensure the same pre-teardown snapshot path captures orchestration metadata.
3. **Restore**: add an orchestration branch in `src/ui.rs` parallel to the mode-tab restore at `src/ui.rs:1899-2033`. Recreate the orchestrator pane and all role panes, re-issue role commands, and surface clear warnings (not silent fallbacks) if the orchestration config has changed between exit and restore.

The agent's *internal* state (Claude Code conversation, OpenCode state, role-pane conversational history) is explicitly out of scope — only the *workspace structure* must be restored.

## Acceptance Criteria

### Mode tabs
- [x] Open `dot-agent-deck`, create at least one mode tab via `Ctrl+n` with a `.dot-agent-deck.toml`-backed mode, exit with `Ctrl+c`. Re-launch with `dot-agent-deck --continue`.
- [x] The mode tab reappears in the tab bar with the original tab name.
- [x] The agent pane is present and the agent command (e.g. `claude`) was re-run.
- [x] All side panes from the mode are present and running their configured commands.
- [ ] If the project's `.dot-agent-deck.toml` was deleted or the mode name was changed between exit and restore, a clear warning is shown to the user (not silently swallowed) and the pane falls back to a plain dashboard pane.

### Orchestration tabs
- [ ] Open `dot-agent-deck`, create an orchestration tab, exit with `Ctrl+c`. Re-launch with `dot-agent-deck --continue`.
- [ ] The orchestration tab reappears in the tab bar with the original tab name.
- [ ] The orchestrator pane is present and re-running its configured command, with the original `orchestrator_prompt` available.
- [ ] All role panes are present in their original order and running their configured commands.
- [ ] `start_role_index` and any other persistent orchestration state are preserved across restart.
- [ ] If the orchestration configuration changed (role added/removed/renamed) between exit and restore, a clear warning is shown and the user is not left with a silently-broken tab.

### Cross-cutting
- [ ] Add a regression test that exercises save → restore for at least one mode tab AND one orchestration tab simultaneously, asserting side/role panes are recreated.
- [ ] Add a regression test that loads an old-format `session.toml` (no `orchestration` field, possibly no `mode` field) and confirms it still parses without error.
- [ ] After fix, restore the mode-tab-restoration paragraph in `docs/session-management.md` that was removed when this PRD was filed; add a parallel paragraph for orchestration-tab restoration.

## Out of Scope

- Restoring the agent's own conversation/session state (Claude Code, OpenCode internal session, role-pane conversation history). Agent Deck only restores the workspace structure.
- Reordering tabs across exit/restore — current behavior of always opening on the dashboard tab is acceptable.
- Adding new mode-config or orchestration-config features.

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Fix changes the `session.toml` schema in a way that breaks older saved sessions | All new fields optional with sensible defaults; regression test loads an old-format `session.toml`. |
| Restoring multiple mode/orchestration tabs at once introduces resize / focus races | Regression test covers ≥2 tabs simultaneously, mixed types. |
| Warnings remain swallowed in some failure paths | Audit `ui.session_warnings` flush logic so users always see why a restore fell back; verify after the save fix whether hypotheses #2 (silent `load_project_config`), #3 (invisible tab), #4 (resize race) are still relevant and address any that are. |
| Orchestration schema is genuinely new (unlike mode) | Design `OrchestrationSnapshot` as a versioned/extensible struct from day one to ease future evolution; default-construct on missing field. |

## Decision Log

- **2026-04-29 — Root cause confirmed**: Hypothesis #1 (teardown-before-save at `src/ui.rs:3414-3451`). Empirical evidence captured from a real `session.toml` (no `mode` key on any pane after exit-with-mode-tab). Hypotheses #2–#4 deferred until save side is fixed; revisit then.
- **2026-04-29 — Fix direction**: Reorder save vs. teardown rather than introduce a separate snapshot data structure. The existing `pane_metadata` already holds all needed state for mode tabs; the bug is purely an ordering issue.
- **2026-04-29 — Scope expansion**: Orchestration tabs added to this PRD. They share the teardown-before-save bug and additionally need a brand-new persistence schema. Bundled rather than split because (a) the save-side fix benefits both, (b) the restore-path code lives in the same area of `ui.rs`, (c) acceptance criteria are mostly symmetrical, (d) one regression test can cover both at once.
