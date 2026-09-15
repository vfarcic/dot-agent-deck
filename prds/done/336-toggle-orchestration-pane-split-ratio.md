# PRD #336: Toggle orchestration pane-column split ratio

**Status**: Complete
**Priority**: Medium
**Created**: 2026-08-03

## Problem Statement

In orchestration tabs, the sidebar (role list, left) and the pane column (agent terminals, right) split at a fixed ratio: `ORCHESTRATION_LEFT_PERCENT = 34` / `ORCHESTRATION_PANES_PERCENT = 66` (`src/ui.rs`). On a laptop screen this leaves the working pane noticeably narrower than it could be, and there is no quick way to reclaim that width — only a config edit and restart.

This is a companion to [#311](https://github.com/vfarcic/dot-agent-deck/issues/311) (removed the collapsed non-focused frames) and [#313](https://github.com/vfarcic/dot-agent-deck/issues/313) (a toggleable full-width zoom). Neither addresses the everyday case: keep the sidebar visible, just narrower.

## Solution Overview

Add a keybinding action, default `Ctrl+l` (remappable via the existing `keybindings.rs` `Action`/`ActionSpec` system), that toggles the orchestration tab's split between the default 34/66 and a narrower-sidebar 25/75 (1/4 sidebar, 3/4 panes) — and back again on a second press. Scoped to orchestration tabs **in command mode**; everywhere else — in a pane, or on any other tab — the chord is never claimed, so it reaches the focused pane as ordinary input.

## Scope

### In Scope

- A new `Action::ToggleOrchestrationSplit` registered in `src/keybindings.rs`'s `ACTIONS` table, default chord `Ctrl+l`, section `Global`.
- Global (not per-tab) state tracking which ratio orchestration tabs are currently using, starting at 34/66, so one toggle applies to every orchestration tab — including ones opened afterwards.
- The layout call sites that read the `ORCHESTRATION_LEFT_PERCENT` / `ORCHESTRATION_PANES_PERCENT` constants directly resolve the active ratio instead of the fixed constants.
- L1 coverage pinning both ratio states' geometry, the global scope, and the tab/mode-scoping guard.
- A PTY-attached L2 test driving the real chord and asserting the visible column boundary moves and round-trips.
- `docs/keyboard-shortcuts.md` updated with the new binding; changelog fragment added.

### Out of Scope

- [#312](https://github.com/vfarcic/dot-agent-deck/issues/312) (retiring the global stacked/tiled toggle) and [#313](https://github.com/vfarcic/dot-agent-deck/issues/313) (zoom) — unrelated toggles on the same layout seam.
- Dashboard or mode tabs — this ratio is specific to the orchestration tab's sidebar/pane-column split. Extending the toggle to the Dashboard, and to a third "hidden sidebar" stage, is [#361](https://github.com/vfarcic/dot-agent-deck/issues/361).
- More than two ratio states (no cycling through arbitrary splits) — this is a two-state toggle. See #361.
- Persisting the toggled state across restarts — resets to the 34/66 default on next launch.

## Technical Approach

`ORCHESTRATION_LEFT_PERCENT` / `ORCHESTRATION_PANES_PERCENT` remain as the default-state values. A resolver, `orchestration_split_percents(narrow) -> (u16, u16)`, returns `(25, 75)` or `(34, 66)` and is the single source of truth every call site goes through.

**The split is global, and `TabManager` owns it.** Sidebar width is a reading preference, not a property of which orchestration happens to be open. Per-tab state meant every newly opened tab reset to 34/66, so anyone who prefers the narrow sidebar re-toggled forever; the genuinely per-task case ("I want this diff wide *right now*") is #313's zoom, not this toggle. The single source of truth is therefore `TabManager::orchestration_split_narrow: bool`, flipped by `TabManager::toggle_orchestration_split()`. `dispatch_action` calls that and lets the next frame reflow, exactly as `ToggleLayout` does — no resize is pushed from the handler. The handler still guards on the active tab being an orchestration tab, so the chord remains inert everywhere else.

**Global scope without the ordering hazard: the owner is the type that already owns the tabs.** `TabManager` holds both the global flag and `tabs: Vec<Tab>`, so `toggle_orchestration_split` writes the flag *and* every open orchestration tab's `split_narrow` mirror inside one `&mut self` method that cannot be observed half-applied, and the two construction sites (`open_orchestration_tab`, `open_orchestration_tab_with_existing_role_panes`) seed the mirror from the flag. Those are the only writers, both in `src/tab.rs`. The invariant "every `Tab::Orchestration::split_narrow` equals the global" therefore holds at every point any reader could observe it, and no caller has to remember to sync anything before rendering. That last clause is the whole difference from the earlier revision, which mirrored the active tab's flag into a **thread-local**: that was replaced because its correctness rested on unenforced call ordering — a `set` that had to precede a render — not because a global value was wrong. Routing every write through the owner removes the ordering assumption rather than reintroducing it.

**The flag still travels as data through the render path.** `ActiveTabView::Orchestration` carries `split_narrow`, so `compute_frame_layout` stays a pure function of its inputs and needs no `TabManager` reference mid-layout. `orchestration_role_pane_dims` likewise takes `narrow` as an explicit parameter; its only caller is the spawn path, which now passes `tab_manager.orchestration_split_narrow()` so a role PTY opens at the width it will actually be rendered at. A restored or hydrated tab adopts the current global like any other tab — and because the global is not persisted across launches, a restore during startup still lands on the 34/66 default.

**#361** (the same toggle on Dashboard tabs, plus a third hidden-sidebar stage) is neutral-to-easier under this shape: it widens the value on `TabManager` from a bool to a stage enum, or adds a sibling field next to it, and its apply-loop lives in the same method as this one. The concern the earlier revision raised — "a second sync site to the same global" — was specific to the thread-local, where each new surface needed its own set-before-render call at a distance. Here there is no sync site to add, only one more field the owner writes alongside the tabs it already owns.

### Tab and mode scoping

`global_action` stays a pure chord→action table with no tab awareness. A separate pure function, `scope_orchestration_split(action, is_orchestration_tab, mode)`, un-resolves `ToggleOrchestrationSplit` to `None` unless the active tab is an orchestration tab **and** the deck is in command mode. `handle_key_event` applies it at the one point in the funnel that has tab context. Keeping it a standalone function makes it unit-testable without a PTY (`orchestration/layout/005`); an inline `if` would only be reachable through the full event loop.

Both halves of the narrowing exist for the same reason: the default `Ctrl+l` is readline's `clear-screen`, so anything running in a pane has a legitimate claim on it. Off an orchestration tab the action can do nothing, so claiming the chord is pure loss. In a pane on an orchestration tab it would be worse — that is the most likely place to want a screen clear, and the user would get a sidebar resize instead.

**Command-mode scoping mirrors `close_pane`** (PRD #241 M1), which is command-mode only precisely so `Ctrl+w` still reaches the PTY as word-delete while the user is typing. The cost is one extra keystroke (`Ctrl+d` first); the alternative is silently eating a chord people press reflexively — and #361 would widen that swallow to every pane on the deck. Consequently the help overlay lists the binding under "Dashboard (command mode)" rather than "Global (works from any pane)", exactly as PRD #241 review F6 did for `close_pane`.

### Cross-version safety

None. This is TUI-side rendering state: no daemon protocol, no hooks, no orchestration routing, and `Tab` derives no `Serialize`/`Deserialize`, so the new field cannot affect any persisted format. CLAUDE.md rule 12's contract question does not arise. Patch-level bump.

### Experimental flag (rule 9)

**Decision: ships visible by default — no `experimental` gate.** The surface is a single additional keybinding on an existing pane layout, off by default in the sense that nothing changes until the user presses it, fully reversible with a second press, and not persisted. There is no new pane, field, tab, or footer to stage behind a flag, and no partially-built surface a user could stumble into. Accordingly there is no `src/features.rs` wrapper, no note in `docs/develop/experimental-flag.md`, and no `graduate-` follow-up issue. Recorded here so the rule 9 question reads as answered rather than skipped.

## Success Criteria

- In an orchestration tab, pressing the toggle chord once changes the sidebar/pane-column split from 34/66 to 25/75; the sidebar visibly narrows and the pane column visibly widens. ✅ `tabs/orchestration/007`
- Pressing it again returns to the 34/66 default. ✅ `tabs/orchestration/007`, `orchestration/layout/004`
- The toggle is global across orchestration tabs — toggling from one tab changes every other open orchestration tab, and a tab opened afterwards adopts the current split rather than resetting to 34/66. ✅ `orchestration/layout/004`
- No regression to non-orchestration tabs (dashboard, mode tabs) — the toggle has no effect there, and the chord still reaches the focused pane's PTY. ✅ `orchestration/layout/005`, `tabs/orchestration/008`
- In a pane (`PaneInput`) the chord is NOT claimed even on an orchestration tab, so a role agent still receives it. ✅ `orchestration/layout/005`, `tabs/orchestration/007`
- The chord is remappable through the same config mechanism as every other keybinding. ✅ `ACTIONS` entry `toggle_orchestration_split`

## Milestones

- [x] **M1 — Global split-ratio state added.** `TabManager::orchestration_split_narrow` as the single source of truth, starting at the 34/66 ratio and mirrored onto `Tab::Orchestration::split_narrow` at every construction site.
- [x] **M2 — Toggle action wired.** `Action::ToggleOrchestrationSplit` registered with default chord `Ctrl+l`; pressing it in an orchestration tab flips the global ratio state for every orchestration tab.
- [x] **M3 — Layout call sites resolve the active ratio.** `compute_frame_layout` reads the split off the render snapshot; `orchestration_role_pane_dims` takes it as a parameter, which the spawn path fills from the current global.
- [x] **M4 — L1 coverage.** `orchestration/layout/003` (geometry + chord resolution), `/004` (global scope + round trip, including a tab opened after a toggle), `/005` (the scoping guard).
- [x] **M5 — L2 coverage.** `tabs/orchestration/007` drives a real orchestration tab through the PTY and asserts the visible boundary moves and round-trips; `tabs/orchestration/008` proves the chord still reaches a pane's PTY off an orchestration tab.
- [x] **M6 — Docs and changelog.** `docs/keyboard-shortcuts.md` updated (both the quick table and the remappable-actions table); `changelog.d/336.feature.md` added.

## Risks

- **Chord conflicts.** `Ctrl+l` is free in the default `ACTIONS` table, verified against `main`. Note it is *not* free inside a pane — plenty of programs bind it (readline's clear-screen) — which is exactly why the tab-scoping guard and `tabs/orchestration/008` exist.
- **Follow-on rework.** #361 proposes turning this two-state toggle into a three-stage cycle covering Dashboard tabs too. Keeping the owner (`TabManager`) the same type that owns the tabs, and still threading the resolved value as data through the render path, is what keeps that an additive change: #361 widens or duplicates one field on `TabManager` rather than adding a second set-before-render call at a distance.

## Open Questions

1. ~~Does this UI state already have a natural home?~~ Resolved: `TabManager` for the global value, `Tab::Orchestration` for the per-tab mirror the render path reads, `ActiveTabView::Orchestration` for the render snapshot.
2. ~~Should the toggle state survive tab restore?~~ Resolved: a restored tab adopts the current global split like any other tab, so within a session it survives. Across launches it does not — the global itself is not persisted, so a restore during startup lands on the 34/66 default. Persistence stays out of scope.
3. ~~Per-tab or global?~~ Resolved 2026-08-06: global. See the Technical Approach and the work-log entry below.

## Work Log

### 2026-08-03 — Created

Split out of the "1/3 vs 1/4 sidebar width" ask as a quick, scoped toggle. Distinct from #312 (retiring the global layout toggle) and #313 (full zoom) — this is a narrower, additive keybinding on the same layout seam.

### 2026-08-03 — M1-M6 complete

Split state (per-tab at the time — later inverted to global, see below), the `toggle_orchestration_split` action (default `Ctrl+l`), and the layout call sites all landed with L1 and L2 coverage green. Docs and the changelog fragment close out M6.

### 2026-08-04 — Rebased onto `main`; thread-local replaced; scoping guard extracted

PRD #311 landed separately (#334), so its changes were dropped from this branch and the spec ids renumbered around main's (`orchestration/layout/003-005`, `tabs/orchestration/007-008`). Three review findings addressed while rebasing:

- The thread-local mirror of the active tab's flag was replaced by threading `split_narrow` through `ActiveTabView::Orchestration` and an explicit `orchestration_role_pane_dims` parameter.
- The tab-scoping check was extracted from an inline `if` in the event loop into the pure `scope_orchestration_split`, and given its own L1 test — the guard is what keeps `Ctrl+l` from being swallowed on non-orchestration tabs (Greptile P1 on PR #342).
- `Ctrl+l` was missing from the in-app help overlay (`?`) even though the docs advertise that overlay as the full list; it is now listed, qualified as orchestration-tab-only.

`tabs/orchestration/008` was also rewritten: it previously asserted that readline's `clear-screen` wiped a sentinel, which depends on the host's terminal setup and failed on a machine where the forwarding was in fact correct. It now runs `cat -v` and asserts the pane echoes `^L`, observing the forwarded byte directly.

### 2026-08-06 — Split scope inverted from per-tab to GLOBAL

Reviewing the shipped behaviour, the maintainer proposed inverting the scope, and the user decided for **global**. Three reasons on record: sidebar width is a reading preference, not a property of which orchestration is open; per-tab meant every newly opened orchestration tab reset to 34/66, so anyone who prefers the narrow sidebar re-toggles forever; and the transient "I want this diff wide right now" case is per-task and already belongs to #313's zoom, not to this toggle. Values and keybinding are unchanged — 34/66 ↔ 25/75 on `Ctrl+l`, command mode, orchestration tabs only. This is a scope change only.

The implementation resists the obvious trap. A naive global would reinstate exactly what the 2026-08-04 entry removed, so the source of truth went onto `TabManager` — the type that already owns `tabs: Vec<Tab>` — rather than into a thread-local or a free-floating static. `toggle_orchestration_split()` writes the global and every open orchestration tab in one `&mut self` call, and the two tab-construction sites seed each new tab from the global; those are the only writers and both live in `src/tab.rs`. Nothing has to be synced before a render, so the ordering assumption that condemned the thread-local has no place to reappear. `Tab::Orchestration::split_narrow` survives as a per-tab mirror (the render path reads it, and it keeps `compute_frame_layout` a pure function of its inputs) with its invariant documented on the field.

Two behaviours changed alongside the scope: the spawn path now sizes role PTYs from the current global instead of the hardcoded default, so a pane opened while narrow no longer starts at 66% and reflows on the first frame; and a restored or hydrated tab adopts the current global rather than forcing the default — though since the global is not persisted, a restore during startup still lands on 34/66.

`orchestration/layout/004` was inverted to pin the new behaviour (toggle A → open B narrow → toggle from B flips both → toggle from A flips both), and is green. `/003`, `/005` and the `orchestration_role_pane_dims` unit tests confirm the geometry and the command-mode guard are undisturbed.
