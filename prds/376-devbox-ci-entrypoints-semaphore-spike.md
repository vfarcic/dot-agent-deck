# PRD #376: Devbox-native CI entrypoints and a Semaphore pipeline spike

**Status**: In progress
**Priority**: Medium
**Created**: 2026-08-04

## Problem Statement

Two problems, one of which is a live bug.

**The toolchain is pinned locally and floating in CI.** `devbox.json` pins `rustc@1.97.1`, `cargo@1.97.1`, `clippy@1.97.1`, `rustfmt@1.97.1` and `cargo-nextest@0.9.140`. `.github/workflows/ci.yml:109` uses `dtolnay/rust-toolchain@stable` and `:113` installs nextest via `taiki-e/install-action@nextest` — both resolve to whatever is current on the day the job runs. So the local gate (`cargo test-fast`, aliased in `.cargo/config.toml`) and CI compile with different toolchains, and the divergence widens silently until a new release introduces a lint or a behaviour change. The first symptom will be a clippy failure that cannot be reproduced locally.

That is the same class of problem the `ci.yml:117-134` comment documents from 2026-07-30, where `cargo test` and `cargo nextest run` produced different flake behaviour and the fix was to align the runners so that *"green locally means the same thing as green in CI."* The runner was aligned; the toolchain version was not.

**There is no pipeline abstraction to reuse.** `Taskfile.yml` covers docs, demo reels, checksums, homebrew and scoop — release and packaging automation. It has no `build`, `test`, `lint` or `ci` task. The CI steps exist only as raw `cargo` invocations inside `ci.yml`, so nothing outside GitHub Actions can run them, including an agent working locally.

Separately, we want to evaluate Semaphore as a second CI provider (including its agentic `sem-ai` tooling) for a comparison. That evaluation needs the build steps to be invokable outside GHA anyway, because Semaphore has no equivalent of the marketplace actions `ci.yml` depends on. The toolchain has to be provisioned by hand there regardless — and `devbox.json` already pins exactly the right set, already under Renovate management (`renovate.json`, `matchManagers: ["devbox"]`, automerged for patch/minor).

So the second provider is the forcing function, but the pinning fix is worth landing on its own.

## Solution Overview

Introduce `task`-level CI entrypoints that wrap the existing cargo invocations, and provision the toolchain from `devbox.json` rather than from marketplace actions. Stand the result up as a new Semaphore pipeline covering the three jobs Semaphore Cloud can host, measure it against the GHA baseline, and use those numbers to decide whether to retrofit GHA.

Deliberately **do not** touch `ci.yml` or `release.yml` in this PRD. They are the working gate that Renovate automerges against, and they are the experimental control for the measurement. Editing them in the same change makes the comparison meaningless and puts a green pipeline at risk for no benefit.

## Scope

### In Scope

- New `task` entrypoints wrapping the commands `ci.yml` runs: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo build --release`, `cargo nextest run`, `cargo xtask linkage-check` (`ci.yml:114-144`) and `cargo audit` (`ci.yml:273`). Seven, not six as originally written: `ci.yml` also runs a plain debug `cargo build` on its macOS and Windows jobs (`ci.yml:256`, `:223`), which is a distinct command and got its own `ci-build-debug` entrypoint so the Semaphore macOS block can reproduce what the GHA macOS job does rather than doing strictly more work.
- `cargo-audit` added to `devbox.json`. `ci.yml:272` currently runs `cargo install cargo-audit --locked`, which compiles from source on every run and is almost certainly the slowest single step in CI. This is an unambiguous win independent of everything else in this PRD.
- `.semaphore/semaphore.yml` covering the three jobs Semaphore Cloud can host: `build` (Linux), `build-macos`, `security`.
- Measurement against the GHA baseline: cold nix bootstrap, warm bootstrap, `target/` cache hit rate, per-job and total wall clock.
- A recorded decision on whether to retrofit `ci.yml`, backed by those numbers.

### Out of Scope

- **Any edit to `ci.yml` or `release.yml`.** `git diff` against both must be empty when this PRD closes. Retrofitting GHA is a follow-up, gated on M5's numbers.
- **Windows.** Nix does not run natively on Windows — `ci.yml:193-195` already records this (*"the devbox/nix toolchain used locally is Linux-only"*) — and Semaphore Cloud has no hosted Windows runner, only self-hosted agents. `build-windows` stays on GHA with rustup. Two toolchain-provisioning paths is the accepted end state, not a gap to close later.
- **Publishing of any kind.** Publishing is confined to `release.yml` (triggered by `push: tags: ['v*']`) and `docs-publish.yml` (`workflow_call` only). `ci.yml` has no side effects, so a `ci.yml`-shaped Semaphore pipeline has nothing to double-publish. No publish flags, kill switches or conditionals are introduced anywhere — adding one to `release.yml` would create a silent-non-publish failure mode strictly worse than the double-publish it prevents.
- **Making Semaphore a required status check.** Renovate automerges cargo patch and ≥1.0 minor bumps on green CI (`renovate.json`). An unproven pipeline does not go in that path.
- Semaphore promotions and deployment targets. Interesting for a later comparison, but they imply release-shaped behaviour, which is out of scope here.

## Technical Approach

### Task entrypoints

Thin wrappers, one per existing CI step, so the mapping to `ci.yml` stays auditable and a diverging step is visible as a diff rather than as a behaviour change. No consolidation into a single `task ci` that hides which step failed — the four-job split in `ci.yml` exists so a platform-specific break is visible independently (`ci.yml:191-210`, `:233-244`), and the entrypoints should preserve that granularity.

### Reading `cargo --version` when comparing providers

Under the `cargo@1.97.1` devbox pin, `rustc --version` reports `1.97.1` while `cargo --version` reports `cargo 1.97.0 (c980f4866 2026-06-30)`. That is nixpkgs' 1.97.1 derivation reporting a 1.97.0 internal version — not a pin that failed to resolve, and not devbox picking the wrong package. It is written down here because any Semaphore-vs-GHA diff analysis that reads `cargo --version` will see `1.97.0` on the Semaphore side and must not conclude the pin did not take. `.semaphore/ci.sh` echoes both versions during the bootstrap and repeats this warning next to them.

### Caching is the part that does not abstract

This is the main technical risk and the reason this is a spike rather than a refactor.

`Swatinem/rust-cache@v2` (`ci.yml:112`) is not a thin wrapper. It derives keys from `Cargo.lock` plus the rustc version, selectively saves `~/.cargo` registry and git db plus `target/`, and prunes stale and incremental artifacts to keep the archive small. Replacing it means owning that logic.

Worse, the cache *backend* is irreducibly provider-specific: GHA uses the `actions/cache` REST API, Semaphore uses its `cache store` / `cache restore` CLI, and locally there is no cache because `target/` is already warm. No script abstracts that away — the pipeline will be provider-agnostic where it costs nothing (invoking cargo) and provider-coupled exactly where CI time is won or lost.

And the nix store itself now needs caching on both providers, which is a second cache problem that does not exist today.

### Nix bootstrap cost

Devbox in CI needs nix installed and packages fetched. Cold, that is minutes. The conventional fix is `jetify-com/devbox-install-action`, which is a marketplace action and therefore unavailable on Semaphore and self-defeating on GHA. Bootstrap plus store caching has to be hand-rolled per provider. The honest expectation is that the first version is **slower** than the current `rust-cache`-based jobs; M5 exists to find out by how much.

### Machine mapping

| GHA job | Semaphore equivalent |
|---|---|
| `build` (`ubuntu-latest`) | `f1-standard-4` — 4 vCPU / 16 GB, Ubuntu 24.04 |
| `build-macos` (`macos-latest`) | `a2-standard-4` — Apple Silicon M2, 4 vCPU / 8 GB, Xcode16 or Xcode26 |
| `security` (`ubuntu-latest`) | `f1-standard-4` |
| `build-windows` | none — out of scope |

Note `a2-standard-4` has 8 GB against `ubuntu-latest`'s 16 GB, and it runs `cargo build --release` plus the full `nextest` tier. Memory pressure is a live possibility.

### Cost

`vfarcic/dot-agent-deck` is public, so GHA is free for it. Semaphore's `f1` is $0.0075/min, so this is added cost, not saved cost — roughly $0.35–0.40 per full run. The evaluation must be argued on wall clock, cache behaviour and agent experience, not price.

### Renovate

No regression. `renovate.json` already has a `matchManagers: ["devbox"]` rule grouped as "Devbox packages" with automerge for digest/pin/patch/minor, so moving toolchain versions from GHA action refs into `devbox.json` keeps them bot-managed. The `github-actions` manager rule simply has less to do.

## Success Criteria

- Each `ci.yml` step has a `task` entrypoint that runs identically on a local devbox shell and on Semaphore.
- `cargo-audit` comes from `devbox.json`; nothing compiles a CI tool from source.
- The Semaphore pipeline is green for `build`, `build-macos` and `security` on a PR and on a push to `main`.
- Measured and recorded: cold bootstrap, warm bootstrap, `target/` cache hit rate, per-job and total wall clock, each against the current GHA baseline.
- `git diff` against `ci.yml` and `release.yml` is empty.
- A written decision on retrofitting GHA, citing the numbers — including "no" as an acceptable outcome.

## Milestones

- [x] **M1 — Task entrypoints and `cargo-audit` in devbox.** All six steps runnable locally through `task`; `cargo-audit` resolved from `devbox.json`. Landable and useful on its own even if every later milestone is abandoned. *Done — seven entrypoints, not six (`ci-build-debug` added for the debug `cargo build` the macOS/Windows jobs run), plus an aggregate `task ci`. `cargo-audit@0.22.2` is pinned in `devbox.json`/`devbox.lock` and covered by `scripts/devbox-smoke.sh`.*
- [x] **M2 — Darwin toolchain gate.** Confirm `devbox` resolves `rustc@1.97.1` and the rest for `aarch64-darwin` from a binary cache rather than building from source. **If it builds Rust from source, stop and reconsider** — the macOS job is not viable and the scope shrinks to Linux only. *Done, verdict GO — every pinned package is substitutable for `aarch64-darwin`, verified three ways (see the Work Log). Note the gate covered the `devbox.lock` roots only; `path:gcloud#google-cloud-sdk` is not in `devbox.lock` and was not checked.*
- [ ] **M3 — Semaphore Linux green.** `build` and `security` passing, with `target/` and nix-store caching in place. *Pipeline FILE written (`.semaphore/semaphore.yml`, `.semaphore/ci.sh`) with both caches implemented; NOT green and cannot be, because no Semaphore Cloud project is connected to the repo. Nothing in either file has ever executed.*
- [ ] **M4 — Semaphore macOS green.** `build-macos` passing on `a2-standard-4`, memory headroom confirmed. *Block written and in scope after M2's GO; unrun, so memory headroom is still an open question rather than a measurement.*
- [ ] **M5 — Measurements recorded.** The full comparison table against the GHA baseline, written down where the retrofit decision will be made. *Not started — blocked on M3/M4. The only numbers so far are local ones: a 4.4 GiB / 442-path devbox profile closure and an 8.5 GiB long-lived `target/`, both in the Work Log.*
- [ ] **M6 — Decision and docs.** Retrofit-or-not recorded with rationale; `docs/develop/` note covering the task entrypoints and how to run a CI step locally. *Docs done (`docs/develop/ci-entrypoints.md`, linked from `CONTRIBUTING.md`). The retrofit decision is NOT made and must not be made without M5's numbers.*

## Risks

- **The abstraction misses the part that matters.** Caching is where CI time lives and it is exactly what cannot be made provider-agnostic. The pipeline will still carry `if GITHUB_ACTIONS / elif SEMAPHORE` branches in its cache steps. If that is unacceptable, the premise of the PRD is weaker than it looks.
- **CI gets slower.** `rust-cache` is well tuned; a hand-rolled equivalent plus a nix bootstrap plausibly loses to it initially. M5 is the check, and a slower result is a legitimate reason to close this PRD without retrofitting GHA.
- **macOS binary cache miss.** nixpkgs darwin cache hit rates are worse than Linux. Building the Rust toolchain from source on `a2-standard-4` would make the macOS job unusable. M2 gates this deliberately.
- **Scope creep into `ci.yml`.** The single largest risk for a session that does not have the originating discussion. `ci.yml` and `release.yml` are off limits: they are the control for the measurement, `release.yml` touches real users' `brew upgrade` path through `vfarcic/homebrew-tap` and `vfarcic/scoop-bucket`, and Renovate automerges against `ci.yml` being green.
- **Confounded comparison.** Changing GHA and standing up Semaphore at the same time means a slow Semaphore job cannot be attributed — machine, nix bootstrap or hand-rolled cache. This is the concrete reason for the out-of-scope rule above, not tidiness.
- **Two toolchain paths forever.** Windows keeps rustup and marketplace actions, so the pinning fix does not reach it. Given `portable-pty` is held at `=0.8.1` for a Windows ConPTY reason (`renovate.json`, `Cargo.toml`), Windows is where the load-bearing bugs live — and it is the platform this change cannot help.

## Open Questions

1. ~~**Does `devbox` resolve the pinned Rust toolchain for `aarch64-darwin` from a binary cache?**~~ **ANSWERED (2026-08-04): yes.** At nixpkgs rev `a5cbcfe954791221bfffe2307f7d1a1bf61a871e` every pinned package has a prebuilt `aarch64-darwin` binary in `cache.nixos.org` — narinfo HTTP 200 on all of them (including the real `z7m83pil…-rustc-1.97.1` at ~285 MiB compressed behind the 712-byte `rustc-wrapper`), a full runtime-closure BFS over 30 paths with zero misses, and `nix path-info --store https://cache.nixos.org` resolving all seven roots. M4 stays in scope. Two caveats carried into the design: the closure is ~1.5 GiB unpacked / ~330 MiB compressed nar cold, so the macOS job caches the nix store from the start rather than as a follow-up; and "substitutable" is not "runs correctly" — the `aarch64-darwin` lock entries have still never been exercised by any CI job (`ci.yml:294-295`).
2. **How much of `Swatinem/rust-cache`'s behaviour has to be reimplemented?** Key derivation is easy; the pruning of stale and incremental artifacts is what keeps the archive small enough to be worth restoring. Restoring a bloated `target/` can be slower than a cold build. **PARTIALLY ANSWERED (2026-08-04)** by what `.semaphore/ci.sh` does and does not do. Reimplemented: keys from `Cargo.lock` plus a toolchain fingerprint (taken from `devbox.json` + `devbox.lock` + `gcloud/flake.lock` rather than `rustc -vV`, so a key exists before a toolchain does), the three cached locations, prefix-fallback restores, `CARGO_INCREMENTAL=0` plus deletion of any incremental dir, dropping `registry/src` and `registry/index/*/.cache`, and write-once-per-key-on-pass. Not reimplemented: pruning `target/` down to artifacts the current dependency graph references, build-script output pruning, and invalidation on toolchain-env changes beyond the lockfiles. Consequence, stated rather than hidden: `target/` grows monotonically per key lineage, so the script caps the archive at `CARGO_TARGET_CACHE_MAX_MB` (4000) and logs loudly when it declines to store — a measured local `target/` is 8.5 GiB (1.4 GiB of it incremental), and Semaphore documents a per-project cache quota in the same order of magnitude. Whether the cap trips on a CI-shaped `target/` is one of the first things M5 will find out.
3. **Do the task entrypoints eventually become GHA's interface too, or stay Semaphore-only?** This is the M6 decision. Staying Semaphore-only leaves the version-skew bug unfixed on the platform that actually gates merges, which would be an odd place to stop.
4. **Semaphore pins 1.97.1 while GHA floats on `stable` — does that confound the comparison?** A red Semaphore job could be a genuine 1.97-vs-current difference rather than a Semaphore problem. Worth deciding up front whether to pin GHA temporarily for the measurement window, which is the one edit to `ci.yml` that might be justified.
5. **Is the `r1` native-ARM runner worth a separate job?** `release.yml` cross-compiles `aarch64-unknown-linux-gnu` through `cross` (Docker) and `aarch64-crossbuild-check.yml` guards it; Semaphore's `r1` machines are native ARM, which could remove the cross machinery. But `r1` has no Docker support, so anything container-shaped fails there. Out of scope for this PRD, potentially its own.
6. ~~**Does `cargo xtask linkage-check` need anything not already in `devbox.json`?**~~ **ANSWERED (2026-08-04): no.** It needs `cargo`/`rustc` to build `xtask-linkage-check` and nothing else; all seven checks plus rule 7's in-process docs generator read `tests/CATALOG.md`, the `tests/` tree and `xtask/linkage-check/m2.allowlist` from the working tree. It does **not** shell out to `git` — the three `Command::new("git")` sites in `xtask/linkage-check/src/list_tests.rs` belong to `cargo xtask list-tests` (which diffs the branch against `origin/main` and is invoked by the release flow, not by CI). The Semaphore pipeline still asks for a full-history checkout (`SEMAPHORE_GIT_DEPTH=0`) as cheap insurance for any future git-shaped xtask step, and notes that a full clone does not by itself guarantee an `origin/main` remote-tracking ref.

## Work Log

### 2026-08-04 — M1, M2, and the M3/M4 pipeline file

**BLOCKED, and this is the headline.** M3, M4, M5 and the M6 retrofit decision all require a **Semaphore Cloud project connected to `vfarcic/dot-agent-deck`**. There is none, there is no `sem` CLI and no credentials on the machine this was written on, and creating an account or authenticating was explicitly out of bounds. So the pipeline file exists and has never executed. Everything below distinguishes what was measured from what was only written.

**M1 — entrypoints (done).** `Taskfile.yml` gained `ci-fmt`, `ci-clippy`, `ci-build` (`--release`), `ci-build-debug`, `ci-test`, `ci-linkage-check`, `ci-audit` and an aggregate `ci`. Seven, not the six this PRD originally listed: `ci.yml` also runs a plain debug `cargo build` on macOS and Windows (`ci.yml:256`, `:223`), and the Semaphore macOS block needs that shape rather than the release build, or an 8 GB machine gets handed strictly more work than its GHA counterpart and the M5 macOS comparison is confounded before it starts. `cargo-audit@0.22.2` is now pinned in `devbox.json`/`devbox.lock` for all four systems and `scripts/devbox-smoke.sh` runs `cargo audit --version`, so the one CI job that can see a devbox diff covers it. `ci.yml:272`'s `cargo install cargo-audit --locked` is untouched, by design.

**M2 — Darwin gate (done, verdict GO).** At nixpkgs rev `a5cbcfe954791221bfffe2307f7d1a1bf61a871e`, every pinned package has a prebuilt `aarch64-darwin` binary in `cache.nixos.org`: narinfo HTTP 200 on all of them (including the real `z7m83pil…-rustc-1.97.1`, ~285 MiB compressed, sitting behind the 712-byte `rustc-wrapper`), a full runtime-closure BFS over 30 paths with zero misses, and `nix path-info --store https://cache.nixos.org` resolving all seven roots. The stop condition did not trigger; `build-macos` stays in scope. Cold closure is ~1.5 GiB unpacked / ~330 MiB compressed nar, so the macOS job caches the nix store from the first version.

**M3/M4 — pipeline file written, not green.** `.semaphore/semaphore.yml` (three blocks — `build`, `build-macos`, `security` — each `dependencies: []` so they run in parallel like the GHA jobs) plus `.semaphore/ci.sh` (nix install via the Determinate installer, nix-store cache as a sudo-created tar plus `nix-store --dump-db`/`--load-db` re-registration, devbox install, `devbox run` provisioning, and the cargo/`target/` cache). Build steps go through `devbox run -- task ci-*`, never raw cargo. Verified only that the YAML parses and that every key used appears in Semaphore's documented v1.0 schema. `git diff .github/workflows/` is empty.

**Open Question 6 — answered no.** `cargo xtask linkage-check` needs only `cargo`/`rustc`, both already pinned. It does **not** shell out to `git`: the three `Command::new("git")` sites in `xtask/linkage-check/src/list_tests.rs` belong to `cargo xtask list-tests`, a different subcommand that CI never runs. The pipeline still takes a full-history checkout as insurance and says why.

**Two measurements worth keeping, neither of them flattering.** The full `devbox.json` profile closure is **4.4 GiB unpacked across 442 store paths** on `x86_64-linux` — roughly 3× the Rust-toolchain-only figure M2 measured, because the dev shell also carries ffmpeg, asciinema/agg, the Google Cloud SDK, `gh`, `vals` and `upcloud-cli`, none of which any CI step uses. And a long-lived local `target/` is **8.5 GiB** (8.0 GiB debug, 1.4 GiB of that incremental; 528 MiB release). Both land directly on the "CI gets slower" risk: trimming the CI closure is the largest available win and is deliberately not attempted here, since it would mean restructuring the contributor shell's own `devbox.json`.

**`cargo --version` reports 1.97.0 under the 1.97.1 pin.** Recorded under Technical Approach so a provider-diff analysis does not misread it as a failed pin.

**Left deliberately wrong:** `ci.yml:289` still says devbox pins `rustc 1.97.0`. Same factual drift that was fixed in this PRD's prose, but fixing it means editing `ci.yml`, which this PRD may not do. It is a follow-up.

### 2026-08-04 — Created

Came out of a Semaphore evaluation discussion. Three decisions worth preserving, because they are the parts a fresh session is most likely to undo:

1. **The order is inverted on purpose.** The obvious plan is "make GHA provider-agnostic, then porting is trivial." Rejected: marketplace actions do not exist on Semaphore, so the toolchain has to be hand-provisioned there regardless. Writing the devbox layer on the Semaphore side first means no extra work, keeps GHA as an unmodified control, and produces the numbers that justify (or kill) the retrofit before touching a working gate.
2. **The version-skew fix is the real motivation**, not portability. Portability of the invocation layer is cheap and mostly cosmetic; caching stays provider-specific either way. `devbox.json` pinning vs `rust-toolchain@stable` floating is an actual latent bug.
3. **No publish conditionals anywhere.** `ci.yml` has no side effects and `release.yml` is out of scope, so the double-publish problem does not arise at this scope. If a full release pipeline is ever demoed on Semaphore, do it by pointing at **separate destinations** — the existing `NAME=dot-agent-deck-beta` channel in `Taskfile.yml`, a throwaway tap, a `:semaphore-test` image tag — rather than by flagging the real one. A flag can be misconfigured; a different destination structurally cannot collide. Note also that `release.yml`'s `concurrency: group: release` gives zero protection against a second CI provider, since concurrency domains do not span systems.
