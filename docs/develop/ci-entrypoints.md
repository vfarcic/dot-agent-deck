# CI entrypoints — running a CI step locally

Each of the seven **cargo invocations** `ci.yml` runs has a `task` entrypoint in [`Taskfile.yml`](../../Taskfile.yml), so the commands that gate a merge are also invokable outside GitHub Actions — from a local devbox shell, from an agent, or from a second CI provider that has no equivalent of the marketplace actions `ci.yml` depends on (PRD #376 M1).

What the wrappers cover is exactly those seven commands, and nothing else `ci.yml` does. They do **not** cover toolchain or devbox provisioning (marketplace actions on GHA, `.semaphore/ci.sh` on Semaphore), the `devbox` smoke job, the `changes` skip gate, or `windows-cross-check`. `task ci` is narrower still — see the table.

## The entrypoints

| Task | Command | `ci.yml` job that owns the step |
|---|---|---|
| `task ci-fmt` | `cargo fmt --check` | `build` (`ci.yml:114`) |
| `task ci-clippy` | `cargo clippy -- -D warnings` | `build` (`ci.yml:115`), and the same invocation in `build-macos` / `build-windows` |
| `task ci-build` | `cargo build --release` | `build` (`ci.yml:116`) |
| `task ci-build-debug` | `cargo build` | `build-macos` (`ci.yml:256`) and `build-windows` (`ci.yml:223`) |
| `task ci-test` | `cargo nextest run` | `build` (`ci.yml:135`), and the same invocation in `build-macos` / `build-windows` |
| `task ci-linkage-check` | `cargo xtask linkage-check` | `build` (`ci.yml:144`) — Linux only |
| `task ci-audit` | `cargo audit` | `security` (`ci.yml:273`) |
| `task ci` | all of the above except `ci-build-debug`, in order | the Linux `build` job's cargo steps plus `security`'s |

Each command is copied verbatim from `ci.yml` — no extra flags, no `--locked` where `ci.yml` has none, and `cargo nextest run` rather than `cargo test` for the reason `ci.yml:117-134` documents at length. Keeping them character-identical is what makes a future divergence show up as a diff in `Taskfile.yml` rather than as a behaviour difference somebody has to bisect.

`ci-build` and `ci-build-debug` are separate because `ci.yml` genuinely runs both: the Linux `build` job builds `--release`, while the macOS and Windows jobs build debug. The aggregate `task ci` mirrors the Linux gate and therefore uses the release build.

## Running them

Anything that resolves `cargo`, `rustfmt`, `clippy`, `cargo-nextest` or `cargo-audit` needs the devbox shell, because those are pinned in [`devbox.json`](../../devbox.json) and generally are not on a bare `PATH`:

```sh
devbox shell
task ci-clippy        # one step
task ci                # the Linux gate's cargo steps plus cargo audit, in order
```

Or without entering the shell:

```sh
devbox run -- task ci-clippy
```

`task ci` stops at the first failing step, so it reports the same first failure CI would.

## Why one task per step

There is deliberately no single opaque `task ci` that is the *only* way in. `ci.yml` splits its work across jobs (`build`, `build-macos`, `build-windows`, `security`) precisely so a platform-specific or step-specific break stays independently visible — see the comments at `ci.yml:191-210` and `ci.yml:233-244` — and the entrypoints preserve that granularity. `task ci` exists as a convenience for "run the Linux gate's cargo steps before pushing"; when something breaks, invoke the one step directly. A wrapper that hides which step failed would trade away the thing the job split was built to give you.

## `ci.yml` is deliberately NOT routed through these tasks

This looks like an unfinished job. It is not. Do not wire `.github/workflows/ci.yml` to call `task ci-*`.

Two reasons, and the second one expires:

1. `ci.yml` is the merge gate, and Renovate automerges cargo patch and devbox digest/pin/patch/minor bumps on it being green (`renovate.json`). Changes to it put unreviewed merges at risk for no user-visible benefit.
2. For as long as PRD #376's measurement is open, `ci.yml` is the **experimental control**. The Semaphore pipeline is measured against it. Change both at once and a slow Semaphore job can no longer be attributed — machine, nix bootstrap, or the hand-rolled cache. The retrofit is a follow-up gated on those numbers, including "no" as an acceptable outcome.

## The version skew these entrypoints exist to fix

`devbox.json` pins `rustc@1.97.1`, `cargo@1.97.1`, `clippy@1.97.1`, `rustfmt@1.97.1`, `cargo-nextest@0.9.140` and `cargo-audit@0.22.2`. `ci.yml` installs its toolchain with `dtolnay/rust-toolchain@stable` (`ci.yml:109`) and nextest with `taiki-e/install-action@nextest` (`ci.yml:113`), both of which resolve to whatever is current on the day the job runs. So the local gate and CI compile with **different toolchains today**, and the divergence widens silently until a new release introduces a lint or a behaviour change — the first symptom being a clippy failure nobody can reproduce locally.

`ci.yml`'s `devbox` job comment (`ci.yml:286-292`) argues the other side deliberately: the floating toolchain answers "does this still compile on current stable", which is what `release.yml` ships with. Both positions are real; the point of the PRD is that we currently get the floating one *by accident* rather than by choice on the platform that gates merges. The entrypoints make either choice implementable.

One thing not to misread when comparing toolchain versions across providers: under the `cargo@1.97.1` pin, `rustc --version` reports `1.97.1` while `cargo --version` reports `cargo 1.97.0 (c980f4866 2026-06-30)`. That is nixpkgs' 1.97.1 derivation reporting a 1.97.0 internal version, not a pin that failed to resolve.

## Before connecting a Semaphore project

**This is a checklist to work through at project creation, not advice to keep in mind.** Several of the pipeline's real safety properties are provider-side settings that nothing in this repository can enforce or even observe — no project exists, so no reviewer has been able to audit them. Do these before the first run:

1. **Disable tag triggers in the project settings.** Semaphore treats a tag push as a trigger by default, so a release tag would start build/audit/cache work on this pipeline alongside `release.yml` and report a second status — the release-shaped behaviour PRD #376 rules out. Every block in `semaphore.yml` also carries `run: when: "tag !~ '.*'"`, but that guard has never been evaluated by a runner and, even when it works, only skips the *work*: Semaphore still creates a workflow and reports a status. The project setting is the fix; the YAML is defence in depth.
2. **Confirm forked-PR cache isolation still holds.** The cache threat model in `.semaphore/ci.sh` relies on Semaphore denying forked-PR workflows access to the project cache. Verify that is still the platform's behaviour for the plan and project type in use, rather than assuming it from the docs.
3. **Keep untrusted refs out of the trusted cache namespace — ideally with a separate project.** Cache keys are derived from repository content, so a branch that leaves the lockfiles untouched derives the default branch's keys while running its own `ci.sh`. Semaphore's cache CLI exposes `store`, `delete` and `clear` project-wide, so such a branch can replace an archive a later trusted job restores. `ci.sh` only writes the cache on a push to the default branch, but **that check lives in the script the untrusted ref controls, so it is defence in depth, not an authorization boundary.** The boundary has to be provider-side: separate projects for trusted and untrusted refs, or cache namespaces an untrusted ref cannot reach.
4. **Verify what `cache restore` does with absolute and `..` members, on a real agent, before connecting a project that shares a cache across trust levels.** `ci.sh` restores into a scratch directory and adopts only an allowlisted member that passes validation, but the provider's extractor runs *before* any of that code sees the result. If Semaphore's `cache restore` honours an absolute or `..` member, the archive has already written outside the scratch directory by then, and a scratch-only cleanup can neither detect nor undo it. That behaviour is currently **unverified** — nothing here has run on an agent. Until it is established, or until the restore is sandboxed so that only the scratch directory is writable, the scratch-plus-allowlist mechanism is defence in depth and **not** containment of a crafted archive.
5. **Do not make Semaphore a required status check.** Renovate automerges cargo patch and devbox bumps on green CI (`renovate.json`); an unproven pipeline does not go in that path. This is also PRD #376 scope.

## The Semaphore pipeline

[`.semaphore/semaphore.yml`](../../.semaphore/semaphore.yml) plus [`.semaphore/ci.sh`](../../.semaphore/ci.sh) mirror `ci.yml`'s `build`, `build-macos` and `security` jobs on Semaphore Cloud, provisioning the toolchain from `devbox.json` by hand-rolling what `jetify-com/devbox-install-action` does on GHA (install nix, install devbox, realise the environment) and then running the `task ci-*` entrypoints through `devbox run`. Both installers are pinned to an exact version and digest-verified before they execute rather than piped from a floating URL into a shell.

Status, stated plainly: **the pipeline file exists and has never run.** No Semaphore Cloud project is connected to `vfarcic/dot-agent-deck`, so it has been verified only by parsing the YAML, re-reading it against Semaphore's documented v1.0 schema, and exercising `ci.sh`'s key derivation and cache logic against a stand-in `cache` CLI locally. Nothing about the nix bootstrap, the real cache CLI or the macOS job has been observed working. Both files flag the individual steps whose behaviour is assumed rather than measured. It is a spike: not a required status check anywhere, with no promotions, no deployment targets and no publishing.

**There is no nix-store cache, deliberately.** Every job substitutes the devbox closure from `cache.nixos.org`, which nix verifies by signature, so **cold bootstrap is the baseline M5 measures** and there is no warm-bootstrap number to collect yet. The first version cached `/nix/store` through a privileged `sudo tar -C /` round trip; that was removed rather than repaired, because it was both unsafe (an unverified archive extracted at `/` as root, then registered with `nix-store --load-db`) and dead (it stored an absolute path, and Semaphore's `cache store` strips the leading `/`, so the restore could never hit). `.semaphore/ci.sh` carries the full argument, including the signed `nix copy` shape to use if store caching is ever added back — and why it plausibly loses to the CDN for this workload.

The cargo cache that remains is a partial stand-in for `Swatinem/rust-cache`: key derivation and the three cached locations are reimplemented, pruning `target/` down to what the current dependency graph references is not, and the prefix-fallback restores were implemented and then removed — they widened the cache-poisoning surface, at the cost of making a `Cargo.lock` bump a cold build. `.semaphore/ci.sh` documents exactly which behaviours are and are not covered, restores into a scratch directory rather than over the checkout, and caps the `target/` archive size rather than silently pushing a multi-GB restore that can cost more than the build it replaces.

**What the scratch directory and the member allowlist are worth, stated exactly.** A restore is untrusted input: cache keys are derived from repository content, so they authenticate content and not the producer. `ci.sh` therefore unpacks each key into a scratch directory and moves exactly one allowlisted member into place, only after checking that no path component on either side of the rename is a symlink, that the member is a real directory containing nothing but real files and real directories, and that nothing in it is hardlinked to a file outside it; a `cache restore` that does not exit 0 is treated as a miss, so a half-written member is never adopted. That limits what an archive can get **adopted into place**. It does **not** confine what the provider's extractor may already have written, because `cache restore` runs first and its handling of absolute and `..` members is unverified — see item 4 of the checklist above, which is a prerequisite before this pipeline is trusted with a cache shared across trust levels.

## Windows

Windows keeps rustup and marketplace actions. nix has no native Windows support (`ci.yml:193-195`) and Semaphore Cloud has no hosted Windows runner, so `build-windows` cannot be provisioned from `devbox.json` at all. Two toolchain-provisioning paths is the accepted end state here, not a gap for somebody to close later — which is worth knowing, because Windows is where the load-bearing platform bugs live (`portable-pty` is held at `=0.8.1` for a ConPTY reason) and it is the platform the pinning fix cannot reach.

See also: [Checking a Windows compile locally](windows-cross-check.md).
