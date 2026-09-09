# PRD #757: Make the published desktop artifacts trusted

**Status**: In progress — **decided: free options only** (maintainer, 2026-09-04). No Apple Developer Program membership, no Windows certificate, no signing secret in this repository. That decision has a hard consequence worth stating in the first line rather than burying: **the macOS warning stays**, because there is no free way to remove it (Decision 2). What the free ceiling *does* buy is landed: M2's release-note correction, and **build provenance on every published asset, on all three platforms, with no credential of any kind** (Decision 12) — which is also the only trust mechanism that reaches the Linux `.deb`, where a package signature is inert. M1's measurement is the one open piece of free work; Decisions 4-6 are kept as the record of what a later reversal would cost rather than as work.
**Priority**: Medium
**Created**: 2026-09-04
**Issue**: [#757](https://github.com/vfarcic/dot-agent-deck/issues/757)

## Problem Statement

[PRD #740](740-publish-desktop-gui-artifacts.md) published the desktop GUI, and **v0.39.3 on 2026-09-04 is the first release where that actually happened.** Every release cut between #740 landing and that one — v0.39.0, v0.39.1 and v0.39.2 — carries the five CLI assets and no desktop bundle. `desktop-publish` was dying on three `gh` calls that, in a checkout-less job, had no `--repo` and no git context to infer a repository from, fixed by [#852](https://github.com/vfarcic/dot-agent-deck/issues/852)/[#853](https://github.com/vfarcic/dot-agent-deck/issues/853); the bundles were built, uploaded as workflow artifacts, and never attached to anything. As of v0.39.3 the release carries `dot-agent-deck-desktop-alpha-macos-arm64.dmg`, `dot-agent-deck-desktop-alpha-linux-amd64.deb` and `checksums-desktop-alpha.txt`.

So the artifacts are real now, and with them the thing this PRD exists to remove. The release body carries a fixed section, also published for the first time in v0.39.3:

> The `dot-agent-deck-desktop-alpha-*` assets are an early preview of the desktop GUI. They are **unsigned**, so your OS will warn you… On macOS, clear the quarantine flag after moving the app to /Applications: `xattr -dr com.apple.quarantine "/Applications/Agent Deck.app"`

That instruction is the current state of the art, and it is worse than it looks in three separate ways. It is not a warning a user clicks past — issue #757's own description of the unsigned case is that a downloaded build "reads as *damaged and can't be opened*", which looks like a broken download rather than an unsigned app, and the only button offered is *Move to Trash*. The workaround is a terminal command that recursively strips quarantine from a directory in `/Applications`, which is precisely the habit a user should not be taught: the next thing that asks them to run it will not be us. And it is the **only** route the note gives, in a world where macOS Sequoia removed the Control-click bypass — so a user who does not want to run that command is given nothing, even though System Settings → Privacy & Security → *Open Anyway* exists. **The instruction is the defect, not the mitigation.**

**The clock has also just moved, in the wrong direction.** Homebrew removed `--no-quarantine` for casks and requires every cask in its tap to be codesigned and notarized, with unsigned casks removed by **1 September 2026** — three days before this PRD was written. That closes the one genuinely free route to a warning-free macOS install that this project could have taken: a Homebrew cask no longer works around notarization, it now *requires* it. #740 put "Homebrew cask entries for the GUI" out of scope as a support commitment an unsigned alpha had not earned; that decision is now also a technical fact.

The other two platforms are in genuinely different states, and treating them as one problem is what made this look bigger than it is. There is **no Windows artifact at all** to sign — v0.39.3's eight assets contain no Windows executable, and neither of `release.yml`'s two matrices (four CLI targets at `release.yml:253-268`, two desktop targets at `:539-543`) names a `*-pc-windows-*` triple. And on Linux the `.deb` has no trust gate that an artifact signature would satisfy, because the default install path checks no such signature.

## Solution Overview

Split the work by what it costs, because the cost is the decision and everything else follows from it.

**Free, shipped, and the piece that covers every platform:** build provenance for every published asset — the `.dmg`, the `.deb` and the four CLI binaries — generated from the job's OIDC token with no private key anywhere in this repository, and verified by a user with one `gh attestation verify` command (Decision 12). It does not remove any OS warning, and it is the only trust mechanism that reaches Linux at all, where `dpkg -i` checks no package signature.

**Free, and shipped:** stop the release note lying. It gave macOS users one route that macOS Sequoia had removed the alternative to, and told `.deb` users their OS would warn them, which is false. Landed with a static assertion holding both properties.

**Free, and the one piece of work left:** measure, on a Mac, whether ad-hoc signing changes the macOS failure mode from *damaged, Move to Trash* to *unverified, Open Anyway* — and whether the daemon sidecar survives `codesign --verify --deep --strict`, which is a no-cost proxy for an open upstream Tauri bug. Neither needs a certificate, an account, or a secret (Decision 3).

**$99/yr, and declined:** Developer ID signing plus Apple notarization. This is the only thing that actually removes the macOS warning, and Decision 2 records that there is no free substitute at any effort. The mechanism is designed and written down (Decisions 4 and 5) so that a later reversal is a build rather than a re-think, and so the cost of the decision is legible: **every macOS user meets a security dialog on first launch, forever, until this is revisited.**

**Free, and not yet reachable:** Windows. The free path there is the *best* path — a Microsoft Store MSIX submission is the only option in Microsoft's own comparison table that produces no SmartScreen warning at all, better than a $400/yr EV certificate — and it is moot because **this project publishes no Windows executable of any kind** (Decision 6).

The topology PRD #740 chose is untouched throughout: `desktop-bundle` still hangs off `prepare`, `finalize` still lists no desktop job in `needs:`, and a signing failure still turns the run red while leaving the CLI release complete. Signing adds a job to an existing chain; it does not get between the CLI matrix and `finalize`.

## Scope

### In Scope

- **Done:** SLSA build provenance for every release asset via a new `attest` job — free, no secret, all three platforms (Decision 12), with the verification command in the release note.
- **Done:** correcting the release-body note — the macOS half gains the System Settings route it was missing, and the Linux half stops claiming an OS warning that does not exist (Decision 3), with a static assertion holding both.
- **Open:** measuring, on a real Mac, whether ad-hoc signing changes the macOS failure mode, and whether the daemon sidecar survives `codesign --verify --deep --strict` — the second being a no-cost proxy for an open upstream Tauri bug (Decision 3, M1).
- **Open, conditional on that measurement:** ad-hoc-signing the bundle in `desktop-bundle`, if and only if M1 shows it improves the first-launch dialog.
- Static assertions holding the above: the note's two properties, and the `attest` job's topology and its credential-free permission set (Decision 11).
- A written, costed statement of what the paid path *would* be (Decisions 4 and 5) and of the Windows options (Decision 6), so that a later reversal is a build rather than a re-think and the free-beats-paid finding is not lost.

### Out of Scope

- **Acquiring, generating or configuring any certificate, and adding any secret to this repository.** That is the maintainer's decision and this PRD does not pre-empt it.
- **Windows Authenticode, and the Microsoft Store submission.** There is no Windows artifact to sign (Decision 6). The facts are recorded; the engineering is not scoped.
- **Linux `.deb` signing, and a signed apt repository.** The default install path verifies no package signature (Decision 7); provenance covers what a user can actually check (Decision 12). A repository is a distribution channel and a separate project.
- **Surfacing provenance to CLI users in the release body.** The assets are attested; the instructions currently live only in the desktop section this PRD owns. Decision 12 records it as the one known gap.
- **A Homebrew cask for the GUI.** #740 put it out of scope; Homebrew's 2026-09-01 policy now makes it *depend* on this PRD's paid path rather than substitute for it. If the $99 is spent, a cask becomes possible and is worth its own issue.
- **Tauri's auto-updater.** Its signature is a Tauri-specific **minisign** keypair over the update payload, unrelated to Developer ID and unrelated to Authenticode — it shares only the word "signing". It also needs an update endpoint, a channel policy and a rollback story. #740 deferred it and this PRD leaves it deferred (Decision 8).
- **Graduating the GUI out of alpha** (Decision 9), and **a published install page** — [#765](https://github.com/vfarcic/dot-agent-deck/issues/765) owns that, and this PRD is one of the three things it names as making it due.
- **A universal (x86_64 + arm64) macOS bundle.** #740 Decision 2 measured Intel Mac demand at one CLI download across seven releases; signing does not change that arithmetic.

## Technical Approach

### Decision 1 — macOS is the whole of the *OS-trust* scope, because it is the only platform with both an artifact and a trust gate

Three platforms, three genuinely different states, and the difference is not a matter of degree:

| Platform | Artifact published today | Does the OS gate it? | Would a signature help? |
| --- | --- | --- | --- |
| macOS arm64 | `…-macos-arm64.dmg`, since v0.39.3 | Yes — quarantined, and per #757 reads as *damaged* | Yes, and it is the documented fix |
| Windows | **none** | n/a | n/a — nothing to sign (Decision 6) |
| Linux x86_64 | `…-linux-amd64.deb`, since v0.39.3 | No | No — the default install path checks none (Decision 7) |

That table scopes the *OS-trust* question, and only that one. It is not the whole of this PRD's engineering: Decision 12 adds build provenance, which is free, needs no credential, and applies to every asset on every platform — including the Linux `.deb`, where the table's "a signature would not help" is true and easy to misread as "nothing would".

As a scoping argument for OS trust it holds. The issue's own estimate — "roughly a week of engineering; a calendar window several times that, driven almost entirely by Windows" — assumed three platforms of work. One of them has no artifact and one has no gate, so the engineering is one platform's worth and the calendar is Apple's enrollment time rather than Microsoft's verification time.

Worth recording because it explains why this problem appeared only now: **the CLI has never needed a signature, and that is a property of how it is delivered rather than of the binary.** `com.apple.quarantine` is set by the *downloader*, so a binary fetched by `brew install` of a formula, or by `curl`, carries no quarantine attribute and faces no Gatekeeper first-launch check. The moment a `.app` started shipping through a browser download, that free ride ended.

### Decision 2 — "without paying" has a different answer on each platform, and the asymmetry is the opposite of what the issue assumed

The issue frames macOS as "moderate, mostly waiting" and Windows as "the hard one". On the cost axis that inverts: **macOS has no free path at any effort, and Windows' free path is better than anything money buys there.**

**macOS: $99/yr is a floor, and there is no way under it.** Four candidate routes, all closed:

- **A free Apple Developer account cannot notarize.** Tauri's own documentation states it, and a free account issues Apple Development certificates rather than a Developer ID Application certificate. Without notarization, a Developer ID signature alone does not clear Gatekeeper for a downloaded app anyway, so this is closed twice over.
- **Apple's fee waiver does not apply, and it says so explicitly.** It exists for nonprofit organizations, accredited educational institutions and government entities, and its eligibility page states outright that the applicant must *not* "be an individual, sole proprietor, or single-person business". There is no reading of it that covers this project.
- **The Mac App Store is not cheaper and not plausible.** It needs the same $99 membership, and it requires the App Sandbox — which a daemon sidecar whose job is spawning and supervising arbitrary user-chosen agent binaries does not fit, because that is precisely what the sandbox exists to stop.
- **The Homebrew cask loophole closed on 1 September 2026.** `--no-quarantine` is removed and unsigned, unnotarized casks are being dropped from the tap. This was the one route that would have delivered a genuinely warning-free install for free, and it is gone as of three days before this was written.

So the honest answer to "is there an alternative to paying for it?" on macOS is **no**. What is available for free is a *better failure mode*, not the absence of failure — Decision 3.

**Windows: free is the best option, and paid is a downgrade.** Microsoft's own current comparison of code-signing options has exactly one row marked "✅ No warnings", and it costs nothing:

| Option | Cost | SmartScreen on first download |
| --- | --- | --- |
| Microsoft Store, MSIX — Store re-signs | **Free** (free developer account) | **No warnings** |
| Microsoft Store, MSI/EXE — publisher signs | cost of a CA certificate | No prompts during Store install |
| Azure Artifact Signing | ~$9.99/mo | Warning; reputation builds |
| OV certificate (DigiCert, Sectigo…) | $150–300/yr | Warning; reputation builds |
| EV certificate | $400+/yr | **Same as OV since 2024** |
| Self-signed / unsigned | Free | Blocked |

Two things fall out of that table and both contradict the issue. First, **EV no longer buys anything for SmartScreen** — Microsoft says paying the premium for that purpose is "no longer justified", which retires the issue's OV-vs-EV question entirely. Second, **the free option is strictly the best one**: an MSIX submitted to the Store is re-signed by Microsoft, so users never see a SmartScreen prompt, which is an outcome no certificate at any price can produce for a direct download.

And if a direct download is wanted alongside the Store, there is a second free route: **[SignPath Foundation](https://signpath.org/) provides free code signing to qualifying open-source projects**, HSM-backed, and Microsoft's own documentation names it. This project's MIT license with no commercial dual-licensing satisfies the license criterion; the other conditions are real and worth reading before assuming eligibility — verifiable builds from the project's own source, MFA across the team, an Authors/Reviewers/Approvers split, manual approval of each release, and a published *code signing policy* page. It signs Windows Authenticode only, so it is not an answer for macOS.

**Linux: free, and better served by provenance than by a signature.** OS-level package signing is inert there (Decision 7), but that is an argument against signing the `.deb`, not an argument that Linux users get nothing. Decision 12 is the free mechanism that actually reaches them — and reaches macOS and Windows too.

**Decided, 2026-09-04: free options only.** The maintainer's answer to the $99 question was to take the free ceiling on every platform. What that costs is worth writing in the decision rather than leaving to be discovered: **on macOS the security dialog stays on first launch, for every user, on every release, until this is revisited.** The free work below makes that dialog more honest and possibly less alarming; nothing free makes it go away. Decisions 4 and 5 stay in this document as the record of what a reversal would build and what it would risk, not as scheduled work.

### Decision 3 — the free macOS work is a better failure mode and an honest note, and it is worth doing whatever the $99 answer is

Two pieces, and the second is already justified while the first needs a measurement.

**Correct the note. No measurement needed, and it stands on its own.** Today's text has one route and one false clause:

- It gives only `xattr -dr com.apple.quarantine`, and macOS Sequoia removed the Control-click bypass, so a user unwilling to run a recursive command against `/Applications` is offered nothing. **System Settings → Privacy & Security → *Open Anyway*** is the no-terminal route and belongs in the note ahead of the command, not instead of it — some users will still want the one-liner.
- It tells `.deb` users "they are **unsigned**, so your OS will warn you", and the second clause is simply false for them: `dpkg -i` warns about nothing (Decision 7). The Linux half should say what is true — unsigned, integrity verifiable against `checksums-desktop-alpha.txt`, no OS gate to defeat.

**Ad-hoc-sign the bundle, if a measurement says it helps.** The hypothesis is specific: macOS reports *"is damaged and can't be opened"* when Gatekeeper cannot verify a quarantined file's signature at all, and reports *"cannot be opened because Apple cannot check it for malicious software"* — with an *Open Anyway* route in System Settings — when there is a verifiable but untrusted signature. macOS supports ad-hoc signing with the pseudo-identity `-`, which produces a real signature structure and no trust, for free. If the hypothesis holds, one line in the bundling job converts *"damaged — Move to Trash"* into *"unverified — Open Anyway"*, which is the difference between an app that looks broken and an app that looks unsigned.

**That hypothesis is explicitly unverified and M1 exists to settle it, not to assume it.** The mechanism is well attested and the exact dialog is not something this PRD has evidence for, and it is the kind of claim that is embarrassing to ship as a fact. It costs one download and two launches on a Mac to know.

The same M1 session buys a second thing for free, and this one is a genuine risk retirement: **[tauri-apps/tauri#11992](https://github.com/tauri-apps/tauri/issues/11992) is open and reports notarization failing with *"The signature of the binary is invalid"* (error 4000) precisely when `externalBin` is configured**, resolving when it is removed. This app has an `externalBin` — `binaries/dot-agent-deck`, the daemon sidecar, which is the feature that makes an installed GUI work without a Rust toolchain. Discovering that after buying a certificate would be the expensive order to discover it in, and an ad-hoc signature is enough to run `codesign --verify --deep --strict --verbose=4` over the nested sidecar and the outer bundle, which is a direct proxy for the failure class. **Free, and it is the reason M1 comes before the money question rather than after it.**

### Decision 4 — if the $99 is ever spent, the signing key never shares a runner with third-party build code

*Not being built — the free-options decision above declined the paid path. Kept because a design argued once is cheaper to reread than to re-derive, and because the reasoning below is the part that would be lost.*

This is the design of the paid half, and everything else is detail. The obvious implementation is the one Tauri documents: put `APPLE_CERTIFICATE` and friends in `desktop-bundle`'s `env:` and let `tauri build` sign during bundling. It is about a day of work and it is what most projects do. **It also puts a Developer ID private key in the environment of the process that compiles the desktop crate's Rust dependency graph and installs `desktop/`'s npm dependency graph**, and this repository has a specific, non-hypothetical path by which unreviewed third-party code reaches that process:

- `renovate.json`'s first two `packageRules` automerge **cargo patch bumps and cargo minor bumps for crates at ≥1.0** on green CI, with no human in the loop. `desktop-bundle` runs `cargo build` over that graph, and a cargo build script is arbitrary code.
- The `GitHub Actions` rule automerges action-ref bumps (`digest`, `pin`, `patch`, `minor`) — including the six SHA-pinned actions inside `desktop-bundle`.
- npm is the *less* exposed lane, which is worth stating precisely rather than lumping in: `desktop/**` npm bumps are **not** automerged (only `site/**` is), and every npm proposal carries `minimumReleaseAge: 3 days`. The npm story here is better than the cargo one, not worse.

None of that is an argument against automerge, which is a good trade for a test suite. It is an argument that the blast radius of automerge must not include a code-signing key.

**So the job splits.** Three jobs, each with one concern:

```
prepare
  ├─ build (existing CLI matrix) ── finalize (creates the Release) ── docs
  └─ desktop-bundle (unchanged: builds, NO credential in scope)
         └─ desktop-sign      (NEW: macOS runner, the ONLY job holding the credential)
                └─ desktop-publish (unchanged except its input)
```

`desktop-sign` runs on a macOS runner and, like `desktop-publish` today, has **no `actions/checkout`** — it downloads the built app as an artifact and calls Apple's tooling. Its entire step list is: `actions/download-artifact`, import the certificate into a throwaway keychain, `codesign`, build the `.dmg`, `codesign` the `.dmg`, `xcrun notarytool submit --wait`, `xcrun stapler staple`, `actions/upload-artifact`. Nothing in that list is code from this repository's dependency graph.

**What it costs, stated honestly.** Tauri's integrated signing is bypassed, so the `.dmg` step is re-implemented by hand (`hdiutil`), and the hardened-runtime entitlements Tauri would otherwise supply have to be authored here — including the JIT entitlement its WebView needs, whose absence produces a bundle that signs cleanly and then crashes on launch. That is the difference between "about a day" and "two to three days", plus a set of macOS-specific traps: nested binary signing order, `--options runtime`, the deprecated `--deep`. M1 walks into all of them before any money is spent.

**`desktop-bundle` must therefore emit the `.app`, not the `.dmg`.** A `.app` is a *directory* (#740 Decision 7 noted this when rejecting `--bundles app` as a release asset), and `actions/upload-artifact` documents that it does not maintain file permissions — "all directories will have `755` and all files will have `644`" — so handing it a bundle directly strips the executable bit off the very binaries `codesign` is about to sign. The artifact is therefore a single `ditto -c -k --sequesterRsrc --keepParent` archive, which is Apple's own tool for this and records modes and extended attributes *inside* the zip where the uploader cannot flatten them. `desktop-sign` restores it with `ditto -x -k`.

**The three alternatives, and why each loses.**

*Sign inside `desktop-bundle` (the simple one).* Cheapest, and the fallback if the split proves unworkable in M1. Loses on the automerge path above.

*Sign on a self-hosted macOS runner.* Counterintuitively the **worst** option for a public repository, and worth writing down because the intuition runs the other way. The key would stay on the maintainer's own machine rather than in a GitHub secret — but a self-hosted runner registered to a public repository can be targeted by `runs-on:` in a **fork's** pull request workflow, which is why GitHub recommends against self-hosted runners on public repositories. The secret would not be in CI; the key would be in the keychain of a machine that runs code from strangers. Strictly worse than a GitHub-hosted ephemeral VM.

*Sign locally, publish by hand.* Genuinely eliminates the CI exposure, and it is the only option that does. It loses to #740 Decision 4's own reasoning, written about this exact failure mode: "something a human must remember to trigger gets triggered for two releases and then never, which is how an alpha artifact ends up six versions stale." The failure here is softer than the one that argument was about — forgetting yields an *unsigned* artifact rather than a *missing* one — but the result is a release history where trust is a coin flip, which is worse for a user than consistent unsignedness. It also requires Apple hardware in the maintainer's hands on every release day.

### Decision 5 — a signing secret would go on a runner, and this is the argument that would have to be made for it

*Not live — no secret is being added. This is the threat model a reversal would have to answer, recorded while the research is fresh.*

**This decision is the maintainer's, not this PRD's.** What follows is the threat model, stated so the decision is informed. Nothing is added to this repository under this PRD.

**What the secrets are.** With the App Store Connect API-key route (preferred, for the reason below), `desktop-sign` needs five values:

| Secret | What it is | What it is worth to an attacker |
| --- | --- | --- |
| `APPLE_CERTIFICATE` | base64 of the Developer ID Application `.p12` — **the private key itself** | The ability to sign anything as this publisher, until revoked |
| `APPLE_CERTIFICATE_PASSWORD` | the `.p12` export password | Nothing alone; everything with the above |
| `APPLE_API_KEY` / `APPLE_API_ISSUER` / `APPLE_API_KEY_PATH` | an App Store Connect API key, its issuer id, and the `.p8` file | The ability to submit software for notarization under this account |

**What a compromise costs, and it is not a rotation.** The credential that matters is the first, and it is categorically unlike an API key: not a token that can be invalidated and reissued behind the scenes. Apple's remedy for an exposed Developer ID key is **revocation**, it is not self-service (it goes to `product-security@apple.com`), and revoking a Developer ID certificate stops software signed with it from installing **and stops already-installed copies from launching**. So a leak is not "rotate and move on" — the remediation is a remote kill switch on every copy of the app anyone has installed, fired by us. That is the sentence to weigh, and it is why this is a bigger decision than adding a test API key.

The one thing that improves the picture is notarization itself, easy to read as pure overhead: Apple's notary service keeps an audit trail of what is submitted under the account, so unauthorised builds can be identified and their **tickets** revoked, which is a narrower instrument than revoking the certificate.

**Why this is not the thing CLAUDE.md rule 5 forbids, and where it genuinely is.** Rule 5's argument is that "running a credentialed job less often does not reduce the risk that a credential leaks from a public repository's CI — it reduces the number of exposures, not the existence of the vulnerability." That argument applies here in full and must not be answered with frequency. Three things distinguish this case; one does not.

1. **Rule 5's boundary is drawn around a *test* credential for the e2e tier**, and it is drawn there because the alternative — running those tests on a developer machine — exists and works. There is no equivalent here: the alternative to signing in CI is signing by hand on release day (Decision 4's third alternative) or not signing at all.
2. **`release.yml` has no `pull_request` trigger.** Its `on:` is `push: tags: ['v*']` and `workflow_dispatch`, so no outside contributor can cause it to run and no fork PR can reach its secrets. This is a scoping property, not a frequency one, and it is the specific reason rule 5's "per-merge instead of per-PR is risk theatre" does not transfer: that argument is about a job an attacker can *trigger*.
3. **This repository already holds a comparably consequential secret on this exact workflow.** `RELEASE_TOKEN` is an **admin PAT** that bypasses the `main-protected` ruleset and pushes directly to `main`. Whatever the right answer is here, "no consequential secret lives in `release.yml`" is not the status quo being protected.
4. **What does *not* distinguish it**, and must not be offered as if it did: a GitHub environment with a required reviewer. It is worth having — environment secrets are readable only by jobs that declare the environment, which is real access scoping on top of Decision 4's job split — but the reviewer prompt is a human clicking *approve* on their own release, which is exactly the ceremony rule 5 names.

**The precedent for how such a decision gets recorded** is the Codex issue-labeler, which does put `OPENAI_API_KEY` on a runner and does reach a real agent, and which carries its own threat model in [`docs/develop/issue-labeling.md`](../docs/develop/issue-labeling.md) rather than an exemption. This PRD follows that shape: if the answer is yes, `docs/develop/desktop-signing.md` carries the argument, the secret inventory, the rotation procedure and the revocation runbook, and rule 5's e2e statement is left untouched because this is not the e2e tier.

**Prefer the App Store Connect API key over the Apple ID route.** `APPLE_ID` + `APPLE_PASSWORD` + `APPLE_TEAM_ID` works and is one secret fewer, but `APPLE_PASSWORD` is an app-specific password on the maintainer's Apple ID, and its blast radius is an Apple ID rather than a notarization scope. An App Store Connect API key is per-key revocable from the portal, carries a role, and is not the account password. Same mechanism cost, smaller radius.

### Decision 6 — Windows: free beats paid, and nothing is urgent because no artifact exists

There is nothing to sign. That is the whole decision for now, and it is not a judgment call: v0.39.3 publishes no Windows executable, `release.yml` names no Windows target in either matrix, and PRD #740 Decision 1 deferred the Windows *bundle* behind [#741](https://github.com/vfarcic/dot-agent-deck/issues/741) on reachability grounds — a Windows GUI cannot attach to a remote Linux daemon because the IPC transport is a named pipe on Windows and a Unix socket everywhere else. None of that has moved. Scoping Windows signing now would be designing a job for an artifact that does not exist.

**This is the part most likely to be misread, so it is stated flatly: there is no `.exe` on any release, and signing is not what is standing between the project and one.** Not a desktop `.exe`, and not a CLI one either — v0.39.3's eight assets are two checksum files, four CLI binaries for Linux and macOS, and two desktop bundles. Producing a Windows *daemon/CLI* binary is [#164](https://github.com/vfarcic/dot-agent-deck/issues/164); producing a Windows *GUI* bundle is gated on #741's reachability problem, because a Windows client speaks named pipes and cannot attach to the Unix socket a Linux daemon listens on. Neither is blocked on trust, neither is in this PRD's scope, and no amount of certificate procurement moves either one.

The repository is nonetheless **already advertising an executable it does not build**, which is worth fixing whoever gets to it first. `Taskfile.yml:227` derives the Scoop manifest's hash by grepping `checksums.txt` for `dot-agent-deck-windows-amd64.exe`, a filename nothing produces, so the published manifest at `vfarcic/scoop-bucket` carries — verified on 2026-09-04, for v0.39.3 — `"hash": ""` and a download URL that 404s. PRD #740 spotted this and assigned it to #164; it is repeated here because "we publish a Windows exe" is exactly the belief that manifest creates.

**What to do when one does exist, in order.** Decision 2 has the comparison table; this is the ranking that falls out of it.

1. **Microsoft Store, MSIX. Free, and the only option with no SmartScreen warning at all.** The obstacle is packaging, not money or identity: **Tauri has no native MSIX bundle target** ([tauri-apps/tauri#4818](https://github.com/tauri-apps/tauri/issues/4818), open for years), and its own Microsoft Store page documents the *Win32 MSI/EXE* submission route instead — which per Microsoft's table is the one where **the publisher must sign**, so it is not the free route. Reaching the free route means an MSIX built by something outside Tauri: Microsoft's own `winapp` CLI publishes a Tauri guide, and there is a community packager. That is real work with real unknowns, and it is the work worth doing rather than buying a certificate.
2. **SignPath Foundation. Free OV signing for open-source projects**, HSM-backed, named by Microsoft's own documentation. It does not remove the warning — OV means reputation accrues — but it puts a verified publisher name in it and starts accumulating that reputation for nothing. Its conditions are substantive: verifiable builds from the project's own source, MFA across the team, an Authors/Reviewers/Approvers split, manual approval per release, and a published *code signing policy* page. The MIT license with no commercial dual-licensing satisfies the license criterion.
3. **Azure Artifact Signing, ~$9.99/mo.** Only worth it if the two above are unavailable. Its private key is never released to the customer, which is a materially better property than a `.p12`: CI holds `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET` / `AZURE_TENANT_ID`, a service-principal credential rotated in one portal action. It needs a **paid** Azure subscription (free, trial, sponsored, Visual Studio and student subscriptions are refused), identity validation takes **1 to 20 business days**, and eligibility is geographic: organizations across the US, Canada, EU and UK; **individual developers in the United States or Canada only**. It does not issue EV certificates and states no plan to — which, given that EV no longer helps SmartScreen, is no longer a limitation.
4. **A CA's OV certificate, $150–300/yr.** The fallback when Artifact Signing's geography excludes you. Since June 2023 the CA/Browser Forum requires the private key on an HSM or hardware token, so this is a cloud-HSM or USB-token arrangement rather than a `.pfx` file.

**Recommendation: procure nothing for Windows.** Not because it is expensive, but because the best available outcome there is free, and because it buys a warning with a name in it for an artifact that does not exist on a platform whose blocker is #741 rather than trust. The counter-argument worth taking seriously is the calendar — Artifact Signing's identity validation runs to 20 business days — and it does not apply to either free route, which is the main practical consequence of this decision.

### Decision 7 — the `.deb` stays unsigned, because the default install path checks no package signature

*Linux is not out of scope — Decision 12 is its answer, and it is a better one than a signature would have been. What is out of scope is signing the package itself, for the reason below.*

The `.deb` is unsigned and this PRD leaves it unsigned, and the reason is not effort. Debian's package tooling **disables per-package signature verification by default** — `dpkg` supports it, and `/etc/dpkg/dpkg.cfg` ships with `no-debsig`, so `dpkg -i` on a downloaded `.deb` checks nothing unless the user has changed that default. Signing the artifact with `dpkg-sig` or `debsigs` therefore produces a signature that the installation path this project actually ships against will not look at.

The trust mechanism that *is* real on Linux is a **signed apt repository**, verified by `apt` against a key in the machine's trusted set. That is a distribution channel, not an artifact property: it means hosting a repository, publishing a key, and committing to keeping both alive. Legitimate future work, and not this — it belongs with whatever decides how Linux users are meant to *get* the app, which is closer to #765's territory.

What a direct download can honestly offer is integrity, and it already does: `checksums-desktop-alpha.txt` ships alongside the bundles. **GPG-signing that checksums file was considered and is not needed.** It would let a user who already trusts a published maintainer key verify provenance rather than just integrity — but it needs a published key whose distribution is its own trust problem, and Decision 12 delivers the same property with **no key at all**: the certificate is short-lived, issued to the workflow's own identity, and logged publicly. A GPG key would be strictly more to protect for strictly less. Recorded so it is a decision rather than an omission.

### Decision 8 — no experimental flag, no protocol change, and the updater stays out

**CLAUDE.md rule 9 (experimental flag): no.** The flag gates a *presentation* seam — a render or input binding. This PRD adds a CI job and changes a release-note string. There is no seam, and a feature flag compiled into the binary could not hide a signature on a `.dmg` in any case. Same conclusion and same reason as #740 Decision 10.

**CLAUDE.md rule 12 (cross-version contract): no change, and no manual test.** Nothing here touches the daemon, the TUI↔daemon protocol, orchestration or hooks. `PROTOCOL_VERSION` stays put, no field changes meaning, and there is no contract to test across versions. The only changes are in `.github/workflows/release.yml`, in `xtask/linkage-check`, and in docs. Under `docs/develop/versioning.md`'s `0.x` policy this is **feature → patch**.

**Tauri's updater is a separate PRD and it is not the same key.** Tempting to fold in because "it wants signing anyway", but that is a false economy: the updater's signature is a Tauri-specific **minisign** keypair over the update payload, unrelated to Developer ID and unrelated to Authenticode. It shares only the word. It also needs an update endpoint, a channel policy and a rollback story, none of which this PRD touches.

### Decision 9 — signing does not graduate the GUI out of alpha

The assets keep the `-alpha-` infix and the release-body section keeps its heading. Worth deciding explicitly, because "signed" reads like "ready" and someone will propose dropping the word.

What alpha is carrying is not only the trust apology. #740 Decision 11 named three things that would each make the GUI's install story due, and this PRD is one of them: the other two are [#746](https://github.com/vfarcic/dot-agent-deck/issues/746) (the logo, so screenshots do not go stale immediately) and [#164](https://github.com/vfarcic/dot-agent-deck/issues/164)/[#741](https://github.com/vfarcic/dot-agent-deck/issues/741) (Windows, so the page is not two-thirds of a platform matrix). Renaming the asset would also break every checksum line and every saved link, for a word.

### Decision 10 — the release note becomes a function of what was actually signed

Today's note is a fixed string appended by `desktop-publish`. Decision 3's corrections keep it fixed, because with nothing signed there is still exactly one true story to tell. **The moment macOS is signed and Linux is not, that stops being true** — an unconditional "they are **unsigned**, so your OS will warn you" is then false for one asset and true for the other, and an unconditional replacement is false in the other direction. So at that point the step composes per-platform text from what it can see: the assets present in `dist-desktop/`, and whether `desktop-sign` succeeded.

**The `xattr -dr com.apple.quarantine` line has to disappear from the macOS half the moment macOS is signed**, and that is the sharp end of this decision rather than a tidy-up. A stale workaround on a signed release does not merely mislead — it teaches a habit that transfers, and the next thing that asks a user to recursively strip quarantine from `/Applications` will not be us. A signed release that still prints it is a worse artifact than an unsigned release that prints it honestly.

### Decision 11 — the wiring is guarded statically, and the guard worth adding is "one job names the secret"

`release.yml` fires only on a tag. Nothing can execute it on a PR, so every property of it that matters is enforced by static assertions in `xtask/linkage-check/src/release_workflow_wiring.rs` or by nothing at all — the module's own header says so, and #740's experience of mutation-testing each assertion is why it is trusted.

The existing assertions must keep passing unchanged; if a change to the desktop chain reddens `finalize_does_not_wait_on_the_desktop_jobs` or `desktop_bundle_runs_beside_the_cli_matrix_not_after_it`, that is the guard working and the change is wrong. Signing inserts a job into the `bundle → publish` chain; it must not insert itself anywhere near `build → finalize`.

Three new assertions land with the paid path, and the third earns its place:

1. `desktop-sign` runs after `desktop-bundle` and before `desktop-publish`, and `finalize` still lists none of the three in `needs:`.
2. `desktop-sign` has no `actions/checkout`, mirroring the existing `checks_out` predicate already used for `desktop-publish` — because the entire security property of Decision 4 is that this job runs no repository code.
3. **The signing secrets are named in exactly one job.** A grep of the workflow for `APPLE_CERTIFICATE`, `APPLE_API_KEY` and the rest must find them only inside the `desktop-sign` block. This is the assertion that makes Decision 4 enforceable rather than aspirational: the natural future edit — someone adds `env: APPLE_SIGNING_IDENTITY` to `desktop-bundle` to make a local reproduction easier — is exactly the edit that silently undoes the whole design, and it is invisible in review because it looks like a one-line convenience.

All three must run through the module's existing `code_before_comment` helper, whose docstring already records why: the `desktop-publish` steps carry prose *about* `--repo` and about having no checkout, so a comment can satisfy a naive predicate by talking about the thing. The secret-name assertion has that hazard in its worst direction — a comment in `desktop-bundle` explaining why it holds no credential would, grepped naively, read as `desktop-bundle` holding one, and the guard would fire on the correct state.

The free path needs three assertions of its own. **The note's macOS half offers the System Settings route as well as the command**, so a later edit cannot quietly reduce it back to a single terminal instruction, and it addresses the `.deb` on its own terms — scoped to the note *step* rather than the job, because over a whole job block a substring can be satisfied by something that is not the note. And Decision 12's `attest` job gets two: that **`finalize` does not wait on it** and that it gates on the release existing rather than on the desktop matrix, and that it **holds no credential and needs none** — exactly `id-token: write` and `attestations: write`, no `contents: write`, and no `secrets.` reference at all. That last one is the same shape of guard as the signing-secret assertion above, pointed at the opposite property: the whole argument for provenance over code signing is that no key is involved, and the natural future edit that breaks it is a one-line convenience.

### Decision 12 — build provenance is the free trust mechanism that reaches all three platforms, and it is what reaches Linux where a package signature does not

Decisions 3 through 7 are organised by platform, and that framing has a blind spot: it asks *"what does each OS check?"* and concludes that Linux checks nothing, so Linux gets nothing. The better question is *"what can a user check?"*, and there the answer is the same on every platform and costs nothing. (A signed apt repository would reach Linux users too — Decision 7 says why that is a distribution-channel project rather than an artifact property, and it is not free of hosting or of a key.)

**Every published asset now carries SLSA build provenance**, generated by `actions/attest-build-provenance` in a new `attest` job. A user verifies with one command:

```sh
gh attestation verify <the-file-you-downloaded> --repo vfarcic/dot-agent-deck
```

**This is a stronger claim than the checksums file, and the difference is not academic.** `checksums.txt` says a file matches a number printed on the same page that served the file — it detects corruption, and an attacker who can replace the asset can replace the line. An attestation says *this exact digest was produced by this repository's release workflow, from this commit*, signed by a certificate bound to that workflow identity and recorded in a public transparency log. The checksums files stay, because they are useful without a `gh` install; the attestation is what makes provenance checkable.

**No secret is involved, and that is the point rather than a convenience.** The signing certificate is short-lived and obtained through the job's OIDC token (`id-token: write`), so there is no private key in this repository — nothing to rotate, nothing a leak could expose, and none of Decision 5's threat model to argue about. It is free on public repositories on every current GitHub plan; private and internal repositories need GitHub Enterprise Cloud, which is worth knowing only because it is the one condition attached.

**What it does not do, stated flatly, because "signed" is the word everyone reaches for.** It does not remove the macOS Gatekeeper dialog: Gatekeeper checks Apple's chain and knows nothing about Sigstore. It does not remove SmartScreen. It is not checked by any operating system, package manager or installer on the user's behalf — verification is opt-in and requires the `gh` CLI. Provenance and OS trust are different problems with different mechanisms, and this solves exactly one of them.

**Off the critical path, by the same rule as everything else here.** `attest` hangs off `finalize` and `finalize` does not list it in `needs:`, so an attestation-service blip turns the run red and leaves the CLI release complete. It is gated on `needs.finalize.result == 'success'` rather than on the desktop matrix, because `needs:` on a matrix job resolves to the aggregate and gating on `desktop-publish` would discard the *CLI* binaries' attestations every time a bundler leg failed — the exact defect Greptile's P1 found in `desktop-publish` on #768, one job over. The desktop download step tolerates failure and the subject list is built in shell rather than handed to the action as a glob, because a glob matching nothing is an error there and an empty `dist-desktop/` is a legitimate state.

**One gap, recorded rather than quietly left.** The release body's provenance instructions live in the desktop note, which is the section this PRD owns. The CLI assets are attested too, but a CLI user reading the release page is not told so. Surfacing it for them means adding a fixed section to the body `prepare` assembles from the changelog — a small change to a different job, worth doing and deliberately not smuggled in here.

### Test plan

Release and packaging work again, so CLAUDE.md rule 4's TUI-harness requirement does not engage: there is no pane, status, prompt, focus, layout, mode or hook delivery to observe, and no L1 or L2 test can see a GitHub Release or a Gatekeeper verdict. What is testable is tested statically; the rest is a real run.

| Item | Catalog ID | Tier | Scenario | Action |
|---|---|---|---|---|
| `attest` is off the critical path and not hostage to the desktop matrix | n/a — `xtask/linkage-check` module | fast-tier static | Asserts `finalize` does not list `attest` in `needs:`, and that `attest` gates on `needs.finalize.result == 'success'` rather than on `desktop-publish`. Mutation-tested both ways. | create |
| `attest` holds no credential and needs none | n/a | fast-tier static | Asserts the job declares exactly `id-token: write` and `attestations: write`, carries no `contents: write`, and references no `secrets.`. Mutation-tested by adding `contents: write` and by removing `id-token`. | create |
| The workflow still parses, with valid permission scopes | n/a | manual, pre-commit | `actionlint` over `release.yml` — clean, and confirmed to validate the `attestations` scope by rejecting a bogus value for it. A syntax or scope error here breaks **every** release and no PR check would catch it, since `release.yml` fires only on a tag. | create |
| Provenance actually verifies on a published asset | n/a | manual (first tag) | After the next release: `gh attestation verify <asset> --repo vfarcic/dot-agent-deck` on a downloaded `.dmg`, `.deb` and CLI binary. Nothing before a real tag proves the job runs. | create |
| The note offers a no-terminal route and does not claim an OS warning for the `.deb` | n/a — `xtask/linkage-check` module | fast-tier static | Asserts the note step's code lines mention the System Settings route, and that the "your OS will warn you" clause is not applied to the Linux asset. | create |
| Ad-hoc signing changes the macOS failure mode | n/a | manual (M1) | Download v0.39.3's `.dmg` on a Mac that has never seen this source, record the exact dialog; ad-hoc-sign a locally built bundle, quarantine it by hand, and record the dialog again. Settles Decision 3's hypothesis. | create |
| The signature structure survives the sidecar | n/a | manual (M1) | Ad-hoc-sign the real bundle (`APPLE_SIGNING_IDENTITY="-"`), then `codesign --verify --deep --strict --verbose=4` the nested sidecar and the outer bundle. Targets tauri#11992's failure class; needs no credential. | create |
| `desktop-sign` sits between bundle and publish, and `finalize` still needs none of them | n/a | fast-tier static | Parses `release.yml`'s job graph and asserts the `needs:` edges, extending #740's assertions rather than replacing them. | create (paid path) |
| `desktop-sign` has no `actions/checkout` | n/a | fast-tier static | Reuses the module's existing `checks_out` predicate over code lines only. | create (paid path) |
| The signing secrets appear in exactly one job | n/a | fast-tier static | Greps the workflow's code lines for each `APPLE_*` name and asserts every hit is inside the `desktop-sign` block. Mutation-tested by adding one to `desktop-bundle` and confirming exactly one test reddens. | create (paid path) |
| A published `.dmg` is trusted on a real Mac | n/a | manual (M6) | Download from a real release on a clean machine. `xcrun stapler validate` the `.dmg`, `spctl -a -t open --context context:primary-signature -v` it, mount it, `spctl -a -vvv` the `.app`, and launch with the network **off** to prove the ticket is stapled rather than fetched. | create (paid path) |
| Existing e2e / L1 / L2 suites | — | — | Unchanged. No production source behaviour moves. | skip |

Per CLAUDE.md rule 5, the tests covering what this PRD touches are `xtask/linkage-check`'s `release_workflow_wiring.rs` and `sidecar_staging.rs`, plus `cargo test-fast`. Named here so the obligation is checkable.

The honest limit is the same one #740 recorded and it has not improved: no test in this repository can execute `release.yml`, and **notarization cannot be exercised at all without the certificate and the account**. M1 shrinks the untested surface to Apple's half; M6 is the only thing that closes it.

## Success Criteria

**The free position, which is the one being taken:**

- Every asset on a release — the `.dmg`, the `.deb` and the four CLI binaries — carries build provenance a user can verify with `gh attestation verify <file> --repo vfarcic/dot-agent-deck`. **Met in code; provable only by a real tag.**
- That provenance costs no credential: the `attest` job holds `id-token: write` and `attestations: write`, no `contents: write`, and references no secret. **Met, and asserted.**
- Linux users get a verification that something actually checks, rather than a package signature that `dpkg -i` ignores. **Met.**
- The release note offers a macOS route that does not require a terminal, and no longer tells `.deb` users their OS will warn them. **Met.**
- Neither property can silently regress: a static assertion in `xtask/linkage-check` fails the build if the note offers the terminal command as its only macOS route, or stops addressing the `.deb` on its own terms. **Met.**
- The macOS failure mode is *measured* rather than assumed, and ad-hoc signing is applied if and only if the measurement supports it — so a user meets *"Apple could not verify…"* with an *Open Anyway* route rather than *"damaged — Move to Trash"*. **Open (M1).**
- Whether tauri#11992's failure class reaches this app is known, for free, rather than being a surprise waiting for whoever revisits the paid path. **Open (M1).**
- The Linux `.deb` and the CLI assets are unaffected in how they are produced, named and checksummed. **Met.**
- A bundling **or attestation** failure still leaves the CLI release complete and on schedule and turns the run red — PRD #740's topology is untouched, its assertions still pass, and `finalize` waits on none of the three new jobs. **Met, and asserted.**

**What the free position explicitly does not achieve**, stated as a criterion because leaving it implicit is how a trade quietly becomes a belief:

- macOS users **do** meet a security dialog on first launch. Every user, every release, until the $99 decision is revisited. Provenance does not touch this: Gatekeeper checks Apple's chain and knows nothing about Sigstore.
- No Windows user is helped, because there is no Windows artifact to help them with.
- Provenance is **opt-in and requires the `gh` CLI**. No operating system, package manager or installer checks it on a user's behalf, so its practical reach is whoever chooses to run one command.

**The acceptance test a reversal would have to pass**, preserved rather than deleted: a user downloads the `.dmg` on a clean Mac, drags the app to `/Applications`, and launches it **offline** with no dialog beyond the ordinary "downloaded from the internet" confirmation; `xcrun stapler validate` succeeds and `spctl` reports a Notarized Developer ID; and no signing credential is named anywhere in `release.yml` outside `desktop-sign`.

## Milestones

The free-options decision of Decision 2 settles M3 and takes M4 and M6 off the board. They are kept, struck through and with their reopening condition named, because a milestone deleted is a decision that has to be made again.

- [ ] **M1 — Measure, on a Mac, for free.** Two questions, one session, no credential and no money: does ad-hoc signing turn *"damaged"* into *"unverified, Open Anyway"* (Decision 3's hypothesis), and does `codesign --verify --deep --strict` survive the daemon sidecar (tauri#11992's failure class)? **The only open piece of free work, and the only thing that can still improve a macOS user's first launch.** Needs Apple hardware, which is available; needs nothing else.
- [x] **M2 — Ship the free note work.** *(Landed in PR [#879](https://github.com/vfarcic/dot-agent-deck/pull/879).)* The note corrections of Decision 3 and their static assertion `the_alpha_note_does_not_leave_a_user_with_only_a_terminal_command`, mutation-tested both ways. **Ad-hoc signing is deliberately not part of this milestone** — it belongs to M1's result, and shipping it on a hypothesis is the thing this repository's discipline exists to prevent.
- [x] **M2a — Ship build provenance.** *(Landed in PR [#879](https://github.com/vfarcic/dot-agent-deck/pull/879).)* The `attest` job of Decision 12, its two static assertions, and the verification command in the release note. Free, no credential, all three platforms, and the only trust mechanism that reaches the Linux `.deb`. **Proof requires a real tag** — nothing in this repository can execute `release.yml`, so the first tagged release is where this is genuinely tested, exactly as PRD #740's M6 was.
- [x] **M3 — The $99 decision.** *Answered 2026-09-04: **no**.* Free options only, on every platform. Decision 2 records that this is a spend-or-accept-the-warning choice rather than a build-or-buy one, so the answer carries a permanent consequence: the macOS dialog stays.
- [ ] ~~**M4 — `desktop-sign`, wired and guarded.**~~ **Not being built.** Reopens only if M3 is revisited. The design is Decision 4 and the assertions are Decision 11; both are written down so a reversal is a build rather than a re-think.
- [ ] **M5 — Docs.** Scoped down by M3 to what actually landed: `docs/develop/desktop-gui.md`'s **Release bundles** section now records both guarded properties of the note and points here. A `docs/develop/desktop-signing.md` is **not** written, because there is no signing to document — writing a certificate-lifecycle page for a certificate nobody holds is the kind of documentation that rots unread. User-facing install instructions stay with [#765](https://github.com/vfarcic/dot-agent-deck/issues/765). Remaining: fold M1's measured answer in once it exists.
- [ ] ~~**M6 — Verified by a real signed release.**~~ **Not reachable.** There will be no signed release to verify. Its substance — download on a clean Mac, `stapler validate`, `spctl`, launch offline — is the acceptance test a reversal would have to pass, and is preserved in the test plan for that purpose.

## Windows, restated as a dependency rather than a milestone

Nothing in this PRD produces a Windows artifact, and nothing in it is what stands between the project and one. The order of operations, whenever someone picks it up, is:

1. **An artifact has to exist first.** [#164](https://github.com/vfarcic/dot-agent-deck/issues/164) for a Windows daemon/CLI binary; [#741](https://github.com/vfarcic/dot-agent-deck/issues/741) for a Windows GUI, whose blocker is reachability rather than packaging.
2. **Then the free trust question, in Decision 6's ranking** — Microsoft Store MSIX first (the only route with no warning at all, and free), SignPath Foundation second (free, warning stays, publisher name appears and reputation accrues).
3. **The paid options stay unranked and unbought**, because EV stopped bypassing SmartScreen in 2024 and free now beats paid on this platform.

The one thing worth doing before any of that is fixing the Scoop manifest that already points at a nonexistent `.exe` with an empty hash — Decision 6 has the detail, and it belongs to #164.

## Risks

The decision to take the free ceiling removes most of this list — a certificate that is never bought cannot expire, leak, or be revoked. What is left is what the *free* position carries, plus what a reversal would inherit.

- **The macOS dialog is now a standing cost, not a temporary one.** Every macOS user meets a security prompt on first launch, on every release, indefinitely. That is the accepted price of the free-options decision and it should be re-weighed if desktop adoption ever matters more than $99/yr does. Nothing in this PRD reduces it below "a dialog with an *Open Anyway* route"; the *only* mechanism that removes it is the one Decision 2 records as unavailable for free.
- **M1's hypothesis may not hold, and then the free ceiling is lower than this document hopes.** If ad-hoc signing does not change the dialog, the free position is exactly what shipped in M2 — better wording, two routes — and nothing more. Written as a hypothesis rather than a plan precisely so that outcome is a measurement rather than a disappointment.
- **tauri#11992 — notarization fails with `externalBin`.** Open upstream, no workaround reported, and this app's sidecar is not optional. Dormant while nothing is notarized, and the first thing a reversal would trip over. M1 answers the cheap half of it for free either way, which is why M1 is still worth running.
- **Gatekeeper's behaviour is a moving target, and the free position depends entirely on its details.** Sequoia removed the Control-click override; Homebrew's cask policy changed on 2026-09-01; Decision 3's whole hypothesis is about which dialog appears. A position built on "the warning is survivable" needs re-checking at each major macOS release in a way that a notarized artifact would not.
- **Homebrew is now closed to this project's GUI.** Casks must be codesigned and notarized. Not a regression caused here, but a door that the free decision keeps shut — worth knowing before anyone proposes a cask as the install story.
- **Nothing has executed the `attest` job, and nothing in this repository can.** `release.yml` fires only on a tag. `actionlint` says the file parses and its permission scopes are valid, the wiring assertions say the topology is right, and neither of those is the same as the job having run. The first tagged release is the test, and the containment is that a failure there turns the run red and leaves the release complete.
- **Provenance is easy to over-read.** "Signed" is the word people reach for, and an attestation is not an OS-trusted signature: Gatekeeper and SmartScreen ignore it, and verification needs the `gh` CLI. Decision 12 states this explicitly, and the release note says what the check proves rather than implying the warnings are gone. The risk is a future summary quietly upgrading it.
- **Scope creep toward Windows.** Every Windows question that arises belongs to #164 or #741 first and to a signing decision second. Decision 6 records the facts and the ranking so they do not have to be rediscovered.

## Open Questions

The $99 question is answered (no) and the Windows question is answered by there being no artifact. Two things remain, and neither blocks anything shipped.

1. **Does #757 close once M1 is run, or stay open as "revisit when adoption justifies $99"?** The argument for closing is that the decision is made and the free work is done; the argument for leaving it open is that the decision is explicitly a cost trade rather than a technical conclusion, and a stale open issue is a cheaper reminder than a rediscovery.
2. **M1 needs someone at a Mac.** The commands are in the test plan and the whole thing is one session. Until it is run, whether ad-hoc signing improves the first-launch dialog stays a hypothesis in this document rather than an answer.

## Work Log

### 2026-09-04 — Created

Written against `main` at `a023377`, from issue #757's placeholder body. The issue's shape held — three platforms, macOS moderate, Windows hard, Linux optional — but five of its specifics did not, and the corrections changed the plan rather than decorating it.

- **"EV removes that at higher cost"** is no longer true. Microsoft's current guidance states that EV certificates no longer bypass SmartScreen and that paying the premium for that purpose is "no longer justified". This retires the issue's OV-vs-EV question entirely.
- **The cost asymmetry runs the opposite way to the issue's framing.** Windows — "the hard one" — has a *free* path that produces a strictly better outcome than any paid one: an MSIX submitted to the Microsoft Store is re-signed by Microsoft, and Microsoft's own comparison table marks it as the only option with no SmartScreen warning. macOS — "moderate, mostly waiting" — has no free path at all.
- **"Whether Windows trust is worth the recurring cost at this stage"** presumes a Windows artifact. There is none: v0.39.3 publishes no Windows executable of any kind and neither release matrix names a Windows triple.
- **Azure Trusted Signing has been renamed Azure Artifact Signing**, and its binding constraint turned out to be geography rather than the organisation-verification burden the issue anticipated: individual developers must be in the US or Canada, organizations have a wider list. It also does not issue EV certificates and states no plan to — which, given the point above, is no longer a limitation.
- **Linux is not merely "optional", it is inert.** `dpkg` ships with per-package signature verification disabled (`no-debsig`), so a signed `.deb` installed with `dpkg -i` is checked by nothing. The real mechanism is a signed apt repository, which is a distribution channel rather than an artifact property.

And three things the issue did not anticipate, two of them shaping the design:

- **[tauri-apps/tauri#11992](https://github.com/tauri-apps/tauri/issues/11992) is open and reports notarization failing specifically when `externalBin` is used.** This app has one — the daemon sidecar, which is the feature that makes an installed GUI work without a Rust toolchain. That risk is why M1 runs before the money question: ad-hoc signing (`APPLE_SIGNING_IDENTITY="-"`) produces a real signature structure with no account, so `codesign --verify --deep --strict` is a free proxy for the failure class.
- **Homebrew closed the free macOS route on 1 September 2026**, three days before this was written: `--no-quarantine` removed, and unsigned/unnotarized casks dropped from the tap. A cask is now a *consumer* of this PRD's paid path rather than a substitute for it.
- **Signing in the bundling job would put the private key alongside automerged dependency code.** `renovate.json` automerges cargo patch bumps and ≥1.0 minor bumps with no human review, and `desktop-bundle` compiles that graph including build scripts. Hence Decision 4's job split. The npm lane is the *better* one here — `desktop/**` npm bumps are held for a human and carry a three-day minimum release age — so the argument rests on cargo and on action-ref digests, not on the npm supply chain generally.

**Maintainer input, same day.** Asked whether the certificate should live in CI, on a Mac, or nowhere, the answer was a question back: *"Is there an alternative to paying for it? I'd like the best possible experience for users but without paying for anything"* — and the same for Windows. That reframed the document around cost, which is why Decision 2 exists at all and why the milestones now put the free work first and the spend decision third. A Mac is confirmed available, so M1 and M6 are both reachable.

Two claims were deliberately narrowed rather than repaired, per CLAUDE.md rule 17. "No consequential secret lives in `release.yml`" is false — `RELEASE_TOKEN` is an admin PAT that bypasses the `main-protected` ruleset — so Decision 5 states the narrower and more useful fact: `release.yml` has no `pull_request` trigger, so no outside contributor can cause it to run. And "signing removes the Windows warning" is false at any price for a direct download; the accurate version is that it puts a verified publisher name in the warning and starts reputation accumulating, and that only the Store route removes the warning.

One claim is recorded as an explicit **hypothesis rather than a fact**, because it is the crux of the free recommendation and there was no Mac in this session to settle it: that ad-hoc signing converts the *"damaged and can't be opened"* dialog into the *"Apple could not verify…"* dialog with an *Open Anyway* route. The mechanism — macOS reports "damaged" when it cannot verify a quarantined file's signature at all — is well attested and the exact dialog is not, so M1 measures it before M2 acts on it.

Figures re-checked on 2026-09-04 against Apple, Microsoft Learn, Azure pricing, Homebrew and SignPath. The one figure carried over from the issue without independent verification is its $300–600/yr cloud-HSM range; Microsoft's own current page puts OV certificates at $150–300/yr, and Decision 6's ranking does not depend on either.

### 2026-09-04 — Decided: free options only

The maintainer's answer to the $99 question was **no** — free options on every platform — together with a check on the goal: *"The goal is to have dmg and exe files in the releases in GitHub. Correct?"*

**Half of that goal is already met and half of it is not this PRD's to meet, and the distinction matters enough to record.** The `.dmg` has been on the release since v0.39.3. There is **no `.exe` of any kind** — not a GUI one and not a CLI one — and signing is not what stands between the project and one: a Windows daemon/CLI binary is #164, and a Windows GUI bundle is gated on #741's named-pipe-versus-Unix-socket reachability problem. No certificate, free or paid, produces an artifact. Decision 6 now says this flatly, because the belief is actively manufactured by the repository itself: the published Scoop manifest for v0.39.3 points at `dot-agent-deck-windows-amd64.exe` with `"hash": ""` and a URL that 404s, which was verified against `vfarcic/scoop-bucket` on the day this was written.

**What the decision costs, recorded where it cannot be missed.** On macOS the free ceiling is a *better dialog*, never the absence of one. Decision 2 enumerates the four closed routes — a free Apple account cannot notarize, the fee waiver's eligibility page excludes individuals and sole proprietors in as many words, the Mac App Store needs both the same membership and an App Sandbox a process-supervising sidecar does not fit, and Homebrew removed `--no-quarantine` and began dropping unsigned casks on 2026-09-01. So every macOS user meets a security prompt on first launch, on every release, until this is revisited. That is now the Status line, a Decision, a Success Criterion and a Risk, deliberately, because a trade this shape is exactly the kind that decays into an assumption that the problem was solved.

Decisions 4 and 5 — the split-job topology and the threat model — are kept struck through rather than deleted, and M4 and M6 with them. The research behind them is a day's work and the reasoning is the part that would be lost; a milestone deleted is a decision that has to be made again.

**One free thing is still open and is worth running.** M1 needs a Mac and one session: whether ad-hoc signing (`APPLE_SIGNING_IDENTITY="-"`, no account, no money) converts *"damaged — Move to Trash"* into *"Apple could not verify…"* with an *Open Anyway* route, and whether the daemon sidecar survives `codesign --verify --deep --strict`. The first is the last free improvement available to a macOS user's first launch; the second retires tauri#11992's failure class for free whether or not the paid path is ever taken. It stays a **hypothesis** in this document until it is measured — shipping the config change on the strength of the mechanism alone is precisely what this repository's discipline exists to prevent.

**Review round (Greptile on PR #879).** One P2, valid and fixed here: the PRD still read "Not started" while the same PR landed M2. The status line, the milestones and the success criteria now record what shipped and what did not. Greptile's file overview separately noted that the new assertion's substring checks were scoped to the whole `desktop-publish` job rather than to the note step — also right, and also fixed: the guard now extracts the note step and asserts against that, so an unrelated step mentioning `dpkg` can no longer satisfy it.

### 2026-09-04 — Linux included, and the free option that had been missed

The maintainer corrected the goal: *"I made a mistake by mentioning only mac and Windows. Linux releases should be included as well."*

The literal half of that was already satisfied — `dot-agent-deck-desktop-alpha-linux-amd64.deb` has been on the release since v0.39.3, alongside two Linux CLI binaries, so Linux is in fact the **best**-covered platform for artifacts and the only one with both a CLI and a GUI download. But the correction exposed a real gap in this document's reasoning, and it was a framing error rather than a missing fact.

**Decisions 3 through 7 were organised by asking "what does each OS check?", and that question has a blind spot.** It correctly concluded that nothing checks a `.deb` signature and then let that stand as "Linux gets nothing", which is a different claim and a false one. Asking instead *"what can a user check?"* produces an answer that is the same on every platform, costs nothing, and had been left out: **build provenance**.

Hence Decision 12 and the `attest` job. `actions/attest-build-provenance` obtains a short-lived Sigstore certificate through the job's OIDC token, so there is **no private key in this repository** — none of Decision 5's threat model applies, there is nothing to rotate, and nothing a leak could expose. It is free on public repositories on every current GitHub plan. Every asset is covered: the `.dmg`, the `.deb` and the four CLI binaries, verified with `gh attestation verify <file> --repo vfarcic/dot-agent-deck`.

It also retires the GPG-signed-checksums option Decision 7 had been holding open: provenance delivers the same property with no key to publish, protect or distribute, which is strictly less to get wrong.

**What it is not, because "signed" is the word everyone will reach for.** It does not remove the macOS Gatekeeper dialog — Gatekeeper checks Apple's chain and knows nothing about Sigstore — and it does not remove SmartScreen. No OS, package manager or installer checks it on a user's behalf; verification is opt-in and needs the `gh` CLI. Decision 12 says this in its own words, and the release note describes what the check proves rather than implying the warnings are gone.

**Two claims elsewhere in this document became false and were narrowed, per CLAUDE.md rule 17.** Decision 1's heading said macOS "is the whole of the engineering scope", which stopped being true the moment a cross-platform job landed; it now scopes the *OS-trust* question only, and says so where the table could otherwise be read as "nothing would help Linux". And Decision 12's own heading initially claimed provenance was "the only" mechanism reaching Linux — a signed apt repository would too, so it now says it is what reaches Linux *where a package signature does not*.

**On testing a workflow nothing can execute.** `release.yml` fires only on a tag, so a syntax error or an invalid permission scope in it would break every future release with no PR check to catch it — a materially worse failure than the runtime kind, which the topology contains. `actionlint` was run over the file (clean) and confirmed to actually validate the new scopes by rejecting a bogus value for `attestations`. The two new wiring assertions were mutation-tested four ways: making `finalize` wait on `attest`, gating `attest` on the desktop matrix, granting it `contents: write`, and dropping `id-token`. Each reddens exactly the intended test. None of that proves the job runs; the first tagged release is where that is learned, and the containment is that `finalize` does not wait on it.

**One gap left deliberately.** The provenance instructions live in the desktop note, which is the section this PRD owns. The CLI assets are attested too, but a CLI user reading the release page is not told so — surfacing it means a fixed section in the body `prepare` assembles from the changelog, a small change to a different job that is recorded in Decision 12 rather than smuggled in here.
