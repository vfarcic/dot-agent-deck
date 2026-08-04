# CI entrypoints — running a CI step locally

Every step CI runs has a `task` entrypoint in [`Taskfile.yml`](../../Taskfile.yml), so the commands that gate a merge are also invokable outside GitHub Actions — from a local devbox shell, from an agent, or from a second CI provider that has no equivalent of the marketplace actions `ci.yml` depends on (PRD #376 M1).

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
| `task ci` | all of the above except `ci-build-debug`, in order | the Linux `build` job plus `security` |

Each command is copied verbatim from `ci.yml` — no extra flags, no `--locked` where `ci.yml` has none, and `cargo nextest run` rather than `cargo test` for the reason `ci.yml:117-134` documents at length. Keeping them character-identical is what makes a future divergence show up as a diff in `Taskfile.yml` rather than as a behaviour difference somebody has to bisect.

`ci-build` and `ci-build-debug` are separate because `ci.yml` genuinely runs both: the Linux `build` job builds `--release`, while the macOS and Windows jobs build debug. The aggregate `task ci` mirrors the Linux gate and therefore uses the release build.

## Running them

Anything that resolves `cargo`, `rustfmt`, `clippy`, `cargo-nextest` or `cargo-audit` needs the devbox shell, because those are pinned in [`devbox.json`](../../devbox.json) and generally are not on a bare `PATH`:

```sh
devbox shell
task ci-clippy        # one step
task ci                # every step the Linux gate runs, in order
```

Or without entering the shell:

```sh
devbox run -- task ci-clippy
```

`task ci` stops at the first failing step, so it reports the same first failure CI would.

## Why one task per step

There is deliberately no single opaque `task ci` that is the *only* way in. `ci.yml` splits its work across jobs (`build`, `build-macos`, `build-windows`, `security`) precisely so a platform-specific or step-specific break stays independently visible — see the comments at `ci.yml:191-210` and `ci.yml:233-244` — and the entrypoints preserve that granularity. `task ci` exists as a convenience for "run the lot before pushing"; when something breaks, invoke the one step directly. A wrapper that hides which step failed would trade away the thing the job split was built to give you.

## `ci.yml` is deliberately NOT routed through these tasks

This looks like an unfinished job. It is not. Do not wire `.github/workflows/ci.yml` to call `task ci-*`.

Two reasons, and the second one expires:

1. `ci.yml` is the merge gate, and Renovate automerges cargo patch and devbox digest/pin/patch/minor bumps on it being green (`renovate.json`). Changes to it put unreviewed merges at risk for no user-visible benefit.
2. For as long as PRD #376's measurement is open, `ci.yml` is the **experimental control**. The Semaphore pipeline is measured against it. Change both at once and a slow Semaphore job can no longer be attributed — machine, nix bootstrap, or the hand-rolled cache. The retrofit is a follow-up gated on those numbers, including "no" as an acceptable outcome.

## The version skew these entrypoints exist to fix

`devbox.json` pins `rustc@1.97.1`, `cargo@1.97.1`, `clippy@1.97.1`, `rustfmt@1.97.1`, `cargo-nextest@0.9.140` and `cargo-audit@0.22.2`. `ci.yml` installs its toolchain with `dtolnay/rust-toolchain@stable` (`ci.yml:109`) and nextest with `taiki-e/install-action@nextest` (`ci.yml:113`), both of which resolve to whatever is current on the day the job runs. So the local gate and CI compile with **different toolchains today**, and the divergence widens silently until a new release introduces a lint or a behaviour change — the first symptom being a clippy failure nobody can reproduce locally.

`ci.yml`'s `devbox` job comment (`ci.yml:286-292`) argues the other side deliberately: the floating toolchain answers "does this still compile on current stable", which is what `release.yml` ships with. Both positions are real; the point of the PRD is that we currently get the floating one *by accident* rather than by choice on the platform that gates merges. The entrypoints make either choice implementable.

One thing not to misread when comparing toolchain versions across providers: under the `cargo@1.97.1` pin, `rustc --version` reports `1.97.1` while `cargo --version` reports `cargo 1.97.0 (c980f4866 2026-06-30)`. That is nixpkgs' 1.97.1 derivation reporting a 1.97.0 internal version, not a pin that failed to resolve.

## The Semaphore pipeline

[`.semaphore/semaphore.yml`](../../.semaphore/semaphore.yml) plus [`.semaphore/ci.sh`](../../.semaphore/ci.sh) mirror `ci.yml`'s `build`, `build-macos` and `security` jobs on Semaphore Cloud, provisioning the toolchain from `devbox.json` by hand-rolling what `jetify-com/devbox-install-action` does on GHA (install nix, restore the nix store from cache, install devbox, realise the environment) and then running the `task ci-*` entrypoints through `devbox run`.

Status, stated plainly: **the pipeline file exists and has never run.** No Semaphore Cloud project is connected to `vfarcic/dot-agent-deck`, so it has been verified only by parsing the YAML and re-reading it against Semaphore's documented v1.0 schema. Nothing about the nix bootstrap, the store cache, the cargo cache or the macOS job has been observed working. Both files flag the individual steps whose behaviour is assumed rather than measured. It is a spike: not a required status check anywhere, with no promotions, no deployment targets and no publishing.

It is also not a free second gate. It caches two things — the nix store and `target/` plus the cargo registry — and the second one is only a partial stand-in for `Swatinem/rust-cache`: key derivation, the cached locations and prefix-fallback restores are reimplemented, but pruning `target/` down to what the current dependency graph references is not. `.semaphore/ci.sh` documents exactly which behaviours are and are not covered, and caps the `target/` archive size rather than silently pushing a multi-GB restore that can cost more than the build it replaces.

## Windows

Windows keeps rustup and marketplace actions. nix has no native Windows support (`ci.yml:193-195`) and Semaphore Cloud has no hosted Windows runner, so `build-windows` cannot be provisioned from `devbox.json` at all. Two toolchain-provisioning paths is the accepted end state here, not a gap for somebody to close later — which is worth knowing, because Windows is where the load-bearing platform bugs live (`portable-pty` is held at `=0.8.1` for a ConPTY reason) and it is the platform the pinning fix cannot reach.

See also: [Checking a Windows compile locally](windows-cross-check.md).
