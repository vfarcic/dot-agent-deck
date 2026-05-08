# PRD #76: Remote Agent Environments

**Status**: In progress
**Priority**: High
**Created**: 2026-05-08
**GitHub Issue**: [#76](https://github.com/vfarcic/dot-agent-deck/issues/76)

## Problem Statement

AI coding agents (Claude Code, OpenCode, etc.) launched from `dot-agent-deck` today live as local PTY children of the deck process. Three concrete consequences:

1. **Laptop is a single point of failure.** When the laptop sleeps, closes, or loses network, every running agent dies. Long-running tasks (codebase-wide refactors, large test runs, multi-step orchestrations) cannot survive a normal day's interruptions.
2. **No way to offload to a beefier box.** Users with a remote workstation, a Hetzner server, or a Kubernetes cluster cannot point the deck at it; the deck only knows local processes.
3. **No persistent context across "I'll come back to this later".** There is no notion of a session that keeps running while the user is away and is reattachable from the same or a different device.

The primitives to fix this exist (ssh, PTYs, hook callbacks already work over a Unix socket) but the deck has no architecture for "the agents live somewhere else." Bolting ssh-wrapping onto the existing command field is fragile (no version negotiation, no session listing, hooks can't reach back) and tmux-wraps-the-app is operationally clumsy.

## Solution Overview

Introduce **remote environments** as a first-class concept in `dot-agent-deck`. A remote environment is a **long-running, per-project box** (VM or single long-running Kubernetes pod) on which a **deck daemon** runs persistently, owns all agent PTYs, accepts hook callbacks on a local socket, and exposes a streaming attach protocol. The local client becomes a thin viewer that connects to the remote daemon over ssh exec or `kubectl exec` (MVP) or a daemon-exposed streaming socket (later).

### Core architectural choices

- **Per-project long-running environment, not per-agent ephemeral pods.** The "feels local" criterion wins: shared filesystem, warm build caches, ability to stop one agent and start another against the same in-progress state. Pod-per-agent was considered and rejected — it's the right shape for ephemeral CI runners, the wrong shape for a remote dev box.
- **Daemon-on-remote, not reverse tunnel.** The deck daemon (already present at `src/daemon.rs` for hook ingestion) is extended to also own agent PTYs and runs as the long-lived process inside the environment. Reverse tunnels were rejected — they tie the hook target's lifetime to the laptop, defeating the persistence goal.
- **Transport is a config detail, not an architectural fork.** ssh-to-VM and `kubectl exec`-to-pod are isomorphic for this design. Pick whichever matches the user's provisioning preference; the daemon protocol is the same.
- **Provisioning is out of scope.** The product documents environment requirements (Linux distro, ssh, disk, RAM, network egress, optional container runtime) and users provision via multipass / Hetzner / fly / k3s / whatever. No cloud-provider abstraction in the product, no terraform in-repo.

### User-facing model

Two-level hierarchy:

- **Environment** — long-lived, per project, registered locally by name. Owns the daemon, owns the project filesystem, persists across client disconnects.
- **Agent** — process inside an environment, started/stopped on demand by the deck.

CLI surface (noun-verb):

```
dot-agent-deck remote add <name>      # register a host, install binary + hooks
dot-agent-deck remote list             # show registered environments + last-known status
dot-agent-deck remote remove <name>
dot-agent-deck remote upgrade <name>   # version-bump the remote install
dot-agent-deck connect [name]          # picker if name omitted; per-env agent picker after
```

`connect` flow: pick environment → daemon enumerates running agents → user attaches to existing or starts a new one. Same flow whether the environment is a VM or a pod.

### Lifecycle semantics

| Event | Behavior |
|-------|----------|
| `Ctrl+W` on agent pane | Stop the agent process. Local: kill PTY child. Remote: send "stop pane X" message to daemon over its socket; daemon kills the child. **Same meaning local or remote.** |
| Explicit "Detach" action | Disconnect viewer; agent process keeps running on the environment. **Remote-only action**, distinct from Ctrl+W. |
| Laptop sleep / network drop | Implicit detach. Identical to explicit detach from the remote's point of view. Daemon and agent processes survive. |
| `dot-agent-deck remote remove` | Tear down the local registration; does **not** destroy the environment. Environment teardown is the user's job (their VM, their pod, their lifecycle). |

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

[[remotes]]
name = "k3s-prod"
type = "kubernetes"
context = "k3s-prod"
namespace = "agent-deck"
install_image = "ghcr.io/vfarcic/dot-agent-deck:0.25.0"
last_known_version = "0.25.0"
last_connected = "2026-05-07T09:11:00Z"
```

No secrets. Auth delegated to ssh / kubeconfig. The file is human-editable; nothing is injected at runtime that the user can't inspect.

### Failure-mode clarity

The connect flow distinguishes three "can't connect" states:

- **Host unreachable** (network / kubeconfig invalid) — show ssh/kubectl error verbatim, suggest checking network or kubeconfig.
- **Host reachable, daemon binary missing or version-mismatched** — suggest `remote upgrade <name>`.
- **Host fine, no running agents** — offer "start new agent" directly.

Generic "connect failed" with no diagnostic is the path to user frustration; this is the wrong default and the PRD calls it out explicitly.

### Hook delivery in the remote model

Hooks (`src/hook.rs`) currently POST to a local Unix socket. In the remote model:

- The daemon socket lives **on the environment**, alongside the agents. Hooks reach it via localhost — no network hop, no tunnel, no auth dance.
- Agent images / install scripts pre-wire hook URLs to the local daemon socket.
- The viewer queries the daemon over the streaming attach protocol for accumulated state on (re)connect — events that fired during disconnect are not lost.

### Filesystem model

- Project files live on the environment. Agent edits happen there.
- **Git is the sync layer** between laptop working copy and remote. No bidirectional sync (mutagen, syncthing) baked into the product.
- For Kubernetes: project state on a PVC per environment, mounted into the pod. Pod restart preserves state.
- For VM: project state on the VM's disk. Standard.

## Scope

### In Scope

- Remote environment as a first-class concept (config, CLI, viewer).
- ssh-to-VM transport.
- Kubernetes (`kubectl exec`) transport.
- Daemon-on-remote architecture: daemon owns agent PTYs, accepts hook callbacks, exposes streaming attach protocol.
- CLI surface: `remote add | list | remove | upgrade`, `connect`.
- Local registry file (`~/.config/dot-agent-deck/remotes.toml`).
- Lifecycle distinction: Ctrl+W (stop agent) vs explicit detach (keep agent running).
- Persistent agent processes across viewer disconnects.
- Reattachment to existing agents on a known environment.
- Failure-mode-aware connect UX (three distinct error states).
- Version negotiation between client and remote daemon.
- Documentation: environment requirements, provisioning recipes, daily-use guide.
- A test environment that the maintainer uses for development of this PRD, set up by following the documented requirements (forcing function on the docs).

### Out of Scope

- VM / cluster provisioning automation (no terraform, multipass-wrapping, fly-wrapping in core).
- Multi-provider abstraction layer.
- Bidirectional file sync (mutagen, syncthing) — git is the sync layer.
- Multi-host federation: one dashboard controlling agents across multiple environments **simultaneously**. Single-environment-at-a-time is the v1 model.
- Reverse tunnels.
- Local-dir-mounted-into-remote-pod via reverse sshfs and similar tricks.
- Pod-per-agent ephemeral model.
- A web UI / browser viewer. Terminal viewer only.
- Authentication beyond what ssh / kubeconfig already provide.
- Remote-side multi-user access controls beyond Linux/Kubernetes user separation.

## Technical Approach

### Daemon process model (`src/daemon.rs`)

The current daemon is a Unix-socket consumer that ingests hook events into `SharedState`. Extend it so it can also:

- **Own agent PTYs.** Today PTYs are spawned by the TUI process via `portable-pty` (`src/embedded_pane.rs`). In the remote model, the daemon spawns and owns them. Locally, the TUI keeps spawning its own — the daemon-PTY split is gated on transport.
- **Expose a streaming attach protocol.** New socket-based protocol the viewer connects to. Carries: list-agents, start-agent, stop-agent, attach-stream (bidirectional bytes), detach, current-state-snapshot.
- **Persist as PID-1ish** of the environment. Recommended runtime: a `systemd --user` unit on VMs, an ENTRYPOINT on Kubernetes — both restart the daemon on crash without losing the agent processes (which are children spawned with their own session IDs and survive an orderly daemon restart via re-attach to existing PTYs, or are explicitly cleaned up on daemon hard crash; see Open Decisions).

### Transport: ssh-to-VM

- `dot-agent-deck remote add --type=ssh --target=<user@host>`:
  - Verify ssh works (`ssh <target> true`).
  - scp / install the matching `dot-agent-deck` binary to a known path (`~/.local/bin` by default).
  - Install hooks on the remote (run `dot-agent-deck hooks install` over ssh).
  - Verify the daemon starts and a basic protocol roundtrip succeeds.
  - Write the registry entry locally.
- `connect <name>`: ssh to the target, run `dot-agent-deck daemon attach`, the daemon's streaming attach protocol takes over the ssh stdio. Closing the local viewer kills the local ssh process — daemon and agents keep running.

### Transport: Kubernetes

- `dot-agent-deck remote add --type=kubernetes --context=<ctx> --namespace=<ns>`:
  - Verify `kubectl` works against the context.
  - Apply a manifest creating a long-running Pod (or StatefulSet for the PVC) running the daemon image. Manifest is opinionated and ships in-repo.
  - Pod's ENTRYPOINT is the daemon. Pod has a PVC mounted at `/workspace` for project state.
  - Verify pod becomes Ready and a basic protocol roundtrip succeeds via `kubectl exec`.
- `connect <name>`: `kubectl exec -it <pod> -- dot-agent-deck daemon attach`. Same protocol, different transport.
- Image versioning: image tag matches `dot-agent-deck` version. `remote upgrade` re-applies the manifest with a new tag.

### CLI: `remote` subcommand group (new)

Implementation lives in a new module (e.g., `src/remote.rs`) with subcommands wired into `main.rs`:

- `add` — interactive prompts or flags for type, target/context, install path; runs install verification.
- `list` — reads `remotes.toml`, optionally pings each for current status.
- `remove` — deletes registry entry.
- `upgrade` — reinstalls binary / re-applies manifest matching the local version.
- `connect [name]` — picker (TUI or fzf-style) if no name; otherwise direct attach.

### CLI: `daemon attach` (new)

`dot-agent-deck daemon attach` is what runs **on the remote** when the viewer connects. It speaks the streaming attach protocol over stdio, so it works equally well over ssh exec or kubectl exec.

### Local viewer integration

The TUI (`src/ui.rs`, `src/state.rs`) gains a remote-deck mode where panes are not local PTY-backed but **stream-backed**, reading bytes from the daemon's attach protocol and forwarding keystrokes to it. Local-deck mode is unchanged; the two coexist.

### Hooks on the remote

`dot-agent-deck hooks install` already writes per-agent hook scripts pointing at a local Unix socket. On the remote, the same install runs and points at the local daemon socket. No code change to the hooks themselves; they're agnostic to local-vs-remote.

### Documentation deliverables

- `docs/remote-environments.md` (new) — what a remote environment is, the lifecycle model, Ctrl+W vs detach, failure modes.
- `docs/remote-requirements.md` (new) — exact environment requirements (Linux distro, ssh access, disk, RAM, egress, optional container runtime). This doc is what the maintainer follows to set up the dev/test VM, and what users follow to set up theirs.
- `docs/remote-recipes.md` (new) — copy-pasteable provisioning snippets for multipass, Hetzner, fly, k3s. Maintenance burden kept low: each recipe is a few commands, no abstractions.
- Update `docs/getting-started.mdx` and `docs/installation.md` to mention the remote option.

## Success Criteria

- A user with an empty laptop and a fresh remote box (provisioned via the recipes doc) can run `dot-agent-deck remote add hetzner-1`, `dot-agent-deck connect hetzner-1`, start an agent, and have that agent survive `Ctrl+Z`-ing the laptop, closing the lid, and reconnecting from a different network — within 24 hours, the agent is still there with full scrollback.
- `Ctrl+W` on a remote agent pane stops the agent process on the remote (verified via `ps` on the remote: the process is gone). It does **not** also kill the daemon or the environment.
- Detach (separate keybinding) leaves the agent running and merely disconnects the viewer.
- `remote list` shows last-known status; `connect` distinguishes the three failure modes (unreachable / binary missing / no agents) with actionable messages.
- A user with `kubectl` access to a cluster can run `remote add --type=kubernetes` and get the same UX as the ssh path; the only thing that changes is the transport.
- The maintainer's own dev/test VM is provisioned by following `docs/remote-requirements.md` from a clean box, with no out-of-band steps. Anything missing from the docs is a docs bug, fixed before merge.
- Hook callbacks fired during a viewer disconnect are reflected in the viewer state on reattach (the Decision Log / pane events ledger has them).
- Existing local-deck flow is unchanged. Users who never run `remote add` see no behavioral difference.

## Milestones

### Phase 0: Test environment and requirements doc

- [x] **M0.1** — Draft `docs/remote-requirements.md` from scratch based on what the daemon will need (best guess; will be refined as M1+ exposes real requirements).
- [x] **M0.2** — Maintainer provisions a personal dev/test VM by following the draft requirements doc end-to-end. **Not** committed to the repo. Anything that had to be done out-of-band is a docs gap to fix.
- [x] **M0.3** — Refine the requirements doc until a clean re-provision works without consulting any other source.

### Phase 1: Daemon owns PTYs (local-only)

- [x] **M1.1** — Refactor `src/daemon.rs` to own agent PTYs (currently spawned by the TUI). Local-deck mode keeps working; PTY ownership simply moves. Existing tests pass.
- [x] **M1.2** — Define and implement the streaming attach protocol (over Unix socket initially): list-agents, start-agent, stop-agent, attach-stream, detach, snapshot.
- [x] **M1.3** — TUI viewer can attach to its own local daemon over the new protocol (still all on one machine). This proves the protocol works before any network is involved.

### Phase 2: ssh transport (MVP remote)

- [ ] **M2.1** — `dot-agent-deck daemon attach` subcommand: speaks the protocol over stdio.
- [ ] **M2.2** — `remote add --type=ssh` command: verifies ssh, installs binary on the remote, runs `hooks install`, writes registry entry.
- [ ] **M2.3** — `remote list`, `remote remove`, `remote upgrade` commands.
- [ ] **M2.4** — `connect [name]` command: picker, ssh exec into the daemon, viewer attaches.
- [ ] **M2.5** — Lifecycle: `Ctrl+W` stops remote agent; explicit detach keybinding leaves it running; laptop sleep = implicit detach. End-to-end verified on the dev/test VM.
- [ ] **M2.6** — Failure-mode-aware connect: distinguishes the three error states with clear messages.

### Phase 3: Kubernetes transport

- [ ] **M3.1** — Daemon container image (Dockerfile in-repo, builds on existing `Dockerfile` work).
- [ ] **M3.2** — Opinionated manifest (StatefulSet + PVC + Service) shipped in-repo, parameterized by name/namespace.
- [ ] **M3.3** — `remote add --type=kubernetes` command: verifies kubectl, applies manifest, waits for pod Ready, runs basic roundtrip.
- [ ] **M3.4** — `connect` over `kubectl exec` works equivalently to ssh path.
- [ ] **M3.5** — `remote upgrade` re-applies manifest with new image tag; data on PVC preserved.

### Phase 4: Quality

- [ ] **M4.1** — Tests for the streaming attach protocol (round-trip of all message types, disconnect/reconnect, partial frames).
- [ ] **M4.2** — Integration test: spin up the daemon locally, attach, start an agent, detach, reattach, stop. End-to-end on a single machine.
- [ ] **M4.3** — Manual end-to-end validation on dev/test VM **and** on a Kubernetes cluster (kind or k3s on the dev VM is fine).

### Phase 5: Documentation and release

- [ ] **M5.1** — `docs/remote-environments.md`: lifecycle model, Ctrl+W vs detach semantics, failure modes, hook-on-remote behavior.
- [ ] **M5.2** — `docs/remote-recipes.md`: provisioning snippets for multipass / Hetzner / fly / k3s.
- [ ] **M5.3** — Final pass on `docs/remote-requirements.md` reflecting anything learned in M1–M4.
- [ ] **M5.4** — Update `docs/getting-started.mdx` and `docs/installation.md` to mention the remote path.
- [ ] **M5.5** — Changelog fragment, release.

## Key Files

- `src/daemon.rs` — extended to own PTYs and serve the streaming attach protocol.
- `src/embedded_pane.rs` — PTY spawn paths refactored to be daemon-owned in remote mode.
- `src/ui.rs`, `src/state.rs` — viewer integration; remote panes are stream-backed.
- `src/hook.rs` — unchanged; verifies it works pointing at the daemon's socket on the remote.
- `src/main.rs` — wire new subcommands.
- `src/remote.rs` (new) — `remote add | list | remove | upgrade | connect` implementations and the `~/.config/dot-agent-deck/remotes.toml` registry.
- `src/protocol.rs` (new, or inside `daemon.rs`) — streaming attach protocol types and codec.
- `Dockerfile` — daemon image (extends or reuses existing image work if present).
- `deploy/k8s/` (new) — opinionated manifest shipped for `remote add --type=kubernetes`.
- `docs/remote-environments.md` (new), `docs/remote-requirements.md` (new), `docs/remote-recipes.md` (new).
- `docs/getting-started.mdx`, `docs/installation.md` — minor cross-references.

## Design Decisions

### 2026-05-08: Long-running per-project environment, not pod-per-agent

Considered three models: VM-per-environment, pod-per-agent (ephemeral), and pod-per-environment (long-running). The product requirement "feels local" is decisive: shared filesystem, warm caches, ability to stop one agent and start another in the same in-progress state are all properties of a long-running environment. Pod-per-agent is the right shape for ephemeral CI runners, the wrong shape for a remote dev box. VM and long-running pod are isomorphic for this purpose; transport is a config detail.

### 2026-05-08: Daemon-on-remote, not reverse tunnel

The deck already has a daemon (`src/daemon.rs`) that ingests hook events. Extending it to also own PTYs and live on the remote unifies the architecture: hooks reach a local-on-remote socket, no tunnels, no laptop dependency, no NAT punching. Reverse tunnels were the alternative — rejected because they tie the hook target's lifetime to the laptop's network presence, which defeats the persistence goal.

### 2026-05-08: Provisioning is documented, not productized

Considered building VM creation (`dot-agent-deck remote create-vm --provider=fly`). Rejected: cloud-provider matrix is a maintenance burden the project can't sustain (SDKs, regions, instance types, auth, billing, image rotation), and existing tools (multipass, fly, terraform) already do this well. The product accepts any environment that meets documented requirements; the docs ship recipes for common providers as starting points but no abstraction layer.

### 2026-05-08: Ctrl+W stops the agent (local or remote); detach is a separate action

Initial framing had close-deck-on-remote default to detach. Reversed after user feedback: keep the mental model uniform across local and remote. Ctrl+W means "I'm done with this agent" in both. Remote adds a *new* capability — explicit detach — that local doesn't need. This makes "keep it running while I disconnect" the opt-in path, which is the right default for safety (detach-by-default would mean a stray Ctrl+W leaves orphaned agents accumulating on the remote).

### 2026-05-08: Phase 0 is the test environment, set up by the docs

The first task is meta: the maintainer provisions a personal dev/test VM by following the same `docs/remote-requirements.md` that ships to users. The VM itself is not in-repo (no terraform, no scripts beyond what the docs say). This is a forcing function: if the docs aren't enough to set it up from a clean box, they're not enough for users. Re-validation by clean re-provision happens any time the requirements change.

## Open Decisions

To be resolved during implementation, not blocking PRD acceptance:

- **Daemon persistence layer**: `systemd --user` unit (VM-native, works without containers), container restart-policy (Kubernetes-native), or a deliberate supervisor (`abduco`-wraps-the-daemon, transport-agnostic). Likely answer: systemd for ssh transport, restart-policy for Kubernetes transport — match each transport's idiom.
- **Attach protocol over the wire**: stdio-piped framed bytes (simple, works over ssh exec and kubectl exec) vs. structured RPC (gRPC, JSON-RPC). Likely answer: framed bytes, custom minimal protocol — gRPC drags in build complexity not justified at this scale.
- **Transport order in v1**: ship ssh first (smaller surface, no cluster dependency for testing), then Kubernetes; or both at once. Likely answer: ssh first (Phase 2), Kubernetes follows (Phase 3).
- **Project filesystem on Kubernetes**: PVC per environment with git as the sync layer (recommended) vs. ephemeral with init-container clone vs. mounted ConfigMap-style. Likely answer: PVC + git.
- **Picker UX in `connect`**: TUI list inside `dot-agent-deck` (consistent with rest of app) vs. shell-out to `fzf` (zero-effort, requires fzf installed). Likely answer: in-app TUI, fzf as fallback if not interactive.
- **Daemon-side socket file permissions**: ~~`src/daemon.rs` currently binds the Unix socket without an explicit `set_permissions` / `chmod`, so the socket file mode follows the process umask (typically world-readable/connectable). M0.1 docs work around this with a recommended `umask 077` instruction for shared hosts; daemon-side enforcement (set umask before bind, or chmod immediately after) should land in Phase 1 alongside the daemon-owns-PTYs work.~~ **Resolved in M1.1** — the daemon now sets `umask 0o177` (serialized via `UMASK_LOCK`) before `UnixListener::bind` so the socket inode is created at mode `0o600` directly, with a post-bind `chmod` retained as defense-in-depth. Verified by `daemon::tests::socket_is_0600_immediately_after_bind`.

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Streaming attach protocol design is over- or under-engineered, requires churn after Phase 2 | Start with the simplest framing (length-prefixed JSON or msgpack frames) on Unix socket in Phase 1; only generalize to network-grade in Phase 2 once real ssh-exec usage exposes problems. Version the protocol from day one. |
| Version drift between client and remote daemon causes wire-format breakage | `connect` performs a version handshake; on mismatch, refuses with a clear `remote upgrade` recommendation. Hook event schema (`src/event.rs`) is the highest-risk surface — version-tag it. |
| Hooks fire during disconnect and are silently lost | Daemon ingests hooks regardless of viewer presence (already true today). On reattach, viewer requests a state snapshot. Bound the buffer (lines, bytes, or "since last attach") and document the limit. |
| Daemon crash on the remote leaves orphaned agent processes | Daemon spawns agents with explicit process-group IDs and tracks them on disk; on restart, the daemon reconciles (reattach to existing PTYs where possible, mark unattachable as crashed). |
| `kubectl exec` keystroke latency is too high to feel local | Document the RTT-bound nature; recommend co-located clusters; do not over-promise. Mosh-style UDP is **not** in scope — users with high-latency setups should run the deck on the remote and ssh into it (the original "run app on remote" pattern). |
| Docs drift from reality once dev VM is set up | M0.3 explicitly requires a clean re-provision from docs alone. Repeat any time `remote-requirements.md` changes. |
| The Kubernetes manifest shipped in-repo gets stale or doesn't fit users' clusters | Manifest is a starting point, documented as such. Users can apply their own manifest and point `remote add` at the resulting pod. |
| Local-deck users see regressions from the daemon refactor | Phase 1 is gated on existing local tests passing. Daemon-owns-PTYs change is observable only in remote mode; local-deck PTY spawning path stays.|

## References

- PRD #58 (multi-role agent orchestration) — `prds/58-multi-role-agent-orchestration.md`
- Existing daemon: `src/daemon.rs`
- Existing PTY ownership: `src/embedded_pane.rs`
- Existing hooks: `src/hook.rs`, `src/hooks_manage.rs`
- Existing event schema (version-risk surface): `src/event.rs`
