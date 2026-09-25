# Desktop signing and notarization

The macOS desktop bundle is signed with an Apple **Developer ID Application** certificate and notarized by Apple in one job, `desktop-sign` in `.github/workflows/release.yml`. This page is the maintainer's reference for the credentials that job holds: what they are, how to register them, what the workflow does with and without them, when they expire, how to rotate them, and what to do if one leaks. It is the threat-model page [PRD #757](../../prds/757-desktop-artifact-trust.md) Decision 5 said would be owed if the paid path were ever taken, and on 2026-09-24 it was.

**Read the leak section before generating the certificate, not after.** An exposed Developer ID private key is not rotated — it is **revoked**, revocation is not self-service, and per PRD #757 Decision 5 it stops software signed with that certificate from installing **and stops already-installed copies from launching**. That is the property that makes this unlike adding an API key, and the maintainer accepted it when reversing the decision.

The Linux `.deb` is not signed and this page does not change that; PRD #757 Decision 7 has the reason. There is no Windows artifact to sign (Decision 6).

## What the workflow does

`desktop-bundle`'s macOS leg builds the `.app` with `tauri build --bundles app --no-sign` and hands it on as a `ditto` archive under the artifact name `desktop-app-macos-arm64`. It does not build a `.dmg` any more. `desktop-sign` downloads that archive, checks its member list (see [Validating the app archive](#validating-the-app-archive)), restores it with `ditto -x -k`, and then does one of three things, decided by its **Classify the signing credentials** step:

| The five secrets | What `desktop-sign` does | `signed` output | Job |
| --- | --- | --- | --- |
| all empty (today) | builds an **unsigned** `.dmg` with `hdiutil`; nothing is signed | `false` | green |
| all set | signs the app, notarizes and staples it, builds the `.dmg`, signs, notarizes and staples that | `true` | green when Apple accepts both submissions |
| some set, some empty | refuses, naming which are missing (names only, never values) | empty | **red** |

A registered set also fails the job when the certificate has expired or expires within 24 hours, when the `.p12` does not hold exactly one valid `Developer ID Application` identity, when `codesign --verify --deep --strict` rejects the signed app, and when either notarization submission comes back anything other than `Accepted` — in which case the job prints `xcrun notarytool log` for the submission. None of these falls back to an unsigned build: once credentials are registered, an unsigned `.dmg` would mean something went wrong, and shipping it silently is the failure this is designed to avoid.

When `desktop-sign` fails, there is no `.dmg` on the release. `desktop-publish` still publishes the `.deb`, warns that the `macos-arm64` asset is missing, and writes a release note with no macOS paragraph. `finalize` waits on none of the desktop jobs, so the CLI release is unaffected either way — the run turns red and the release stays complete, which is PRD #740's topology unchanged.

The disk image is the finished `.app` plus an `/Applications` symlink for drag-install, built by `hdiutil` in both paths. It is plainer than the `.dmg` Tauri used to produce, which had a background image and a laid-out window: Tauri's dmg bundler runs inside `tauri build`, on the job that must not hold the key.

The release note is composed from what was published (Decision 10). With `signed=true` its macOS paragraph says the build is signed and notarized and carries **no** Gatekeeper workaround; with `signed=false` it keeps the unsigned text, the System Settings route and the `xattr -dr com.apple.quarantine` command. Anything other than an explicit `true` beside a published `.dmg` is described as unsigned, because that is the safe direction to be wrong in.

## Why the key is in its own job

PRD #757 Decision 4, in one paragraph: `desktop-bundle` compiles the desktop crate's full Rust dependency graph and installs `desktop/`'s npm graph, and Renovate automerges cargo patch bumps, and minor bumps of crates already at 1.0 or later, on green CI with no human in the loop. A cargo build script is arbitrary code. Putting `APPLE_CERTIFICATE` in that job's environment — which is what Tauri's documented integrated signing does — would make the private key readable by whatever a dependency update brought in. So `desktop-sign` has no `actions/checkout`; it runs its own inline steps, two SHA-pinned artifact actions (`actions/download-artifact` and `actions/upload-artifact`) and the macOS runner's system tools, and none of its steps invokes the binaries it signs. The two actions are the third-party code on that runner, which is why Renovate holds their updates for a person ([below](#dependency-updates-on-the-signing-runner)).

**What the split does not buy, stated so it is not read as more.** `desktop-bundle` can still put anything it likes inside the app, and `desktop-sign` will sign it. The split keeps the key from being *exfiltrated* by build-time code; it does not vouch for what that code produced. Apple's notary scan and its audit trail of submissions are the check on that, and a malicious submission can have its notarization ticket revoked, which is a narrower instrument than revoking the certificate.

The alternatives — signing inside `desktop-bundle`, a self-hosted macOS runner, signing by hand on release day — are argued and rejected in Decision 4; the self-hosted runner is the counterintuitive worst option for a public repository.

`xtask/linkage-check/src/release_workflow_wiring.rs` holds regression checks for the specific edits most likely to undo this, and they run in `cargo test-fast` because `release.yml` runs on no pull request. Each one catches the pattern it names; none of them proves the isolation in general, and the property actually keeping the key off other runners is GitHub's environment scoping, not these tests:

- `desktop_sign_is_between_bundle_and_publish_off_the_cli_path` reads the `needs:` lists: `desktop-sign` needs `desktop-bundle`, `desktop-publish` needs `desktop-sign`, and `finalize` needs none of the three desktop jobs or `attest`.
- `desktop_sign_does_not_check_out_repository_code` looks for `uses: actions/checkout` on a code line of `desktop-sign`. It does not see repository code fetched some other way — a `git clone` in a `run:` step, a different checkout action, `gh api …/contents`.
- `apple_signing_secrets_are_named_only_in_desktop_sign` looks for the five literal secret names on non-comment lines outside `desktop-sign`. It does not see a secret reached without its name; an environment secret is already absent from `toJSON(secrets)` in a job that declares no environment, which is the scoping doing the work rather than this check.
- `desktop_sign_declares_the_signing_environment` looks for the literal `environment: desktop-signing` (or the mapping form with that name).

The natural future edit that undoes the design — adding `APPLE_SIGNING_IDENTITY` to `desktop-bundle` "to make a local reproduction easier" — is the one the third check was written for. The same file also checks the `ditto` hand-off, the hardened-runtime and JIT entitlements, the two expiry thresholds, the release note's unsigned-only workaround and the credential classifier's shell behaviour, and runs each guard against an in-memory workflow mutated to break it; read the file for the current list rather than this paragraph.

## The secret inventory

All five are **environment secrets** of the `desktop-signing` environment, not repository secrets. The handoff comment on issue #757 said "repository secrets"; that was before the environment was part of the design, and the environment is what the workflow now reads.

| Secret | What it holds | What it is worth to an attacker |
| --- | --- | --- |
| `APPLE_CERTIFICATE` | base64 of the Developer ID Application `.p12` — **the private key itself**, with its certificate | The ability to sign anything as this publisher, until the certificate is revoked |
| `APPLE_CERTIFICATE_PASSWORD` | the `.p12` export password | Nothing alone; everything together with the above |
| `APPLE_API_KEY` | the App Store Connect API **key id** (10 characters) | Identifies the key; nothing without the `.p8` |
| `APPLE_API_ISSUER` | the App Store Connect **issuer id** (a UUID) | Identifies the team; nothing without the `.p8` |
| `APPLE_API_KEY_PATH` | the **contents** of the `.p8` key file | With the two ids: submitting software for notarization under this account, until the key is revoked |

**`APPLE_API_KEY_PATH` holds the file's contents, not a path**, despite its name. A secret cannot be a file, so the workflow writes the value to `$RUNNER_TEMP/AuthKey.p8` at run time and deletes it in the job's final `if: always()` step. The name is kept because it is the one the handoff checklist registered, and renaming a secret means re-registering it.

The App Store Connect API key is used rather than an Apple ID plus app-specific password (Decision 5): the key is per-key revocable from the portal, carries a role, and is not the account password.

### Why an environment, and what its restriction does

GitHub makes an environment's secrets available only to jobs that reference that environment. Two consequences are concrete here. First, a job in `release.yml` that does not declare the environment does not receive them — including `docs`, which calls `docs-publish.yml` with `secrets: inherit` and would forward every *repository* secret to that reusable workflow. Second, the environment's deployment-ref restriction limits which refs can reach them at all.

**Configure the restriction to exactly the refs a release runs from:**

- **tag** `v*` — the normal path. `/tag-release` dispatches `tag-release.yml`, which pushes the `v<version>` tag, and the tag push triggers `release.yml` with `github.ref` = `refs/tags/v<version>`.
- **branch** `main` — `release.yml`'s own `workflow_dispatch` escape hatch. A dispatch runs with `github.ref` set to the branch it was dispatched from, and `main` is the branch a release is cut from. `main` is covered by the `main-protected` ruleset, so its `release.yml` is reviewed code.

A run from any other ref — a branch carrying a modified `release.yml`, dispatched by anyone with write access — then fails `desktop-sign` at the environment gate instead of reading the key, and `desktop-publish` still ships the `.deb`. `release.yml` has no `pull_request` trigger, so a fork's pull request does not run it.

**Deliberately no required reviewer and no wait timer** (Decision 5, point 4). A reviewer prompt here would be the maintainer approving their own release, which is ceremony rather than scoping. The access scoping is real; the approval click would not be.

**What the restriction does not cover: tag creation.** The `v*` rule admits *any* ref named `v…` under `refs/tags/`, and a tag carries its own copy of `release.yml`. So anyone who can create a `v*` tag can point it at a commit whose `release.yml` does whatever they like in a job that declares `environment: desktop-signing`, and that job receives the secrets — the environment checks the ref's *name*, not who made it or what it points at. Nothing narrower stood in the way when this was written (2026-09-24): the repository's one ruleset, `main-protected`, targets branches, and every collaborator with the write role can push tags. The same holds for dispatching `release.yml` *on* an existing `v*` tag, which also runs that tag's copy. **The tag namespace is therefore part of the boundary, and a tag ruleset is a mandatory step, taken before any secret is registered** — registration step 7.

This cannot be enforced from inside `release.yml`. A step that checked "was this tag made by the release identity?" would live in the very file an attacker's tag replaces, so it would be absent from precisely the run it was meant to stop. It would read as protection and be none; the check has to live in repository settings, where a tag cannot carry its own copy.

**The environment does not exist yet.** `gh api repos/vfarcic/dot-agent-deck/environments/desktop-signing` returned `404` on 2026-09-24. GitHub creates a referenced environment on first use if it does not exist, **with no protection rules**, so the first release after this change creates `desktop-signing` bare, admitting every ref. That is harmless while it holds no secrets, and it is why the ruleset and the ref restriction come before the secrets in the steps below. Each run that uses the environment also records a deployment on the repository's Deployments page.

## Registering the credentials

This needs the Apple Developer account, admin access to this repository, and a Mac for the Keychain steps.

1. **Enrol in the Apple Developer Program** (about $99 a year) at <https://developer.apple.com/programs/enroll/>. Individual enrolment is sufficient. Approval takes hours to days, and it is the only step that blocks everything else.
2. **Create the Developer ID Application certificate**, as the account holder (Apple restricts who on a team may create Developer ID certificates; on an individual enrolment that is you). Either in Xcode (**Settings > Accounts > Manage Certificates > + > Developer ID Application**), or on the developer portal under **Certificates, Identifiers & Profiles > Certificates > +** choosing **Developer ID Application** with the **G2 Sub-CA** profile, uploading a certificate signing request made with Keychain Access (**Certificate Assistant > Request a Certificate From a Certificate Authority**, saved to disk). Download the `.cer` and open it, so it pairs with its private key in the login keychain. It must be **Developer ID Application** — not *Apple Development*, *Apple Distribution*, or *Developer ID Installer*; the workflow looks for exactly one `Developer ID Application` identity and refuses otherwise.
3. **Export the `.p12`.** In Keychain Access, under **My Certificates**, select `Developer ID Application: <name> (<team id>)` — the row that expands to show its private key — and **Export** it as a `.p12` with a strong, unique password. That password is `APPLE_CERTIFICATE_PASSWORD`.
4. **Check it and read its expiry** before registering anything: `openssl pkcs12 -in DeveloperID.p12 -nokeys -clcerts | openssl x509 -noout -subject -enddate`. The subject should name `Developer ID Application`. OpenSSL 3 needs `-legacy` on the first command to read a `.p12` exported with the older RC2-40 encryption; the workflow tries the plain form first and retries with `-legacy`, so either export works.
5. **Base64 it** as a single line: `base64 -i DeveloperID.p12 -o DeveloperID.p12.b64` on macOS (`base64 -w0 DeveloperID.p12 > DeveloperID.p12.b64` with GNU coreutils). That file's contents are `APPLE_CERTIFICATE`.
6. **Create the App Store Connect API key.** In App Store Connect, **Users and Access > Integrations > App Store Connect API > Team Keys > Generate API Key**, with the **Developer** role — enough to submit for notarization, and less than Admin or App Manager. Download the `.p8` immediately: Apple offers the download once. Record the **Key ID** (`APPLE_API_KEY`) and the **Issuer ID** shown above the key list (`APPLE_API_ISSUER`).
7. **Create the tag ruleset — MANDATORY, and before any secret exists.** Without it, anyone who can push a `v*` tag can run their own `release.yml` with the signing secrets ([why](#why-an-environment-and-what-its-restriction-does)). The ruleset restricts creating, moving and deleting `refs/tags/v*` to the release identity. That identity is `RELEASE_TOKEN`, the maintainer's admin PAT, under which `tag-release.yml` runs `git push origin "refs/tags/v${VERSION}"`; a PAT acts as the user who owns it, so the bypass actor is the repository **admin** role (`RepositoryRole` id `5`) — the same bypass `main-protected` uses. On a user-owned repository that role is the owner plus any collaborator granted admin, so read who that is before relying on it: `gh api repos/vfarcic/dot-agent-deck/collaborators --jq '.[] | select(.role_name == "admin") | .login'` (on 2026-09-24: the owner alone). Do **not** add Renovate's app (`Integration` 2740, which `main-protected` admits for pull requests) or the GitHub Actions app as bypass actors: either would let a workflow on any branch mint a `v*` tag with `GITHUB_TOKEN`, and a `v*` tag, however it was made, can then be dispatched against.

   In the UI: **Settings > Rules > Rulesets > New ruleset > New tag ruleset**, name `release-tags`, enforcement **Active**, bypass list **Repository admin** (always allow), target tags by pattern `v*` and `v*/**`, and enable **Restrict creations**, **Restrict updates** and **Restrict deletions**. The same with `gh`:

   ```sh
   gh api -X POST repos/vfarcic/dot-agent-deck/rulesets --input - <<'EOF'
   {
     "name": "release-tags",
     "target": "tag",
     "enforcement": "active",
     "conditions": {"ref_name": {"include": ["refs/tags/v*", "refs/tags/v*/**"], "exclude": []}},
     "rules": [{"type": "creation"}, {"type": "update"}, {"type": "deletion"}],
     "bypass_actors": [{"actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "always"}]
   }
   EOF
   ```

   The second pattern is there because a ruleset's `*` does not cross `/` (per GitHub's ruleset pattern documentation), and whether the environment's `v*` tag rule does is not something this page has measured; covering both shapes costs nothing. **One consequence for the escape hatch:** `release.yml`'s `workflow_dispatch` from `main` with a version whose tag does not exist yet asks `softprops/action-gh-release` to create the release, and with it the tag, under `GITHUB_TOKEN`. The ruleset is expected to refuse that creation, failing `finalize` (unmeasured — no dispatch has run against the ruleset). Push the tag first, as an admin, and the push runs the release by itself; a dispatch against a tag that already exists creates no tag.

8. **Create and restrict the environment.** In the repository, **Settings > Environments > New environment** named `desktop-signing` (or open it if a release already created it). Under **Deployment branches and tags** choose **Selected branches and tags** and add two rules: branch `main`, and tag `v*`. Add no required reviewers and no wait timer. The same with `gh`:

   ```sh
   gh api -X PUT repos/vfarcic/dot-agent-deck/environments/desktop-signing \
     --input - <<'EOF'
   {"deployment_branch_policy": {"protected_branches": false, "custom_branch_policies": true}}
   EOF
   gh api -X POST repos/vfarcic/dot-agent-deck/environments/desktop-signing/deployment-branch-policies -f name=main -f type=branch
   gh api -X POST repos/vfarcic/dot-agent-deck/environments/desktop-signing/deployment-branch-policies -f name='v*' -f type=tag
   ```

9. **Read the effective state back before registering anything.** The steps above are settings a later edit can loosen, so check what is actually in force rather than what was typed:

   ```sh
   # Every ruleset, then each one's target, enforcement, patterns, rules and bypass list.
   gh api repos/vfarcic/dot-agent-deck/rulesets --jq '.[] | [.id, .name, .target, .enforcement] | @tsv'
   for id in $(gh api repos/vfarcic/dot-agent-deck/rulesets --jq '.[].id'); do
     gh api "repos/vfarcic/dot-agent-deck/rulesets/$id" \
       --jq '{name, target, enforcement, include: .conditions.ref_name.include, rules: [.rules[].type], bypass: .bypass_actors}'
   done
   # The environment's ref restriction, its protection rules, and the admins the bypass admits.
   gh api repos/vfarcic/dot-agent-deck/environments/desktop-signing --jq '{deployment_branch_policy, protection_rules}'
   gh api repos/vfarcic/dot-agent-deck/environments/desktop-signing/deployment-branch-policies --jq '.branch_policies[] | [.type, .name] | @tsv'
   gh api repos/vfarcic/dot-agent-deck/collaborators --jq '.[] | select(.role_name == "admin") | .login'
   ```

   Proceed only when a ruleset with `target: tag`, `enforcement: active`, both `refs/tags/v*` patterns and the rules `creation`, `update` and `deletion` is listed, its bypass list is the admin role alone, and the environment shows `custom_branch_policies: true` with exactly `branch main` and `tag v*`.

   Also check the tags that already exist, since the ruleset stops new ones and cannot vouch for old ones: `git fetch origin --tags`, then `for t in $(git tag -l 'v*'); do git grep -q desktop-signing "$t" -- .github/workflows/ && echo "$t"; done` should print nothing before the first signed release. A tag whose `release.yml` does not name the environment cannot reach its secrets, and on 2026-09-24 none of the 102 `v*` tags did (two of them, `v0.25.1` and `v0.31.0`, point at commits that are not ancestors of `main`; neither names the environment).

10. **Register the five environment secrets** — the environment's **Add environment secret**, or `gh secret set … --env desktop-signing`, which reads the value from standard input so it does not land in shell history:

    ```sh
    gh secret set APPLE_CERTIFICATE          --env desktop-signing --repo vfarcic/dot-agent-deck < DeveloperID.p12.b64
    gh secret set APPLE_CERTIFICATE_PASSWORD --env desktop-signing --repo vfarcic/dot-agent-deck   # prompts
    gh secret set APPLE_API_KEY              --env desktop-signing --repo vfarcic/dot-agent-deck   # prompts: the key id
    gh secret set APPLE_API_ISSUER           --env desktop-signing --repo vfarcic/dot-agent-deck   # prompts: the issuer id
    gh secret set APPLE_API_KEY_PATH         --env desktop-signing --repo vfarcic/dot-agent-deck < AuthKey_<KEYID>.p8
    gh secret list --env desktop-signing --repo vfarcic/dot-agent-deck
    ```

    Register all five before the next release: a partial set fails `desktop-sign` by design.
11. **Put the originals somewhere safe and delete the working copies.** The `.p12` and its password, and the `.p8`, belong in a password manager or other offline store; the `.p8` cannot be downloaded again. Remove `DeveloperID.p12`, `DeveloperID.p12.b64` and the `.p8` from wherever they were exported.
12. **Check the first signed release** with the acceptance commands below. `release.yml` runs on a `v*` tag push or a manual dispatch, so nothing before the next release exercises the signing path.

## Certificate expiry

A Developer ID Application certificate is valid for **five years** from issue. The expiry is on the certificate itself, so read it from the `.p12` (step 4 above), from Keychain Access, or from the certificate list in the developer portal. The secret cannot be read back out of GitHub.

**What the job checks.** On every signed run, before anything enters the throwaway keychain, `desktop-sign` reads the certificate straight out of the `.p12` (`openssl pkcs12 -nokeys -clcerts`, retrying with `-legacy`), requires exactly one certificate-with-key whose subject names `Developer ID Application`, and runs `openssl x509 -enddate` and `-checkend` on it. After the import it requires the keychain's one valid `Developer ID Application` identity to have that certificate's SHA-1, so the certificate whose expiry was checked is the one that signs. Reading the `.p12` rather than the keychain is deliberate: `security find-identity -v` hides an expired identity, and a name search (`security find-certificate -c`) returns the first match, which need not be the signing certificate. It logs the certificate's `notAfter` date, **warns** when the certificate expires within 30 days, and **fails** when it has expired or expires within 24 hours. The one-day margin is because the job signs twice and can run for up to three hours after the check. Before this check existed the only guard was an emptiness test, which an expired certificate passes — PRD #757 Decision 5 names that as the defect to close.

**Expiry is not revocation.** Every signature the job makes carries a secure timestamp (`codesign --timestamp`), so software signed while the certificate was valid keeps launching after it expires. An expired certificate stops *new* signing; it does not touch installed copies.

**Renewal**, which is replacement — Apple does not extend a certificate:

1. Create a new Developer ID Application certificate (registration step 2), ideally when the job's 30-day warning first appears.
2. Export it as a new `.p12` with a new password, check it (step 4), base64 it (step 5).
3. Replace `APPLE_CERTIFICATE` and `APPLE_CERTIFICATE_PASSWORD` in the `desktop-signing` environment together — one without the other fails the import.
4. Cut the next release as normal and confirm its `desktop-sign` log prints the new `notAfter`.
5. **Do not revoke the old certificate.** Let it expire. Revoking it is the leak remedy below, and it would stop every release signed with it from launching.

## Rotating the App Store Connect API key

The API key is the credential that *can* be rotated, and doing so has no effect on anything already signed or notarized.

1. Generate a new team key with the **Developer** role (registration step 6) and download its `.p8`.
2. Replace `APPLE_API_KEY` and `APPLE_API_KEY_PATH` in the environment. `APPLE_API_ISSUER` is the team's and does not change.
3. Confirm the next release's notarization steps succeed.
4. **Revoke the old key** in App Store Connect (**Users and Access > Integrations > App Store Connect API**). Revocation is immediate and self-service.

Rotate when a maintainer with access leaves, whenever a leak is suspected, and otherwise on whatever schedule the maintainer prefers.

## If a credential leaks

Treat any appearance of a value outside the `desktop-signing` environment as a leak — a job log that printed one, a secret moved to a repository secret, a copy found on disk. GitHub masks registered secret values in logs, which reduces this risk and does not remove it.

**The API key alone (`APPLE_API_KEY_PATH`, with or without the ids).**

1. Revoke the key in App Store Connect immediately.
2. Rotate it (above).
3. Check the notarization history for submissions nobody made: `xcrun notarytool history --key <new AuthKey.p8> --key-id <new key id> --issuer <issuer id>`. Report any unknown submission to Apple so its ticket can be revoked.

**The certificate (`APPLE_CERTIFICATE`, and especially with `APPLE_CERTIFICATE_PASSWORD`).** This is the serious case, and there is no quiet fix.

1. Delete `APPLE_CERTIFICATE` and `APPLE_CERTIFICATE_PASSWORD` from the environment, so the workflow stops using the key. That does not un-leak it; it stops making it worse.
2. Email **product-security@apple.com** with the team id, the certificate's serial number and expiry, and what is known about when and how it was exposed, and ask for the certificate to be revoked. Revocation is not self-service.
3. Understand what revocation does before it happens, and say it in the report: per PRD #757 Decision 5 it stops software signed with the certificate from installing **and stops already-installed copies of every release signed with it from launching**. That is a remote kill switch on every installed copy, fired by us.
4. Revoke and rotate the API key as well, since it was on the same runner.
5. Create a new certificate, re-register (steps 2–5 and 10, after re-running step 9's read-back), and cut a new release so users have a build that launches.
6. Tell users: a pinned issue and a paragraph in the next release's notes saying which releases were signed with the revoked certificate and that they need the new one.

## tauri#11992: the sidecar and notarization

[tauri-apps/tauri#11992](https://github.com/tauri-apps/tauri/issues/11992) is open and reports notarization failing with *"The signature of the binary is invalid"* (error 4000) when `externalBin` is configured. This app has one: `binaries/dot-agent-deck`, the daemon sidecar, which lands at `Contents/MacOS/dot-agent-deck` and is what lets an installed GUI work without a Rust toolchain. It is not optional.

The issue is about Tauri's integrated signing, which this workflow does not use, and `desktop-sign` does what the failure class suggests is missing: it signs every nested Mach-O binary itself, deepest first, with the hardened runtime and a secure timestamp, before sealing the bundle, and runs `codesign --verify --deep --strict --verbose=4` over the result before anything is submitted. **None of that is measured yet.** Whether it is enough is learned on the first signed release (M6) — or earlier and for free by PRD #757's M1 ad-hoc run. If notarization rejects the bundle, the job prints `notarytool log`, which names the offending file and the reason.

## Entitlements

The app is signed with a hardened-runtime entitlements file written inline in the **Sign the app** step, not read from the tree: `desktop-sign` has no checkout, and a plist carried over from `desktop-bundle` would let the job that runs third-party build code choose what the key vouches for. The step's comment carries the reasoning for each key; in short:

- `com.apple.security.cs.allow-jit` — PRD #757 Decision 4 records its absence as producing a bundle that signs cleanly and crashes on launch. **Unmeasured for this app**: WKWebView runs JavaScript in Apple's own WebContent process, so the host may not need it. Kept until M6 measures it, because the cost of omitting it wrongly is a signed release that cannot start.
- `com.apple.security.device.audio-input` — voice capture (PRD #802 M7) opens the microphone, and the hardened runtime denies audio input to a process without this entitlement.

Deliberately absent: `allow-unsigned-executable-memory`, `disable-library-validation` and `get-task-allow`. The daemon sidecar is signed with the hardened runtime and **no** entitlements. Adding a key means adding its reason to that comment.

## Dependency updates on the signing runner

`desktop-sign` runs two pieces of third-party code: `actions/download-artifact` and `actions/upload-artifact`, each pinned by commit SHA. The download runs first and can change what every later step executes (a step can write `$GITHUB_PATH` or `$GITHUB_ENV`, for instance); the upload runs while the throwaway keychain holding the private key and `$RUNNER_TEMP/AuthKey.p8` are still on the runner.

Renovate's `GitHub Actions` rule automerges digest, pin, patch and minor updates of action refs on green CI, which would land a new commit of either action next to the key with nobody reading it. So `renovate.json` ends with a rule that matches `.github/workflows/release.yml` and those two package names and sets `automerge: false`, `groupName: null` and the labels `dependencies` and `manual-review`:

- **`automerge: false`** holds the PR for a person.
- **`groupName: null`** takes the update out of the `GitHub Actions` group, so it arrives as its own PR rather than inside a grouped one. Measured with a local `renovate --platform=local --dry-run=full` (Renovate 44.115.0, 2026-09-24) against `upload-artifact` pinned one patch behind in both `ci.yml` and `release.yml`: with the rule, `ci.yml`'s update went to the grouped `renovate/patch-github-actions` branch and `release.yml`'s to its own `renovate/actions-upload-artifact-7.0.x`; without it, both went to `renovate/patch-github-actions`. The dry run does not print the merged `automerge` value, so that half rests on Renovate's documented rule precedence rather than on the measurement.
- **It is the last `packageRules` entry.** Later rules override earlier ones, so no rule after it can hand these updates back to the group.
- **It is scoped by file, not by job**, so it also holds every other job's uses of the two actions in `release.yml`. Elsewhere they keep automerging.

`renovate-config-validator` (the same Renovate version) accepts the file. If `desktop-sign` ever gains another `uses:`, add that action to the rule.

**Before merging one of these PRs**, check the new digest against the action's own repository — `gh api repos/actions/upload-artifact/commits/<tag> --jq .sha` resolves the tag Renovate names to its commit, dereferencing an annotated tag — and read the diff between the old and new commits. A green CI run does not mean the new action code ran: no pull-request check executes `release.yml`.

## Validating the app archive

`desktop-sign` restores an archive that `desktop-bundle` made, and `desktop-bundle` runs third-party build code. So before `ditto -x -k` writes anything, the step **Validate the app archive before extracting it** reads the zip's member list with `python3`'s standard-library `zipfile` and refuses the archive when an entry:

- has an absolute name, a `..`, `.` or empty path component, or a NUL or backslash in its raw name;
- is neither a regular file, a directory nor a symlink — a device, fifo or socket — judged from the Unix mode bits;
- lies outside the one top-level `<name>.app/` directory, other than under `__MACOSX/`, where `ditto --sequesterRsrc` keeps extended attributes and resource forks as plain files — and there only when it describes something inside that `.app`: `__MACOSX/<name>.app/…`, or `__MACOSX/._<name>.app`, the AppleDouble file for the bundle root itself, which `--keepParent` stores beside it whenever the `.app` directory carries an extended attribute (a `._` prefix is stripped from a file's last component before the compare), plus the bare `__MACOSX/` directory entry; any symlink under `__MACOSX/` is refused;
- repeats a name, or sits beneath an archived symlink, compared after Unicode normalisation and case folding because the runner's APFS volume is case- and normalisation-insensitive by default;
- is a symlink whose target is absolute or empty, or which, resolved inside the archive (following other archived symlinks), does not land inside the `.app`.

It also opens every member, which makes `zipfile` reject one whose local header names a different path than its central-directory entry. The step is self-contained — it reads only `APP_ARCHIVE` and uses nothing macOS-specific — so its `run:` block can be lifted out and run on Linux against constructed hostile zips.

**Why symlinks are allowed at all.** A legitimate bundle can carry them: a versioned macOS framework has `Versions/Current -> A` and top-level links into it. This bundle configures no framework today (`tauri.conf.json` sets no `bundle.macOS.frameworks`), but rejecting every symlink would make the first framework dependency fail the release over a layout Apple requires. A link that resolves inside the bundle can only alias something the archive already controls; one that leaves it is how an unpack writes, or a later step reads, outside the unpack root. That is where the line is drawn.

After `ditto` has run, the **Unpack the app** step checks what was actually written: `find` for device, fifo and socket files, and every symlink's fully resolved target against the unpack root.

**What this does not establish.** `ditto`'s own handling of absolute paths, `..` components and write-through-symlink entries has not been measured on a Mac, so this page makes no claim about what `ditto` would have done without the check. The check reads the central directory; a local file header that no central-directory entry points at is not seen by it, and which of the two `ditto` reads is also unmeasured. Size limits (a decompression bomb) are not checked. And none of it says anything about what the files *contain* — see [what the split does not buy](#why-the-key-is-in-its-own-job).

## The Apple intermediate pin

The **Import the certificate into a throwaway keychain** step imports Apple's Developer ID G2 intermediate beside the `.p12`, so the signing chain resolves whether or not the `.p12` was exported with it. It fetches `https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer` with `curl --proto '=https' --proto-redir '=https' -fsSL`, so a redirect cannot downgrade it to plain HTTP, and it compares the SHA-256 of the DER bytes with `G2_SHA256` in `release.yml` before `security import`. **A mismatch fails the job** with a message saying to review and update the pin; it does not fall back to importing whatever was served.

The pin, `f16cd3c54c7f83cea4bf1a3e6a0819c8aaa8e4a1528fd144715f350643d2df3a`, was computed on 2026-09-24 from that URL, which answered HTTP 200 with no redirect and 1090 bytes: subject `CN=Developer ID Certification Authority, OU=G2, O=Apple Inc., C=US`, issuer `Apple Root CA`, serial `7FB4003FCD97497ACB834D92A48A7873C2845D43`, valid from 2021-09-22 to **2031-09-17**. It needs replacing before that date, or sooner if Apple reissues it. To recompute:

```sh
curl --proto '=https' --proto-redir '=https' -fsSL https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer -o DeveloperIDG2CA.cer
shasum -a 256 DeveloperIDG2CA.cer
openssl x509 -inform DER -in DeveloperIDG2CA.cer -noout -subject -issuer -serial -dates -fingerprint -sha256
```

Accept a new value only when the subject, issuer and dates describe the Developer ID G2 intermediate issued by `Apple Root CA`, ideally cross-checked against the certificate list on Apple's [certificate authority page](https://www.apple.com/certificateauthority/) from a second network. Update `G2_SHA256` and the comment beside it together.

## Acceptance on a real Mac (M6)

On a Mac that has never built this source, download the release's `.dmg`, then:

```sh
DMG=dot-agent-deck-desktop-alpha-macos-arm64.dmg
gh attestation verify "$DMG" --repo vfarcic/dot-agent-deck --signer-workflow vfarcic/dot-agent-deck/.github/workflows/release.yml
xcrun stapler validate "$DMG"
spctl -a -t open --context context:primary-signature -v "$DMG"      # accepted, source=Notarized Developer ID
hdiutil attach "$DMG"
cp -R "/Volumes/Agent Deck/Agent Deck.app" /Applications/
hdiutil detach "/Volumes/Agent Deck"
xcrun stapler validate "/Applications/Agent Deck.app"
spctl -a -vvv "/Applications/Agent Deck.app"                        # accepted, source=Notarized Developer ID
codesign --verify --deep --strict --verbose=4 "/Applications/Agent Deck.app"
codesign -d --entitlements - "/Applications/Agent Deck.app"
```

Then **turn the network off** and launch the app from Finder. It should open with no dialog beyond the ordinary confirmation for an app downloaded from the internet. The offline launch is the part that tests stapling: a notarized but unstapled app passes online, because Gatekeeper fetches the ticket, and fails on a machine with no network. While there, hold the microphone open once (the audio-input entitlement) and note whether the WebView renders and runs — the evidence for keeping or dropping `allow-jit`. Record the results in PRD #757's work log.
