# PRD #107: Orchestration tab uses config name instead of user-entered name

**Status**: In Progress
**Priority**: Medium
**Created**: 2026-05-24
**GitHub Issue**: [#107](https://github.com/vfarcic/dot-agent-deck/issues/107)

## Problem Statement

When a user opens the new-pane form, selects an orchestration, and types a custom name in the **Name** field, the tab that appears at the top of the TUI always shows the name from the TOML config (or the cwd basename fallback) — not what the user typed.

### Root Cause

In `src/ui.rs` the `KeyResult::NewPane` handler builds a `NewPaneRequest` that carries the user's input in `req.name`:

```rust
let req = NewPaneRequest {
    name: form.name.clone(),   // ← user typed this
    orchestration_config: form.selected_orchestration().cloned(),
    ...
};
```

But when the orchestration branch is taken (around line 4427), `req.name` is never consulted. Only `orch_config` — cloned straight from the TOML — is forwarded to `open_orchestration_tab()`:

```rust
match tab_manager.open_orchestration_tab(
    &orch_config,   // ← name always comes from TOML
    &dir_str,
    prompt,
    spawn_dims,
) {
```

`src/tab.rs:259` then resolves the tab title purely from `config.name`:

```rust
let resolved_name = resolve_orchestration_name(&config.name, Path::new(cwd));
```

The user's input is discarded before it ever reaches the tab or the daemon-side `TabMembership`.

## Solution Overview

Before calling `open_orchestration_tab()`, override `orch_config.name` with `req.name` when the user provided one (non-empty). Because `OrchestrationConfig` is already cloned at the call site (via `form.selected_orchestration().cloned()`), mutating it for this invocation is safe and does not affect any other state.

```rust
if let Some(mut orch_config) = req.orchestration_config {
    if !req.name.is_empty() {
        orch_config.name = req.name.clone();
    }
    // ... rest of orchestration tab creation unchanged
```

No changes to `open_orchestration_tab()`'s signature, `tab.rs`, `daemon_protocol.rs`, or any daemon-side code are required — the name flows through the existing path once it is set on the config before the call.

## Scope

### In Scope

- **Override `orch_config.name` with `req.name` in `src/ui.rs`** when `req.name` is non-empty, immediately before `open_orchestration_tab()` is called. This is the complete fix.
- **Tests**: add a unit/integration test confirming that a name typed in the form becomes the rendered tab label (regression guard).
- **Edge cases**:
  - Empty `req.name` → keep existing behaviour (config name or cwd-basename fallback). No change.
  - Non-empty `req.name` with a config that already has a name → user's input wins.
  - Non-empty `req.name` with a config whose name is empty → user's input replaces the cwd-basename fallback.

### Out of Scope

- Persisting the overridden name back to the TOML config. The override is per-invocation only.
- Changing the form UI (field labels, field order, placeholder text). The Name field already exists and already captures user input; the fix is purely in how that input is consumed.
- Renaming a tab after it has been opened.
- The `open_orchestration_tab_with_existing_role_panes` path (used on reconnect / snapshot restore). That path reconstructs from daemon-side `TabMembership`, which is populated with whatever name was used at creation time — so once the creation-side fix is in, reconnect will naturally carry the correct name forward.

## Key Files

- `src/ui.rs` — `KeyResult::NewPane` handler (around line 4427): the one-line fix.
- `src/tab.rs` — `open_orchestration_tab` (line 229): no change needed; documents why the existing resolution logic is correct once `config.name` is set properly.
- Test file (new or existing integration test): verify the name flows end-to-end from form input to rendered tab label.

## Success Criteria

- A user who selects an orchestration in the new-pane form and types "my-feature" in the Name field sees a tab labelled "my-feature" (not the TOML config name or the cwd basename).
- Leaving the Name field empty continues to produce the existing behaviour (config name → cwd-basename fallback).
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass.
- `cargo test` passes, including the new regression test.

## Milestones

- [x] **M1** — Apply the one-line fix in `src/ui.rs`: override `orch_config.name` with `req.name` when non-empty.
- [x] **M2** — Add a regression test verifying that a name typed in the new-pane form appears as the orchestration tab label.
- [ ] **M3** — PR, review, merge, close issue.
