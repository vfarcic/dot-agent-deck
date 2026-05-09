# PRD #76: Remote Agent Environments

**Status**: In progress
**Priority**: High
**Created**: 2026-05-08
**Scope updated**:
- 2026-05-09 — Kubernetes transport split out into **PRD #80**.
- 2026-05-09 (later) — **Architectural pivot**. Phase 6 (laptop-as-real-client with `ProjectIO` trait + extended protocol) was added when M2.x exposed project-parity gaps, then dropped after a cost/value review. v1 now ships the simpler **TUI-on-remote** model: `connect` is an `ssh -t` wrapper, the TUI runs server-side, the daemon owns PTYs as a separate process so they survive ssh disconnects. See the *2026-05-09 architectural pivot* entry in Design Decisions.
**GitHub Issue**: [#76](https://github.com/vfarcic/dot-agent-deck/issues/76)

## Problem Statement

AI coding agents (Claude Code, OpenCode, etc.) launched from `dot-agent-deck` today live as local PTY children of the deck process. Three concrete consequences:

1. **Laptop is a single point of failure.** When the laptop sleeps, closes, or loses network, every running agent dies. Long-running tasks (codebase-wide refactors, large test runs, multi-step orchestrations) cannot survive a normal day's interruptions.
2. **No way to offload to a beefier box.** Users with a remote workstation or a Hetzner-style VM cannot point the deck at it; the deck only knows local processes.
3. **No persistent context across "I'll come back to this later".** There is no notion of a session that keeps running while the user is away and is reattachable from the same or a different device.

The primitives to fix this exist (ssh, PTYs, hook callbacks already work over a Unix socket) but the deck has no architecture for "the agents live somewhere else."

## Solution Overview

Introduce **remote environments** as a first-class concept in `dot-agent-deck`. A remote environment is a **long-running, per-project box** (a VM, in v1) that the user has provisioned. The deck's local CLI manages a **registry** of these environments (add, list, remove, upgrade) and provides a **`connect`** command that opens an `ssh -t` session to the remote and runs the deck TUI there.

The TUI on the remote attaches to a **separate-process daemon** that owns all agent PTYs and accepts hook callbacks on a local Unix socket. Because the daemon is a separate process, it survives ssh disconnects: PTYs and agent state persist across "laptop closed the lid", and re-running `connect` re-attaches a fresh TUI to the still-running daemon.

A future Kubernetes (`kubectl exec`) transport ships separately as **PRD #80**.

### Core architectural choices

- **Per-project long-running environment, not per-agent ephemeral pods.** The "feels local" criterion wins: shared filesystem, warm build caches, ability to stop one agent and start another against the same in-progress state. Pod-per-agent was considered and rejected — it's the right shape for ephemeral CI runners, the wrong shape for a remote dev box.
- **TUI-on-remote, not laptop-as-real-client.** The deck TUI runs on the environment, not on the laptop. ssh forwards the terminal. This collapses an entire category of "make every UI feature work over a custom protocol" work (directory pickers, project config loading, side panes, watch rules, delegate dispatch) into "they just work, they're running locally on the same box as the daemon."
- **Daemon as separate process, not in-TUI tokio task.** In remote mode the daemon must outlive the TUI so PTYs survive ssh disconnect. Local mode (the laptop) keeps the in-process daemon for simplicity; the separation is opt-in via the lazy-spawn path that already exists from M4.3.
- **Transport is a config detail, not an architectural fork.** ssh-to-VM is the v1 transport. `kubectl exec`-to-pod fits the same model (the TUI runs in-pod, kubectl just forwards the terminal); PRD #80 picks that up.
- **Provisioning is out of scope.** The product documents environment requirements (Linux distro, ssh, disk, RAM, network egress) and users provision via multipass / Hetzner / fly / whatever. No cloud-provider abstraction in the product, no terraform in-repo.

### User-facing model

Two-level hierarchy:

- **Environment** — long-lived, per project, registered locally by name. Holds the daemon and the project filesystem; persists across client disconnects.
- **Agent** — process inside an environment, started/stopped on demand from inside the deck (running on the remote).

CLI surface (noun-verb):

```
dot-agent-deck remote add <name>      # register a host, install binary + hooks
dot-agent-deck remote list             # show registered environments + last-known status
dot-agent-deck remote remove <name>
dot-agent-deck remote upgrade <name>   # version-bump the remote install
dot-agent-deck connect [name]          # picker if name omitted; ssh -t into the remote and run the deck TUI
```

`connect` flow: pick environment → `ssh -t <target> dot-agent-deck` (with the right args to attach to the persistent daemon) → terminal handed over to the remote TUI.

### Lifecycle semantics

| Event | Behavior |
|-------|----------|
| `Ctrl+W` on agent pane | Stop the agent process. The TUI sends "stop pane X" to its (local-on-remote) daemon over the existing socket protocol; daemon kills the child. **Same meaning local or remote.** |
| ssh disconnect (laptop sleep, lid close, network drop, or explicit detach) | TUI dies on the remote. Daemon (separate process) keeps running. Agents and their PTYs survive. **No special "detach" keybinding** — closing the ssh session is the detach. |
| Reconnect (`connect` again) | New TUI spawns on the remote, attaches to the still-running daemon, picks up the existing agent list and scrollback. |
| `dot-agent-deck remote remove` | Tear down the local registration; does **not** destroy the environment or the daemon on it. Environment teardown is the user's job (their VM, their lifecycle). |

### Local config

`~/.config/dot-agent-deck/remotes.toml`, keyed by name:

```toml
[[remotes]]
name = "hetzner-1"
type = "ssh"
ssh_target = "viktor@1.2.3.4"
install_path = "~/.local/bin/dot-agent-deck"
last_known_version = "0.25.0"
last_connected = "2026-05-08T14:23:00Z"
```

(PRD #80 will extend the registry with a `type = "kubernetes"` shape; the schema is forward-compatible.)

No secrets. Auth delegated to ssh. The file is human-editable; nothing is injected at runtime that the user can't inspect.

### Failure-mode clarity

The connect flow distinguishes three "can't connect" states:

- **Host unreachable** (network / ssh config invalid) — show ssh error verbatim, suggest checking network or ssh config.
- **Host reachable, deck binary missing or version-mismatched** — suggest `remote upgrade <name>`.
- **Host fine, no daemon yet** — handled by the lazy-spawn path: the TUI on the remote starts the daemon if there isn't one, no user-visible error.

Generic "connect failed" with no diagnostic is the path to user frustration; this is the wrong default and the PRD calls it out explicitly.

### Hook delivery in the remote model

Hooks (`src/hook.rs`) currently POST to a local Unix socket. In the remote model:

- The daemon socket lives **on the environment**, alongside the agents and the TUI. Hooks reach it via localhost — no network hop, no tunnel, no auth dance.
- Agent images / install scripts pre-wire hook URLs to the local daemon socket.
- The TUI queries the daemon over the same socket protocol for accumulated state on (re)connect — events that fired during disconnect are not lost.

### Filesystem model

- Project files live on the environment. Agent edits happen there.
- **Git is the sync layer** between laptop working copy and remote. No bidirectional sync (mutagen, syncthing) baked into the product.
- For VM: project state on the VM's disk. Standard.
- (Kubernetes filesystem model — PVC per environment — covered in PRD #80.)

## Scope

### In Scope

- Remote environment as a first-class concept (config, CLI, viewer).
- ssh-to-VM transport.
- Daemon as a separate, persistent process on the remote (lazy-spawned by the TUI on first attach; survives ssh disconnect).
- TUI-attaches-to-external-daemon mode for use on the remote: TUI uses the existing socket protocol instead of an in-process daemon.
- CLI surface: `remote add | list | remove | upgrade`, `connect`.
- Local registry file (`~/.config/dot-agent-deck/remotes.toml`).
- Lifecycle: `Ctrl+W` stops the agent; ssh disconnect detaches; reconnect resumes.
- Persistent agent processes across viewer disconnects.
- Reattachment to existing agents on a known environment.
- Failure-mode-aware connect UX.
- Version negotiation between client (laptop install) and remote daemon binary.
- Documentation: environment requirements, provisioning recipes, daily-use guide.
- A test environment that the maintainer uses for development of this PRD, set up by following the documented requirements (forcing function on the docs).

### Out of Scope

- **Kubernetes (`kubectl exec`) transport — moved to PRD #80.** This PRD ships ssh-to-VM only.
- **Laptop-as-real-client architecture.** The laptop's TUI does not consume a custom remote-fs/remote-exec protocol; it just forwards terminal bytes over ssh. Extending the protocol with `ListDir`, `ReadFile`, `StateSnapshot`, `SubscribeEvents`, etc. is **explicitly rejected** for v1 (see Design Decisions).
- **Multi-client viewing** of the same session from two laptops simultaneously. Tmux already solves this if a user really needs it; we won't reimplement it.
- VM provisioning automation (no terraform, multipass-wrapping, fly-wrapping in core).
- Multi-provider abstraction layer.
- Bidirectional file sync (mutagen, syncthing) — git is the sync layer.
- Multi-host federation: one dashboard controlling agents across multiple environments **simultaneously**.
- Reverse tunnels.
- Local-dir-mounted-into-remote via reverse sshfs and similar tricks.
- Pod-per-agent ephemeral model.
- A web UI / browser viewer. Terminal viewer only.
- Authentication beyond what ssh already provides.
- Remote-side multi-user access controls beyond Linux user separation.

## Technical Approach

### Daemon process model (`src/daemon.rs`)

The daemon is a Unix-socket consumer that ingests hook events and owns agent PTYs (Phase 1 work, already landed). It also exposes a streaming attach protocol used by the TUI to list agents, start/stop them, attach to PTY streams, and pull snapshots.

For remote use, the daemon must run as a **separate process** so it survives the TUI's exit (i.e., ssh disconnect). The infrastructure for this already exists from M4.3:

- `dot-agent-deck daemon serve` — runs the daemon as a long-lived process.
- Lazy-spawn: when something tries to attach and no daemon is running, spawn one detached (`setsid` + stdio-redirect + `flock`-serialized + trust-checked socket).

What's new in the pivot: a TUI mode where the TUI **attaches to an existing daemon** via the local Unix socket instead of running the daemon as an in-process tokio task. This is the path used on the remote.

### Transport: ssh-to-VM

- `dot-agent-deck remote add --type=ssh --target=<user@host>`:
  - Verify ssh works (`ssh <target> true`).
  - scp / install the matching `dot-agent-deck` binary to a known path (`~/.local/bin` by default).
  - Install hooks on the remote (run `dot-agent-deck hooks install` over ssh).
  - Verify the deck launches (`ssh <target> dot-agent-deck --version`).
  - Write the registry entry locally.
- `connect <name>`: `ssh -t <target> dot-agent-deck` (with arguments / env that make it attach to a persistent external daemon, not run an in-process one). Closing the local ssh session detaches; daemon and agents keep running.

### Transport: Kubernetes

Deferred to PRD #80. The same pattern applies — the TUI runs in-pod, `kubectl exec` forwards the terminal, the daemon is a separate process inside the pod.

### CLI: `remote` subcommand group

Implementation in `src/remote.rs` with subcommands wired into `main.rs`:

- `add` — interactive prompts or flags for type, target, install path; runs install verification.
- `list` — reads `remotes.toml`, optionally pings each for current status.
- `remove` — deletes registry entry.
- `upgrade` — reinstalls the matching binary on the remote.
- `connect [name]` — picker (TUI or fzf-style) if no name; otherwise immediate `ssh -t` into the remote.

### CLI: `daemon serve` and TUI external-daemon mode

`dot-agent-deck daemon serve` runs the daemon as a long-lived process (already exists from M4.3). The TUI on the remote is invoked with a flag/env (e.g. `DOT_AGENT_DECK_DAEMON_SOCKET=<path>` or `--external-daemon`) that switches it from in-process daemon to socket-attached daemon. Lazy-spawn ensures the daemon exists; if not, the TUI starts one detached and then attaches.

### Local viewer integration

In remote mode, **there is no laptop-side TUI**. The laptop is a terminal. The TUI runs on the remote, talking to the remote daemon over a Unix socket. ssh-the-binary handles forwarding.

The local-deck path (no `connect`, no remote) is unchanged: TUI spawns daemon as an in-process tokio task.

### Hooks on the remote

`dot-agent-deck hooks install` already writes per-agent hook scripts pointing at a local Unix socket. On the remote, the same install runs and points at the local daemon socket. No code change to the hooks themselves; they're agnostic to local-vs-remote because the remote *is* "local" from the daemon's point of view.

### Documentation deliverables

- `docs/remote-environments.md` (new) — what a remote environment is, the lifecycle model, Ctrl+W vs ssh-disconnect, failure modes, persistence model.
- `docs/remote-requirements.md` (new) — exact environment requirements (Linux distro, ssh access, disk, RAM, egress, optional container runtime). This doc is what the maintainer follows to set up the dev/test VM, and what users follow to set up theirs.
- `docs/remote-recipes.md` (new) — copy-pasteable provisioning snippets for multipass, Hetzner, fly. Maintenance burden kept low: each recipe is a few commands, no abstractions. (k3s recipe lives in PRD #80.)
- Update `docs/getting-started.mdx` and `docs/installation.md` to mention the remote option.

## Success Criteria

- A user with an empty laptop and a fresh remote box (provisioned via the recipes doc) can run `dot-agent-deck remote add hetzner-1`, `dot-agent-deck connect hetzner-1`, start an agent, and have that agent survive `Ctrl+Z`-ing the laptop, closing the lid, and reconnecting from a different network — within 24 hours, the agent is still there with full scrollback, and re-running `connect` shows it.
- `Ctrl+W` on a remote agent pane stops the agent process on the remote (verified via `ps` on the remote: the process is gone). It does **not** also kill the daemon.
- ssh disconnect leaves the daemon and agents running. Reconnect picks up the existing state.
- `remote list` shows last-known status; `connect` distinguishes the "host unreachable" vs "binary missing" failure modes with actionable messages.
- The maintainer's own dev/test VM is provisioned by following `docs/remote-requirements.md` from a clean box, with no out-of-band steps. Anything missing from the docs is a docs bug, fixed before merge.
- Hook callbacks fired during a viewer disconnect are reflected in the TUI state on reattach.
- Existing local-deck flow is unchanged. Users who never run `remote add` see no behavioral difference.
- Every TUI feature behaves identically in remote mode and local mode (because in remote mode it *is* local mode, just running on the remote box). No "this works locally but not remotely" carve-outs.

## Milestones

### Phase 0: Test environment and requirements doc

- [x] **M0.1** — Draft `docs/remote-requirements.md` from scratch.
- [x] **M0.2** — Maintainer provisions a personal dev/test VM by following the draft requirements doc end-to-end.
- [x] **M0.3** — Refine the requirements doc until a clean re-provision works without consulting any other source.

### Phase 1: Daemon owns PTYs

- [x] **M1.1** — Refactor `src/daemon.rs` to own agent PTYs. Local-deck mode keeps working; PTY ownership simply moves. Existing tests pass.
- [x] **M1.2** — Define and implement the streaming attach protocol over Unix socket: list-agents, start-agent, stop-agent, attach-stream, detach, snapshot.
- [x] **M1.3** — TUI viewer can attach to its own local daemon over the new protocol (single-machine).

### Phase 2: ssh transport (MVP remote)  [REWORKED post-pivot]

The pre-pivot Phase 2 built a laptop-side bridge that forwarded the daemon socket protocol over ssh. That work is **superseded** by the simpler TUI-on-remote model below; the bridge code is deleted in M2.7.

- [x] **M2.1** — `dot-agent-deck daemon serve`: runs the daemon as a long-lived separate process (originally landed as part of M4.3's lazy-spawn work).
- [x] **M2.2** — `remote add --type=ssh`: verifies ssh, installs binary on the remote, runs `hooks install`, writes registry entry.
- [x] **M2.3** — `remote list`, `remote remove`, `remote upgrade` commands.
- [x] **M2.4** — *(superseded — original "ssh-bridge connect" implementation; replaced by M2.8 / M2.9).*
- [x] **M2.5** — *(superseded — original "Ctrl+W vs explicit detach" semantics. New lifecycle uses ssh disconnect as detach; no separate keybinding.)*
- [x] **M2.6** — *(superseded — original "failure-mode-aware connect" depended on the bridge implementation. New connect surfaces ssh's own errors plus version mismatch.)*
- [ ] **M2.7** — **Delete the bridge.** Remove `src/connect.rs`'s ssh socket-bridging code (`build_tokio_ssh_command`, `bridge_socket_path`, `ConnectBridge`, the laptop-side stdio↔Unix-socket relay) and the `daemon attach` bridge entrypoint in `src/daemon_attach.rs` / `src/main.rs`. Keep what survives: `state_dir()`, lazy-spawn (used by `daemon serve`), trust-check / O_NOFOLLOW / flock hardening.
- [ ] **M2.8** — **TUI external-daemon mode.** The deck binary, when run with a flag/env (e.g. `DOT_AGENT_DECK_DAEMON_SOCKET=<path>` or `--external-daemon`), connects to an existing daemon socket via the M1.2 protocol instead of spawning an in-process daemon. Lazy-spawns one detached if absent (reusing M4.3's machinery). Local-mode behavior is unchanged when the flag/env is unset.
- [ ] **M2.9** — **`connect` becomes ssh-t wrapper.** Rewrite `connect [name]` to `ssh -t <target> dot-agent-deck <args-that-trigger-external-daemon-mode>`. Picker, version-mismatch detection, host-unreachable error surfacing all live in this milestone. End-to-end verified on the dev/test VM.
- [ ] **M2.10** — Lifecycle verification on the dev/test VM: `Ctrl+W` stops a remote agent (ps confirms); ssh disconnect leaves daemon + agents running; reconnect via `connect` re-attaches to the same agents with scrollback intact.

### Phase 3: Kubernetes transport — moved to PRD #80

PRD #80 picks up its own milestones (image, manifest, `remote add --type=kubernetes`, `connect` over `kubectl exec` running an in-pod TUI, PVC-preserving upgrade).

### Phase 4: Quality

- [x] **M4.1** — Tests for the streaming attach protocol (round-trip of all message types, disconnect/reconnect, partial frames).
- [x] **M4.2** — Integration test: spin up the daemon locally, attach, start an agent, detach, reattach, stop. End-to-end on a single machine.
- [x] **M4.3** — Lazy-spawn-on-attach: `daemon serve` subcommand, detached spawn via `setsid` + stdio-redirect, `flock`-serialized concurrency guard, XDG state dir, O_NOFOLLOW + 0o600 log file, socket trust check (uid + mode 0o600 + is-socket), full unit test coverage.
- [ ] **M4.4** — Manual end-to-end validation on the dev/test VM after M2.7–M2.10: ssh-t wrapper, lazy-spawn from remote TUI startup, hook events round-tripping through the remote daemon, every TUI feature behaves identically to local mode (because in remote mode it *is* local mode, just running on the remote).

### Phase 5: Documentation and release

- [x] **M5.1** — `docs/remote-environments.md`: lifecycle model, Ctrl+W semantics, ssh-disconnect-as-detach, failure modes, hook-on-remote behavior.
- [x] **M5.2** — `docs/remote-recipes.md`: provisioning snippets for multipass / Hetzner / fly.
- [x] **M5.3** — Final pass on `docs/remote-requirements.md` reflecting anything learned in M1–M4.
- [x] **M5.4** — Update `docs/getting-started.mdx` and `docs/installation.md` to mention the remote path.
- [ ] **M5.5** — Post-pivot doc refresh: revise `docs/remote-environments.md` to describe TUI-on-remote (drop any mentions of laptop-side bridge / streaming protocol over ssh / explicit detach keybinding). Revise the ssh-disconnect-as-detach lifecycle. Changelog fragment, release.

## Key Files

- `src/daemon.rs` — owns PTYs and serves the streaming attach protocol over a Unix socket. (Phase 1.)
- `src/embedded_pane.rs` — PTY spawn paths; daemon-owned in remote mode. (Phase 1.)
- `src/ui.rs`, `src/state.rs` — TUI; gains an external-daemon-attach mode (M2.8). Otherwise unchanged.
- `src/hook.rs` — unchanged; verifies it works pointing at the daemon's socket on the remote.
- `src/main.rs` — wires `connect`, `remote *`, `daemon serve`. The `daemon attach` bridge subcommand is removed in M2.7.
- `src/remote.rs` — `remote add | list | remove | upgrade` implementations and the `~/.config/dot-agent-deck/remotes.toml` registry.
- `src/connect.rs` — substantially shrinks in M2.7; the new `connect` is an `ssh -t` wrapper plus picker.
- `src/daemon_attach.rs` — the bridge entrypoint is removed in M2.7. Hardening helpers (`state_dir`, `verify_socket_trusted`, `ensure_daemon_running`) survive and are reused by M2.8.
- `src/daemon_protocol.rs` — unchanged from Phase 1. **No `ListDir` / `ReadFile` / `StateSnapshot` / `SubscribeEvents` ops** (those were Phase 6, dropped).
- `docs/remote-environments.md`, `docs/remote-requirements.md`, `docs/remote-recipes.md`.
- `docs/getting-started.mdx`, `docs/installation.md` — minor cross-references.

## Design Decisions

### 2026-05-08: Long-running per-project environment, not pod-per-agent

Considered three models: VM-per-environment, pod-per-agent (ephemeral), and pod-per-environment (long-running). The product requirement "feels local" is decisive: shared filesystem, warm caches, ability to stop one agent and start another in the same in-progress state are all properties of a long-running environment. Pod-per-agent is the right shape for ephemeral CI runners, the wrong shape for a remote dev box.

### 2026-05-08: Daemon-on-remote, not reverse tunnel

The deck already has a daemon (`src/daemon.rs`) that ingests hook events. Extending it to also own PTYs and live on the remote unifies the architecture: hooks reach a local-on-remote socket, no tunnels, no laptop dependency, no NAT punching. Reverse tunnels were the alternative — rejected because they tie the hook target's lifetime to the laptop's network presence, which defeats the persistence goal.

### 2026-05-08: Provisioning is documented, not productized

Considered building VM creation (`dot-agent-deck remote create-vm --provider=fly`). Rejected: cloud-provider matrix is a maintenance burden the project can't sustain (SDKs, regions, instance types, auth, billing, image rotation), and existing tools (multipass, fly, terraform) already do this well. The product accepts any environment that meets documented requirements; the docs ship recipes for common providers as starting points but no abstraction layer.

### 2026-05-08: Ctrl+W stops the agent (local or remote)

Initial framing had close-deck-on-remote default to detach. Reversed after user feedback: keep the mental model uniform across local and remote. Ctrl+W means "I'm done with this agent" in both. (The post-pivot model also drops the *separate* detach keybinding — ssh disconnect handles that — but Ctrl+W's meaning is still "stop the agent.")

### 2026-05-09: Architectural pivot — laptop is a terminal, not a real client

A first attempt at remote mode kept the laptop's TUI as the user-facing process, bridging the daemon socket protocol over ssh. M2.4–M2.6 implemented the bridge and it worked for PTY data, but project-aware features (directory picker `Ctrl+N`, `.dot-agent-deck.toml`-driven mode tabs and orchestrations, side panes, reactive panes, watch rules, delegate roles) all read laptop-side state, so `connect` was a misleading half-experience.

A "Phase 6" was scoped to fix this by extending the protocol with `ListDir`, `ReadFile`, `Stat`, `StateSnapshot`, `SubscribeEvents`, side-pane spawn, and rewiring all project-aware code through a `ProjectIO` trait. Estimated 9 milestones, plus a state-replication addition that surfaced in testing.

This was rejected on cost/value grounds:

- The complexity of replicating every TUI behavior over a custom protocol is high and ongoing — every new project-aware feature would need a protocol op and a `ProjectIO` branch.
- The benefit (multi-client viewing, laptop-native scripting against a remote) is not a stated user requirement.
- A simpler model — the TUI runs on the remote, ssh forwards the terminal, the daemon is a separate process so PTYs survive disconnect — gets the same product outcome (persistence, run-on-beefier-box, "feels local") with a fraction of the code.

The pivot:

- Drop Phase 6 entirely. No `ProjectIO`, no `ListDir`/`ReadFile`/`Stat`/`StateSnapshot`/`SubscribeEvents` protocol ops, no `--project-dir` plumbing through the laptop.
- Replace the ssh socket-bridge (`src/connect.rs` `ConnectBridge`, `daemon attach` entrypoint in `src/daemon_attach.rs`) with a thin `ssh -t <target> dot-agent-deck` wrapper.
- Add a TUI mode that attaches to an external daemon process (so the daemon can outlive ssh disconnect). Lazy-spawn (M4.3, already done) starts the daemon when needed.
- Keep what's still useful: Phase 1 (daemon owns PTYs as a separable process), M4.3 hardening (lazy-spawn, flock, XDG state dir, O_NOFOLLOW, trust-check), `daemon serve`, the `remote add/list/remove/upgrade` registry.

Net code delta is negative: roughly **−1500 LOC** across `src/connect.rs` and `src/daemon_attach.rs`'s bridge half, plus skipping ~9 milestones of Phase 6 work. The release window pulls in.

### 2026-05-08: Phase 0 is the test environment, set up by the docs

The first task is meta: the maintainer provisions a personal dev/test VM by following the same `docs/remote-requirements.md` that ships to users. The VM itself is not in-repo (no terraform, no scripts beyond what the docs say). This is a forcing function: if the docs aren't enough to set it up from a clean box, they're not enough for users.

## Open Decisions

To be resolved during implementation, not blocking PRD acceptance:

- **TUI external-daemon switch shape**: env var (`DOT_AGENT_DECK_DAEMON_SOCKET=<path>`), CLI flag (`--external-daemon`), or auto-detect (if a known socket path exists, use it). Likely answer: env var driven by `connect` so the laptop's `connect` decides; explicit flag for power-users invoking the deck on the remote directly.
- **Daemon persistence layer (VM)**: ad-hoc `setsid`-detached process from lazy-spawn (current M4.3 behavior, survives shell exit) is sufficient for v1. `systemd --user` units are an optional hardening step the docs can recommend; not required.
- **Picker UX in `connect`**: TUI list inside `dot-agent-deck` (consistent with rest of app) vs. shell-out to `fzf`. Likely answer: in-app TUI, fzf as fallback if not interactive.
- **Daemon-side socket file permissions**: ~~`src/daemon.rs` currently binds the Unix socket without explicit `set_permissions` / `chmod`...~~ **Resolved in M1.1** — `umask 0o177` (serialized via `UMASK_LOCK`) before `UnixListener::bind` so the socket inode is created at mode `0o600`, with a post-bind `chmod` retained as defense-in-depth. Verified by `daemon::tests::socket_is_0600_immediately_after_bind`.

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Streaming attach protocol design is over- or under-engineered | Already used in production via Phase 1. The pivot **reduces** the protocol's exposure (only used over local Unix sockets, never over the network), which lowers the risk further. |
| Version drift between client (laptop install) and remote daemon binary causes wire-format breakage | `connect` performs a version check via `ssh <target> dot-agent-deck --version` before handing off the terminal; on mismatch, refuses with a clear `remote upgrade` recommendation. |
| Hooks fire during disconnect and are silently lost | Daemon ingests hooks regardless of TUI presence (already true today). On reattach, the new TUI requests a state snapshot from the still-running daemon. |
| Daemon crash on the remote leaves orphaned agent processes | Daemon spawns agents with explicit process-group IDs and tracks them on disk; on restart (via lazy-spawn), reconciles (reattach to existing PTYs where possible, mark unattachable as crashed). |
| Docs drift from reality once dev VM is set up | M0.3 explicitly requires a clean re-provision from docs alone. Repeat any time `remote-requirements.md` changes. |
| Local-deck users see regressions from the daemon refactor | Phase 1 is gated on existing local tests passing. The post-pivot M2.8 (TUI external-daemon mode) is opt-in; the in-process daemon path is the default and unchanged. |
| Removing the bridge code (M2.7) breaks something subtle the bridge was doing | M2.7 is a delete-only milestone for the bridge halves; nothing else depends on them (the daemon protocol, hardening helpers, registry, and hooks are all bridge-independent). M4.4 verifies end-to-end on the VM after the delete. |

## References

- PRD #58 (multi-role agent orchestration) — `prds/58-multi-role-agent-orchestration.md`
- Existing daemon: `src/daemon.rs`
- Existing PTY ownership: `src/embedded_pane.rs`
- Existing hooks: `src/hook.rs`, `src/hooks_manage.rs`
- Existing event schema (version-risk surface): `src/event.rs`
