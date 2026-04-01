# PRD #32: Saved Pane Layouts

**Status**: Draft
**Priority**: Medium
**Created**: 2026-04-02
**GitHub Issue**: [#32](https://github.com/vfarcic/dot-agent-deck/issues/32)

## Problem Statement

Launching `dot-agent-deck` always starts from a blank slate: users must re-open every agent pane, reselect directories, re-enter pane names, and retype commands every time the dashboard is launched. This slows down adoption for people who work with a fixed set of repositories or long-running agents, and introduces errors when a pane is configured differently than expected.

## Background

- The dashboard already controls pane creation (directory picker + name + optional command) but discards those launch parameters after the pane is spawned.
- Users asked for a way to “save current setup” and reload it the next time the deck starts.
- Manual workarounds (custom Zellij layouts, shell scripts) exist but bypass the dashboard and do not integrate with dot-agent-deck’s pane metadata, session tracking, or hook auto-configuration.

## Solution Overview

Introduce **Saved Layouts**:

1. Capture every pane’s launch metadata (dir, name, command) inside the dashboard while it is running.
2. Provide CLI commands to save the current set of open panes as a named layout, list existing layouts, and delete them.
3. When dot-agent-deck starts, detect saved layouts and offer to restore one (resume last-used, pick from list, or start empty). Restoring a layout programmatically recreates each pane using the stored launch metadata and renames panes to match the saved names.
4. Layouts live in the config file so they sync with dotfiles/backups and can be edited manually if needed.

## Scope

### In Scope
- Tracking pane launch metadata (dir, name, command) for panes created via the dashboard.
- Persisting layouts to the existing config location (TOML) with a schema that supports multiple named layouts and timestamps.
- CLI subcommands: `layouts save <name>`, `layouts list`, `layouts remove <name>`.
- Startup UX that prompts the user to pick a saved layout (default to “empty”); include an option to auto-launch the most recently used layout without prompting.
- Graceful handling of missing directories or commands when restoring (warn, skip pane, continue with the rest).
- Documentation updates (README + in-app help overlay) describing layouts.

### Out of Scope
- Auto-detection of panes created outside the dashboard (cannot reliably capture launch metadata).
- Synchronizing layouts across machines beyond copying the config file.
- Workspace templating beyond directories/commands/names (e.g., environment variables, hook settings).

## Success Criteria

- Users can save the current set of panes as a named layout via CLI while the dashboard is running.
- On launch, the dashboard surfaces available layouts and can recreate them without user re-entry.
- Restored panes are renamed automatically and inherit the saved commands and directories.
- Missing directories/commands are reported to the user without aborting the restore process.
- README documents how to save, list, delete, and restore layouts.

## Milestones

- [ ] Capture pane launch metadata in `UiState` and prune entries when panes close or sessions end.
- [ ] Define layout schema in `DashboardConfig` (layouts array with name, panes, timestamps) plus load/save helpers and tests.
- [ ] Implement `dot-agent-deck layouts save/list/remove` CLI subcommands (updates config, surfaces errors nicely).
- [ ] Add startup layout picker: prompt user, handle “resume last used,” and drive pane recreation through existing pane controller.
- [ ] Handle restore failures gracefully (warnings, partial successes) and surface them via dashboard status messages/logs.
- [ ] Update README/help overlay with Saved Layouts instructions and add automated tests covering config serialization + CLI flows.

## Key Files

- `src/ui.rs` — Track pane launch metadata, wire layout restore into pane creation logic, UI prompt.
- `src/config.rs` — Extend dashboard config with layout schema and persistence.
- `src/main.rs` — CLI plumbing for layout subcommands and startup prompt.
- `README.md` — Document how to save and restore layouts.

## Technical Notes

- Store pane metadata as `HashMap<pane_id, PaneLaunch>` (dir/name/command). Remove entries when panes close (`KeyResult::ClosePane`) or when `pane.list_panes()` no longer reports them.
- Layout schema suggestion:
  ```toml
  [[layouts]]
  name = "client-work"
  last_used = "2026-04-02T09:15:00Z"
  panes = [
    { dir = "/repo/api", name = "api", command = "claude" },
    { dir = "/repo/ui", name = "ui", command = "opencode" }
  ]
  ```
- Restoring: iterate saved panes, call `pane.create_pane()` with stored `command` (empty string → default shell) and rename pane immediately.
- CLI commands can run out of process (non-TUI) and talk to the daemon via the existing socket to fetch current pane metadata.

## Risks

- **Pane drift**: Panes created outside the dashboard won’t be captured; mitigate by clearly stating the requirement in docs.
- **Stale paths**: Layouts may reference directories that no longer exist; ensure restore surfaces per-pane warnings and continues with the rest.
- **Prompt fatigue**: Startup prompt should be lightweight and dismissible; offer an environment variable or config to auto-load last layout for power users.
