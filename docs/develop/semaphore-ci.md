# Semaphore CI — evaluation port of the GitHub Actions workflows

This is an **evaluation**, not a migration. Every file under `.github/workflows/` is untouched and remains the authoritative CI. The pipelines under `.semaphore/` are a parallel implementation of the same gates, added so that Semaphore — and specifically its new AI-agent tooling, `sem-ai` — can be assessed against a real workload rather than a toy repo.

Nothing here runs until a Semaphore project is created and pointed at this repo, so merging the branch is inert on its own.

## Why Semaphore was worth evaluating: `sem-ai`

Semaphore's differentiator is not the pipeline engine, which is conventional. It is [`sem-ai`](https://github.com/semaphoreio/sem-ai) (Apache-2.0, Go), a CLI built for coding agents to drive CI/CD without a browser. Four properties matter for how this repo actually gets worked on:

- **JSON by default.** Every command emits structured JSON on stdout and structured errors on stderr (`{"error": true, "code": "not_found", …}`), with `-f table` / `-f yaml` as opt-ins for humans. Compare `gh run view --log`, where an agent parses prose.
- **Self-discovery.** `sem-ai discover` returns a machine-readable map of all ~80 commands with their flags and examples, so an agent can enumerate the surface instead of guessing.
- **An embedded MCP server.** `sem-ai mcp` exposes the whole CLI as MCP tools, so it drops into `.mcp.json` directly. Long-running commands (`watch`, `promote-and-wait`) are deliberately excluded so they cannot block an agent.
- **Compound diagnostics.** `sem-ai diagnose <id>` aggregates workflow state, failed-job context, log tails and parsed test results into one response; `blast-radius` separates root failures from cascading cancellations; `critical-path` finds the bottleneck chain; `test flaky` identifies flaky tests across runs. These are the queries you actually want after a red build, and each is one call rather than a log-scraping session.

There is also `sem-ai testbox`, which allocates a real Semaphore VM, syncs local files into it and runs commands there over SSH — a pre-push "does this work on the runner" loop with no equivalent in GitHub Actions. That is genuinely interesting for this repo, whose CI failures have historically been platform-specific (the `openpty`/`ioctl` libc divergence that broke the v0.34.0 macOS build; the `cargo test` vs `nextest` flakes of 2026-07-30).

The translation guidance in this document follows Semaphore's own `gha-to-semaphore` skill, which ships in the `sem-ai` repo under `assets/plugin/skills/`.

## File map

| GitHub Actions | Semaphore | Notes |
| --- | --- | --- |
| `ci.yml` → `build` | `semaphore.yml` → `Build (Linux)` | Full fidelity, all flags preserved |
| `ci.yml` → `windows-cross-check` | `semaphore.yml` → `Windows cross-check` | Full fidelity |
| `ci.yml` → `build-windows` | **none** | No Windows agents on Semaphore Cloud — see below |
| `ci.yml` → `build-macos` | `semaphore.yml` → `Build (macOS)` | Full fidelity |
| `ci.yml` → `security` | `semaphore.yml` → `Security audit` | Full fidelity |
| `ci.yml` → `nix` | `semaphore.yml` → `Nix flake check` | Full fidelity |
| `ci.yml` → `devbox` | `semaphore.yml` → `Devbox smoke` | Full fidelity |
| `ci.yml` → `changes` | `run.when: change_in(…)` on each block | Semantics differ — see below |
| `docs.yml` → `docs-build` | `semaphore.yml` → `Docs site` | Full fidelity |
| `aarch64-crossbuild-check.yml` | `semaphore.yml` → `aarch64 cross-build check` | Full fidelity, still via `cross` |
| `release.yml` | `release.yml` (auto-promoted on tag) | See "Release" below |
| `docs-publish.yml` | `docs-publish.yml` (promoted) | Two entry points preserved |
| `labeler.yml` | **none** | GitHub-API automation, not CI |
| `stale.yml` | **none** | Portable to a Semaphore scheduled task; not done |

Three GitHub Actions workflow *files* collapse into one Semaphore *pipeline*. GitHub keys triggers per file (`on: pull_request: paths:`), so "only run the aarch64 leg when `Cargo.lock` moved" is expressed by giving that job its own file. Semaphore inverts this: one pipeline is the entry point for every push, PR and tag, and per-block `run.when` decides what executes. The condition ends up next to the block it guards instead of in a separate file's `on:` stanza.

Supporting scripts live in `.semaphore/scripts/`. They exist because Semaphore blocks share no outputs the way GHA jobs share `outputs:` — so the SemVer gate, the flake-version check and the per-target build logic each need one implementation callable from several blocks, rather than being computed once in `prepare` and passed downstream.

## What did not port

### Windows (the significant one)

**Semaphore Cloud offers no Windows agents.** `ci.yml`'s `build-windows` job — native MSVC build, clippy, and the `test-fast` tier on a real `windows-latest` runner — has no cloud equivalent and stays on GitHub Actions.

This is not fully mitigable. What *does* still run on Semaphore is the `Windows cross-check` block, which type-checks the workspace for `x86_64-pc-windows-msvc` from Linux via `scripts/windows-cross-check.sh`. That catches compile errors but not the things a real Windows runner catches, and `ci.yml` is explicit that the two answer different questions.

Closing the gap properly would require registering a **self-hosted** Windows agent (`sem-ai agent types`), which Semaphore does support. Until then, dropping GitHub Actions entirely would mean losing native Windows coverage — which, given PRD #42 M8 added that job precisely because macOS-only and Windows-only breaks were reaching releases undetected, is a regression worth refusing.

### `workflow_dispatch`

Semaphore has no manual-trigger-with-inputs primitive. Two consequences:

- `release.yml`'s dispatch path (release an arbitrary version off `main` without a tag) is gone; the Semaphore release resolves its version from `SEMAPHORE_GIT_TAG_NAME` only. This also removes the issue #250 hazard that path carried, since there is no longer a case where the checkout's `git describe` disagrees with the version being published.
- `docs-publish.yml`'s dispatch path (the `/publish-docs` skill) becomes a **manual promotion** — the "Publish docs only" button on a CI pipeline. Behaviorally equivalent.

Semaphore promotion *parameters* could restore typed inputs if the dispatch path turns out to matter.

### `permissions:` and `GITHUB_TOKEN`

GitHub Actions grants each workflow an ambient, scoped identity. Semaphore has no such thing: every credential is an explicit secret. Where `docs-publish.yml` logs in to GHCR with `secrets.GITHUB_TOKEN` and `packages: write`, the Semaphore port authenticates with the admin PAT instead. This is a *reduction in least-privilege* and should be weighed — the PAT is broader than the scoped token it replaces.

### Renovate's version pins

`renovate.json`'s `customManagers` match `toolchain:` and `tool: cargo-nextest@` **under `.github/workflows` only**. The pins in `.semaphore/semaphore.yml` (`RUST_TOOLCHAIN`, `NEXTEST_VERSION`) are therefore *not* tracked and will silently drift from the GHA ones. Widening the config was deliberately left out of this branch because it would mean editing an existing file; it is the first thing to fix if this port is ever adopted rather than merely evaluated.

### `labeler.yml` and `stale.yml`

Neither is CI. `labeler.yml` uses `pull_request_target` and the `actions/labeler` action against the GitHub API — no Semaphore equivalent, and no reason to move it. `stale.yml` is a nightly cron that could become a Semaphore scheduled task (`sem-ai task create`), but doing so would just add a second system that writes to GitHub issues.

## Behavior differences to be aware of

**The `changes` job is not reproduced faithfully.** `ci.yml` skips the Rust matrix only for **Renovate-authored** devbox/flake-only PRs, and its comments are explicit that a *human* PR touching those files still gets the full matrix. Semaphore's conditions DSL exposes `branch`, `tag`, `pull_request`, `result` and `result_reason` — there is **no PR-author variable** — so this uses `change_in()` for everyone. A human PR touching only `devbox.*`/`flake.*` now skips the Rust blocks too.

This was a deliberate choice: `change_in` is evaluated before an agent boots, so it costs nothing, whereas the faithful alternative (a shell guard calling `gh api` for the author) would have to boot the agent before it could decide to skip. There is also a smaller divergence — a PR touching devbox files *and* flake files clears both flags in GHA and gets the full matrix, while `change_in` skips.

**CI now runs on tags.** GitHub Actions triggers `release.yml` directly from `push: tags: ['v*']`, independent of CI. Semaphore has no per-file tag trigger, so the release is an auto-promotion gated on `result = 'passed'` — which means the full matrix must be green on the tagged commit before anything publishes. Stricter than today, and it costs a full CI run per release.

**`main` no longer cancels itself.** `ci.yml` sets `cancel-in-progress: true` for every ref. That quietly undercuts the `push: branches: [main]` trigger, which exists so a broken `main` is a notification rather than a discovery — a cancelled post-merge run notifies nobody. The Semaphore port cancels running pipelines on feature branches and *queues* them on `main`.

**Caching is coarser.** `Swatinem/rust-cache` is Rust-aware: it prunes stale artifacts and keys on the toolchain and target. The Semaphore toolbox `cache` is a generic key/path store, so this uses `$(checksum Cargo.lock)` keys over `~/.cargo/registry` and `target/`. Expect worse hit rates and larger cache entries. Note also the toolbox footgun that `cache store` accepts exactly **one** path — hence separate keys rather than one call.

**No test reporting.** Semaphore has a genuinely good `test-results publish` surface with a per-pipeline aggregated report, which GitHub Actions has no built-in answer for. It is not wired up here because enabling JUnit output from nextest means editing `.config/nextest.toml`, and this branch does not modify existing files. It is the most valuable thing left on the table.

## Setup required before any of this runs

1. ~~**Create the project.**~~ Already done — a `dot-agent-deck` project exists in org `dot`, connected over the GitHub App integration. (For reference, the command would have been `sem-ai project create`, or `sem-ai init`, which detects the existing `.github/workflows/` and offers to translate.)
2. **Create the secret.** Both `release.yml` and `docs-publish.yml` bind one org-level secret named `dot-agent-deck-release`, carrying three env vars that mirror the GitHub repository secrets of the same names:

   ```
   sem-ai secret create dot-agent-deck-release \
     --env RELEASE_TOKEN=<admin PAT> \
     --env HOMEBREW_TAP_TOKEN=<token> \
     --env SCOOP_BUCKET_TOKEN=<token>
   ```

   `RELEASE_TOKEN` must be an admin PAT: it pushes the changelog commit and the docs chart bump straight to `main`, which the `main-protected` ruleset otherwise rejects with `GH013` (CLAUDE.md rule 8).
3. **Enable PR builds** in the project settings. Semaphore configures push/PR triggers server-side, not in YAML — there is no `on:` block to port.
4. **Validate.** `sem-ai yaml validate --file .semaphore/semaphore.yml` (and the other two). This is a **server-side** check against the Semaphore API, so it requires `sem-ai connect <org>.semaphoreci.com <token>` first.

## Verification status

**All three pipelines pass `sem-ai yaml validate`** against the API (org `dot`).

**But read what that check does and does not cover before trusting it.** It is a *structural* validation, not a resource one. Measured, by feeding it two deliberately broken pipelines:

- a block whose `dependencies:` names a nonexistent block is **caught** — `{:malformed, {:unknown_block_name, "Nonexistent Block"}}`;
- an `agent.machine.type` of `not-a-real-machine` is **reported valid**.

So the API confirms the shape of these files is correct and says nothing about whether the machines they ask for exist or are available on this org's plan. The `a2-standard-4` / `macos-xcode16` macOS agent and the `f1-standard-4` sizing are therefore still unverified, and Semaphore exposes no way to enumerate cloud machine types — `sem-ai discover` surfaces `--machine` only as an input to `testbox warmup`. The way to actually settle it is to run something: either push the branch, or allocate a probe VM with `sem-ai testbox warmup --project dot-agent-deck --machine a2-standard-4 --os-image macos-xcode16`.

Also confirmed against the live org:

- A **`dot-agent-deck` project already exists** (`c45e9515-…`), wired to `git@github.com:vfarcic/dot-agent-deck.git` over the GitHub App integration, with prior workflow runs on `main` and on feature branches. So step 1 of the setup list below is already done, and pushing this branch would trigger a real pipeline.
- `sem-ai agent types` returns `[]` — **no self-hosted agents are registered**, which confirms empirically that there is no Windows capacity in this org today, cloud or otherwise.

Verified locally, independent of the API: the pipelines parse and every `commands:` entry is a string — worth checking mechanically, because a plain YAML scalar containing `": "` silently parses as a *mapping* rather than a command, which is how `- echo "toolchain: $X"` becomes a key/value pair. Three such lines existed in the first draft and are now explicitly quoted. `resolve-version.sh` and `verify-flake-version.sh` were exercised against the real repository: the SemVer gate accepts `1.2.3` and `0.36.0-rc.1` and rejects `1.2`, `not-semver`, the full-width `１.２.３`, `1.2.3-é` and the empty string; the flake check accepts the real pinned `0.36.0` and rejects a mismatch.

**Still unobserved**, because nothing has run on an agent: that the pinned Rust 1.97.1 installs cleanly on both images (Ubuntu ships 1.95.0, macOS ships no Rust at all), that `cross` works on the `f1` Docker-enabled machines, and that the Nix and devbox installers behave on the agent image.
