---
name: pr-create
description: Take committed work from a branch to a verified pull request — push, open the PR, settle CI and the automated review, answer and resolve every finding, and hand off. Never merges; arms auto-merge only when nothing downstream gates the merge. Use when work on a branch is finished and needs to become a reviewed PR, whether or not a PRD started it.
user-invocable: true
---

# Create a pull request and drive it to verified

Owns one arc: **committed work on a branch → a PR that is green, reviewed, and ready for someone to merge.**

Out of scope on purpose:

- **Merging.** Nothing here merges. Arm auto-merge and hand off — CLAUDE.md rule 8: nobody merges their own unapproved PR, and for an admin that succeeds *silently* rather than failing, which is how the gate decays into ceremony.
- **Closing the issue.** `Closes #<n>` in the PR body does it on merge.
- **Implementation.** If a finding needs a code change, make it; if it needs a decision you cannot make, say so on the thread.

This file is **project-local and owned by this repository** — it was forked out of the `dot-ai` mirror precisely so corrections survive (CLAUDE.md rule 13). Edit it freely.

## 1. Before the PR

- CLAUDE.md is the authority on the gates. As of writing: `cargo fmt --check` and `cargo clippy --workspace --all-targets --features e2e,e2e-live -- -D warnings` before every commit, `cargo test-fast` per task, plus the tests covering what you touched — **name those in the PR body**, since part of the tier runs on no runner anywhere. Read the rules rather than trusting this list.
- **Changelog fragment**: `changelog.d/<issue>.<type>.md`, type one of `breaking|feature|bugfix|doc|misc`. Release notes are built from these, not from PR labels.
- **Rule 12** if the change touches the daemon, protocol, orchestration or hooks: answer the `PROTOCOL_VERSION`-vs-`.breaking.md` question explicitly in the PR body, including the cross-version manual test.
- Working tree clean, branch pushed. Never push to `main` — it is protected and returns `GH013`.

## 2. Open it

`gh pr create`, with a body that says what changed and why, how it was verified (name the tests), and `Closes #<n>`.

Then `gh pr edit <n> --add-reviewer <the other maintainer>` — but **request review last**, after CI and the automated review have settled and you have pushed the fixes. `dismiss_stale_reviews_on_push` voids an approval on any later push, so asking early buys a guaranteed second round trip.

## 3. Settle CI and the automated review

Wait for the check-runs. `gh pr checks <n>` reports both CI and the reviewer's own check-run.

**The wait must be bounded.** An automated reviewer that is out of quota, uninstalled, or broken produces **no check-run at all** — there is no message and no failed state, so "wait until it appears" never terminates. Measured on this repo 2026-08-23: an exhausted Greptile quota produced zero comments *and* zero check-runs, indistinguishable from the app being gone.

So: give the reviewer a budget (~15 minutes from PR creation is ample; it normally lands in 3–5). If no reviewer check-run exists when the budget expires, **proceed and say so explicitly in your report** — "no automated review was obtained" is a result. Do not hang, and do not report the gate as passed. Never block on a reviewer that is not configured here at all.

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

Push fixes **before** requesting review (step 2). Do not wait for a re-review: `greptile.json` sets `triggerOnUpdates: false`, so the reviewer runs once at open and never again.

## 5. Hand off

Report and stop: PR URL, check status, each finding and what you did about it, and whether an automated review was obtained at all.

**Arm auto-merge only when nothing downstream gates the merge.** `gh pr merge <n> --auto --squash` waits for exactly what a manual merge needs — the approval, the required checks, every thread resolved — so against a bare hand-off it is not a bypass.

It *is* a bypass when your caller has a gate of its own after this step. The orchestrated PRD workflow is exactly that shape: `.dot-agent-deck.toml` runs this skill at step 5, then builds a demo reel at step 6, then asks the **user** for an explicit merge go-ahead at step 7. Arming here would land the PR the moment the approval arrived — during step 6, with nobody present — and nothing later can un-arm it, because re-running this skill only arms it again.

So the default is **do not arm**; arm only when you are the last gate. If you were told to stop before merge, that is your answer: report and leave it disarmed.

## Reference

- `docs/develop/governance.md` — the ruleset, bypass actors, who may merge, the emergency override.
- CLAUDE.md rules 2, 5, 8, 12, 13.
