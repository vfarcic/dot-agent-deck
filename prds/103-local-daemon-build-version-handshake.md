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

The proximate cause is **three safeguards that look adjacent but do not compose**:

- **`PROTOCOL_VERSION`** (`src/daemon_protocol.rs:121`) gates wire compatibility. It is still `1`, and *should* be — the JSON shape of every frame is wire-compatible across rounds 9-12. Bumping it for every internal refactor would force every user to restart their daemon on every release, defeating the purpose of a stable wire.
- **The existing `Hello` handshake** (PRD #76 M2.21) exists on the `connect` strict path for ssh-hop version skew detection. It only carries `server_version: u32` (PROTOCOL_VERSION). It is not used by the *local* attach path at all.
- **`DAD_VERSION`** (set by `build.rs` from `git describe --tags --abbrev=0`) is *tag-only* — it has no commit hash and no dirty-tree marker. So two binaries built at the same tag but different commits (or one with a dirty tree, common during local development) carry the same `DAD_VERSION` even though their handler code differs. This is exactly the case that hit us: the stale daemon and the new TUI were both nominally `0.25.0`-bracketed builds but at different commits across rounds 9-12. A naive `DAD_VERSION`-only handshake would *not* have caught it. The handshake field has to carry commit identity, not just tag identity.

So today: `PROTOCOL_VERSION` alone is the wrong knob (it cannot distinguish "compatible wire, divergent semantics"), `DAD_VERSION` alone is too coarse (same-tag-different-commit collisions), and the local attach has no equivalent of the remote pre-flight version check anyway. Remote (`dot-agent-deck connect`) was unaffected because each ssh-connect spawns a fresh daemon at the *remote*'s installed binary version (PRD #93 — the remote daemon lifecycle is per-connection), so a stale-daemon-on-disk case cannot arise there.

PRD #93's line 39 promised "an equivalent local command" to `dot-agent-deck remote stop` for shutting down the local daemon. That command does not yet exist. Today the only documented recovery is `kill -9` on the daemon PID — which the user has to discover by `pgrep`, which loses any agent state without warning, and which we cannot recommend in docs as a routine step.

## Solution Overview

Close the local upgrade-path gap on three axes, all small, all pure-additive:

1. **Sharper build identity from `build.rs`.** Introduce a new `DAD_BUILD_ID` env var alongside `DAD_VERSION`, of the form `<DAD_VERSION>-g<short-sha>[-dirty]` (e.g. `0.25.0-g243b049`, or `0.25.0-g243b049-dirty` when the working tree was dirty at build time). `DAD_VERSION` stays exactly as it is (user-facing version string for `--version` and the remote-install registry). `DAD_BUILD_ID` is the *new* axis used only by the handshake — it changes on every commit, even when the tag does not. This is what makes the handshake actually catch the bug we hit.

2. **Build-version handshake on local attach.** Extend `AttachRequest::Hello` / `AttachResponse` to carry `build_version: Option<String>` (the daemon's compiled-in `env!("DAD_BUILD_ID")`) alongside the existing `server_version: u32` (PROTOCOL_VERSION). After `ensure_external_daemon_or_die` succeeds, the TUI sends `Hello`, compares the daemon's `build_version` to its own, and on mismatch prints a clear stderr error pointing at the new recovery command and exits non-zero. PROTOCOL_VERSION semantics are unchanged (wire compat); the new field captures *handler-code identity*, which is the actual axis of failure. The same field is added to the existing remote pre-flight (`probe_remote_protocol` in `src/connect.rs`) so the remote-upgrade flow gains the same precision for free.

3. **`dot-agent-deck daemon stop` CLI.** A documented, non-`kill -9` way to recycle the local daemon. Resolves the daemon PID using **`SO_PEERCRED` / `LOCAL_PEERPID` on the connected attach socket** — an OS-level facility that works against *any* daemon version (no protocol exchange, no new wire variants). This is the load-bearing choice: the entire raison d'être of this command is recovering from a *stale* daemon that does not implement any new protocol surface we add, so PID discovery must not depend on the daemon's cooperation. Once we have the PID, send `SIGTERM`, wait for graceful shutdown, fall back to `SIGKILL` after a short grace period only with `--force`. Refuses without `--force` when managed agents are still alive (data-loss guard — the agents would lose their PTYs on daemon exit). Optionally add `daemon restart` as a thin wrapper (`stop` then let the next TUI invocation lazy-spawn).

Together these mean: the upgrade-path race becomes detectable (TUI tells the user "this daemon is build 0.25.0-g243b049 but you are 0.25.0-gabc1234 — run `dot-agent-deck daemon stop` and retry") and recoverable (the user has a documented command instead of `kill -9`).

## Scope

### In Scope

- **Extend `build.rs` to emit a new `DAD_BUILD_ID` env var** of the form `<DAD_VERSION>-g<short-sha>[-dirty]`. Use `git rev-parse --short HEAD` for the SHA and `git status --porcelain` (non-empty → dirty) for the dirty marker. Fall back to `<DAD_VERSION>-unknown` if git is unavailable (same fallback discipline `git_version()` already follows). **Rerun strategy**: `cargo:rerun-if-changed=.git/HEAD` (catches branch switches and detached-HEAD changes) is necessary but not sufficient — on a normal branch, `.git/HEAD` is the literal text `ref: refs/heads/<branch>` and never changes when commits land on that branch. Cargo's `rerun-if-changed` is also a *narrowing* directive: once any `rerun-if-changed` is emitted, the build script no longer re-runs on every build. So `build.rs` must additionally (a) read `.git/HEAD`, parse the ref path (e.g. `refs/heads/main`), and emit `cargo:rerun-if-changed=.git/refs/heads/main` for the resolved ref so new commits invalidate; (b) emit `cargo:rerun-if-changed=.git/index` so dirty/clean transitions invalidate; (c) emit `cargo:rerun-if-changed=.git/packed-refs` as a fallback for refs that live only in packed-refs after `git gc`. For detached HEAD, the HEAD file itself contains the SHA, so the existing `.git/HEAD` watch suffices. `DAD_VERSION` itself stays unchanged.
- **Add `build_version: Option<String>` to `AttachResponse`.** Optional so older daemons (which do not populate it) deserialize cleanly; the TUI treats `None` as "incompatible — daemon predates this check, ask the user to recycle it". The daemon populates it from `env!("DAD_BUILD_ID")`.
- **Extend `AttachRequest::Hello`** to optionally carry the client's `client_build_version: Option<String>` too. Symmetric to `server_version` / `client_version`. Daemon does not enforce on `client_build_version` (matching the existing PRD #76 pattern — only the client decides).
- **Wire-format contract for both new fields**: see "Wire format" subsection below — both fields are `Option<String>` with `#[serde(default, skip_serializing_if = "Option::is_none")]` so older peers (which omit them) round-trip cleanly. Non-negotiable for forward-compat.
- **TUI-side check in `run_tui_session`** (`src/main.rs`): after `ensure_external_daemon_or_die`, open the attach socket, send `Hello { client_version: PROTOCOL_VERSION, client_build_version: Some(env!("DAD_BUILD_ID").into()) }`, parse the response, compare `build_version`. Mismatch (including `None`) → write a clear error to stderr that names the local TUI build-id, the daemon build-id, and the recovery command (`dot-agent-deck daemon stop`), then exit non-zero.
- **Remote-side comparison in `probe_remote_protocol`** (`src/connect.rs`): the existing strict pre-flight already parses the remote daemon's `AttachResponse` from `daemon hello`. Extend it to compare `build_version` against the local `env!("DAD_BUILD_ID")` and surface a `ProtocolMismatch { remote, local, .. }`-style error on divergence. **Policy difference vs local**: remote-build skew is a *configuration* concern (the user can `dot-agent-deck remote upgrade` per PRD #90), not a stale-daemon concern, so the error must point at the *remote-upgrade* command, not at `daemon stop`. Local and remote share the field but route to different remediation.
- **PID discovery via socket peer credentials** (no protocol change). After opening the attach socket, read the peer credentials on the connected `UnixStream` to obtain the daemon's PID. Linux: `SO_PEERCRED` (`struct ucred { pid, uid, gid }`). macOS: `LOCAL_PEERPID` (returns `pid_t`) combined with `getpeereid` for uid/gid. Both are exposed by `std::os::unix::net::UnixStream::peer_cred()` (returns `UCred`); on macOS `UCred::pid()` returns `Some(pid)` on modern Rust stable, but in case of stale-toolchain concerns, drop to `nix::sys::socket::getsockopt::<PeerCredentials>` on Linux and a raw `getsockopt(socket, SOL_LOCAL, LOCAL_PEERPID, ...)` on macOS. Either implementation works against **any** daemon version, because it does not depend on the daemon's protocol handlers. No new `AttachRequest` variant is added; no `PROTOCOL_VERSION` bump is needed.
- **No pidfile** (see Out of Scope). The socket itself is the rendezvous; peer credentials are the source of truth.
- **New CLI subcommand `daemon stop`**: opens the attach socket; reads peer-cred PID; sends `ListAgents` (existing variant — supported by current daemons, including stale ones we need to recover from). If any agents are alive and not `--force`, refuses with a message naming the live agent IDs. Otherwise sends `SIGTERM`; polls socket-file disappearance every 100ms up to ~5s; on timeout with `--force`, sends `SIGKILL`; on timeout without `--force`, reports the daemon did not exit cleanly and exits non-zero. If the socket is missing, prints "no daemon running" and exits 0 (idempotent). **Stale-daemon coverage**: because PID discovery uses peer-cred and the agent-liveness check uses the already-existing `ListAgents` variant, `daemon stop` works against *any* daemon version including the stale daemon that motivated this PRD.
- **Data-loss guard**: enforced inline in the `daemon stop` flow above (the `ListAgents` check before `SIGTERM`). Documented separately for emphasis: a stale daemon that is *still* hosting live managed agents should not be killed silently; the user must either detach the agents first or pass `--force` consciously.
- **New CLI subcommand `daemon restart`** (thin wrapper): `daemon stop` followed by a no-op return (next TUI invocation lazy-spawns per PRD #93). Same `--force` semantics.
- **Tests**:
  - Unit: `AttachResponse::hello` populates `build_version` from the daemon's compiled-in value; serde round-trip preserves the new field; older-shape JSON (no `build_version`) deserializes to `None`; older-shape `Hello` JSON (no `client_build_version`) deserializes successfully on the daemon side.
  - Build-script unit (or smoke check via `cargo test --test build_id`): `DAD_BUILD_ID` of the form `<version>-g<sha>` exists; a dirty working tree produces a `-dirty` suffix.
  - Integration: spawn a real daemon with a fake `DAD_BUILD_ID` override (via a test helper that injects the comparison value rather than rebuilding the binary), run the TUI attach path, assert it exits non-zero with the expected message on mismatch and proceeds on match.
  - Integration: `daemon stop` against a daemon with no agents → exits 0, socket gone within 5s. Against a daemon with a live managed agent → refuses without `--force`, succeeds with `--force`.
  - Integration (or fake-ssh harness): `probe_remote_protocol` returns a build-mismatch error when the remote's `daemon hello` reports a different `build_version`, and the error message names `remote upgrade` (not `daemon stop`).
- **Docs**: update the relevant docs page (`docs/installation.md` or wherever the daemon lifecycle is documented per PRD #93's M for docs) to (a) tell users to run `daemon stop` after upgrading the binary if a daemon is still running, and (b) document the `daemon stop`/`daemon restart` commands. Add a Troubleshooting entry: "delegate prompts silently no-op after an upgrade" → recovery via `daemon stop`.
- **Changelog fragment** via `dot-ai-changelog-fragment`. Frame as a bug fix that closes a sharp edge in PRD #93's rollout.

### Wire format

Both new fields are added as `Option<String>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`. Concretely:

```rust
// src/daemon_protocol.rs
pub enum AttachRequest {
    // ...
    Hello {
        client_version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_build_version: Option<String>,
    },
}

pub struct AttachResponse {
    // ...existing fields...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_version: Option<String>,
}
```

`#[serde(default)]` ensures pre-existing peers (whose JSON omits these keys entirely) deserialize cleanly; `skip_serializing_if` ensures new peers do not emit the keys to older deserializers that might be strict about unknown fields. Round-trip tests cover both directions in M4.1.

### Out of Scope

- **Auto-killing a daemon hosting live agents.** The whole point of keeping the daemon up across TUI detaches is that agents survive. A version-mismatch check that silently terminates a daemon would cause exactly the silent data loss we are trying to prevent. The user is told to `daemon stop --force` themselves, or detach agents first.
- **Cross-user daemon sharing.** Per PRD #93, daemons are per-user. The handshake compares the calling user's daemon to the calling user's TUI; we do not consider scenarios where multiple users share a daemon.
- **Windows.** Per PRD #93's out-of-scope; Unix sockets / `SIGTERM` semantics assumed throughout.
- **Auto-upgrade / auto-restart of the daemon when a mismatch is detected.** Tempting but the same data-loss objection applies: we cannot assume the user is willing to lose state. The fix is detect + report + provide a recovery command, not silently bounce the daemon.
- **Bumping `PROTOCOL_VERSION`.** Considered explicitly because `src/daemon_protocol.rs:5` says new `AttachRequest` variants require a bump. This PRD does **not** add a new `AttachRequest` variant: the original draft proposed a `GetDaemonInfo` variant, but that was replaced with `SO_PEERCRED`/`LOCAL_PEERPID` peer-credential reads on the connected `UnixStream`, which is an OS-level facility, not a protocol message. Only optional fields are added to the existing `Hello` request and `AttachResponse` (both gated by `#[serde(default, skip_serializing_if = "Option::is_none")]` — see Wire format), which is the additive case that the protocol module's contract explicitly allows without a version bump. The wire shape of every existing frame remains backward-compatible across rounds 9-12. If a future PRD needs to add a *new* variant, that PRD owns the `PROTOCOL_VERSION` bump.
- **Cross-version compatibility shims.** No attempt to make a v0.24.x daemon work with a v0.25.0 TUI. The handshake is detect-and-refuse, not negotiate-and-adapt.
- **A general "rolling upgrade" / hot-reload story for the daemon.** That is a much larger design problem; this PRD only closes the cliff-edge case.
- **A pidfile at `<state_dir>/daemon.pid`.** Considered and rejected. Pros: cheap PID lookup without an attach round-trip. Cons: another piece of state to keep coherent (stale-file after a crash, ownership/permissions, cleanup on graceful shutdown), and the attach socket is *already* the rendezvous point and *already* authoritative — if the socket is gone, there is nothing to stop; if it is present, `SO_PEERCRED` / `LOCAL_PEERPID` on the connected stream returns the PID directly without any protocol exchange and against any daemon version. Revisit only if a real need arises (e.g. a `systemctl` integration that wants to read the PID out-of-band).
- **Changing `DAD_VERSION`'s shape.** It stays exactly what it is today (the tag, e.g. `0.25.0`). User-facing surfaces (`--version`, the remote registry's `version` field, the `daemon hello` JSON `version` field if any) keep their semantics. `DAD_BUILD_ID` is a separate variable serving a different purpose.

## Success Criteria

- `DAD_BUILD_ID` is non-empty in every release build and includes commit hash and dirty marker. Two builds at the same tag but different commits produce different `DAD_BUILD_ID`s. A dirty-tree build produces a `-dirty`-suffixed `DAD_BUILD_ID`. Verified by a build-script-level test.
- A TUI compiled at build-id A attaching to a daemon at build-id B (A ≠ B, including the same-tag-different-commit case that hit us) prints a clear stderr error naming both build-ids and the recovery command, and exits non-zero. Verified by an integration test.
- A TUI at build-id A attaching to a daemon at build-id A proceeds without user-visible change (modulo a single extra round-trip on startup). Verified by integration test.
- `dot-agent-deck daemon stop` on a daemon with no managed agents shuts it down and the socket file is gone, in under 5s. Verified by integration test.
- `dot-agent-deck daemon stop` on a daemon with a live managed agent refuses with a clear message and exits non-zero. `--force` bypasses the guard. Verified by integration test.
- `probe_remote_protocol` reports a build-version mismatch when the remote's `daemon hello` carries a different `build_version`, and the error points at `remote upgrade`, not `daemon stop`. Verified by a fake-ssh-executor test in the existing `connect.rs` suite.
- Documentation explains the upgrade flow ("after upgrading, run `daemon stop` if a previous daemon is still running") and the new commands. A user following only the docs can recover from the original bug.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass. `cargo test` passes.
- Manual smoke: upgrade the binary while a daemon is running, launch the TUI, observe the mismatch error; run `daemon stop`; relaunch the TUI; observe normal startup.

## Milestones

### Phase 1: Build identity and wire-level changes

- [ ] **M1.0** — Extend `build.rs` to emit `cargo:rustc-env=DAD_BUILD_ID=<DAD_VERSION>-g<short-sha>[-dirty]`. Use `git rev-parse --short HEAD` for the SHA and `git status --porcelain` (non-empty output → dirty) for the dirty marker. Fall back to `<DAD_VERSION>-unknown` when git is unavailable or fails (same fallback discipline `git_version()` already follows). **Rerun-if-changed strategy** (so a clean branch advancing past the latest tag actually invalidates the cached `DAD_BUILD_ID`): keep the existing `cargo:rerun-if-changed=.git/HEAD` AND read `.git/HEAD` from `build.rs`, parse the `ref:` line if present (e.g. `ref: refs/heads/main`), and emit `cargo:rerun-if-changed=.git/<that-ref-path>` for the resolved ref file. Additionally emit `cargo:rerun-if-changed=.git/index` (dirty/clean transitions) and `cargo:rerun-if-changed=.git/packed-refs` (the resolved ref may live there post-`git gc`). On detached HEAD (HEAD file contains a raw SHA), the `.git/HEAD` watch alone suffices because the SHA in HEAD itself changes on commit. `DAD_VERSION` stays unchanged. Add a smoke test (e.g. `tests/build_id.rs`) asserting `env!("DAD_BUILD_ID")` is non-empty and starts with `env!("DAD_VERSION")`.
- [ ] **M1.1** — Extend `AttachResponse` with `build_version: Option<String>` (`src/daemon_protocol.rs:323`) using the wire-format serde attrs documented above. Update `AttachResponse::hello` (`:395`) to populate it from the daemon's compiled-in `env!("DAD_BUILD_ID")`. Add a serde round-trip test asserting backward-compat deserialization (older JSON with no `build_version` → `None`).
- [ ] **M1.2** — Extend `AttachRequest::Hello` (`src/daemon_protocol.rs:307`) with `client_build_version: Option<String>` using the same wire-format serde attrs. Daemon-side handler in the `Hello` arm (`:777`) reads it for logging only — does not reject on client value (mirrors PRD #76 M2.21 server-policy: only client decides). Add a serde test asserting older `Hello` JSON (no `client_build_version`) deserializes successfully.
- [ ] **M1.3** — Update `daemon hello` CLI subcommand (`src/main.rs:777`, the `cmd_daemon_hello` function) to also emit `build_version = env!("DAD_BUILD_ID")` in its static-print response so the `connect` strict path can pick up build-version skew across ssh too.
- [ ] **M1.4** — Update `probe_remote_protocol` in `src/connect.rs` (around `:460`) to compare the parsed remote `AttachResponse.build_version` against `env!("DAD_BUILD_ID")`. On mismatch (including the case where the remote omits `build_version`, which means a pre-this-PRD remote binary), surface a structured error variant (extend the existing `ProtocolMismatch` family) whose user-facing message names the remote's `build_version`, the local `build_version`, and points at `dot-agent-deck remote upgrade <name>` (NOT `daemon stop` — remote-build skew is fixed by re-installing the binary on the remote, not by stopping a local daemon). Add a fake-ssh-executor test covering both the match and mismatch cases.

### Phase 1.5: PID discovery via socket peer credentials

- [ ] **M1.5** — Add a small `peer_pid(stream: &UnixStream) -> io::Result<u32>` helper next to the attach-socket plumbing (e.g. `src/daemon_attach.rs`). Implementation: prefer `std::os::unix::net::UnixStream::peer_cred()` and extract `UCred::pid()` (returns `Option<u32>`; on a modern Rust toolchain `Some(pid)` on both Linux and macOS). If `peer_cred().pid()` returns `None` at runtime, fall back to direct `getsockopt`: `SO_PEERCRED` on Linux (target_os = "linux") returning `struct ucred`, and `LOCAL_PEERPID` on macOS (target_os = "macos") via `getsockopt(fd, SOL_LOCAL, LOCAL_PEERPID, ...)` — both via the `nix` crate or `libc` directly. Unit tests bind a `UnixListener`, connect, and assert the helper returns the current process's PID on both supported targets (or `#[cfg]`-gates the test per-OS). Critically: **this helper exchanges no protocol bytes with the daemon**, so it works against any daemon binary — including the v0.24.x daemon that motivated this PRD.

### Phase 2: TUI local-attach check

- [ ] **M2.1** — In `run_tui_session` (`src/main.rs`), after `ensure_external_daemon_or_die` succeeds, open a fresh client connection and send `Hello { client_version: PROTOCOL_VERSION, client_build_version: Some(env!("DAD_BUILD_ID").into()) }`. Parse `AttachResponse`. If `build_version` is `None` (older daemon) or differs from `env!("DAD_BUILD_ID")`, write a clear stderr message and exit non-zero. Message format:
  ```
  error: local daemon is build <daemon-build-id> but this TUI is build <tui-build-id>
  recover with: dot-agent-deck daemon stop
  ```
- [ ] **M2.2** — Make the check `RUST_LOG=debug`-traceable: log the round-trip on success too, so users debugging unrelated startup issues can see the handshake fired. Keep the success path silent on stderr.
- [ ] **M2.3** — Decide and document: what happens if the daemon was just spawned by `ensure_external_daemon_or_die` itself (i.e. it's necessarily our build)? Either skip the check (small optimization) or run it anyway (defense in depth, catches bugs in `ensure_external_daemon_or_die`). Recommend running it anyway — the cost is one extra round-trip on cold start, the upside is a smoke test of the handshake on every launch.

### Phase 3: `daemon stop` and `daemon restart` CLI

- [ ] **M3.1** — Add `DaemonCmd::Stop { force: bool }` and `DaemonCmd::Restart { force: bool }` variants in `src/main.rs:135`. Wire them through the existing `Daemon` subcommand dispatcher.
- [ ] **M3.2** — Implement `daemon stop`: open the attach socket; if the socket is missing or connect fails with `ECONNREFUSED`/`ENOENT`, print "no daemon running" and exit 0 (idempotent). Call `peer_pid()` from M1.5 to learn the daemon's PID — this is the load-bearing step that works against stale daemons. Send `ListAgents` (existing variant — supported by all daemon versions back to PRD #76 era). If any agents are alive and `!force`, print a clear refusal message naming the live agent IDs and exit non-zero. Otherwise send `SIGTERM`; poll the socket file disappearance every 100ms up to 5s; on timeout with `--force`, send `SIGKILL`; on timeout without `--force`, report the daemon did not exit cleanly and exit non-zero. **Add an integration test that explicitly exercises the stale-daemon recovery path**: spawn a daemon built without `build_version` support (simulated via a feature flag or a test-only protocol stub that omits the field), then run `daemon stop` against it and assert success — this verifies the command is not transitively dependent on any new protocol surface.
- [ ] **M3.3** — Implement `daemon restart`: just `daemon stop` with the same flags, then return. Lazy-spawn on next TUI invocation per PRD #93.

### Phase 4: Tests

- [ ] **M4.1** — Unit tests in `daemon_protocol.rs`: `AttachResponse::hello` populates both fields; serde round-trip preserves them; older-shape JSON (no `build_version`) deserializes to `None`; older-shape `Hello` JSON (no `client_build_version`) deserializes successfully on the daemon side.
- [ ] **M4.2** — Integration test: spawn a daemon, attach with a TUI built against a synthetic `DAD_BUILD_ID` (use a test helper that injects the comparison value rather than rebuilding the binary). Assert mismatch → exit-non-zero + expected stderr message; match → normal startup. Cover the same-tag-different-commit case explicitly (two synthetic build-ids sharing a tag prefix but differing in the `-g<sha>` suffix).
- [ ] **M4.3** — Integration test for `daemon stop`: spawn daemon with no agents → `daemon stop` succeeds, socket gone within 5s. Spawn daemon, start an agent, attempt `daemon stop` → refuses with informative error. Same scenario with `--force` → succeeds, agent dies. With no daemon running → idempotent exit 0.
- [ ] **M4.4** — Integration test for `daemon restart`: spawn daemon, run `daemon restart`, confirm the original daemon PID is gone and the next TUI lazy-spawn produces a fresh daemon at the current build.
- [ ] **M4.5** — Fake-ssh-executor test in `connect.rs` for the remote build-version mismatch path (M1.4): assert the resulting error message names `remote upgrade` (not `daemon stop`) and includes both build-ids.

### Phase 5: Docs and release

- [ ] **M5.1** — Update the daemon-lifecycle docs page (introduced by PRD #93) to describe the upgrade workflow and the new commands. Add a Troubleshooting entry for "delegate prompts silently no-op after an upgrade".
- [ ] **M5.2** — Changelog fragment via `dot-ai-changelog-fragment`. Bug-fix framing: "fix: local TUI now detects version skew against a stale daemon and exits cleanly; add `dot-agent-deck daemon stop`/`restart`".
- [ ] **M5.3** — PR, review, audit, merge, close.

## Key Files

- `build.rs` — adds the new `DAD_BUILD_ID` env var alongside `DAD_VERSION` (M1.0).
- `src/daemon_protocol.rs` — `AttachRequest::Hello` (`:307`), `AttachResponse` (`:323`), `AttachResponse::hello` (`:395`), `PROTOCOL_VERSION` (`:121`), the `Hello` handler arm (`:777`). Only additive field changes here (no new `AttachRequest` variant — see Out of Scope). Round-trip tests live here.
- `src/daemon_attach.rs` — `ensure_external_daemon_or_die` (`:393`). The version check and the new `peer_pid()` helper live here. The version comparison itself lives in `main.rs` because the binary crate is where `env!("DAD_BUILD_ID")` resolves.
- `src/main.rs` — `run_tui_session` (around `:583` where `ensure_external_daemon_or_die` is called); `DaemonCmd` enum (`:135`); `cmd_daemon_hello` (`:777`); new `cmd_daemon_stop` / `cmd_daemon_restart`.
- `src/connect.rs` — `probe_remote_protocol` (`:460`); the remote `AttachResponse` deserialization (`:495`) and `server_version` comparison (`:517`) — the new `build_version` comparison sits alongside.
- `prds/93-always-external-daemon.md` — parent PRD. Line 39 references the promised "equivalent local command".
- `prds/90-remote-daemon-upgrade.md` — related PRD that benefits from the M1.4 remote build-version comparison.
- `docs/installation.md` and/or `docs/getting-started.mdx` — daemon-lifecycle docs introduced by PRD #93; this PRD extends them.

## Risks and Mitigations

- **Risk**: An older daemon's `Hello` response is `None` for `build_version` and we mis-classify a *deliberately* compatible older daemon as incompatible.
  - *Mitigation*: We are explicitly treating `None` as "incompatible — recycle the daemon". The case where an older daemon happens to be wire-compatible *and* semantically compatible is impossible to detect without out-of-band knowledge, so erring on the side of refuse-and-explain is correct. The user can `daemon stop` cheaply.
- **Risk**: The check adds startup latency on every TUI launch.
  - *Mitigation*: One extra Unix-socket round-trip is negligible (microseconds locally). Verified in M2.2 by `RUST_LOG=debug` traces.
- **Risk**: `daemon stop` races with a TUI that is concurrently attaching (the TUI just spawned a daemon; another shell runs `daemon stop` before the TUI sends `Hello`).
  - *Mitigation*: Document that `daemon stop` is a user-initiated recovery, not a routine. The race produces a clean error in the TUI ("connection refused" or socket disappearance) rather than corruption. No need to coordinate.
- **Risk**: PID resolution is fragile if the daemon was not started by us (e.g. `systemctl --user` unit, or a developer running `cargo run -- daemon serve` directly).
  - *Mitigation*: We use `SO_PEERCRED` / `LOCAL_PEERPID` on the connected attach socket (M1.5) — an OS-level facility that returns the actual PID of the process holding the other end, regardless of how that process was started or what protocol surface it implements. No pidfile; no protocol round-trip. If we cannot reach the socket, there is no daemon to stop and `daemon stop` exits 0 idempotently.
- **Risk**: `std::os::unix::net::UCred::pid()` historically returned `None` on macOS in older Rust versions, even though the OS supports `LOCAL_PEERPID`.
  - *Mitigation*: M1.5 specifies a fallback to direct `getsockopt`(`LOCAL_PEERPID`) on macOS via `nix`/`libc`. A unit test gated by `#[cfg(target_os = "macos")]` confirms the helper returns a real PID on the current toolchain. If the Rust stdlib already returns `Some(pid)`, the fallback is dead code that costs nothing.
- **Risk**: `daemon stop` against a *stale* daemon — the very case this command exists for — must not depend on any protocol surface the stale daemon doesn't implement.
  - *Mitigation*: PID discovery is OS-level peer-cred (no protocol). The agent-liveness check uses `ListAgents`, which is an existing variant supported by every daemon in scope (PRD #76 era and later). M3.2 includes an explicit integration test that exercises `daemon stop` against a daemon stubbed to omit `build_version` (i.e. simulating the v0.24.x stale daemon) and asserts success.
- **Risk**: `DAD_BUILD_ID` derivation in `build.rs` fails in shallow clones (CI, `cargo install` from crates.io, tarball builds) where git metadata is absent.
  - *Mitigation*: Fall back to `<DAD_VERSION>-unknown` (mirrors the existing `git_version()` fallback to `CARGO_PKG_VERSION`). In a tarball build, both client and server come from the same source artifact so build_ids match trivially and the `-unknown` suffix is harmless. For shallow CI clones, encourage `fetch-depth: 0` in release jobs; non-release CI builds are not version-sensitive.
- **Risk**: A dirty-tree build (`-dirty` suffix) means every successive `cargo build` during local development produces a new `build_id`, forcing a `daemon stop` between iterations.
  - *Mitigation*: Document the workflow — local development of daemon code should detach existing managed agents and `daemon stop` before each iteration anyway, since the *behavior* under test is what changed. If this becomes painful in practice, add a `DOT_AGENT_DECK_SKIP_BUILD_CHECK=1` escape hatch (deferred to an open question rather than baked into this PRD).
- **Risk**: The recovery command name (`daemon stop`) is taken by a future use case (e.g. stopping a *remote* daemon).
  - *Mitigation*: `dot-agent-deck remote stop` already exists for remotes; `dot-agent-deck daemon stop` is the local equivalent. The split is symmetric with `remote add` vs. (future) `daemon ...` and consistent with PRD #93's line 39.
- **Risk**: Bumping `PROTOCOL_VERSION` retroactively for rounds 9-12 might be a more honest fix.
  - *Mitigation*: It would force every user to manually recycle their daemon on every release whose only change is internal refactors — a regression in user experience. The `build_version` field is the targeted fix; `PROTOCOL_VERSION` stays meaningful for actual wire-shape changes.
- **Risk**: The contract in `src/daemon_protocol.rs:5` says new `AttachRequest` variants require a `PROTOCOL_VERSION` bump. An earlier draft of this PRD violated that by proposing a `GetDaemonInfo` variant.
  - *Mitigation*: That variant is removed. PID discovery is now OS-level peer-cred. The only wire changes are *additive optional fields* on existing variants (`Hello`, `AttachResponse`) gated by `#[serde(default, skip_serializing_if = "Option::is_none")]`, which is the additive case the protocol module explicitly permits without a bump. See the "Bumping `PROTOCOL_VERSION`" item in Out of Scope for the full rationale.

## Open Questions

- Should `daemon stop` emit a JSON-formatted summary (for scripting) in addition to human text? Defer to M3.2 — start with human text, add `--json` if a real consumer appears.
- Should the version-mismatch error mention `--force` of `daemon stop`, or only the safe form? Recommendation: only the safe form. `--force` is documented in the command's `--help`; the error message should not encourage data loss as a first resort.
- Should a daemon detect its *own* `env!("DAD_BUILD_ID")` mismatching the binary on disk (e.g. by reading `/proc/self/exe`)? Out of scope for this PRD — the laptop-side check is the simpler, sufficient fix for the observed bug. Revisit only if real cases arise where the daemon process itself outlives a binary swap *and* a new TUI does not catch it via this handshake.
- Should a `DOT_AGENT_DECK_SKIP_BUILD_CHECK=1` escape hatch exist for local development against dirty-tree builds? Defer until pain is reported; the recommended workflow (stop daemon between iterations) is the correct default.
