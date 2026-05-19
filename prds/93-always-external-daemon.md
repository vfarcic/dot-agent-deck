# PRD #93: Always-external daemon (unify local and remote architecture)

**Status**: Planning
**Priority**: Medium
**Created**: 2026-05-17
**GitHub Issue**: [#93](https://github.com/vfarcic/dot-agent-deck/issues/93)
**Depends on**: PRD #76 closing first. Ideally PRD #92's audit informs which invariants need extra care during the unification.

## Problem Statement

The deck has two architectures bolted together:

- **Local mode**: the daemon runs in-process alongside the TUI. `EmbeddedPaneController::ControllerMode::LocalDeck`. `PaneBackend::Pty`. PTYs are owned by the deck process; quitting the deck kills the agents.
- **Remote mode**: the daemon runs as a separate process. `EmbeddedPaneController::ControllerMode::RemoteDeckLocal`. `PaneBackend::Stream`. PTYs are owned by the daemon; the deck is a terminal that attaches and detaches.

Every code path that touches pane I/O, lifecycle, hook events, or session state has to handle both modes. The branching points are not always co-located: some live in `embedded_pane.rs`, some in `state.rs`, some in `ui.rs`, some in `daemon.rs`. The two arms diverge subtly — fields exist in one and not the other, error handling differs, event delivery differs.

The practical consequence is that the IPC-boundary class of bugs (PRD #76 M2.11–M2.20) only surfaces when someone tests on a real remote VM. Local development exercises the wrong code path. Every IPC invariant we have introduced (`pane_id_env`, broadcast event fanout, tab restoration, agent_type plumbing, `DOT_AGENT_DECK_PANE_ID` re-delivery) is implicitly under-tested because day-to-day development hits the in-process path that does not need any of them.

This is a structural problem, not a bug. The fix is to delete the in-process path entirely.

## Solution Overview

Make the deck always talk to an external daemon. Even when the user runs `dot-agent-deck` against their own machine, the binary spawns (or attaches to) a separate daemon process and uses the attach protocol to talk to it.

Concretely:

- `dot-agent-deck` on first invocation spawns the daemon as a background process if one is not already running for the current user. The daemon binds a per-user Unix socket at a well-known path (e.g. `${XDG_RUNTIME_DIR}/dot-agent-deck.sock`).
- The TUI always connects to that socket. There is no in-process daemon code path.
- The daemon exits N seconds (configurable, default 30s) after the last attached client disconnects, **unless** there are still managed agents alive — in which case it stays up to host them (matching today's remote behavior).
- `PaneBackend::Pty` and `EmbeddedPaneController::ControllerMode::LocalDeck` are deleted. Everything uses `PaneBackend::Stream` and `RemoteDeckLocal` (which can probably drop the "Remote" prefix once it is the only mode).

## Scope

### In Scope

- **Auto-spawn daemon on TUI startup** if not already running for the current user. Use file locking on the socket path to avoid race conditions when two TUIs start simultaneously.
- **Idle shutdown**: daemon exits N seconds after last client disconnects *and* no managed agents remain. Configurable timeout. Default 30s.
- **Persistent daemon mode**: if managed agents remain (i.e. user spawned agents and detached), daemon stays up — matching remote behavior. User can `dot-agent-deck remote stop` (or equivalent local command) to force shutdown.
- **Delete `PaneBackend::Pty`** and all its call sites.
- **Delete `ControllerMode::LocalDeck`** and collapse the controller down to a single mode.
- **Wire the same attach protocol** for local as remote. The transport is Unix sockets in both cases, so the path is the same.
- **Rename the remaining types** for clarity (`RemoteDeckLocal` → `Attached`, `PaneBackend::Stream` → `PaneBackend` if it is the only variant left).
- **Update tests**: the local-mode integration tests need to start a real daemon (most already do via `start_real_server`; the rest get migrated).
- **Update docs**: `docs/installation.md` and `docs/getting-started.mdx` mention the daemon lifecycle. Quit/detach dialog wording (PRD #76 M2.18) is reconsidered in this new world.

### Out of Scope

- Network transport. Local and remote both use Unix sockets — over `ssh -t` for remote (existing) or directly for local (new). No new wire types.
- Cross-user daemon sharing. The daemon is per-user. Multiple users on the same host get their own daemons.
- Windows support. The Unix socket path is Unix-only. PRD #42 (native Windows support) handles Windows; this PRD assumes Unix.
- Migrating users from the old local mode. The new binary just works differently on first run. No state migration; old in-process agents die with the old binary.
- The performance question of "is IPC fast enough for local TUI updates" — the attach protocol is already known to be fast enough for remote use over SSH, so direct Unix socket use is bounded above by that.

## Success Criteria

- `grep -r "PaneBackend::Pty\|ControllerMode::LocalDeck" src/ tests/` returns no production-code matches (test-only matches removed too).
- A fresh `dot-agent-deck` invocation on a machine with no running daemon transparently spawns one and works identically to today's local mode from the user's perspective (modulo any deliberate behavior changes for daemon lifecycle).
- Quitting the deck with no running agents shuts the daemon down within the idle timeout.
- Quitting the deck with running agents leaves them running; reconnecting picks them back up — same as today's remote behavior.
- All PRD #76 IPC-boundary regressions (M2.11–M2.20 fixes) are exercised by the standard `cargo test` run because they now sit in the only code path.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass. `cargo test` passes.
- User can run the deck locally without ever noticing anything has changed in the common case.

## Milestones

### Phase 1: Daemon lifecycle

- [ ] **M1.1** — Implement auto-spawn: `dot-agent-deck` checks for a running daemon at the per-user socket path; if absent, spawns one as a detached background process.
- [ ] **M1.2** — Implement idle shutdown: daemon tracks attached-client count and managed-agent count; when both hit zero for N seconds, it exits.
- [ ] **M1.3** — Implement startup race protection: two TUIs starting at the same instant must not both spawn a daemon. Use socket file-lock or atomic bind.

### Phase 2: Remove in-process path

- [ ] **M2.1** — Delete `PaneBackend::Pty`. Replace all call sites with the `Stream` variant. Tests that constructed `Pty` panes get migrated to spawn against the real daemon.
- [ ] **M2.2** — Delete `ControllerMode::LocalDeck`. Collapse `EmbeddedPaneController` down to a single mode. Rename `RemoteDeckLocal` to something mode-agnostic (e.g. `Attached`).
- [ ] **M2.3** — Sweep call sites that match on `ControllerMode` or `PaneBackend` and simplify the now-trivial branches.

### Phase 3: Tests and validation

- [ ] **M3.1** — Run the full test suite; migrate any test that constructed in-process panes directly. Most tests already use `start_real_server` and need no change.
- [ ] **M3.2** — Validate that PRD #76 M2.11–M2.20 regression tests still pass and are now exercised by `cargo test` without a `--features remote` flag or equivalent.
- [ ] **M3.3** — Manual smoke test: fresh machine, no daemon, run `dot-agent-deck` → confirm transparent spawn, confirm idle shutdown, confirm detach/reconnect.

### Phase 4: Docs and release

- [ ] **M4.1** — Update `docs/installation.md` and `docs/getting-started.mdx` to describe the daemon lifecycle in a "How it runs" subsection (short, not user-facing concern in the common case).
- [ ] **M4.2** — Reconsider PRD #76 M2.18 (quit/detach dialog) — now that local and remote share the same lifecycle, the dialog choice probably collapses further.
- [ ] **M4.3** — Changelog fragment via `dot-ai-changelog-fragment`. Focus on user-visible behavior change (daemon now persistent across deck restarts; agents survive).
- [ ] **M4.4** — PR, review, audit, merge, release.

## Key Files

- `src/embedded_pane.rs` — `ControllerMode`, `PaneBackend`, write/read paths.
- `src/daemon.rs` — daemon startup, idle-shutdown logic.
- `src/main.rs` — auto-spawn on first run, lock contention.
- `src/state.rs`, `src/ui.rs` — anywhere that matches on `PaneBackend` or `ControllerMode`.
- `tests/*.rs` — migration of any test that constructed in-process panes.
- `docs/installation.md`, `docs/getting-started.mdx` — lifecycle prose.

## Design Decisions

### 2026-05-17: Auto-spawn over user-managed daemon

Two alternative shapes were considered: (a) require the user to start the daemon explicitly (`dot-agent-deck daemon start`) before running the TUI, or (b) auto-spawn transparently. Auto-spawn wins because the entire point of unifying is to make the IPC path the default — adding a manual setup step would push users back toward "I'll just `dot-agent-deck` and not deal with this" frustration. The daemon process must be invisible in the common case.

### 2026-05-17: Idle-shutdown over always-on

The daemon could stay up forever once started. Rejected because: (a) users testing the deck would accumulate background daemon processes they did not ask for, (b) the no-agents-running case is the dominant case for first-time users, and (c) idle shutdown matches the principle that the daemon is invisible. The persistent-when-agents-alive carve-out preserves the actual user benefit (detach/reconnect agents survive).

### 2026-05-17: Run after PRD #76 closes and PRD #92 informs

This is a big-bang refactor that deletes a code path. Doing it while PRD #76's remote path is still stabilizing would conflate bugs in the remote code with bugs in the unification. Waiting until PRD #76 closes gives a stable baseline. PRD #92's audit findings inform which call sites need extra care during the deletion — anywhere the audit flagged as "likely broken" is also where the `LocalDeck` branch was hiding a bug that needs fixing in the unified path, not silently inherited.

### 2026-05-17: Delete `PaneBackend::Pty` entirely rather than wrap it

Considered keeping `PaneBackend::Pty` as a thin wrapper that talks to an in-process daemon-equivalent. Rejected — the whole point is to delete the in-process semantics, not abstract over them. Two variants of `PaneBackend` is a tax on every code path that touches panes; reducing to one is a meaningful simplification.
