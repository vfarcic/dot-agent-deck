# PRD #740: Publish the desktop GUI as an unsigned alpha artifact

**Status**: Not started
**Priority**: Medium
**Created**: 2026-08-30

## Problem Statement

The desktop GUI ([PRD #176](https://github.com/vfarcic/dot-agent-deck/issues/176), `desktop/`) landed on `main` in `daf94f0` and has never been packaged. The only way to run it is to clone the repository, install Tauri's system prerequisites, run `pnpm install`, build a matching `dot-agent-deck` daemon from the same checkout, and launch `pnpm tauri dev`. That is a maintainer's workflow, not a user's, and it is a maintainer's workflow even for the maintainers — nobody is dogfooding the GUI from an installed app, so nothing that only breaks in a packaged build is being caught.

The machinery to fix this is half-present and macOS-shaped. `pnpm bundle:app` exists but hardcodes `--bundles app`, which is the macOS `.app` target only. `desktop/scripts/prepare-sidecar.sh` stages the matching daemon as a Tauri sidecar so the bundle carries its own daemon, but names it without an `.exe` suffix, so it cannot work on Windows. Nothing in `.github/workflows/` has ever run `tauri build` on any trigger — `grep -rniE 'tauri' .github/` returns three lines, all of them comments in `ci.yml`.

What is *not* missing is buildability. `ci.yml` compiles `dot-agent-deck-desktop` on all three platforms today: the `build` job installs the WebKitGTK dependency set on Ubuntu, and `build-windows` and `build-macos` both run `cargo nextest run --workspace`, which selects the desktop crate. The gap is bundling and publishing, not compiling.

## Solution Overview

Every tag publishes desktop GUI bundles alongside the existing CLI binaries, as clearly-labelled unsigned alpha assets, on a path that runs *beside* the CLI release rather than in front of it — so a broken bundler can never delay or block an ordinary release.

Users clear the OS trust warning by hand (`xattr -dr com.apple.quarantine` on macOS). Removing that friction is [#757](https://github.com/vfarcic/dot-agent-deck/issues/757) and is deliberately not attempted here; the two have different blockers, and this one has nothing to procure.

## Scope

### In Scope

- macOS arm64 (`aarch64-apple-darwin`) `.dmg`, built on `macos-latest`.
- Linux x86_64 (`x86_64-unknown-linux-gnu`) `.deb`, built on `ubuntu-latest`. (AppImage was planned and dropped on evidence — see Decision 7.)
- Injecting the real release version into the Tauri bundle, replacing the hardcoded `0.1.0`.
- New `release.yml` jobs that bundle in parallel with the existing CLI matrix and upload to the release *after* it is created, so `finalize` never waits on them.
- Asset naming, checksums, and a fixed release-notes section that states plainly that these are unsigned alpha builds and how to clear the OS warning.
- Fixing `desktop/scripts/prepare-sidecar.sh` to handle the `.exe` suffix on Windows triples, with fast-tier test coverage — groundwork only, shipped without a Windows artifact (see Decision 1).

### Out of Scope

- **Windows artifacts.** Deferred behind [#741](https://github.com/vfarcic/dot-agent-deck/issues/741) — see Decision 1. The `.exe` groundwork lands; the artifact does not.
- **Signing, notarization, and SmartScreen reputation.** That is [#757](https://github.com/vfarcic/dot-agent-deck/issues/757) in full.
- **x86_64 macOS.** Measured out, not assumed out — see Decision 2.
- **A user-facing install page.** The alpha ships unadvertised on purpose; [#765](https://github.com/vfarcic/dot-agent-deck/issues/765) writes the page when the GUI leaves alpha. See Decision 11.
- **aarch64 Linux.** See Decision 8.
- **`.rpm`.** `deb` plus `AppImage` covers Debian/Ubuntu natively and everything else portably; nobody has asked for `rpm`.
- **Auto-update.** Tauri's updater needs signing keys and an update endpoint; it belongs with #757 or later.
- **Homebrew cask / Scoop bucket entries for the GUI.** The existing `finalize` job publishes a Homebrew formula and Scoop manifest for the CLI. Adding the GUI to a package manager implies a support commitment an unsigned alpha has not earned.
- **The logo.** [#746](https://github.com/vfarcic/dot-agent-deck/issues/746) replaces the artwork; this PRD ships with what is in the tree (see Decision 9).
- **The experimental feature flag.** CLAUDE.md rule 9 asks the question; the answer is no (see Decision 10).

## Technical Approach

### Decision 1 — Windows is deferred, because a Windows GUI has no daemon it can reach

The issue frames Windows as blocked on two things: the `prepare-sidecar.sh` `.exe` fix, and [#754](https://github.com/vfarcic/dot-agent-deck/issues/754) leaving the GUI degraded. Both are real. But the decisive question is simpler, and it is the one to ask first: **what daemon would a Windows GUI actually talk to?**

There are two candidate answers. Today neither works.

**A local Windows daemon does not exist as a download.** `release.yml`'s matrix has exactly four entries — `x86_64`/`aarch64-unknown-linux-gnu` and `x86_64`/`aarch64-apple-darwin` — and none is `*-pc-windows-*`; `gh release view v0.38.0` confirms five assets, none Windows. The daemon *compiles* and runs natively on Windows ([#42](https://github.com/vfarcic/dot-agent-deck/issues/42) and [#163](https://github.com/vfarcic/dot-agent-deck/issues/163) landed the platform backends), so a Windows user could build one — but "build the daemon from source" is precisely the friction this PRD exists to remove. Publishing one is the substance of [#164](https://github.com/vfarcic/dot-agent-deck/issues/164), which is not started.

**A remote daemon is the intended shape, and Windows cannot reach one.** [#741](https://github.com/vfarcic/dot-agent-deck/issues/741) — "connect the desktop GUI to any daemon, anywhere, configured in the app" — is where that lives, and remote attach is already *proven*: #741 records a measured session on 2026-08-29 attaching to a real remote daemon over `ssh -N -L /tmp/dad-remote.sock:/run/user/1000/dot-agent-deck-attach.sock`, listing nine remote agents and streaming their PTYs live. Transport, as #741 puts it, is not the obstacle.

But that proof is **Unix client to Unix daemon**, and the transport is `cfg`-dispatched rather than universal. `src/platform/ipc/` hides "the Unix-domain-socket / Windows-named-pipe split behind a single `cfg`-dispatched API", and `attach_socket_path()` (`src/platform/paths.rs:1147-1180`) returns a filesystem socket path on Unix and `\\.\pipe\dot-agent-deck-{user}-attach` on Windows. A Windows client speaks named pipes; a forwarded Unix socket is not one, and OpenSSH on Windows does not forward a remote Unix socket onto a local named pipe. Independently of that, the desktop crate contains **no remote code at all** — `grep -rniE "\bssh\b|\btcp\b|remote" desktop/src-tauri/src/*.rs` returns zero lines — so the app cannot establish the tunnel itself either. #741 lists exactly that as work still to do: "the app must manage the tunnel itself", because launched from Finder there are no environment variables to carry a socket path.

So a Windows alpha shipped today is an application that can connect to nothing: no daemon to download, and no transport to a remote one.

**#754 is the third reason, not the first.** Even once a Windows GUI *can* reach a Linux daemon, the launch guard tests the **client's** OS (`desktop/src-tauri/src/dto.rs:491`, mirrored in `desktop/src/lib/platform.ts`) rather than the daemon's, so it would wrongly refuse workflow launch in precisely the Windows-GUI-to-Linux-daemon configuration that makes a Windows build worth having. Fixing it properly puts the daemon's OS in the handshake, which is a CLAUDE.md rule 12 contract change — it would drag this packaging PRD across the breaking-change line and turn a patch into a minor.

**The dependency is #741, not #164.** This is worth stating precisely because the intuitive answer is the wrong one. #164 would deliver a downloadable *Windows daemon*, which only enables the local story — a Windows user running their agents on their own Windows box. The shape actually wanted here is a Windows GUI driving a Linux daemon, and for that #164 is not needed at all. What is needed is #741, plus an answer inside #741 to the Windows transport question, which its placeholder body does not currently address. Windows GUI packaging sequences after that, and the transport question belongs in #741 rather than here.

**What lands here anyway: the `.exe` fix.** It is small, the issue names it explicitly, and leaving it out means the next person re-derives it. Two places, both fatal: cargo emits `dot-agent-deck.exe` on `*-pc-windows-*` so the copy's *source* is wrong, and Tauri resolves external binaries as `{path}-{triple}{ext}` with `ext = ".exe"` on Windows (`tauri-utils-2.9.3/src/resources.rs:52-59`) so the *destination* name is wrong too. It ships as verified-by-reading groundwork plus a fast-tier test, not as a shipped artifact.

One thing deliberately not claimed: that a Windows bundle *must* carry a sidecar. A GUI that only ever attaches to a remote daemon needs no local daemon binary, and a `tauri.windows.conf.json` platform overlay clearing `externalBin` is the plausible mechanism. That was not verified — the check was cut short — so it is recorded as an open avenue rather than a fact. It does not change the decision, because the blocker is reachability, not packaging.

An unrelated bug is worth recording before it is lost: `Taskfile.yml:226` builds the Scoop manifest by grepping `checksums.txt` for `dot-agent-deck-windows-amd64.exe`, which is never built, so the **published Scoop manifest currently carries an empty hash and a 404 URL**. It belongs to #164, it is shipping broken today, and adding a Windows leg would fix it incidentally.

### Decision 2 — macOS arm64 and Linux x86_64 ship together in one slice

The issue suggests macOS arm64 alone as a first slice. The reviewer's read was Linux first, as the cheapest honest target. Both are arguing about the wrong axis: the expensive part is the *scaffolding* — a new matrix job, a pnpm install, a sidecar stage, a `tauri build`, an asset rename, an upload path, and the `finalize` interaction — and that scaffolding is shared. Once it exists for one platform, a second is one matrix row plus its platform-specific dependency step.

Splitting them means paying the review-and-merge cost twice for one workflow. Doing both means the first shipped alpha covers both dogfooding platforms. The genuinely platform-specific unknowns are small and independent: macOS needs a `.icns` that does not exist in the tree yet, and Linux needs bundler packages beyond the compile set already installed in `ci.yml`.

**x86_64 macOS is not built**, and the evidence is this project's own release telemetry rather than a guess about the market. Across the seven releases `v0.36.0` through `v0.38.0`, GitHub's per-asset download counts are:

| Asset | Downloads across v0.36.0–v0.38.0 |
| --- | --- |
| `dot-agent-deck-darwin-arm64` | 77 |
| `dot-agent-deck-linux-amd64` | 78 |
| `dot-agent-deck-linux-arm64` | 11 |
| `dot-agent-deck-darwin-amd64` | **1** |

One download, in v0.36.0, across seven releases. The CLI has shipped an Intel Mac binary the whole time and essentially nobody has taken it, so the "an Intel audience is already assumed" argument for the GUI turns out to be assuming an audience that is not there. Building a `.dmg` for it would mean a second macOS matrix leg, a cross-compile from the arm64 runner, and a bundle nobody can test on real hardware — for a platform Apple stopped selling in 2023 and whose users cannot run an arm64 build under Rosetta in any case (Rosetta translates x86 to arm, not the reverse, so this is genuinely all-or-nothing for them).

If an Intel Mac user ever asks, adding the matrix row is an afternoon. Until then it is a bundle built for one download. (The same table raises a fair question about whether the *CLI* should keep shipping `darwin-amd64` — noted as an observation, deliberately not proposed here, and not this PRD's business.)

### Decision 3 — the version is injected, never committed

`desktop/src-tauri/tauri.conf.json:4` is a literal `"version": "0.1.0"`. That value is not cosmetic: it lands in `Info.plist`'s `CFBundleShortVersionString`, the `.deb` version field, and the MSI/NSIS product version. A bundle cut today would be labelled `0.1.0` while carrying a sidecar correctly stamped `0.38.x`.

This is the same shape of bug that [#250](https://github.com/vfarcic/dot-agent-deck/issues/250) exists to prevent one layer down, and the fix mirrors the existing seam rather than inventing one. `release.yml`'s `prepare` job already resolves and SemVer-gates the version once, and already injects it into the CLI build as `DAD_VERSION`, which `build.rs` consumes ahead of `git describe` and ahead of `Cargo.toml`'s placeholder. The bundling job does the same thing for the Tauri config: it merges the resolved version into the config at build time, so nothing in the tree is rewritten and no commit carries a version number.

Two mechanisms are available and the implementation picks whichever actually works with `@tauri-apps/cli` 2.11.4. Preferred is a second inline `--config` JSON argument carrying only `{"version": "<resolved>"}`, stacked on the existing `--config src-tauri/tauri.bundle.conf.json`; Tauri merges configs by RFC 7396 JSON Merge Patch (`tauri-utils-2.9.3/src/config/parse.rs:185`), so a one-key overlay is well-defined. If stacked `--config` arguments turn out not to be supported, the fallback is a `jq` rewrite of a copy of the config inside the job. Either way the job **asserts** the produced bundle carries the expected version rather than trusting it, in the same spirit as `release.yml:375-406`, which greps the CLI artifact's rodata to prove `DAD_VERSION` actually took.

Local developer builds keep saying `0.1.0`, exactly as `Cargo.toml` does, and for the same documented reason: the version belongs to the tag, not to the tree. Note the overlap with [#487](https://github.com/vfarcic/dot-agent-deck/issues/487), which wants the `0.1.0` placeholders retired repo-wide; this PRD deliberately does not pre-empt that decision, it just stops the bundle from shipping a meaningless number.

### Decision 4 — bundling runs on every tag, in parallel, and never gates the release

The issue asks for an opt-in gate "so it cannot slow or break an ordinary tag". The issue title asks for the GUI on *every* release. PRD #176's M5.1 asks for packaging "excluded from the default release". Three requirements that read as conflicting are satisfied at once by job topology rather than by a flag:

```
prepare
  ├─ build (existing 4-target CLI matrix) ── finalize (creates the Release) ── docs
  └─ desktop-bundle (NEW: macos-latest + ubuntu-latest)
         └─ desktop-publish (NEW, needs: prepare, finalize, desktop-bundle)
```

`desktop-bundle` hangs off `prepare`, not off `build`, so it runs concurrently with the CLI matrix and adds nothing to the critical path. `finalize` does **not** list it in `needs:`, so the GitHub Release object and all five existing CLI assets are created on exactly today's ~6-minute path whether the bundler succeeds, fails, or is skipped. `desktop-publish` runs afterwards and attaches the GUI assets to the already-published release with `gh release upload --clobber`.

A bundler failure therefore turns the workflow run red while leaving the release complete and correct for CLI users. That is deliberate: `continue-on-error: true` would keep the run green and make a silently missing artifact indistinguishable from a healthy one. The escape hatch is a `skip_desktop` boolean `workflow_dispatch` input, so a release can still be cut deliberately without the GUI when the bundler is known-broken — an explicit human act, not a silent default.

This does mean PRD #176's M5.1 wording ("excluded from the default release") no longer describes the intent, and it should be amended to say what M5.1 was actually protecting: that the GUI is labelled preview and cannot compromise the CLI release. Both properties survive; only the mechanism changed.

### Decision 5 — alpha status is carried by the asset, not by the release flag

The issue says "mark the assets prerelease". GitHub has no per-asset prerelease flag, and the repo's only prerelease mechanism is release-wide: `finalize`'s "Detect channel" step (`release.yml:435-455`) sets `prerelease=true` when the version contains a `-`, and routes Homebrew and Scoop to a parallel `dot-agent-deck-beta` channel. Using it would demote the entire CLI release to a prerelease to label a GUI bundle. That is the wrong trade by a wide margin, so **the release-level `prerelease` flag is left exactly as it is today.**

Alpha status is instead carried in three places a user actually looks:

- **The filename.** `dot-agent-deck-desktop-alpha-<os>-<arch>.<ext>` — the word is unavoidable at the moment of download.
- **The release notes.** A fixed section appended to the generated changelog body, naming the assets as unsigned alpha builds and giving the exact `xattr -dr com.apple.quarantine` invocation.
- **The docs page.** One short page, linked from the release notes.

### Decision 6 — `productName` stays "Agent Deck"; the release asset gets renamed

`tauri.conf.json:3` is `"productName": "Agent Deck"`, with a space. That is the right name for the dock, the menu bar and the window title, and changing it to satisfy a shell glob would be the tail wagging the dog. But it means the bundler emits names like `Agent Deck_0.38.1_amd64.deb`, which two existing pieces of machinery would mishandle: `finalize`'s upload glob is `dist/dot-agent-deck-*` with `fail_on_unmatched_files: true`, and `task checksums` runs `shasum -a 256 dot-agent-deck-*` (`Taskfile.yml:116`) — a pipeline that a space in a filename breaks on its own terms.

The bundling job therefore renames each artifact to the scheme in Decision 5 before uploading, and stages them in a `dist-desktop/` directory kept separate from `dist/`, so `finalize`'s glob and checksum step never see them at all. Desktop checksums go into their own `checksums-desktop-alpha.txt`; unsigned artifacts make integrity verification more valuable, not less, so this is not the corner to cut.

### Decision 7 — `dmg` for macOS, `deb` for Linux (AppImage dropped on evidence)

`--bundles app` produces a **directory**, not a file, so it cannot be a release asset without a zip step. `dmg` is a single file, is the idiomatic macOS delivery, and gives the drag-to-Applications gesture users already know — so it replaces `app` rather than being zipped alongside it. On Linux, `deb` is native for the Debian/Ubuntu machines being dogfooded on.

**AppImage was in this plan and has been removed, on evidence from the first real bundle run.** It failed (`failed to run linuxdeploy`) while the `.deb` from the same invocation succeeded and verified clean. Flakiness alone would only have justified a retry; what justifies removal is what the failure exposed about how that bundler works. Producing an AppImage downloads **five** third-party artifacts at bundle time — `AppRun-x86_64`, `linuxdeploy-x86_64.AppImage`, `linuxdeploy-plugin-appimage-x86_64.AppImage`, and two plugin shell scripts — and the two scripts come from `raw.githubusercontent.com/tauri-apps/…/master/…`, a **mutable ref**. That means a release build executes whatever happens to be on someone else's default branch at tag time, while producing an artifact users install and run. That is a supply-chain property, not a reliability one, and it is the wrong trade for the marginal reach an AppImage buys over a `.deb` on the machines actually being dogfooded.

Re-adding it is one word in the matrix once those inputs are pinned. It should be a deliberate decision with the pinning done, not a default.

Note `bundle.active` is `false` in `desktop/src-tauri/tauri.conf.json:31` and only the bundle overlay flips it to `true`. An invocation that forgets `--config src-tauri/tauri.bundle.conf.json` produces **no bundle at all and still exits 0**, so the job asserts the expected files exist rather than relying on the exit code.

### Decision 8 — aarch64 Linux is out of scope

The CLI ships `linux-arm64` by cross-compiling in a `cross` container. That container has no WebKitGTK, so it cannot build a Tauri app, and there is no free arm64 Linux runner in the existing matrix to build one natively. Making it work means either provisioning a sysroot or moving to `ubuntu-*-arm` runners — a self-contained piece of work with its own unknowns, and not one an unsigned alpha needs to have solved. Revisit if anyone actually asks.

### Decision 9 — ship with the icons in the tree

`desktop/src-tauri/icons/` holds `icon.png` (512×512), `icon.ico` and `icon.svg`, and the artwork is genuinely ours — a terminal window with "A"/"D" letterforms, not the Tauri template. Two gaps: the config references only `icons/icon.png` (`tauri.conf.json:32`), so the `.ico` is unused, and **there is no `.icns` anywhere in the tree**, which is what macOS `.app`/`.dmg` bundles want. Whether `tauri-bundler` 2.11.4 synthesizes a missing `.icns` from the PNG or hard-errors is genuinely unknown — the bundler ships inside the prebuilt npm CLI and could not be read — so M1 determines it empirically and generates the missing sizes if needed.

[#746](https://github.com/vfarcic/dot-agent-deck/issues/746) will replace the artwork later, and the intent is explicitly to **re-ship the bundles carrying the new icon as part of that PRD** rather than to leave the alpha looking like a draft forever. Waiting for it would block an alpha on a design task; changing a dock icon between alpha builds is an acceptable cost, and #746 itself notes the dependency is soft.

### Decision 10 — no experimental flag, and no contract change

**CLAUDE.md rule 9 (experimental flag): no.** The flag is a *presentation* switch gating a TUI render or input-binding seam. This PRD adds no pane, field, command, tab, footer or keybinding — it adds CI jobs and release assets. There is no seam to gate, and the flag could not hide a published `.dmg` in any case. PRD #176 reached the same conclusion for the same reason and stated the alternative explicitly: for a separate binary, maturity is enforced by *packaging*, and Decisions 4, 5 and the docs framing are how this PRD does that.

**CLAUDE.md rule 12 (cross-version contract): no change.** Nothing here touches the daemon, the TUI↔daemon protocol, orchestration or hooks. The wire shape does not move, no field changes meaning, and `PROTOCOL_VERSION` stays put. The only non-CI source change is `desktop/scripts/prepare-sidecar.sh`, a build-time staging script. Under the `0.x` policy in `docs/develop/versioning.md` this is a **feature → patch** bump, and its changelog fragment is `changelog.d/740.feature.md`. The cross-version manual test does not apply because there is no contract to test across.

### Decision 11 — the alpha ships unadvertised

No user-facing documentation page. The assets are on the release, the release notes carry a fixed unsigned-alpha section naming them and giving the exact `xattr -dr com.apple.quarantine` invocation, and that is the whole of the public surface. This holds PRD #176 M5.1's "unadvertised" framing intact, and it is the honest position while the artifact is unsigned, Windows-less, and about to change its icon: an install page is an invitation, and this build is not yet inviting.

The obligation does not disappear, it is scheduled. [#765](https://github.com/vfarcic/dot-agent-deck/issues/765) covers writing the published install page under `docs/` when the GUI leaves alpha, and names the three things that would each make it due: #757 (signed, so the install story stops being an apology), #746 (logo settled, so screenshots do not immediately go stale), and #164 (Windows exists, so the page is not two-thirds of a platform matrix).

`docs/develop/desktop-gui.md` stays where it is and gets a short note that packaging now exists — it is the maintainer guide, excluded from the Docusaurus build, and a different document for a different audience.

### Build details worth writing down before someone rediscovers them

- **The sidecar is built inside the bundling job, not reused from `build`.** `prepare-sidecar.sh` builds the daemon itself (`cargo build --locked --bin dot-agent-deck --target <triple>`), so reusing `build`'s artifact would need a new script mode *and* would make `desktop-bundle` depend on `build`, serializing what Decision 4 deliberately keeps parallel. The repository is public, so GitHub-hosted runner minutes — macOS included — are not billed; duplicated compute costs wall-clock on a parallel branch and nothing else. The job exports `DAD_VERSION` so the sidecar is stamped correctly by the same `build.rs` seam.
- **`pnpm install --frozen-lockfile` must run before `tauri build`.** The merged `beforeBuildCommand` is `npm run build && npm run sidecar:prepare` — `npm run`, in a tree installed with pnpm — which only works because `node_modules/.bin` is already populated. Switching that config to `pnpm run` is a defensible cleanup but should be a deliberate change, not an incidental one.
- **Linux needs bundler packages beyond the compile set.** `ci.yml:172-175` installs `libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev`, which is enough to *compile* the crate. The `deb`/`AppImage` bundlers additionally want some of `patchelf`, `fakeroot`, `file` and FUSE. Exactly which is unverified; M1 settles it by running a bundle.
- **AppImage bundling downloads `linuxdeploy` at build time**, adding a network dependency inside the release path. Noted in Risks.
- **Toolchain pinning.** The bundling job pins Rust 1.97.1 to match the rest of `release.yml`, adds the target triple explicitly, and uses `pnpm/action-setup@v4` + `actions/setup-node@v4` matching `ci.yml`'s `desktop-web` job (Node 20, pnpm 10) so the tested floor is the built floor.

### Test plan

This is release/packaging work, so CLAUDE.md rule 4's TUI-harness requirement does not engage: there is no pane, status, prompt, focus, layout, mode or hook delivery to snapshot, and no L1 or L2 test can observe a GitHub Release. Claiming a TUI test here would be ceremony. What *is* testable is tested, and the rest is verified by a real run.

| Item | Catalog ID | Tier | Scenario | Action |
|---|---|---|---|---|
| `prepare-sidecar.sh` names the staged sidecar `…-<triple>.exe` on Windows triples and `…-<triple>` elsewhere | n/a — `xtask/linkage-check` module, no `#[spec]` catalog entry | fast-tier unit (script-driving) | Runs the real `prepare-sidecar.sh` against a stubbed `cargo` on `PATH` inside a `tempfile::tempdir()`, for a windows triple and a unix triple, asserting the staged filename and that a missing triple is a hard error. | create |
| `release.yml` desktop jobs are wired as designed | n/a | static check in the same module | Asserts `finalize` does not list `desktop-bundle` in `needs:` — the single property that keeps a bundler failure from touching the CLI release. | create |
| The bundle actually builds, on both platforms, from a clean checkout | n/a | manual (M1) | Local `tauri build` on Linux and macOS; settles the `.icns` and bundler-dependency unknowns. | create |
| The published assets install and run | n/a | manual (M6) | A real `workflow_dispatch` run produces downloadable assets; the `.dmg` and the `.deb`/`.AppImage` are installed on a real machine, launched, and connect to a daemon. | create |
| Existing e2e / L1 / L2 suites | — | — | Unchanged. No production source behaviour moves. | skip |

The script-driving fast-tier tests follow the precedent CLAUDE.md rule 5 already documents for `verify_pr_stream.rs` and `pin_lockstep.rs`: they read repo files and shell out to `bash`, which is the rule rather than a shortcut around one, because the thing under test *is* a shell script. Unix-only by `#[cfg]`, no git, no network, no sleep. Per CLAUDE.md rule 3 the module is named for what it contains (`sidecar_staging.rs`), not for this PRD.

The honest limit: the release workflow itself cannot be tested without cutting a tag. M6's `workflow_dispatch` run against a real version is the proof, and it is a milestone rather than an afterthought precisely because nothing before it exercises the whole path.

## Success Criteria

- A tagged release carries a macOS arm64 `.dmg` and Linux x86_64 `.deb` and `.AppImage`, named so their alpha status is visible before download, alongside the five existing CLI assets.
- Every bundle reports the release version — in the macOS About window, in `dpkg -I`, and in the filename — never `0.1.0`.
- Each bundle carries a matching daemon sidecar stamped with the same version, so an installed GUI can start a daemon without a local Rust toolchain.
- A user who downloads the `.dmg`, clears quarantine per the release notes, and opens the app reaches a working control deck against a daemon it started itself.
- A bundler failure leaves the CLI release complete, on schedule, and correct — and turns the workflow run red so the failure is seen.
- An ordinary tag's time-to-published-CLI-assets is unchanged from today's ~6 minutes.
- Nothing about the CLI's release, its Homebrew formula, or its Scoop manifest changes.

## Milestones

- [x] **M1 — Bundle locally.** *(Linux DONE and verified — a real `.deb` was produced, inspected and collected. macOS NOT done: no Apple hardware available here, so the `.dmg` path and the `.icns` question remain unverified. See Work Log 2026-08-30 (bundle evidence).)* `tauri build` producing a `.dmg` on macOS arm64 and `.deb` + `.AppImage` on Linux x86_64, from a clean checkout. Settles the two empirical unknowns: whether a `.icns` must be generated, and which bundler packages Linux needs beyond the compile set. Record both answers in the Work Log.
- [x] **M2 — Version injection.** The resolved version reaches the bundle; a build asserts it rather than assuming it. Confirms whether stacked `--config` works or the `jq` fallback is needed.
- [x] **M3 — `prepare-sidecar.sh` `.exe` groundwork**, covering both the source path and the destination name, plus the fast-tier script tests. No Windows artifact.
- [x] **M4 — `release.yml` wiring.** `desktop-bundle` and `desktop-publish` jobs per Decision 4, plus the `skip_desktop` dispatch input, plus the static check that `finalize` stays independent.
- [x] **M5 — Asset naming, checksums and release notes.** The rename step, `checksums-desktop-alpha.txt`, and the fixed unsigned-alpha section with the quarantine instructions.
- [ ] **M6 — Verified by a real run.** *(NOT DONE — requires a real `workflow_dispatch` execution and installs on real machines; nothing in this PR proves a bundle builds.)* A `workflow_dispatch` execution produces downloadable assets; the macOS and Linux bundles are installed and launched on real machines and connect to a daemon. This is the gate that the whole path works.
- [x] **M7 — Docs and changelog.** A note in `docs/develop/desktop-gui.md` that packaging now exists, PRD #176's M5.1 amended per Decision 4, and `changelog.d/740.feature.md`. **No published `docs/` page** and no `site/sidebars.js` entry, per Decision 11 — that is [#765](https://github.com/vfarcic/dot-agent-deck/issues/765).

## Risks

- **The whole path is only provable by running it.** No test in this repository can observe a GitHub Release. M6 is therefore load-bearing and must not be collapsed into "CI was green" — a green run with an empty `dist-desktop/` is exactly the failure that `bundle.active: false` produces silently.
- **`.dmg` creation on a headless runner.** Tauri's DMG bundler drives `hdiutil` and, in some versions, AppleScript for window layout, which is the classic thing that behaves differently without a GUI session. If it proves unreliable, the fallback is a zipped `.app` — worse UX, same reachability. Decide on evidence from M1, not in advance.
- ~~**AppImage's build-time download.**~~ **Materialised, and acted on.** The risk was real and arrived on the first attempt; Decision 7 now drops AppImage rather than carrying it. Decision 4's topology did contain the blast radius exactly as designed — the failure could not have touched a CLI release — but the fix is to not run the code, not to rely on the containment. Worth remembering as a data point that the containment was the thing that made a calm decision possible.
- **An unsigned alpha is a support surface.** People will hit Gatekeeper and file issues. The docs page and the release-notes section are the mitigation, and they need to be blunt rather than apologetic: this build is unsigned, here is the exact command, #757 is where that gets fixed.
- **Sidecar/daemon version confusion.** The bundle carries its own daemon and the handshake refuses a build-stamp mismatch. A user with an existing `dot-agent-deck` daemon running from Homebrew at a different version will meet that refusal as their first experience of the app. Worth checking during M6 what that actually looks like, and whether the message names the fix.
- **Scope creep toward #757.** Every signing question that arises during this work belongs in #757. The moment this PRD starts discussing certificates, it has stopped being the PRD it is.
- **Runner-minute growth is free today because the repo is public.** If the repository ever goes private, two extra matrix legs — one of them macOS at a 10× multiplier — become a real bill. Not a reason to change the design, but a reason to write it down.

## Open Questions

None outstanding. All five questions raised at plan time were answered before implementation began; the answers are folded into Decisions 1, 2, 4, 9 and 11, and the reasoning is recorded in the Work Log entry below.

## Work Log

### 2026-08-30 — Created

Written from issue #740's placeholder body after verifying its "Where it stands today" claims against `main` at `83d9bf3`. Most held; three did not, and they changed the plan:

- **"The Linux runner already installs Tauri's system deps for the `build` job"** — true of `ci.yml`'s `build` job (lines 172-175), not of `release.yml`, which installs nothing of the sort. The release workflow starts from zero here.
- **"`desktop/src-tauri` is not even compiled"** in the release build — correct, but not because it is a separate crate. It *is* a workspace member (`Cargo.toml:1-8`); it is excluded because `cargo build` with no `-p`/`--workspace` selects the root package alone, the same mechanism CLAUDE.md rules 2 and 5 document for the `xtask/*` members.
- **"Mark the assets prerelease"** — not possible as stated. GitHub has no per-asset prerelease flag, and this repo's only prerelease mechanism flips the entire release and reroutes Homebrew and Scoop to a beta channel. Decision 5 replaces it.

And three things the issue did not anticipate, one of them decisive:

- **No Windows daemon binary is published at all.** The release matrix has four targets, none Windows, so a Windows GUI has no sidecar to carry. This, more than #754, is why Decision 1 defers Windows.
- **The release asset glob and checksum step both hardcode the CLI's name**, and `productName` contains a space — so bundler output would be silently skipped by `finalize`'s `dist/dot-agent-deck-*` glob and would break `task checksums`' `shasum` pipeline. Decision 6 handles it.
- **`bundle.active` is `false` in the base config**, flipped only by the overlay, so a bundling invocation that loses `--config` produces nothing and still exits 0 — a failure mode that looks like success.

Also noted for whoever picks this up: [#487](https://github.com/vfarcic/dot-agent-deck/issues/487) covers retiring the `0.1.0` placeholders repo-wide and overlaps Decision 3; `Taskfile.yml:226`'s Scoop manifest already points at an unpublished Windows binary with an empty hash, which belongs to #164; and #754 has no PR open against it.

### 2026-08-30 — Plan confirmed

All five open questions were answered by the maintainer, and the PRD now carries decisions rather than questions.

**x86_64 macOS: no.** The question came back as "does anyone use x86 Macs these days?", which turned out to be answerable from this project's own release telemetry instead of an opinion about the market. Across `v0.36.0`–`v0.38.0`, `darwin-amd64` was downloaded **once**, against 77 for `darwin-arm64` and 78 for `linux-amd64`. The strongest argument for building it — that the CLI already ships it, so the audience is assumed — inverts once the numbers are in: the CLI has shipped it the whole time and nobody has taken it. Folded into Decision 2.

**Windows: deferred, confirmed.** The chain is what settled it, not #754 on its own. A Tauri bundle carries the daemon as a sidecar; there is no published Windows daemon binary; adding one is issue #164's substance and would mean shipping the first Windows daemon build without #164's Windows-VM e2e validation, under a packaging issue number. #754 is the second reason and the one that would hurt users: even with a sidecar, live workflow launch is refused on Windows by both `dto.rs:491` and `platform.ts`, so the headline action would be off — and fixing it properly puts the daemon's OS in the handshake, which is a rule 12 contract change that would turn this patch into a minor. The `.exe` groundwork lands anyway so the next person does not re-derive it.

**"Every release": confirmed, by topology rather than by a flag.** The three statements in play — the issue title's "every release", M5.1's "excluded from the default release", and the issue's own "gate it so it cannot slow or break an ordinary tag" — only conflict if "excluded" is read as the goal rather than as the mechanism someone reached for before anyone had looked at the job graph. The two properties M5.1 was protecting are that the GUI is labelled preview and that it cannot compromise the CLI release. Decision 4's wiring gives the second one more strongly than exclusion does: because `finalize` does not list `desktop-bundle` in `needs:`, the bundler cannot slow the release *and* cannot fail it. A `workflow_dispatch` gate was rejected for the opposite reason — something a human must remember to trigger gets triggered for two releases and then never, which is how an alpha artifact ends up six versions stale and worse than none.

**Documentation: unadvertised, with the obligation scheduled.** New Decision 11. [#765](https://github.com/vfarcic/dot-agent-deck/issues/765) filed to write the published install page when the GUI leaves alpha.

**Icons: ship on what is in the tree**, and re-ship carrying the new icon as part of #746 rather than leaving the alpha looking like a draft. Folded into Decision 9.

Also surfaced during planning and deliberately left out of this PRD's scope: **CLAUDE.md rule 2's mandated `cargo clippy --workspace --all-targets --features e2e` cannot run on a Linux machine that lacks the GTK/WebKit development packages.** `--workspace` has included `dot-agent-deck-desktop` since `daf94f0`, those packages are in neither `devbox.json` nor the host used for this planning session, and the gate dies at `glib-2.0.pc not found`. `ci.yml` installs them for its own `build` job, so CI is unaffected and this is invisible there. Every Linux contributor without those packages has been unable to run the mandated pre-commit gate since `daf94f0` landed. Adjacent to this PRD but not part of it.

### 2026-08-30 — Windows reasoning corrected

The maintainer challenged Decision 1's original reasoning, and the challenge landed. The first draft argued Windows was blocked because "a Tauri bundle carries the daemon as a sidecar, and no Windows daemon binary is published, so there is nothing to put in the bundle." His counter: the desktop app is a *client*, meant to connect to a daemon anywhere, so a Windows user could attach to a Linux daemon and would need no Windows daemon at all — and there is a PRD for exactly that.

He is right about the intent, and the original reason was too absolute. A sidecar-less Windows bundle is plausible, and the sidecar is therefore not an inherent blocker. What the correction surfaced is a **stronger** blocker sitting underneath it: reachability. The IPC transport is `cfg`-dispatched — a Unix domain socket on Unix, a named pipe on Windows — so a Windows client cannot attach to the forwarded Unix socket that #741's measured 2026-08-29 remote session used, and OpenSSH on Windows will not bridge the two. The desktop crate has no remote code of its own to work around that, and #741 lists app-managed tunnelling as work still outstanding.

Two things changed as a result. The stated reason is now reachability rather than the sidecar, and the **named dependency moved from #164 to #741** — #164 would only unlock the local Windows story, while the remote shape actually wanted needs #741 and does not need #164 at all. The conclusion is unchanged: Windows is deferred, its `.exe` groundwork still lands.

Surfaced and worth acting on separately: **#741's placeholder body does not mention the Windows transport question**, and its measured evidence is Unix-to-Unix. Whoever writes #741 in full should decide there whether app-managed tunnelling covers Windows named pipes, because that decision — not this PRD — is what gates a Windows GUI.

### 2026-08-30 — Implementation

Built solo rather than through the worker roles: mid-run, `dot-agent-deck delegate` began failing with `the daemon holds no orchestration role for pane …`, so the tester/coder/reviewer/auditor chain the orchestration template calls for was unavailable. Diagnosed separately — the session had moved into a pane orphaned by a deliberate daemon restart on 2026-08-28, and orchestration role state is in-memory only — and that is not a defect in anything this PRD touches.

**What landed.** M3 first, TDD, because it is the only part of this work with a real test seam: `xtask/linkage-check/src/sidecar_staging.rs` drives the actual `prepare-sidecar.sh` under a stubbed `cargo` in a tempdir. The first run failed for a *fixture* reason — PATH narrowed so far that `sh` itself was unreachable — which is exactly the false red the `reproduce-first` skill warns about; after fixing the harness, precisely the three Windows cases failed with the right message while all four controls passed. Both halves of the fix were then confirmed load-bearing by reverting each alone: dropping the source suffix fails on the missing `dot-agent-deck.exe`, dropping the destination suffix fails on the `externalBin` name, and the two produce different messages rather than one indistinguishable red.

M2, M4 and M5 all live in `release.yml`'s two new jobs. The job-graph properties that make them safe are asserted by `xtask/linkage-check/src/release_workflow_wiring.rs`, and each assertion was mutation-tested — coupling `finalize` to `desktop-bundle`, deleting the artifact `pattern`, and pointing `desktop-bundle` at `build` each turn exactly one test red. That matters more here than usual: nothing can execute this workflow outside a tag, so a bad edit is otherwise observable only after a release has already gone out wrong.

Two decisions changed shape while implementing:

- **Version injection uses one merged config, not two stacked `--config` flags.** Decision 3 preferred stacking and named a `jq` rewrite as fallback. Since stacked `--config` could not be verified without running the Tauri CLI, the fallback became the primary: the job merges the version into the bundle overlay with `jq` and passes a single `--config`. Same outcome, no dependency on unverified CLI behaviour. The build then *fails* if no output filename carries the release version, so a bundle labelled `0.1.0` cannot be published.
- **`finalize`'s artifact download needed a `pattern:`, which Decision 6 did not anticipate.** `merge-multiple: true` flattens every artifact of the run into one directory, so desktop bundles would have raced into `dist/` depending on which job finished first — then been swept into the release by the `dist/dot-agent-deck-*` glob and fed to a `shasum` pipeline that cannot survive the space in `Agent Deck_<version>_amd64.deb`. Fixed by constraining `finalize` to `pattern: dot-agent-deck-*` and naming the desktop *artifacts* `desktop-bundle-*`, while the *files inside* keep the `dot-agent-deck-desktop-alpha-*` names users see.

**What is NOT done, and cannot be from here.** M1 and M6 are both open. This machine has no GTK/WebKit development packages — `glib-2.0.pc` is absent from the host and from `devbox.json` — so no `tauri build` ran, and the two empirical unknowns M1 exists to settle are still unknown: whether `tauri-bundler` synthesizes a missing `.icns` from the PNG, and exactly which Linux bundler packages are required (the job installs `patchelf`, `fakeroot`, `file` and `desktop-file-utils` on reasoning, not on measurement). **No bundle has been produced by this work on any platform.** The first `workflow_dispatch` run is where this is genuinely tested, and it should be treated as the milestone it is rather than as a formality.

Same gap affects the pre-commit gate: CLAUDE.md rule 2's `cargo clippy --workspace --all-targets --features e2e` cannot run here, because `--workspace` includes `dot-agent-deck-desktop`. It was run with `--exclude dot-agent-deck-desktop`, which covers every crate this branch actually changes — no Rust in the desktop crate was touched — but that is a narrower gate than the rule specifies, and CI runs the full one.

### 2026-08-30 — Bundle evidence (M1, Linux)

Installed the GTK/WebKit development packages and ran a real `tauri build` for `x86_64-unknown-linux-gnu`. This moved four things from argued to measured, and changed one decision.

**Version injection works, end to end.** The `jq`-merged config carried `"version": "0.38.1"` alongside the overlay's `bundle.active` and `externalBin`, and it reached the artifact: the file is `Agent Deck_0.38.1_amd64.deb` and `dpkg-deb -f` reports `Package: agent-deck`, `Version: 0.38.1`. The `0.1.0` literal never appears. This is the specific bug Decision 3 exists to prevent, now shown not to happen rather than reasoned about.

**The sidecar is genuinely in the bundle.** `dpkg-deb -c` lists both `usr/bin/dot-agent-deck-desktop` (14.5 MB) and `usr/bin/dot-agent-deck` (19.8 MB) — so an installed GUI carries its own daemon, which is the success criterion that most needed proving.

**The collect-and-rename step survives the space.** Running the workflow step's exact shell against the real output turned `Agent Deck_0.38.1_amd64.deb` into `dot-agent-deck-desktop-alpha-linux-amd64.deb` and produced a valid `checksums-desktop-alpha.txt`. Decision 6 was right that a rename is unavoidable, and the `-print0` handling is doing real work rather than being defensive.

**AppImage failed, and Decision 7 changed as a result** — see that decision for the reasoning. Briefly: the failure prompted looking at *how* the bundler works, which is where the actual objection is (five build-time downloads, two from a mutable `master` ref, executed inside a release pipeline). The `.deb` from the same invocation succeeded, so this is a considered removal rather than a capitulation to a flake.

**One thing only running it could have caught:** the bundle generates `desktop/src-tauri/tauri.release.conf.json` in the working tree. Left untracked and unignored, every local bundle would dirty `git status` and eventually someone commits a version number into a file whose whole point is that it never carries one. Now in `.gitignore` alongside `dist-desktop/`.

**Environment note for anyone reproducing this in a `devbox shell`.** The shell runs under nix's glibc, whose `ld.so.cache` does not exist — `ldconfig -p` returns zero entries — so system libraries are invisible to the loader even once installed. `cargo test-fast` and rule 2's clippy therefore fail with `libgdk-3.so.0: cannot open shared object file` until `LD_LIBRARY_PATH=/usr/lib/x86_64-linux-gnu` is exported. But that same variable then breaks nix's `node` with `undefined symbol: uv_tcp_keepalive_ex`, because it picks up the system libuv — so the Rust gates and the frontend build cannot both run in one shell that way. Bundle *without* it; the Rust link resolves through pkg-config's `-L` flags. The durable fix is to add the GTK/WebKit packages to `devbox.json` as nix packages so nix's own loader can find them; telling contributors to `apt install` does not work, which is the non-obvious part. Out of scope here, and worth its own issue.

**Still unproven:** everything macOS. No Apple hardware was available, so the `.dmg` path, whether `tauri-bundler` synthesizes a missing `.icns` from the 512×512 PNG, and the drag-to-Applications shape are all untested. M6's real `workflow_dispatch` run remains the gate for that half.

### 2026-08-30 — Review round (Greptile on PR #768)

Two findings, both valid. All twelve checks were green, which is exactly the case CLAUDE.md rule 8 warns about: the check-run passing is not the review, and the P1 below was a real defect sitting under a green tick.

**P1 — a failed platform leg discarded the other platform's bundle. Fixed.** `fail-fast: false` was doing less than it looked like. It stops a failing leg from *cancelling* its sibling, so both legs run and both upload — but `needs:` on a matrix job resolves to the **aggregate** result, so one failed leg still skipped `desktop-publish` entirely and threw the surviving bundle away after building and uploading it. That is precisely the outcome the flag was added to prevent, and the wiring tests missed it because the `needs:` edge they asserted was correct; it was the edge's *semantics* that were wrong.

`desktop-publish` now gates on `!cancelled() && needs.finalize.result == 'success'` — on the release object existing, rather than on every leg succeeding — with a guard step that fails loudly when no leg produced anything and emits a per-platform `::warning::` when it publishes a partial set. A release carrying one platform where it should carry two must not be discovered from a user's bug report. Two new assertions cover both halves, and the aggregate-gate mutation is caught.

Fixing it also tripped one of the existing tests, which is worth recording rather than quietly resolving: `desktop_jobs_do_not_swallow_their_own_failures` forbade `continue-on-error` anywhere in the desktop jobs, and the fix needs it on one *step* (so a partial matrix reaches the guard instead of dying on `download-artifact`'s less useful error). The instinct was right and the form was too blunt, so the assertion was narrowed to **job-level** `continue-on-error`, which is the one that actually suppresses a result. Narrowed, not deleted.

**P2 — mutable action tags in an artifact-producing path. Acknowledged, not fixed here, and the reason is consistency rather than disagreement.** The finding is legitimate, and it is close kin to the argument used to drop AppImage in Decision 7. Two things separate them. First, degree: AppImage fetches from `master`, a branch that moves continuously and can carry anything; `@v4` is a release tag a maintainer moves deliberately and only within a major. Related, not equivalent. Second, and decisive: `release.yml` is *uniformly* tag-pinned today, including every step of the `build` job that produces the CLI binaries already shipped to users. Pinning only the new desktop job would leave one file with two conventions while the far more widely installed artifacts stayed unpinned — worse than either consistent state. The coherent change is repo-wide (the 113 SHA pins that do exist are all in auto-generated agentic lock files), and it belongs in its own PR where the Renovate interaction can be thought through. Recorded here so it is not lost.
