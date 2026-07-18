# PRD #140: Per-tab orchestration instance id (session-partition delegate/work-done routing)

**Status**: Planning
**Priority**: High
**Created**: 2026-07-18
**GitHub Issue**: [#140](https://github.com/vfarcic/dot-agent-deck/issues/140)
**Related**: PRD #93 (always-external daemon — introduced the shared per-user daemon and the `pane_role_map` / `pane_orchestration_map` daemon-side routing; round-11 auditor #C added the `(name, cwd)` scoping this PRD replaces); PRD #107 (orchestration tab name/title survives detach/reattach — the hydration round-trip this PRD must extend); PRD #120 (issue-dispatch orchestration surfacing — a daemon-initiated construct site).

## Problem Statement

The daemon is global per user — its sockets are keyed only by UID (`src/config.rs:51`, `src/config.rs:75`), so one daemon is shared across every directory and every TUI tab. Delegate and work-done routing is resolved entirely from daemon-side state maps, using an orchestration **identity** to decide which panes are "in the same orchestration".

That identity is the tuple `(orchestration_name, orchestration_cwd)`, stored as the value of `pane_orchestration_map: HashMap<String, (String, String)>` (`src/state.rs:205`). Round-11 auditor #C added the `cwd` half specifically so two *different* orchestrations whose names collide (e.g. `~/a/foo` and `~/b/foo` both resolving `name` to `foo`) get distinct identities.

**The tuple is still not unique when the same orchestration is launched twice from the same directory.** `TabMembership::Orchestration` (`src/agent_pty.rs:231`) carries `name`, `role_index`, `role_name`, `is_start_role`, `orchestration_cwd`, and `display_title` — but **no per-tab / per-instance id**. Two `Ctrl+N` tabs opening the same orchestration in the same cwd produce byte-identical `(name, cwd)` identities, so the daemon cannot tell their panes apart. This is exactly the reproduction reported on the issue (2026-07-18):

1. `dot-agent-deck`
2. `Ctrl+N` → select orchestration → start task A
3. `Ctrl+N` → select the **same** orchestration → start task B

Concretely, with Tab A = {`A_orch` orchestrator, `A_coder` role "coder"} and Tab B = {`B_orch`, `B_coder` role "coder"}, both tabs share identity `("foo","/x")`:

- **Delegate cross-fan-out** — `handle_delegate`'s target filter (`src/state.rs:816`) selects *every* pane whose role matches AND whose `pane_orchestration_map` entry equals the orchestrator's. `A_orch` delegating to "coder" matches **both** `A_coder` and `B_coder`, so B's worker receives A's task.
- **Work-done cross-delivery** — `handle_work_done` picks the orchestrator via `orchestrator_pane_ids.iter().find(|p| pane_orchestration_map.get(p) == worker_identity)` (`src/state.rs:930`). Iterating a `HashSet`, `.find()` returns `A_orch` *or* `B_orch` **non-deterministically** (hash order). A worker's report can land in the wrong orchestrator's pane.

The issue's original title ("even in separate directories") predates the round-11 `(name, cwd)` work and is **already fixed** on `main` — different directories now yield different identities, verified from the routing filters (`src/state.rs:823`, `src/state.rs:934`) and from both spawn construct sites populating `orchestration_cwd` (`src/tab.rs:540`, `src/spawn.rs:330`). What remains unfixed is the same-orchestration / same-directory / concurrent-tabs case, plus two residual edges surfaced during analysis (see Scope).

Recovery today requires killing the app, which leaves in-flight worker tasks uncommitted — so concurrent orchestrations are effectively unsafe and the documented workaround is "one orchestration tab at a time," which defeats the point of a multi-tab deck.

## Solution Overview

Give every orchestration **tab** a unique **instance id** minted once at tab-creation time, carried through the daemon round-trip on `TabMembership::Orchestration`, and used as the routing identity in place of `(name, cwd)`. Each tab then forms an isolated routing group even when its `name` and `cwd` are byte-identical to another tab's.

Three properties make this correct and safe:

1. **Uniqueness at the source.** The id is generated once per orchestration tab (not per pane) and shared by every role pane in that tab, so all of a tab's panes — orchestrator and workers — resolve to the same group, and no other tab's panes do.
2. **Round-trips through hydration.** The id must survive detach/reattach: the daemon stores `tab_membership` on each `AgentRecord` and echoes it back via `list_agents`, and the TUI rebuilds the tab from that echo (`open_orchestration_tab_with_existing_role_panes`, buckets in `src/ui.rs:2043`). The instance id rides the same path `orchestration_cwd` / `display_title` already do, so a reconnect reconstructs the *same* group rather than minting a fresh one that would orphan the running panes.
3. **Backwards-compatible fallback.** The field is additive (`Option<String>`, `#[serde(default, skip_serializing_if = "Option::is_none")]`). When it is present the daemon keys `pane_orchestration_map` on the instance id; when it is absent (older client, or a daemon-initiated path that has not been updated) it falls back to today's `(name, cwd)` identity. `Some` vs `None` is detectable, so the fallback is documented behavior, not a silent misroute.

This is a **semantic contract change behind a stable wire** in the sense of CLAUDE.md rule 12: the *shape* of `pane_orchestration_map`'s routing key changes even though the JSON frame stays additively compatible. It therefore requires a `changelog.d/140.breaking.md` fragment, the cross-version manual test, and a decision on whether `PROTOCOL_VERSION` (currently `4`, `src/daemon_protocol.rs:154`) should bump (recommended: **no** bump — the wire is additively forward/backward compatible via the fallback; the change is versioned as a compatibility break through the `.breaking.md` fragment and the `0.x` minor bump).

## Scope

### In Scope

- **Add an `orchestration_id: Option<String>` field to `TabMembership::Orchestration`** (`src/agent_pty.rs:231`), with `#[serde(default, skip_serializing_if = "Option::is_none")]` so older peers round-trip cleanly. Semantics: a per-tab instance token, shared by every role pane in one orchestration tab, opaque to the daemon. Distinct from `name` (identity-by-config), `orchestration_cwd` (directory disambiguator), and `display_title` (presentation-only).
- **Validate the new field at the wire boundary** in `validate_tab_membership` (`src/agent_pty.rs:312`) with the same discipline as `orchestration_cwd` / `role_name` — reject control bytes / oversized values so a hostile or buggy peer cannot smuggle escapes that later reach a tab label or log line.
- **Mint the id at both interactive and daemon-initiated construct sites**:
  - Interactive new-pane flow (`src/tab.rs:531`): generate one id for the whole tab before the `for role in config.roles` loop and stamp it on every role's `TabMembership::Orchestration`.
  - Scheduled / issue-dispatch flow (`src/spawn.rs:326`): generate one id per orchestration spawn request and stamp it on every role's membership.
  - Id generation must avoid `Math.random`/wall-clock pitfalls only relevant to the workflow engine, not here — use a process-unique monotonic counter combined with the pid, or a UUID, whichever the codebase already has a helper for. **Determinism note**: the id need not be reproducible across restarts (it is re-hydrated from the daemon echo, never regenerated for a live tab), but two tabs created in the same process must never collide.
- **Key `pane_orchestration_map` on the instance id when present** in the daemon spawn handler (`src/daemon_protocol.rs:997`). Options considered:
  - (chosen) Keep `pane_orchestration_map` value as a struct/enum identity that is *either* `Instance(id)` *or* `NameCwd(name, cwd)`, so old and new clients coexist and equality still means "same routing group." The value type changes from the raw `(String, String)` tuple — update `src/state.rs:205` and every read site accordingly.
  - (rejected) Add a parallel `pane_instance_map` and consult it first: two maps that must stay in lockstep is more failure surface than one identity value.
- **Route delegate and work-done on the new identity**:
  - `handle_delegate` target filter (`src/state.rs:816`) compares the new identity value.
  - `handle_work_done` orchestrator lookup (`src/state.rs:930`) compares the new identity value.
  - Both keep working for older clients via the `NameCwd` fallback variant.
- **Extend the hydration round-trip** so a detach/reattach reconstructs the same instance id: the daemon already stores `tab_membership` on `AgentRecord` and echoes it via `list_agents`; confirm the id survives `validate_tab_membership` and reaches the TUI bucketing in `src/ui.rs:2043` (`synthesize_from_bucket_metadata`). Bucket key must include the instance id so two same-`(name,cwd)` tabs rebuild as two tabs, not one merged tab, on reconnect.
- **Residual edge 1 — cross-directory regression test.** The already-fixed separate-directories behavior has *no* test guarding it (the only delegate test, `tests/delegate_prompt_injection.rs`, exercises a single happy-path orchestration). Add a routing-level test asserting a delegate/work-done in one `(name, cwdA)` orchestration never reaches a `(name, cwdB)` orchestration, so the fix cannot silently regress.
- **Residual edge 2 — both-cwd-None collision.** When both `orchestration_cwd` and `StartAgent.cwd` are absent (older pre-round-11 clients only), the identity collapses to `("name","")` and can still collide across directories. With the instance id as the primary key this narrows to "older client AND no cwd," but document the remaining collision explicitly and, where cheap, prefer the instance id / a non-empty fallback so current clients are never exposed.
- **Tests**:
  - Unit (serde): `TabMembership::Orchestration` round-trips the new field; older-shape JSON (no `orchestration_id`) deserializes to `None`; `validate_tab_membership` rejects control-byte ids and accepts valid ones.
  - Routing unit (in `src/state.rs` tests): construct an `AppState` with two same-`(name,cwd)` orchestrations bearing distinct instance ids; assert `handle_delegate` fans out only to the sibling worker of the delegating orchestrator, and `handle_work_done` writes back only to that orchestrator. Include the cross-directory case from edge 1 in the same suite.
  - **L2 two-tab isolation e2e** (CLAUDE.md rule 4 — PTY-attached, `.cast`-recording, demo-reel-eligible): drive the real binary, open the **same** orchestration in two tabs from the same cwd, delegate in each, and assert each worker's `work-done` lands **only** in its own orchestrator pane. Model on the `scheduler/dispatch/013` reference and `tests/e2e_delegate_work_done_chain.rs`.
  - Cross-version (rule 12): an older-daemon + newer-TUI exercise confirming the `NameCwd` fallback still routes a single orchestration end to end.
- **Changelog fragment** via `dot-ai-changelog-fragment`, plus `changelog.d/140.breaking.md` (semantic contract change per rule 12). Bug-fix framing: "fix: concurrent orchestrations (same config, same directory, multiple tabs) no longer cross-deliver delegate/work-done signals."
- **Docs**: replace the current "one orchestration at a time per user/machine" workaround wording (wherever the orchestration limitation is documented) with "concurrent orchestrations are safe, including multiple tabs of the same orchestration." Note the cross-version fallback in `docs/develop/versioning.md` alongside the `.breaking.md` discipline if that page enumerates breaks.

### Out of Scope

- **Reworking the socket / daemon-per-directory model.** The issue floats "partition by cwd / a session id"; per-tab instance id is the minimal correct partition and leaves the single-daemon architecture (PRD #93) intact. A daemon-per-directory redesign is a much larger change and unnecessary once routing is instance-scoped.
- **Changing how workers discover their orchestrator.** Workers still signal by their own `DOT_AGENT_DECK_PANE_ID`; the daemon does the grouping. No new worker-facing surface.
- **`PROTOCOL_VERSION` bump** (recommended out, pending the Open Question below): the wire stays additively compatible via the fallback; the break is versioned through the `.breaking.md` fragment and the `0.x` minor bump.

## Success Criteria

- Two tabs of the **same** orchestration in the **same** directory run concurrently with zero cross-delivery: each orchestrator's delegate reaches only its own workers, and each worker's `work-done` reaches only its own orchestrator — deterministically, across repeated runs.
- Separate-directory and different-name isolation continue to hold and are now covered by a regression test.
- A newer TUI attached to an **older** daemon (no `orchestration_id`) still routes a single orchestration correctly via the `(name, cwd)` fallback (cross-version manual test passes).
- Detach/reattach of a two-tab same-orchestration setup rebuilds **two** distinct tabs, each retaining its own routing group.
- `cargo test-fast` green per task; `cargo test-e2e` (including the new L2 two-tab isolation test) green pre-PR.

## Milestones

### Phase 1: Wire type and construct sites

- [ ] **M1.0** — Add `orchestration_id: Option<String>` to `TabMembership::Orchestration` (`src/agent_pty.rs:231`) with `#[serde(default, skip_serializing_if = "Option::is_none")]`. Serde round-trip test: new field preserved; older-shape JSON → `None`.
- [ ] **M1.1** — Extend `validate_tab_membership` (`src/agent_pty.rs:312`) to sanitize `orchestration_id` (reject control bytes / oversize, mirroring `orchestration_cwd`). Test rejects a control-byte id, accepts a valid one.
- [ ] **M1.2** — Mint one id per tab at the interactive construct site (`src/tab.rs:531`), stamped on every role pane's membership. Add a helper (process-unique, collision-free within a process).
- [ ] **M1.3** — Mint one id per orchestration spawn at the daemon-initiated / scheduled construct site (`src/spawn.rs:326`), stamped on every role pane's membership.

### Phase 2: Daemon routing identity

- [ ] **M2.0** — Change `pane_orchestration_map`'s value (`src/state.rs:205`) to an identity type that is `Instance(id)` when the tab carries an `orchestration_id`, else `NameCwd(name, cwd)`. Update the daemon spawn handler (`src/daemon_protocol.rs:997`) to populate it.
- [ ] **M2.1** — Route `handle_delegate`'s target filter (`src/state.rs:816`) on the new identity; keep the orchestrator-self-exclusion and role-match logic unchanged.
- [ ] **M2.2** — Route `handle_work_done`'s orchestrator lookup (`src/state.rs:930`) on the new identity. Confirm the `.find()` over `orchestrator_pane_ids` is now deterministic because at most one orchestrator shares any instance id.
- [ ] **M2.3** — Update the map cleanup paths (`unregister_pane`, `src/state.rs:759`) and any other read sites the type change touches; `cargo build` + `cargo clippy -- -D warnings` clean.

### Phase 3: Hydration round-trip

- [ ] **M3.0** — Ensure `orchestration_id` survives the daemon echo (`AgentRecord.tab_membership` → `list_agents` → `validate_tab_membership`) into the TUI bucketing at `src/ui.rs:2043` (`synthesize_from_bucket_metadata`). Bucket key includes the instance id so two same-`(name,cwd)` tabs rebuild as two tabs.
- [ ] **M3.1** — Detach/reattach test: two same-orchestration same-cwd tabs, reconnect, assert two distinct tabs each retain their routing group.

### Phase 4: Residual edges

- [ ] **M4.0** — Cross-directory regression test (edge 1): a delegate/work-done in `(name, cwdA)` never reaches `(name, cwdB)`.
- [ ] **M4.1** — Both-cwd-None edge (edge 2): document the remaining older-client collision and ensure current clients always carry a distinguishing id; test the fallback path.

### Phase 5: Tests, cross-version, docs, release

- [ ] **M5.0** — Routing unit tests in `src/state.rs`: two same-`(name,cwd)` orchestrations with distinct ids route delegate + work-done in isolation.
- [ ] **M5.1** — L2 PTY-attached two-tab isolation e2e (rule 4): real binary, same orchestration in two tabs, no cross-delivery; `.cast`-recording, modeled on `scheduler/dispatch/013` and `tests/e2e_delegate_work_done_chain.rs`.
- [ ] **M5.2** — Cross-version manual test (rule 12): older daemon + newer TUI still routes a single orchestration via the `NameCwd` fallback.
- [ ] **M5.3** — `changelog.d/140.breaking.md` + changelog fragment via `dot-ai-changelog-fragment`; docs updated to drop the "one orchestration at a time" workaround.
- [ ] **M5.4** — PR, Greptile review, cross-version contract check, merge, close #140.

## Key Files

- `src/agent_pty.rs` — `TabMembership::Orchestration` (`:231`), `validate_tab_membership` (`:312`).
- `src/state.rs` — `pane_orchestration_map` (`:205`), `handle_delegate` (`:788`), `handle_work_done` (`:894`), `unregister_pane` (`:759`).
- `src/daemon_protocol.rs` — `StartAgent` role-map population (`:997`), `PROTOCOL_VERSION` (`:154`).
- `src/daemon.rs` — hook-loop dispatch of `Delegate` / `WorkDone` (`:929`, `:941`).
- `src/tab.rs` — interactive construct site (`:531`), `open_orchestration_tab_with_existing_role_panes` (`:647`).
- `src/spawn.rs` — scheduled / daemon-initiated construct site (`:326`).
- `src/ui.rs` — hydration bucketing (`:2043`, `synthesize_from_bucket_metadata` at `:1927`).
- `tests/delegate_prompt_injection.rs`, `tests/e2e_delegate_work_done_chain.rs` — existing delegate/work-done coverage to extend.

## Risks and Mitigations

- **Hydration mismatch orphans panes.** If the reattach bucket key does not include the instance id, two same-`(name,cwd)` tabs merge into one on reconnect and orphan half the panes. Mitigation: M3.0/M3.1 assert two-tab reconstruction explicitly.
- **Map value-type churn.** Changing `pane_orchestration_map`'s value touches several read sites; a missed site silently reverts to a broken comparison. Mitigation: enum identity with exhaustive `match` so the compiler flags every site (M2.3), plus clippy `-D warnings`.
- **Older-client fallback regressions.** The `NameCwd` fallback must remain byte-equivalent to today's behavior. Mitigation: cross-version manual test (M5.2) and a fallback-path unit test (M4.1).
- **Demo-reel gap (rule 4).** Without the PTY-attached L2 test the feature ships with no reel clip and weaker validation. Mitigation: M5.1 is a hard gate, not optional.

## Open Questions

- **`PROTOCOL_VERSION` bump?** Recommendation: no — the frame stays additively compatible and the break is versioned via `.breaking.md` + the `0.x` minor bump. Confirm during the cross-version contract check that an older daemon + newer TUI (and vice-versa) both degrade to correct single-orchestration routing; if either mis-parses, bump.
- **Id generation primitive.** Reuse an existing pane-id / uuid helper vs. a new pid+counter helper — pick whatever the codebase already exposes to avoid a new dependency.
- **Experimental flag (CLAUDE.md rule 9)?** This is a bug fix to existing behavior, not a new user-visible surface, so it likely ships un-flagged. Confirm with the maintainer when starting the PRD.
