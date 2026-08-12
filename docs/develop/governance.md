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

- **Request review last, not first.** `dismiss_stale_reviews_on_push: true`, so any push after an approval silently voids it. Settle CI, read Greptile's inline comments, push the fixes, resolve the threads — *then* ask for review. Requesting earlier buys a guaranteed second round trip.
- **Resolve every review thread.** `required_review_thread_resolution: true` is what turns "read Greptile's inline comments" from a habit into something the merge button enforces — see [What is gated](#what-is-gated).
- **The author merges after approval.** Nothing in the ruleset constrains who presses the button (`require_last_push_approval` is `false`), and the approving maintainer is under no obligation to. Reviewer-merges is a Prow/Kubernetes convention in which a *bot* merges on `/lgtm`; the GitHub-native equivalent is auto-merge, queued by the author. Auto-merge is currently disabled on this repository; enabling it would let the author queue the merge before review lands and never return to the pull request.

**Never merge your own unapproved pull request.** For the owner it will succeed — the admin bypass makes it silent rather than blocked — and that silence is precisely the decay this arrangement exists to prevent. Any automated flow whose last step is a merge (`/prd-done`, `/prd-full`) must stop at "green, reviewed, approved" and hand off.

## Why CI has to change first

Two workflows push straight to `main`:

- `.github/workflows/release.yml` — the changelog commit, in the `prepare` job
- `.github/workflows/docs-publish.yml` — the docs chart bump, in the `publish` job

Neither push carries check runs, and the default `GITHUB_TOKEN` is not an admin. Under a protected `main` both are rejected with `GH006: Protected branch update failed`, which kills the tag in `prepare` and breaks standalone `/publish-docs` runs. This is not hypothetical: it is precisely what happened to v0.35.6 when required status checks were briefly enabled, and it is why `main` carries no protection at all today (CLAUDE.md rule 8).

The fix is a `RELEASE_TOKEN` secret holding a fine-grained PAT with **Contents: read and write** on this repository, owned by an account with admin access. Both workflows now pass it to `actions/checkout`:

```yaml
token: ${{ secrets.RELEASE_TOKEN || github.token }}
```

The `|| github.token` fallback means the workflows behave exactly as they do today while the secret is unset. That is deliberate — it makes the workflow change safe to merge before any protection exists, so the two steps can be sequenced independently.

Note that `GITHUB_TOKEN` **cannot** be named as a ruleset bypass actor on a user-owned repository; the API rejects it with `422: Actor GitHub Actions integration must be part of the ruleset source or owner organization`. A PAT or a GitHub App is the only route.

## Rollout

The order matters. Each step is safe to stop at.

> **Where this stands:** steps 1–5 are done. The gate went up at `REQUIRED_APPROVALS=0` on 2026-08-08, and was raised to `1` on 2026-08-09 when [@prageethw](https://github.com/prageethw) joined as the second maintainer (issue #432). Step 6 — required status checks — is still open, and is now a sharper decision than it looks: see CLAUDE.md rule 8 for why a required check binds a `write` maintainer but not the admin owner. The steps below are kept as the procedure for onboarding the *next* maintainer.

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

**6. Reconsider required status checks.** They were removed for the same `GH006` reason and can come back once the canary has proven the token bypasses. Add them to the ruleset's `rules` array as a `required_status_checks` entry.

## Renovate and automerge

`renovate.json` automerges five groups on green CI: Rust patch crates, Rust minors on crates already at 1.0, devbox packages, GitHub Actions, and the docs-site npm dependencies. Renovate merges these itself — PR #426 was merged by `renovate[bot]`, not by a human — so the ruleset applies to it like any other actor.

**Renovate is a GitHub App, not a collaborator.** The `RepositoryRole: admin` bypass does not cover it; apps are a separate `actor_type` (`Integration`). That distinction is the whole hazard:

- At `REQUIRED_APPROVALS=0` nothing breaks. A pull request is required, Renovate opens one anyway, and no approval is needed.
- At `REQUIRED_APPROVALS=1` every automerge group **stalls silently**. A bot cannot approve its own pull request, and GitHub counts approvals only from write/admin accounts. Nothing errors and nothing is logged — the pull requests simply accumulate, which is a slow and confusing way to discover the cause.

The script therefore adds Renovate (app id 2740, from `gh api /apps/renovate --jq .id`) as a bypass actor by default, in `pull_request` mode rather than `always`: it may merge a pull request that lacks the required approvals, but still cannot push directly to `main`. That is strictly narrower than the admin bypass.

It is enabled by default deliberately. While approvals are 0 the entry is inert, so turning it on early costs nothing — and the alternative is remembering this at the exact moment a second maintainer is onboarded, which is when attention is elsewhere. Set `RENOVATE_BYPASS=false` to leave it out and review every dependency bump by hand.

One consequence to accept: with the bypass in place, CI gating on dependency pull requests rests on Renovate's own configuration (it waits for branch status before merging), not on the ruleset. Adding `required_status_checks` in step 6 does not change that, because a bypass actor bypasses those too.

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

```bash
scripts/apply-branch-protection.sh delete   # remove the gate
scripts/apply-branch-protection.sh apply    # put it back
```

Both are recorded in the repository audit log.
