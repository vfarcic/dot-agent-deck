---
name: merge-pr
description: 'Merge one pull request into `main` in this repo, then watch `main` until its CI for that merge has finished, and own a red. Confirms the PR is approved for its current head and every required check passed, runs the overlap check (has `main` changed the PR''s files since its CI ran), merges pinned to the checked head, and waits for the push run of the merge commit. Use whenever you are about to run `gh pr merge` in this repo for any reason, by hand or from inside another skill: `/issue-queue`''s bug-fix units, `/land-prs`, `/code-cleanup` and any other skill that merges point here instead of carrying their own copy. Not for arming auto-merge (`gh pr merge --auto`), where nobody is present after the merge; `/pr-create` covers that.'
user-invocable: true
---

# Merge a pull request, and see `main` through it

A merge is not done when GitHub says `MERGED`. It is done when `main`'s own CI for the merge commit has finished green. Two things make that more than a formality here:

- **Pull requests do not run everything.** `build-macos` runs on pushes to `main`, not on pull requests (`.github/workflows/ci.yml`), so `main`'s run is the first time a merged change is tested on macOS. It fails where Linux passed in about 3% of runs (measured over 545 runs, 2026-10-06 to 10-21), and the ones that were real defects reached `main` only because nothing ran macOS before the merge.
- **A release requires it.** `tag-release.yml` refuses to release a `main` whose CI run is not green, so a red left behind blocks everyone's release, not only yours.

The calling skill decides *whether* to merge: its approval rounds, its sensitive-path rule, its conflict handling. This skill is *how*: the checks right before the merge, the merge itself, and the watch after it. When no skill called this one, the decision is yours, and CLAUDE.md rule 8 still applies in full: never merge your own unapproved PR, and stop for a person when the reviewer's approval says to read the PR first (`needs-human-eye`, a sensitive path in `DENY_PATHS` of `.github/scripts/pr_review_common.py`).

## 1. Right before the merge

Read the PR at its current head, not from memory:

```bash
gh pr view <n> --json headRefOid,reviewDecision,mergeStateStatus,statusCheckRollup
gh api repos/vfarcic/dot-agent-deck/rules/branches/main \
  --jq '[.[] | select(.type == "required_status_checks") | .parameters.required_status_checks[].context]'
```

- `reviewDecision` is `APPROVED`, for this head. A commit pushed after the approval voids it (`dismiss_stale_reviews_on_push`); a `gh pr update-branch` merge was measured not to (2026-09-07).
- **Every required context passed for this head.** Take the list from the ruleset, not from this file: it changes, and `RWX: linux` is the Linux verdict when `.github/ci-backend` says `rwx` (docs/develop/rwx.md). A job that skipped itself counts as passed, which is how `build-windows` (`if: false`) and the side the CI switch turned off report. The rollup holds two shapes: check runs (`name`, `conclusion`) and commit statuses (`context`, `state`), and RWX reports as the second, so read both. Right after a push the rollup can hold only the checks that have registered so far: nothing pending, and every required context present.
- **Nothing failing, required or not**, that you have not read and can explain. An advisory red is not a merge blocker, but it is a red you have met (CLAUDE.md rule 6).
- `mergeStateStatus` is not `DIRTY`. A conflict is the calling skill's to resolve (merge `origin/main` into the branch, run the gates, push), and the approval has to be re-cast after that push.

Then run the overlap check below, and merge only on `NO OVERLAP`:

```bash
gh pr merge <n> --squash --match-head-commit <sha>   # the sha on the overlap check's `head` line
gh pr view <n> --json state,mergeCommit --jq '"\(.state) \(.mergeCommit.oid)"'   # MERGED <merge sha>
```

Decide on the read-back, not on the exit code. **Not `--auto`:** CLAUDE.md rule 8 records that `--auto` on an already-approved PR with only a non-required check pending merges on the spot and exits 0. **No `--admin`, no bypass**, with one sanctioned exception: `/land-prs` Step 6's admin merge of a PR held by a stale `CHANGES_REQUESTED`, on the user's explicit instruction. That merge still runs everything else here, the overlap check before it and the watch of `main`'s run after it; only the approval check is waived, and only by that instruction.

## The overlap check before a merge

**Every merge without `--auto` runs this check first, and this skill is where it is written down** (it moved here from `/issue-queue`; `/issue-queue`, `/land-prs` and `/code-cleanup` merge through this skill, and CLAUDE.md rule 8 asks the same of a person merging by hand). `xtask/linkage-check`'s `merge_overlap_check` tests fail when a skill runs `gh pr merge` without naming this check. Auto-merge cannot run it, which `pr-create` and `prd-queue` say where they allow arming. The ruleset does not require a branch to be up to date before it merges (`strict_required_status_checks_policy: false`), so a PR merges on CI that built it against an older `main`, and two PRs that are each green can break `main` together without a textual conflict. It happened twice in two days (issue #1610). On 2026-10-04 #1546 added `remember_command` to `AttachRequest::StartAgent`, and #1557, whose CI ran before #1546 landed, added a `StartAgent` literal to `tests/daemon_protocol.rs` without it: `main`'s test targets stopped compiling (E0063), fixed by #1585. On 2026-10-05 #1558 added `AgentRecord::prompt_keys`, and #1524, merged by hand after it on CI from before it, added an `AgentRecord` literal to `src/ui.rs` without it: five CI jobs went red on `main`, fixed by #1594.

**The check.** Right before merging, list the commits `main` gained since the PR branched off that touch any file the PR changes:

```bash
n=<n>
(   # a subshell, so `set -e` ends the check on any failed read and leaves your shell alone
  set -euo pipefail
  git fetch -q origin main "+pull/$n/head:refs/remotes/origin/pr-$n"
  head=$(gh pr view "$n" --json headRefOid --jq .headRefOid)
  [ "$(git rev-parse "origin/pr-$n")" = "$head" ] || { echo "STOP: the fetched ref is not the PR head; run the check again" >&2; exit 1; }
  [ "$(gh pr view "$n" --json changedFiles --jq .changedFiles)" -le 3000 ] || { echo "STOP: over 3000 changed files, more than the files endpoint returns" >&2; exit 1; }
  list=$(gh api "repos/{owner}/{repo}/pulls/$n/files" --paginate --jq '.[] | (.filename, (.previous_filename // empty)) | if test("\n") then error("STOP: a changed path contains a newline") else . end')
  [ -n "$list" ] || { echo "STOP: no changed files read" >&2; exit 1; }
  files=(); while IFS= read -r f; do files+=("$f"); done <<<"$list"
  overlap=$(git log --format='%h %s' "$(git merge-base origin/main "origin/pr-$n")..origin/main" -- "${files[@]}")
  echo "head $head"
  if [ -z "$overlap" ]; then echo "NO OVERLAP"; else echo "OVERLAP:"; echo "$overlap"; fi
)
```

**It ends in one of three ways, and only one of them lets you merge.** `NO OVERLAP` and `OVERLAP:` are verdicts. Anything else (a `STOP:` line, a failed fetch, a failed or partial API read) ends the check with a non-zero status and no verdict, which is a stop rather than a pass: an empty listing after a failed read proves nothing. Run it again; if it keeps failing, stop and report. It fetches the PR by its pull ref, for the reason `/land-prs` Step 1 gives: a fork's branch is not on `origin`, and an `origin` branch of the same name can be unrelated code. It takes the files from the paginated endpoint (`gh pr view --json files` stops at 100), which returns at most 3000, so a larger PR stops rather than being checked against part of its files. `previous_filename` adds the old path of a file the PR renames, which `main` may still be changing. The file list is read one line per path with a `while read` loop so that the check also runs under macOS's system Bash 3.2, and a path that itself contains a newline, which that loop would split, stops the check.

- **`NO OVERLAP`:** the green CI still holds for the files this PR changes. Merge, pinned with `--match-head-commit <sha>`, where `<sha>` is the one on the check's `head` line, not one read again now, so a push after the check makes the merge refuse.
- **`OVERLAP:` followed by commits:** update the branch with `gh pr update-branch <n>`, which merges `main` into it (when it reports a conflict, follow the calling skill's conflict step instead). Then let CI run on the combined head, confirm the approval still stands (`gh pr view <n> --json reviewDecision`) and request it again if it was dismissed — a `gh pr update-branch` merge was measured not to dismiss an approval (2026-09-07), while a hand-pushed commit does — and run this check once more before merging. **Each update is one round of the calling skill's bound** (for `/issue-queue`'s bug-fix units, one of its step 2's three rounds; `/land-prs` allows three updates per PR): `main` can gain a commit on the PR's files between any two runs, so a skill that re-checks without counting can loop forever. The check after the last update the bound allows still runs, and a `NO OVERLAP` from it merges; only when it lists commits again, so clearing them would take one update more than the bound allows, stop and report the commits it listed rather than merging. Every update is also a push that Qodo re-reviews, so the calling skill waits (bounded) for that re-review and answers it before checking again — `/issue-queue` step 3 and `/land-prs` Step 3 each say how. Any local checkout of the branch is now behind it; pull before committing on it again.
- **A commit that changes only comments or prose** in a file the PR also changes still counts, and certainly when a test reads that file. If you judge one harmless, say so in the report, naming the commit; do not skip the check. In practice the overlap is often `tests/CATALOG.md`: of the 27 commits this check lists for #1524, 22 touched it.
- **Re-check right before merging, every time.** `main` moves while CI and the review run. `--match-head-commit` pins the PR's head, not `main`, so a commit that lands between this check and the merge is not caught; checking again immediately before the merge narrows that window and does not close it.

**What it catches, and what it does not** (CLAUDE.md rule 17). It catches overlap in the files a PR changes. It counts from the branch's merge-base with `main`, which is at or before the `main` the PR's CI built against, so it can also list commits that CI already covered; that costs an update that was not needed, and no overlapping commit that landed after CI ran is left out. It does **not** catch every semantic conflict, because a break can come through a file the PR does not touch: a new required field breaks a struct literal in whatever file holds one. The second incident shows both halves. The check would have flagged #1524, but only through `src/spawn.rs` and `tests/CATALOG.md`, which both PRs changed; the break itself joined `src/agent_pty.rs`, which only #1558 changed, to `src/ui.rs`, which only #1524 changed. `main`'s own push CI, which [After the merge](#after-the-merge-watch-mains-run) below waits for, is the backstop for what this misses.

## After the merge: watch `main`'s run

Every push to `main` gets its own `ci.yml` run, and a later push does not cancel an earlier one (its concurrency group is the run id), so the run for your merge commit is yours to watch even when other merges follow it:

```bash
merge=<merge sha from the read-back above>
for _ in $(seq 1 45); do   # minutes
  run=$(gh api "repos/vfarcic/dot-agent-deck/actions/workflows/ci.yml/runs?head_sha=$merge&event=push&branch=main&per_page=1" \
    --jq '.workflow_runs[0] // empty | "\(.id) \(.status) \(.conclusion) \(.html_url)"') || run=""
  case "$run" in *" completed "*) break ;; esac
  sleep 60
done
echo "${run:-no run for $merge}"   # <id> completed success|failure|... <url>
```

The bound is 45 minutes; the run takes about 15 to 25 (the full Linux matrix and macOS, on GitHub Actions). When it has not finished by then, report that rather than waiting on. A run that never appears is a finding too, since nothing else would test the merge.

- **Green**: the merge is done. Report the merge commit and the run.
- **Red**: it is yours, whoever's change caused it (CLAUDE.md rule 6), and it blocks the next release. Read the failed jobs (`gh run view <id> --log-failed`) and the `load context` block a harness panic ends with, and rerun the failed job once (`gh run rerun <id> --failed`) if it reads as contention; a rerun that passes turns the run green. A real failure gets a fix PR of its own, merged through this skill, so its own run is watched in turn; a macOS-only one can be reproduced on a Mac with `cargo test-fast <test>`. When you cannot fix it in this session, say so in the report and leave the issue `notify-main-red` opened (or open one) with what you found. Do not quarantine to make it go away; CLAUDE.md rule 6 has the conditions for that.

A skill that merges several PRs in a row (`/land-prs`) watches each merge commit's run; they run side by side, so the batch costs one wait, not one per PR.
