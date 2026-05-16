# PRD #89: Auto-restore TUI state on attach; remove `--continue` flag

**Status**: Planning
**Priority**: Medium
**Created**: 2026-05-16
**GitHub Issue**: [#89](https://github.com/vfarcic/dot-agent-deck/issues/89)

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
- **Provide a "fresh start" escape hatch.** Per-deck removal for remote (existing `remove` flow clears that deck's saved state); a small CLI affordance for the local snapshot.

As a side effect, daemon crash recovery is "free": a respawned-empty daemon triggers the same snapshot fallback path as a first-time launch on a machine with prior state.

## Scope

### In Scope

- **Continuous snapshot freshness.** Write the saved-session snapshot to disk on detach (ssh disconnect, Ctrl+W) and on every meaningful TUI state change (new pane, rename, mode tab open/close, agent stop/restart, orchestration changes). Coalesce/debounce as needed so we're not writing on every keystroke.
- **Auto-restore on TUI startup.** Both `dot-agent-deck` (local) and `dot-agent-deck connect` (remote): attempt daemon hydration, fall back to snapshot if daemon is empty, fall through to empty dashboard only if both are empty. Daemon state wins over snapshot when both exist.
- **Delete the `--continue` flag.** Remove the CLI argument, the `continue_session` plumbing, and the conditional in `src/ui.rs:2748`. The saved-session-load path becomes unconditional (gated only on whether the daemon was empty).
- **Fresh-start escape hatch.** For remote: confirm `dot-agent-deck remove <name>` clears the deck's saved state (or add the wiring if it doesn't). For local: add `dot-agent-deck reset` (or equivalent — exact CLI shape is a Design Decision) that deletes the local snapshot.
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
- A user who wants a fresh start has one obvious action: remove the deck (remote) or run the local reset command (local). Both clear the snapshot.
- Snapshot writes are coalesced so they don't impact TUI responsiveness during heavy interaction.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` all pass.

## Milestones

### Phase 1: Snapshot freshness

- [ ] **M1.1** — Identify the set of "meaningful state change" events that should trigger a snapshot write: new pane created, pane renamed, pane closed, mode tab opened/closed, orchestration tab opened/closed/role-changed, agent stop, agent restart. Document the list inline in the PRD or as a Design Decision.
- [ ] **M1.2** — Add a snapshot-write trigger to each of those events. Coalesce/debounce writes so a burst of changes (e.g., orchestration setup) produces one or two disk writes, not dozens.
- [ ] **M1.3** — Add a snapshot-write trigger to the detach paths: ssh disconnect (remote), Ctrl+W close-pane (where applicable), explicit detach from the quit-confirm dialog.
- [ ] **M1.4** — Tests confirming each trigger writes a snapshot and that coalescing actually coalesces.

### Phase 2: Auto-restore on TUI startup

- [ ] **M2.1** — Make the snapshot-load path in `src/ui.rs:2748` unconditional (no longer gated on `continue_session`). Restore from snapshot on every TUI startup.
- [ ] **M2.2** — Wire the daemon-state-vs-snapshot precedence: if M2.11/M2.12 hydration produced any panes, skip snapshot restore. If hydration produced zero panes, load and apply the snapshot. Decide via a structural check (any hydrated `managed_pane_id` in `state`), not a flag.
- [ ] **M2.3** — Verify the existing M2.11/M2.12 hydration path still works unchanged in the common detach/reattach case. No regressions for users currently relying on automatic remote restore.
- [ ] **M2.4** — Tests: daemon-with-agents wins over snapshot; daemon-empty + non-empty-snapshot recreates from snapshot; both empty lands at empty dashboard.

### Phase 3: Delete `--continue`

- [ ] **M3.1** — Remove the `--continue` argument from `Cli` in `src/main.rs:24-25`.
- [ ] **M3.2** — Remove the `continue_session: bool` parameter from `run_dashboard`, `run_tui_session`, `run_connect`, the TUI internals (`src/ui.rs:2405`), and any other callers found by `grep continue_session`.
- [ ] **M3.3** — Update help text and `src/ui.rs:5639` ("Restore: dot-agent-deck --continue") to remove the obsolete reference.
- [ ] **M3.4** — Add a friendly error message if a user runs `dot-agent-deck --continue` after removal (clap will reject the unknown flag with its default message; a custom message that tells them auto-restore is the new default is a nice touch).

### Phase 4: Fresh-start escape hatch

- [ ] **M4.1** — Confirm that `dot-agent-deck remove <name>` already clears that deck's saved state. If it doesn't, add the wiring.
- [ ] **M4.2** — Add a `dot-agent-deck reset` (or `--reset`, or equivalent) subcommand that deletes the local snapshot. Exact CLI shape is a Design Decision; pick during implementation. Confirm with the deck-removal symmetry (one obvious action, no overlap).
- [ ] **M4.3** — Tests for both escape hatches.

### Phase 5: Documentation + release

- [ ] **M5.1** — Update `docs/getting-started.mdx` and any other user-facing doc that mentions `--continue` to describe the new auto-restore model and the fresh-start escape hatch.
- [ ] **M5.2** — Draft a changelog fragment (via the `dot-ai-changelog-fragment` skill) flagging this as a breaking change with a one-line migration note ("Remove `--continue` from any wrapper scripts; auto-restore is now the default.").
- [ ] **M5.3** — Tag a release (`dot-ai-tag-release`) once everything lands.

## Dependencies

- **PRD #76 (Remote Agent Environments) shipping first.** This PRD assumes M2.11/M2.12 hydration is the in-place mechanism for daemon-state restore in remote mode. If #76 is still in flight, the auto-restore logic in M2.2 will conflict with whatever interim state #76 leaves behind.
- **PRD #76's M2.14 narrowed.** This PRD deletes `--continue`, so M2.14 in PRD #76 will be amended to drop `--continue` propagation from its scope (M2.14 will then only cover `--theme` propagation through `ssh -t`).

## Key Files

- `src/main.rs` — `Cli` flag removal, parameter plumbing changes.
- `src/ui.rs` — snapshot-load unconditional, daemon-state-vs-snapshot precedence, snapshot-write triggers on TUI state changes (~`2748`, `4542-4554`, plus new triggers throughout).
- `src/config.rs` — `SavedSession::snapshot` / `load` may grow a debounce/coalesce wrapper.
- `src/connect.rs` — `run_connect` parameter cleanup (`_continue_session` goes away).
- `src/state.rs` — possibly a "is this hydration empty?" helper for M2.2.
- `docs/getting-started.mdx`, `docs/` user-facing pages — remove `--continue` references.

## Design Decisions

### 2026-05-16: Why this PRD exists, why now

PRD #76 surfaced the gap when a user asked: "If I `connect --continue` and the daemon is running, will it ignore the flag? And if the daemon is dead?" The honest answer was *neither plain `connect` nor `connect --continue` does what users want after a daemon crash*, because the snapshot only refreshes at clean quit. Rather than patch `--continue` to fit remote semantics (a moving target), unify the model: auto-restore on every startup, snapshot stays fresh continuously, `--continue` becomes vestigial and goes away.

### 2026-05-16: Daemon-side registry persistence rejected as scope

Considered "make the daemon's registry survive its own crash" as a complementary mechanism. Rejected because the agent PTY processes themselves die with the daemon (they're its child processes), so the registry surviving without the PTYs is metadata about nothing. The TUI-side snapshot-fallback path already covers crash recovery by re-spawning agents from saved structure, and each agent's own conversation state lives in its own state dir. So daemon-side persistence adds zero user-visible benefit.

### 2026-05-16: Breaking change is the right call

Removing `--continue` and making restore the default flips the meaning of plain `dot-agent-deck` from "empty session" to "restore last setup." This is a deliberate breaking change. Justification: restoring is the common case; starting fresh is rare. The current CLI optimizes for the rare case. New users are better served by the new default; existing users get a one-line changelog migration. Worth it.

### Open: shape of the local "fresh start" command

The remote case is clear: `dot-agent-deck remove <name>` already removes the deck and (per M4.1) clears its snapshot. The local case needs an analogous action. Options: `dot-agent-deck reset`, `dot-agent-deck --reset`, `dot-agent-deck snapshot clear`, or a TUI affordance ("Quit and clear saved state" in the quit-confirm dialog). Decide during M4.2 implementation.
