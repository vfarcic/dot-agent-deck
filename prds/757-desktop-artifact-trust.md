# PRD #757: Make the published desktop artifacts trusted

**Status**: Not started — the free work is scoped and shippable; the paid work is **blocked on one question**: whether to spend the Apple Developer Program's $99/yr. There is **no free path to a trusted macOS app** (Decision 2), and on Windows the free path is *better* than any paid one — but there is no Windows artifact yet. See [Open Questions](#open-questions).
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

**Free, shippable now, and not blocked on anything:** stop the release note lying, and — if a measurement supports it — ad-hoc-sign the macOS bundle so its failure mode stops being *damaged, Move to Trash*. Neither needs a certificate, an account, or a secret.

**$99/yr, and the maintainer's call:** Developer ID signing plus Apple notarization of the macOS bundle, done in a job that holds the signing key and runs nothing else — no checkout, no `pnpm install`, no `cargo build`, no third-party build scripts. This is the only thing that actually removes the warning, and there is no free substitute for it.

**Free, but not yet:** Windows. The free path there is the *best* path — a Microsoft Store MSIX submission is the only option in Microsoft's own comparison table that produces no SmartScreen warning at all, better than a $400/yr EV certificate — and it is moot until a Windows artifact exists at all.

The topology PRD #740 chose is untouched throughout: `desktop-bundle` still hangs off `prepare`, `finalize` still lists no desktop job in `needs:`, and a signing failure still turns the run red while leaving the CLI release complete. Signing adds a job to an existing chain; it does not get between the CLI matrix and `finalize`.

## Scope

### In Scope

- **Free:** correcting the release-body note — the macOS half gains the System Settings route it is missing, and the Linux half stops claiming an OS warning that does not exist (Decision 3).
- **Free:** measuring, on a real Mac, whether ad-hoc signing changes the macOS failure mode, and whether the daemon sidecar survives `codesign --verify --deep --strict` — the second being a no-cost proxy for an open upstream Tauri bug that would otherwise be discovered after the certificate is bought (Decision 3).
- **Paid, if approved:** Developer ID Application signing and Apple notarization of the macOS arm64 bundle, with the ticket stapled so a first launch works offline.
- **Paid, if approved:** a `desktop-sign` job that is the only place in `release.yml` where a signing credential is named, with a static assertion enforcing it (Decisions 4 and 11).
- A developer-facing `docs/develop/desktop-signing.md` covering whichever of the above lands, plus the certificate lifecycle, secret inventory, rotation and revocation runbook if the paid path is taken.
- A written, costed statement of the Windows options, so nobody re-derives them and so the free-beats-paid finding is not lost (Decision 6).

### Out of Scope

- **Acquiring, generating or configuring any certificate, and adding any secret to this repository.** That is the maintainer's decision and this PRD does not pre-empt it.
- **Windows Authenticode, and the Microsoft Store submission.** There is no Windows artifact to sign (Decision 6). The facts are recorded; the engineering is not scoped.
- **Linux `.deb` signing.** The default install path verifies nothing (Decision 7).
- **A Homebrew cask for the GUI.** #740 put it out of scope; Homebrew's 2026-09-01 policy now makes it *depend* on this PRD's paid path rather than substitute for it. If the $99 is spent, a cask becomes possible and is worth its own issue.
- **Tauri's auto-updater.** Its signature is a Tauri-specific **minisign** keypair over the update payload, unrelated to Developer ID and unrelated to Authenticode — it shares only the word "signing". It also needs an update endpoint, a channel policy and a rollback story. #740 deferred it and this PRD leaves it deferred (Decision 8).
- **Graduating the GUI out of alpha** (Decision 9), and **a published install page** — [#765](https://github.com/vfarcic/dot-agent-deck/issues/765) owns that, and this PRD is one of the three things it names as making it due.
- **A universal (x86_64 + arm64) macOS bundle.** #740 Decision 2 measured Intel Mac demand at one CLI download across seven releases; signing does not change that arithmetic.

## Technical Approach

### Decision 1 — macOS is the whole of the engineering scope, because it is the only platform with both an artifact and a trust gate

Three platforms, three genuinely different states, and the difference is not a matter of degree:

| Platform | Artifact published today | Does the OS gate it? | Would a signature help? |
| --- | --- | --- | --- |
| macOS arm64 | `…-macos-arm64.dmg`, since v0.39.3 | Yes — quarantined, and per #757 reads as *damaged* | Yes, and it is the documented fix |
| Windows | **none** | n/a | n/a — nothing to sign (Decision 6) |
| Linux x86_64 | `…-linux-amd64.deb`, since v0.39.3 | No | No — the default install path checks none (Decision 7) |

That table is the whole scoping argument. The issue's own estimate — "roughly a week of engineering; a calendar window several times that, driven almost entirely by Windows" — assumed three platforms of work. One of them has no artifact and one has no gate, so the engineering is one platform's worth and the calendar is Apple's enrollment time rather than Microsoft's verification time.

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

**Linux: free already, and already adequate** — see Decision 7.

### Decision 3 — the free macOS work is a better failure mode and an honest note, and it is worth doing whatever the $99 answer is

Two pieces, and the second is already justified while the first needs a measurement.

**Correct the note. No measurement needed, and it stands on its own.** Today's text has one route and one false clause:

- It gives only `xattr -dr com.apple.quarantine`, and macOS Sequoia removed the Control-click bypass, so a user unwilling to run a recursive command against `/Applications` is offered nothing. **System Settings → Privacy & Security → *Open Anyway*** is the no-terminal route and belongs in the note ahead of the command, not instead of it — some users will still want the one-liner.
- It tells `.deb` users "they are **unsigned**, so your OS will warn you", and the second clause is simply false for them: `dpkg -i` warns about nothing (Decision 7). The Linux half should say what is true — unsigned, integrity verifiable against `checksums-desktop-alpha.txt`, no OS gate to defeat.

**Ad-hoc-sign the bundle, if a measurement says it helps.** The hypothesis is specific: macOS reports *"is damaged and can't be opened"* when Gatekeeper cannot verify a quarantined file's signature at all, and reports *"cannot be opened because Apple cannot check it for malicious software"* — with an *Open Anyway* route in System Settings — when there is a verifiable but untrusted signature. macOS supports ad-hoc signing with the pseudo-identity `-`, which produces a real signature structure and no trust, for free. If the hypothesis holds, one line in the bundling job converts *"damaged — Move to Trash"* into *"unverified — Open Anyway"*, which is the difference between an app that looks broken and an app that looks unsigned.

**That hypothesis is explicitly unverified and M1 exists to settle it, not to assume it.** The mechanism is well attested and the exact dialog is not something this PRD has evidence for, and it is the kind of claim that is embarrassing to ship as a fact. It costs one download and two launches on a Mac to know.

The same M1 session buys a second thing for free, and this one is a genuine risk retirement: **[tauri-apps/tauri#11992](https://github.com/tauri-apps/tauri/issues/11992) is open and reports notarization failing with *"The signature of the binary is invalid"* (error 4000) precisely when `externalBin` is configured**, resolving when it is removed. This app has an `externalBin` — `binaries/dot-agent-deck`, the daemon sidecar, which is the feature that makes an installed GUI work without a Rust toolchain. Discovering that after buying a certificate would be the expensive order to discover it in, and an ad-hoc signature is enough to run `codesign --verify --deep --strict --verbose=4` over the nested sidecar and the outer bundle, which is a direct proxy for the failure class. **Free, and it is the reason M1 comes before the money question rather than after it.**

### Decision 4 — if the $99 is spent, the signing key never shares a runner with third-party build code

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

### Decision 5 — a signing secret goes on a runner, and this is the argument for it rather than an assumption

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

**What to do when one does exist, in order.** Decision 2 has the comparison table; this is the ranking that falls out of it.

1. **Microsoft Store, MSIX. Free, and the only option with no SmartScreen warning at all.** The obstacle is packaging, not money or identity: **Tauri has no native MSIX bundle target** ([tauri-apps/tauri#4818](https://github.com/tauri-apps/tauri/issues/4818), open for years), and its own Microsoft Store page documents the *Win32 MSI/EXE* submission route instead — which per Microsoft's table is the one where **the publisher must sign**, so it is not the free route. Reaching the free route means an MSIX built by something outside Tauri: Microsoft's own `winapp` CLI publishes a Tauri guide, and there is a community packager. That is real work with real unknowns, and it is the work worth doing rather than buying a certificate.
2. **SignPath Foundation. Free OV signing for open-source projects**, HSM-backed, named by Microsoft's own documentation. It does not remove the warning — OV means reputation accrues — but it puts a verified publisher name in it and starts accumulating that reputation for nothing. Its conditions are substantive: verifiable builds from the project's own source, MFA across the team, an Authors/Reviewers/Approvers split, manual approval per release, and a published *code signing policy* page. The MIT license with no commercial dual-licensing satisfies the license criterion.
3. **Azure Artifact Signing, ~$9.99/mo.** Only worth it if the two above are unavailable. Its private key is never released to the customer, which is a materially better property than a `.p12`: CI holds `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET` / `AZURE_TENANT_ID`, a service-principal credential rotated in one portal action. It needs a **paid** Azure subscription (free, trial, sponsored, Visual Studio and student subscriptions are refused), identity validation takes **1 to 20 business days**, and eligibility is geographic: organizations across the US, Canada, EU and UK; **individual developers in the United States or Canada only**. It does not issue EV certificates and states no plan to — which, given that EV no longer helps SmartScreen, is no longer a limitation.
4. **A CA's OV certificate, $150–300/yr.** The fallback when Artifact Signing's geography excludes you. Since June 2023 the CA/Browser Forum requires the private key on an HSM or hardware token, so this is a cloud-HSM or USB-token arrangement rather than a `.pfx` file.

**Recommendation: procure nothing for Windows.** Not because it is expensive, but because the best available outcome there is free, and because it buys a warning with a name in it for an artifact that does not exist on a platform whose blocker is #741 rather than trust. The counter-argument worth taking seriously is the calendar — Artifact Signing's identity validation runs to 20 business days — and it does not apply to either free route, which is the main practical consequence of this decision.

### Decision 7 — Linux is out of scope because the default install path checks no package signature

The `.deb` is unsigned and this PRD leaves it unsigned, and the reason is not effort. Debian's package tooling **disables per-package signature verification by default** — `dpkg` supports it, and `/etc/dpkg/dpkg.cfg` ships with `no-debsig`, so `dpkg -i` on a downloaded `.deb` checks nothing unless the user has changed that default. Signing the artifact with `dpkg-sig` or `debsigs` therefore produces a signature that the installation path this project actually ships against will not look at.

The trust mechanism that *is* real on Linux is a **signed apt repository**, verified by `apt` against a key in the machine's trusted set. That is a distribution channel, not an artifact property: it means hosting a repository, publishing a key, and committing to keeping both alive. Legitimate future work, and not this — it belongs with whatever decides how Linux users are meant to *get* the app, which is closer to #765's territory.

What a direct download can honestly offer is integrity, and it already does: `checksums-desktop-alpha.txt` ships alongside the bundles. **Deliberately not proposed here:** GPG-signing that checksums file. It is cheap, and would let a user who already trusts a published maintainer key verify provenance rather than just integrity — but it needs a published key whose distribution is its own trust problem, nobody has asked, and adding a second signing key to reason about while arguing about the first is a poor trade. Recorded so it is a decision rather than an omission.

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

The free path needs one assertion of its own, and it is the cheapest guard in the PRD: **the note's macOS half offers the System Settings route as well as the command**, so a later edit cannot quietly reduce it back to a single terminal instruction.

### Test plan

Release and packaging work again, so CLAUDE.md rule 4's TUI-harness requirement does not engage: there is no pane, status, prompt, focus, layout, mode or hook delivery to observe, and no L1 or L2 test can see a GitHub Release or a Gatekeeper verdict. What is testable is tested statically; the rest is a real run.

| Item | Catalog ID | Tier | Scenario | Action |
|---|---|---|---|---|
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

**Free path (no decision required):**

- The release note offers a macOS route that does not require a terminal, and no longer tells `.deb` users their OS will warn them.
- The macOS failure mode is measured rather than assumed, and if ad-hoc signing improves it, it is applied — so a user meets *"Apple could not verify…"* with an *Open Anyway* route rather than *"damaged — Move to Trash"*.
- Whether tauri#11992's failure class reaches this app is known before any money is spent.

**Paid path (if approved):**

- A user downloads the `.dmg`, opens it, drags the app to `/Applications`, and launches it — no terminal command, no System Settings visit, and no security dialog beyond the ordinary "downloaded from the internet" confirmation.
- The launch works **offline**, proving the ticket is stapled to the artifact rather than fetched.
- `xcrun stapler validate` succeeds on the downloaded `.dmg`, and `spctl` accepts both the `.dmg` and the mounted `.app`, reporting a Notarized Developer ID.
- The release body never carries the `xattr` instruction on a release whose macOS asset was signed, and never claims a signature on one that was not.
- No signing credential is named anywhere in `release.yml` outside the `desktop-sign` job, and a static test fails if one appears.

**Both:**

- A signing or bundling failure leaves the CLI release complete and on schedule, exactly as today, and turns the run red.
- The Linux `.deb` and the CLI assets are unaffected in how they are produced, named and checksummed.

## Milestones

- [ ] **M1 — Measure, on a Mac, for free.** Two questions, one session, no credential and no money: does ad-hoc signing turn *"damaged"* into *"unverified, Open Anyway"* (Decision 3's hypothesis), and does `codesign --verify --deep --strict` survive the daemon sidecar (tauri#11992's failure class)? **This is deliberately first, before the money question**, because both answers change what the $99 buys and one of them could invalidate the whole paid path. Needs Apple hardware; needs nothing else.
- [ ] **M2 — Ship the free work.** The note corrections of Decision 3, their static assertion, and ad-hoc signing **if and only if M1 says it helps**. Blocked on nothing. This is the increment that improves a user's day whatever the answer to M3 is.
- [ ] **M3 — The $99 decision.** *Blocked on the maintainer.* Whether to enroll in the Apple Developer Program and hold the resulting certificate as a repository secret, per Decisions 4 and 5. Decision 2 records that there is no free substitute, so this is a spend-or-accept-the-warning choice rather than a build-or-buy one.
- [ ] **M4 — `desktop-sign`, wired and guarded.** *Blocked on M3.* The new job per Decision 4, `desktop-bundle` switched to emitting a `ditto` archive of the `.app`, and the three static assertions of Decision 11, each mutation-tested the way #740's were.
- [ ] **M5 — Docs.** `docs/develop/desktop-signing.md`, scoped to whichever path landed — and if the paid one did, carrying the certificate lifecycle, the secret inventory, rotation, and the revocation runbook — plus a pointer from `docs/develop/desktop-gui.md`'s **Release bundles** section. Developer-facing, so under `docs/develop/` and **not** in `site/sidebars.js`, per CLAUDE.md rule 11. User-facing install instructions stay with [#765](https://github.com/vfarcic/dot-agent-deck/issues/765), which this PRD unblocks rather than absorbs.
- [ ] **M6 — Verified by a real signed release.** *Blocked on M3.* A real tag produces a signed, notarized, stapled `.dmg`; it is downloaded on a Mac that has never seen this source tree, checked with `stapler` and `spctl`, and launched offline. Not collapsible into "CI was green" — a green run that produced a correctly-signed but unnotarized bundle is exactly the failure this gate exists for.

## Risks

- **tauri#11992 — notarization fails with `externalBin`.** Open upstream, no workaround reported, and this app's sidecar is not optional. M1 is scheduled before M3 precisely because of this. Its result may force the fallback: sign the sidecar explicitly before bundling, or sign the nested binaries by hand in `desktop-sign` — which Decision 4's design already does, so the exposure is smaller here than for a project using Tauri's integrated signing.
- **Half the mechanism cannot be tested until the credential exists**, and the untestable half is Apple's. Not a scheduling risk to be managed away; it is why M6 is a milestone rather than a formality.
- **Certificates expire on a schedule, and `release.yml`'s existing guard pattern will not catch it.** The `RELEASE_TOKEN` guard tests for emptiness (`-z`), and CLAUDE.md rule 8 already records that an expired-but-present token passes it and dies later. That failure mode is *worse* for a certificate, because expiry is a known future date rather than an accident: the Developer ID certificate expires, the membership renews annually, and the App Store Connect API key can be revoked independently. `desktop-sign` should check the certificate's `notAfter` and fail early with the date in the message — cheap, offline, and the one check that turns an annual surprise into a warning.
- **Revocation is the remediation and it is destructive.** Decision 5 states it: a leaked key is fixed by revoking a certificate that, once revoked, prevents already-installed copies from launching. The M5 runbook exists so that decision is not made for the first time under pressure.
- **macOS runner minutes are free only while the repository is public.** #740 recorded this for the bundler; `desktop-sign` adds a second macOS job, and `notarytool --wait` can block for minutes while Apple's service processes the submission.
- **Gatekeeper's behaviour is a moving target and the free path depends on its details.** Sequoia removed the Control-click override; Homebrew's cask policy changed on 2026-09-01; Decision 3's whole hypothesis is about which dialog appears. Every claim of that kind here should be re-checked, not assumed, at the next major macOS release.
- **Scope creep toward Windows.** Every Windows signing question that arises belongs to whatever PRD ships a Windows bundle. Decision 6 records the facts and the ranking so they do not have to be rediscovered; it does not open the door to scoping the job.

## Open Questions

1. **Spend $99/yr on the Apple Developer Program?** This is the only question that gates anything, and Decision 2 is the reason it cannot be avoided: a free Apple Developer account cannot notarize, the fee waiver covers only nonprofits, educational institutions and government entities, the Mac App Store needs the same membership and would not accept a process-spawning sidecar, and Homebrew closed the cask loophole on 1 September 2026. **There is no free path to a trusted macOS app.** The alternatives are: pay, or take M2's free improvement and leave the warning in place.
2. **If yes — hold the certificate in CI (Decision 4's split job) or sign by hand on release day?** Decision 5 has the threat model. The key point is that a leak's remediation is Apple revoking a certificate, which stops already-installed copies from launching.
3. **If no — is M2 the settled answer, or should the note simply be corrected and the PRD closed?** M2 is worth doing either way; the question is whether #757 stays open as "waiting for a certificate" or is closed as "decided against, warning documented honestly".
4. **Windows: confirm nothing is to be procured.** Decision 6 recommends it, and the reason is now that free beats paid rather than that paid is expensive — a Microsoft Store MSIX is the only option with no SmartScreen warning at all, and SignPath Foundation offers free OV signing to OSS projects. Both are moot until a Windows artifact exists, and neither has a procurement lead time to start early.

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
