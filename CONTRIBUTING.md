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
- [Versioning and the "breaking" definition](docs/develop/versioning.md) — what "breaking" means here (the TUI↔daemon contract, including semantic breaks behind a stable wire), the `PROTOCOL_VERSION` floor and `.breaking.md` fragment, the `0.x` bump policy (breaking→minor, feature/bugfix→patch), and the cross-version manual-test discipline.
