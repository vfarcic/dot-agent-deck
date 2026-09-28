# PRD #312: Retire the global stacked/tiled layout toggle

**Status**: Not started
**Priority**: Medium
**Created**: 2026-08-01

> **Update (#1199):** workspace modes were removed in [#1199](https://github.com/vfarcic/dot-agent-deck/issues/1199), so mode tabs no longer exist and `docs/workspace-modes.md` is now a removal stub. Read the mode-tab bullet, the M3 milestone and Open Question 2 below as moot; the toggle now governs two tab types — the dashboard and orchestration tabs.

## Problem Statement

`ui.pane_layout` is a single global field (`src/ui.rs:1531`, default `Stacked`) toggled by `Ctrl+T` (`src/ui.rs:6481`) and read by all three `render_terminal_panes` call sites — dashboard, mode tabs and orchestration tabs. One switch governs three tab types whose needs are not the same, and in two of the three neither setting is right:

- **Orchestration tabs.** `Tiled` divides the pane column equally among every role — with seven roles that is a handful of rows each, unusable. `Stacked` is the only workable setting, and once [#311](https://github.com/vfarcic/dot-agent-deck/issues/311) removes collapsed frames the two become indistinguishable here. The toggle then does nothing.
- **Mode tabs.** Side panes exist to be watched while the agent works — live test and lint output (`docs/workspace-modes.md:8`). `Stacked` collapses them to title rows, defeating the point. `Tiled` is the correct arrangement, and it is not what the default gives you.
- **Dashboard tabs.** Behaviour depends on how many panes happen to be open.

So the user is handed a global mode switch to manage a decision that follows deterministically from which kind of tab they are looking at. `Ctrl+T` is also spent — one of only four global chords (`docs/keyboard-shortcuts.md:25`) — on a setting nobody should have to think about.

## Solution Overview

Remove the toggle and the `toggle_layout` action, and derive the arrangement from the tab type: orchestration tabs show the focused role (post-#311), mode tabs render their side panes simultaneously, dashboard tabs per the decision in #311's Open Question 3.

This frees `Ctrl+T` and removes a user-facing concept rather than replacing it with a different one.

## Scope

### In Scope

- The `Action::ToggleLayout` handler (`src/ui.rs:6481`) and the `toggle_layout` keybinding action.
- The `PaneLayout` enum's role: either it disappears entirely, or it survives as an internal per-tab-type detail rather than user state.
- The `[Toggle Layout Ctrl+T]` button in the persistent button bar (`docs/keyboard-shortcuts.md:12`) — a clickable affordance, so removal has to cover the mouse path too.
- Documentation: `docs/keyboard-shortcuts.md` lines 25, 145 and 161 all describe the toggle.

### Out of Scope

- The zoom toggle ([#313](https://github.com/vfarcic/dot-agent-deck/issues/313)), which is a different feature that happens to want a keybinding.
- Changing what any tab type renders beyond what #311 already establishes.

## Technical Approach

`PaneLayout` currently threads through `pane_stack_rects`, `resize_panes_to_layout`, `cards_pane_rects`, `orchestration_role_pane_dims`, `dashboard_pane_dims` and the spawn-dims helpers. Whether it is deleted or demoted to a per-tab-type constant, the compiler enumerates the call sites — which is the reason to make this a type-level change rather than hardcoding a value at each site.

### Migration

A user with `toggle_layout = "..."` in `.dot-agent-deck.toml` is already handled: unknown action names in the `[keybindings]` config produce a warning and are ignored rather than failing startup, pinned by `unknown_action_ignored_with_warning` (`src/keybindings.rs:1384`). So no migration path is needed — but the warning text should be worth reading, and the removal belongs in the changelog since it is a documented shortcut disappearing.

### Cross-version safety

None — TUI-only, no daemon protocol involvement. Per CLAUDE.md rule 12 this is not a compatibility break; per the bump policy it is a feature-level change while in `0.x`, so patch.

## Success Criteria

- `Ctrl+T` no longer toggles anything, and nothing in the UI advertises that it does.
- Each tab type renders correctly with no user action: orchestration tabs show the focused role full-height, mode tabs show their side panes simultaneously.
- A config still carrying `toggle_layout` starts cleanly with a readable warning.
- No orphaned `PaneLayout` plumbing left behind — if the enum survives, every remaining use is justified.

## Milestones

- [ ] **M1 — Arrangement derives from tab type.** Each call site resolves its own arrangement; the global field no longer drives rendering.
- [ ] **M2 — Toggle removed.** Action, handler, keybinding entry and button-bar affordance all gone.
- [ ] **M3 — Mode tabs verified.** Side panes render simultaneously by default, which is the case the old default got wrong.
- [ ] **M4 — Tests updated.** The existing `PaneLayout` tests (`src/ui.rs:15618`, `:17513` and neighbours) are re-pointed or retired; L1 snapshots cover each tab type's arrangement.
- [ ] **M5 — Docs and changelog.** All three `docs/keyboard-shortcuts.md` references removed, changelog fragment noting the removed shortcut.

## Risks

- **Someone relies on `Tiled` in an orchestration tab.** Watching several roles at once is a legitimate thing to want, and this removes the only way to do it. If that use case matters, it argues for a per-tab arrangement control rather than none — worth asking before deleting.
- **Wide test surface.** `PaneLayout` appears throughout `src/ui.rs`'s test module; the risk is churn, not behaviour.
- **Sequencing.** Landing this before #311 means reasoning about an arrangement that is about to change. Do #311 first.

## Open Questions

1. **Does `PaneLayout` survive as an internal concept, or disappear?** Keeping it as a per-tab-type constant preserves the geometry helpers unchanged and keeps the door open for #313's zoom state; deleting it is a larger, cleaner change.
2. **Is the mode-tab default currently wrong?** If `Stacked` is the default and mode tabs read it, side panes have been collapsing by default all along — a latent bug this PRD would incidentally fix. Confirm before writing it up as intended behaviour.
3. **Does anything replace `Ctrl+T`, or does it stay free?** #313 wants a binding; reusing the muscle memory of an adjacent chord may or may not be a good idea.

## Work Log

### 2026-08-01 — Created

Split out of the [#307](https://github.com/vfarcic/dot-agent-deck/issues/307) discussion. Depends on [#311](https://github.com/vfarcic/dot-agent-deck/issues/311).
