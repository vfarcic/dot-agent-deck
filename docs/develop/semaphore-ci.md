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

## Pros and cons, from this evaluation

Everything below is something this port actually hit. It is not a feature comparison; it is what showed up while moving one real repository's CI.

### In Semaphore's favour

**`sem-ai` is the real product, and it delivers on its premise.** This entire evaluation — discovery, validation, machine-type probing, triggering, diagnosis — was driven from the CLI without opening the web console once. `discover` enumerates the surface so nothing has to be guessed, JSON is the default so nothing has to be scraped, and the embedded MCP server means the same surface drops into an agent config directly. GitHub's `gh` is a good CLI, but it was built for humans and retrofitted; this was built the other way round.

**`testbox` is the standout feature and has no GitHub Actions equivalent.** It allocates a real CI VM you can SSH into. It found three things that documentation and the web console both got wrong — see the machine-type table and the missing Rust below — *before any pipeline ran*. For a project whose CI failures are historically platform-specific, being able to interrogate the runner directly is worth a lot.

**`diagnose` collapses a debugging session into one call.** Failed blocks, failed jobs, log tails and commit context in a single structured response. Finding the same information in GitHub Actions means opening a run, expanding a job, and scrolling a log.

**Manual triggering is more capable than `workflow_dispatch`.** A Task can run any branch against any pipeline file on demand. GitHub Actions requires the workflow to already exist on the default branch, which is a genuine obstacle when the thing you want to test is the workflow itself.

**Conditions live next to the block they guard.** Three GHA workflow files collapsed into one pipeline because `change_in()` replaced per-file `on: paths:` triggers. Less indirection, and the whole CI graph is visible in one file.

**The toolbox removes setup boilerplate.** `checkout`, `cache`, `artifact`, `sem-version`, `retry` and `test-results` are preinstalled, so most `uses: actions/...` steps collapse to nothing.

**Promotions are a better deploy model than reusable workflows.** A promoted pipeline binds its own secrets, which structurally prevents the v0.35.9 failure — where `docs-publish.yml` was called via `uses:` without `secrets: inherit`, silently got an empty token, and died *after* publishing the image.

**`test-results` has no GitHub Actions counterpart.** Per-pipeline aggregated test reporting is built in. Not wired up here, but it is the most valuable thing left on the table.

### Against

**No Windows agents, at all.** This is the one disqualifying gap. `build-windows` cannot move, and `sem-ai agent types` confirms no self-hosted capacity exists either. Retiring GitHub Actions would mean losing the native Windows coverage that PRD #42 M8 added precisely because platform-specific breaks were reaching releases.

**The Free Plan gates every Linux machine above 2 vCPU** — `f1-standard-4`, `e1-standard-4` and `e1-standard-8` all fail to start. The heaviest block in the pipeline is the critical path and is stuck on 2 vCPU.

**The web console lists machines it cannot run.** "Available Agents" showed all three of the machines above. Only `testbox` revealed the truth. A configuration surface that advertises unavailable options is worse than one that lists fewer.

**The docs are wrong about the agent images.** They state Rust 1.95.0 ships on `ubuntu2404`. In reality `rustc` is command-not-found and `rustup` is absent on both `ubuntu2204` and `ubuntu2404`, and on macOS `sem-version` is not even on `PATH` in a testbox session. A pipeline written from the documentation would fail on its first command.

**`sem-ai yaml validate` is structural only.** It catches a dependency on a nonexistent block but passes `type: not-a-real-machine`. A green validation says much less than it appears to.

**No ambient job identity.** GitHub Actions grants a scoped `GITHUB_TOKEN` via `permissions:`. Semaphore has nothing equivalent, so the GHCR push that used a scoped token now needs the admin PAT — a real reduction in least-privilege, not a mechanical swap.

**The conditions DSL has no PR-author variable**, so the Renovate-scoped skip in `ci.yml` cannot be reproduced faithfully.

**No Rust-aware caching.** `Swatinem/rust-cache` prunes stale artifacts and keys on toolchain and target; the toolbox `cache` is a generic key/path store with a one-path-per-`store` footgun. Expect worse hit rates and larger entries.

**Ecosystem gaps cost real time.** There is no `nix-installer-action` and no `devbox-install-action`, so both had to be driven by hand — one of them wrongly, and the other now runs uncached for 17+ minutes. Much of what makes a GitHub Actions workflow short is the Marketplace, and that does not travel.

**The onboarding wizard can leave a project half-created and silently non-building**, with a stored config that looks correct from the API. That cost most of a debugging session.

**Minor:** `diagnose` accepts a *workflow* id but 404s on a *pipeline* id, with no hint that the distinction is the problem.

### Why a team would choose Semaphore over GitHub Actions

Separating what this evaluation *verified*, what independent users report, and what is vendor marketing — because the three disagree in places.

**1. Agent-native CI, verified here.** `sem-ai` is the strongest reason and the hardest for GitHub to copy quickly. It is a deliberate design — JSON by default, `discover` for self-enumeration, an embedded MCP server, and compound diagnostics — rather than a CLI with a `--json` flag bolted on. If CI is increasingly driven by agents rather than humans clicking through a web UI, this is a structural advantage. `gh` is a fine tool built for humans first.

**2. `testbox`, verified here, and genuinely unique.** Being able to SSH into a real CI runner *before* pushing found three things that Semaphore's own docs and console got wrong. There is no GitHub Actions equivalent; the closest is `act` (a local Docker approximation, not the real runner) or push-and-pray.

**3. Speed and cost — but check who is measuring.** Semaphore's marketing claims GitHub Actions is [94% slower on the same workload](https://semaphore.io/best-ci-cd-tools-in-2026-performance-and-cost-compared) at $0.04/job. That is a **vendor-run benchmark against its own competitor** and should be treated as such. Independent review sites are more measured but directionally supportive: Capterra reviewers report test run times "halved" after switching, and G2 reviewers report cutting CI/CD costs 38–50% — though mostly **versus CircleCI and Travis, not versus GitHub Actions**. Our own run is not evidence either way, since the Free Plan capped us at 2 vCPU.

**4. Monorepo and conditional execution, verified here.** `change_in()` is better than GitHub's per-workflow `on: paths:`, and it is why three GHA files collapsed into one pipeline. Independent comparisons single out monorepo support as a Semaphore strength.

**5. Ephemeral VMs.** Every job gets a fresh VM, which removes the environment-drift class of flake. GitHub-hosted runners are also fresh, so this matters most against self-hosted setups.

**6. Support quality.** This is the most consistent theme across independent reviews on [G2](https://www.g2.com/products/semaphore/reviews) and [Capterra](https://www.capterra.com/p/171934/Semaphore/reviews/) — fast, human, willing to engage with unusual setups. GitHub Actions support at comparable spend is not a thing.

**7. The promotion model**, verified here: a promoted pipeline binds its own secrets, which structurally prevents the class of bug that broke v0.35.9.

**Reasons *not* to, which this evaluation weighted heavily:** no Windows agents; the Marketplace ecosystem does not travel (both blocks that failed or timed out are exactly the two that leaned on a GHA action); and independent reviewers additionally flag a cluttered/slow UI, limited repository-host support — one notes that leaving GitHub would mean leaving Semaphore too — and pricing that climbs with commit frequency and macOS usage.

**The honest summary for this repo:** the compelling reasons are `sem-ai` and `testbox`, not speed or cost. Those are real, verified, and ahead of anything GitHub ships for agent-driven CI. They are not currently enough to outweigh losing native Windows coverage.

### Where it nets out

For a Linux-and-macOS Rust project, the port is faithful and six of eight blocks passed on the first real run. The blockers are not about pipeline expressiveness — Semaphore's model is at least as good as GitHub Actions' and in places better. They are Windows, the plan's machine ceiling, and the Marketplace ecosystem that a mature GHA setup quietly depends on. `sem-ai` is genuinely ahead of anything GitHub ships for agent-driven CI, and is the strongest reason to keep watching this.

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

### First real end-to-end run

Triggered 2026-08-12 against commit `53d33a3` on `semaphore-ci`. **Six of eight blocks passed on the first attempt.**

| Block | Result |
| --- | --- |
| `Build (Linux)` — fmt, clippy `--all-targets --features e2e`, release build, 1705 nextest tests, linkage-check | **passed** |
| `Windows cross-check` | **passed** |
| `Build (macOS)` — `a2-standard-4` | **passed** |
| `Security audit` — `cargo audit` | **passed** |
| `Docs site` — `npm ci`, Docusaurus build, docker build | **passed** |
| `aarch64 cross-build check` — `cross` + Docker | **passed** |
| `Nix flake check` | **failed** — see below |
| `Devbox smoke` | very slow — see below |

That closes most of the "still unverified" list at once: `cross` genuinely works on the `f1` machines, the Rust toolchain pin holds on both Linux and macOS in a real job, and the whole fast tier passes on 2 vCPU.

**`Nix flake check` failed on a bug in this pipeline, not in the flake.** The prologue passed `--init none` to the Determinate installer, so no daemon was ever started:

```
error: opening lock file "/nix/var/nix/db/big-lock": Permission denied
… run as non-root in a single-user Nix installation, or the Nix daemon may have crashed
```

GitHub Actions never hits this because `DeterminateSystems/nix-installer-action` handles daemon setup; there is no such action on Semaphore, so the installer has to be driven correctly by hand.

**`Devbox smoke` hit the one-hour job timeout and took the whole pipeline with it** — final pipeline state `stopped`, `result_reason: timeout`. GHA's `jetify-com/devbox-install-action` sets `enable-cache: true`, caching the whole Nix store keyed on `devbox.json`, and ci.yml's comment notes the `path:gcloud#google-cloud-sdk` flake builds the SDK from source and dominates a cold run. There is no equivalent action here, so every run is cold — and cold is more than an hour. This block is **not merely slow, it is currently unusable**, and it needs a hand-rolled `sudo tar` of `/nix` into a `cache store`/`cache restore` pair before it can pass.

Note the blast radius: one un-cacheable block failing on time turned an otherwise-green pipeline into a `stopped` one. Six blocks passing does not make the pipeline pass.

### Triggering a pipeline on a branch without a webhook

Yes, this is possible, and it is how the run above happened — but **not** via `workflow run`, which only reschedules an existing workflow and returns `no workflows found to rerun` for a branch that has never built. The mechanism is a **Task**:

```
sem-ai task create semaphore-ci-manual --project dot-agent-deck \
  --branch semaphore-ci --file .semaphore/semaphore.yml
sem-ai task run <task-id> --branch semaphore-ci --pipeline-file .semaphore/semaphore.yml
```

Omitting `--cron` gives a manual-only task. `task run` then accepts `--branch` and `--pipeline-file` overrides, so one task can trigger *any* branch against *any* pipeline file on demand. The resulting workflow reports `triggered_by: MANUAL_RUN`.

This is strictly more capable than GitHub Actions' `workflow_dispatch`, which requires the workflow file to already exist on the default branch. It also means the broken webhook is not actually blocking: pipelines can be exercised on a branch today.

### The webhook itself

**The project was never finished being created.** The Semaphore console shows it parked on step 3 of 4 of the onboarding wizard ("Select the environment"), having never reached "4. Setup workflow". Its last workflow was 2026-08-06, which is when that wizard was presumably started. Pushing the `semaphore-ci` branch produced no run at all (`no workflow found`).

The project's stored config is otherwise correct — `run_on: [tags, branches, draft_pull_requests]`, empty branch whitelist, `pipeline_file: .semaphore/semaphore.yml` — which is why this looked like a dead webhook from the API side.

It is **not** a case of Semaphore wanting the pipeline file on the default branch: Semaphore reads `.semaphore/semaphore.yml` from the commit being built, so a file that exists only on a feature branch is fine. (That intuition comes from GitHub Actions, where `workflow_dispatch` really does require the workflow on the default branch.)

`sem-ai workflow run --branch semaphore-ci` cannot substitute for finishing setup: it *reruns* an existing workflow and returns `no workflows found to rerun` for a branch that has never built.

**When finishing the wizard, choose "skip onboarding" rather than letting it generate a starter workflow.** The final step offers to define build steps for you, and a generated `.semaphore/semaphore.yml` would either overwrite the one in this branch or be committed to the repository — neither of which is wanted, and the second violates the no-pushing rule these pipelines otherwise satisfy. The YAML already exists; the project only needs to be finalized so it starts building pushes.

### Verified locally

The pipelines parse and every `commands:` entry is a string — worth checking mechanically, because a plain YAML scalar containing `": "` silently parses as a *mapping* rather than a command, which is how `- echo "toolchain: $X"` becomes a key/value pair. Three such lines existed in the first draft and are now explicitly quoted. `resolve-version.sh` and `verify-flake-version.sh` were exercised against the real repository: the SemVer gate accepts `1.2.3` and `0.36.0-rc.1` and rejects `1.2`, `not-semver`, the full-width `１.２.３`, `1.2.3-é` and the empty string; the flake check accepts the real pinned `0.36.0` and rejects a mismatch.

`resolve-version.sh` falls back to the version `flake.nix` pins when there is no tag, so the release path can be dry-run by promoting `Release build` manually from any branch. That fallback makes `verify-flake-version.sh` a tautology on branch runs — it compares `flake.nix` against itself — and the check only means something on a tag, where the two sources are genuinely independent.
