# Semaphore CI — evaluation port of the GitHub Actions workflows

This is an **evaluation**, not a migration. Every file under `.github/workflows/` is untouched and remains the authoritative CI. The pipelines under `.semaphore/` are a parallel implementation of the same gates, added so that Semaphore — and specifically its new AI-agent tooling, `sem-ai` — can be assessed against a real workload rather than a toy repo.

## Scope: everything builds, nothing publishes

The rule these pipelines follow is: **do everything the GitHub Actions workflows do, except Windows and except publishing.**

So every gate that can *fail* runs — all four release target builds, the issue #250 `DAD_VERSION` artifact assertion, the SemVer gate, the flake-pin check, changelog assembly, checksums, and generation of the Homebrew formula and Scoop manifest. What is switched off is anything with an effect outside the pipeline:

| Step | Where | State |
| --- | --- | --- |
| changelog commit + `git push origin main` | `release.yml` → `Prepare` | commented out |
| `gh release create` | `release.yml` → `Package` | commented out |
| `task homebrew-publish` | `release.yml` → `Package` | commented out |
| `task scoop-publish` | `release.yml` → `Package` | commented out |
| GHCR `docker login` / `docker push` | `docs-publish.yml` | commented out |
| chart bump + `git push origin main` | `docs-publish.yml` | commented out |

They are **commented out rather than deleted**, together with the `secrets:` and `queue:` blocks they depend on, so adopting Semaphore is an uncomment rather than a rewrite.

Two caveats. First, **nothing validates a commented block** — not `sem-ai yaml validate`, not any run — so they will drift silently if the Taskfile targets or token names change; review them before uncommenting rather than trusting them. Second, `artifact push` still appears in the per-target build script: that is Semaphore's *internal* block-to-block file handoff, the counterpart of GHA's `upload-artifact`/`download-artifact` pair, not publication to anywhere external.

A useful consequence: as it stands the entire Semaphore setup requires **no secrets at all**, so it cannot affect anything outside itself even by accident.

This is enforced mechanically, not by inspection — a scan of every live `commands:` entry across all three pipelines finds zero occurrences of `git push`, `docker push`, `docker login`, `gh release`, `homebrew-publish`, `scoop-publish`, `git commit` or `remote set-url`.

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
| `release.yml` | `release.yml` (auto-promoted on tag) | Builds and packages; **publishing commented out** |
| `docs-publish.yml` | `docs-publish.yml` | Image builds; **push and chart commit commented out** |
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

### What real agents proved

`sem-ai testbox` allocates a real CI VM and runs commands on it over SSH, which made it possible to verify the risky parts without the pipeline ever running. Three findings, all measured:

**Every Linux machine above 2 vCPU is unavailable on this org's Free Plan** — and the web console is not a reliable guide, because it lists several of them under "Available Agents" anyway.

| Machine | `testbox warmup` |
| --- | --- |
| `f1-standard-2`, `e1-standard-2`, `e2-standard-2` | READY |
| `a2-standard-4` (macOS) | READY |
| `f1-standard-4`, `e1-standard-4`, `e1-standard-8` | FAILED — `job finished before reaching RUNNING state` (5 attempts, both images) |

So 2 vCPU is the ceiling on Linux, and `f1-standard-2` is the current-generation option at that size (`e1`/`e2` are legacy). Four blocks originally asked for `f1-standard-4` — `Build (Linux)`, `Nix flake check`, `aarch64 cross-build check`, and the release pipeline default — and all are pinned down, with a comment recording what to revert on a paid plan. `Build (Linux)` is the critical path, so this costs real wall-clock. macOS is evidently metered separately, since `a2-standard-4` is 4 vCPU and comes up fine.

**The agent images ship no Rust and no rustup**, contradicting the docs, which list Rust 1.95.0 for `ubuntu2404`. On both `ubuntu2204` and `ubuntu2404`, `rustc` is `command not found` and `rustup` is absent from `PATH` — Rust is reachable only through `sem-version`. This vindicates `setup-rust.sh` installing rustup from scratch rather than trusting the image, and it means a pipeline that assumed a preinstalled `cargo` would fail immediately.

**`setup-rust.sh` works on a real agent.** On `ubuntu2404` it installed rustup from nothing, pinned exactly 1.97.1, added rustfmt and clippy, and fetched cargo-nextest 0.9.140 — in **8.3 seconds**. Verified after the fact: `rustc 1.97.1`, `rustfmt 1.9.0-stable`, `clippy 0.1.97`, `cargo-nextest 0.9.140`.

The toolbox is present as documented — `checkout`, `cache`, `artifact`, `sem-version`, `retry` and `test-results` all resolve — and Docker is available (28.4.0), which is what the `aarch64 cross-build check` and the docs image build need.

One caveat on `testbox run`: it rsyncs the working directory before executing, so running it from this repo hangs on the multi-GB `target/`. Run it from a small scratch directory, or SSH directly using the key and address `warmup` prints.

**The macOS agent works, and `setup-rust.sh` works on it.** `a2-standard-4` / `macos-xcode16` came up as macOS 15.4.1. As on Linux there is no Rust and no rustup — and `sem-version` is not even on `PATH` in a testbox SSH session, which is why the toolbox docs tell you to `source ~/.toolbox/toolbox` there. The script installed 1.97.1 for `aarch64-apple-darwin`, added clippy and the `x86_64-apple-darwin` cross target, and fetched nextest 0.9.140. That validates both the `Build (macOS)` block and the macOS half of the release matrix. (`testbox warmup --os-image macos-xcode16` works even though `--help` documents only the Ubuntu images.)

### Still unverified

That `cross` works on the f1 machines, and that the Nix and devbox installers behave on the agent image.

### The pipelines have not run end to end

**The project was never finished being created.** The Semaphore console shows it parked on step 3 of 4 of the onboarding wizard ("Select the environment"), having never reached "4. Setup workflow". Its last workflow was 2026-08-06, which is when that wizard was presumably started. Pushing the `semaphore-ci` branch produced no run at all (`no workflow found`).

The project's stored config is otherwise correct — `run_on: [tags, branches, draft_pull_requests]`, empty branch whitelist, `pipeline_file: .semaphore/semaphore.yml` — which is why this looked like a dead webhook from the API side.

It is **not** a case of Semaphore wanting the pipeline file on the default branch: Semaphore reads `.semaphore/semaphore.yml` from the commit being built, so a file that exists only on a feature branch is fine. (That intuition comes from GitHub Actions, where `workflow_dispatch` really does require the workflow on the default branch.)

`sem-ai workflow run --branch semaphore-ci` cannot substitute for finishing setup: it *reruns* an existing workflow and returns `no workflows found to rerun` for a branch that has never built.

**When finishing the wizard, choose "skip onboarding" rather than letting it generate a starter workflow.** The final step offers to define build steps for you, and a generated `.semaphore/semaphore.yml` would either overwrite the one in this branch or be committed to the repository — neither of which is wanted, and the second violates the no-pushing rule these pipelines otherwise satisfy. The YAML already exists; the project only needs to be finalized so it starts building pushes.

### Verified locally

The pipelines parse and every `commands:` entry is a string — worth checking mechanically, because a plain YAML scalar containing `": "` silently parses as a *mapping* rather than a command, which is how `- echo "toolchain: $X"` becomes a key/value pair. Three such lines existed in the first draft and are now explicitly quoted. `resolve-version.sh` and `verify-flake-version.sh` were exercised against the real repository: the SemVer gate accepts `1.2.3` and `0.36.0-rc.1` and rejects `1.2`, `not-semver`, the full-width `１.２.３`, `1.2.3-é` and the empty string; the flake check accepts the real pinned `0.36.0` and rejects a mismatch.

`resolve-version.sh` falls back to the version `flake.nix` pins when there is no tag, so the release path can be dry-run by promoting `Release build` manually from any branch. That fallback makes `verify-flake-version.sh` a tautology on branch runs — it compares `flake.nix` against itself — and the check only means something on a tag, where the two sources are genuinely independent.
