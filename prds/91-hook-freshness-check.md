# PRD #91: Hook freshness check on TUI startup

**Status**: Planning
**Priority**: Medium
**Created**: 2026-05-17
**GitHub Issue**: [#91](https://github.com/vfarcic/dot-agent-deck/issues/91)
**Related to**: PRD #90 (Remote daemon upgrade) — orthogonal; either can land first.

## Problem Statement

`dot-agent-deck hooks install` (`src/hooks_manage.rs`) edits `~/.claude/settings.json` to register `<binary-path> hook` for each entry in `HOOK_TYPES` (currently: `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Notification`, `Stop`, `PreCompact`, `SubagentStart`, `SubagentStop`). It runs:

- **Automatically:** during `dot-agent-deck remote add` (`src/remote.rs:824`).
- **Manually:** never. Nothing else triggers it.

That means the following user scenarios end up with **stale or missing hook registrations** and no visible signal that anything is wrong:

1. **Local installs.** Users who install with `cargo install` or build from source and copy the binary into `$PATH` get no hooks until they read the README and run `dot-agent-deck hooks install` themselves. The project maintainer just hit this in PRD #76 testing — the TUI ran, the agents ran, but every dashboard card showed `Tools: 0` and `Last:` frozen at spawn time because no hook events ever fired.
2. **Manual remote installs** (scp the binary instead of `remote add`). Same outcome.
3. **Binary upgrades.** If a future release adds a new hook type to `HOOK_TYPES` (e.g. when `SubagentStart` / `SubagentStop` were added — see `src/hooks_manage.rs:14-15`), every existing install needs `hooks install` re-run. Today nothing detects this; events for the new type silently never reach the daemon.
4. **Broken settings.json.** A user (or another tool) edits `~/.claude/settings.json` and removes a hook, or the file gets corrupted. The TUI has no idea.
5. **Binary path drift.** A user moves `dot-agent-deck` to a different location. The hook commands still reference the old path. Same silent failure.

The symptom in all five cases is identical: dashboard cards never update, no events visible, no error message. Without a startup signal, every new user / upgrader runs into "why isn't this working?" before the README link to `hooks install`.

## Solution Overview

At TUI startup, compute the **expected** hook-rule set for the current binary (same logic `install_impl` uses), read the **actual** `~/.claude/settings.json` (per `settings_path()` in `src/hooks_manage.rs:18-21`), and compare. Three outcomes:

1. **All current.** Silent; no signal.
2. **Missing or stale.** Surface a clear, dismissable warning in the TUI status line / first-time banner: "Hook registration is stale. Run `dot-agent-deck hooks install`." Provide a TUI keybinding to run install now (optional, see Scope).
3. **`settings.json` unreadable / malformed.** Warn with the specific reason; don't auto-fix (the user may be in the middle of editing the file).

Cleanly separable from PRD #90 (remote daemon upgrade): this PRD runs **wherever the TUI runs**, including local-mode and the laptop-side TUI when connecting to a remote. PRD #90 covers the remote upgrade flow that *also* re-runs `hooks install` as a side effect.

## Scope

### In Scope

- **Hook-freshness checker function** in `src/hooks_manage.rs`: given the current binary path, return a list of `HookCheckIssue` (missing type, stale command, malformed entry).
- **TUI startup invocation**: call the checker once at startup, surface results in the dashboard's session-warnings area (the same `ui.session_warnings` vector used today for restore failures, see `src/ui.rs:3033-3036` for a similar pattern).
- **TUI dismissal**: warnings persist until the user dismisses them or runs install (auto-clears when re-check passes).
- **Optional: keybinding to run `hooks install`** from inside the TUI (e.g. from a help menu / dashboard prompt) — saves the user from quitting + re-running + reconnecting.
- **Compatibility with concurrent edits**: the check is read-only. If `settings.json` is locked or in the process of being written by another tool, the check warns "could not read settings.json (will retry on next start)" but does not block startup.
- **Tests** for the checker (missing type, stale path, malformed entry, healthy state) and for the TUI-side surfacing (the warning appears, dismisses correctly, clears on re-check).

### Out of Scope

- **Auto-install without user consent.** The user-visible side effect of `hooks install` is editing `~/.claude/settings.json` — a user-owned config file. We notify but don't mutate without explicit consent (either by them running the CLI or pressing the TUI keybinding).
- **Migrating away from `~/.claude/settings.json`.** Claude Code's hook config lives there. If/when Claude Code adds a hooks plugin manifest format, that's a separate concern.
- **Versioned hook manifest in `settings.json`.** A `"dot-agent-deck-hook-version": "0.x.y"` stamp would simplify staleness detection. Useful but not required for the first cut — comparing the rule set against `HOOK_TYPES` is sufficient.
- **Cross-machine settings sync.** Users with multiple machines need to run `hooks install` on each (or use `remote add`).
- **Notifying about Claude Code hook system changes** (i.e., upstream changes to what hook types exist). The check is against our own `HOOK_TYPES` constant; if Claude Code deprecates a type we register, that's a separate detection problem.

## Success Criteria

- A user who builds dot-agent-deck locally and runs the TUI for the first time sees a clear warning that hooks are not installed, with the exact command to run.
- After running `dot-agent-deck hooks install`, the warning clears on the next TUI start (or, if the in-TUI keybinding lands, immediately on next render).
- A user who upgrades the binary across a release that adds a new hook type sees a warning that registration is stale until they re-run install.
- A user whose `~/.claude/settings.json` becomes unreadable sees a specific reason in the warning, not a generic "something's wrong."
- The check adds negligible startup latency (read one small JSON file, compare to a static list).
- All three gates pass: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## Milestones

### Phase 1: Checker

- [ ] **M1.1** — Add `check_hooks(binary_path: &str) -> Vec<HookCheckIssue>` to `src/hooks_manage.rs`. Returns an empty `Vec` when healthy. Variants: `MissingType(&'static str)`, `StaleCommand { hook_type, found, expected }`, `MalformedEntry { hook_type, detail }`, `SettingsUnreadable(io::Error)`.
- [ ] **M1.2** — Unit tests cover: empty settings (everything missing), healthy state, one missing type, stale command path, malformed entry (string instead of object), unreadable file.

### Phase 2: TUI integration

- [ ] **M2.1** — At TUI startup, call `check_hooks` and push a single human-readable summary into `ui.session_warnings` if non-empty. Wording: "Hook setup incomplete: run `dot-agent-deck hooks install`. (N issue(s): …)" with the most user-relevant first.
- [ ] **M2.2** — Warning persists across renders until dismissed or until a re-check returns clean. Re-check fires whenever the user comes back to the dashboard from another tab.
- [ ] **M2.3** — Tests for the surfacing logic (`tests/ui/...` or inline) cover: warning appears with bad state, clears on healthy re-check, doesn't reappear after dismissal until the underlying state changes.

### Phase 3 (optional): TUI keybinding to run install

- [ ] **M3.1** — Add a help-menu entry "Install hooks" that runs `dot-agent-deck hooks install` as a subprocess and re-checks on return. Skip if Phase 1+2 ship without it.

### Phase 4: Docs

- [ ] **M4.1** — Update `docs/installation.md` (or wherever installation steps live) to mention `hooks install` for non-`remote add` paths. Cross-reference the warning UX.

## Key Files

- `src/hooks_manage.rs` — new `check_hooks` function alongside the existing `install` / `uninstall`.
- `src/ui.rs` — startup hook into the checker; warning surfacing.
- `src/main.rs` — no changes expected; the existing TUI entry point already owns startup-time wiring.
- `tests/hooks_manage.rs` (new or extended) — checker tests.

## Design Decisions

### 2026-05-17: Notify-only, don't auto-install

`hooks install` writes to a user-owned config file (`~/.claude/settings.json`). The principle of least surprise says we don't silently mutate it on TUI startup. An explicit "press X to install" keybinding gives the same convenience without ambushing the user. Future work could add a `--auto-install-hooks` config flag if there's user demand.

### 2026-05-17: Separate from PRD #90

PRD #90 (remote daemon upgrade) re-runs `hooks install` as part of the upgrade flow, but that only catches users on the `remote upgrade` happy path. This PRD catches every other path: local installs, manual scp, dev-loop binaries, edited settings.json, partial upgrades. The two PRDs are orthogonal and either can ship first.

### 2026-05-17: Comparison strategy

The check compares the rule set in `settings.json` against the current binary's `HOOK_TYPES` and computed expected-command string (`<binary_path> hook`). Looks for: every type in `HOOK_TYPES` present and pointing at the current binary; no extra dot-agent-deck entries for types we no longer use. A `dot-agent-deck-hook-version` stamp would be cleaner but adds a write-on-install side effect; skip for now.
