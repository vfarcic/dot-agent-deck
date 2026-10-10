---
name: dispatch-base
description: 'Bring the base that every dispatched unit is cut from up to date before the first `dot-agent-deck dispatch` of a batch in this repo, and check the base `dispatch` reports afterwards. Fetches, fast-forwards `main` when there is no local work to move, and otherwise leaves the checkout alone and reports which precondition failed and how far `HEAD` is from `origin/main`. Use whenever you are about to run `dot-agent-deck dispatch` in this repo for any reason: an ad-hoc "start X as a separate line of work" in a dispatcher pane, `/issue-queue`, `/prd-queue`, `/pr-review-queue`, `/code-cleanup`, or any other skill that dispatches. Those skills point here instead of carrying their own copy.'
user-invocable: true
---

# Bring the dispatch base up to date

This is where the base step for **every** dispatch in this repo is defined: an ad-hoc request in a dispatcher pane, `/issue-queue`, `/prd-queue`, `/pr-review-queue`, `/code-cleanup`, or a skill written later. The queues used to carry their own copies of it; issue #1638 moved it here after an ad-hoc dispatch, which had no copy at all, cut three units from the wrong branch. A dispatching skill says *when* in its own flow to run this and where to carry a refusal; *what* the step does is here.

## `HEAD` is the base, and nothing else is

**`dispatch` has no base or branch option.** It resolves the caller's **`HEAD`** to a commit once, then runs `git worktree add <dir> -b agent/dispatch-<name> <that sha>` **in the caller's own working directory** (`resolve_dispatch_base` and `create_dispatch_worktree` in `src/dispatch.rs`, feeding `create_worktree_from` in `src/issue_dispatch_run.rs`), and reports that same sha in its `cut from <branch> at <sha>` reply. So whatever the dispatcher checkout has checked out at dispatch time is the base every unit inherits, and no flag overrides it.

**Updating the local `main` ref while another branch is checked out changes nothing for a unit.** `git fetch origin main:main` and `git branch -f main origin/main` move a ref that `dispatch` never reads. Only what `HEAD` points at counts.

**What getting that wrong cost, on 2026-10-10.** A dispatcher pane was asked to dispatch PRDs #1631, #1401 and #1258 while its checkout was on `prd/1487-shared-daemon-upgrade`. The agent ran `git fetch origin main:main`, took that as the base, and dispatched. All three units were cut from the feature branch, 19 commits behind `origin/main` and carrying one commit that was not on `main`; #1631's unit lacked #1617, the PR its PRD builds on, while its task said #1617 was present. All three were stopped, their worktrees and branches deleted, and the batch re-dispatched. `dispatch`'s own reply had said `cut from prd/1487-shared-daemon-upgrade at 0935e2a7`, which is why [step 4](#step-4--after-dispatch-read-the-base-dispatch-reports) exists.

**What a stale `main` cost, on 2026-08-30.** Two units were dispatched from a local `main` at `820ba40`, six commits behind `origin/main` at `83d9bf3`. One of those six was `daf94f0`, the commit that introduces `desktop/`, and both units had been dispatched to work on the desktop app. They were cut from a tree with no `desktop/` directory, could not have done anything, and were re-dispatched after a pull with not one original commit between them. **A unit cannot discover this about itself.** It sees a valid checkout, finds the code its task names missing, and reasonably concludes that the *task* is stale rather than its base.

## When to run it

**Once before the first dispatch of a batch, never between two.** A batch is every unit dispatched for one request: one ad-hoc ask, or one queue run. Updating mid-batch splits it across two bases, and the units already started keep the old one. A later, separate request is a new batch and runs this again.

A dispatching skill may say otherwise for its own flow. `/issue-queue` re-runs it before every dispatch when its bug-fix units merge their own PRs, because there the loop itself moves `main` between dispatches; that exception, and why, is in its step 0.

## Step 1 — Fetch, then read the state

```bash
git fetch origin --quiet
git rev-parse --abbrev-ref HEAD                        # the branch every unit is cut from
git status --porcelain --untracked-files=no            # ANY output means tracked changes
git rev-list --left-right --count HEAD...origin/main   # "0  6" is 0 ahead, 6 behind
```

A skill that fetched in an earlier step of its own does not need to fetch again, as long as nothing has run in between that could have moved `origin/main`.

## Step 2 — Fast-forward when there is no local work to move

**When `HEAD` is `main`, that status output is empty, and the ahead count is `0`, fast-forward it and say you did.** No prompt and no question: an up-to-date base is the default here, and the user is told what happened rather than asked to authorise it. Print the sha before moving, so the report can say where the base was, and again after, since that is the base the units inherit.

```bash
git rev-parse --short HEAD                             # the sha before moving
git merge --ff-only origin/main
git rev-parse --short HEAD                             # the sha after: the units' base
```

When step 3 declines to move, the base is `HEAD` as step 1 left it: record `git rev-parse --short HEAD` then, with the distance step 1 read.

**Why this is safe.** The hazard behind the older rule, which surfaced staleness and asked every time (until issue #760), was that *the user may have local work, and a dispatching agent has no business moving their branch*. That hazard is kept: it is what the three preconditions test for. Together they say **there is no local work here to move**: no uncommitted tracked change, no commit that is not already on the remote, and the branch is the one the remote's is. A fast-forward under them rewrites nothing, discards nothing, creates no merge commit, and is undone exactly by `git reset --hard <the sha you printed before moving>`.

**`git merge --ff-only origin/main`, never `git pull`.** The fetch already put the ref in the repository, so the merge is purely local: no second network round trip, and nothing for a `pull.rebase` setting to reinterpret into a rebase of the user's branch. It is also the second of two independent guards: the preconditions decide and `--ff-only` enforces, so if the two ever disagree the merge fails instead of writing a merge commit onto `main`.

## Step 3 — Otherwise, leave the checkout alone and report

**When the base cannot be brought up to date, do not touch the checkout.** Three of the four cases below are precondition failures and the fourth is the merge itself refusing. Say which one it was, with the distance from `origin/main`, in these terms:

- **Tracked changes present**: name the files. They are invisible to the units either way: a unit's copy is made from the last commit ([`docs/dispatcher-mode.md`](../../../docs/dispatcher-mode.md)), so uncommitted work never reaches one. Committing or stashing is the user's to do, not yours. **Untracked files are deliberately not a blocker**, which is why `--untracked-files=no` is in step 1: a fast-forward that would clobber one fails cleanly by itself, and counting them would refuse on nearly every real checkout.
- **`HEAD` is not `main`**: every unit is cut from *that* branch and carries its unmerged work into every PR the batch produces. Name the branch and its distance from `origin/main`. This is the sharpest of the four, because nothing about it looks wrong: a feature branch dispatches exactly as smoothly as `main` does. It is the 2026-10-10 case above.
- **`HEAD` is ahead of `origin/main`**: there is nothing to fast-forward *to*, and the commits that put it ahead are inherited by every unit's branch and turn up in every unit's PR. Report the count; pushing or moving is the user's call.
- **The merge command itself fails although every precondition passed**: a fast-forward that would clobber a file `origin/main` newly tracks is the concrete case. Report the git error and do not dispatch. **Decide on the exit status, never on the output**: git writes `Updating <old>..<new>` to stdout and the refusal to stderr, so captured output of a refusal can end in that line, *after* `Aborting`, reading exactly like a successful fast-forward. `--ff-only` never partially applies, so the checkout is unchanged and there is nothing to undo.

`git log --oneline HEAD..origin/main` names the commits behind the count, which makes a refusal something the user can act on rather than a number.

**The next move is the user's, and there are three legitimate ones:** dispatch anyway onto the older base, clear the blocker and dispatch after it, or defer the batch. Take their answer rather than picking one, and never clear the blocker on their behalf: committing, stashing or switching branch is the local work this step refuses to touch. If they clear it, run step 1 again before dispatching. Ad hoc, put the refusal to them before the first dispatch. A queue skill says where it carries the refusal instead, usually to the moment it asks how many units to dispatch, because its selection works from `origin/main` and does not depend on the checkout.

## Step 4 — After dispatch, read the base `dispatch` reports

`dispatch`'s success line ends with the base it cut the worktree from (`resolve_dispatch_base` in `src/dispatch.rs`):

```text
dispatch: spawned isolated agent for '<name>' in <dir>, cut from main at c701932
```

**Read that clause on every dispatch, and compare its sha with `origin/main`:**

```bash
git rev-list --left-right --count <sha>...origin/main  # "0  0" is origin/main itself
```

**Treat any of these as a finding to report, not as noise:**

- a branch other than `main`, or `detached HEAD at <sha>`;
- a sha that is not `origin/main`, unless the user chose to dispatch onto an older base after a step 3 refusal, in which case report the distance they chose;
- **no clause at all**: that is an older build, or a probe of `HEAD` that failed, in which case `dispatch` cut the worktree from `HEAD` without naming it. It is never a base that is fine. Fall back to the branch's own record below, and say the clause was missing.

**When the clause is missing, read the commit the unit's branch was created at from its first reflog entry**, which later commits in the unit do not change. Take the branch from the worktree the success line names rather than from the name you passed: `dispatch` sanitizes the name before it builds the branch, so `fix auth` becomes `agent/dispatch-fix-auth`. Read it right after the dispatch, before the unit has had time to switch branch:

```bash
branch=$(git -C "<dir>" rev-parse --abbrev-ref HEAD)            # <dir> from the success line, quoted
git reflog show --format='%h %gs' "$branch" | tail -1          # "<sha> branch: Created from <...>"
```

**Use that sha only when the subject starts `branch: Created from`.** It reads `Created from HEAD` when `dispatch` cut the worktree with no start-point, which is what a missing clause means, and `Created from <full sha>` when it passed the sha it reported. An empty result or any other subject means the reflog cannot answer, because it is disabled, expired or was rewritten. Then say the base could not be verified, and do not report a sha.

On a build older than the fix for issue #1643, the clause was a probe of `HEAD` read just before a `git worktree add` that read `HEAD` again, so the two could differ if `HEAD` moved in between; the reflog sha is the base there. A current build cuts the worktree from the sha it reports, so the clause is the base.

Catching it here costs one stopped unit, before the unit has spent any time working from the wrong tree.

## Report the base as a distance from `origin/main`

Wherever the dispatching skill or the ad-hoc reply says where the work went, give the base as the sha plus `0 behind` (after step 2 fast-forwarded it, or when it was already current) or `N behind` (when step 3 declined to move it), measured at the moment of dispatch. Quote `dispatch`'s own `cut from … at <sha>` clause rather than recomputing it. Report it when the base was current too: nothing else tells a base that was checked from one nobody looked at, and "cut from `main`" reads identically whether `main` is level with the remote or six commits behind it.
