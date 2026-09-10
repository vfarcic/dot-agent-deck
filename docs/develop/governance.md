# Governance: maintainers and the protected `main`

This page describes how changes reach `main`, who may approve them, and — because the two are inseparable here — why turning the gate on requires a CI change first. It is maintainer-facing and deliberately unpublished (CLAUDE.md rule 11).

## The model

`main` is protected by a repository ruleset named `main-protected`. Every change lands through a pull request with at least one approving review. A maintainer's own pull request is reviewed by another maintainer. The repository owner retains ownership and can override anything, either by holding the `admin` bypass or by disabling the ruleset outright.

Two properties of that arrangement are worth stating plainly rather than discovering later.

**The admin bypass is what keeps releases alive, and it also softens the rule for the owner.** CI pushes two commits directly to `main`, so *something* has to be allowed past the gate. Granting the bypass to the `admin` repository role covers CI's PAT and, unavoidably, covers the owner's own hands at the same time. Enforcement against the owner is therefore a matter of habit, not of mechanism. The stricter arrangement — no admin bypass, with a GitHub App token as the sole bypass actor — is available and is described under [Making the gate bind the owner too](#making-the-gate-bind-the-owner-too).

**A gate needs two maintainers before it means anything.** Nobody can approve their own pull request. With a single collaborator, "requires one approving review" means every pull request that person opens is unmergeable without a bypass, so every merge becomes a bypass and the rule decays into ceremony within a week. The rollout below is sequenced around that fact.

## Who counts as a maintainer

GitHub counts an approving review only from an account with **write** or **admin** permission. The set of people who can satisfy "one approving review" is therefore exactly the collaborator list, which [`MAINTAINERS.md`](../../MAINTAINERS.md) documents so it is visible in the repository and not only in repository settings.

[`.github/CODEOWNERS`](../../.github/CODEOWNERS) exists, but only as a **router** and only as a single pathless rule: `* @vfarcic @prageethw`. That distinction carries the whole decision, because the original choice here was to have no `CODEOWNERS` at all and the reasoning behind it still holds for the shape it rejected. What was rejected is *per-path* ownership: a path list restates "a maintainer must approve", which the approval count already says, while adding a hardcoded set of source paths that goes stale silently every time a file is renamed or split — and stale code-owner paths do not error, they simply stop routing, which is the worst failure mode a gate can have. A pathless `*` has no paths to go stale, so none of that argument reaches it.

What the pathless rule buys is the one thing the ruleset cannot express: **routing**. GitHub omits the pull request's author when auto-requesting review from code owners, so `* @vfarcic @prageethw` requests the other maintainer on every pull request, with nobody having to remember a flag. That is not a hypothetical convenience — on 2026-08-09, the day approvals were raised to 1, seven open pull requests from the owner had no reviewer requested at all, so the maintainer whose approval they needed had no signal they existed.

It stays a router rather than a second gate: `require_code_owner_review` remains `false`, so any maintainer's approval satisfies the single required review exactly as it did before the file existed. Three mechanics are worth knowing, and they share a failure mode: each one stops the routing without failing anything, so the only symptom is a pull request sitting with no reviewer. `CODEOWNERS` is read from the **base** branch, so a pull request that edits it does not benefit from its own change. A malformed entry disables routing entirely — check `gh api repos/vfarcic/dot-agent-deck/codeowners/errors` after editing it. And a **draft** pull request is not routed to code owners until it is marked ready for review, so `gh pr ready <n>` is what actually triggers the request on anything opened with `--draft`.

A consequence worth accepting deliberately: the approval requirement is not path-scoped, so a documentation typo needs a review round trip exactly like a protocol change does. Rulesets condition on ref names rather than file paths, so there is no clean way to exempt `docs/` without giving up the gate. One review on a typo is the cheaper half of that trade.

## How review is requested, and who merges

Review is requested through the pull request's **reviewer** field — not by mentioning someone in a comment. A comment notifies, but it does not put the pull request into the reviewer's *Review requested* queue, does not show up in their `gh pr status`, and is not what the approval rule counts.

```sh
gh pr create --reviewer prageethw ...       # at creation
gh pr edit 464 --add-reviewer prageethw     # afterwards
```

With `CODEOWNERS` in place this normally happens on its own. The commands are for what it misses: a pull request opened before the routing existed, an extra reviewer beyond the routed one, or a re-request after one was removed. They are also the manual route on a draft — a reviewer named with `--reviewer` is recorded on a draft, but code-owner routing does not run until `gh pr ready <n>`. Do **not** assign the other maintainer instead — an assignee is who is responsible for driving the pull request to done, which is normally the author.

Three rules about sequencing, each of which exists because of a specific ruleset setting:

- **Request review last, not first.** `dismiss_stale_reviews_on_push: true`, so a push carrying your own new content silently voids an approval. Settle CI, read Greptile's inline comments, push the fixes, resolve the threads — *then* ask for review. Requesting earlier buys a guaranteed second round trip. **Not every push dismisses, though, and this bullet said "any push" until 2026-09-07.** Measured that day: a `gh pr update-branch` merge left all eight of #924, #923, #909, #907, #902, #899, #894 and #886 `APPROVED` with zero `DISMISSED` reviews eleven hours after approval, while three pull requests whose conflicts were resolved by hand and pushed as ordinary commits (#872, #901, #919) were all dismissed. So bringing a stale approved pull request up to date **with `gh pr update-branch`** costs no re-approval — which is how the queue gets propagated past a `main`-side fix now that a fifth required check can block it. A **rebase** was not measured and is not covered; nor is there any claim that GitHub documents the distinction (unverified). Check with `gh pr view <n> --json reviewDecision,reviews` rather than predicting.
- **Resolve every review thread.** `required_review_thread_resolution: true` is what turns "read Greptile's inline comments" from a habit into something the merge button enforces — see [What is gated](#what-is-gated).
- **The author merges after approval, or arms auto-merge.** Nothing in the ruleset constrains who presses the button (`require_last_push_approval` is `false`), and the approving maintainer is under no obligation to. Reviewer-merges is a Prow/Kubernetes convention in which a *bot* merges on `/lgtm`; the GitHub-native equivalent is auto-merge, queued by the author. **Auto-merge was enabled on 2026-08-11** (`allow_auto_merge: true`), so an author may arm a pull request and let GitHub land it when the ruleset is satisfied. The objection previously recorded here — that arming it early lets the author "never return to the pull request" — does not survive the two rules above: auto-merge waits for the required approval, so a human still reads and judges, and `required_review_thread_resolution: true` means an armed pull request cannot merge while Greptile's threads sit unresolved. Someone must still clear them by hand. **That safety rests entirely on the ruleset being in force**, though, because `allow_auto_merge` is a repository setting the ruleset knows nothing about: lift the gate with an armed pull request outstanding and it lands unreviewed on the spot. [Emergency override](#emergency-override) therefore disarms before it deletes.

**Never merge your own unapproved pull request.** For the owner it will succeed — the admin bypass makes it silent rather than blocked — and that silence is precisely the decay this arrangement exists to prevent. An automated flow whose last step is a merge (`/prd-done`, `/prd-full`) may arm auto-merge and hand off, since that cannot land anything unapproved, but it must never merge directly.

## Why CI has to change first

Two workflows push straight to `main`:

- `.github/workflows/release.yml` — the changelog commit, in the `prepare` job
- `.github/workflows/docs-publish.yml` — the docs chart bump, in the `publish` job

Neither push carries check runs, and the default `GITHUB_TOKEN` is not an admin. Under a protected `main` both are rejected with `GH006: Protected branch update failed`, which kills the tag in `prepare` and breaks standalone `/publish-docs` runs. This is not hypothetical: it is precisely what happened to v0.35.6 when required status checks were briefly enabled, and it is why they stayed off for as long as they did (CLAUDE.md rule 8). Both pieces are now in place — the `RELEASE_TOKEN` admin identity below, plus the fail-fast guard that replaced its `|| github.token` fallback — and required checks went back on 2026-08-11.

The fix is a `RELEASE_TOKEN` secret holding a fine-grained PAT with **Contents: read and write** on this repository, owned by an account with admin access. Both workflows now pass it to `actions/checkout`:

```yaml
token: ${{ secrets.RELEASE_TOKEN }}
```

Each job also verifies the secret is non-empty in its **first** step, failing there with an actionable message. This replaced an earlier `token: ${{ secrets.RELEASE_TOKEN || github.token }}` fallback, whose purpose was to let the workflow change merge before any protection existed so the rollout steps could be sequenced independently. That purpose expired when protection went up on 2026-08-08: from then on the fallback could only downgrade an **unset or empty** secret to `github-actions[bot]`, which cannot push to `main`, deferring the failure to the push step — for `release.yml` after the tag is cut, for `docs-publish.yml` after the image is already in GHCR. Removing it was the prerequisite for step 6.

**What the guard covers, stated precisely, because it is easy to overstate.** It tests `-z` — unset or empty — and nothing more. A token that is present but **expired, revoked or under-scoped passes the guard and still fails later at `git push origin main`**, in exactly the half-done state described above. The guard converts one specific failure (nobody set the secret, or a caller did not forward it) from a late mystery into an immediate, named error; it is not a proof that the token works. Note also that the old fallback never helped in the expired case either: a GitHub `||` returns the left operand whenever it is non-empty, so an expired PAT was passed straight through to `actions/checkout` rather than being swapped for `github.token`. The two behaviours differ only for an unset or empty secret. Probing the token — say, an authenticated API call in the guard step — was considered and rejected: it buys coverage of a case that has never occurred here, at the price of an extra network dependency and a new way for the release path to fail. The cheap check plus [step 5's canary](#rollout) is the better trade; **the canary is what actually validates a token, so re-run it after every PAT rotation.**

**In place, and as of 2026-09-07 exercised too — this paragraph asserted "this specific combination has not been" for a month.** Both halves are verified. The bypass actors survived the full `PUT` that added the required checks — `gh api repos/vfarcic/dot-agent-deck/rulesets/20587589` still reports `actor_id: 5` (`RepositoryRole`, `bypass_mode: always`) and `actor_id: 2740` (`Integration`, `bypass_mode: pull_request`), which `scripts/apply-branch-protection.sh status` prints alongside the required contexts. And the PAT has pushed to a `main` protected by the `pull_request` rule. **The second leg is now exercised too, and this paragraph asserted otherwise for a month.** It said no direct push to `main` had happened since `required_status_checks` returned at 2026-08-11T15:00:51Z, v0.35.10 having been published the day before; that went false about five hours later. **Eleven releases** have shipped under the required checks — v0.36.0 (2026-08-11T20:20:18Z), v0.36.1, v0.36.2, v0.37.0, v0.37.1, v0.37.2, v0.38.0, v0.39.0, v0.39.1, v0.39.2, v0.39.3 (2026-09-04T13:56:14Z) — each pushing `release.yml`'s changelog commit and `docs-publish.yml`'s chart bump directly to `main`. That is **22** commits, and the property was verified on **all 22** rather than sampled: `gh api repos/{owner}/{repo}/commits/<sha>/pulls` returns empty for every one, so none came through a pull request, and all 22 name `github-actions[bot]` as author and committer — precisely the committer-is-not-pusher trap described above. That identity is not a bypass actor and cannot push to a protected `main`, so the pushes succeeded under the `RELEASE_TOKEN` PAT's admin bypass — the leg in question. So v0.35.6's failure mode has not recurred in eleven consecutive releases, and the honest status is *exercised*, not *untested*. **Run `scripts/apply-branch-protection.sh status` before the next tag anyway** and confirm the admin bypass is still listed: a full `PUT` is exactly what can drop a bypass actor silently. What has narrowed is the canary's role — it validates a **rotated token**, which is what it always actually covered, rather than probing an unexercised mechanism. `bypass_actors` is admin-only, so a `write` maintainer cannot check this and should ask rather than assume.

Note that `GITHUB_TOKEN` **cannot** be named as a ruleset bypass actor on a user-owned repository; the API rejects it with `422: Actor GitHub Actions integration must be part of the ruleset source or owner organization`. A PAT or a GitHub App is the only route.

## Rollout

The order matters. Each step is safe to stop at.

> **Where this stands: the rollout is complete.** The gate went up at `REQUIRED_APPROVALS=0` on 2026-08-08 and was raised to `1` on 2026-08-09 when [@prageethw](https://github.com/prageethw) joined as the second maintainer (issue #432). Step 6 — required status checks — landed on 2026-08-11 with `build`, `build-macos`, `build-windows` and `security`, alongside `allow_auto_merge: true`, and a **fifth** context, `e2e-deterministic`, was added on 2026-09-07 (see [step 6](#rollout)). The `write`-versus-admin asymmetry it raises was accepted deliberately rather than resolved: those checks are objective, so they are the same bar the owner would hold himself to. Judgment-bearing signals — `Greptile Review` above all — are deliberately left unrequired, because waiving a finding is a legitimate approval. The steps below are kept as the procedure for onboarding the *next* maintainer.

**1. Merge the plumbing.** The `token:` change, `scripts/apply-branch-protection.sh`, `MAINTAINERS.md`, and this page. Nothing is enforced yet and nothing changes behaviour.

**2. Create the PAT and set the secret.** A fine-grained PAT scoped to this repository with Contents: read and write, stored as `RELEASE_TOKEN`. Verify with `scripts/apply-branch-protection.sh status`, which reports whether the secret is visible.

**3. Onboard the second maintainer.** Grant `write` (or `maintain`) access and add them to `MAINTAINERS.md` in the same change. Do not skip ahead while there is only one collaborator, or apply step 4 with `REQUIRED_APPROVALS=0`. Raising approvals to 1 is also the moment the Renovate bypass starts mattering — see [Renovate and automerge](#renovate-and-automerge).

**4. Apply the ruleset.**

```bash
scripts/apply-branch-protection.sh apply
```

The script refuses to run if `RELEASE_TOKEN` is unset. It defaults to one approving review; `REQUIRED_APPROVALS=0` requires a pull request without requiring a review, which is the sensible setting if the gate goes up before a second maintainer does.

**5. Fire the canary immediately — do not wait for a real release.**

```bash
gh workflow run docs-publish.yml --repo vfarcic/dot-agent-deck
```

This is the step that actually validates the token, and it has to come *after* step 4. A push to an unprotected `main` proves only that the PAT can write; it says nothing about whether the PAT can **bypass a ruleset**, because with no ruleset there is nothing to bypass. The two are separate mechanisms: writing is a token permission, bypassing is evaluated against the actor's role in `bypass_actors`. A fine-grained PAT carries its own permission model alongside the role, so this is exactly the combination where a surprise is plausible.

`docs-publish` is the right canary because it pushes to `main` the same way the release flow does, is `workflow_dispatch`-able on demand, and costs nothing if it fails. Discovering a bypass problem here costs a re-run; discovering it during a release burns a version tag, which is how v0.35.6 died.

If the canary comes back `GH006`, the token cannot bypass. Fall back to a classic PAT (unambiguous, but `repo` scope reaches every repository the account can see) or move to the GitHub App variant below, which is both narrowly scoped and unambiguously a bypass actor.

**6. Add required status checks.** Done on 2026-08-11 by adding a `required_status_checks` entry to the ruleset's `rules` array with `build`, `build-macos`, `build-windows` and `security`, and `strict_required_status_checks_policy: false` so pull requests are not forced up to date with `main` (with a dozen open, `true` means near-continuous rebasing for no correctness gain). Update the ruleset with a full `PUT` of every rule and bypass actor, not a partial payload — omitting `pull_request` or a bypass actor deletes it. `scripts/apply-branch-protection.sh` emits the whole ruleset, including this rule, so `apply` reconstructs it rather than reverting to a weaker shape; the contexts are overridable via `REQUIRED_CHECKS`. Emitting the ruleset *without* the rule takes two variables rather than one — `REQUIRED_CHECKS=` **and** `ALLOW_NO_REQUIRED_CHECKS=true` — because on a full `PUT` "omit the rule" means "strip every required check off the repository", silently and with a success exit. An empty (or whitespace-only) `REQUIRED_CHECKS` on its own is a hard error that names the alternatives; see [Forks](#forks) for the case the escape hatch exists to serve.

**A fifth context, `e2e-deterministic`, was added on 2026-09-07 — to the live ruleset directly, and the script caught up later the same day.** `gh api repos/vfarcic/dot-agent-deck/rulesets/20587589` now returns five contexts on the active `main-protected`, `updated_at` 2026-09-07T15:30:16Z, and the lane genuinely blocks — measured on #924 while it was open: `APPROVED`, `MERGEABLE`, no review threads to resolve and four green checks, yet `mergeStateStatus: BLOCKED` on `e2e-deterministic` alone. The rationale for holding it back had expired — the job's `timeout-minutes: 60` was derived rather than measured because nothing had ever run the tier on a runner, and across the last 40 `pull_request` runs of `ci.yml` (measured 2026-09-07) 35 completed `e2e-deterministic` jobs took 9.0–42.0 minutes, median 12.6, with none reaching the kill window. **The matching change to `REQUIRED_CHECKS` has since landed, and the two are in step as of 2026-09-10:** `scripts/apply-branch-protection.sh`'s `REQUIRED_CHECKS` default on `main` is `build build-macos build-windows security e2e-deterministic`, the same five the live ruleset returns, so `apply` restores the fifth context rather than stripping it. PR #909 — which added the fifth and is the tracking change for the stricter fix-or-quarantine rule — merged on 2026-09-07 in `0c35e537`, the first commit to touch that script since `b55d8118` (2026-08-18). Read the contexts back with `status` after any `apply` regardless: the window between the two changes is exactly the drift the read-back exists to catch, it lasted about six hours, and nothing prevents another. That is the second reason (after [Forks](#forks)) to distrust a bare `apply`. It also carries a cost that was argued *for* waiting: #909 measured 16 of 21 open PRs red on 2026-09-05, `orchestration_remit_001` alone on 11, and requiring the lane makes every un-rebased PR unmergeable at once. Mitigating that turns out to be cheap — a `gh pr update-branch` merge does **not** dismiss an approval (measured on eight approved PRs on 2026-09-07), so the queue can be brought past a `main`-side fix without buying a round of re-approvals.

Four things step 6 surfaced, all worth knowing before doing it again elsewhere:

- **A required check that never reported blocks the pull request forever.** #416, an outside contributor's fork whose workflows had never run, went `mergeable=UNKNOWN` the moment the checks went up, with no path forward until its runs existed; it has since merged. Check for fork pull requests with no check history *before* adding required checks, not after — and note that **each additional context widens this trap**, which is worth weighing before requiring a sixth.
- **A check *skipped by a job conditional* is not that trap — it counts as passing.** The distinction matters here because `ci.yml`'s `changes` gate skips all five required jobs for devbox-only and flake-only pull requests, which looks like the #416 hazard reachable by anyone and is not. (`e2e-deterministic` carries the same `needs: changes` and `if:` pair as the other four, so becoming required did not take it out of the skip path — grep the job name in `ci.yml` rather than trusting a line number. This parenthetical used to carry one, and it was wrong at two consecutive heads: it pointed into the `build` job while `e2e-deterministic` sat 46 lines further down, and then merging #872 and #958 grew the file by 148 lines and moved the job again. A `file:line` citation into a file this active is a claim that rots on somebody else's merge, which is the defect class this page is itself about.) GitHub is explicit: *"A job that is skipped will report its status as 'Success'. It will not prevent a pull request from merging, even if it is a required check"* ([Actions docs](https://docs.github.com/actions/using-jobs/using-conditions-to-control-job-execution)). The two cases differ in whether a check run exists at all — a conditionally-skipped job produces one with conclusion `skipped`, whereas a workflow that never ran produces nothing to evaluate. A **second**, independent reason the skip path cannot strand a human here: that gate is scoped to `renovate[bot]` by author, so a human pull request touching only `devbox.json`, only Markdown or only `prds/` gets the full matrix regardless of paths. Measured on #504 (workflows plus docs), #499 (one skill file) and #469/#464 (one PRD file each) — all four ran `build`, `build-macos`, `build-windows` and `security` rather than skipping them. Keep both properties in mind if the `changes` gate is ever widened beyond Renovate: the author scope is a *defence in depth* here, not the load-bearing part.
- **A red required check on `main` now blocks every open pull request at once.** That is the gate working as intended, but it changes the cost of merging a break: before, `ci.yml`'s post-merge run on `main` turned a broken `main` into a *notification* while pull requests kept merging; now the same break stops all of them, because every pull request's merge ref inherits it. Demonstrated the day the checks went up — `xtask-linkage-check`'s `clean_tmp::tests::explicit_roots_replace_the_standard_set` fails on Windows on `main` (from #322, merged as #472), so `build-windows` went red simultaneously on #504, #469 and #464, none of which contain a line of Rust; the fix is #512. So when several unrelated pull requests go red on the same required job, suspect `main` before suspecting the pull requests, and treat a `main`-fixing pull request as the one that unblocks the queue.
- **Renovate's `bypass_mode: pull_request` does bypass required checks — confirmed, and it is worth stating as a security property rather than a note about automerge.** PR #510 (`cargo-nextest` v0.9.143, touching only `devbox.json` and `devbox.lock`) was authored *and* merged by `app/renovate` at 2026-08-11T16:21:23Z, 81 minutes after the checks went up, with `reviewDecision: REVIEW_REQUIRED` and all four required checks reporting `SKIPPED`. So the bypass covers both rules, and **`main` has a lane on which a change lands with zero human approvals** — the mechanism [Renovate and automerge](#renovate-and-automerge) already described as fact, which is why this bullet reads as a confirmation rather than a discovery. That is **accepted by design**: the compensating control is `ci.yml`'s `push:` trigger, which re-verifies `main` after every merge, so a bad bump becomes a notification instead of a discovery by the next unlucky pull request. What the skipped checks add is that on the devbox-only and flake-only paths, that post-merge run is the *first* time any Rust job sees the change at all. Set `RENOVATE_BYPASS=false` in the script to close the lane and review every dependency bump by hand.

## Renovate and automerge

`renovate.json` automerges five groups on green CI: Rust patch crates, Rust minors on crates already at 1.0, devbox packages, GitHub Actions, and the docs-site npm dependencies. Renovate merges these itself — PR #426 was merged by `renovate[bot]`, not by a human — so the ruleset applies to it like any other actor.

**Renovate is a GitHub App, not a collaborator.** The `RepositoryRole: admin` bypass does not cover it; apps are a separate `actor_type` (`Integration`). That distinction is the whole hazard:

- At `REQUIRED_APPROVALS=0` nothing breaks. A pull request is required, Renovate opens one anyway, and no approval is needed.
- At `REQUIRED_APPROVALS=1` every automerge group **stalls silently**. A bot cannot approve its own pull request, and GitHub counts approvals only from write/admin accounts. Nothing errors and nothing is logged — the pull requests simply accumulate, which is a slow and confusing way to discover the cause.

The script therefore adds Renovate (app id 2740, from `gh api /apps/renovate --jq .id`) as a bypass actor by default, in `pull_request` mode rather than `always`: it may merge a pull request that lacks the required approvals, but still cannot push directly to `main`. That is strictly narrower than the admin bypass.

It is enabled by default deliberately. While approvals are 0 the entry is inert, so turning it on early costs nothing — and the alternative is remembering this at the exact moment a second maintainer is onboarded, which is when attention is elsewhere. Set `RENOVATE_BYPASS=false` to leave it out and review every dependency bump by hand.

One consequence to accept: with the bypass in place, CI gating on dependency pull requests rests on Renovate's own configuration (it waits for branch status before merging), not on the ruleset. Adding `required_status_checks` in step 6 did not change that, because a bypass actor bypasses those too — **confirmed on 2026-08-11 by #510**, authored and merged by `app/renovate` with `reviewDecision: REVIEW_REQUIRED` and all four required checks reporting `SKIPPED`. So this lane merges to `main` with no human approval and, on the devbox-only and flake-only paths, with no Rust job having run at all; the post-merge `ci.yml` run on `main` is the whole net. Both halves are deliberate, but they are the reason `renovate.json`'s automerge groups deserve the same scrutiny as the ruleset itself — see [step 6](#rollout) for the measurement, and `RENOVATE_BYPASS=false` to close the lane.

The `required_review_thread_resolution` rule is not a problem here in practice — Greptile posts no review and no inline comments on Renovate pull requests (verified on #426, #389 and #384). It would become one if that ever changed.

## What is gated

Everything that lands on `main`, uniformly: one approving review from a maintainer, all review threads resolved, no deletion, no force-push. There is no path scoping — see [Who counts as a maintainer](#who-counts-as-a-maintainer) for why, and for the round-trip-on-a-typo cost that comes with it.

The requirement that review threads resolve before merge is doing specific work. Greptile reviews every pull request and re-reviews on each push, and its actual findings live in the inline comments rather than in the check-run or the summary — a green check has accompanied real P1 defects here before (CLAUDE.md rule 8). Thread resolution is what turns "read the inline comments" from a habit into something the merge button enforces.

## Making the gate bind the owner too

If the honour-system caveat above is unacceptable, replace the admin bypass with a GitHub App:

1. Create a GitHub App owned by the repository owner, with **Contents: read and write**, and install it on this repository.
2. Have CI mint an installation token (for example with `actions/create-github-app-token`) and pass that to `actions/checkout` instead of `RELEASE_TOKEN`.
3. Re-run the script with `ADMIN_BYPASS_MODE` removed from the payload and the App added as the sole `bypass_actors` entry (`actor_type: "Integration"`).

The owner can still override at any time by editing or deleting the ruleset — `scripts/apply-branch-protection.sh delete` — but the override becomes a deliberate, audit-logged act rather than an invisible one. That friction is the entire point.

## Emergency override

Two switches guard `main` and they come back on by different routes, so the order matters. `allow_auto_merge` is a **repository setting**, not part of the ruleset: deleting the ruleset does not touch it, and any pull request with auto-merge already armed becomes immediately mergeable the instant the gate is gone — landing unreviewed, which is the exact outcome [the argument for arming early](#how-review-is-requested-and-who-merges) says cannot happen. That argument holds only while the ruleset exists.

Disarm first, then lift:

```bash
gh pr list --state open --json number,autoMergeRequest \
  --jq '.[] | select(.autoMergeRequest != null) | .number'   # what is armed
gh pr merge <n> --disable-auto                               # disarm each one
gh repo edit vfarcic/dot-agent-deck --enable-auto-merge=false # stop new arming
scripts/apply-branch-protection.sh delete                    # remove the gate
```

Restore in the reverse order, gate first, so there is no window in which auto-merge is available on an unprotected `main`:

```bash
scripts/apply-branch-protection.sh apply                     # put the gate back
scripts/apply-branch-protection.sh status                    # confirm all four rules and both bypass actors
gh repo edit vfarcic/dot-agent-deck --enable-auto-merge=true  # re-enable arming
```

`apply` sends a full `PUT`, so it restores exactly what `payload()` emits and deletes anything else — that is why the `status` line is not optional. Read it and confirm `required_status_checks` lists `build`, `build-macos`, `build-windows` and `security`, and that the `RepositoryRole id=5` bypass is present; a ruleset that comes back without the checks looks identical from the pull request page. Anything configured by hand in the GitHub UI and not represented in the script is lost here, so add it to the script rather than to the UI.

All of these are recorded in the repository audit log.

## Forks

The `RELEASE_TOKEN` guard has a consequence downstream: **a fork with no `RELEASE_TOKEN` secret cannot release.** Its `release.yml` now fails at step 1 of `prepare` rather than at the changelog push. Before the guard, a fork's release worked *because* of the `|| github.token` fallback — an unprotected fork `main` accepts `github-actions[bot]`, so the fallback was not a degradation there at all.

This is not an argument for keeping the fallback: silently swapping identities is precisely what made the upstream failures hard to read. But the breakage should be expected rather than diagnosed from scratch, and it is not hypothetical — `prageethw/dot-agent-deck` shipped five releases in the six days to 2026-08-11 (v0.35.8 through v0.37.1, verifiable from its releases API) and, per @prageethw's review of #504, holds only `SONAR_TOKEN`. Its next tag after syncing this change therefore dies immediately. A fork's secret list is **not** readable from upstream (`gh secret list --repo <fork>` returns `HTTP 403`), so this is one for each fork operator to check on their own side rather than something upstream can audit.

Two ways to fix a fork, both one-time:

- **Set the secret** (recommended, and what upstream expects): create a PAT with **Contents: read and write** on the fork and store it as `RELEASE_TOKEN`. On an unprotected fork `main` it needs no admin rights and no bypass — the token only has to be able to push. This also keeps the fork's workflows diff-free against upstream.
- **Or re-add the fallback locally** as a one-line fork-side patch, accepting that it will conflict on every sync of these files.

The same applies to `scripts/apply-branch-protection.sh` if a fork ever runs it: `REQUIRED_CHECKS` names this repository's own `ci.yml` job ids — all five of them as of `main`, matching the live ruleset (see [step 6](#rollout)) — and requiring a context the fork's CI never produces leaves every pull request unmergeable with nothing red to fix — the #416 trap, self-inflicted. Set `REQUIRED_CHECKS` to the fork's own job names, or, for the pull-request gate with no required checks at all, say so with both variables:

```bash
REQUIRED_CHECKS= ALLOW_NO_REQUIRED_CHECKS=true scripts/apply-branch-protection.sh apply
```

**An empty `REQUIRED_CHECKS` by itself is refused, and that refusal is not pedantry.** `apply` sends a full `PUT`, so on a repository that *does* have required checks those same two keystrokes remove all five — no error, no warning, success exit, and a pull request page that looks exactly as it did before. Arriving there by accident is far more ordinary than being a fork: a CI step with an unset variable, a sourced env file, a mistyped export. Requiring a second, plainly named variable is what makes the weakening a decision rather than a typo. The flag is inert whenever `REQUIRED_CHECKS` is non-empty — it permits an omission, it never causes one.
