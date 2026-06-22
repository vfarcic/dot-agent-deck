# Project snapshot — `~/code/dot-ai-infra/` (PRD #116 baseline input)

Captured for the baseline-regeneration procedure (M1.2). The deck's real config-gen
flow lets the agent **explore** the project with its own tools; because the
reproducible regeneration path runs the model **single-shot with filesystem tools
disabled** (so it can never touch the user's repo), this snapshot is the "project laid
out" that stands in for that exploration. It is the same set of signals the prompt's
step 1 tells the agent to probe for (build/task manifests, reproducible-env manifest,
agent launchers, slash commands/skills, spec dir, CI configs, infra).

For Phase 1 this snapshot was captured by hand from the commands shown below; the
standalone, fully-documented re-run procedure is a Phase-3 deliverable (M3.3, under
`docs/develop/`). The exact reproduction steps live in
[`analysis.md`](analysis.md#exact-reproduction).

## Top-level entries

```
apps/              # Argo CD Application manifests (GCP/Crossplane/external-secrets/gateway)
apps-youtube/      # Argo CD Application manifest for youtube-automation
argocd/            # Argo CD self-management (app.yaml, app-youtube.yaml)
argocd-values.yaml
.claude/           # commands/, hooks/, skills/, settings.json
CLAUDE.md
devbox.json        # reproducible-env manifest  -> init_command
devbox.lock
.dot-agent-deck.toml   # (the user-improved config under analysis)
dot.nu             # Nushell task-runner entry point
.env.vals.yaml     # vals-managed secrets
examples/
gcloud/            # flake.nix (gcloud SDK)
.github/workflows/ # create-solution.yaml
kubeconfig.yaml
.mcp.json, .mcp-kubernetes.json
prds/              # spec directory (107-, 129-, done/21-)
renovate.json
scripts/           # *.nu Nushell modules
```

No `README`. No `package.json` / `go.mod` / `Cargo.toml` / `Makefile` / `Taskfile.yml` —
this is **not** a code project; it is GitOps/Kubernetes infrastructure.

## `devbox.json` (verbatim)

```json
{
  "$schema": "https://raw.githubusercontent.com/jetify-com/devbox/0.16.0/.schema/devbox.schema.json",
  "packages": [
    "nushell@0.113.1",
    "kubernetes-helm@3.20.2",
    "git@2.54.0",
    "yq-go@4.53.2",
    "vals@0.44.0",
    "kubectl-tree@0.6.0",
    "linode-cli@5.56.2",
    "path:gcloud#google-cloud-sdk",
    "viddy@1.3.0",
    "kubectl@1.36.1"
  ],
  "shell": {
    "init_hook": [
      "export PATH=\"$HOME/.local/bin:$PATH\"",
      "[ -n \"$USE_VALS\" ] && eval \"$(vals env -export -f .env.vals.yaml)\"",
      "[ -f .env ] && source .env"
    ],
    "scripts": {
      "agent": ["claude --continue"]
    }
  }
}
```

**Toolchain (from devbox):** `nu` (nushell), `helm`, `git`, `yq`, `vals`,
`kubectl-tree` (a.k.a. `kubectl tree`), `linode-cli`, `gcloud`, `viddy`, `kubectl`.
**Agent launcher (the project's only one):** devbox script `agent` → `claude --continue`,
i.e. invocation form `devbox run agent`.

## `CLAUDE.md` (key sections)

> GitOps infrastructure repository for deploying the **dot-ai** platform on GKE. Uses
> Argo CD for continuous deployment. Key tech: Kubernetes/GKE, Argo CD, Helm, Nushell
> (`.nu` files), External Secrets + vals, Gateway API, Grafana Cloud + Alloy.

Common commands are all run via `dot.nu` with Nushell, e.g. `nu dot.nu setup`,
`nu dot.nu destroy`.

## `dot.nu` task-runner subcommands (discovered)

```
nu dot.nu setup
nu dot.nu destroy
nu dot.nu apply argocd | certmanager | clusterissuer | dot-ai | dot-ai-controller
nu dot.nu create kubernetes_creds
nu dot.nu destroy kubernetes
nu dot.nu packages kubernetes
nu dot.nu get provider | ingress
nu dot.nu print source
nu dot.nu delete temp_files
```

These are **infrastructure lifecycle / mutating** operations (create cluster, apply,
destroy) — none is a read-only "test"/"build"/"lint" task suitable for a reactive rule
or a CI gate.

## `.claude/commands/`

```
trace-request-flow-dot-ai-ui.md
```

## `.claude/skills/` (coordination skills available to an orchestrator)

```
dot-ai, dot-ai-changelog-fragment, dot-ai-generate-cicd, dot-ai-generate-dockerfile,
dot-ai-impact_analysis, dot-ai-manageKnowledge, dot-ai-manageOrgData, dot-ai-operate,
dot-ai-prd-close, dot-ai-prd-create, dot-ai-prd-done, dot-ai-prd-full, dot-ai-prd-next,
dot-ai-prds-get, dot-ai-prd-start, dot-ai-prd-update-decisions, dot-ai-prd-update-progress,
dot-ai-process-feature-request, dot-ai-projectSetup, dot-ai-query, dot-ai-query-dot-ai,
dot-ai-recommend, dot-ai-remediate, dot-ai-request-dot-ai-feature, dot-ai-tag-release,
dot-ai-users, dot-ai-version, dot-ai-worktree-prd, dot-ai-write-docs
```

PRD/coordination skills present: `/dot-ai-prd-next`, `/dot-ai-prd-update-progress`,
`/dot-ai-prds-get`, `/dot-ai-prd-done`, `/dot-ai-tag-release`,
`/dot-ai-changelog-fragment`.

## `.github/workflows/`

```
create-solution.yaml   # workflow_dispatch (manual, Port-triggered solution creation)
```

This is a **manual `workflow_dispatch`** workflow, **not** a PR/push CI gate and **not** a
release workflow. There is no automated PR validation or release-CI signal in this repo.

## Spec directory

```
prds/107-vpa-inplace-resizing.md
prds/129-grafana-cloud-alloy-migration.md
prds/done/21-gke-to-linode-migration.md
```
