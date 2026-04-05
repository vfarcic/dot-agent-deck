# PRD #32: Saved Pane Layouts

**Status**: Draft
**Priority**: Medium
**Created**: 2026-04-02
**GitHub Issue**: [#32](https://github.com/vfarcic/dot-agent-deck/issues/32)

## Problem Statement

Launching `dot-agent-deck` always starts from a blank slate: users must re-open every agent pane, reselect directories, re-enter pane names, and retype commands every time the dashboard is launched. This slows down adoption for people who work with a fixed set of repositories or long-running agents, and introduces errors when a pane is configured differently than expected.

## Background

- The dashboard already controls pane creation (directory picker + name + optional command) but discards those launch parameters after the pane is spawned.
- Users asked for a way to "save current setup" and reload it the next time the deck starts.
- Manual workarounds (custom Zellij layouts, shell scripts) exist but bypass the dashboard and do not integrate with dot-agent-deck's pane metadata, session tracking, or hook auto-configuration.

## Solution Overview

Introduce **auto-save and restore** for the current session:

1. Track every pane's launch metadata (dir, name, command) inside the dashboard while it is running.
2. On exit, automatically persist the current set of open panes to the config file — no explicit save step required.
3. On startup with `--continue`, restore the saved session by recreating each pane using the stored launch metadata. Without the flag, start with a blank slate as today.
4. Session state lives in the config file so it syncs with dotfiles/backups and can be edited manually if needed.

## Scope

### In Scope
- Tracking pane launch metadata (dir, name, command) for panes created via the dashboard.
- Auto-saving the current pane set to the config file (TOML) on dashboard exit.
- `--continue` CLI flag on `dot-agent-deck` to restore the last saved session on startup.
- Graceful handling of missing directories or commands when restoring (warn, skip pane, continue with the rest).
- Documentation updates (README + in-app help overlay) describing session restore.

### Out of Scope
- Named/multiple layouts (future PRD for customizable presets).
- Auto-detection of panes created outside the dashboard (cannot reliably capture launch metadata).
- Synchronizing sessions across machines beyond copying the config file.
- Workspace templating beyond directories/commands/names (e.g., environment variables, hook settings).

## Success Criteria

- The dashboard auto-saves the current pane set to config on exit without user intervention.
- `dot-agent-deck --continue` recreates the saved panes in the correct directories with the correct commands.
- Restored panes are renamed automatically to match the saved names.
- Missing directories/commands are reported to the user without aborting the restore process.
- README documents how `--continue` works.

## Milestones

- [x] Capture pane launch metadata in `UiState` and prune entries when panes close or sessions end.
- [x] Define session schema in `DashboardConfig` (saved panes array) plus load/save helpers and tests.
- [x] Auto-save session state to config on dashboard exit.
- [x] Add `--continue` flag to CLI and drive pane recreation through existing pane controller on startup.
- [x] Handle restore failures gracefully (warnings, partial successes) and surface them via dashboard status messages/logs.
- [ ] Update README/help overlay with session restore instructions and add automated tests covering config serialization + restore flows.

## Key Files

- `src/ui.rs` — Track pane launch metadata, wire session restore into pane creation logic.
- `src/config.rs` — Extend dashboard config with session schema and persistence.
- `src/main.rs` — `--continue` flag and startup restore logic.
- `README.md` — Document `--continue` usage.

## Technical Notes

- Store pane metadata as `HashMap<pane_id, PaneLaunch>` (dir/name/command) in `UiState`. Remove entries when panes close (`KeyResult::ClosePane`) or when `pane.list_panes()` no longer reports them.
- Session schema suggestion:
  ```toml
  [[session.panes]]
  dir = "/repo/api"
  name = "api"
  command = "claude"

  [[session.panes]]
  dir = "/repo/ui"
  name = "ui"
  command = "opencode"
  ```
- On exit: serialize `pane_launch_metadata` values into `config.session.panes` and call `config.save()`.
- On `--continue`: iterate saved panes, call `pane.create_pane()` with stored `command` (empty string → default shell) and rename pane immediately.

## Risks

- **Pane drift**: Panes created outside the dashboard won't be captured; mitigate by clearly stating the requirement in docs.
- **Stale paths**: Saved session may reference directories that no longer exist; ensure restore surfaces per-pane warnings and continues with the rest.
