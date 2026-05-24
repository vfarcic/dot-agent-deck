# PRD #111: Fix orchestration tab hydration when TUI config path is remote

**Status**: Planning
**Priority**: High
**Created**: 2026-05-24
**GitHub Issue**: [#111](https://github.com/vfarcic/dot-agent-deck/issues/111)
**Related**: PRD #93 (always-external daemon), PRD #76 M2.12 (hydration partition)

## Problem Statement

When a TUI reconnects to a running daemon (e.g., a laptop TUI reconnecting to a VM daemon), the hydration code at `src/ui.rs:2591-2686` attempts to reconstruct orchestration tabs. The critical step is:

```rust
let cfg = lookup_config(&mut config_cache, &bucket.cwd);  // line 2592
let orch_config = cfg.as_ref().and_then(|c| {
    c.orchestrations.iter().find(|o| o.name == bucket.orchestration_name).cloned()
});
let Some(orch_config) = orch_config else {
    tracing::error!("dropping to dashboard");  // line 2604
    continue;
};
```

`bucket.cwd` is the path **on the daemon's host** (e.g., `/root/code/dot-agent-deck`). When the TUI is running on a different machine (laptop), that path does not exist locally. `lookup_config` returns `None`, the guard fires, and **every orchestration pane is dropped to the dashboard tab** instead of being placed in its own orchestration tab.

This means: any user connecting to a remote daemon always sees their active orchestration sessions dumped into the dashboard rather than appearing in properly structured orchestration tabs. The panes are structurally correct in the daemon (they retain their `TabMembership::Orchestration` metadata); the failure is purely in the TUI's inability to load a config file it doesn't have access to.

A secondary issue: even when the config *is* found (local connection), `tab_manager.switch_to(0)` at line 2693 always snaps the active tab back to the dashboard after rebuilding all tabs, so the user lands on the dashboard instead of their orchestration tab after every reconnect.

## Solution Overview

Decouple orchestration tab reconstruction from local config file access:

1. **Structural reconstruction from TabMembership metadata**: The daemon's `list_agents` response already provides everything needed to rebuild the tab layout: orchestration name, role name, role index, and `orchestration_cwd`. This is sufficient to reconstruct `Vec<Option<String>>` role slots and open the orchestration tab. Move to this path as the primary reconstruction strategy, making the local config file lookup **optional** (used only for display enrichment like `description`).

2. **Build a minimal `OrchestrationConfig` from daemon metadata**: When the local config file is absent (remote case), synthesise a minimal `OrchestrationConfig` from the `TabMembership` data returned by `list_agents`. This minimal config has the correct name and role list (derived from bucket role_slots), with `command`, `prompt_template`, and `description` left as defaults. The tab is fully functional; only display-only fields are missing.

3. **Preserve active tab on reconnect**: After hydration, instead of unconditionally calling `switch_to(0)`, restore the previously active tab if it can be identified, or stay on whichever tab was built last. The current comment acknowledges this is a UX compromise; for orchestration tabs the user expects to land where they left off.

## Scope

### In Scope

- **Synthesise minimal `OrchestrationConfig` from bucket metadata** when `lookup_config` returns `None`. The synthesised config uses `bucket.orchestration_name` as name and derives roles from `bucket.role_slots` (each slot's `role_name` and `role_index` are already in the hydrated data).
- **Make `open_orchestration_tab_with_existing_role_panes` work without full config** — or create a sibling variant that accepts the minimal synthesised config.
- **Remove or gate the `switch_to(0)` reset** at `src/ui.rs:2693` so that after reconnect the user lands on the most recently active orchestration tab (or the first one) rather than always the dashboard.
- **Tests**:
  - Unit test for the synthesised-config path: given bucket metadata with no matching local config, tab is rebuilt correctly.
  - Integration test: simulate remote reconnect (daemon has orchestration agents, TUI config cache has no matching path) → orchestration tabs appear, panes not in dashboard.
  - Regression: existing local-config hydration path still works when config is available (enriched fields populated correctly).

### Out of Scope

- Fetching the remote config file over the wire (out of scope for now — adds protocol complexity; synthesis from metadata is sufficient for structural correctness).
- Persisting the "last active tab" across full process restarts (separate concern).
- Changes to the daemon-side `list_agents` response shape (all needed fields already present).

## Key Files

| File | Change |
|------|--------|
| `src/ui.rs` | Synthesise `OrchestrationConfig` from bucket when local config absent; gate `switch_to(0)` |
| `src/tab.rs` | May need a variant of `open_orchestration_tab_with_existing_role_panes` accepting minimal config |
| `src/project_config.rs` | Possibly add `OrchestrationConfig::from_bucket` constructor |
| `tests/` | New integration test for remote-path hydration |

## Milestones

- [ ] **M1 — Synthesise minimal config**: Implement logic to build a minimal `OrchestrationConfig` from `OrchestrationBucket` metadata when `lookup_config` returns `None`. Name and role list are structurally correct; display fields default.
- [ ] **M2 — Hydration uses synthesised config**: `src/ui.rs` hydration loop uses the synthesised config as a fallback instead of `continue`-ing to dashboard; panes land in their orchestration tab.
- [ ] **M3 — Active tab preserved on reconnect**: Remove or gate `switch_to(0)` so reconnect lands on the orchestration tab (or the previously active one), not the dashboard.
- [ ] **M4 — Tests pass**: Unit and integration tests for the remote-path and local-path hydration cases, plus the active-tab regression.
- [ ] **M5 — Local enrichment still works**: When the config file *is* found locally, `prompt_template`, `description`, and other extras are still applied (no regression for local connections).

## Success Criteria

1. A laptop TUI reconnecting to a VM daemon with active orchestration agents sees properly structured **orchestration tabs** — not all panes dumped on the dashboard.
2. After reconnect, the TUI's active tab is the orchestration tab (or the last active tab), not the dashboard.
3. Local connections (same-host TUI + daemon) continue to work exactly as before, with full config enrichment.
4. No `tracing::error!("dropping to dashboard")` fires during a normal remote reconnect.

## Notes

- The root structural data is already on the wire (`TabMembership::Orchestration` carries `name`, `role_name`, `role_index`, `orchestration_cwd`). The fix is entirely client-side.
- The `switch_to(0)` change is intentionally conservative: preserve it for the case where there are *no* orchestration tabs (pure dashboard session) and only skip it when at least one orchestration tab was successfully rebuilt.
