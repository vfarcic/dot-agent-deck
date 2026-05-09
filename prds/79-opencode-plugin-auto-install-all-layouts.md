# PRD #79: OpenCode plugin auto-install must refresh every existing layout

**Status**: Not started
**Priority**: Medium
**Created**: 2026-05-09

## Problem

`auto_install` (`src/opencode_manage.rs:386-409`) refreshes the OpenCode plugin in exactly one location: whatever `detect_opencode_root` returns. That helper checks `~/.config/opencode/` first, falls back to `~/.opencode/`, and otherwise returns `None`. The decision is based purely on whether the **directory** exists.

When *both* roots exist on a user's machine, the helper unconditionally picks XDG. But OpenCode itself does not necessarily load plugins from XDG — it loads them from wherever the user's `opencode.jsonc` lives, which on long-standing installs is frequently `~/.opencode/`. The `~/.config/opencode/plugin/` subtree may exist for a totally unrelated reason: a *prior* run of `auto_install` created it as a side effect.

The result is a silent failure mode:

1. A previous (post-PR-#72) `auto_install` writes a working plugin to `~/.config/opencode/plugin/dot-agent-deck/index.js`. This creates the `~/.config/opencode/` directory.
2. The user's actual OpenCode config (`~/.opencode/opencode.jsonc`) explicitly references `~/.opencode/plugin/dot-agent-deck/index.js`. That file was written by an even earlier `auto_install` run from a now-deleted worktree, so its `BINARY_PATH` constant is a stale absolute path.
3. Every subsequent `auto_install` finds `~/.config/opencode/` first, writes only there, and never refreshes the legacy plugin.
4. The legacy plugin's `execFileSync` throws `ENOENT`. The plugin swallows the error in `try { … } catch (_) {}` (`src/opencode_manage.rs:76-84`).
5. No events reach the daemon. Dashboard cards for OpenCode panes stay on `AgentType::None` (`src/state.rs:260-262`), which the card renderer displays as "No agent" with `Tools: 0` (`src/ui.rs:4919`).

PR #72 (commit `d058647`) fixed the inverse case — XDG-only configs that were being ignored when `auto_install` only knew about `~/.opencode/`. It did not anticipate the case where both roots already coexist.

## Solution

Stop treating "first directory that exists" as a proxy for "the root OpenCode actually uses." Instead, refresh **every** existing plugin layout, so whichever one OpenCode loads is guaranteed to point at the running binary.

Two viable shapes; v1 should pick (a) for simplicity:

a. **Write to all existing layouts.** On every `auto_install` run, enumerate the candidate roots (`~/.config/opencode`, `~/.opencode`, plus any future locations) and write the plugin file into each one whose root directory exists. If neither exists, fall back to the current XDG-default-create behavior. This mirrors what `existing_plugin_dirs` (`src/opencode_manage.rs:52-61`) already does for `uninstall`.

b. **Detect the active root from `opencode.jsonc`.** Look for `opencode.jsonc` (not `plugin/`) under each candidate root and target that one. More precise but more brittle (config can be elsewhere, can be JSONC-with-comments, etc.).

Approach (a) is preferred: it is idempotent, cheap, requires no parsing, and degrades gracefully if OpenCode adds yet another layout in the future. Writing twice is a no-op when one of the two locations is unused.

### Design decisions

- **Mirror `uninstall` semantics.** `uninstall` already sweeps every existing layout (`existing_plugin_dirs`). `install` / `auto_install` should be symmetric. The asymmetry is the bug.
- **No active-root detection in v1.** `opencode.jsonc` parsing is out of scope. Approach (a) sidesteps the question entirely.
- **`auto_install` is silent on success and warn-only on failure.** Existing behavior. Per-target failures should not abort the other targets — log a `tracing::warn!` and continue.
- **Explicit `install` (CLI subcommand) gets the same fan-out.** Today it targets `plugin_dir_for_install`. After this change it should also write to every existing layout, falling back to XDG-default when none exists. Keeps `install` and `auto_install` in lockstep.
- **No migration / cleanup of stale plugins.** Out of scope. Users with both layouts keep both layouts; we just keep both up to date. `uninstall` is already the path for cleaning up.
- **Document the new fan-out behavior in the changelog.** Users who are explicitly maintaining one of the two layouts should understand that `dot-agent-deck` will start writing to the other one too if it exists.

## Acceptance Criteria

### Auto-install fan-out
- [ ] When both `~/.config/opencode/` and `~/.opencode/` exist, `auto_install` writes the plugin file into **both** `~/.config/opencode/plugin/dot-agent-deck/index.js` and `~/.opencode/plugin/dot-agent-deck/index.js`.
- [ ] When only one of the two roots exists, `auto_install` writes only to that one (current behavior preserved).
- [ ] When neither root exists, `auto_install` is a no-op (current behavior preserved). It does **not** create either directory speculatively.
- [ ] Every written plugin file has its `BINARY_PATH` set to the value returned by `std::env::current_exe()` (current behavior preserved).

### Explicit install fan-out
- [ ] `dot-agent-deck install` (the CLI subcommand) follows the same fan-out rule: writes to every existing layout, or falls back to the XDG default if neither exists.
- [ ] Stdout messages name every path written, one per line (parity with the existing single-line `Installed OpenCode plugin: <path>` message).

### Resilience
- [ ] If writing to one layout fails (permission error, etc.), the other layout is still written. Failures are logged via `tracing::warn!` (auto-install) or surfaced as the function's `io::Result` (explicit install) — current handling pattern preserved per call site.
- [ ] Repeated `auto_install` runs are idempotent and overwrite previous plugin contents in every layout.

### Tests
- [ ] Unit test: both layouts present → both files written, both with the expected `BINARY_PATH`.
- [ ] Unit test: only legacy present → only legacy file written; XDG path is not created.
- [ ] Unit test: only XDG present → only XDG file written; legacy path is not created.
- [ ] Unit test: neither present → no files written; no directories created.
- [ ] Existing `auto_install_*` and `install_*` tests continue to pass without modification, or are updated alongside the new behavior.

### Manual validation
- [ ] On a machine with both layouts and an OpenCode config at `~/.opencode/opencode.jsonc` referencing the legacy plugin path, after running the dashboard once: the legacy plugin's `BINARY_PATH` matches `which dot-agent-deck`, and OpenCode panes' dashboard cards transition out of "No agent" / `Tools: 0` on first hook event.

## Out of Scope

- Parsing `opencode.jsonc` to discover the actual plugin path. Future work if approach (a) proves insufficient.
- Migrating users from one layout to the other. Users keep whatever layout(s) they already have.
- Auto-cleanup of stale plugins for deleted worktree binaries. `uninstall` remains the cleanup path.
- Changes to the plugin's runtime error handling (the `try { … } catch (_) {}` swallow). Surfacing those errors is a separate concern — see Risks.
- Hardening hook delivery against silent failure (e.g., adding a heartbeat or unrouted-event log). Tracked separately if it ever becomes a frequent diagnostic gap.
- Cross-platform behavior changes. macOS + Linux only; Windows OpenCode plugin support is not addressed by this PRD.

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Writing to a layout the user doesn't actually use feels like clutter | Already aligns with how `uninstall` treats both layouts. Both files are tiny (~8 KB) and idempotent — writing one extra copy is functionally invisible. |
| User has manually edited one of the plugin files (e.g., for debugging) | `auto_install` already overwrites unconditionally — no regression. Document in the changelog that the dashboard always rewrites the plugin on startup. |
| Future OpenCode layout (a third root path) is added and we silently miss it | Centralize the candidate-root list in one place (mirroring `existing_plugin_dirs`) so adding a new layout is a one-line change. |
| The plugin's swallow-all `catch (_) {}` means even a fixed plugin can fail silently in other ways | Out of scope here, but worth flagging: if the user's OpenCode config explicitly references a plugin path that does not exist on disk (e.g., they renamed `~/.opencode/`), this PRD does not detect that. Consider a future diagnostic mode. |
| GitHub issue numbering: this PRD is #79 because #78 was already in flight locally | Cosmetic; no functional impact. |

## Implementation Notes

- All changes localize to `src/opencode_manage.rs`.
- Introduce a `candidate_roots()` helper that returns `[xdg_root, legacy_root]` unconditionally (existence check happens at the call site). This becomes the shared list used by `existing_plugin_dirs` (uninstall), the new fan-out for `auto_install`, and the new fan-out for `install`.
- `auto_install` becomes: enumerate candidate roots → keep the ones whose root directory `.exists()` → for each, ensure `plugin/dot-agent-deck/` exists and write `index.js`. If the resulting list is empty, return early (current no-op behavior). No fallback creation when nothing exists — same as today.
- `install` (the explicit CLI version) becomes: same enumeration, but if the list is empty, fall back to `plugin_dir_for_install()` and create the XDG-default location (current first-time-install behavior).
- Test seam: refactor `auto_install_to` (currently `#[cfg(test)]`) into something that takes a slice of candidate roots and a target dir generator, so the new multi-target tests can drive it without touching the real `$HOME`.
- No public API change. `install_to`, `uninstall_from` keep their current single-path signatures for direct test use.
- No new dependencies. No JSON/JSONC parsing.

## References

- `src/opencode_manage.rs:20-39` — `detect_opencode_root` and `detect_opencode_root_in`
- `src/opencode_manage.rs:42-61` — `plugin_dir_for_install` and `existing_plugin_dirs` (the existing fan-out for uninstall)
- `src/opencode_manage.rs:386-409` — `auto_install` (call site to update)
- `src/opencode_manage.rs:427-433` — `install` (call site to update)
- `src/opencode_manage.rs:63-359` — `plugin_template` (no changes; produces the JS plugin written into each layout)
- `src/state.rs:260-262` — placeholder → real `agent_type` upgrade on first event
- `src/ui.rs:4919-4924` — "No agent" rendering when `agent_type == AgentType::None`
- PR #72 / commit `d058647` — added XDG detection, established the asymmetry this PRD removes
