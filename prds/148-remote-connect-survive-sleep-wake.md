# PRD #148: Remote connect survives laptop sleep/wake

**Status**: In Progress — implementation, tests, and docs complete (M1–M3, M5); pending real-remote sleep/wake verification (M4)
**Priority**: High
**Created**: 2026-06-12
**GitHub Issue**: [#148](https://github.com/vfarcic/dot-agent-deck/issues/148)
**Related**: `src/connect.rs` (`build_connect_command`, `run_connect`, `ConnectSpawner`, `exit_code_from_status`), `src/remote.rs:243-251` (existing `ServerAliveInterval`/`ServerAliveCountMax` probe pattern; `:291` exit-255 transport-error contract), [#89](https://github.com/vfarcic/dot-agent-deck/issues/89) (auto-restore TUI state), [#93](https://github.com/vfarcic/dot-agent-deck/issues/93) (always-external daemon — why remote agent state survives a reconnect), [#81](https://github.com/vfarcic/dot-agent-deck/issues/81) (remote Kubernetes transport — explicitly out of scope here)

## Problem Statement

Connecting to a remote daemon runs the deck TUI **on the remote** and hands the user's terminal over an `ssh -t` session (`build_connect_command`, `src/connect.rs:631`). The local process simply blocks on that ssh child until it exits.

The live session is built with **only `ConnectTimeout`** and no keepalive. The code states the design intent explicitly at `src/connect.rs:634`:

```rust
// ConnectTimeout (separate from session-runtime — once the session is up,
// ssh keeps it alive indefinitely). ...
cmd.arg("-o").arg(format!("ConnectTimeout={}", probe_timeout_secs()));
```

When the laptop sleeps, the TCP connection dies silently — the sleeping endpoint never exchanges a FIN/RST. On wake, the ssh client is parked on a **dead socket it cannot tell is dead**, with no keepalive probing to discover it. ssh never times out and never exits, so:

1. **The TUI is frozen** — keystrokes go into a dead socket, reads never return.
2. **The only escape is killing the terminal tab** and starting a fresh `connect`.

This is the single most common way a long-lived remote session breaks, and the failure mode (silent freeze) is the worst possible one.

The fix pattern already exists in the codebase: the version-probe path (`src/remote.rs:243-251`) sets `ConnectTimeout` + `ServerAliveInterval` + `ServerAliveCountMax=1` precisely to "catch transport-level stalls ... dead-TCP keepalives." It was deliberately left off the live connect session. And `src/remote.rs:291` documents that **ssh uses exit code 255 for its own transport/auth errors** — a clean signal we can use to distinguish a dropped connection from a clean user quit.

## Solution

Two layers, building on contracts the codebase already relies on.

### Layer 1 — Keepalive on the live session (detect & drop)

Add SSH keepalive to `build_connect_command` so a dead connection is detected and the session terminates instead of hanging:

```rust
// Once the session is up, ssh otherwise keeps it alive indefinitely with no
// liveness probing — so a connection killed by laptop sleep is never noticed
// and the TUI freezes. ServerAlive* makes ssh probe the peer over the
// encrypted channel (works through NAT/firewalls, unlike TCPKeepAlive) and
// abort after ServerAliveCountMax consecutive unanswered probes.
cmd.arg("-o").arg(format!("ServerAliveInterval={LIVE_KEEPALIVE_INTERVAL_SECS}"));
cmd.arg("-o").arg(format!("ServerAliveCountMax={LIVE_KEEPALIVE_COUNT_MAX}"));
```

Proposed defaults: `ServerAliveInterval=15`, `ServerAliveCountMax=3` → a dead connection is dropped within ~45s of wake. Unlike the probe path (`ServerAliveCountMax=1`, which wants fail-fast), the live session uses a higher count so a brief real network blip doesn't tear down an active session.

On its own, Layer 1 already kills the "frozen forever" symptom: on wake, ssh aborts within ~45s, the local process unblocks, and the user can reconnect. Layer 2 makes that reconnect automatic.

### Layer 2 — Auto-reconnect on transport drop (seamless resume)

Today `run_connect` (`src/connect.rs:751`) does: probe → `spawn` (blocks) → bookkeeping → return ssh's exit code. Wrap the spawn/bookkeeping in a reconnect loop keyed on **exit 255**:

```text
loop:
  probe (version + protocol)        // doubles as the reachability gate
  exit = spawner.spawn(...)         // blocks until ssh exits
  if exit == 255:                   // ssh transport failure (dropped/keepalive-timeout)
      print "connection to <name> lost — reconnecting…"
      backoff (bounded retries / wall-clock budget)
      continue                      // re-probe then re-spawn
  else:
      break                         // clean remote exit (0), Ctrl-C (130), TUI crash (non-255)
return exit
```

Why this is safe and correct:

- **Exit 255 means transport, not intent.** ssh passes a clean remote exit through verbatim (TUI quit → 0, Ctrl-C → 130, remote panic → its own non-255 code) and reserves 255 for its own connection/auth failures (`src/remote.rs:291`). So we reconnect *only* on a dropped transport, never on a user quitting or a crashing remote TUI (which would just crash again).
- **State survives.** The remote daemon is external and persistent (#93); the remote TUI restores its view from daemon state on startup (#89, #74). So a fresh `ssh -t` after a drop re-attaches to the *same* running agents — the user sees their session resume, not a blank slate.
- **The probe is the backoff gate.** Re-running the existing probe each attempt confirms the host is reachable again (laptop wifi may not be up yet) and that versions still match, reusing `ConnectTimeout`/keepalive to fail fast while still down.
- **Bounded, not infinite.** A retry budget (attempts and/or wall-clock) caps reconnection so a genuinely-gone remote surfaces an error and a sane exit code instead of an endless loop.

### Design decisions (from discussion)

- **Do both layers, not just keepalive.** Keepalive alone leaves the user re-typing `connect` after every sleep; auto-reconnect is what makes "reopen the laptop and it's just there" true. The user explicitly chose keepalive **+** auto-reconnect.
- **Keep keepalive tolerance higher on the live session than on the probe.** Probe wants `CountMax=1` (fail fast); a live interactive session wants `CountMax=3` so a 15–30s network hiccup doesn't kill a working session.
- **Reconnect only on 255.** Reuse ssh's existing exit-code contract rather than inventing new signaling. Any non-255 exit is treated as intentional/terminal.
- **Re-run the full probe on each reconnect.** Cheap, already implemented, and gives reachability + version/protocol safety for free instead of blindly re-spawning into an incompatible or absent remote.
- **Bounded retries with user-visible messaging.** No silent infinite loop; print a "reconnecting…"/"giving up" line to stderr between the handed-over terminal sessions.
- **Reset the local terminal on give-up.** If ssh dies mid-session the remote TUI never restored the terminal (raw mode / alt screen). A successful reconnect re-inits it; if we exhaust retries we must restore a sane terminal so the user isn't left with a garbled prompt.

## Acceptance Criteria

### Keepalive on live session
- [x] `build_connect_command` emits `-o ServerAliveInterval=<n>` and `-o ServerAliveCountMax=<m>` in addition to the existing `ConnectTimeout`. *(M1: `LIVE_KEEPALIVE_INTERVAL_SECS=15`, `LIVE_KEEPALIVE_COUNT_MAX=3`; arg-assertion test updated.)*
- [ ] After a real sleep/wake (or a simulated transport drop), the ssh session terminates within roughly `interval × countMax` seconds instead of hanging indefinitely. *(Runtime ssh behavior — pending M4 real-remote verification.)*
- [x] The probe path in `src/remote.rs` is unchanged (still `ServerAliveCountMax=1`). *(Confirmed untouched by reviewer + auditor.)*

### Auto-reconnect
- [x] When `spawn` returns 255, `run_connect` re-probes and re-spawns rather than returning; the user is shown a "reconnecting to `<name>`…" message between sessions. *(M2; test `reconnect_then_clean_exit_returns_zero`.)*
- [ ] On reconnect, the remote TUI re-attaches to the still-running agents (verified the session resumes, not a fresh empty dashboard). *(Real-remote behavior — pending M4 verification.)*
- [x] A clean remote exit (0), a Ctrl-C/signal exit (e.g. 130/143), and a non-255 remote-TUI crash all return immediately with that exit code — **no** reconnect. *(Tests `clean_exit_does_not_reconnect`, `ctrl_c_exit_is_terminal_no_reconnect`.)*
- [x] Reconnection is bounded by a retry/backoff budget; once exhausted, `run_connect` returns a clear error and restores a sane local terminal. *(`MAX_CONNECT_ATTEMPTS=5`; tests `repeated_transport_failure_is_bounded`, `reconnect_time_unreachable_is_bounded_not_immediate`; terminal restore on both give-up routes.)*
- [x] `last_connected` bookkeeping still updates only on a genuine clean exit (status 0), not on intermediate reconnects. *(Asserted in reconnect tests.)*

### Tests
- [x] Unit test: `build_connect_command` arg assertion updated to expect the new keepalive options.
- [x] Unit tests via the fake `ConnectSpawner` + fake `SshExecutor`: `[255, 0]` ⇒ exactly two spawns + one reconnect, returns 0; `[0]` ⇒ one spawn, no reconnect; `[130]` ⇒ one spawn, no reconnect; repeated `255` ⇒ bounded number of spawns then a terminal error (backoff sleep is injectable so tests don't actually wait). *(All present; plus reconnect-time probe-failure bounded-retry coverage.)*

## Milestones

- [x] **M1 — Keepalive on the live session.** Add `ServerAliveInterval`/`ServerAliveCountMax` to `build_connect_command` with named constants; update the existing arg-assertion unit test. (Layer 1 alone already removes the freeze.)
- [x] **M2 — Auto-reconnect loop.** Wrap spawn/bookkeeping in `run_connect` with the 255-keyed, probe-gated, bounded-backoff reconnect loop, including user-visible messaging and local-terminal restore on give-up. Make the backoff sleep injectable for tests. *(Review blocker found + fixed: reconnect-time `HostUnreachable` probe failures now fold into the same bounded retry/restore path via `is_reachability_error` + shared `on_transport_failure`.)*
- [x] **M3 — Tests.** Fake-spawner/fake-executor unit tests covering the reconnect state machine and exit-code routing (M2 acceptance list); confirm `cargo test-fast`, `cargo fmt --check`, `cargo clippy -- -D warnings` are green. *(`cargo test-fast` 708 passed; fmt clean; clippy 0 warnings.)*
- [ ] **M4 — Verified against a real remote.** Connect to a remote daemon, sleep/wake the laptop (or drop the network), and confirm the session is dropped promptly and auto-reconnects to the running agents without closing the tab. Run `cargo test-e2e` before the PR. *(`cargo test-e2e` 1026 passed; real-hardware sleep/wake manual check still pending — a user step.)*
- [x] **M5 — Docs.** Update the Remote Environments docs to note that sessions now survive sleep/wake and auto-reconnect, including the keepalive tuning knobs and the bounded-retry behavior. *(`docs/remote-environments.md`: new "Surviving sleep/wake" section + updated Stop-vs-detach row.)*

## Out of Scope

- **Mosh-style seamless TCP migration.** We accept a brief (~tens of seconds) reconnect, not zero-loss session migration. Plain ssh cannot survive a dead TCP flow; auto-reconnect over a persistent remote daemon is the pragmatic equivalent.
- **Other remote transports.** The remote Kubernetes transport (#81) is not implemented yet and is not covered here; if/when it lands it needs its own liveness/reconnect handling.
- **User-configurable keepalive/retry tuning surface.** Start with sensible constants; exposing them via config/flags is a later ergonomic improvement.
- **Local (unix-socket) daemon attach.** The embedded-pane I/O loop's own stall handling (`src/embedded_pane.rs`) is a separate concern from the SSH connect path and is unaffected by this PRD.

## Notes

- Root cause: the live `ssh -t` session set `ConnectTimeout` only and intentionally no keepalive ("once the session is up, ssh keeps it alive indefinitely"), so a connection killed by sleep is never detected and the TUI freezes.
- The fix reuses two existing contracts: the `ServerAlive*` pattern from the probe path (`src/remote.rs:243-251`) and ssh's exit-255-means-transport-error convention (`src/remote.rs:291`).
- Surfaced by a user report: "connected to a remote daemon, put my laptop to sleep, on wake the TUI is frozen and I have to close the whole tab and reconnect."
