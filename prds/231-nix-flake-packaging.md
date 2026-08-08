# PRD #231: Package dot-agent-deck as a Nix flake

**Status**: Implementation complete (M1-M5); PR pending
**Priority**: Medium
**Created**: 2026-08-06

## Problem Statement

The only documented way to install dot-agent-deck is the Homebrew tap: `brew tap vfarcic/tap && brew install dot-agent-deck` (`README.md:12`). Nix, home-manager and NixOS users have no first-class path, so each of them either hand-wraps the prebuilt binary and pins a fresh `sha256` on every release, or skips the tool. That is the ask in [#231](https://github.com/vfarcic/dot-agent-deck/issues/231).

Flakes are not foreign to the repo: `gcloud/flake.nix` already exists. It is a separate wrapper for a different purpose and does not package the deck, so it neither answers the request nor gets in its way.

**The seam a packager needs is already built.** Issue [#250](https://github.com/vfarcic/dot-agent-deck/issues/250) added `DAD_VERSION` / `DAD_BUILD_ID` injection so packaging automation can bake the correct version into the binary (`build.rs:22-30`, with the pure resolution logic in `build_version_resolve.rs`), and `VersionSource::Placeholder` exists precisely to flag that a source build silently claiming to be `0.1.0` is the bug. A Nix flake is exactly the consumer #250 anticipated, and there is none. `release.yml:177-195` already does that injection for release artifacts, passing the tag-derived version as `DAD_VERSION`.

## Solution Overview

Add a root `flake.nix` that builds from source with [crane](https://github.com/ipetkov/crane) against the committed `Cargo.lock`, so there is no per-release hash for anyone to maintain, and expose the outputs a consumer actually reaches for: `packages.default`, `apps.default` for `nix run`, `devShells.default`, `checks.default`, `overlays.default` and `homeModules.default`. Then `nix run github:vfarcic/dot-agent-deck` works with nothing installed, `inputs.dot-agent-deck.url = "github:vfarcic/dot-agent-deck"` makes it a flake input for home-manager and NixOS users, and home-manager users get `programs.dot-agent-deck` for the package plus their `config.toml` and `keybindings.toml`. This complements devbox rather than replacing it: devbox stays the dev environment, the flake is the consumer install.

The version reaches the binary through the seam #250 built, so a source build reports the release it actually is, and CI gains a guard so that pin cannot go stale unnoticed. Five milestones, each independently shippable, M1 being the one that resolves the issue.

## Scope

### In Scope

- Root `flake.nix` and `flake.lock`.
- Outputs: `packages.default` (also named `dot-agent-deck`), `apps.default`, `devShells.default`, `checks.default`, `overlays.default`, `homeModules.default`.
- A home-manager module exposing `programs.dot-agent-deck` with `enable`, `package`, `settings` and `keybindings`, managing `config.toml` and `keybindings.toml`.
- A `nix` job in `.github/workflows/ci.yml` running `nix flake check -L --all-systems`.
- A `release.yml` guard step asserting the flake's pinned version equals the tag being released, and that the pin still reaches the build.
- An "Install via Nix" section in `README.md` and in `docs/installation.md`, including the home-manager module.

### Out of Scope

- **A NixOS module.** The system-level module has no per-user config to render, so it would add an option surface that duplicates `environment.systemPackages = [ ... ]` for no gain. The overlay and `packages.default` already cover NixOS.
- **`session.toml`, `remotes.toml` and `schedules.toml` in the home-manager module.** See Technical Approach; the first two are written by the application and the third resolves its path differently from its neighbours.
- **Running `dot-agent-deck hooks install` from an activation script.** It mutates other tools' configuration files, outside home-manager's ownership, and does not roll back with a generation switch.
- **Upstreaming into nixpkgs.** A separate conversation with different maintainers.
- **Replacing devbox.** The flake complements it, and the issue says so explicitly.
- **Running the test suites inside the flake.** Neither tier can run in a Nix sandbox; see Technical Approach.
- **`gcloud/flake.nix`.** Untouched, different purpose.

## Technical Approach

### The version, and why it is pinned

`Cargo.toml:11` is `version = "0.1.0"` and it is a permanent placeholder: `release.yml`'s own comments call it "the `0.1.0` placeholder", while real releases are tagged `v0.35.7` and friends. The build script resolves the version in this order: injected env, then `git describe --tags --abbrev=0`, then `CARGO_PKG_VERSION`. A `nix run github:vfarcic/dot-agent-deck` build gets a source tarball with no `.git`, so the git step is unavailable and the fallback yields `0.1.0`, which is the exact misreporting `VersionSource::Placeholder` was added to name.

So the flake pins `version = "0.35.7"` in a let-binding and passes it as `env.DAD_VERSION`. This is the standard nixpkgs pattern for a project whose manifest version is not its release version. To stop the pin going stale, `release.yml` gains a guard step asserting the flake's version equals the tag being released, so a forgotten bump fails the release loudly instead of shipping a binary that misreports itself. That is the same class of bug #250 was opened for, so the guard is in the spirit of the existing work. The `dot-ai-tag-release` skill bumps the pin as a step of cutting a tag, before the tag exists, because the guard runs in `prepare` and would otherwise halt a release whose tag had already been pushed.

The guard makes a second assertion for a reason worth stating: matching the pin against the tag proves only that a `version = "X.Y.Z";` line exists and agrees. It does not prove that value reaches the binary, and `DAD_VERSION = "0.1.0";` next to a correct pin passes the first check while shipping the exact bug the check exists to catch. So it also asserts one anchored `DAD_VERSION = version;` line.

`env.DAD_BUILD_ID` is **not** the version. It is `${version}-g${self.shortRev}`, the same `<version>-g<sha>` shape `build.rs` composes from git metadata, because it is per-commit rather than per-release. `nix run github:vfarcic/dot-agent-deck` is an unpinned moving ref, so two builds of different commits both report `0.35.7`; `src/build_version_handshake.rs` decides "your daemon is stale, restart it" by comparing build ids, and a constant would make every such upgrade look like nothing had changed. It falls back to `dirtyShortRev`, then to `unknown`, mirroring the `-unknown` sentinel `resolve_build_id` degrades to.

### Build shape

The build is split in two by crane. `craneLib.buildDepsOnly` compiles the dependency closure over a stub source that crane synthesises from the workspace manifests, and `craneLib.buildPackage` then compiles this crate on top of those artifacts. Both take the same `commonArgs` (the filtered `src`, `strictDeps = true`, `cargoExtraArgs = "-p dot-agent-deck"`), and the shared set is kept deliberately small: everything in it is part of the dependency closure's cache key.

That is why `pname`, `version`, `env.DAD_VERSION` and `env.DAD_BUILD_ID` are set on `buildPackage` and nowhere else. All of them move per release, and the build id moves per commit. In `commonArgs` they would give the dependency derivation a new store path every time the version was bumped, throwing away the aws-lc-sys C build on each release, which is most of what crane was adopted for. Nothing in the dependency graph reads either variable; only this repo's `build.rs` does. `cargoArtifacts` therefore takes its name from `Cargo.toml`'s permanent `0.1.0` placeholder and stays put.

The workspace is `.` plus `xtask/docs`, `xtask/linkage-check` and `xtask/spec` (`Cargo.toml:1-7`). Only the root package ships, so both halves pass `cargoExtraArgs = "-p dot-agent-deck"`. The default binary is `src/main.rs`; there is no `[[bin]]` section to name.

`edition = "2024"` (`Cargo.toml:12`) needs rustc 1.85 or newer. nixos-unstable ships a recent stable rustc, so no `rust-overlay` input is required and the input set stays small.

reqwest is configured without default features and with `rustls` (`Cargo.toml:46-49`), so there is no openssl. The crypto backend behind that is **aws-lc-rs**, not ring: #269 moved rustls onto it, and `.github/workflows/ci.yml:149` records that its build script "compiles ~600 C files". `Cargo.lock` carries `aws-lc-sys 0.43.0` to match, so this build does compile C inside the Nix sandbox and a cold build is genuinely expensive. `ring 0.17.14` is in the lock file too but is never built: it is reachable only through `quinn-proto` and `rustls-webpki`, and a cold `nix flake check -L` log shows `aws-lc-sys` compiling with no `ring` line at all. On Darwin only the usual libiconv / apple-sdk wiring may surface. Note that `semver` is listed in both `[dependencies]` and `[build-dependencies]`, so `build.rs` and the test crate can both see it.

Because the whole tree is expensive to rebuild, the derivation does not use a bare `src = ./.`. A `lib.fileset` filter narrows the source to what `cargo build -p dot-agent-deck` actually reads, so editing a doc, a workflow or this PRD leaves the derivation hash untouched. The filter and crane are complementary rather than alternatives: the filter decides how often a rebuild is triggered at all, crane decides how much a triggered rebuild has to redo. The filter also has a second job under crane, since `buildDepsOnly` needs the workspace members' manifests present in `src` to resolve the workspace at all; `xtask/` was already in the fileset for the dev-dependency reason above, so nothing had to be widened for it.

### Systems

`x86_64-linux`, `aarch64-linux` and `aarch64-darwin`, listed explicitly rather than taken from `flake-utils.lib.eachDefaultSystem`. The fourth default system is `x86_64-darwin`, which the pinned nixpkgs (26.11 unstable) has dropped: importing it does not merely fail to build, it `throw`s during evaluation. Intel Macs keep the release binaries and the Homebrew tap.

### Why the flake does not run the tests

`doCheck = false`, and `checks.default` is the package build. The fast tier needs `cargo-nextest`, and the e2e tier needs live `claude` / `opencode` CLIs plus network access (`CONTRIBUTING.md`). Neither is available in a Nix sandbox. Correctness stays gated by the existing CI matrix; the flake's job is to prove the thing **builds** reproducibly from source.

### CI

`.github/workflows/ci.yml` today runs `changes` (a path filter), `build`, `windows-cross-check`, `build-windows`, `build-macos`, `security` and `devbox`. The new `nix` job sits alongside these and runs `nix flake check -L --all-systems`, so a nixpkgs bump that breaks the build is caught here rather than by the first user who types `nix run`. `--all-systems` matters: a bare `nix flake check` narrows to the runner's own system and would leave the two non-x86_64-linux outputs unevaluated. The installer runs with `determinate: false`, so the job proves the flake on upstream Nix, which is what consumers have.

### The home-manager module

`homeModules.default` sits outside `eachSystem` next to `overlays.default`, because a home-manager module is a function of the evaluating configuration and receives that configuration's own `pkgs`. It exposes `programs.dot-agent-deck` with four options: `enable`, `package`, `settings` (rendered to `config.toml`) and `keybindings` (rendered to `keybindings.toml`). Both attribute sets are typed with `pkgs.formats.toml { }` and rendered with its `generate`, so they are freeform and anything the two files accept can go in them without the module tracking their schemas.

**The module uses `home.file`, not `xdg.configFile`, and that is load-bearing.** The deck resolves its config root with `config_dir()`, which is HOME-anchored and deliberately ignores `$XDG_CONFIG_HOME` (`src/platform/paths.rs:370-382`). The test `config_dir_is_home_anchored_and_ignores_xdg_config_home` (`src/platform/paths.rs:521-531`) asserts exactly that: with `XDG_CONFIG_HOME` pointed elsewhere, `config_dir()` is still `home_dir()/.config/dot-agent-deck`. `xdg.configFile` follows `xdg.configHome`, so for any user who has moved it the module would write these files somewhere the deck never reads. Nothing would error; the deck would run on its defaults and the user would be left wondering why their config had no effect. The module carries a comment saying so, because the "helpful" conversion to `xdg.configFile` is the obvious wrong move for a reviewer who does not know the path rule.

`package` defaults to `pkgs.dot-agent-deck or (mkDotAgentDeck pkgs)`, so the overlay and the module resolve to the same derivation and a user who applies both does not end up with two builds of the tool in one closure. Building against the evaluating nixpkgs rather than the pinned one is the same tradeoff the overlay already documents, and a user who wants the pinned build sets `package` explicitly.

Each file is written only when its attribute set is non-empty, so `enable = true` on its own installs the package and plants no empty `config.toml` over one the user already had.

Three neighbours in the same directory are deliberately unmanaged. `session.toml` is runtime state the deck writes itself, and home-manager symlinks its files read-only out of the store, so managing it would stop the deck saving its workspace. `remotes.toml` is written imperatively by `remote add` and has the same problem. `schedules.toml` is the odd one out on paths: it is the single file that *does* honour `$XDG_CONFIG_HOME` (`schedules_path`, via `xdg_config_home()` at `src/platform/paths.rs:390-402`), so managing it correctly needs `xdg.configHome` handling the other two files must not get, and it goes to a follow-up rather than being written to the wrong place. `dot-agent-deck hooks install` is not run from an activation script either: it edits other tools' configuration, which home-manager does not own and cannot roll back on a generation switch, so it stays an imperative one-off the user runs knowingly.

### Docs surface

`README.md` is deliberately minimal: a `## Quick Start` at line 9 with the brew one-liner, then `## Documentation` pointing at the docs site. The real install page is `docs/installation.md`. Both get an "Install via Nix" section carrying the `nix run` one-liner and a flake-input snippet, and the README's stays as short as the brew line it sits beside.

## Milestones

Each is independently shippable. M1 is the one that resolves the issue.

- [x] **M1 the flake.** `flake.nix` plus `flake.lock`. `nix build`, `nix run` and `nix develop` all work, and the built binary reports `0.35.7` rather than `0.1.0`.
- [x] **M2 CI.** A `nix` job in `ci.yml` running `nix flake check -L --all-systems`, so a nixpkgs bump that breaks the build is caught.
- [x] **M3 the version guard.** A `release.yml` step asserting the flake version matches the tag, and that the pin is still wired to the build as `DAD_VERSION`.
- [x] **M4 docs.** The README Quick Start section and `docs/installation.md`.
- [x] **M5 the home-manager module.** `homeModules.default` exposing `programs.dot-agent-deck`, managing `config.toml` and `keybindings.toml` through `home.file`, plus its section in `docs/installation.md`.

## Key Files

- `flake.nix` (new, root) and `flake.lock` (new).
- `.github/workflows/ci.yml` (the `nix` job) and `.github/workflows/release.yml` (the version guard).
- `README.md` and `docs/installation.md`.
- For the home-manager module's path rule, not modified: `src/platform/paths.rs` (`config_dir` at :370-382, `xdg_config_home` at :390-402, and the test at :521-531).
- For reference, not modified: `build.rs`, `build_version_resolve.rs`, `gcloud/flake.nix`.

## Design Decisions

### 2026-08-06: Root flake, not a `nix/` subdir

`nix run github:vfarcic/dot-agent-deck` and `inputs.x.url = "github:..."` both resolve the flake at the repo root. A subdir forces every consumer to append `?dir=nix`, which is exactly the friction the issue asks to remove. The issue author noted the layout is the maintainer's call; root is the recommendation.

### 2026-08-06: crane, not `rustPlatform.buildRustPackage`

crane builds the dependency closure as a derivation of its own, so a change to this project's source recompiles this project's source and reuses everything under it. `rustPlatform.buildRustPackage` has a single vendor-and-build step, so any source change redoes the whole thing.

This decision was made the other way first, and the reasoning was wrong. The first draft picked `buildRustPackage` on the grounds that it is the nixpkgs default, needs no extra flake input, and gets the issue's core ask (no per-release hash) from `cargoLock.lockFile = ./Cargo.lock`. It also credited the tree with no C to compile. That last part is false: `aws-lc-sys` compiles roughly 600 C files (see Technical Approach), and under `buildRustPackage` every edit to a `.rs` file in this repo paid for them again. Review caught it, and the ranking flips once the premise is corrected. Dependency-level caching is worth an extra flake input on a tree with a C build that size.

Both the "no per-release hash" ask and the input cost survive the switch. crane vendors from the committed `Cargo.lock` too, so no `sha256` appears anywhere in `flake.nix`, and crane declares no flake inputs of its own, so it adds exactly one node to `flake.lock` and nothing to keep in sync with nixpkgs.

The `lib.fileset` source filter stays. It was the mitigation the first draft reached for instead of crane, but it is not the same lever and it is not made redundant: the filter stops a rebuild being triggered by a file the compiler never reads, and crane shrinks what a genuine source change has to rebuild.

Measured after the switch, on an aarch64-darwin laptop, `nix flake check -L --all-systems`:

| Store state | Time |
|---|---|
| nixpkgs, toolchain and vendored crate sources present, no compiled artifacts | 1m47s |
| after a source-only edit | 35s |
| fully cached | 2.4s |

The 35s row is the one crane moved. That build log carries exactly one `Compiling` line, `dot-agent-deck` itself, and zero `aws-lc` lines: the dependency derivation is reused as a store path and its artifacts are decompressed rather than rebuilt. Under `buildRustPackage` the same edit paid the 1m47s row every time.

### 2026-08-06: Pin the version in the flake and guard it in CI

Reasoning above. The rejected alternative was deriving the version from `self.shortRev`: it needs zero maintenance, but it can never report `0.35.7`, so a user could not tell which release they were running. Pinning plus a loud CI guard keeps the reported version true and makes a missed bump fail the release rather than ship.

### 2026-08-06: `doCheck = false`

Reasoning above. The flake proves the build; the CI matrix proves the behaviour.

### 2026-08-06: `home.file`, never `xdg.configFile`, in the home-manager module

The reasoning is in Technical Approach. It is recorded as a decision because the failure mode is silent: `xdg.configFile` reads as the more modern choice, it is what a reviewer will reach for, and for every user who has not moved `xdg.configHome` it behaves identically. For the ones who have, the deck would quietly ignore its own managed config. The module comment cites `src/platform/paths.rs:521-531` so the next person to consider the swap finds the test that forbids it.

The same asymmetry is why `schedules.toml` is not managed. It honours `$XDG_CONFIG_HOME` while `config.toml` and `keybindings.toml` do not, so one module cannot write all three with a single placement rule, and mixing two rules in a first module is how the wrong one gets copy-pasted later.

## Success Criteria

Phrased as the re-runnable checks, so they can be scripted end to end:

- `nix flake check -L --all-systems` passes.
- `nix build .#default -L` succeeds, and `./result/bin/dot-agent-deck --version` reports the real version rather than `0.1.0`.
- `nix run . -- --help` exits 0, which is what proves `apps.default`.
- `nix develop -c rustc --version` works, which is what proves the devShell.
- `nix flake show` lists `packages`, `apps`, `devShells` and `checks`.
- `homeModules.default` evaluates against a real home-manager configuration with `enable = true`, and the resulting `home.file` entries are `.config/dot-agent-deck/config.toml` and `.config/dot-agent-deck/keybindings.toml` even when `xdg.configHome` points elsewhere.
- The upstream CI matrix plus the new `nix` job are green on the branch.

## Open Questions For The Maintainer

To be repeated in the PR description:

1. **Root versus `nix/` layout.** Root is recommended above; please confirm, since the issue leaves it to you.
2. **Keeping the flake version pin in sync at release.** The CI guard makes a miss loud rather than silent, but it is still one more line to bump on a tag.
3. **Is the home-manager module's option surface the one you want?** It is `enable`, `package`, `settings` and `keybindings`, and it manages two files. A NixOS module is not included, on the grounds that a system-level module would only wrap `environment.systemPackages`.
4. **Should the flake be covered by the release workflow at all**, or stay purely a source-build convenience?
