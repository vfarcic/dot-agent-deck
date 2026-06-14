# PRD #89: Auto-restore TUI state on attach; remove `--continue` flag

**Status**: Planning
**Priority**: Medium
**Created**: 2026-05-16
**Last Updated**: 2026-06-14
**GitHub Issue**: [#89](https://github.com/vfarcic/dot-agent-deck/issues/89)

> **2026-06-14 refresh — read before implementing.** This PRD predates the daemon architecture shipping. The landscape has moved: PRD #76 (Remote Agent Environments) **shipped**, and PRD #93 (always-external daemon) made the daemon the **unified architecture even locally** — `run_tui_session` now always connects to an external daemon (`ensure_external_daemon_or_die`). Two consequences: (1) the original line references below are **stale** (e.g. the `continue_session` gate is no longer at `src/ui.rs:2748` — it is threaded through `run_tui` around `src/ui.rs:5862`, plus `5426/5462/5849`); re-locate by symbol, not line number, at implementation time. (2) Several milestones are **partially delivered** by #76/#93 and the scope has **grown** to absorb orchestration-tab restore (formerly PRD #74, now closed). See the new Design Decision entries dated 2026-06-14 and the revised Dependencies/Scope/Milestones below.

## Problem Statement

Today the TUI's session-restore behavior is gated by a `--continue` flag, and the snapshot that flag reads is only written at clean quit time. That worked for the original local-only mental model — user quits Ctrl+Q, reads what they had — but it doesn't survive contact with the remote workflow PRD #76 introduced:

1. **`--continue` doesn't match remote semantics.** In remote mode, M2.11/M2.12 hydrate the TUI from the daemon registry on every `connect`. Users don't run `--continue`; reattach is the default. So `--continue` is either redundant (daemon has state) or actively misleading (loads a snapshot from the last clean quit, which may be weeks stale or empty, ignoring what the daemon currently has).

2. **The snapshot rots in the "daemon runs forever" workflow.** The snapshot is written at clean quit (`src/ui.rs:4542-4554`). Users on a long-lived remote daemon never quit; they detach. So when the daemon crashes or is intentionally torn down, the snapshot on disk is from the last quit (potentially weeks ago) or doesn't exist at all. `--continue` in that state restores nothing useful.

3. **Two mental models for the same task.** Local: `dot-agent-deck` for empty, `dot-agent-deck --continue` for restore. Remote: `dot-agent-deck connect` always restores (via daemon hydration). Same user, same intent, two different invocations.

4. **The local empty-by-default is the wrong optimization.** Restoring a workspace is the common case; starting fresh is the rare one. The CLI surface inverts that.

Working assumption: the daemon doesn't crash (or crashes rarely enough that "best-effort recovery on next attach" is sufficient). Daemon-side registry persistence is *out of scope* for this PRD — even if it existed, agent PTYs die with the daemon anyway, so the recovery flow still reduces to "respawn agents from snapshot," which is what this PRD delivers.

## Solution Overview

Unify the restore model across local and remote into a single behavior:

- **On every TUI startup**, attempt daemon hydration first. If the daemon has agents, that wins. If the daemon is empty (fresh spawn or crash recovery), fall back to the disk snapshot and recreate the workspace.
- **Keep the snapshot fresh.** Write it on detach and on every meaningful TUI state change (new pane, rename, mode tab open/close, agent stop/restart, orchestration changes) — not only at clean quit.
- **Delete the `--continue` flag.** With auto-restore as the default, there is no decision left for the user to express via a flag.
- **Provide a "fresh start" escape hatch.** The snapshot is a single global file, so one CLI affordance covers it: `dot-agent-deck snapshot clear` deletes the global snapshot. (`dot-agent-deck remote remove <name>` is registry-only and does not touch it — see the 2026-06-14 escape-hatch Design Decision.)

As a side effect, daemon crash recovery is "free": a respawned-empty daemon triggers the same snapshot fallback path as a first-time launch on a machine with prior state.

## Scope

### In Scope

- **Continuous snapshot freshness.** Write the saved-session snapshot to disk on detach (ssh disconnect, Ctrl+W) and on every meaningful TUI state change (new pane, rename, mode tab open/close, agent stop/restart, orchestration changes). Coalesce/debounce as needed so we're not writing on every keystroke.
- **Auto-restore on TUI startup.** Both `dot-agent-deck` (local) and `dot-agent-deck connect` (remote): attempt daemon hydration, fall back to snapshot if daemon is empty, fall through to empty dashboard only if both are empty. Daemon state wins over snapshot when both exist. (Post-#93, local startup is itself daemon-backed, so this is one uniform path, not separate local/remote logic.)
- **Orchestration-tab restore (absorbed from closed PRD #74).** A single-agent/mode workspace is not enough — the restore model must also recreate **orchestration tabs**: the orchestrator pane and its prompt, the role panes in their saved order, and the `start_role_index` cursor. Two sub-paths, and they are NOT equivalent:
  - *Daemon-hydration path (warm daemon):* PRD #76 M2.12 + PRD #111 already hydrate orchestration tabs from the daemon registry. **Verify** this covers orchestrator+role panes, prompts, role order, and `start_role_index` end-to-end; if hydration is already complete, no new capture work is needed for the warm-daemon case — only a regression test asserting it.
  - *Snapshot-fallback path (daemon empty — fresh machine or crash recovery):* the disk snapshot must carry enough orchestration metadata to rebuild the tab when the daemon has nothing to hydrate from. This is where the old #74 schema work (orchestration metadata on the saved pane: role order, `orchestrator_prompt`, resolved config name+project for re-resolution, `start_role_index`, a `version` field) genuinely re-homes. On config drift (config deleted, orchestration renamed, role removed) surface a clear `session_warnings` message and fall back to a plain dashboard pane rather than a half-broken tab.
- **Delete the `--continue` flag.** Remove the CLI argument, the `continue_session` plumbing, and the conditional in `src/ui.rs:2748`. The saved-session-load path becomes unconditional (gated only on whether the daemon was empty).
- **Fresh-start escape hatch.** The saved snapshot is a single global file, not per-deck (see the 2026-06-14 escape-hatch Design Decision), so there is exactly one fresh-start action: `dot-agent-deck snapshot clear`, which deletes the global snapshot via `config::SavedSession::clear()`. `dot-agent-deck remote remove <name>` stays registry-only and intentionally does NOT touch the snapshot.
- **Backward-compat consideration.** This changes the meaning of `dot-agent-deck` (no flag) from "empty session" to "restore last setup." Document as a deliberate breaking change in the changelog.
- **Tests.** Snapshot is written on each in-scope state change; auto-restore prefers daemon over snapshot; empty daemon + non-empty snapshot recreates the workspace; empty daemon + empty snapshot lands at empty dashboard cleanly.
- **Documentation.** Update `docs/` to reflect the new restore behavior, remove all references to `--continue`, document the fresh-start escape hatch.

### Out of Scope

- **Daemon-side registry persistence.** The daemon does not checkpoint its registry to disk. Agent PTYs die with the daemon regardless of whether the registry survives, so daemon-side persistence buys nothing the TUI-side snapshot fallback doesn't already provide.
- **Recovering in-flight state.** Half-typed prompts, buffered unflushed PTY output, and the live PTY itself are unrecoverable across any process crash. The snapshot recovers the workspace structure, not the exact instant.
- **Renaming or restructuring `SavedSession`.** Schema stays compatible with existing on-disk snapshots; bumping the schema is a separate concern.
- **Changes to PRD #76 milestones beyond narrowing M2.14.** Specifically, M2.14 in PRD #76 will be amended to drop `--continue` from its scope (this PRD deletes the flag entirely; nothing left for M2.14 to propagate).

## Success Criteria

- `dot-agent-deck` (local, no flag) on a machine with a prior snapshot restores the previous workspace; on a fresh machine lands at an empty dashboard.
- `dot-agent-deck connect <name>` (remote, no flag) attaches to the daemon and restores any hydrated agents; if the daemon is empty (fresh spawn, crash recovery), falls back to the snapshot and recreates the workspace.
- After a daemon crash and reconnect, the TUI ends up with the same panes/tabs the user had before the crash (modulo in-flight state). Agent processes are respawned fresh; each agent's own conversation state is restored by the agent's own command line (e.g., `claude --continue`).
- `--continue` is removed from the CLI surface and from `--help`. Existing users of `--continue` get a clear deprecation/removal message if they try to use it.
- A user who wants a fresh start has one obvious action: `dot-agent-deck snapshot clear`, which deletes the single global saved-session snapshot. (The snapshot is global, not per-deck; `dot-agent-deck remote remove <name>` is registry-only and intentionally does NOT clear it.)
- Snapshot writes are coalesced so they don't impact TUI responsiveness during heavy interaction.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` all pass.

## Milestones

### Phase 1: Snapshot freshness

- [ ] **M1.1** — Identify the set of "meaningful state change" events that should trigger a snapshot write: new pane created, pane renamed, pane closed, mode tab opened/closed, orchestration tab opened/closed/role-changed, agent stop, agent restart. Document the list inline in the PRD or as a Design Decision.
- [ ] **M1.2** — Add a snapshot-write trigger to each of those events. Coalesce/debounce writes so a burst of changes (e.g., orchestration setup) produces one or two disk writes, not dozens.
- [ ] **M1.3** — Add a snapshot-write trigger to the detach paths: ssh disconnect (remote), Ctrl+W close-pane (where applicable), explicit detach from the quit-confirm dialog.
- [ ] **M1.4** — Tests confirming each trigger writes a snapshot and that coalescing actually coalesces.

### Phase 2: Auto-restore on TUI startup

> **2026-06-14:** Post-#93 the local TUI is daemon-backed, so hydration-first applies uniformly — M2.2 is one path, not local-vs-remote. The snapshot-load gate is no longer at `src/ui.rs:2748`; it is the `continue_session` block around `src/ui.rs:5862` in `run_tui`. Re-locate by symbol.

- [ ] **M2.1** — Make the snapshot-load path (the `if continue_session { … }` block in `run_tui`, ~`src/ui.rs:5862` — verify current location) unconditional (no longer gated on `continue_session`). Restore from snapshot on every TUI startup.
- [ ] **M2.2** — Wire the daemon-state-vs-snapshot precedence: if M2.11/M2.12 hydration produced any panes, skip snapshot restore. If hydration produced zero panes, load and apply the snapshot. Decide via a structural check (any hydrated `managed_pane_id` in `state`), not a flag.
- [ ] **M2.3** — Verify the existing M2.11/M2.12 hydration path still works unchanged in the common detach/reattach case. No regressions for users currently relying on automatic remote restore.
- [ ] **M2.4** — Tests: daemon-with-agents wins over snapshot; daemon-empty + non-empty-snapshot recreates from snapshot; both empty lands at empty dashboard.

### Phase 2b: Orchestration-tab restore (absorbed from closed PRD #74)

- [ ] **M2b.1** — **Verify the daemon-hydration path.** Confirm by inspection + a regression test that PRD #76 M2.12 + PRD #111 hydration already recreate an orchestration tab end-to-end from a warm daemon: orchestrator pane + prompt, role panes in saved order, and `start_role_index`. If complete, no new capture code is needed for the warm case.
- [ ] **M2b.2** — **Snapshot-fallback capture.** For the daemon-empty case (fresh machine / crash recovery), extend the saved-pane schema with orchestration metadata (role order, `orchestrator_prompt`, resolved config name + project path for re-resolution, `start_role_index`, and a `version: u32`), `Option<…>` + `#[serde(default)]` so old snapshots still parse. Port the design from `prds/done/74-restore-orchestration-tabs-on-continue.md`.
- [ ] **M2b.3** — **Snapshot-fallback restore branch.** Rebuild the orchestration tab from the snapshot when the daemon is empty: re-resolve the `OrchestrationConfig`, recreate orchestrator + role panes in order, re-issue commands, restore `start_role_index`. On config drift (config deleted, orchestration renamed, role removed) surface a clear `session_warnings` message and fall back to a plain dashboard pane — never a half-broken tab.
- [ ] **M2b.4** — Tests: warm-daemon hydration restores an orchestration tab (M2b.1); daemon-empty + snapshot recreates it; old snapshot without the orchestration field still parses; drift triggers the warning + plain-pane fallback.

### Phase 3: Delete `--continue`

> **2026-06-14:** The remote side is **already done** — `run_connect` ignores `_continue_session` ("applies to a laptop-side TUI that no longer exists"). Remaining live work is the **local** plumbing. Line numbers below are stale; re-locate by symbol (`grep continue_session`).

- [ ] **M3.1** — Remove the `--continue` argument from `Cli` (currently `src/main.rs:25-26` — `#[arg(long = "continue")] continue_session: bool`).
- [ ] **M3.2** — Remove the `continue_session: bool` parameter from `run_dashboard`, `run_tui_session`, the TUI internals (`run_tui`, ~`src/ui.rs:5426`), and drop the already-ignored `_continue_session` from `run_connect`. Sweep all callers via `grep continue_session`.
- [ ] **M3.3** — Update help text and the in-TUI restore hint (`"  Restore: dot-agent-deck --continue"`, currently ~`src/ui.rs:9698`) to remove the obsolete reference. Also sweep the explanatory `--continue` comments elsewhere in `run_tui` (~`5459`, `5465`, `5850`, `6216`, `7590`) so they don't describe a removed flag.
- [ ] **M3.4** — Add a friendly error message if a user runs `dot-agent-deck --continue` after removal (clap will reject the unknown flag with its default message; a custom message that tells them auto-restore is the new default is a nice touch).

### Phase 4: Fresh-start escape hatch

- [ ] **M4.1** — *Resolved (see the 2026-06-14 escape-hatch Design Decision below).* Investigation found the saved snapshot is a **single global file** (`config::session_path()` → `DOT_AGENT_DECK_SESSION` or `~/.config/dot-agent-deck/session.toml`), not keyed per deck — there is no per-deck saved state on disk. There is also no top-level `dot-agent-deck remove`; the real command is `dot-agent-deck remote remove <name>`, which mutates only the local registry (`remotes.toml`). Decision (Option 1): `remote remove` stays **registry-only and intentionally does NOT clear the snapshot**. The one obvious global fresh-start action is `dot-agent-deck snapshot clear` (M4.2) — so there is no per-deck wiring to add.
- [ ] **M4.2** — Add a `dot-agent-deck snapshot clear` subcommand — a `snapshot` subcommand **group** (room for future snapshot actions) with a `clear` **action** — that deletes the local snapshot by calling `config::SavedSession::clear()` (the same teardown clear, so it honors the `DOT_AGENT_DECK_SESSION` override). Ships visible by default (no experimental feature flag). Exact CLI shape resolved as a Design Decision (`snapshot clear`, not `reset`/`--reset`).
- [ ] **M4.3** — Tests for the escape hatch: `snapshot clear` deletes the local snapshot and exits 0 (`session/snapshot/001`); and a green-guard that `remote remove <name>` leaves the global snapshot intact (`session/snapshot/002`).

### Phase 5: Documentation + release

- [ ] **M5.1** — Update the user-facing docs that mention `--continue` — verified current set: `docs/getting-started.md` and `docs/session-management.md` — to describe the new auto-restore model and the fresh-start escape hatch. (`session-management.md` is also where closed PRD #74 would have added its orchestration-restore paragraph; cover orchestration-tab restore here instead.)
- [ ] **M5.2** — Draft a changelog fragment (via the `dot-ai-changelog-fragment` skill) flagging this as a breaking change with a one-line migration note ("Remove `--continue` from any wrapper scripts; auto-restore is now the default.").
- [ ] **M5.3** — Tag a release (`dot-ai-tag-release`) once everything lands.

## Dependencies

- **PRD #76 (Remote Agent Environments) — ✅ SHIPPED (archived `prds/done/`, merged PR #95).** Its M2.11/M2.12 hydration (extended by PRD #111 for orchestration tabs) is now the in-place mechanism for daemon-state restore. The dependency is satisfied; the auto-restore logic in M2.2 can build on hydration directly rather than racing interim state.
- **PRD #76's M2.14 — ✅ already resolved in #76's favour of this PRD.** #76 M2.14 is recorded as *"`--continue` was originally in scope, but PRD #89 replaces the flag with unconditional auto-restore; nothing left to propagate for that case"* (M2.14's remaining `--theme` propagation was deferred to PRD #93). So the remote/`connect` side **already** treats `--continue` as a no-op (`run_connect` ignores `_continue_session` — *"applies to a laptop-side TUI that no longer exists in this flow"*). Phase 3 (delete the flag) is therefore partly done on the remote side; the live work is the **local** `run_dashboard`/`run_tui_session` path that still threads `continue_session`.
- **PRD #93 (Always-external daemon) — ⏳ IN PROGRESS (Phases 1–3 complete; Phase 4 in flight), now the primary architectural dependency.** #93 collapsed local and remote into one daemon-backed architecture: `run_tui_session` always connects to an external daemon, even locally, so **daemon hydration now applies to local startup too** — not just `connect`. This *simplifies* this PRD (M2.2's "daemon-state-vs-snapshot precedence" is one uniform path, not local-vs-remote), but it means #89 should **land after #93's Phase 4** (or coordinate closely) so it builds on the settled unified flow rather than a moving target. Re-validate every code reference against the post-#93 source.

## Key Files

- `src/main.rs` — `Cli` flag removal, parameter plumbing changes.
- `src/ui.rs` — snapshot-load unconditional, daemon-state-vs-snapshot precedence, snapshot-write triggers on TUI state changes, and the in-TUI restore hint. **Line refs below are stale (post-#93); locate by symbol:** the `continue_session` gate is in `run_tui` (~`5862`; param ~`5426`), the restore hint is ~`9698`, the pre-teardown snapshot is ~`7590`.
- `src/config.rs` — `SavedSession::snapshot` / `load`; the orchestration-metadata schema extension (M2b.2) and a possible debounce/coalesce wrapper.
- `src/main.rs` — `Cli` flag removal (~`25-26`) and `run_dashboard`/`run_tui_session` parameter plumbing; drop the already-ignored `_continue_session` from `run_connect` (~`872`).
- `src/state.rs` — the daemon-hydration partition + a "is this hydration empty?" helper for M2.2.
- `src/tab.rs`, `src/pane.rs`, `src/spawn.rs` — orchestration-tab hydration/rebuild glue (PRD #76 M2.12 + #111) that M2b.1 must verify and M2b.3 reuses for the snapshot-fallback path.
- `docs/getting-started.md`, `docs/session-management.md` — remove `--continue` references; document auto-restore + the orchestration-tab restore paragraph.

## Design Decisions

### 2026-05-16: Why this PRD exists, why now

PRD #76 surfaced the gap when a user asked: "If I `connect --continue` and the daemon is running, will it ignore the flag? And if the daemon is dead?" The honest answer was *neither plain `connect` nor `connect --continue` does what users want after a daemon crash*, because the snapshot only refreshes at clean quit. Rather than patch `--continue` to fit remote semantics (a moving target), unify the model: auto-restore on every startup, snapshot stays fresh continuously, `--continue` becomes vestigial and goes away.

### 2026-05-16: Daemon-side registry persistence rejected as scope

Considered "make the daemon's registry survive its own crash" as a complementary mechanism. Rejected because the agent PTY processes themselves die with the daemon (they're its child processes), so the registry surviving without the PTYs is metadata about nothing. The TUI-side snapshot-fallback path already covers crash recovery by re-spawning agents from saved structure, and each agent's own conversation state lives in its own state dir. So daemon-side persistence adds zero user-visible benefit.

### 2026-05-16: Breaking change is the right call

Removing `--continue` and making restore the default flips the meaning of plain `dot-agent-deck` from "empty session" to "restore last setup." This is a deliberate breaking change. Justification: restoring is the common case; starting fresh is rare. The current CLI optimizes for the rare case. New users are better served by the new default; existing users get a one-line changelog migration. Worth it.

### ~~Open: shape of the local "fresh start" command~~ — RESOLVED 2026-06-14

The local case needed an analogous action to deck removal. Options considered: `dot-agent-deck reset`, `dot-agent-deck --reset`, `dot-agent-deck snapshot clear`, or a TUI affordance ("Quit and clear saved state" in the quit-confirm dialog). **Resolved** in favour of `dot-agent-deck snapshot clear` — see the 2026-06-14 escape-hatch Design Decision below for the full rationale (the snapshot turned out to be a single global file, which reshaped the whole escape-hatch model).

### 2026-06-14: Refresh against the shipped daemon architecture (#76 shipped, #93 unified local+remote)

**Decision.** Re-anchor this PRD on the daemon architecture that shipped after it was written, rather than the local-only `--continue` mental model it was drafted against.

**Rationale.** When #89 was written, #76 was in flight and the daemon was a remote-only concern. Since then: #76 **shipped** (hydration M2.11/M2.12, plus #111 for orchestration), and #93 made the daemon the **unified architecture even locally** — `run_tui_session` always connects to an external daemon. Verified in current source: `ensure_external_daemon_or_die` runs on every local startup, and `run_connect` already ignores `_continue_session`.

**Impact.**
- *Dependencies:* #76 dependency satisfied; **#93 (in flight) becomes the primary dependency** — land #89 after #93 Phase 4 to build on the settled flow.
- *Phase 2 (auto-restore):* M2.2's daemon-vs-snapshot precedence is now **one uniform path** (local is daemon-backed too), not local-vs-remote branching — a simplification.
- *Phase 3 (delete `--continue`):* the **remote side is already done** (flag ignored on `connect`); remaining live work is the **local** `run_dashboard`/`run_tui_session` plumbing that still threads `continue_session`.
- *Code refs:* all original `src/ui.rs:NNNN` line numbers are **stale** — `continue_session` now lives around `src/ui.rs:5862` (plus `5426/5462/5849`). Re-locate by symbol at implementation time.

**Owner.** Viktor (decided in 2026-06-14 planning discussion).

### 2026-06-14: Absorb PRD #74 (orchestration-tab restore); #74 closed as superseded

**Decision.** Close PRD #74 ("Restore orchestration tabs with `--continue`") as *No Longer Needed* and **re-home its goal into #89's restore scope**.

**Rationale.** #74 was designed to extend the `--continue` + clean-quit snapshot mechanism to orchestration tabs. #89 **deletes that flag** and replaces the mechanism with daemon-hydration-first + continuous snapshot, so #74 would have built on a foundation this PRD removes — a direct conflict. The *user need* (orchestration tabs survive restart/reattach) stays valid and belongs wherever the restore model now lives: here.

**Impact.**
- New In-Scope item: **orchestration-tab restore**, split into the daemon-hydration path (verify #76/#111 already cover it; add a regression test) and the snapshot-fallback path (port #74's orchestration-metadata schema onto the saved pane for the daemon-empty case).
- New milestone phase (below) for orchestration restore + drift-fallback warnings.
- #74's schema sketch and restore-branch design remain useful as implementation reference in `prds/done/74-*.md`.

**Owner.** Viktor (decided in 2026-06-14 planning discussion).

### 2026-06-14: M1.1 — the exact state-change events wired to a snapshot write, and how they coalesce

**Decision.** Phase 1 keeps the saved-session snapshot continuously fresh by marking it *dirty* at each meaningful state-change / detach call site and flushing it from the main loop through a coalescer, rather than writing inline at every site. The exact set of events wired to mark the snapshot dirty (all funnel through one helper, `UiState::mark_session_dirty`):

- **New dashboard pane created** — the new-pane form submit (`Action::SpawnPane`, non-orchestration path, right after the `pane_metadata` insert). Covers both a plain dashboard pane and a **mode tab opened** (the mode-tab branch shares that same insert).
- **Orchestration tab opened** — the `Action::SpawnPane` orchestration path, on the `open_orchestration_tab` success branch.
- **Pane / tab closed** — `Action::CloseSelected` (Ctrl+W: closable-tab close, mode/orchestration tab close from a dashboard card, and plain-pane close) and `close_tab_by_index` (the `[×]` click / `Action::CloseTab`), covering **mode tab closed** and **orchestration tab closed**.
- **Pane renamed** — `Action::SaveRename` (after `commit_rename`).
- **Agent stop / restart** — the main loop's reactive pane-pool change (`route_reactive_commands` returning `(old_id, new_id)` pairs, e.g. after a `/clear` restart), which swaps live pane ids.

**Detach paths (M1.3).** Ctrl+W close-pane marks dirty as above. Explicit *detach from the quit-confirm dialog* (`Action::DetachAndQuit`) breaks the loop straight into the existing unconditional pre-teardown snapshot write, so no extra trigger is needed there. *SSH disconnect (remote)* never runs a clean teardown (the process dies on SIGHUP), so it is covered structurally by the continuous freshness above: the snapshot already reflects the latest state-change at disconnect time.

**Coalescing.** `mark_session_dirty` only sets a dirty flag on a `config::SnapshotCoalescer` (a pure-data leading-edge throttle, unit-tested as `session/save/003`). The main loop calls `flush_session_snapshot_if_due` once per iteration (including the 16ms idle ticks): the first pending change writes immediately, further changes are throttled to at most one write per `SNAPSHOT_COALESCE_INTERVAL` (750 ms), and a single trailing write flushes whatever accumulated during the quiet-down. A burst (e.g. orchestration setup spawning many panes) therefore collapses to one or two disk writes, not one per pane.

**Owner.** Viktor (Phase 1 implementation, 2026-06-14).

### 2026-06-14: Phase 4 escape-hatch — the snapshot is global, so `snapshot clear` is the single fresh-start action and `remote remove` stays registry-only

**Decision (Option 1, chosen by Viktor).** There is exactly one local fresh-start action — `dot-agent-deck snapshot clear` — and `dot-agent-deck remote remove <name>` intentionally does NOT clear the snapshot.

**Finding that reshaped M4.1/M4.2.** The PRD was drafted assuming a *per-deck* saved state that `dot-agent-deck remove <name>` would clear. Investigation during Phase 4 found two things that invalidate that framing: (1) the saved snapshot is a **single global file** — `config::session_path()` resolves to `DOT_AGENT_DECK_SESSION` or `~/.config/dot-agent-deck/session.toml`, not keyed per deck/remote — so there is no per-deck saved state on disk to clear; and (2) there is no top-level `dot-agent-deck remove` command at all — the real command is `dot-agent-deck remote remove <name>` (`RemoteCmd::Remove`), which mutates only the local registry (`remotes.toml`) and never references `SavedSession`.

**Why Option 1 (remove stays registry-only).** Because the snapshot is global and shared with the local deck, wiring `remote remove` to call `SavedSession::clear()` would clear the *local* workspace as a side effect of removing an *unrelated remote* registry entry — surprising cross-deck coupling. Keeping `remote remove` registry-only preserves the principle of least surprise: removing a remote touches only remote metadata. The fresh-start need is then served by one explicit, obvious global action rather than smeared across the remove flow.

**Shape.** `snapshot` is a subcommand **group** (parallel to `remote`/`schedule`) with a `clear` **action**, leaving room for future snapshot operations (e.g. `snapshot show`, `snapshot export`). `clear` calls `config::SavedSession::clear()` — the same teardown clear the TUI already uses — so it honors the `DOT_AGENT_DECK_SESSION` override and deletes the one global file. Ships visible by default; **no experimental feature flag** (decided in `.dot-agent-deck/prd-89-context.md`).

**Test impact.** `session/snapshot/001` asserts `snapshot clear` exits 0 and deletes the staged snapshot. `session/snapshot/002` flips from "remove deletes the snapshot" to a **green-guard** asserting `remote remove <name>` leaves the global snapshot intact.

**Owner.** Viktor (decided 2026-06-14).
