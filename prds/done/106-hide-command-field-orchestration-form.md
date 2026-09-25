# PRD #106: Hide command field in new-pane form when orchestration is selected

**Status**: Open  
**Priority**: Low  
**Created**: 2026-05-24  
**GitHub Issue**: [#106](https://github.com/vfarcic/dot-agent-deck/issues/106)

## Problem Statement

When a user opens the new-pane form (`Ctrl+n`) and cycles the Mode field to select an orchestration, the Command field remains visible and editable. It looks required and the user may spend time filling it in — but it is never used. Every role pane in an orchestration tab is spawned using the `command` defined in `[[orchestrations.roles]]` in `.dot-agent-deck.toml`. Whatever the user types in the form's Command field is silently ignored.

This creates two problems:
- **False affordance**: the form implies the command matters, which it doesn't.
- **User confusion**: users who read the docs or notice the mismatch lose trust in the UI.

## Solution Overview

When the Mode field is set to an orchestration (as opposed to "No mode" or a workspace mode), hide the Command field entirely. The form becomes simpler and self-consistent: the user picks a directory, picks the orchestration, and confirms — nothing more.

## Success Criteria

- Selecting an orchestration in the Mode field causes the Command field to disappear from the form.
- Selecting "No mode" or a workspace mode restores the Command field.
- Pressing `Enter` on the form with an orchestration selected opens the orchestration tab correctly without requiring a command.
- Existing keyboard navigation (`Tab`/`Shift+Tab` between fields) skips the Command field gracefully when it is hidden.
- No regression in the non-orchestration new-pane flow.

## Scope

### In Scope
- Hide Command field in the unified new-pane form when an orchestration is selected
- Restore Command field when mode selection returns to "No mode" or a workspace mode
- Update keyboard navigation to skip the hidden field

### Out of Scope
- Changes to how orchestration tabs are spawned or how role commands are resolved
- Any changes to `.dot-agent-deck.toml` config format
- Hiding or changing the Name field

## Milestones

### M1: Hide command field on orchestration selection
- [ ] Detect when the selected mode is an orchestration in the unified form state
- [ ] Conditionally render the Command field only when no orchestration is selected
- [ ] Verify `Tab`/`Shift+Tab` field cycling skips the hidden field cleanly

### M2: Validation and non-regression
- [ ] Confirm orchestration tab opens correctly without a user-supplied command
- [ ] Confirm normal pane creation (no mode, workspace mode) still requires and uses the command
- [ ] Add or update tests covering the unified form field visibility logic

## Key Files

- `src/ui.rs` — unified new-pane form rendering and field cycling logic

## Dependencies

None. Self-contained UI change.

## Risks

- **Field index off-by-one**: hiding a field shifts the tab-order indices; careful index handling needed to avoid `Tab` landing on wrong field or skipping confirmation.
