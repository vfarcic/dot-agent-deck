# PRD #83: Per-tab selection state (deck/pane focus memory)

**Status**: Defined; ready to implement
**Priority**: Medium
**Created**: 2026-05-10
**GitHub Issue**: [#83](https://github.com/vfarcic/dot-agent-deck/issues/83)

## Problem

Switching between tabs does not restore the deck (dashboard card) or pane focus the user had the last time they were in that tab. Selection bleeds across tabs: whatever was last selected anywhere in the app is what shows up when you land on any tab.

Concretely, the user's experience today:

1. On the Dashboard tab, select session card B (was on A).
2. Switch to a Mode tab. Focus a side pane (say, side pane #2).
3. Switch back to Dashboard. The card highlight may now be on whatever session pointed to the *Mode tab's* focused pane — not on B.
4. Switch to a different Mode tab. The "focused" pane border is still pointing at the previous tab's pane #2, even though that pane isn't visible here.
5. Switch to the original Mode tab. Focus has not been restored to its side pane #2 — input goes wherever the global focus last landed.

The user expects each tab to remember its own selection and restore it on switch-in.

### Why this is happening (current architecture)

Three pieces of state are involved, none of them tab-scoped in a useful way:

- **`UiState.selected_index`** (`src/ui.rs:463`) is one `usize` for the whole app. It indexes into the *currently rendered* filtered session list. Switching tabs never snapshots or restores it; it just gets clamped against the new tab's filtered count.
- **`EmbeddedPaneController`'s pane focus** (`is_focused` per pane in the registry, `src/embedded_pane.rs`) is global — exactly one pane is "focused" across the entire process. Switching tabs does not move this. Whatever pane was focused last (possibly in another tab) stays focused, which determines where keystrokes go and which border is highlighted.
- **`Tab::Mode.focused_side_pane_index`** (`src/tab.rs:65`) *is* per-tab, but it's a positional `Option<usize>` — fragile if the pane pool changes — and it is treated as a visual hint at render time rather than the source of truth that drives `pane.focus_pane(...)` on tab switch. `Tab::Orchestration` has no analogous field at all.

In addition, a render-time block at `src/ui.rs:2248-2258` *overwrites* `ui.selected_index` to match the global focused pane ID. Combined with the global-focus leak, this is the mechanism by which "the deck selected in one tab appears selected in all tabs."

### Why this matters

- It breaks the user's mental model. Tabs are workspaces; a workspace that doesn't remember "where I was" is a workspace that has to be re-navigated every time.
- It compounds with growing tab counts. The more tabs (Dashboard + N Mode tabs + M Orchestration tabs), the more disorienting the cross-tab leak becomes.
- It silently makes keystrokes go to the wrong place. After a tab switch, typing into "the focused pane" can hit a pane in a tab the user isn't even looking at, because the global pane focus is whatever was last set.

## Solution

Make per-tab selection state authoritative on each `Tab` variant, keyed by **stable IDs** (session id for dashboard cards, pane id for tab panes). On `switch_to`, restore both the visual selection and the actual `EmbeddedPaneController` pane focus from the destination tab's stored selection. Fall back to a sensible default when the remembered id no longer exists.

Stable IDs (not indices) because filter changes, sort changes, session restarts, and reactive pane recreation all invalidate positional indices but preserve session/pane IDs.

### Shape of the change

- **`Tab::Dashboard`** gains `selected_session_id: Option<String>` — the session whose card was last selected on the dashboard.
- **`Tab::Mode`** replaces (or augments) `focused_side_pane_index: Option<usize>` with `focused_pane_id: Option<String>`. `None` means the agent pane has focus; `Some(pane_id)` means that side pane has focus. Switching to this tab calls `pane.focus_pane(focused_pane_id.unwrap_or(agent_pane_id))`.
- **`Tab::Orchestration`** gains `focused_role_pane_id: Option<String>` with the same restore semantics.
- **`TabManager::switch_to`** (or a new `restore_focus` helper called by callers) takes a `&dyn PaneController` and restores the destination tab's focus. Currently `switch_to` is a pure index update; the focus restore must run alongside it whenever a tab switch happens (Tab/Shift+Tab, Left/Right/h/l, Ctrl+PageUp/Down, click-on-tab-bar, programmatic).
- **The render-time sync** at `src/ui.rs:2248-2258` is gated to only run when the active tab is `Tab::Dashboard`, and writes back into `Tab::Dashboard.selected_session_id` (not just into `UiState.selected_index`). On the Dashboard tab, `selected_index` becomes a *derived* value computed from `selected_session_id` against the current filtered list each frame — not the source of truth.
- **Fallback when an ID is gone** (session ended, pane closed): clear the field and select the first item if any, otherwise nothing. No "remember the index where it used to be" — that's the failure mode this PRD is replacing.

### Out of scope

- Persisting per-tab selection across deck restarts. In-memory only; restart resets to defaults.
- Per-tab filter/sort/scroll state. This PRD is about *selection* (which deck/pane is "current"). Filter text and scroll offset can be addressed separately if needed.
- Changing the visual styling of the selected card or focused pane border. This PRD does not touch render appearance, only *which* card/pane gets the existing styling.
- New keybindings. The existing j/k/Tab/Enter behaviour stays; only the *state behind it* changes.
- Multi-select (selecting multiple cards/panes at once).

## Milestones

- [ ] **M1 — Per-tab selection fields, stable-id based.** Add `selected_session_id` to `Tab::Dashboard`, change `Tab::Mode` to track focus by `focused_pane_id: Option<String>` (preserving existing j/k/Esc/Enter semantics), add `focused_role_pane_id` to `Tab::Orchestration`. Update all read sites that today consume `focused_side_pane_index` / `selected_index` to read from the new fields. Existing keyboard handlers that *write* selection (j/k on dashboard, j/k/Esc/Enter on Mode tab side panes, anything analogous on Orchestration tabs) write to the per-tab field.
- [ ] **M2 — Restore focus on tab switch.** Plumb a focus-restore call through every tab-switch entry point (`Tab`/`Shift+Tab`, `Left`/`Right`/`h`/`l`, `Ctrl+PageUp`/`PageDown`, mouse tab-bar click if present, programmatic switches like post-`open_mode_tab`). On switch-in, call `pane.focus_pane(...)` with the destination tab's remembered pane id (or its agent pane / first role pane / no-op for Dashboard). On switch-out, capture the current focused pane id from the embedded controller into the source tab's per-tab field if it has changed since last sync.
- [ ] **M3 — Gate the dashboard-selection-from-focused-pane sync.** The block at `src/ui.rs:2248-2258` that snaps `selected_index` to the global focused pane only runs when `tab_manager.active_tab()` is `Tab::Dashboard`, and writes to `Tab::Dashboard.selected_session_id` rather than directly to `UiState.selected_index`. `UiState.selected_index` becomes a per-frame derived value (lookup of `selected_session_id` in the current filtered list, fallback to 0).
- [ ] **M4 — Stale-id fallback paths.** When a remembered session id is no longer in the filtered list (session ended, filter excludes it), or a remembered pane id is no longer in the controller's pane set (pane closed, reactive pane recreated), clear the field and default to the first item / agent pane. Verify the reactive-pane-recreation path at `src/ui.rs:2163-2178` still works, since today it clamps a positional index — under the new model it should map old-id → new-id where the change is known, otherwise clear.
- [ ] **M5 — Tests.** Add unit tests in `src/tab.rs` for per-tab selection round-trips and stale-id fallback. Add an integration-style test that drives multi-tab switching and asserts each tab restores its own selection. If `EmbeddedPaneController`'s focus mock is sufficient, assert the controller-level focus call too — not just the per-tab field.
- [ ] **M6 — Pre-PR validation.** Single end-to-end pass per `feedback_validate_pre_pr.md`: open Dashboard + ≥2 Mode tabs + ≥1 Orchestration tab, walk through the failure scenario from the Problem section in order, confirm each tab restores its own deck/pane on switch-in. Then push and open the PR.

## Validation Strategy

The bug is purely interactive — no metric, no log, just "does the right thing happen when I switch tabs." Validation is the user driving the failure scenarios from the Problem section and confirming each tab now retains its own selection across switches. Per `feedback_validate_pre_pr.md`, this is a single pass before PR, not per-milestone.

Automated test coverage (M5) protects against regression in the per-tab state fields and stale-id fallback, but does not replace the interactive pass — the bug surfaced as a UX issue and must be confirmed fixed as a UX issue.

## Risks & Mitigations

| Risk | Mitigation |
|---|---|
| Replacing `focused_side_pane_index: Option<usize>` with a pane id breaks one of the many call sites that read it (rendering, keyboard handlers, reactive-pane-pool clamp logic) | Migrate the field name; let the compiler list every read site. Audit each: render uses are translated to "is this id the focused id"; index-arithmetic uses (the j/k step logic) translate to "find current id's position, step, look up new id". |
| Reactive pane recreation (`src/ui.rs:2163-2178`) currently clamps a positional index when the pool shrinks. Under stable ids, the focused pane id may simply *vanish* mid-session without a successor | Use the `(closed_pane_id, new_pane_id)` pairs already returned by `route_reactive_commands` (`src/tab.rs:373`) to remap focus when known; when no successor exists, fall through to the M4 fallback (clear + default). |
| Restoring `pane.focus_pane(...)` on every tab switch may double-fire if some other code path also focuses panes during the switch (e.g. an autofocus-on-create path) | Capture the focused pane id once at switch-out and restore once at switch-in; do not also let render-time logic call `focus_pane`. Verify by grepping `pane.focus_pane(` and checking each call's trigger. |
| The render-time sync at `src/ui.rs:2248-2258` exists for a reason — it keeps the dashboard selection in lockstep with whichever pane the user clicks. Removing or gating it might break click-to-select on the dashboard | Gating to "only when active tab is Dashboard" should preserve the original intent (dashboard click → dashboard selection updates) while stopping the cross-tab leak. Verify click-to-focus on a dashboard card still selects that card after the change. |
| Stable session ids on the dashboard depend on the session staying alive; sessions that end mid-tab-switch will fall back to the default. This may surprise users who expect "where I was" to be remembered even across a `/clear` | Acceptable. `pane_metadata` already maps pane id → SavedPane across `/clear`-induced session restarts (`src/ui.rs:476`), so for the common restart case the *pane id* survives even when the session id changes. Mode/Orchestration tabs key on pane id and benefit directly. The Dashboard's session-id default is a known limitation; document if it surfaces. |

## References

- `src/ui.rs:463` — `UiState.selected_index` (the global selection that needs to become per-tab-derived)
- `src/ui.rs:2248-2258` — render-time sync that overwrites `selected_index` from the global focused pane (to be gated to Dashboard-only)
- `src/ui.rs:2163-2178` — reactive-pane-pool clamp that today operates on a positional index (must work with stable ids in M4)
- `src/ui.rs:2942-3003` — tab-switch keyboard handlers (Ctrl+PageUp/Down, Tab/Shift+Tab, Left/Right/h/l) where focus restore must hook in
- `src/ui.rs:3010-3104` — Mode-tab j/k/Esc/Enter handlers that write `focused_side_pane_index` today (to be migrated to `focused_pane_id`)
- `src/tab.rs:55-85` — `Tab` enum (the variants gaining new fields)
- `src/tab.rs:134-141` — `TabManager::switch_to` (entry point for focus-restore hook)
- `src/tab.rs:373-411` — `route_reactive_commands` returning `(closed_id, new_id)` pairs (M4 remap source)
- `src/embedded_pane.rs:209-215` — `focused_pane_id()` (per-process focus state, to be queried at switch-out)
- `feedback_validate_pre_pr.md` — single pre-PR validation pass policy
