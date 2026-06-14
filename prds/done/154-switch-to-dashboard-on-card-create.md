# PRD #154: Switch to Dashboard when creating a single-agent card from a non-Dashboard tab

**Status**: Complete
**Priority**: Medium
**Created**: 2026-06-13
**Completed**: 2026-06-14
**GitHub Issue**: [#154](https://github.com/vfarcic/dot-agent-deck/issues/154)
**Related**: `src/ui.rs` (`Action::SpawnPane` handler, "regular dashboard pane" branch ~L4920), `src/tab.rs` (`TabManager::switch_to`, `open_orchestration_tab`, `open_mode_tab`)

## Problem Statement

The new-deck dialog (`Ctrl+N`) is reachable from any tab. It produces one of three things:

| What you create | Where it lives | Active tab after create |
|-----------------|----------------|-------------------------|
| Orchestration   | its own new tab | the new orchestration tab (`open_orchestration_tab` sets `active_index = index`, `src/tab.rs:549`) |
| Mode            | its own new tab | the new mode tab (`open_mode_tab` sets `active_index = index`, `src/tab.rs:435`) |
| **Single-agent card** | the **Dashboard** tab (always index 0) | **whatever tab you launched from** — unchanged |

The single-agent card path is the odd one out. Its branch in `Action::SpawnPane` (`src/ui.rs:4920-4950`) focuses the new pane and selects it on the Dashboard:

```rust
} else {
    // No mode — regular dashboard pane.
    let _ = pane.focus_pane(&new_id);
    ui.mode = UiMode::PaneInput;
    ui.selected_index = filtered.len();   // intends to select the new card on the Dashboard
    resize_dashboard_panes(pane, ui, tab_manager, frame_area);
    ...
}
```

…but it never switches `tab_manager.active_index` back to the Dashboard. A card doesn't get its own tab — it belongs to the Dashboard (index 0) — so when this path runs while an **orchestration or mode tab** is active, the active tab stays put and the new card is created on a tab the user isn't viewing. The `focus_pane` + `selected_index = filtered.len()` calls clearly intend to surface the new card, but they land on the Dashboard while the orchestration/mode tab is still rendered.

This is invisible in the common case because `Ctrl+N` is usually pressed *from the Dashboard*, where `active_index` is already 0. The gap only shows when the dialog is launched from a non-Dashboard tab.

User-visible symptom (reported): creating a new dashboard card from inside an orchestration tab leaves the view in the orchestration; the new card appears "lost" until the user manually switches back to the Dashboard.

## Solution

In the "regular dashboard pane" branch of `Action::SpawnPane`, switch to the Dashboard tab (always index 0) **before** applying focus and selection, so the existing `focus_pane` / `selected_index` calls act on the now-visible Dashboard. The switch is preceded by `capture_focus_on_switch_out()` — mirroring every other production `switch_to` — so the leaving tab's live focus is snapshotted and restores correctly on return:

```rust
} else {
    // No mode — regular dashboard card. The card lives on the Dashboard
    // (tab 0), so make the Dashboard active before focusing/selecting it —
    // otherwise, when launched from an orchestration/mode tab, the new card
    // lands on a tab the user isn't viewing. (Orchestration/mode creation
    // already switch to their own new tab via open_*_tab.)
    //
    // Capture the leaving tab's live focus before the switch, mirroring
    // the established switch-out invariant, so its prior focus restores
    // on return.
    tab_manager.capture_focus_on_switch_out();
    tab_manager.switch_to(0);
    let _ = pane.focus_pane(&new_id);
    ui.mode = UiMode::PaneInput;
    ui.selected_index = Some(filtered.len());
    // PRD #84 M4: the pane was spawned at the dashboard layout dims; the
    // pre-draw `resize_panes_to_layout` reconciles it to the exact rect
    // next frame. No resize here.
    ...
}
```

### Design decisions (from discussion)

- **Cover all non-Dashboard tabs, not just orchestration.** The user reported it from an orchestration tab, but the same inconsistency applies when launching from a mode tab. The fix is "a single-agent card always lands on the Dashboard," which covers both uniformly and needs no per-source branching.
- **Dashboard index 0 is a stable invariant.** `TabManager::new` constructs `vec![Tab::Dashboard { .. }]` first (`src/tab.rs:144`), new tabs are always pushed to the end, and closing the Dashboard is a no-op (`CloseTabOutcome::default()`). So `switch_to(0)` is the Dashboard. The fix relies on that invariant rather than searching for the Dashboard tab.
- **No change to orchestration/mode creation.** Those already switch to their newly created tab; only the plain-card path is touched.
- **Keep the existing focus/selection behavior.** The fix re-orders intent (switch tab, then focus/select) rather than adding new selection logic — `selected_index = filtered.len()` already targeted the new card.

## Acceptance Criteria

### Lands on the Dashboard
- [x] Creating a single-agent card (no mode, no orchestration) while an **orchestration tab** is active leaves the **Dashboard** tab active afterward, with the new card selected and focused. — L1 test `tabs/spawn/001` (GREEN).
- [x] Same when launched from a **mode tab**: the active tab afterward is the Dashboard, new card selected. — L1 test `tabs/spawn/002` (GREEN).
- [x] Creating a card while already on the Dashboard is unchanged (still selects/focuses the new card; no visible regression). — L1 baseline guard `tabs/spawn/003` (GREEN).

### Other creation paths unchanged
- [x] Creating an **orchestration** from any tab still switches to the new orchestration tab (unchanged). — fix is in the no-mode branch only; orch path untouched; e2e `spawn_002_orchestration_vs_single_agent` GREEN.
- [x] Creating a **mode** from any tab still switches to the new mode tab (unchanged). — mode-creation path untouched; existing `tabs/mode/001` + full e2e GREEN.

### No regressions
- [x] `selected_index`, focus, and selection still apply to the new card after the tab switch; the leaving tab's focus is captured (via `capture_focus_on_switch_out`) and restores on return. — asserted by `tabs/spawn/001-003` (selection/focus) and `tabs/spawn/004` (leaving-tab focus round-trip). Layout reconciles next frame (PRD #84 M4), so no explicit resize call remains.
- [x] `cargo fmt --check` and `cargo clippy -- -D warnings` clean; fast test tier green. — fast tier 747/747; e2e suite 1108/1108.

## Milestones

- [x] **M1 — Fix.** Add `tab_manager.switch_to(0)` (with explanatory comment) to the "regular dashboard pane" branch of `Action::SpawnPane`, before the focus/selection calls. Also prepends `capture_focus_on_switch_out()` (review finding) to preserve the leaving tab's focus-restore.
- [x] **M2 — Tests.** L1 tests `tabs/spawn/001-004`: orchestration→Dashboard, mode→Dashboard, Dashboard-source baseline, and leaving Mode-tab focus round-trip; orchestration/mode creation remain covered by existing `tabs/orchestration/001` + `tabs/mode/001` and the e2e suite.
- [x] **M3 — Verified in the running TUI.** Launch the deck, open an orchestration tab, create a single-agent card, and confirm the view switches to the Dashboard with the new card selected (per `run-dot-agent-deck`). — Verified manually: view switches to Dashboard with new card selected.

## Out of Scope

- Any change to how orchestration or mode tabs are created or activated.
- Adding a "stay on current tab" preference/toggle — the agreed behavior is that a single-agent card always lands on the Dashboard.
- Directory-picker / new-pane-form flow changes (the form already produces the correct `SpawnPane` request; only the post-spawn active-tab handling changes).

## Notes

- Root cause: the single-agent-card branch sets focus/selection for the Dashboard but never makes the Dashboard active, unlike the orchestration/mode branches which create *and switch to* their own tab.
- Surfaced by a user report: "When I create a new dashboard card while inside an orchestration tab, the view stays in orchestration — shouldn't it switch to the Dashboard with the new card selected?"
