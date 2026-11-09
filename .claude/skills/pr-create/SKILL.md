---
name: pr-create
description: Take committed work from a branch to a verified pull request — push, open the PR, settle CI and the automated review, answer and resolve every finding, and hand off. Never merges and never arms auto-merge. Use when work on a branch is finished and needs to become a reviewed PR, whether or not a PRD started it.
user-invocable: true
---

# Create a pull request and drive it to verified

Owns one arc: **committed work on a branch → a PR that is green, reviewed, and ready for someone to merge.**

Out of scope on purpose:

- **Merging.** Nothing here merges or arms auto-merge; it hands off. CLAUDE.md rule 8: nobody merges their own unapproved PR (for an admin that succeeds *silently* rather than failing, which is how the gate decays into ceremony), and agents never arm auto-merge. An approved PR is merged through [`merge-pr`](../merge-pr/SKILL.md) by an agent whose task says to merge, or by a person.
- **Closing the issue.** `Closes #<n>` in the PR body does it on merge.
- **Implementation.** If a finding needs a code change, make it; if it needs a decision you cannot make, say so on the thread.

This file is **project-local and owned by this repository** — it was forked out of the `dot-ai` mirror precisely so corrections survive (CLAUDE.md rule 13). Edit it freely.

## 1. Before the PR

- CLAUDE.md is the authority on the gates (rules 2 and 5). **Run `cargo xtask affected-checks` and run what it prints** (issue #1575): for a branch with any Rust, build input or unmapped path in it, that is rule 2's fmt and clippy commands and rule 5's `cargo test-fast`; for a branch that is only mapped text (docs, skills, `changelog.d/`, `.github/`, PRDs, `CLAUDE.md` and the like), it is the xtask tests plus the root-package tests that read those files. `cargo xtask affected-checks --run` prints the plan and runs it, stopping at the first failure — prefer it to piping the printed plan into a shell, which reports success when the helper itself fails to build or run. Either way add the tests covering what you touched — **name those in the PR body**, since part of the tier runs on no runner anywhere. Read the rules rather than trusting this summary.
- **A red you met along the way is yours** (CLAUDE.md rule 6), whoever caused it and even if it passed on a retry: after CLAUDE.md rule 6's isolation rerun, fix it in this PR or quarantine it (a named owner, an expiry issue, and `#[ignore = "quarantined: <owner>, #<issue>"]` on the test), and **say in the PR body which, for each one**. Rerunning it until green and mentioning it is neither; `.claude/skills/verify-pr/SKILL.md` Phase 5 has the mechanics. Before fixing a red your change did not cause, check whether an open PR already fixes it (`gh pr list --search '<test name>'`); if one does, name that PR in the body and leave the red to it — #1460 and #1462 both changed `selectRow` in `desktop/driver/harness.ts` to fix `terminal_002`.
- **Changelog fragment**: `changelog.d/<issue>.<type>.md`, type one of `breaking|feature|bugfix|doc|misc`. Release notes are built from these, not from PR labels.
- **User docs** (CLAUDE.md rule 21): a user page under `docs/` the PR touches, and the changelog fragment, say what the user does, sees and configures; implementation detail and issue history go to `docs/develop/` or nowhere, fixed in this PR.
- **CLAUDE.md rule 23** if the change adds a desktop control: say in the PR body how it is reached by voice — the `commands.toml` row, or the `no_voice` reason — and name any entry it adds to `xtask/linkage-check/voice-control-sites.toml`, `Gap:` entries above all. Linkage-check rule 14's ratchet makes the question unavoidable but cannot judge the answer, so the body is where a reviewer reads it.
- **CLAUDE.md rule 12** if the change touches the daemon, protocol, orchestration or hooks: answer the `PROTOCOL_VERSION`-vs-`.breaking.md` question explicitly in the PR body, including the cross-version manual test. The `cross-version` CI job runs that test for any PR that changes the binary (`docs/develop/cross-version-harness.md`, "In CI"): once it passes on the head, link its run in the PR body as the record. When it reports a `PROTOCOL_VERSION` move instead, record that refusal as the outcome. Run `cargo xver` yourself only to diagnose a red job, or for a pairing the job does not set up. A `.breaking.md` fragment also needs its `CONTRACT_BREAKS` entry in `src/daemon_protocol.rs` (issue #801) — `xtask/linkage-check` fails the build without it.
- Working tree clean, branch pushed. Never push to `main` — it is protected and returns `GH013`.

## 2. Open it

`gh pr create`, with a body that says what changed and why, how it was verified (name the tests), and `Closes #<n>` — or `Refs #<n>` for a PR that ships only part of a PRD, which leaves the PRD open (the orchestrator template's step 5 in `.dot-agent-deck.toml` has the rule).

Then `gh pr edit <n> --add-reviewer <the other maintainer>` — but **request review last**, after CI and the automated review have settled and you have pushed the fixes. `dismiss_stale_reviews_on_push` voids an approval when you push new commits (a `gh pr update-branch` merge was measured not to), so asking early buys a guaranteed second round trip.

## 3. Settle CI and the automated review

Wait for CI with `gh pr checks <n>`, and for the automated review apps with:

```sh
.claude/skills/pr-create/wait-reviews.sh <n>
```

It waits for every app listed in `reviewers.json` beside it to finish reviewing the PR's **current head**, prints `DONE=` or `TIMED_OUT=` per app, and ends with `REVIEWS=complete` or `REVIEWS=incomplete`. The apps signal "done" differently, which is why the skills do not describe the signals themselves: Qodo creates no check-run, and its review comment names the commit it reviewed; Greptile reviews only when the PR opens, and posts its summary comment when it finishes. Adding, dropping or replacing a reviewer is an edit to `reviewers.json`, not to this file (issue #1256; `docs/develop/governance.md`, "The automated reviewer", has the measurements).

**The wait is bounded, per app.** An app that is out of quota, uninstalled, or broken produces **nothing at all** — no comment, no check-run, no failed state — so "wait until it appears" never terminates. Measured on this repo 2026-08-23: an exhausted Greptile quota produced zero comments *and* zero check-runs, indistinguishable from the app being gone. The script stops at each app's `timeout_minutes`: for an app that reviews every push, counted from when the script starts and again whenever the head moves; for one that reviews only when the PR opens, counted from the PR's creation, so a push never buys it another wait. On `REVIEWS=incomplete`, **proceed and say explicitly in your report which review was not obtained** — that is a result. `ERROR=` (exit 2) means the script could not read the registry or the PR, or could not confirm the head before deciding: treat it as no review obtained, and report the error. Do not hang, and do not report the gate as passed.

**A check that goes red here falls under the same rule as one met locally** (step 1): a test that fails in CI and then passes on `gh run rerun` is a flaky test you have now met, so it gets a fix or a quarantine in this PR, not just the green rerun. The PR body was written before this wait, so add the red and its exit to it (`gh pr edit <n> --body-file <file>`) once you have taken one — the body, not only your report, is what step 1 asks to carry it.

**A green check-run is not the review.** The findings live only in the inline comments:

```sh
gh api repos/{owner}/{repo}/pulls/<n>/comments --paginate
```

Keep `--paginate` — replies count toward the page, so a busy PR silently truncates the findings you are about to certify as read. The summary comment and the review object carry none of them, and a `COMMENTED` review with a passing check can still carry real defects.

## 4. Answer and resolve every finding

For each one: fix it, or reply saying why not. Then **resolve the thread.**

Resolving is not bookkeeping — it is half the job, and skipping it blocks the merge twice over:

- `required_review_thread_resolution` is on, so an unresolved thread blocks the **merge button**;
- the agent PR reviewer skips any PR carrying one, so it also blocks the **approval** that merge needs.

Measured: #1035 sat a full day with 14 green checks and every finding already fixed, and #1019 sat two days on a finding the reviewer itself had retracted 34 seconds after the author rebutted it.

```sh
# unresolved thread ids — PAGINATED. `first:100` is a page, not the total, and
# resolved threads stay in the connection, so a second unpaginated call returns
# the same first 100 and silently hides the rest.
gh api graphql --paginate \
  -f query='query($o:String!,$r:String!,$n:Int!,$endCursor:String){repository(owner:$o,name:$r){
    pullRequest(number:$n){reviewThreads(first:100,after:$endCursor){
      pageInfo{hasNextPage endCursor} nodes{id isResolved path}}}}}' \
  -f o=<owner> -f r=<repo> -F n=<pr> \
  --jq '.data.repository.pullRequest.reviewThreads.nodes[] | select(.isResolved==false) | .id'

gh api graphql -f query='mutation($t:ID!){resolveReviewThread(input:{threadId:$t}){thread{isResolved}}}' -f t=<id>
```

Re-run the query after resolving and confirm it returns nothing. "I resolved the ones I saw" is not the same claim as "none is unresolved".

Resolve only what you actually addressed. Leave a thread open when you are waiting on the commenter, or when they asked something they still need answered there — a thread closed over unaddressed feedback is worse than one left open.

Push fixes **before** requesting review (step 2). Greptile does not re-review — `greptile.json` sets `triggerOnUpdates: false` — but Qodo re-reviews on **every push** (`.pr_agent.toml`), a merge-only one included. So **after your last push, run `wait-reviews.sh <n>` again** and repeat this step for anything new it raised; a finding raised on your final push and left unanswered is still an unresolved thread blocking the merge. The script judges the new head, so a Qodo review of an earlier push does not count. Its **medium and above** findings arrive at the same endpoint as Greptile's and are answered and resolved the same way; its **informational** tier exists only in its "Code Review by Qodo" issue comment (`inline_comments_severity_threshold = 2`), so read that comment as well as the inline endpoint.

## 5. Hand off

Report and stop: PR URL, check status, each finding and what you did about it, each red you met and whether it was fixed or quarantined, and whether an automated review was obtained at all.

**Do not arm auto-merge** (CLAUDE.md rule 8). GitHub would merge the PR the moment the approval lands, with nobody present to run `/merge-pr`'s overlap check first ("The overlap check before a merge", `.claude/skills/merge-pr/SKILL.md`) or to watch `main`'s CI run for the merge commit and fix a red. When your task says to take the PR to merged, do it through `/merge-pr` once it is approved; otherwise stop at the PR.

## Reference

- `docs/develop/governance.md` — the ruleset, bypass actors, who may merge, the emergency override.
- CLAUDE.md rules 2, 5, 6, 8, 12, 13.
