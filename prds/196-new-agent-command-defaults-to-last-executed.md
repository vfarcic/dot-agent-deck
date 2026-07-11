# PRD #196: New-agent command defaults to the last executed command

**Status**: Not Started
**Priority**: Medium
**Created**: 2026-06-25
**GitHub Issue**: [#196](https://github.com/vfarcic/dot-agent-deck/issues/196)
**Related**: PRD #170 (configurable agent command + `default_command` plumbing — this reuses the same config field and form seam), PRD #20 (multi-agent support — quality-of-life for switching between agent commands)

## Problem Statement

When a user spins up a new agent, the new-pane form's Command field is pre-seeded from `default_command` (`src/config.rs:179`, applied at the `DirPickerIntent::NewPane` seam in `transition_after_dir_pick`, `src/ui.rs`). When `default_command` is left empty — its default value (`String::new()`, `src/config.rs:188`) — the field starts **blank**, so the user must type the command (e.g. `claude`, `opencode`, `devbox run agent-new`) by hand on every single spawn. In practice almost everyone re-runs the same command they used last time, so the blank field is repetitive friction for the common case while delivering nothing — the form already records each spawned pane's command to disk (`SavedPane.command`, `src/config.rs:321`), so "the command I ran last" is already sitting in persisted state with nothing reading it back as a starting point.

## Solution Overview

When `default_command` is empty, seed the new-agent Command field from the **last executed command** instead of leaving it blank. The behavior is a strict fallback chain, with explicit configuration always winning:

1. **`default_command` (when set)** — unchanged precedence; an explicit config value always wins.
2. **Last executed command** — the most recent command a user spawned an agent with, if any.
3. **Blank** — fresh install / never spawned an interactive agent: current behavior, unchanged.

The value is **global** (one last-command value, not per-directory), **persisted** so it survives a deck restart, and it only ever **pre-fills the editable Command field** — it is never silently auto-run. The user still sees the field, can edit it, and can clear it; a wrong guess costs one clear. **Every** command submitted through the new-agent form is recorded as the last command, regardless of the selected mode (schedule / issue-dispatch authoring included) — there is no authoring-vs-interactive special-casing, since every pane launched from the form is the same kind of action. Orchestration role panes take their command from config (the form hides the Command field), so they never record, and empty commands are ignored.

## Scope

### In Scope

- A single persisted, global `last_command` value (stored alongside the existing session/config persistence the deck already writes).
- Recording `last_command` on each successful agent spawn submitted through the new-pane form, regardless of the selected mode (orchestration takes its command from config, not the form, and empty commands are ignored, so neither records).
- Extending the form-seed logic at the `DirPickerIntent::NewPane` seam in `transition_after_dir_pick` (`src/ui.rs`) to resolve the fallback chain: `default_command` if non-empty, else `last_command` if present, else blank.
- L1 widget/behavior coverage that the form seeds correctly for each branch of the fallback chain (config set; config empty + last present; config empty + no last), and that authoring-mode spawns also record `last_command` (no mode special-casing).
- User docs: a short note that the new-agent command defaults to your last command when no `default_command` is configured.

### Out of Scope / Non-Goals

- **Per-directory last command** ("last command used *in this cwd*"). Plausibly nicer, but more state and more design; explicitly deferred to a possible follow-up.
- **A full command history / cycling UI** (up-arrow through previous commands, a recent-commands picker). This v1 remembers exactly one value. Multi-entry history is a separate, larger effort.
- **Changing `default_command` semantics or precedence.** An explicit `default_command` still wins unconditionally; this only changes the *empty* case.
- **Auto-running the command.** The field is only pre-filled; spawn still requires the user to submit the form.
- **The `experimental` feature flag** (PRD #139). This is a refinement of an existing default-seed behavior, not a new user-visible surface (no new pane/field/command/keybinding), so it ships visible by default — to be confirmed with the user at `/prd-start` per CLAUDE.md rule 9.

## Design Decisions

1. **Fallback chain, not replacement.** Explicit configuration is intent; we never override a set `default_command`. The last-command fallback fills only the empty case, so users who configured a command see no change.
2. **Pre-fill, never auto-run.** The Command field stays visible and editable; seeding it is non-destructive (one keystroke to clear). This matches the existing form behavior where `default_command` pre-fills the same field.
3. **Dedicated `last_command` value, not derived from the pane list.** Although `SavedPane.command` already persists per pane, panes get removed and their ordering is not a reliable "most recent" signal. A single explicitly-tracked value written on each interactive spawn is unambiguous and cheap.
4. **Global for v1.** One value is the simplest useful version and matches the stated intent ("the command I just typed"). Per-directory is deferred.
5. **Record every form-launched command; no mode special-casing.** *(Revised during implementation — the original plan excluded authoring-mode spawns.)* Every pane launched from the new-agent form — regular or schedule / issue-dispatch authoring — is the same kind of action: a user submitting a command. Recording only some was inconsistent, so all form-launched commands are recorded. Orchestration takes its command from config (the form hides the Command field) and empty commands are ignored, so neither records. Because the value only ever pre-fills an editable field, a stray or one-off command costs a single clear.
6. **Persist across restarts.** The deck already writes session/config state to disk; persisting one extra string is trivial and the feature is most valuable on the first spawn of a session — exactly when an in-memory-only value would be empty.

## Success Criteria

- With `default_command` empty and a prior interactive spawn of `claude`, opening the new-agent form pre-fills the Command field with `claude`.
- With `default_command` set, the form pre-fills from `default_command` regardless of any last command (precedence preserved).
- With `default_command` empty and no prior interactive spawn (fresh state), the Command field is blank (no regression).
- The last command survives a full deck restart (persisted).
- Spawning via the schedule / issue-dispatch authoring option **records** its command as `last_command`, exactly like any other form spawn (no mode special-casing).
- The field is only pre-filled; no command runs without the user submitting the form.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test-fast` pass; `cargo test-e2e` passes before the PR (CLAUDE.md rule 5).
- User docs note the new fallback behavior.

## Milestones

### Phase 1 — Persisted last-command tracking

- [ ] **M1.1** — Add a persisted, global `last_command` value to the existing config/session persistence; load it on startup, default to empty/absent.
- [ ] **M1.2** — Record `last_command` on each successful new-pane spawn submitted through the form, regardless of mode (orchestration excluded by construction — no form command; empty commands ignored).

### Phase 2 — Fallback-chain form seeding

- [ ] **M2.1** — Extend the new-pane form seed (`DirPickerIntent::NewPane` seam in `transition_after_dir_pick`, `src/ui.rs`) to resolve `default_command` (if non-empty) → `last_command` (if present) → blank.
- [ ] **M2.2** — Tests: L1/behavior coverage for all three seed branches and for authoring-mode spawns also recording `last_command`.

### Phase 3 — Docs & release gate

- [ ] **M3.1** — User docs note the fallback behavior; changelog fragment via `dot-ai-changelog-fragment`.
- [ ] **M3.2** — Pre-PR gate: `cargo test-e2e` green; review (Greptile) settled per CLAUDE.md rule 8.

## Risks & Mitigations

- **A stale or one-off command lingering as the pre-fill.** Every recorded value is a command the user submitted through the form (orchestration and empty commands never record); because it only ever pre-fills an editable field, a wrong or one-off value costs a single clear, and an explicit `default_command` overrides it entirely.
- **Surprise from a stale/one-off last command.** The field is pre-filled and editable, so the cost of a wrong guess is a single clear; explicit `default_command` still wins for users who want a fixed value.
- **Persistence corruption / migration.** Reuse the deck's existing atomic-write persistence path; treat a missing/unreadable value as empty (fall through to blank), never a hard failure.
