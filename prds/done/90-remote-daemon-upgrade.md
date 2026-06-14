# PRD #90: Remote daemon upgrade flow

**Status**: No Longer Needed (superseded by PRD #76 + PRD #103)
**Priority**: Medium
**Created**: 2026-05-17
**Last Updated**: 2026-06-14
**Closed**: 2026-06-14
**GitHub Issue**: [#90](https://github.com/vfarcic/dot-agent-deck/issues/90)
**Depends on**: PRD #76 (Remote Agent Environments) shipping. Tightly related to PRD #91 (Hook freshness check).

## Closure Note (2026-06-14)

Closed as **No Longer Needed** — the core deliverables shipped under other PRDs:

- **`remote upgrade` command exists** — `RemoteCmd::Upgrade` → `remote::upgrade()` (`src/remote.rs:1186`): downloads the matching release artifact, atomically installs it on the remote (tempfile + rename), and re-runs `hooks install`.
- **Client/daemon build-version handshake shipped via PRD #103** — `src/build_version_handshake.rs` (`ensure_compatible_daemon_or_die`); `AttachRequest::Hello`/`AttachResponse` carry build versions. On mismatch it SIGTERMs the daemon so the next attach lazy-spawns a fresh one — covering the "daemon version compatibility" problem this PRD raised.
- **Graceful `daemon stop`/`restart` shipped via PRD #103** — `src/daemon_stop.rs` (SIGTERM + poll + optional SIGKILL via peer-PID). The protocol-level `AttachRequest::DaemonStop` this PRD proposed was deliberately **not** built; #103 chose a better OS peer-credential approach instead.

The remaining gaps are minor and below the bar for a standing PRD: an explicit graceful-stop-before-binary-swap (today the upgrade relies on #103's handshake to tear down a mismatched daemon on next attach), a `--from-local` flag (push the laptop binary instead of downloading a release), and a "same version → no-op" short-circuit. If wanted, file these as a single small issue rather than tracking them here.

## Problem Statement

`dot-agent-deck remote add` (`src/remote.rs`) is a one-shot install: it downloads the binary from GitHub releases to `~/.local/bin/dot-agent-deck` on the remote, runs `dot-agent-deck hooks install` there, and that's it. There is no first-class flow for upgrading an existing remote install.

Concrete pain points as of today:

1. **No `remote upgrade` command.** Users who need a newer version on the remote have to manually scp or rebuild + copy the binary, kill the running daemon, and let the next `connect` lazy-spawn the new one. This was verified end-to-end by the project maintainer during PRD #76 testing — the manual flow works but is undocumented and unergonomic.
2. **No version compatibility check.** A laptop running version X talking to a daemon running version Y has no protocol-level signal that they're mismatched; the connect succeeds but features added in X may silently misbehave on Y (or vice versa).
3. **Hooks don't re-install on binary swap.** Even when the user manually upgrades the binary, the registered hook commands in `~/.claude/settings.json` keep pointing at the old binary path (still correct if the new binary is at the same location), but any *new* hook types added in the upgrade are not registered until `hooks install` is run again. Today nothing prompts the user to do that. PRD #91 covers the detection side; this PRD covers the upgrade-time invocation.
4. **No graceful daemon stop.** Even a future `remote upgrade` command needs a clean way to stop the running daemon — currently the only API is "kill the process and let lazy-spawn pick up the new binary," which means in-flight attach connections drop ungracefully.

## Solution Overview

Add a `dot-agent-deck remote upgrade [name]` command that:

1. Resolves the remote target from the registered name (reusing `src/remote.rs` plumbing).
2. Detects the laptop-side binary version and the remote-side binary version. If equal, prints a "nothing to do" message and exits 0.
3. Downloads the matching release artifact (or transfers the laptop-side binary if running a dev build, with explicit `--from-local` flag).
4. Connects to the remote daemon's attach socket and issues a graceful-stop request (new protocol op, e.g. `AttachRequest::DaemonStop` — see Design Decisions for protocol choice).
5. Waits for the daemon to exit cleanly. Falls back to SIGTERM via ssh if the protocol-level stop doesn't return within a timeout.
6. Atomically swaps the binary at `~/.local/bin/dot-agent-deck` on the remote (download to a sibling tempfile + `rename`).
7. Re-runs `dot-agent-deck hooks install` on the remote so any new hook types added in the upgrade get registered.
8. Exits. The next `connect` invocation lazy-spawns the new daemon. No automatic restart — the daemon is lazy-spawned on demand anyway.

The graceful-stop protocol op also unblocks PRD #76's M2.10 lifecycle verification (no more "ssh and kill the process" out-of-band step) and any future "rotate the daemon" workflow.

## Scope

### In Scope

- New `dot-agent-deck remote upgrade [name]` CLI subcommand.
- New attach protocol op: graceful daemon stop (`AttachRequest::DaemonStop`). Includes wire-format addition with `#[serde(default, skip_serializing_if = "Option::is_none")]` rules where applicable for backward compat. Daemon-side handler: drain attach connections, close the hook listener, persist anything that needs persisting, exit cleanly.
- Version check that compares the laptop-side `CARGO_PKG_VERSION` (or `--version` output) against a new daemon-side endpoint (could piggyback on an existing op — see Design Decisions).
- Binary transfer over ssh (reuse the existing download-from-releases path in `src/remote.rs`; add a `--from-local` flag that scp's the laptop's own binary up instead, for dev/test).
- Atomic on-remote swap (tempfile + rename in the same directory).
- Re-run `hooks install` on the remote after swap.
- Tests: unit tests for the new protocol op (round-trip, error cases), integration test that spins up a daemon, sends `DaemonStop`, verifies clean exit, lazy-spawns a fresh one.
- Documentation updates (handed off to PRD #87's remote-environments docs once this lands — note the new command and where it fits in the lifecycle).

### Out of Scope

- The upgrade flow for **PRD #81's Kubernetes transport** — that PRD owns its own PVC-preserving upgrade. This PRD covers SSH-based remotes only.
- Automated upgrade scheduling / "always upgrade on connect" auto-magic. Upgrades are explicit user actions.
- Downgrades. The same mechanics work, but we don't actively test/support going backward; user can re-run `remote upgrade --version <older>` if needed but no compatibility guarantees.
- Multi-binary or multi-version coexistence on the same remote. One binary path, one version.
- Bundling the hook-freshness *detection* (that's PRD #91 — this PRD only re-runs `hooks install` at upgrade time as a side effect).
- Rolling upgrade across multiple registered remotes (`remote upgrade --all`). Out of scope here; one remote at a time.

## Success Criteria

- `dot-agent-deck remote upgrade <name>` upgrades the remote binary end-to-end without manual ssh.
- If the laptop and remote are already at the same version, the command is a no-op and prints a clear message.
- An attached TUI sees a graceful `KIND_STREAM_END` (or equivalent) when the daemon stops, not a torn-down socket — the user is told "daemon stopping for upgrade" rather than seeing the dashboard freeze.
- After upgrade, `connect` works without further user action; the next session uses the new daemon.
- After upgrade, `dot-agent-deck hooks install` has been re-run automatically; any newly-added hook types are registered.
- All three gates pass: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## Milestones

### Phase 1: Graceful daemon stop

- [ ] **M1.1** — Add `AttachRequest::DaemonStop` protocol variant (with backward-compat serde annotations) and a daemon-side handler that drains attach connections, closes both listeners (hook + attach), and exits cleanly.
- [ ] **M1.2** — Surface a "daemon shutting down" signal to attached TUIs (new `KIND_STREAM_END` reason, or a new frame kind for soft-disconnect). TUI displays a friendly message.
- [ ] **M1.3** — Tests for the protocol op (round-trip, in-flight attach behavior, exit code).

### Phase 2: Version check

- [ ] **M2.1** — Add a `ProtocolVersion` / `BinaryVersion` exchange to the attach protocol (could piggyback on the first response after attach, or add a new explicit op). Wire-format-compatible default for older clients.
- [ ] **M2.2** — `remote upgrade` reads the local binary version (`env!("CARGO_PKG_VERSION")`) and the remote's reported version, compares, decides whether to proceed.

### Phase 3: Binary swap

- [ ] **M3.1** — `remote upgrade [name]` CLI: resolve registered remote, version check, download artifact (reuse existing release-download code from `src/remote.rs`), `scp` to a tempfile in `~/.local/bin/`, `rename` atomically.
- [ ] **M3.2** — `--from-local` flag: instead of downloading, `scp` the laptop's own binary up. For dev/test loops.
- [ ] **M3.3** — Issue `DaemonStop` before the swap; wait for the daemon process to exit (poll via `ssh ... pgrep` with a bounded timeout); fall back to SIGTERM if the protocol stop didn't take.
- [ ] **M3.4** — Re-run `dot-agent-deck hooks install` on the remote after swap.

### Phase 4: Tests + docs

- [ ] **M4.1** — Integration test: spin up daemon, run `DaemonStop` via protocol, assert clean exit + zero zombie processes.
- [ ] **M4.2** — Integration test (best-effort, may need a CI runner with two hosts): exercise `remote upgrade` end-to-end against a local ssh loopback target.
- [ ] **M4.3** — Documentation hand-off to PRD #87: note the new command, when to use it, and the relationship to `hooks install`.

## Key Files

- `src/remote.rs` — current SSH-based install lives here; new `upgrade` flow extends the same module.
- `src/main.rs` — CLI parsing; new `remote upgrade` subcommand.
- `src/daemon_protocol.rs` — new `AttachRequest::DaemonStop` variant + frame-level signal for graceful shutdown.
- `src/daemon.rs` — graceful-shutdown handler that drains connections and exits.
- `src/embedded_pane.rs` / `src/daemon_client.rs` — TUI surfaces the shutdown signal.
- `src/hooks_manage.rs` — re-run from the upgrade flow.

## Design Decisions

### 2026-05-17: Separate from PRD #91 (hook freshness)

PRD #91 (hook freshness check at TUI startup) is intentionally split out because the value of detecting stale hooks is independent of how the binary got there. A user who builds locally and copies the binary manually (the path the maintainer just exercised end-to-end) won't go through `remote upgrade`, but they should still get a hook-freshness warning. PRD #91 lands the detection; this PRD invokes `hooks install` at upgrade time as a side effect. The two PRDs can land in either order.

### 2026-05-17: Lazy-spawn vs explicit restart

After the binary swap, this PRD does *not* automatically restart the daemon. Lazy-spawn (M2.8) already handles the next `connect` cleanly. Restarting eagerly would either (a) keep an idle daemon around with no attached TUI, or (b) require an extra round-trip to provision a fresh state directory. Lazy-spawn is simpler and matches PRD #76's overall posture.
