# Project snapshot — `~/code/youtube-automation/` (PRD #116 baseline input)

Captured for the baseline-regeneration procedure (M2.1), same role as the pilot's snapshot:
the "project laid out" that stands in for the agent's own exploration, since the reproducible
regeneration runs the model **single-shot with filesystem tools disabled**. Carries the same
step-1 discovery signals the prompt probes for.

## Top-level entries

```
cmd/, internal/, pkg/, web/   # Go source
go.mod, go.sum                # Go module  -> this is a Go project
main entrypoint: cmd/youtube-automation
CLAUDE.md, GEMINI.md
devbox.json        # reproducible-env manifest  -> init_command
devbox.lock
Justfile           # just recipes (build/test)
Makefile           # make targets (build/version bump)
dot.nu             # minimal Nushell runner (setup)
docker-compose-dot-ai.yaml, Dockerfile
helm/, manuscript/, patterns/, scripts/, vendor/
openapi.yaml, index.yaml, settings.yaml
opencode.json      # opencode agent config (alternative CLI)
prds/              # spec directory (8 PRD specs)
README.md, renovate.json
.claude/           # skills/ + commands/ (analyze-titles.md)
.github/workflows/ # test.yml, release.yml
.mcp.json
```

This is a **Go project** (`go.mod`, `cmd/`, `internal/`, `pkg/`, `vendor/`). A YouTube release
automation tool with a Web UI. Build/test go through `just` and `make` (which call `go`).

## `devbox.json` (verbatim)

```json
{
  "packages": [
    "just@1.43.1", "git@2.51.2", "nushell@0.108.0", "vals@0.42.6",
    "kubernetes-helm@3.20.1"
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
      "agent-plan":         ["claude --model opus --permission-mode plan"],
      "agent-small":        ["claude --model haiku"],
      "agent-big":          ["claude --model opus"],
      "agent-orchestrator": ["claude --model opus"],
      "agent-coder":        ["claude --model opus"],
      "agent-reviewer":     ["opencode --model openrouter/openai/gpt-5.5"],
      "agent-auditor":      ["opencode --model openrouter/openai/gpt-5.5"],
      "agent-release":      ["claude --model sonnet"],
      "oc-release":         ["opencode --model openai/gpt-5.4-mini"],
      "oc-kimi":            ["opencode --model openrouter/moonshotai/kimi-k2.6"]
    }
  }
}
```

**Toolchain (from devbox):** `just`, `git`, `nushell` (`nu`), `vals`, `helm`. Go itself is
**not** a devbox package — it is assumed on the system PATH (the `Justfile`/`Makefile` recipes
invoke `go build`/`go test`).
**Agent launchers (rich, role-specific):** `devbox run agent-orchestrator` (opus), `devbox run
agent-coder` (opus), `devbox run agent-reviewer` (opencode gpt-5.5), `devbox run agent-auditor`
(opencode gpt-5.5), `devbox run agent-release` (sonnet), plus generic
`agent`/`agent-new`/`agent-big`/`agent-small`/`agent-plan`. There is a dedicated launcher for
each of orchestrator/coder/reviewer/auditor/release (reviewer & auditor use opencode).

## `CLAUDE.md` (key sections)

> **Building/Running.** `make build-local` / `just build-local` / `go build -o youtube-release
> ./cmd/youtube-automation`; `make build` for all platforms; `make clean` / `just clean`.
> **Testing.** `go test ./...`; `just test` (= `go test ./... -cover`); `./scripts/coverage.sh`
> for a detailed report; `go test ./internal/publishing/...` for a package; `go test -v -run
> TestUploadVideo ./internal/publishing/` for one function.

## Task / build commands

```
just:  default build build-local run test frontend-build build-full clean
make:  all clean build build-local frontend-build build-local-full + version-bump targets
go:    go test ./...   (unit tests)
```

Read-only / safe dev commands: `go test ./...`, `go build`, `go vet`, `just test`,
`git status|log|diff|show`.

## `.claude/skills/` (coordination skills available to an orchestrator)

PRD/coordination skills present: `/prd-next`, `/prd-update-progress`, `/prds-get`,
`/prd-start`, `/prd-done`, `/prd-full`, `/tag-release`, `/changelog-fragment`, `/worktree-prd`
(the repo exposes the `dot-ai-*` skill family; the user references them unprefixed in-config).
`.claude/commands/`: `analyze-titles.md`. Also `dot-ai`, `write-docs`.

## `.github/workflows/`

```
test.yml     # pull_request           -> PR CI gate (Test)
release.yml  # push / release / tags  -> release CI (Create Release and Upload Binaries)
```

Both a **PR CI gate** (`test.yml`) and a **release workflow** (`release.yml`) exist, plus a
`/prd-done` skill — strong release-flow signals (propose a `release` role).

## Spec directory

```
prds/  — 8 PRD specs (e.g. 333-thumbnail-analytics.md, 375-title-analysis-api-ui.md, ...),
prds/done/
```
