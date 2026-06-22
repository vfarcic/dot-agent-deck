# Project snapshot — `~/code/dot-agent-deck/` (PRD #116 baseline input)

Captured for the baseline-regeneration procedure (M2.1), same role as the pilot's
snapshot: the "project laid out" that stands in for the agent's own exploration, since the
reproducible regeneration runs the model **single-shot with filesystem tools disabled** (so
it can never touch the user's repo). It carries the same step-1 discovery signals the prompt
probes for (build/task manifests, reproducible-env manifest, agent launchers,
slash commands/skills, spec dir, CI configs).

## Top-level entries

```
assets/            # embedded prompt + role library (config_gen_prompt.md, roles.toml)
audit/
bacon.toml
build.rs
Cargo.toml         # Rust crate  -> this is a Rust project
Cargo.lock
CHANGELOG.md, changelog.d/   # changelog fragments (release flow)
CLAUDE.md
CONTRIBUTING.md
devbox.json        # reproducible-env manifest  -> init_command
devbox.lock
docs/, site/       # Docusaurus docs site
examples/
gcloud/            # flake.nix (gcloud SDK)
greptile.json      # Greptile automated PR reviewer config
opencode.json      # opencode agent config (alternative CLI)
AGENTS.md          # agent instructions (opencode/codex convention)
prds/              # spec directory (30 PRD specs)
pyproject.toml     # python tooling for docs/site helpers
renovate.json
scripts/
src/, tests/, xtask/
Taskfile.yml       # go-task task runner (docs/reel/release/packaging tasks)
.claude/           # skills/ (no commands/)
.github/workflows/ # ci.yml, release.yml, docs*.yml, labeler.yml, stale.yml
.mcp.json
```

This is a **Rust project** (`Cargo.toml`, `src/`, `tests/`, `xtask/`). Primary toolchain is
Cargo + cargo-nextest; `Taskfile.yml` covers only docs-site, demo-reel, and release
packaging chores (no `test`/`build`/`lint` task — those go through Cargo directly).

## `devbox.json` (verbatim)

```json
{
  "$schema": "https://raw.githubusercontent.com/jetify-com/devbox/0.16.0/.schema/devbox.schema.json",
  "packages": [
    "vals@0.43.7", "go-task@3.48.0", "kubernetes-helm@3.19.1", "git@2.53.0",
    "upcloud-cli@3.29.0", "gh@2.92.0", "asciinema@3.2.0", "cargo-nextest@0.9.137",
    "rustc@1.94.1", "cargo@1.94.1", "clippy@1.94.1", "rustfmt@1.94.1",
    "path:gcloud#google-cloud-sdk", "asciinema-agg@1.9.0", "ffmpeg@8.1",
    "jq@1.8.1", "curl@8.17.0"
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
      "agent-reviewer":     ["claude --model opus"],
      "agent-reviewer-oc":  ["opencode --model openai/gpt-5.5"],
      "agent-auditor":      ["claude --model opus"],
      "agent-auditor-oc":   ["opencode --model openai/gpt-5.5"],
      "agent-tester":       ["claude --model opus"],
      "agent-release":      ["claude --model sonnet"],
      "oc-release":         ["opencode --model openai/gpt-5.4-mini"],
      "oc-kimi":            ["opencode --model openrouter/moonshotai/kimi-k2.6"]
    }
  }
}
```

**Toolchain (from devbox):** `cargo`/`rustc`/`clippy`/`rustfmt`, `cargo-nextest`, `go-task`
(`task`), `helm`, `git`, `gh`, `vals`, `jq`, `curl`, `asciinema`/`agg`/`ffmpeg` (demo-reel),
`upcloud-cli`, `gcloud`.
**Agent launchers (rich, role-specific):** dedicated devbox scripts exist for each role —
`devbox run agent-orchestrator` (opus), `devbox run agent-coder` (opus), `devbox run
agent-reviewer` (opus), `devbox run agent-auditor` (opus), `devbox run agent-tester` (opus),
`devbox run agent-release` (sonnet) — plus generic `agent`/`agent-new`/`agent-big`/`agent-small`
and `opencode`-backed variants (`agent-reviewer-oc`, `oc-release`).

## `CLAUDE.md` (key sections)

> dot-agent-deck is a Rust TUI that runs multiple agent CLIs in panes with a daemon
> backend. Tests are tiered: `cargo test-fast` (alias `cargo nextest run`) is the per-task
> fast tier (protocol/state + L1 widget/render tests); `cargo test-e2e` (alias `cargo nextest
> run --features e2e`) adds the L2 PTY/real-agent suite, run only before the release flow.
> `cargo fmt --check` and `cargo clippy -- -D warnings` are mandatory before every commit.

Other conventions: TUI test harness L1 (`tests/render_*.rs`, insta) / L2 (`tests/e2e_*.rs`,
PTY+vt100); `#[spec]` test catalog in `tests/CATALOG.md`; **Greptile** is the only active
automated PR reviewer (`greptile.json`, `triggerOnUpdates: true`) — it posts an issue comment
from `greptile-apps`; CodeRabbit is NOT active here. Release/versioning is documented under
`docs/develop/`.

## Task / build commands

- Tests: `cargo test-fast` (fast tier), `cargo test-e2e` (pre-PR L2 tier).
- Lint/format: `cargo clippy -- -D warnings`, `cargo fmt --check`.
- `Taskfile.yml` tasks: `docs-install/dev/build/serve`, `reel-smoke`, `reel-adapter-test`,
  `checksums`, `homebrew-formula/publish`, `scoop-manifest/publish` (packaging + docs only).

## `.claude/skills/` (coordination skills available to an orchestrator)

PRD/coordination skills present (prefix `dot-ai-`): `/dot-ai-prd-next`,
`/dot-ai-prd-update-progress`, `/dot-ai-prds-get`, `/dot-ai-prd-start`, `/dot-ai-prd-done`,
`/dot-ai-prd-full`, `/dot-ai-tag-release`, `/dot-ai-changelog-fragment`, plus project skills
`demo-reel`, `demo-reel-adapter`, `run-dot-agent-deck`, `publish-docs`, and the full
`dot-ai-*` operations suite. No `.claude/commands/`.

## `.github/workflows/`

```
ci.yml           # pull_request + workflow_dispatch  -> PR CI gate
release.yml      # push / release / tags             -> release CI
docs.yml         # pull_request                      -> docs preview
docs-publish.yml # push / release / workflow_call    -> docs publish
labeler.yml, stale.yml
```

Both a **PR CI gate** (`ci.yml`) and a **release workflow** (`release.yml`) exist, plus a
`/dot-ai-prd-done` skill — strong release-flow signals (propose a `release` role).

## Spec directory

```
prds/  — 30 PRD specs (e.g. 116-analyze-improved-configs..., 180-..., 89-...), prds/done/
```
