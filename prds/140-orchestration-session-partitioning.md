# PRD #140: Concurrent orchestration safety — routing correctness + worktree-per-orchestration model

**Status**: Planning
**Priority**: High
**Created**: 2026-07-18
**GitHub Issue**: [#140](https://github.com/vfarcic/dot-agent-deck/issues/140)
**Related**: PRD #93 (always-external daemon — introduced the shared per-user daemon and the `pane_role_map` / `pane_orchestration_map` daemon-side routing; round-11 auditor #C added the `(name, cwd)` scoping this PRD extends); PRD #107 (orchestration tab name/title survives detach/reattach — the hydration round-trip this PRD must extend); PRD #120 (issue-dispatch orchestration surfacing — already uses one git worktree per dispatched orchestration, the model this PRD generalizes); PRD #220 (dispatcher mode + worktree dispatch — makes the worktree-per-orchestration model this PRD documents a one-step, agent-callable action; once it ships, this PRD's guard/warning should point at the dispatcher mode instead of the manual `/worktree-prd` flow, and the prong-1 decision below is revisited); the `/worktree-prd` skill (worktree-per-line-of-work).

## Problem Statement

The daemon is global per user — its sockets are keyed only by UID (`src/config.rs:51`, `src/config.rs:75`), so one daemon is shared across every directory and every TUI tab. Delegate and work-done routing is resolved entirely from daemon-side state maps, using an orchestration **identity** to decide which panes are "in the same orchestration". That identity is the tuple `(orchestration_name, orchestration_cwd)`, stored as the value of `pane_orchestration_map: HashMap<String, (String, String)>` (`src/state.rs:205`). Round-11 auditor #C added the `cwd` half so two *different* orchestrations whose names collide across directories (e.g. `~/a/foo` and `~/b/foo` both resolving `name` to `foo`) get distinct identities.

The tuple is still not unique when the same orchestration is launched twice from the same directory. `TabMembership::Orchestration` (`src/agent_pty.rs:231`) carries `name`, `role_index`, `role_name`, `is_start_role`, `orchestration_cwd`, and `display_title` — but no per-tab / per-instance id. Two `Ctrl+N` tabs opening the same orchestration in the same cwd produce byte-identical `(name, cwd)` identities, so the daemon cannot tell their panes apart. This is exactly the reproduction reported on the issue (2026-07-18): open the same orchestration in two tabs, start a task in each, and the delegate/work-done signals cross-deliver. Concretely, with Tab A = {`A_orch`, `A_coder`} and Tab B = {`B_orch`, `B_coder`} both at identity `("foo","/x")`: `handle_delegate`'s target filter (`src/state.rs:816`) matches *both* workers, and `handle_work_done`'s orchestrator lookup (`src/state.rs:930`) picks `A_orch` *or* `B_orch` non-deterministically via `HashSet` iteration order.

The issue's original title ("even in separate directories") predates the round-11 `(name, cwd)` work and is already fixed on `main` — different directories yield different identities, verified from the routing filters (`src/state.rs:823`, `src/state.rs:934`) and from both spawn construct sites populating `orchestration_cwd` (`src/tab.rs:540`, `src/spawn.rs:330`).

### The collision has three layers — routing is only the first

Making two same-directory orchestrations *fully* safe is a much deeper hole than the routing bug, because there are three independent layers of shared state and only the first is about tabs at all:

1. **Daemon routing (in-memory).** Which pane receives a delegate, which orchestrator receives work-done feedback. This is the reported symptom and the only layer a per-tab identity can fix.
2. **On-disk coordination files.** Both `.dot-agent-deck/worker-task-{role}.md` (`src/state.rs:446`) and `.dot-agent-deck/work-done-{role}.md` (`src/state.rs:967`) are keyed by **role name within a cwd** — no orchestration qualifier, no user qualifier. Any two actors sharing a directory and a role name collide, independent of routing: two tabs of one orchestration, *two differently-named orchestrations* (routing is correct, yet both write `work-done-coder.md` and silently overwrite), or two people in one checkout each running a `coder` role. This layer is a property of the shared directory, not of the orchestration instance.
3. **The working tree itself.** Two orchestrations sharing one checkout share the same source files, git index, and build artifacts. No amount of file-namespacing fixes this — it is the intrinsic hazard of running two sets of agents against one tree, and it is identical to the "two people editing the same checkout" case.

The decisive observation is that layers 2 and 3 are **per-directory** hazards, orthogonal to any per-tab identity: the contending actors are not always tabs, and layer 3 is unfixable by the app. Engineering per-instance file namespacing would paper over one of three cracks while layer 3 stays open — and worse, it could *mislead* users into unsafe parallel edits ("the tool allowed it, so it must be fine"). The clean isolation boundary for all three layers at once is **one working directory per orchestration**, with a separate git worktree per parallel line of work (distinct cwd → distinct routing identity, distinct coordination files, distinct tree). This is already how PRD #120's issue-dispatch works (one worktree per dispatched orchestration) and how the maintainer works in practice.

Recovery today requires killing the app, which leaves in-flight worker tasks uncommitted — so the current documented workaround is "one orchestration tab at a time," which is both stricter than necessary (different directories are already safe) and silent about *why*.

## Solution Overview

Adopt **"one orchestration per working directory"** as the supported model and make the product honest about it, rather than engineering same-directory concurrency that layer 3 makes unsafe anyway. Three prongs:

1. **Routing correctness (unconditional).** The daemon must never silently cross-deliver, even in variants we do not officially support. Add a per-tab `orchestration_id` to `TabMembership::Orchestration`, mint it once per orchestration tab, carry it through the daemon round-trip, and use it as the routing identity in place of `(name, cwd)` (falling back to `(name, cwd)` for older clients). Each tab becomes an isolated routing group even when name + directory are identical. This closes the reported symptom deterministically and keeps the daemon's internal accounting truthful regardless of product policy.

2. **Guard + steer (the product stance).** When a user opens an orchestration in a directory that *already* has a live orchestration, warn them — non-blocking — that same-directory orchestrations share coordination files (`.dot-agent-deck/*-{role}.md`) and one working tree, and point them at a worktree (`/worktree-prd`) as the isolated alternative. The daemon already knows every live orchestration's cwd (via `pane_orchestration_map` / `orchestrator_pane_ids`), so detection is a lookup at new-orchestration spawn time. The warning informs power users without forbidding them; it makes layers 2 and 3 explicit at the exact moment they become a risk.

3. **Document the model.** Replace the "one orchestration tab at a time" workaround wording with the real rule: concurrent orchestrations are safe *across directories*; for parallel work use a worktree per orchestration; same-directory concurrency shares files and tree and is discouraged. Point at the worktree flow.

Explicitly **not** in this PRD: per-instance namespacing of the on-disk coordination files (layer 2). It is captured in "Deferred: full same-directory isolation" below so the analysis is not lost, but it is gated on evidence that same-directory concurrency is a real user need (see the issue thread) and it does not resolve layer 3 regardless.

This is a **semantic contract change behind a stable wire** per CLAUDE.md rule 12: the *shape* of `pane_orchestration_map`'s routing key changes even though the JSON frame stays additively compatible. It requires a `changelog.d/140.breaking.md` fragment, the cross-version manual test, and a `PROTOCOL_VERSION` decision (currently `4`, `src/daemon_protocol.rs:154`; recommended **no** bump — the wire is additively forward/backward compatible via the fallback, and the break is versioned through the `.breaking.md` fragment and the `0.x` minor bump).

## Scope

### In Scope

**Routing correctness (prong 1):**

- **Add `orchestration_id: Option<String>` to `TabMembership::Orchestration`** (`src/agent_pty.rs:231`) with `#[serde(default, skip_serializing_if = "Option::is_none")]` so older peers round-trip cleanly. A per-tab instance token, shared by every role pane in one orchestration tab, opaque to the daemon; distinct from `name` (config identity), `orchestration_cwd` (directory disambiguator), and `display_title` (presentation-only).
- **Validate the new field** in `validate_tab_membership` (`src/agent_pty.rs:312`) with the same control-byte / size discipline as `orchestration_cwd` and `role_name`.
- **Mint the id at both construct sites**: the interactive new-pane flow (`src/tab.rs:531`) generates one id for the whole tab before the `for role in config.roles` loop and stamps it on every role's membership; the scheduled / issue-dispatch flow (`src/spawn.rs:326`) does the same per orchestration spawn request. The id need not survive a restart by regeneration (it is re-hydrated from the daemon echo, never regenerated for a live tab), but two tabs created in the same process must never collide — use an existing uuid/pane-id helper or a pid+counter.
- **Key `pane_orchestration_map` on the instance id when present** in the daemon spawn handler (`src/daemon_protocol.rs:997`) by changing its value to an identity type that is either `Instance(id)` or `NameCwd(name, cwd)`, so old and new clients coexist and equality still means "same routing group". Update `src/state.rs:205` and every read site.
- **Route delegate and work-done on the new identity**: `handle_delegate`'s target filter (`src/state.rs:816`) and `handle_work_done`'s orchestrator lookup (`src/state.rs:930`) compare the identity value; both keep working for older clients via the `NameCwd` fallback.
- **Extend the hydration round-trip** so detach/reattach reconstructs the same instance id: the daemon stores `tab_membership` on `AgentRecord` and echoes it via `list_agents`; confirm the id survives `validate_tab_membership` and reaches the TUI bucketing in `src/ui.rs:2043` (`synthesize_from_bucket_metadata`), and that the bucket key includes the instance id so two same-`(name,cwd)` tabs rebuild as two tabs, not one merged tab.
- **Cross-directory regression test** (currently uncovered — the only delegate test, `tests/delegate_prompt_injection.rs`, exercises a single happy-path orchestration): assert a delegate/work-done in one `(name, cwdA)` orchestration never reaches a `(name, cwdB)` orchestration, so the already-shipped fix cannot silently regress.

**Guard + steer (prong 2):**

- **Detect a shared-directory orchestration at spawn time.** Before opening a new orchestration tab (`src/tab.rs:531` path), query the daemon's live records (`list_agents` already returns `tab_membership` with `orchestration_cwd`) for an existing live orchestration whose cwd equals the new one. If found, surface a **non-blocking warning** in the new-pane flow naming the risk (shared `.dot-agent-deck/*-{role}.md` files and one working tree) and suggesting a worktree via `/worktree-prd`. The user may proceed.
- **Warning copy** points concretely at the worktree alternative and states plainly that same-directory orchestrations are not isolated. No hard block (a hard refusal would break legitimate different-named same-dir orchestrations that route correctly today, and would be more paternalistic than the maintainer's own practice warrants).

**Document (prong 3):**

- **Docs**: replace the "one orchestration at a time per user/machine" wording (wherever the orchestration limitation is documented) with: concurrent orchestrations are safe across directories; use a worktree per orchestration for parallel work; same-directory concurrency shares coordination files and the working tree and is discouraged. Link the worktree flow.

**Release hygiene:**

- **`changelog.d/140.breaking.md`** (semantic contract change per rule 12) plus a changelog fragment via `dot-ai-changelog-fragment`. Bug-fix framing: "fix: concurrent orchestrations no longer cross-deliver delegate/work-done signals; same-directory orchestrations now warn and point at worktrees."
- **Cross-version manual test** (rule 12): older daemon + newer TUI still routes a single orchestration via the `NameCwd` fallback.

### Out of Scope

- **Per-instance file namespacing (layer 2).** Deferred; see below. It does not resolve layer 3 and is gated on real demand for same-directory concurrency.
- **A daemon-per-directory redesign.** The single-daemon architecture (PRD #93) stays; a per-tab instance id is the minimal correct routing partition.
- **Hard-blocking same-directory orchestrations.** The guard warns, it does not forbid.
- **Anything about the working tree (layer 3).** Unfixable by the app; worktrees are the answer.

### Deferred: full same-directory isolation (only if a real need emerges)

Captured so the analysis is not lost. If the issue thread establishes that same-directory concurrency is genuinely needed (rather than "worktrees would serve"), the additional build is: namespace the coordination files by instance id — `.dot-agent-deck/<orchestration_id>/worker-task-<role>.md` and `.../work-done-<role>.md` — threaded through both write sites (`src/state.rs:446`, `src/state.rs:967`) and the injected pane pointers (`src/state.rs:457`, `src/state.rs:1002`), plus a compaction/cleanup story for the per-instance subdirectories on tab close. Even with this, layer 3 (shared working tree) remains unsafe, so the guard/warning from prong 2 stays. This is why the default recommendation is worktrees, not namespacing.

## Success Criteria

- Two tabs of the same orchestration in the same directory no longer cross-deliver: each orchestrator's delegate reaches only its own workers, and each worker's work-done reaches only its own orchestrator — deterministically, across repeated runs.
- Separate-directory and different-name isolation continue to hold and are now covered by a regression test.
- Opening an orchestration in a directory that already hosts one shows a clear, non-blocking warning pointing at worktrees.
- A newer TUI attached to an older daemon (no `orchestration_id`) still routes a single orchestration correctly via the `(name, cwd)` fallback.
- Detach/reattach of a two-tab same-orchestration setup rebuilds two distinct tabs, each retaining its routing group.
- Docs describe the worktree-per-orchestration model; the "one orchestration at a time" workaround wording is gone.
- `cargo test-fast` green per task; `cargo test-e2e` green pre-PR, including a PTY-attached L2 test.

## Milestones

### Phase 1: Wire type and construct sites

- [ ] **M1.0** — Add `orchestration_id: Option<String>` to `TabMembership::Orchestration` (`src/agent_pty.rs:231`) with the forward-compatible serde attrs. Serde round-trip test: field preserved; older-shape JSON → `None`.
- [ ] **M1.1** — Extend `validate_tab_membership` (`src/agent_pty.rs:312`) to sanitize `orchestration_id`. Test rejects a control-byte id, accepts a valid one.
- [ ] **M1.2** — Mint one id per tab at the interactive construct site (`src/tab.rs:531`), stamped on every role pane's membership.
- [ ] **M1.3** — Mint one id per orchestration spawn at the scheduled / daemon-initiated construct site (`src/spawn.rs:326`).

### Phase 2: Daemon routing identity

- [ ] **M2.0** — Change `pane_orchestration_map`'s value (`src/state.rs:205`) to an `Instance(id)` / `NameCwd(name, cwd)` identity; populate it in the daemon spawn handler (`src/daemon_protocol.rs:997`).
- [ ] **M2.1** — Route `handle_delegate`'s target filter (`src/state.rs:816`) on the new identity; keep orchestrator-self-exclusion and role-match unchanged.
- [ ] **M2.2** — Route `handle_work_done`'s orchestrator lookup (`src/state.rs:930`) on the new identity; the `.find()` is now deterministic because at most one orchestrator shares an instance id.
- [ ] **M2.3** — Update map cleanup (`unregister_pane`, `src/state.rs:759`) and every other read site the type change touches; `cargo build` + `cargo clippy -- -D warnings` clean.

### Phase 3: Hydration round-trip

- [ ] **M3.0** — Ensure `orchestration_id` survives the daemon echo (`AgentRecord.tab_membership` → `list_agents` → `validate_tab_membership`) into the TUI bucketing at `src/ui.rs:2043`; bucket key includes the instance id.
- [ ] **M3.1** — Detach/reattach test: two same-orchestration same-cwd tabs reconnect as two distinct tabs, each retaining its routing group.

### Phase 4: Guard + steer

- [ ] **M4.0** — At new-orchestration spawn (`src/tab.rs:531` path), detect an existing live orchestration in the same cwd via the daemon's records; show a non-blocking warning naming the shared-file / shared-tree risk and pointing at `/worktree-prd`. Add a TUI test (L1) asserting the warning appears for a same-cwd second orchestration and not otherwise (CLAUDE.md rule 4).
- [ ] **M4.1** — Cross-directory regression test: a delegate/work-done in `(name, cwdA)` never reaches `(name, cwdB)`.

### Phase 5: Tests, cross-version, docs, release

- [ ] **M5.0** — Routing unit tests in `src/state.rs`: two same-`(name,cwd)` orchestrations with distinct ids route delegate + work-done in isolation.
- [ ] **M5.1** — L2 PTY-attached e2e (rule 4): real binary, same orchestration in two tabs, no cross-delivery; `.cast`-recording, modeled on `scheduler/dispatch/013` and `tests/e2e_delegate_work_done_chain.rs`.
- [ ] **M5.2** — Cross-version manual test (rule 12): older daemon + newer TUI routes a single orchestration via the `NameCwd` fallback.
- [ ] **M5.3** — `changelog.d/140.breaking.md` + changelog fragment; docs updated to the worktree-per-orchestration model.
- [ ] **M5.4** — PR, Greptile review, cross-version contract check, merge, close #140.

## Key Files

- `src/agent_pty.rs` — `TabMembership::Orchestration` (`:231`), `validate_tab_membership` (`:312`).
- `src/state.rs` — `pane_orchestration_map` (`:205`), `handle_delegate` (`:788`), `handle_work_done` (`:894`), coordination-file writes (`:446`, `:967`), pointers (`:457`, `:1002`), `unregister_pane` (`:759`).
- `src/daemon_protocol.rs` — `StartAgent` role-map population (`:997`), `PROTOCOL_VERSION` (`:154`).
- `src/daemon.rs` — hook-loop dispatch of `Delegate` / `WorkDone` (`:929`, `:941`).
- `src/tab.rs` — interactive construct site (`:531`), `open_orchestration_tab_with_existing_role_panes` (`:647`).
- `src/spawn.rs` — scheduled / daemon-initiated construct site (`:326`).
- `src/ui.rs` — hydration bucketing (`:2043`; `synthesize_from_bucket_metadata` at `:1927`).
- `tests/delegate_prompt_injection.rs`, `tests/e2e_delegate_work_done_chain.rs` — existing coverage to extend.

## Risks and Mitigations

- **Hydration mismatch orphans panes.** If the reattach bucket key omits the instance id, two same-`(name,cwd)` tabs merge into one on reconnect and orphan half the panes. Mitigation: M3.0/M3.1 assert two-tab reconstruction explicitly.
- **Map value-type churn.** Changing `pane_orchestration_map`'s value touches several read sites; a missed one silently reverts to a broken comparison. Mitigation: enum identity with exhaustive `match` so the compiler flags every site (M2.3), plus clippy `-D warnings`.
- **Older-client fallback regressions.** The `NameCwd` fallback must stay byte-equivalent to today's behavior. Mitigation: cross-version manual test (M5.2) and a fallback-path unit test.
- **Warning fatigue / false positives.** A too-eager guard that fires on legitimate different-named same-dir orchestrations would annoy. Mitigation: the warning is informational and non-blocking, and the copy names the specific shared resources so the user can judge.
- **Demo-reel gap (rule 4).** Without the PTY-attached L2 test the feature ships with no reel clip and weaker validation. Mitigation: M5.1 is a hard gate.

## Open Questions

- **Is same-directory concurrency a real need?** Asked on the issue thread. If the answer is "worktrees serve," the "Deferred: full same-directory isolation" section stays deferred. If a concrete need emerges, promote it to a follow-up PRD — the instance id built here is already the partition key it would use.
- **`PROTOCOL_VERSION` bump?** Recommendation: no — the frame stays additively compatible and the break is versioned via `.breaking.md` + the `0.x` minor bump. Confirm during the cross-version contract check.
- **Warning surface.** Where the non-blocking warning renders (new-pane form footer vs. a transient status line) — decide during M4.0 against the existing new-pane UI.
- **Experimental flag (rule 9)?** This is a bug fix plus a warning on existing behavior, not a new standalone surface, so it likely ships un-flagged. Confirm with the maintainer at `/prd-start`.
- **Is prong 1 (per-tab `orchestration_id`) still worth its cost once PRD #220 ships?** #220 makes worktree-per-orchestration a one-step action, so every dispatched orchestration gets a distinct cwd and `(name, cwd)` already disambiguates — prong 1 then only protects the *discouraged* same-cwd, two-tab case. Decision fork: keep prong 1 as belt-and-suspenders ("the daemon must never silently cross-deliver, even in unsupported variants" — cheap insurance for a case the non-blocking guard only warns about), or trim this PRD to guard + docs and let #220's worktree model carry isolation. Recommendation: keep it, but scope it honestly as insurance rather than "the fix for concurrency" (#220 is the real ergonomic fix). Confirm at `/prd-start`.
- **Guard/warning handoff to #220.** This PRD's prong-2 warning points at `/worktree-prd`; when #220's dispatcher mode lands, repoint the copy at it. Tracked as an explicit handoff in #220's docs milestone.
