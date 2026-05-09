# PRD #80: Remote Kubernetes Transport

**Status**: Not started
**Priority**: Medium
**Created**: 2026-05-09
**GitHub Issue**: TBD (create with `gh issue create` when work begins)
**Depends on**: PRD #76 (Remote Agent Environments — ssh transport, daemon protocol, registry)

## Problem Statement

PRD #76 introduces remote agent environments with the ssh-to-VM transport as the v1 path. The daemon protocol, registry shape, and CLI surface are designed to support a Kubernetes (`kubectl exec`) transport with no protocol-layer changes — but the actual K8s plumbing (image, manifest, `remote add --type=kubernetes`, `connect` over `kubectl exec`, PVC-preserving upgrade) was deferred so the ssh MVP could ship first. This PRD picks that work back up.

Users with `kubectl` access to a cluster should be able to run `dot-agent-deck remote add --type=kubernetes` and get the same UX as the ssh path, with the only difference being how bytes traverse the wire.

## Solution Overview

Add `type = "kubernetes"` as a second transport in `dot-agent-deck remote`. An opinionated manifest (StatefulSet + PVC + Service) ships in-repo, parameterized by name/namespace; a daemon container image (versioned to match the CLI) is the StatefulSet's container; project state lives on the per-environment PVC; `connect` uses `kubectl exec -- dot-agent-deck daemon attach` instead of `ssh`.

Architecturally this is a transport-only addition. The streaming attach protocol, the registry file format, the lifecycle semantics (Ctrl+W vs detach), and the failure-mode-aware connect UX all carry over from PRD #76 unchanged.

## Scope

### In Scope

- Daemon container image (Dockerfile in-repo).
- Opinionated Kubernetes manifest (StatefulSet + PVC + Service) shipped in `deploy/k8s/`, parameterized by name/namespace.
- `dot-agent-deck remote add --type=kubernetes --context=<ctx> --namespace=<ns>` — verifies kubectl, applies manifest, waits for pod Ready, runs basic protocol roundtrip.
- `dot-agent-deck connect <name>` over `kubectl exec` for kubernetes-typed entries (the picker already handles the type from PRD #76).
- `dot-agent-deck remote upgrade <name>` for kubernetes-typed entries: re-applies manifest with new image tag; data on PVC preserved.
- `dot-agent-deck remote remove <name>` for kubernetes-typed entries: registry-only (consistent with the ssh path); manifest teardown is documented as the user's responsibility (`kubectl delete`).
- Registry schema extension: `type = "kubernetes"` with `context`, `namespace`, `install_image` fields (the schema is forward-compatible from PRD #76).
- Documentation: `docs/remote-recipes.md` k3s recipe; `docs/remote-environments.md` Kubernetes section; updates to `docs/remote-requirements.md` for the cluster path.
- Manual end-to-end validation on a Kubernetes cluster (kind or k3s on the dev VM is fine).

### Out of Scope

- ssh-to-VM transport (already in PRD #76).
- Helm chart (raw manifest is enough for v1; chart can land later).
- Operator / CRD-based provisioning.
- Cluster-side multi-tenancy beyond namespace separation.
- Service mesh integration.
- Auto-scaling the StatefulSet (single replica per environment by design).

## Technical Approach

### Image

`Dockerfile` (in-repo) builds a small image whose ENTRYPOINT is `dot-agent-deck daemon` (the persistent daemon, not the one-shot `daemon attach`). Image tag matches the `dot-agent-deck` version (`DAD_VERSION`). Image hosted at `ghcr.io/vfarcic/dot-agent-deck:<version>`.

### Manifest

`deploy/k8s/dot-agent-deck.yaml` — opinionated StatefulSet + PVC + Service + ConfigMap (for daemon config). Parameterized via a tiny templating layer (envsubst / sed at apply time, not Helm). Fields the user can override: name, namespace, image tag, PVC size, resource limits.

The Pod's container runs the daemon image. The PVC is mounted at `/workspace`. Hooks install at the same per-user path the ssh transport uses, but inside the container's filesystem.

### `remote add --type=kubernetes`

- Validate `--context` and `--namespace` arguments (must be non-empty; pass-through to `kubectl --context X --namespace Y`).
- Run `kubectl --context X --namespace Y version --short` to verify reachability.
- Render the manifest with the user's parameters and apply via `kubectl --context X --namespace Y apply -f -` (stdin, no temp file).
- Wait for the StatefulSet's pod to become Ready (poll with backoff, timeout configurable).
- Run a basic protocol roundtrip: `kubectl exec <pod> -- dot-agent-deck daemon attach` + send list-agents request + assert the empty-list response.
- Write the registry entry.

### `connect <name>` over `kubectl exec`

- The PRD #76 `connect` picker already routes by type. Kubernetes entries currently emit "Phase 3 not yet supported" — replace that with the kubectl-exec path.
- The bridge architecture from M2.4 carries over: spawn `kubectl --context X --namespace Y exec <pod> -- dot-agent-deck daemon attach` instead of ssh; bridge the local Unix socket; TUI mode-gating via `DOT_AGENT_DECK_VIA_DAEMON=1` is unchanged.
- Failure modes (PRD #76 M2.6) extend with kubectl-specific cases: invalid kubeconfig, context not found, pod not Ready, image pull failure.

### `remote upgrade <name>` for kubernetes type

- Re-apply the manifest with the new image tag.
- The StatefulSet rolls the pod; the PVC is reused so project state on `/workspace` is preserved.
- Update the registry's `version` and `upgraded_at` fields (the ssh path already does this; same code path for kubernetes type once the kubectl-apply step replaces the ssh-install step).

## Success Criteria

- A user with `kubectl` access to a cluster can run `dot-agent-deck remote add --type=kubernetes --context=<ctx> --namespace=<ns>` and get the same UX as the ssh path; the only thing that changes is the transport.
- `dot-agent-deck connect <name>` against a kubernetes-typed entry attaches via `kubectl exec`, the TUI shows running agents, Ctrl+W stops them, detach leaves them running — identical UX to the ssh transport.
- `dot-agent-deck remote upgrade <name> --version V` re-applies the manifest with image tag `:V`; the pod rolls; project state on `/workspace` survives the roll.
- The maintainer validates the full flow on a kind or k3s cluster running on the dev/test VM.

## Milestones

### Phase 1: Image & manifest

- [ ] **M1.1** — Daemon container image (Dockerfile in-repo, multi-stage build, distroless base).
- [ ] **M1.2** — Opinionated manifest (StatefulSet + PVC + Service + ConfigMap) shipped in `deploy/k8s/`, parameterized by name/namespace/image-tag/PVC-size.

### Phase 2: CLI integration

- [ ] **M2.1** — `remote add --type=kubernetes` command: verifies kubectl, applies manifest, waits for pod Ready, runs basic roundtrip, writes registry entry.
- [ ] **M2.2** — `connect <name>` over `kubectl exec` works equivalently to ssh path (bridge architecture from PRD #76 M2.4 reused; ssh swapped for kubectl).
- [ ] **M2.3** — `remote upgrade <name>` for kubernetes-typed entries: re-applies manifest with new image tag; PVC data preserved.
- [ ] **M2.4** — Failure-mode-aware connect for kubectl: invalid kubeconfig, context not found, pod not Ready, image pull failure — each surfaces a distinct, actionable message.

### Phase 3: Validation & documentation

- [ ] **M3.1** — Manual end-to-end validation on kind or k3s cluster (on the dev/test VM is fine).
- [ ] **M3.2** — `docs/remote-recipes.md` k3s recipe.
- [ ] **M3.3** — `docs/remote-environments.md` Kubernetes section: `kubectl exec` transport, PVC lifecycle, image versioning.
- [ ] **M3.4** — `docs/remote-requirements.md` updates for the cluster path (kubectl version, RBAC, namespace permissions).
- [ ] **M3.5** — Changelog fragment, release.

## Key Files

- `Dockerfile` — daemon image (extends or reuses any existing image work).
- `deploy/k8s/` (new) — opinionated manifest.
- `src/remote.rs` — extends `RemoteEntry` schema with kubernetes fields; adds kubectl-apply path to `add` and `upgrade`.
- `src/connect.rs` — extends bridge to spawn `kubectl exec` instead of `ssh` when the entry's type is kubernetes.
- `src/main.rs` — wire the `--type=kubernetes` arg variant (already accepted by clap from PRD #76, just needs the impl behind it).
- `docs/remote-recipes.md`, `docs/remote-environments.md`, `docs/remote-requirements.md` — Kubernetes additions.

## Design Decisions

### 2026-05-09: Split Kubernetes transport into a separate PRD

Originally PRD #76 covered both ssh and Kubernetes transports as Phase 2 / Phase 3 of the same PRD. Splitting Kubernetes into a standalone PRD keeps the ssh MVP releasable without dragging the K8s deployment story (image, manifest, PVC, RBAC) along with it. The protocol layer is shared and was already designed for transport-agnosticism in PRD #76, so the split is clean.

### 2026-05-09: Manifest, not Helm chart, for v1

Helm adds a templating language and a chart-versioning lifecycle. For v1, a single parameterized YAML file applied via `kubectl apply -f -` is enough and keeps the dependency surface minimal. A Helm chart can land later if users ask.

## Open Decisions

To be resolved during implementation, not blocking PRD acceptance:

- **Daemon persistence inside the pod**: container restart policy (`Always`) is the default; the daemon needs to gracefully reconcile agent PTYs on restart. This is shared with PRD #76's ssh path's daemon-restart story; verify the same code path works inside a container.
- **Image registry**: ghcr.io is the default. Whether to also publish to docker.io / quay.io is a docs question, not an implementation one.
- **Namespace defaulting**: if `--namespace` is omitted, do we default to `default`, to `dot-agent-deck`, or require explicit input? Likely require explicit; less surprise.
- **PVC size default**: 10 GiB? 50 GiB? Leave it parameterized with a default in the manifest; users can override.
- **Resource limits**: no defaults vs. modest defaults. Likely modest defaults (1 CPU / 1 GiB request, no limit) — agents are bursty and limits cause OOM-kill surprises.

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Manifest drifts from production-realistic defaults | Validate on kind / k3s during M3.1; document any cluster-specific assumptions. |
| Image build adds CI time | Multi-stage build, ship binary into a distroless base. CI builds only on tag, not on every PR. |
| `kubectl exec` stream semantics differ subtly from ssh stdio (e.g., signal propagation, EOF) | Reuse PRD #76 M2.1 stdin-EOF-propagation tests against `kubectl exec` early; surface any divergence in M2.2. |
| PVC migration / sizing problems on upgrade | Document that PVC resize is the user's responsibility (StorageClass-dependent); upgrade only changes image tag, not PVC spec. |
