# Versioning and the "breaking" definition

> **Developer / maintainer reference.** This page documents release-process and contract discipline. It is intentionally excluded from the published documentation site and renders as plain Markdown here on GitHub.

`dot-agent-deck` ships the TUI and the daemon as the **same binary in two modes**, so the only cross-process contract that matters is the attach protocol between them and the handler semantics behind it. The daemon deliberately outlives the TUI (agents survive detach/sleep/network/machine-switch), which means a long-running daemon can keep serving an *older* contract after you upgrade the binary in place. That runtime skew — a newer TUI meeting an older still-running daemon — is the whole reason the word "breaking" needs a precise, project-specific meaning here.

## What "breaking" means in this project

**Breaking = a change to the TUI↔daemon protocol/handler contract such that an older and a newer build cannot safely interoperate.** This is *not* the same axis as "user-facing breaking change". It is specifically the cross-process contract between the two modes of the binary.

A change is breaking in this sense when it would make an older peer mis-behave against a newer one. That includes the obvious structural cases (a new request variant, a changed field shape) **and** the subtle ones: **semantic breaks behind a stable wire** — a field whose *meaning* changes, or a role-map value type that shifts — where the bytes still deserialize but one side now interprets them wrongly. The classic symptom is a delegate signal that the stale daemon silently no-ops because it doesn't understand the newer shape.

Generic user-facing breakage (a renamed flag, a removed command, a changed default) is a normal product change and is **not** what the `breaking` changelog type is reserved for. Use it only for the cross-process compatibility break described above.

## How a break is detected and marked

Detection is layered — there is deliberately **no** mechanical CI schema/snapshot gate, because a type snapshot catches structural breaks (already covered below) but is blind to the residual risk, which is semantic.

1. **`PROTOCOL_VERSION` is the structural marker.** Any wire-shape break must bump `PROTOCOL_VERSION` in `src/daemon_protocol.rs`. This is mandatory and non-negotiable. What it is *not*, today, is a fatal floor: **no call site currently refuses on a `PROTOCOL_VERSION` difference.** The local attach path never compared it ([#405](https://github.com/vfarcic/dot-agent-deck/issues/405)), and `connect`'s laptop↔remote comparison was removed by [#491](https://github.com/vfarcic/dot-agent-deck/issues/491) because it compared two constants that never share a wire — `connect` ssh's in and runs the *remote* binary's TUI against the *remote* daemon, so both ends were the same install by construction. The pairing that can genuinely skew, a newer TUI meeting an older still-running local daemon, is caught by `build_version_handshake`'s `DAD_BUILD_ID` comparison instead; declining its restart prompt attaches with no version check at all, which is what #405 tracks. Bump the constant anyway: the bump is what makes the skew nameable and is the input the refusal in #405 will read. See the enforcement note on `PROTOCOL_VERSION` in `src/daemon_protocol.rs`.
2. **A human-marked `.breaking.md` changelog fragment for semantic breaks.** A same-wire/different-meaning change cannot be detected mechanically, so the author marks it: add a `changelog.d/<issue>.breaking.md` fragment (the `breaking` towncrier type defined in `pyproject.toml`). This is also the signal a future compatibility-classifying handshake would consume.
3. **A cross-version manual test** (see below) for any PR that touches the daemon, the protocol, orchestration, or hooks.

## The 0.x bump policy

While the major version is `0`, the bump rules are deliberately shifted down one level from standard SemVer so the **minor digit tracks compatibility**, not features:

| Change | While `0.x` | From `1.0` onward |
|---|---|---|
| breaking (protocol/handler contract) | **minor** (`0.31.x → 0.32.0`) | major |
| feature (new user-facing functionality) | **patch** (`0.31.1 → 0.31.2`) | minor |
| bugfix | **patch** | patch |

The consequence worth internalizing: while in `0.x`, a feature-only release is a *patch* release, and **only a protocol-breaking change bumps the minor**. So the minor digit stops meaning "has new features" and starts meaning "compatibility broke" — if `0.31.x` becomes `0.32.0`, an older peer can no longer safely talk to a `0.32.x` one. This is already implemented on the release side in the vendored `.claude/skills/dot-ai-tag-release/analyze.sh` (the `breaking → minor`, `feature/bugfix → patch` recalibration); the table above is the policy that script encodes.

## Cross-version manual-test discipline

`PROTOCOL_VERSION` catches structural breaks and the `.breaking.md` fragment records semantic ones the author already knows about — but the failure mode we most want to catch is a semantic break the author *didn't* realize they introduced. The backstop is a manual test, required before merging any PR that touches the daemon / protocol / orchestration / hooks:

1. Build the PR branch's binary.
2. Start a daemon from the **previous release** (the last tagged binary), and start an agent under it.
3. Run the PR-branch **TUI** against that older daemon and confirm the core flows still work end to end: a **delegate** still routes, and **hooks** (work-done, status updates) still arrive.

**Step 2's *"start an agent under it"* is load-bearing, and step 3 has a decline step.** Get either wrong and this gate silently measures nothing:

- **Keep that agent running when the PR-branch TUI starts.** On a `build_version` mismatch with **zero** agents, the newer binary takes `MismatchAction::SilentRestart` (`src/build_version_handshake.rs`, PRD #161): it SIGTERMs the older daemon and lazy-spawns its own, with no prompt and no output, regardless of TTY. Every check then passes against a *new* daemon and you report a clean cross-version gate having actually measured new-against-new — nothing on screen says the daemon was replaced.
- **Decline the mismatch prompt.** With agents present the handshake asks instead of restarting; press any key other than `S`. Declining returns `HandshakeOutcome::ProceedOnExisting`, which attaches to the older daemon unchanged (PRD #161 D4, the never-strand rule). Accepting restarts the daemon on the new binary and lands you in the same false pass, with the agents stopped as well.
- **Confirm before trusting the result:** the daemon serving the new TUI should still have the previous release's binary as its `exe`.

If delegate or hooks silently stop flowing, the change broke the contract behind a stable wire — bump `PROTOCOL_VERSION` (if the wire shape moved) and/or add a `.breaking.md` fragment so the release is versioned as a compatibility break. This step is enforced in-repo by **CLAUDE.md permanent instruction 12**, which every agent in this project loads and follows; the canonical `dot-ai-prd-done` skill in the `dot-ai` repo carries the same check, and syncing the vendored copy under `.claude/skills/dot-ai-prd-done/` is a separate follow-up.

## Where the reported version comes from, and how to inject it

`Cargo.toml` carries `version = "0.1.0"` as a **placeholder** — it is not bumped per release, so it doubles as the last-resort fallback (step 3 below). For any build expected to be *newer* than `v0.1.0` — every build of `main`, and every release since the first — a reported version of `0.1.0` means the two resolution steps above it both failed. That is issue #250. Note that `0.1.0` is not a *unique* failure signature: `v0.1.0` is a real published tag and GitHub release, so a correct build of that tag legitimately reports `0.1.0`, as does an explicit `DAD_VERSION=0.1.0`. Read the number against the version the build was supposed to produce rather than treating it as failure on its own. The two values a build actually bakes in are emitted by `build.rs` as compile-time env vars: `DAD_VERSION` (the SemVer string behind `--version`, the upgrade nudge, `remote add` pre-flight and the PRD #161 negotiation) and `DAD_BUILD_ID` (`<version>-g<short-sha>[-dirty]`, the finer-grained identifier behind the PRD #103 daemon-restart handshake).

Each is resolved in this order (issue #250):

1. a **pre-set `DAD_VERSION` / `DAD_BUILD_ID` in the build environment**, if present and valid;
2. **git** — `git describe --tags --abbrev=0` and `git rev-parse --short HEAD`, the path a release build and a normal dev checkout take;
3. **`CARGO_PKG_VERSION`** / `<version>-unknown` — the last-resort placeholder.

Step 1 exists for builds where git metadata is absent and step 2 therefore degrades: a source tarball, a shallow or `.git`-less clone, `cargo install --git`, or a sandboxed distro/Nix build. Without an injection those builds silently claim to be `0.1.0`, which every version-aware code path then mis-handles. Packagers should build with the real version in the environment:

```sh
DAD_VERSION=1.2.3 DAD_BUILD_ID=1.2.3-nixpkgs cargo build --release
```

Four properties of that path are worth knowing:

- **An injected `DAD_VERSION` must be valid SemVer.** After a leading `v`/`V` is stripped (the strip a git tag gets) and surrounding whitespace trimmed, the remainder is parsed by `semver::Version::parse` — deliberately the *exact* parser `src/version.rs` uses, so the set of values the build script accepts is by construction the set the binary can parse. Accepted: an `X.Y.Z` core of `u64` fields without leading zeros, plus an optional `-<prerelease>` and an optional `+<build>` suffix (`1.2.3`, `v0.35.2`, `0.25.0-alpha.0`, `1.2.3-alpha+meta` are all fine). Rejected: anything that parser rejects (`1.2`, `1.2.3.4`, `release-7`, `01.2.3`) — because `src/version.rs` does `semver::Version::parse(env!("DAD_VERSION")).expect(…)` and would panic the binary at startup. A rejected injection is *ignored with a `cargo:warning`* and resolution falls through to the next step rather than emitting it.
- **An injected `DAD_BUILD_ID` must be a single line drawn from a bounded alphabet:** ASCII alphanumerics plus `.`, `-`, `+` and `_`. There is no *format* to conform to — the build id is only ever compared as an opaque string by the handshake, so `1.2.3-nixpkgs` or `1.2.3-1ubuntu1` are fine — but the value is emitted into a `cargo:rustc-env=` directive and later rendered into terminals and logs, and Cargo parses build-script output line by line. An interior newline would therefore let the value append a *second* build-script directive (`rustc-env`, `rustc-cfg`, `rustc-link-arg`) — so surrounding whitespace is trimmed and anything outside the alphabet is rejected with a `cargo:warning`, falling through to the git-composed id. The same line-protocol rule applies to every value `build.rs` emits, and a rejected value is escaped (never interpolated raw) into the warning that reports it.
- **Falling all the way through to the placeholder emits a `cargo:warning`**, so a mis-provisioned source build is diagnosable at build time instead of silently reporting the oldest possible release.
- **Changing an injected value invalidates the cached build** (`cargo:rerun-if-env-changed`), so a second build with a different `DAD_VERSION` does not quietly keep the first one.

The repo's own release pipeline uses this seam: `release.yml`'s build job passes the `prepare` job's version as `DAD_VERSION` (and names it in `CROSS_BUILD_ENV_PASSTHROUGH` so it reaches the `cross` container too). On a tag push that is the same string `git describe` resolves in the tagged checkout, so nothing changes; on a `workflow_dispatch` release from `main` the tag does not exist yet, and without the injection the artifact would bake the *previous* release's version and be published under the new tag.

The `prepare` job **validates that version as SemVer before it writes it to `GITHUB_OUTPUT`**, and fails the release if it does not match. The reason is the fall-through above: `build.rs` *ignores* an invalid injection rather than failing the build, so a malformed `workflow_dispatch` input or an oddly-named tag would be dropped silently and the artifact would resolve its version from git — or from the `0.1.0` placeholder — while still being published under the requested tag. That is issue #250 again, so the gate rejects the value at the one place it enters the pipeline instead of letting every consumer (the changelog script, the changelog commit message, the release tag, the `task` publish steps, the docs workflow) inherit it. Two details of the check matter if you ever edit it: it is deliberately at least as strict as `semver::Version::parse` — including a digit bound that keeps the three core fields inside `u64` — so nothing it accepts can be rejected downstream; and it pins `LC_ALL=C` and spells its character classes out as explicit ASCII enumerations, because a bracket *range* in a bash regex is resolved through the active locale's collation, which had let values like `1.2.3-é` and full-width digits through on a runner whose locale happened to be `en_US.UTF-8`.

The resolution logic itself lives in `build_version_resolve.rs` at the repo root as pure functions, compiled as an out-of-line module by both `build.rs` (`mod build_version_resolve;`) and `tests/build_version.rs` (`#[path = "../build_version_resolve.rs"] mod …`) — `cargo test` cannot reach code inside a build script, so this is what makes the order testable. A module rather than an `include!` on purpose: rustfmt does not follow `include!`, so the shared file would otherwise escape the mandatory `cargo fmt --check` gate. `lifecycle/version/001` (`tests/e2e_version_injection.rs`) covers the whole chain by rebuilding the binary with the vars injected, twice over changing one variable at a time to pin the rerun directives.

## Where this lives across repos

- The **0.x recalibration** in `analyze.sh` and the generic changelog-fragment guidance are generically correct and belong in the shared skill **source** (the `prompts` repo); the vendored copy here is kept in sync.
- The **dot-agent-deck-specific** parts — this breaking definition and the protocol-surface specifics — stay local (this doc + the `pyproject.toml` comment).
- The **cross-version manual-test step** and the "did this change the TUI↔daemon contract?" prompt are enforced in-repo by **CLAUDE.md permanent instruction 12** (loaded by every agent, including the `release` role that runs `/prd-done`). The same check lives canonically in the `dot-ai` repo's `dot-ai-prd-done` skill; the copy under `.claude/skills/dot-ai-prd-done/` here is vendored, and folding the check into that vendored copy + its upstream source is a separate follow-up.
