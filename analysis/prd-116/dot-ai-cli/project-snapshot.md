# Project snapshot — `~/code/dot-ai-cli/` (PRD #116 baseline input)

Captured for the baseline-regeneration procedure (M2.1), same role as the pilot's snapshot:
the "project laid out" that stands in for the agent's own exploration, since the reproducible
regeneration runs the model **single-shot with filesystem tools disabled**. Carries the same
step-1 discovery signals the prompt probes for.

## Top-level entries

```
cmd/               # Go command entrypoints
internal/          # Go packages
e2e/               # integration tests (//go:build integration)
main.go
go.mod, go.sum     # Go module  -> this is a Go project
CLAUDE.md
devbox.json        # reproducible-env manifest  -> init_command
devbox.lock
docker-compose.yml
docs/
changelog.d/       # changelog fragments (release flow)
openapi.json
prds/              # spec directory
README.md
renovate.json
routing-skill.md
Taskfile.yml       # go-task task runner (build/test/release)
tmp/               # test output redirect target (per CLAUDE.md)
.claude/           # skills/ (no commands/)
.github/workflows/ # ci.yaml, docs.yaml, release.yaml
.mcp.json
```

This is a **Go project** (`go.mod`, `main.go`, `cmd/`, `internal/`). A CLI client for the
dot-ai MCP server. Build/test go through `task` (go-task).

## `devbox.json` (verbatim)

```json
{
  "$schema": "https://raw.githubusercontent.com/jetify-com/devbox/0.16.0/.schema/devbox.schema.json",
  "packages": [
    "nushell@0.108.0", "kubernetes-helm@3.19.1", "git@2.51.2", "vals@0.42.6",
    "kind@0.31.0", "kubectl@1.33.4", "gh@2.83.1", "go-task@3.48.0"
  ],
  "shell": {
    "init_hook": [
      "export PATH=\"$HOME/.local/bin:$PATH\"",
      "[ -n \"$USE_VALS\" ] && eval \"$(vals env -export -f .env.vals.yaml)\" || true",
      "[ -f .env ] && source .env || true"
    ],
    "scripts": {
      "agent":              ["claude --continue"],
      "agent-new":          ["claude"],
      "agent-orchestrator": ["claude --model opus"],
      "agent-coder":        ["claude --model opus"],
      "agent-reviewer":     ["claude --model opus"],
      "agent-auditor":      ["claude --model opus"],
      "agent-releaser":     ["claude --model sonnet"]
    }
  }
}
```

**Toolchain (from devbox):** `go-task` (`task`), `git`, `gh`, `helm`, `kubectl`, `kind`,
`vals`, `nushell` (`nu`). Go itself is assumed on PATH (used by `task build`/`task test`).
**Agent launchers (clean per-role set):** `devbox run agent-orchestrator` (opus), `devbox run
agent-coder` (opus), `devbox run agent-reviewer` (opus), `devbox run agent-auditor` (opus),
`devbox run agent-releaser` (sonnet), plus generic `agent`/`agent-new`. There is a dedicated
launcher for each of orchestrator/coder/reviewer/auditor/release.

## `CLAUDE.md` (key sections)

> **Testing.** Always redirect test output to `./tmp/test-output.txt`:
> `mkdir -p tmp && task test > tmp/test-output.txt 2>&1`, then check the last 30 lines.
> **Integration tests.** All tests are integration tests using the `//go:build integration`
> tag, run via `task test` (which starts/stops the mock server automatically). Tests use the
> binary-subprocess pattern. The mock server (`ghcr.io/vfarcic/dot-ai-mock-server`) provides
> fixtures — prefer integration tests over inline `httptest` unit tests.

## Task / build commands (`Taskfile.yml`)

```
task fetch-spec       # Fetch OpenAPI spec from the dot-ai server repo
task mock-up/mock-down # Start/stop mock server for integration tests
task build            # Build binary for current platform
task build-all        # Cross-compile all platforms
task test             # Run integration tests (starts mock server automatically)
task checksums, homebrew-formula/publish, scoop-manifest/publish  # release packaging
```

Read-only / safe dev commands: `task test`, `task build`, `task build-all`, `task fetch-spec`,
`task checksums`; `go test|vet|build|fmt`; `git status|log|diff|show`.

## `.claude/skills/` (coordination skills available to an orchestrator)

PRD/coordination skills present (prefix `dot-ai-`): `/dot-ai-prd-next`,
`/dot-ai-prd-update-progress`, `/dot-ai-prds-get`, `/dot-ai-prd-start`, `/dot-ai-prd-done`,
`/dot-ai-prd-full`, `/dot-ai-prd-create`, `/dot-ai-prd-close`, `/dot-ai-tag-release`,
`/dot-ai-changelog-fragment`, `/dot-ai-worktree-prd`, `/dot-ai-write-docs`,
`/dot-ai-query-dot-ai`, `/dot-ai-request-dot-ai-feature`, `/dot-ai-process-feature-request`,
`/dot-ai-generate-cicd`, `/dot-ai-generate-dockerfile`. No `.claude/commands/`.

## `.github/workflows/`

```
ci.yaml      # pull_request          -> PR CI gate
docs.yaml    # push                  -> docs
release.yaml # release / dispatch     -> release CI
```

Both a **PR CI gate** (`ci.yaml`) and a **release workflow** (`release.yaml`) exist, plus a
`/dot-ai-prd-done` skill — strong release-flow signals (propose a `release` role).

## Spec directory

```
prds/10-live-remediation-dashboard-tui.md
prds/done/
```
