# Contributing to dot-agent-deck

## Prerequisites

Enter `devbox shell` for the pinned toolchain — it provides `cargo-nextest` (test runner), `asciinema` (cast replay), and the rest of the project's CLI deps. Outside devbox, install nextest manually with `cargo install cargo-nextest --locked`. The `cargo test-fast` / `cargo test-e2e` aliases in `.cargo/config.toml` resolve through nextest; without it they error with `no such command: nextest`.

For `cargo test-e2e` chain-smoke tests you also need the agent CLIs (`claude` and `opencode`) installed locally and logged in — the tests skip with a specific reason if either is missing (per Decision 8).

## Snapshot review workflow

L1 widget/layout regressions are pinned by `insta` file snapshots under `tests/snapshots/`. When a PR's diff includes a new or modified `.snap` file, read the snapshot diff like a rendered screen — each line corresponds to one row of the dashboard's parsed grid. Accept the change only if the new rendering matches the catalog entry's prose; otherwise loop the change back to the author. Locally, `cargo insta review` walks pending diffs interactively.

## TDD loop

Fast tier (per-task gate):

```sh
cargo test-fast lifecycle_001     # filter to one test
cargo test-fast                   # run the full fast tier
```

E2e tier (local-only, pre-PR gate per Decision 8):

```sh
cargo test-e2e lifecycle_001
cargo test-e2e
```

For a watch loop, `bacon test-fast` (or `bacon test-e2e`) reruns on every save; press `f` to filter to currently-failing tests, `esc` to clear. Function names follow Decision 17's `<sub-area>_<NNN>_<suffix>` pattern, so the filter is unique by construction.

## How to add a new test

1. Pick an existing catalog ID in `prds/77-tui-testing-harness.md` under `## Test Case Catalog`, or add a new one (format: `<area>/<sub-area>/<NNN>`).
2. Write the test under `tests/render_<area>.rs` (L1) or `tests/e2e_<area>.rs` (L2), naming the function `<sub>_<NNN>_<short_suffix>` (Decision 17). Annotate with `#[spec("<area>/<sub>/<NNN>")]` from the `spec` dev-dep so the linkage check picks it up.
3. Add a `/// Scenario:` doc comment of 1–3 sentences to the test function describing what it does in plain English (Decision 30). Run `cargo xtask docs --tests` whenever you want to refresh the local rendered `.md` under `.dot-agent-deck/recordings/<test>/test.md` — it's a browsing aid (gitignored, regenerated like `cargo doc`), not a commit gate.
4. Run `cargo xtask linkage-check` locally — it verifies the annotation matches the catalog, the function name carries the required prefix, no raw `sleep` / fixed-count polling crept into `e2e_*.rs`, AND the Scenario doc comment exists + the docs generator succeeds against the current source + catalog (rule 7). If the new ID was previously on `xtask/linkage-check/m2.allowlist`, delete that line.

## Developer docs

Maintainer-facing references that are intentionally **not** published to the documentation site live under [`docs/develop/`](docs/develop/) (excluded from the Docusaurus build). They render as plain Markdown here on GitHub:

- [Experimental flag](docs/develop/experimental-flag.md) — gate in-flight, work-in-progress surfaces behind the `experimental` flag during development, so unfinished UI can merge to `main` without showing up in normal use.
- [Rendering contract](docs/develop/rendering-contract.md) — the four render-path invariants (single layout pass, layout-driven PTY size, 1:1 widget render, fixed resize sequencing) and the call sites that enforce them.
- [Demo reel](docs/develop/demo-reel.md) — turn a PRD's e2e test recordings into one narrated MP4 (title/description card, then the test running, repeated) and upload it unlisted to YouTube; covers the manifest contract, the `agg`/`ffmpeg`/`jq`/`curl` prerequisites, the one-time YouTube OAuth credential setup, local usage, and the orchestrator step.
- [Versioning and the "breaking" definition](docs/develop/versioning.md) — what "breaking" means here (the TUI↔daemon contract, including semantic breaks behind a stable wire), the `PROTOCOL_VERSION` floor and `.breaking.md` fragment, the `0.x` bump policy (breaking→minor, feature/bugfix→patch), and the cross-version manual-test discipline.
- [Agent adapters — adding a new agent](docs/develop/agent-adapters.md) — the contract for adding a new agent (PRD #20): the curated-registry design philosophy (runtime extensibility is a non-goal), the four shipped integration strategies (native-hooks/Claude, plugin/OpenCode, extension/Pi, wrapper/Codex), and a step-by-step "add an agent" checklist keyed to the real seams (`AgentType`, the `AgentSpec` registry entry, the wrapper `RuleSet`, `live_target`/`send_result`, badge colour, and the test ladder), worked end to end with Codex.
- [Pi orchestrator extension](docs/develop/pi-extension.md) — the bundled TypeScript extension that makes Pi a first-class agent (PRD #201): its native `delegate`/`work-done` tools, the Pi-event → status mapping, the additive `agent-event` CLI seam, `include_str!` materialization into Pi's extension dir, the hook-free-by-construction property, the rule-12 classification, and the `experimental` gating.
- [Agent-driven notifications — retired ntfy dogfood](docs/develop/notifications-dogfood.md) — historical note (retired 2026-07-28): the `scripts/notify.sh` + public-ntfy-topic dogfood that PRD #126 started from, what replaced it (the daemon's idle-worker detector plus an orchestrator-only Telegram recipe in `.dot-agent-deck.toml`, with workers escalating through `work-done` instead of notifying), and where the findings and the user-facing docs now live.
- [E2E temp directories](docs/develop/e2e-temp-dirs.md) — how the harness allocates scratch space and why it all nests under one per-process root (issue #322): the `atexit` cleanup and why a `static TempDir` silently leaked one dir per test even on green runs, `cargo xtask clean-e2e-tmp` for reaping what SIGKILLed runs left behind (and why `.tmp*` is opt-in), the free-space pre-flight that stops tmpfs exhaustion from masquerading as product regressions, and the `TMPDIR` / `DAD_E2E_MIN_FREE_MB` / `DAD_E2E_IMPORT_CLAUDE_PLUGINS` knobs.
- [Checking a Windows compile locally](docs/develop/windows-cross-check.md) — `scripts/windows-cross-check.sh` type-checks the workspace *including tests* for `x86_64-pc-windows-msvc` from a Linux host, closing the gap that makes CI's `build-windows` job the first thing to ever compile for Windows. Covers why the devbox toolchain can't do it, the compiler and archiver shims that keep devbox's `CC=gcc`/`AR=ar` from being handed a Windows cross-compile (issue #368), why `--features e2e` is not a gate you can hold yourself to yet, and how to choose between a per-item `#[cfg(unix)]` and a file-level `#![cfg(unix)]` when it finds a break.
- [CI entrypoints — running a CI step locally](docs/develop/ci-entrypoints.md) — the `task ci-*` wrappers around the seven cargo invocations CI runs (PRD #376): which `ci.yml` job owns each step, how to run one or all of them from the devbox shell, why the split is one task per step rather than one opaque `task ci`, why `ci.yml` is deliberately *not* routed through them (it is the merge gate and the measurement control), the local-pins-versus-floating-CI-toolchain skew this addresses, the status of the unproven Semaphore pipeline in `.semaphore/`, and the checklist to work through before connecting a Semaphore project.
- [Config-gen baseline regeneration](docs/develop/config-gen-regeneration.md) — the repeatable PRD #116 workflow for regenerating what the AI config generator would produce for a project and diffing it against that project's live `.dot-agent-deck.toml`: where the tooling lives (`examples/render_config_gen_prompt.rs`, `examples/diff_config.rs`), the engine (deck-default model, agent run against the real repo with tools enabled), and the date-gate methodology. The real config files are the corpus — no captured baselines are stored in-repo.
