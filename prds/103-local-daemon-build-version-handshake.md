# PRD #103: Local daemon build-version handshake + `daemon stop` CLI

**Status**: Planning
**Priority**: High
**Created**: 2026-05-22
**GitHub Issue**: [#103](https://github.com/vfarcic/dot-agent-deck/issues/103)
**Parent**: PRD #93 (always-external daemon) — this PRD closes the upgrade-path gap that #93 introduced.
**Related**: PRD #76 M2.21 (existing `Hello` / `server_version` handshake on the `connect` strict path); PRD #90 (remote daemon upgrade flow — the remote-side analogue of this work).

## Problem Statement

After v0.25.0 (PRD #93) shipped, a real user hit the following sequence on 2026-05-22:

1. The user upgraded the `dot-agent-deck` binary on their workstation.
2. A daemon spawned by the *previous* build was still running (the daemon stays up while managed agents are alive, by design — PRD #93 line 38).
3. The user launched the upgraded TUI. Per PRD #93, the local TUI always attaches to an external daemon; if a socket is already present and trusted (correct mode/uid), `ensure_external_daemon_or_die` connects to it.
4. The TUI attached to the stale daemon. Delegate signals (`StartAgent`, role assignments) reached the daemon over the wire and were accepted at the protocol layer.
5. Inside the daemon, `handle_delegate` and the `pane_role_map` plumbing predated the rounds-9-12 schema changes — concretely:
   - `pane_orchestration_map` value type changed from `String` to `(String, String)`.
   - `TabMembership::Orchestration` gained an `orchestration_cwd` field.
   - Other role-map lookups gained or shifted keys that the stale daemon does not produce.
6. The role-map lookup silently no-op'd. The user saw delegate prompts arrive in the TUI as if they had been queued, but the orchestration pipeline never moved.

The proximate cause is **two safeguards that look adjacent but do not compose**:

- **`PROTOCOL_VERSION`** (`src/daemon_protocol.rs:121`) gates wire compatibility. It is still `1`, and *should* be — the JSON shape of every frame is wire-compatible across rounds 9-12. Bumping it for every internal refactor would force every user to restart their daemon on every release, defeating the purpose of a stable wire.
- **The existing `Hello` handshake** (PRD #76 M2.21) exists on the `connect` strict path for ssh-hop version skew detection. It only carries `server_version: u32` (PROTOCOL_VERSION). It is not used by the *local* attach path at all.

So today: `PROTOCOL_VERSION` alone is the wrong knob (it cannot distinguish "compatible wire, divergent semantics"), and the local attach has no equivalent of the remote pre-flight version check. Remote (`dot-agent-deck connect`) was unaffected because each ssh-connect spawns a fresh daemon at the *remote*'s installed binary version (PRD #93 — the remote daemon lifecycle is per-connection), so a stale-daemon-on-disk case cannot arise there.

PRD #93's line 39 promised "an equivalent local command" to `dot-agent-deck remote stop` for shutting down the local daemon. That command does not yet exist. Today the only documented recovery is `kill -9` on the daemon PID — which the user has to discover by `pgrep`, which loses any agent state without warning, and which we cannot recommend in docs as a routine step.

## Solution Overview

Close the local upgrade-path gap on two axes, both small, both pure-additive:

1. **Build-version handshake on local attach.** Extend `AttachRequest::Hello` / `AttachResponse` to carry `build_version: String` (the daemon's compiled-in `env!("DAD_VERSION")`) alongside the existing `server_version: u32` (PROTOCOL_VERSION). After `ensure_external_daemon_or_die` succeeds, the TUI sends `Hello`, compares the daemon's `build_version` to its own, and on mismatch prints a clear stderr error pointing at the new recovery command and exits non-zero. PROTOCOL_VERSION semantics are unchanged (wire compat); the new field captures *handler-code identity*, which is the actual axis of failure.

2. **`dot-agent-deck daemon stop` CLI.** A documented, non-`kill -9` way to recycle the local daemon. Reads the daemon PID (from the existing pidfile or by querying the attach socket), sends `SIGTERM`, waits for graceful shutdown, falls back to `SIGKILL` after a short grace period only with `--force`. Refuses without `--force` when managed agents are still alive (data-loss guard — the agents would lose their PTYs on daemon exit). Optionally add `daemon restart` as a thin wrapper (`stop` then let the next TUI invocation lazy-spawn).

Together these mean: the upgrade-path race becomes detectable (TUI tells the user "this daemon is build 0.24.x, you are 0.25.0 — run `dot-agent-deck daemon stop` and retry") and recoverable (the user has a documented command instead of `kill -9`).

## Scope

### In Scope

- **Add `build_version: Option<String>` to `AttachResponse`.** Optional so older daemons (which do not populate it) deserialize cleanly; the TUI treats `None` as "incompatible — daemon predates this check, ask the user to recycle it".
- **Extend `AttachRequest::Hello`** to optionally carry the client's `build_version: Option<String>` too. Symmetric to `server_version` / `client_version`. Daemon does not enforce on `client_version` (matching the existing PRD #76 pattern — only the client decides).
- **TUI-side check in `run_tui_session`** (`src/main.rs`): after `ensure_external_daemon_or_die`, open the attach socket, send `Hello { client_version: PROTOCOL_VERSION, client_build_version: Some(env!("DAD_VERSION").into()) }`, parse the response, compare `build_version`. Mismatch → write a clear error to stderr that names the local TUI version, the daemon version, and the recovery command (`dot-agent-deck daemon stop`), then exit non-zero.
- **New CLI subcommand `daemon stop`**: sends `SIGTERM` to the daemon (PID resolved from pidfile under `state_dir()`; fall back to querying the attach socket for the daemon's PID if pidfile is absent). Waits up to ~5s for the daemon to drop the socket. On `--force`, follows with `SIGKILL`.
- **Data-loss guard**: before sending `SIGTERM`, query the daemon's `ListAgents` over the attach socket. If any agent is still alive, refuse and instruct the user to either detach the agents first or pass `--force`.
- **New CLI subcommand `daemon restart`** (thin wrapper): `daemon stop` followed by a no-op return (next TUI invocation lazy-spawns per PRD #93). Same `--force` semantics.
- **Tests**:
  - Unit: `AttachResponse::hello` populates `build_version` from the daemon's compiled-in value; serde round-trip preserves the new field; older-shape JSON (no `build_version`) deserializes to `None`.
  - Integration: spawn a real daemon with a fake `DAD_VERSION` override, run the TUI attach path, assert it exits non-zero with the expected message on mismatch and proceeds on match.
  - Integration: `daemon stop` against a daemon with no agents → exits 0, socket gone within 5s. Against a daemon with a live managed agent → refuses without `--force`, succeeds with `--force`.
- **Docs**: update the relevant docs page (`docs/installation.md` or wherever the daemon lifecycle is documented per PRD #93's M for docs) to (a) tell users to run `daemon stop` after upgrading the binary if a daemon is still running, and (b) document the `daemon stop`/`daemon restart` commands. Add a Troubleshooting entry: "delegate prompts silently no-op after an upgrade" → recovery via `daemon stop`.
- **Changelog fragment** via `dot-ai-changelog-fragment`. Frame as a bug fix that closes a sharp edge in PRD #93's rollout.

### Out of Scope

- **Auto-killing a daemon hosting live agents.** The whole point of keeping the daemon up across TUI detaches is that agents survive. A version-mismatch check that silently terminates a daemon would cause exactly the silent data loss we are trying to prevent. The user is told to `daemon stop --force` themselves, or detach agents first.
- **Cross-user daemon sharing.** Per PRD #93, daemons are per-user. The handshake compares the calling user's daemon to the calling user's TUI; we do not consider scenarios where multiple users share a daemon.
- **Windows.** Per PRD #93's out-of-scope; Unix sockets / `SIGTERM` semantics assumed throughout.
- **Auto-upgrade / auto-restart of the daemon when a mismatch is detected.** Tempting but the same data-loss objection applies: we cannot assume the user is willing to lose state. The fix is detect + report + provide a recovery command, not silently bounce the daemon.
- **Bumping `PROTOCOL_VERSION`.** The wire shape is genuinely unchanged across rounds 9-12 — bumping it would be lying about wire compat. The whole reason we are adding `build_version` is precisely so we do not have to overload `PROTOCOL_VERSION`.
- **Cross-version compatibility shims.** No attempt to make a v0.24.x daemon work with a v0.25.0 TUI. The handshake is detect-and-refuse, not negotiate-and-adapt.
- **A general "rolling upgrade" / hot-reload story for the daemon.** That is a much larger design problem; this PRD only closes the cliff-edge case.

## Success Criteria

- A TUI compiled at version A attaching to a daemon at version B (A ≠ B) prints a clear stderr error naming both versions and the recovery command, and exits non-zero. Verified by an integration test.
- A TUI at version A attaching to a daemon at version A proceeds without user-visible change (modulo a single extra round-trip on startup). Verified by integration test.
- `dot-agent-deck daemon stop` on a daemon with no managed agents shuts it down and the socket file is gone, in under 5s. Verified by integration test.
- `dot-agent-deck daemon stop` on a daemon with a live managed agent refuses with a clear message and exits non-zero. `--force` bypasses the guard. Verified by integration test.
- Documentation explains the upgrade flow ("after upgrading, run `daemon stop` if a previous daemon is still running") and the new commands. A user following only the docs can recover from the original bug.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass. `cargo test` passes.
- Manual smoke: upgrade the binary while a daemon is running, launch the TUI, observe the mismatch error; run `daemon stop`; relaunch the TUI; observe normal startup.

## Milestones

### Phase 1: Wire-level changes

- [ ] **M1.1** — Extend `AttachResponse` with `build_version: Option<String>` (`src/daemon_protocol.rs:323`). Update `AttachResponse::hello` (`:395`) to populate it from the daemon's compiled-in `env!("DAD_VERSION")`. Add a serde round-trip test asserting backward-compat deserialization (older JSON with no `build_version` → `None`).
- [ ] **M1.2** — Extend `AttachRequest::Hello` (`src/daemon_protocol.rs:307`) with `client_build_version: Option<String>`. Daemon-side handler in the `Hello` arm (`:777`) reads it for logging only — does not reject on client value (mirrors PRD #76 M2.21 server-policy: only client decides).
- [ ] **M1.3** — Update `daemon hello` CLI subcommand (`src/main.rs:777`, the `cmd_daemon_hello` function) to also emit `build_version` in its static-print response so the `connect` strict path can pick up build-version skew across ssh too. (Bonus: makes PRD #90 trivially aware of build skew.)

### Phase 2: TUI local-attach check

- [ ] **M2.1** — In `run_tui_session` (`src/main.rs`), after `ensure_external_daemon_or_die` succeeds, open a fresh client connection and send `Hello`. Parse `AttachResponse`. If `build_version` is `None` (older daemon) or differs from `env!("DAD_VERSION")`, write a clear stderr message and exit non-zero. Message format:
  ```
  error: local daemon is build X.Y.Z but this TUI is build A.B.C
  recover with: dot-agent-deck daemon stop  (or `--force` if you don't mind losing in-flight agents)
  ```
- [ ] **M2.2** — Make the check `RUST_LOG=debug`-traceable: log the round-trip on success too, so users debugging unrelated startup issues can see the handshake fired. Keep the success path silent on stderr.
- [ ] **M2.3** — Decide and document: what happens if the daemon was just spawned by `ensure_external_daemon_or_die` itself (i.e. it's necessarily our build)? Either skip the check (small optimization) or run it anyway (defense in depth, catches bugs in `ensure_external_daemon_or_die`). Recommend running it anyway — the cost is one extra round-trip on cold start, the upside is a smoke test of the handshake on every launch.

### Phase 3: `daemon stop` and `daemon restart` CLI

- [ ] **M3.1** — Add `DaemonCmd::Stop { force: bool }` and `DaemonCmd::Restart { force: bool }` variants in `src/main.rs:135`. Wire them through the existing `Daemon` subcommand dispatcher.
- [ ] **M3.2** — Implement `daemon stop`: resolve daemon PID (prefer pidfile under `state_dir()` if present; otherwise query the attach socket for it — add a `GetDaemonInfo` request type if not already present). Query `ListAgents`. If any are alive and `!force`, print a clear refusal message naming the live agent IDs and exit non-zero. Otherwise send `SIGTERM`; poll the socket file disappearance every 100ms up to 5s; on timeout with `--force`, send `SIGKILL`; on timeout without `--force`, report the daemon did not exit cleanly and exit non-zero.
- [ ] **M3.3** — Implement `daemon restart`: just `daemon stop` with the same flags, then return. Lazy-spawn on next TUI invocation per PRD #93.
- [ ] **M3.4** — If no `GetDaemonInfo` request type exists in `daemon_protocol.rs`, add one that returns the daemon's PID and `build_version`. Reuse the existing socket; do not invent a new transport.

### Phase 4: Tests

- [ ] **M4.1** — Unit tests in `daemon_protocol.rs`: `AttachResponse::hello` populates both fields; serde round-trip preserves them; older-shape JSON (no `build_version`) deserializes to `None`.
- [ ] **M4.2** — Integration test: spawn a daemon, attach with a TUI built against a synthetic `DAD_VERSION` (use a test helper that overrides the comparison value rather than rebuilding the binary). Assert mismatch → exit-non-zero + expected stderr message; match → normal startup.
- [ ] **M4.3** — Integration test for `daemon stop`: spawn daemon with no agents → `daemon stop` succeeds, socket gone within 5s. Spawn daemon, start an agent, attempt `daemon stop` → refuses with informative error. Same scenario with `--force` → succeeds, agent dies.
- [ ] **M4.4** — Integration test for `daemon restart`: spawn daemon, run `daemon restart`, confirm the original daemon PID is gone and the next TUI lazy-spawn produces a fresh daemon at the current build.

### Phase 5: Docs and release

- [ ] **M5.1** — Update the daemon-lifecycle docs page (introduced by PRD #93) to describe the upgrade workflow and the new commands. Add a Troubleshooting entry for "delegate prompts silently no-op after an upgrade".
- [ ] **M5.2** — Changelog fragment via `dot-ai-changelog-fragment`. Bug-fix framing: "fix: local TUI now detects version skew against a stale daemon and exits cleanly; add `dot-agent-deck daemon stop`/`restart`".
- [ ] **M5.3** — PR, review, audit, merge, close.

## Key Files

- `src/daemon_protocol.rs` — `AttachRequest::Hello` (`:307`), `AttachResponse` (`:323`), `AttachResponse::hello` (`:395`), `PROTOCOL_VERSION` (`:121`), the `Hello` handler arm (`:777`). New field additions and round-trip tests live here.
- `src/daemon_attach.rs` — `ensure_external_daemon_or_die` (`:393`). The version check follows immediately after, but lives in `main.rs` rather than here so the check sees `env!("DAD_VERSION")` from the binary crate.
- `src/main.rs` — `run_tui_session` (around `:583` where `ensure_external_daemon_or_die` is called); `DaemonCmd` enum (`:135`); `cmd_daemon_hello` (`:777`); new `cmd_daemon_stop` / `cmd_daemon_restart`.
- `src/config.rs` — `state_dir()` for the pidfile path (if we introduce one) or for locating the existing daemon-info channel.
- `prds/93-always-external-daemon.md` — parent PRD. Line 39 references the promised "equivalent local command".
- `docs/installation.md` and/or `docs/getting-started.mdx` — daemon-lifecycle docs introduced by PRD #93; this PRD extends them.

## Risks and Mitigations

- **Risk**: An older daemon's `Hello` response is `None` for `build_version` and we mis-classify a *deliberately* compatible older daemon as incompatible.
  - *Mitigation*: We are explicitly treating `None` as "incompatible — recycle the daemon". The case where an older daemon happens to be wire-compatible *and* semantically compatible is impossible to detect without out-of-band knowledge, so erring on the side of refuse-and-explain is correct. The user can `daemon stop` cheaply.
- **Risk**: The check adds startup latency on every TUI launch.
  - *Mitigation*: One extra Unix-socket round-trip is negligible (microseconds locally). Verified in M2.2 by `RUST_LOG=debug` traces.
- **Risk**: `daemon stop` races with a TUI that is concurrently attaching (the TUI just spawned a daemon; another shell runs `daemon stop` before the TUI sends `Hello`).
  - *Mitigation*: Document that `daemon stop` is a user-initiated recovery, not a routine. The race produces a clean error in the TUI ("connection refused" or socket disappearance) rather than corruption. No need to coordinate.
- **Risk**: PID resolution is fragile if the daemon was not started by us (e.g. `systemctl --user` unit, or a developer running `cargo run -- daemon serve` directly).
  - *Mitigation*: Query the attach socket for the daemon's PID as a fallback (M3.4's `GetDaemonInfo`). The socket is the source of truth — if we can talk to a daemon, we can ask its PID.
- **Risk**: The recovery command name (`daemon stop`) is taken by a future use case (e.g. stopping a *remote* daemon).
  - *Mitigation*: `dot-agent-deck remote stop` already exists for remotes; `dot-agent-deck daemon stop` is the local equivalent. The split is symmetric with `remote add` vs. (future) `daemon ...` and consistent with PRD #93's line 39.
- **Risk**: Bumping `PROTOCOL_VERSION` retroactively for rounds 9-12 might be a more honest fix.
  - *Mitigation*: It would force every user to manually recycle their daemon on every release whose only change is internal refactors — a regression in user experience. The `build_version` field is the targeted fix; `PROTOCOL_VERSION` stays meaningful for actual wire-shape changes.

## Open Questions

- Should `daemon stop` emit a JSON-formatted summary (for scripting) in addition to human text? Defer to M3.2 — start with human text, add `--json` if a real consumer appears.
- Should the version-mismatch error mention `--force` of `daemon stop`, or only the safe form? Recommendation: only the safe form. `--force` is documented in the command's `--help`; the error message should not encourage data loss as a first resort.
- Should a daemon detect its *own* `env!("DAD_VERSION")` mismatching the binary on disk (e.g. by reading `/proc/self/exe`)? Out of scope for this PRD — the laptop-side check is the simpler, sufficient fix for the observed bug. Revisit only if real cases arise where the daemon process itself outlives a binary swap *and* a new TUI does not catch it via this handshake.
