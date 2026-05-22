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

3. **`dot-agent-deck daemon stop` CLI.** A documented, non-`kill -9` way to recycle the local daemon. Resolves the daemon PID by asking the daemon itself over the attach socket (a new `GetDaemonInfo` request), sends `SIGTERM`, waits for graceful shutdown, falls back to `SIGKILL` after a short grace period only with `--force`. Refuses without `--force` when managed agents are still alive (data-loss guard — the agents would lose their PTYs on daemon exit). Optionally add `daemon restart` as a thin wrapper (`stop` then let the next TUI invocation lazy-spawn).

Together these mean: the upgrade-path race becomes detectable (TUI tells the user "this daemon is build 0.25.0-g243b049 but you are 0.25.0-gabc1234 — run `dot-agent-deck daemon stop` and retry") and recoverable (the user has a documented command instead of `kill -9`).

## Scope

### In Scope

- **Extend `build.rs` to emit a new `DAD_BUILD_ID` env var** of the form `<DAD_VERSION>-g<short-sha>[-dirty]`. Use `git rev-parse --short HEAD` for the SHA and `git status --porcelain` to detect a dirty tree. Fall back to `<DAD_VERSION>-unknown` if git is unavailable (same fallback discipline `git_version()` already follows). Add `cargo:rerun-if-changed=.git/index` so dirty/clean transitions are picked up. `DAD_VERSION` stays unchanged.
- **Add `build_version: Option<String>` to `AttachResponse`.** Optional so older daemons (which do not populate it) deserialize cleanly; the TUI treats `None` as "incompatible — daemon predates this check, ask the user to recycle it". The daemon populates it from `env!("DAD_BUILD_ID")`.
- **Extend `AttachRequest::Hello`** to optionally carry the client's `client_build_version: Option<String>` too. Symmetric to `server_version` / `client_version`. Daemon does not enforce on `client_build_version` (matching the existing PRD #76 pattern — only the client decides).
- **Wire-format contract for both new fields**: see "Wire format" subsection below — both fields are `Option<String>` with `#[serde(default, skip_serializing_if = "Option::is_none")]` so older peers (which omit them) round-trip cleanly. Non-negotiable for forward-compat.
- **TUI-side check in `run_tui_session`** (`src/main.rs`): after `ensure_external_daemon_or_die`, open the attach socket, send `Hello { client_version: PROTOCOL_VERSION, client_build_version: Some(env!("DAD_BUILD_ID").into()) }`, parse the response, compare `build_version`. Mismatch (including `None`) → write a clear error to stderr that names the local TUI build-id, the daemon build-id, and the recovery command (`dot-agent-deck daemon stop`), then exit non-zero.
- **Remote-side comparison in `probe_remote_protocol`** (`src/connect.rs`): the existing strict pre-flight already parses the remote daemon's `AttachResponse` from `daemon hello`. Extend it to compare `build_version` against the local `env!("DAD_BUILD_ID")` and surface a `ProtocolMismatch { remote, local, .. }`-style error on divergence. **Policy difference vs local**: remote-build skew is a *configuration* concern (the user can `dot-agent-deck remote upgrade` per PRD #90), not a stale-daemon concern, so the error must point at the *remote-upgrade* command, not at `daemon stop`. Local and remote share the field but route to different remediation.
- **New `GetDaemonInfo` attach-protocol request** (`src/daemon_protocol.rs`) that returns the daemon's PID and `build_version` in `AttachResponse`. This is the sole PID-discovery mechanism — there is no pidfile, intentionally (see Out of Scope). The socket itself is the rendezvous and the source of truth.
- **New CLI subcommand `daemon stop`**: opens the attach socket, sends `GetDaemonInfo` to learn the PID, sends `SIGTERM`, waits up to ~5s for the socket file to disappear. On `--force`, follows with `SIGKILL`.
- **Data-loss guard**: before sending `SIGTERM`, query the daemon's `ListAgents` over the attach socket. If any agent is still alive, refuse and instruct the user to either detach the agents first or pass `--force`.
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
- **Bumping `PROTOCOL_VERSION`.** The wire shape is genuinely unchanged across rounds 9-12 — bumping it would be lying about wire compat. The whole reason we are adding `build_version` is precisely so we do not have to overload `PROTOCOL_VERSION`.
- **Cross-version compatibility shims.** No attempt to make a v0.24.x daemon work with a v0.25.0 TUI. The handshake is detect-and-refuse, not negotiate-and-adapt.
- **A general "rolling upgrade" / hot-reload story for the daemon.** That is a much larger design problem; this PRD only closes the cliff-edge case.
- **A pidfile at `<state_dir>/daemon.pid`.** Considered and rejected. Pros: cheap PID lookup without an attach round-trip. Cons: another piece of state to keep coherent (stale-file after a crash, ownership/permissions, cleanup on graceful shutdown), and the attach socket is *already* the rendezvous point and *already* authoritative — if the socket is gone, there is nothing to stop; if it is present, we can ask. `GetDaemonInfo` over the existing socket is simpler and race-free. Revisit only if a real need arises (e.g. a `systemctl` integration that wants to read the PID out-of-band).
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

- [ ] **M1.0** — Extend `build.rs` to emit `cargo:rustc-env=DAD_BUILD_ID=<DAD_VERSION>-g<short-sha>[-dirty]`. Use `git rev-parse --short HEAD` for the SHA and `git status --porcelain` (non-empty output → dirty) for the dirty marker. Fall back to `<DAD_VERSION>-unknown` when git is unavailable or fails (same fallback discipline `git_version()` already follows). Add `cargo:rerun-if-changed=.git/index` so dirty/clean transitions invalidate the cache. `DAD_VERSION` itself stays unchanged. Add a smoke test (e.g. `tests/build_id.rs`) asserting `env!("DAD_BUILD_ID")` is non-empty and starts with `env!("DAD_VERSION")`.
- [ ] **M1.1** — Extend `AttachResponse` with `build_version: Option<String>` (`src/daemon_protocol.rs:323`) using the wire-format serde attrs documented above. Update `AttachResponse::hello` (`:395`) to populate it from the daemon's compiled-in `env!("DAD_BUILD_ID")`. Add a serde round-trip test asserting backward-compat deserialization (older JSON with no `build_version` → `None`).
- [ ] **M1.2** — Extend `AttachRequest::Hello` (`src/daemon_protocol.rs:307`) with `client_build_version: Option<String>` using the same wire-format serde attrs. Daemon-side handler in the `Hello` arm (`:777`) reads it for logging only — does not reject on client value (mirrors PRD #76 M2.21 server-policy: only client decides). Add a serde test asserting older `Hello` JSON (no `client_build_version`) deserializes successfully.
- [ ] **M1.3** — Update `daemon hello` CLI subcommand (`src/main.rs:777`, the `cmd_daemon_hello` function) to also emit `build_version = env!("DAD_BUILD_ID")` in its static-print response so the `connect` strict path can pick up build-version skew across ssh too.
- [ ] **M1.4** — Update `probe_remote_protocol` in `src/connect.rs` (around `:460`) to compare the parsed remote `AttachResponse.build_version` against `env!("DAD_BUILD_ID")`. On mismatch (including the case where the remote omits `build_version`, which means a pre-this-PRD remote binary), surface a structured error variant (extend the existing `ProtocolMismatch` family) whose user-facing message names the remote's `build_version`, the local `build_version`, and points at `dot-agent-deck remote upgrade <name>` (NOT `daemon stop` — remote-build skew is fixed by re-installing the binary on the remote, not by stopping a local daemon). Add a fake-ssh-executor test covering both the match and mismatch cases.

### Phase 1.5: PID discovery (attach-protocol)

- [ ] **M1.5** — Add a `GetDaemonInfo` variant to `AttachRequest` and a `daemon_info: Option<DaemonInfo>` field (or reuse `id`/`build_version` fields) to `AttachResponse` carrying the daemon's `pid: u32` and `build_version: String`. Handle the new request in the protocol dispatcher (`src/daemon_protocol.rs`). Round-trip serde tests in the same module. This becomes the *only* PID-discovery mechanism for `daemon stop`/`restart` — no pidfile is introduced (see Out of Scope rationale).

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
- [ ] **M3.2** — Implement `daemon stop`: open the attach socket; if the socket is missing, print "no daemon running" and exit 0 (idempotent). Send `GetDaemonInfo` (added in M1.5) to learn the PID. Query `ListAgents`. If any are alive and `!force`, print a clear refusal message naming the live agent IDs and exit non-zero. Otherwise send `SIGTERM`; poll the socket file disappearance every 100ms up to 5s; on timeout with `--force`, send `SIGKILL`; on timeout without `--force`, report the daemon did not exit cleanly and exit non-zero.
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
- `src/daemon_protocol.rs` — `AttachRequest::Hello` (`:307`), `AttachResponse` (`:323`), `AttachResponse::hello` (`:395`), `PROTOCOL_VERSION` (`:121`), the `Hello` handler arm (`:777`). New field additions, `GetDaemonInfo` variant, and round-trip tests live here.
- `src/daemon_attach.rs` — `ensure_external_daemon_or_die` (`:393`). The version check follows immediately after, but lives in `main.rs` rather than here so the check sees `env!("DAD_BUILD_ID")` from the binary crate.
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
  - *Mitigation*: We use `GetDaemonInfo` over the existing attach socket (M1.5) as the *sole* PID-discovery mechanism — no pidfile, intentionally. The socket is the source of truth: if we can talk to a daemon, we can ask its PID; if we cannot reach the socket, there is no daemon to stop and `daemon stop` exits 0 idempotently.
- **Risk**: `DAD_BUILD_ID` derivation in `build.rs` fails in shallow clones (CI, `cargo install` from crates.io, tarball builds) where git metadata is absent.
  - *Mitigation*: Fall back to `<DAD_VERSION>-unknown` (mirrors the existing `git_version()` fallback to `CARGO_PKG_VERSION`). In a tarball build, both client and server come from the same source artifact so build_ids match trivially and the `-unknown` suffix is harmless. For shallow CI clones, encourage `fetch-depth: 0` in release jobs; non-release CI builds are not version-sensitive.
- **Risk**: A dirty-tree build (`-dirty` suffix) means every successive `cargo build` during local development produces a new `build_id`, forcing a `daemon stop` between iterations.
  - *Mitigation*: Document the workflow — local development of daemon code should detach existing managed agents and `daemon stop` before each iteration anyway, since the *behavior* under test is what changed. If this becomes painful in practice, add a `DOT_AGENT_DECK_SKIP_BUILD_CHECK=1` escape hatch (deferred to an open question rather than baked into this PRD).
- **Risk**: The recovery command name (`daemon stop`) is taken by a future use case (e.g. stopping a *remote* daemon).
  - *Mitigation*: `dot-agent-deck remote stop` already exists for remotes; `dot-agent-deck daemon stop` is the local equivalent. The split is symmetric with `remote add` vs. (future) `daemon ...` and consistent with PRD #93's line 39.
- **Risk**: Bumping `PROTOCOL_VERSION` retroactively for rounds 9-12 might be a more honest fix.
  - *Mitigation*: It would force every user to manually recycle their daemon on every release whose only change is internal refactors — a regression in user experience. The `build_version` field is the targeted fix; `PROTOCOL_VERSION` stays meaningful for actual wire-shape changes.

## Open Questions

- Should `daemon stop` emit a JSON-formatted summary (for scripting) in addition to human text? Defer to M3.2 — start with human text, add `--json` if a real consumer appears.
- Should the version-mismatch error mention `--force` of `daemon stop`, or only the safe form? Recommendation: only the safe form. `--force` is documented in the command's `--help`; the error message should not encourage data loss as a first resort.
- Should a daemon detect its *own* `env!("DAD_BUILD_ID")` mismatching the binary on disk (e.g. by reading `/proc/self/exe`)? Out of scope for this PRD — the laptop-side check is the simpler, sufficient fix for the observed bug. Revisit only if real cases arise where the daemon process itself outlives a binary swap *and* a new TUI does not catch it via this handshake.
- Should a `DOT_AGENT_DECK_SKIP_BUILD_CHECK=1` escape hatch exist for local development against dirty-tree builds? Defer until pain is reported; the recommended workflow (stop daemon between iterations) is the correct default.
