# Project snapshot — `~/code/dot-ai/` (PRD #116 baseline input)

Captured for the baseline-regeneration procedure (M2.1), same role as the pilot's snapshot:
the "project laid out" that stands in for the agent's own exploration, since the reproducible
regeneration runs the model **single-shot with filesystem tools disabled**. Carries the same
step-1 discovery signals the prompt probes for.

## Top-level entries

```
src/, tests/       # TypeScript source + tests (vitest)
packages/, shared-prompts/, prompts/   # prompts loaded dynamically (per CLAUDE.md)
mock-server/, eval/, schema/, scripts/
charts/, kind.yaml # Helm chart + Kind cluster config (integration tests)
Dockerfile, Dockerfile-qdrant, docker-compose-dot-ai.yaml
package.json, package-lock.json   # Node project  -> this is a TypeScript/Node project
eslint.config.js
CLAUDE.md
devbox.json        # reproducible-env manifest  -> init_command
devbox.lock
dot.nu             # Nushell infra task-runner (cluster setup/destroy)
docs/, dex-theme/, manuscript?
changelog.d/       # changelog fragments (release flow)
prds/              # spec directory (35 PRD specs)
pyproject.toml     # python helper tooling
renovate.json, server.json, openapi?
.claude/           # skills/ (no commands/)
.github/workflows/ # ci.yml, release.yml, scorecard.yml, labeler.yml, stale.yml, ...
.mcp.json
```

This is a **TypeScript / Node project** (`package.json`, `src/`, `tests/`, vitest). It is the
dot-ai MCP server. `dot.nu` is a Nushell runner for **infrastructure** lifecycle (cluster
setup/destroy), not for test/build.

## `devbox.json` (verbatim)

```json
{
  "$schema": "https://raw.githubusercontent.com/jetify-com/devbox/0.16.0/.schema/devbox.schema.json",
  "packages": [
    "nushell@0.112.2", "kubernetes-helm@3.20.2", "git@2.53.0", "vals@0.44.0",
    "yq-go@4.53.2", "kubectl-tree@0.6.0", "kind@0.31.0", "kubectl@1.36.0",
    "awscli2@2.34.24", "hadolint@2.14.0", "gh@2.92.0", "jq@1.8.1"
  ],
  "shell": {
    "init_hook": [
      "export PATH=\"$HOME/.local/bin:$PATH\"",
      "[ -n \"$USE_VALS\" ] && eval \"$(vals env -export -f .env.vals.yaml)\" || true",
      "[ -f .env ] && source .env || true"
    ],
    "scripts": {
      "agent":        ["claude --continue"],
      "agent-new":    ["claude"],
      "agent-tester": ["claude"],
      "agent-medium": ["claude --model sonnet"]
    }
  }
}
```

**Toolchain (from devbox):** `git`, `gh`, `helm`, `kubectl`, `kubectl-tree`, `kind`, `vals`,
`yq`, `hadolint`, `awscli2`, `nushell` (`nu`), `jq`. Node/npm assumed on PATH (the project's
build/test go through `npm`).
**Agent launchers (sparse):** only `devbox run agent` (claude --continue), `devbox run
agent-new` (claude), `devbox run agent-tester` (claude), `devbox run agent-medium` (claude
sonnet). There is **no** dedicated `agent-coder`/`agent-reviewer`/`agent-auditor` script — only
a generic `agent-new`, a `agent-tester`, and a lighter `agent-medium`.

## `CLAUDE.md` (key sections)

> **MANDATORY:** write integration tests for new functionality; run `npm run test:integration`
> (creates a Kind cluster) — a task is NOT complete with failing tests. `npm run test:unit` is
> the fast unit tier (vitest, no cluster). Run a specific test with `npm run test:integration
> <pattern>`. Long-running tests: redirect to `./tmp/test-output.log`, check the tail; teardown
> with `./tests/integration/infrastructure/teardown-cluster.sh` on success, keep the cluster on
> failure. **Never create branches directly — always use `/worktree-prd`.** All AI prompts live
> in `prompts/`, loaded dynamically. Always check for reusability before implementing.

So this project has a clear **two-tier test split**: fast `npm run test:unit` (vitest) and a
heavy, mandatory `npm run test:integration` (Kind cluster) that doubles as the PR gate.

## Task / build commands (`package.json` scripts, abridged)

```
test            -> npm run test:integration
test:unit       -> vitest (fast, no cluster)
test:integration-> ./tests/integration/infrastructure/run-integration-tests.sh (Kind)
test:integration <pattern>  # scoped run
build           -> tsc; lint -> eslint src/; format -> prettier; audit -> npm audit
```

Read-only / safe dev commands: `npm run test:unit`, `npm run lint`, `git status|log|diff|show`,
`kubectl get|describe|logs|top` (cluster inspection during integration runs).

## `.claude/skills/` (coordination skills available to an orchestrator)

PRD/coordination skills present (prefix `dot-ai-`): `/dot-ai-prd-next`,
`/dot-ai-prd-update-progress`, `/dot-ai-prds-get`, `/dot-ai-prd-start`, `/dot-ai-prd-done`,
`/dot-ai-prd-full`, `/dot-ai-tag-release`, `/dot-ai-changelog-fragment`, `/dot-ai-worktree-prd`,
plus `write-docs`, `infographic-generator`, `publish-mock-server`, and the full `dot-ai-*`
operations suite. No `.claude/commands/`.

## `.github/workflows/`

```
ci.yml          # pull_request + workflow_dispatch  -> PR CI gate (Pipeline & Security)
release.yml     # push / release / tags             -> release CI
scorecard.yml   # OpenSSF scorecard
labeler.yml, stale.yml, build-qdrant-test-image.yml, test-fork-pr.yml
```

Both a **PR CI gate** (`ci.yml`) and a **release workflow** (`release.yml`) exist, plus a
`/dot-ai-prd-done` skill — strong release-flow signals (propose a `release` role). The PR CI
runs the heavy integration suite (Kind cluster).

## Spec directory

```
prds/  — 35 PRD specs (e.g. 109-web-ui-mcp-interaction.md, 375-unified-knowledge-base.md, ...)
```
