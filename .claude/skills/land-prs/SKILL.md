---
name: land-prs
description: Land every open PR the user has not excluded — resolve merge conflicts, get stale approvals re-cast by the agent reviewer, and squash-merge each PR once it is approved and green — then report exactly what is left and why. Use when asked to merge the open PRs, land what is ready, clear the PR backlog before a release, or as the first step of "make a release" (tag-release calls it). It never approves anything, and merges an unapproved PR only in the one admin case Step 6 describes, on the user's explicit instruction.
user-invocable: true
---

# Land the open PRs

The goal is **every open PR merged, except the ones the user excludes**. A PR that cannot merge yet is a problem to fix, not a row to skip: a conflict gets resolved, a dismissed approval gets re-requested, a pending check gets waited for. What this skill does not do is decide on the user's behalf — the stops in Step 6 go back to them.

This skill is **project-local and owned here** (CLAUDE.md rule 13). Edit it when a new scenario turns up; that is expected.

## What this skill never does

- **Approve a PR, or merge one that is not approved.** CLAUDE.md rule 8: nobody approves their own PR, and for an admin an unapproved merge succeeds silently. The one exception is Step 6's stale change request, and only on the user's explicit instruction; no other `--admin`.
- **Force-push.** Conflicts are resolved with a merge commit on top of the PR branch.
- **Rewrite a PR's substance.** Conflict resolution and mechanical upkeep only — including the narrow adaptations Step 4 names. A finding that needs a code change goes to a unit (Step 6).
- **Merge a PR the user excluded, or one whose unit is still working.** Ask which are still in progress if you do not know; a PR that is `APPROVED` can still have an agent pushing to it.

## Step 0 — Read the rules back, do not trust this file

```bash
gh api repos/{owner}/{repo}/rules/branches/main --jq '.[]|{type, p:.parameters}'
```

At the time of writing that says: one approving review, `dismiss_stale_reviews_on_push: true`, `required_review_thread_resolution: true`, five required contexts (`build`, `build-macos`, `build-windows`, `security`, `e2e-deterministic`), and `strict_required_status_checks_policy: false`. Two consequences shape everything below:

- **Branches do not need to be up to date with `main`.** Only a real conflict blocks a merge — not being behind.
- **Any push to a PR dismisses its approval.** Every conflict you resolve costs a re-review, so the order of merges matters (Step 3).

If what you read back differs, trust the API over this paragraph and update it.

## Step 1 — Take stock

```bash
gh pr list --state open --limit 100 --json number,title,reviewDecision,mergeStateStatus,mergeable,author,headRefName,headRefOid
```

For each PR, also read its unresolved thread count (GraphQL `reviewThreads { isResolved }`), its required checks (`gh pr checks <n> --required`), **and its non-required checks** (`gh pr checks <n>`). Then put every PR, minus the user's exclusions, in exactly one bucket:

| Bucket | Test | Action |
|---|---|---|
| Ready | `APPROVED`, `CLEAN`, 0 unresolved, required checks pass, every failing or pending **non-required** check explained | merge (Step 3) |
| Conflicting | `mergeable: CONFLICTING` | resolve (Step 4), then re-review |
| Needs review | `REVIEW_REQUIRED`, green, 0 unresolved | request review (Step 5) |
| Checks running | a required check pending | wait, then re-bucket |
| Unresolved threads | any unresolved thread | Step 6 |
| Failing CI | a required check failed | Step 6 |
| Changes requested | `CHANGES_REQUESTED` | Step 6 |

**A non-required check is not a formality.** `UNSTABLE` in `mergeStateStatus` means one is failing, and GitHub will merge past it. Read why before merging: on 2026-09-27 #1363's advisory `desktop-driver` failure was the only sign that merging it would re-break the desktop build (Step 3, "`main` also moves under you"). Merge past a failing or pending non-required check only once you can say it is unrelated to this PR — the same failure on `main`, a known flake re-run green — and say so in the report. **"Unrelated" clears this PR, not the red** (CLAUDE.md rule 6): a flake that re-ran green is still a red with no owner unless it is already quarantined or an open PR fixes it. So give it one **before** you merge past it — hand it to a unit or to the user (Step 6) — and name the owner in the report. The PR itself does not wait for that fix to land: the red is not on its required path, and holding every PR behind every flake is how a backlog stops moving. CLAUDE.md rule 8 covers the same trap for auto-merge.

**Address every PR by its PR ref, never by `origin/<headRefName>`.** A fork PR's branch is not on `origin`, and an `origin` branch can share a fork's branch name while pointing at unrelated code. Fetch the head the PR will actually merge — **with a leading `+`**, so the ref follows a force-push instead of silently keeping the old commit: `git fetch origin +pull/<n>/head:refs/remotes/origin/pr-<n>`, then confirm `git rev-parse origin/pr-<n>` equals `gh pr view <n> --json headRefOid --jq .headRefOid`, and use `origin/pr-<n>` everywhere below. It also covers the case where `git fetch origin <branch>` simply fails (seen with a Renovate branch) — never read a failed fetch as "no conflicts".

**`UNKNOWN` is not a bucket.** GitHub recomputes mergeability after `main` moves and reports `UNKNOWN` for seconds to minutes. Poll until it settles; never read it as blocked or as clean.

Show the user the table before acting, with the exclusions and what you will do per bucket.

## Step 2 — Before resolving anything, look for duplicated fixes

When several PRs were dispatched from the same base, they often carry **the same fix** for a red they each met — measured on 2026-09-26, four PRs carried their own variant of the same `remote_tunnel` probe-test isolation. The first to merge makes every other copy a conflict. Recognise the pattern up front (the same file conflicting in several PRs), so Step 4 resolves them the same way.

## Step 3 — Merge order

Merge one PR at a time, and re-read every remaining PR's `mergeable` after each merge — a merge can turn a clean PR into a conflicting one.

**Decide the order by simulating it, not by guessing from file lists.** Two PRs editing the same file usually merge cleanly; guessing from overlap held six approved PRs back for no reason on 2026-09-26. `git merge-tree` answers it exactly and touches no working tree:

```bash
git fetch origin
c=$(git rev-parse origin/main)
# stack EVERY PR you intend to land, in the order it will merge — the ready
# ones first, then the ones awaiting review — so the awaiting ones are tested
# against each other too
for x in <PRs, in merge order>; do
  git fetch -q origin "+pull/$x/head:refs/remotes/origin/pr-$x"
  h=$(git rev-parse "origin/pr-$x")
  [ "$h" = "$(gh pr view "$x" --json headRefOid --jq .headRefOid)" ] || { echo "#$x: fetched ref is not the PR head"; exit 1; }
  # capture first, then test merge-tree's OWN exit status: piping it into
  # `head` would test head's status and let a conflict read as clean
  if out=$(git merge-tree --write-tree "$c" "$h"); then
    c=$(git commit-tree "$(printf '%s\n' "$out" | head -1)" -p "$c" -p "$h" -m "sim #$x")
    echo "#$x stacks cleanly"
  else
    echo "#$x conflicts with what is stacked before it"   # do not stack it; it goes to Step 4
  fi
done
git update-ref refs/heads/sim/integration "$c"
```

**Simulate the awaiting-review PRs in the same stack, not just against the ready ones.** Checking each only against the ready stack misses two awaiting PRs that conflict with each other — on 2026-09-26 #1338 and #1331 each merged cleanly on top of the ready stack, and #1331 went `CONFLICTING` the moment #1338 landed.

**A clean merge is not a green merge.** `merge-tree` finds textual conflicts only. Two approved PRs can merge cleanly and still break each other's behaviour — #1340's voice deck-switch test failed after #1334 changed how desktop settings save, with no conflict marker anywhere. Step 4's gates catch that; run them on every branch you merged `main` into, including the desktop frontend's tests when `desktop/` is touched.

**Then build and test the stacked result before anyone reviews or merges anything.** Check `sim/integration` — every PR that stacked cleanly, awaiting-review ones included — out detached in the land worktree, and run the full gates on it — rule 2's clippy, `cargo test-fast`, and the desktop `pnpm test` plus typecheck. This is the merge queue this repository cannot have (a user-owned repo rejects the `merge_queue` rule, #1088), run once for the whole batch. It finds the clashes `merge-tree` cannot see while every PR still has its approval.

**Batch what the simulation clears, serialise what it does not.** Merging one PR never dismisses another's approval — only a push to that PR does. So every PR that stacks cleanly *and* passes on the integration build can be reviewed in one sweep and merged back to back with no further pushes. Only the PRs the simulation flags need a push: resolve those one at a time after the PR they collide with, or stack them onto it beforehand so the resolution is done and gated once — after the base lands, one mechanical merge of `main` and a re-review still remain (item 4 below). Finding collisions during merging instead — what happened on 2026-09-26 — turns every one into a push, a CI run and a re-review, and the next merge can surface the next one.

Then:

1. **Merge every ready PR that makes no awaiting-review PR conflict**, now. Waiting for the rest gains nothing.
2. A ready PR that *would* make an awaiting-review PR conflict: if that PR's review is already in flight, hold the ready one until it lands — otherwise the re-review is paid twice. If the other PR has not been sent for review yet, merge the ready one and resolve the other before sending it.
3. Two ready PRs that conflict with each other: merge one, then resolve the other.
4. **Stacking does not make the upper PR merge cleanly after a squash.** This repository squash-merges, so once A lands, `main` holds A's version of every hunk A touched while B's branch holds the combined version, and git sees two different edits of the same lines wherever both PRs edited them — measured on 2026-09-27, #1346 went `CONFLICTING` the moment #1347 (its base) landed. What stacking buys is that the resolution is already known: merge `main` into B keeping B's side in those hunks (B already contains A's content), check that `git diff origin/main HEAD` now shows only B's own changes, and re-review B. **When `main`'s tree is byte-identical to A's final head** (check `git rev-parse origin/main^{tree}` against `git rev-parse <A-head>^{tree}` — true when nothing else landed in between), B's current tree already *is* the right result: `git merge -s ours origin/main` records `main` as a parent without changing a file, and `cmp <(git diff origin/main HEAD) <(git diff <A-head> <B-head-before>)` proves the diff is exactly B's. Nothing was edited, so the gates that passed on that tree still hold. It does not buy a push-free merge. **And a stacked PR's diff shows the base's changes, so a reviewer's findings about the base's code land on the upper PR** — four of #1346's six threads were about #1347. Route those to the base PR while it is open, or to a follow-up issue once it has merged (#1365), and resolve them on the upper PR with a pointer.
5. **A stack moves together.** When B is stacked on A (A's branch merged into B's), a change to A — a reviewer's `REQUEST_CHANGES`, a late finding — means re-merging A's new head into B and re-reviewing B, and so on up the stack. So review the stack bottom-up and hold B's review until A is approved: on 2026-09-27 #1347 drew a `REQUEST_CHANGES` while #1346 (stacked on it) and #1342 (stacked on #1346) were queued behind it, and reviewing them first would have spent two reviews on heads about to change.
6. **A rename or sweep PR** (a glossary, a mechanical call-site sweep) **lands last.** Every PR that merges after it and uses an old name breaks without a conflict marker. Land the others, merge `main` into the sweep once more, re-run its gates so the rename covers what just arrived, and merge it then.

Merge with a squash, matching the repository's history:

```bash
gh pr merge <n> --squash
gh pr view <n> --json state --jq .state   # read it back: MERGED
```

Decide on the read-back, not on the exit code.

**After every batch of merges, build `main` before doing anything else**: `git fetch origin`, check out `origin/main` detached in the land worktree, and run rule 2's clippy command (it compiles every target), `cargo test-fast`, and the desktop tests. Because the ruleset does not require branches to be up to date, two PRs that are each green can merge cleanly and still not compile together. Measured on 2026-09-26: #1335 added a two-argument call in a `src/main.rs` test, #1341 then gave the function a third parameter, and `main` stopped compiling its bin tests with neither PR ever red. When it happens, fix `main` first with its own small PR (it is a red on `main`, CLAUDE.md rule 6) — every branch you merge `main` into inherits the break, so its gates prove nothing until then. **So does every PR's CI, even on a branch you never touched**: a `pull_request` run builds the PR merged into its base, so a PR whose checks were green goes red on its next push with the same error. Review the fix PR on its own (`pr_number=<fix>`) rather than in a sweep and merge it. Then **merge the fixed `main` into each affected PR and push** — `gh run rerun` does NOT help, because a re-run rebuilds the same merge commit it built the first time, which still carries the break. Only then sweep; the reviewer will not vote on a PR whose checks are red.

**`main` also moves under you from outside this run** — another deck, another session, the user merging by hand. Re-read `main` before each merge, and when a PR's premise is about `main`'s state (a revert, a fix for a red), re-check that premise first: on 2026-09-27 #1363 was approved to revert the Rust half of a Tauri bump, but #1360 had meanwhile moved the npm half to match, so merging the revert would have re-broken the desktop build. A non-required check that fails on the PR but passes on `main` is the tell. Rework or close the PR rather than merge it.

**Renovate merges some updates itself** (its `pull_request` bypass, CLAUDE.md rule 8), so a Renovate PR can vanish from the list mid-run and change `main` under you. Re-list open PRs after every batch rather than working from the first listing. On 2026-09-26 #1358 moved the Rust `tauri` crate to 2.12.0 through that bypass while the npm `@tauri-apps/*` packages stayed at 2.11 — the npm half is held by pnpm's 24-hour release-age gate and Renovate's 3-day npm delay — and `tauri build` refuses the mismatch, which breaks the desktop-driver job and the release's desktop bundle. Check that the Tauri crate and npm versions agree on the same minor before cutting a release; do not paper over it with a `minimumReleaseAgeExclude` without the user's say-so.

## Step 4 — Resolve conflicts

**Work in a sibling worktree** (CLAUDE.md rule 14), never in the main checkout — every dispatched unit is cut from its `HEAD`. The unit that wrote the PR has usually closed its tab by now, and its worktree went with it, so do not count on finding one.

Work on a **detached** checkout of the PR's head, so it does not matter whether a local branch exists or is checked out in another worktree (the #714 team's still was), and push the result back to the PR's branch by name:

```bash
git fetch origin
[ -d ../<repo>-land ] || git worktree add --detach ../<repo>-land origin/main
cd ../<repo>-land
git fetch -q origin "+pull/<n>/head:refs/remotes/origin/pr-<n>"
git switch --detach "origin/pr-<n>"
git merge origin/main              # resolve the conflicts (rules below)
git add <each resolved file>
git diff --name-only --diff-filter=U   # must print nothing
git commit --no-edit               # completes the merge; nothing is pushed without it
# gates (below), then:
# same-repository PR only (isCrossRepository is false) — see below for a fork
git push origin "HEAD:$(gh pr view <n> --json headRefName --jq .headRefName)"
```

That push is a fast-forward of the PR branch, so it is refused rather than overwriting anything if the branch moved meanwhile — fetch again and redo the merge. **Never push a fork PR's resolution to `origin`**: `origin` is the base repository, so `HEAD:<headRefName>` there creates or moves an unrelated same-named branch and leaves the PR untouched. Check `gh pr view <n> --json isCrossRepository,maintainerCanModify` first. For a fork, and only when `maintainerCanModify` is true, push to the head repository itself — `gh pr view` returns the owner and repository as objects, so build the URL from their fields: `git push "$(gh pr view <n> --json headRepositoryOwner,headRepository --jq '"https://github.com/\(.headRepositoryOwner.login)/\(.headRepository.name).git"')" "HEAD:<headRefName>"`. Otherwise ask its author.

Resolution rules, from cases met so far:

- **`CONTRACT_BREAKS` in `src/daemon_protocol.rs` is append-only** (CLAUDE.md rule 12). When both sides appended, keep `main`'s entries first and the branch's after them, and never drop or edit one.
- **A duplicated fix (Step 2):** take `main`'s copy of the file with `git checkout origin/main -- <file>`, but only after confirming the branch's hunks in that file are nothing but the duplicate, and that nothing else in the branch calls a helper the branch added there (`git grep` the branch, excluding that file).
- **Do not use `git merge -X theirs` as a shortcut.** It resolves only the conflicting hunks and keeps the branch's non-conflicting ones, which half-applied a fix on 2026-09-26 and left an unused variable behind.
- **Two sides that each reworked the same test fixture:** keep the structure that landed on `main`, and port the branch's additions onto it (constants, extra env, call-site changes). Read both sides before editing.
- **A call site the other PR's change made stale** — a new argument, a new return type — is resolution when the correct value follows from the other PR's own definition, not from a design choice: #1335's two-argument call took `None` for the parameter #1341 added, and #1346's bare `return;` became `AppliedEvent::Rejected` once #1347 gave the function a return value, because #1347 defines `Rejected` as "nothing on any card moved". Say which definition decided it in the commit. Anything that needs judgement about behaviour goes to a unit.

Then run the gates on the merged branch before pushing — `cargo xtask affected-checks --run`, which for a PR with any Rust, build input or unmapped path in it runs CLAUDE.md rule 2's `cargo fmt --check` and `cargo clippy --workspace --all-targets --features e2e,e2e-live -- -D warnings` and rule 5's `cargo test-fast`, and for a PR that is only mapped text runs the tests that read those files; then the tests covering whatever the conflict touched, and **the desktop frontend's `pnpm test` and typecheck whenever the PR or the merged-in `main` touches `desktop/`** (`cargo test-fast` does not run vitest). A gate that fails after a clean merge is a semantic clash between two PRs: that is code, not conflict resolution, so it goes to a unit (Step 6). The push dismisses the approval, so the PR moves to "Needs review".

Several PRs can share one worktree: switch between detached heads, but finish (commit or `git merge --abort`) one merge before switching.

**If an edit is refused by the permission classifier,** stop on that PR, leave the merge in progress, and tell the user which file and what the resolution would be. Do not reach the same edit by another tool.

## Step 5 — Get approvals re-cast

The approving actor is the agent reviewer, `pr-review-batch.yml` (CLAUDE.md rule 8). Its mechanics decide how to call it:

- **It only votes on a PR whose required checks have all concluded.** Dispatching it right after a push produces `skip #<n>: required context '…' has not concluded` and no vote. Wait for CI first.
- **Its concurrency group holds one running and one pending run.** A second dispatch replaces the pending one, so dispatching once per PR in a burst cancels all but the last. After CI is green on every PR you need reviewed, dispatch **one sweep** instead:

  ```bash
  gh workflow run pr-review-batch.yml -f dry_run=false
  ```

  `dry_run` defaults to `true` on a manual dispatch, which produces verdicts and casts no vote. With `pr_number` set it reviews one PR; dispatch those one at a time, each after the previous run finishes.
- **It reviews three PRs in parallel** (`max-parallel: 3`), so a sweep over many PRs takes several rounds.
- A sweep also covers PRs you are not landing. That is its normal job — an approval is not a merge, and the exclusions are still yours to hold.
- **For a stack, review bottom-up one PR at a time** (`pr_number=<n>`), each after the previous review has finished and the previous PR has merged: an upper PR's review is wasted if its base draws a change request. A small loop that waits for CI, dispatches the review, waits for it, and merges only when the PR reads `MERGEABLE APPROVED` with no unresolved thread and a clean `merge-tree` against `main` — stopping at the first PR that does not — keeps this unattended without ever merging past a problem.
- **Wait for CI on the PR's current head SHA, not for `gh pr checks` to read green.** Right after a push `gh pr checks` can still report the previous head's results, and a review dispatched then skips the PR (`required context 'build' has not concluded`). Read the head with `gh pr view <n> --json headRefOid` and wait for `gh run list --commit <sha> --workflow ci.yml` to report `completed` first.
- **Every push brings a fresh Qodo review**, including a push that only merges `main` in. Its new findings land as threads and block the reviewer's vote, so after each push read the threads again before expecting a vote.

When a vote arrives, go back to Step 3. A `REQUEST_CHANGES` from the reviewer goes to Step 6.

## Step 6 — What needs someone else

Report these; do not work around them.

- **A person's `CHANGES_REQUESTED`.** Theirs to lift. Say what they asked for, and check whether it still applies on the current `main` — a blocker about files `main` no longer carries may have resolved itself (#1252: two fragments it "deleted" had since been consumed by a release). If it no longer applies and the reviewer is unavailable, the user decides. **Do not dismiss the review and do not reopen the PR as a new one** — the second has the effect of a dismissal with less of a record. The agent reviewer does not help here: it skips a PR with an open change request by rule (`skip #<n>: changes requested by a reviewer`), so there is no approval to get. What worked for #1252: on the user's explicit instruction, confirm the required checks are green, no thread is unresolved and the PR merges cleanly, write the reason to a file — which blocker it was, why it no longer applies, and that the review was left standing — and pass it as the squash body: `gh pr merge <n> --squash --admin --body-file <reason.md>`. Without `--body-file` the reason is lost, which defeats the point of recording it. That is the one sanctioned use of `--admin` in this skill.
- **A reviewer `REQUEST_CHANGES` naming a manual obligation** (for example rule 12's cross-version test) — addressed to a person; a green check is not an answer to it.
- **Unresolved review threads, and findings that need code.** Hand the PR to a unit — `/pr-review-queue` composes that task — or, for a single PR, tell the user. A unit that already finished cannot be sent more input from here. A unit fixing an existing PR must not open a new one: its worktree is cut from `main`, so tell it to `git fetch origin +pull/<n>/head:refs/remotes/origin/pr-<n> && git switch --detach origin/pr-<n>`, commit there, and `git push origin HEAD:<headRefName>` — for a same-repository PR; a fork PR's push goes to the fork as Step 4 describes. The detached form also works when the branch is checked out in some other worktree. Tell it to stop after the push without requesting review; this skill runs the re-review.
- **A trivial rule violation the reviewer requested changes on** (a Scenario comment over rule 7's sentence cap, a hard-wrapped paragraph) is mechanical: fix it yourself on the branch, run the gates, push, and say so in a PR comment. Anything that changes behaviour is not trivial.
- **A findings thread that needs facts only the user has** (what they observed on a machine you cannot reach): never write the result yourself. Ask the user, and leave that thread open until they answer.
- **A failing required check** that is not a conflict artefact. The same: a unit, or the user.
- **A red you met that no PR owns** — a flake that re-ran green, a check that fails on `main` too. Search first (`gh pr list --search '<test name>'`): an open PR that fixes it is its owner, and a second unit would only produce the duplicate fix Step 2 then has to untangle. CLAUDE.md rule 6 puts it in scope whoever caused it, and a fix or a quarantine is a code change beyond Step 4's narrow adaptations, so it goes to a unit whose task says to fix it or quarantine it (a named owner, an expiry issue, and `#[ignore = "quarantined: <owner>, #<issue>"]` on the test) and to say which. Name it in the report either way; "re-ran green" alone is not an outcome.
- **Anything credentialed** (lane 2, a real release) — rule 5.

## Step 7 — Report

Per PR: merged (with the squash SHA), or the bucket it is still in and the one thing it is waiting on. Then the totals, and whether anything left blocks a release. If `tag-release` called this skill, say plainly whether it should proceed.
