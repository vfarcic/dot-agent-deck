# PRD #16: Toggle Stacked/Tiled Pane Layout

**Status**: Draft
**Priority**: Medium
**Created**: 2026-03-31
**GitHub Issue**: [#16](https://github.com/vfarcic/dot-agent-deck/issues/16)

## Problem Statement

When multiple agent panes are open in the Zellij session, they use a **stacked layout** — only the active pane is expanded while all others collapse to their title bars. This is efficient for focusing on one agent at a time, but users have no way to see all agent panes simultaneously when they want a broad overview of what every agent is doing.

## Solution Overview

Add a toggle shortcut that cycles between the current **stacked** layout and a new **tiled** layout where all agent panes share the right column equally. Two access points:

1. **`t`** — from the dashboard TUI (Normal mode), calls `zellij action next-swap-layout` via the `PaneController` trait
2. **`Alt+t`** — Zellij-level keybind using the built-in `NextSwapLayout` action, works from any pane

Zellij natively supports multiple `swap_tiled_layout` blocks and cycles through them with `next-swap-layout`. The implementation adds a second layout definition and wires up the keybindings.

## Scope

### In Scope
- Add a second `swap_tiled_layout` (tiled, non-stacked) to the Zellij layout definition
- Add `Alt+t` Zellij keybind for `NextSwapLayout` (works from any pane)
- Add `toggle_layout()` method to `PaneController` trait
- Add `t` keybinding in dashboard Normal mode
- Update help overlay with both shortcuts

### Out of Scope
- Custom layout configurations (user-defined split ratios)
- More than two layout variants
- Floating pane layouts

## Technical Approach

### Layout Definition (`src/main.rs`)
- Rename current `swap_tiled_layout name="dashboard"` to `name="stacked"`
- Add second `swap_tiled_layout name="tiled"` with responsive column breakpoints:
  - 1-3 agents: single column (dashboard 33% + agents 67%)
  - 4-6 agents: 2 columns within the 67% right area
  - 7+ agents: 3 columns within the 67% right area
  - Dashboard stays fixed at 33% width; agent columns are nested inside the remaining 67%

### Zellij Config (`src/main.rs`)
- Add `bind "Alt t" { NextSwapLayout; }` to the `keybinds` block

### PaneController Trait (`src/pane.rs`)
- Add `fn toggle_layout(&self) -> Result<(), PaneError>` to the trait
- `ZellijController`: runs `zellij action next-swap-layout`
- `NoopController`: returns `Err(PaneError::NotAvailable)`

### Dashboard UI (`src/ui.rs`)
- Add `ToggleLayout` variant to `KeyResult` enum
- Add `KeyCode::Char('t') => KeyResult::ToggleLayout` in `handle_normal_key`
- Handle `ToggleLayout` in main event loop with status message feedback
- Update help overlay: add `t` under "Pane Control", `Alt+t` under "Zellij" section
- Bump help overlay `base_height` to accommodate new lines

## Success Criteria

- Pressing `t` from dashboard toggles all agent panes between stacked and tiled
- Pressing `Alt+t` from any pane (including agent panes) toggles the layout
- Help overlay documents both shortcuts
- All existing tests pass
- Layout correctly handles 1, 2, and 3+ pane scenarios

## Milestones

- [x] Second `swap_tiled_layout` added and Zellij `Alt+t` keybind configured (`src/main.rs`)
- [ ] `toggle_layout()` method added to `PaneController` trait with implementations (`src/pane.rs`)
- [ ] `t` keybinding wired up in dashboard TUI with `ToggleLayout` KeyResult (`src/ui.rs`)
- [ ] Help overlay updated with both `t` and `Alt+t` shortcuts (`src/ui.rs`)
- [ ] All tests passing including new noop controller assertion

## Key Files

- `src/main.rs` — Zellij layout definition and config
- `src/pane.rs` — PaneController trait and implementations
- `src/ui.rs` — Keybindings, event handling, help overlay
