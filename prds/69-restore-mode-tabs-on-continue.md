# PRD #69: Restore Mode Tabs With `--continue`

**Status**: Draft
**Priority**: Medium
**Created**: 2026-04-28

## Problem

`dot-agent-deck --continue` is supposed to restore the entire workspace — plain dashboard panes *and* mode tabs (agent pane + side panes from `.dot-agent-deck.toml`). In practice, only plain panes come back. Mode tabs are not restored, even though the code at `src/ui.rs:1882-2026` clearly attempts the restoration:

- `saved_pane.mode` is read at line 1903.
- `load_project_config` is called to find the mode by name (lines 1904-1907).
- `tab_manager.open_mode_tab` is invoked with the saved pane (line 1979).
- Side panes are registered (line 1981-1983).
- PTYs are resized (lines 1984-2001).
- `init_command` and the saved agent command are re-issued (lines 2003-2008).
- Warnings are pushed if anything fails (lines 1895-1929, 2010-2024).

Despite all this, when a user exits with one or more mode tabs open and re-launches with `--continue`, the mode tabs do not reappear and no obvious error is surfaced.

## Workaround

Until this is fixed, the docs should not promise mode-tab restoration on `--continue`. Users can recreate mode tabs manually after restore by pressing `Ctrl+n` and selecting the project's mode in the new-pane form.

## Solution

Investigate why the deferred mode restore path does not produce a visible mode tab in practice. Hypotheses, in order of where to look:

1. **Persistence gap on save**: `saved_pane.mode` may not be written to `session.toml` when the user exits, so the restore branch at `ui.rs:1903` is never taken on the next launch. Check `auto_save_session()` (referenced near `ui.rs:3374`) and `config::SavedSession`/`SavedPane` to confirm the `mode` field is actually populated and round-tripped.

2. **Silent failure of `load_project_config`**: The warning paths at lines 1912-1929 push to `ui.session_warnings` and are flushed only after terminal restore (line 3412). Confirm warnings are actually shown to the user; if not, errors here vanish without a trace.

3. **Tab created but invisible**: `open_mode_tab` may succeed but the post-restore `tab_manager.switch_to(0)` (line 2028) plus subsequent focus logic may leave the mode tab unrendered or in a broken state. Confirm the tab bar shows the restored tab name after restore.

4. **Resize race**: PTYs are resized before the first draw call. If `terminal.get_frame().area()` returns stale dimensions, side panes may end up with zero rows/cols and never display.

Fix so that `dot-agent-deck --continue` recreates each mode tab exactly as it was on exit: agent pane + side panes + init command + saved agent command. The agent's *internal* state (Claude Code conversation, OpenCode state) is explicitly out of scope — only the *workspace structure* must be restored.

## Acceptance Criteria

- [ ] Open dot-agent-deck, create at least one mode tab via `Ctrl+n` with a `.dot-agent-deck.toml`-backed mode, exit with `Ctrl+c`. Re-launch with `dot-agent-deck --continue`.
- [ ] The mode tab reappears in the tab bar with the original tab name.
- [ ] The agent pane is present and the agent command (e.g. `claude`) was re-run.
- [ ] All side panes from the mode are present and running their configured commands.
- [ ] If the project's `.dot-agent-deck.toml` was deleted or the mode name was changed between exit and restore, a clear warning is shown to the user (not silently swallowed) and the pane falls back to a plain dashboard pane.
- [ ] Add a regression test that exercises save → restore for at least one mode tab and asserts side panes are recreated.
- [ ] After fix, restore the mode-tab-restoration paragraph in `docs/session-management.md` that was removed when this PRD was filed.

## Out of Scope

- Restoring the agent's own conversation/session state (Claude Code, OpenCode internal session). Agent Deck only restores the workspace.
- Reordering tabs across exit/restore — current behavior of always opening on the dashboard tab is acceptable.
- Adding new mode-config features.

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Fix changes the `session.toml` schema in a way that breaks older saved sessions | Make any new field optional with a sensible default; add a regression test that loads an old-format `session.toml`. |
| Restoring multiple mode tabs at once introduces resize / focus races | Add the regression test with at least two mode tabs simultaneously. |
| Warnings remain swallowed in some failure paths | Audit `ui.session_warnings` flush logic so users always see why a restore fell back. |
