---
name: pr-review-queue
description: Turn the open PRs that are yours to review into a queue, then dispatch one isolated agent per PR — each running /verify-pr on exactly one PR. Selects by assignee, zero unresolved review threads, and no changes-requested; asks how many to take; composes a self-contained task with a per-PR risk note. Use when asked to review several PRs, work through the review backlog, or find which PRs are waiting on you. It verifies nothing itself — for one named PR, use /verify-pr directly.
user-invocable: true
---

# Dispatch the PR review queue

## When to use this

Several PRs are open and the question is *which of them are mine to review, and can they be worked in parallel*. This skill answers that and starts the work; it does not do the work.

Not this skill:

- **One PR, named** → `/verify-pr` directly. Dispatching a single unit just adds a worktree between you and the answer.
- **Your own in-flight work** → `/prd-done`.
- **A quick static read** → the built-in `/review`.

## What this skill does NOT do

It **never verifies a PR itself**. Every verification happens inside a dispatched unit, in its own worktree, in its own pane. This skill selects, asks, composes, dispatches, and reports where the work went. If you find yourself running `checks.sh` or reading a diff for a verdict, you have left this skill and should be in `/verify-pr`.

## Step 1 — Select the queue

Resolve the current user and the repo at runtime. Never hardcode a login: other maintainers run this skill too, and a hardcoded `vfarcic` silently gives them somebody else's queue.

```bash
ME=$(gh api user --jq .login)
read -r OWNER REPO < <(gh repo view --json owner,name --jq '"\(.owner.login) \(.name)"')
```

A PR is **eligible** when all three hold:

1. its assignee set is **empty or contains `$ME`**;
2. it has **zero unresolved review threads**;
3. its review decision is **not `CHANGES_REQUESTED`**.

Criteria 2 and 3 exist for one reason: when threads are unresolved or changes have been requested, the ball is with the PR's **author**, not with a reviewer. Those PRs are work already delegated to someone else. Dispatching a review at them spends the e2e tier re-deriving a verdict that has already been delivered and not yet acted on, and it can talk over the author mid-fix.

Unresolved threads are **not expressible in `gh pr list`** — no `--json` field carries them. GraphQL is the only route:

```bash
gh api graphql -f query='
query($owner:String!, $repo:String!) {
  repository(owner:$owner, name:$repo) {
    pullRequests(states:OPEN, first:50, orderBy:{field:CREATED_AT, direction:ASC}) {
      nodes {
        number title isDraft headRefName
        author { login }
        reviewDecision
        assignees(first:10) { nodes { login } }
        reviewThreads(first:100) { totalCount nodes { isResolved } }
      }
    }
  }
}' -F owner="$OWNER" -F repo="$REPO" --jq '
  .data.repository.pullRequests.nodes[]
  | {number, title, author: .author.login, draft: .isDraft, branch: .headRefName,
     decision: .reviewDecision,
     assignees: [.assignees.nodes[].login],
     threads: .reviewThreads.totalCount,
     unresolved: ([.reviewThreads.nodes[] | select(.isResolved | not)] | length)}'
```

Then filter in your own head, not in `jq`, so you can explain each exclusion: `assignees == [] or ($ME in assignees)`, `unresolved == 0`, `decision != "CHANGES_REQUESTED"`.

Two honest limits of that query, both worth a sentence in your output rather than a silent wrong answer. `reviewThreads(first:100)` is paginated — if `totalCount > 100` the `unresolved` count is a lower bound and a zero is unproven, so page the rest before calling that PR eligible. And `pullRequests(first:50)` truncates a queue longer than 50; say so if the node count comes back at 50.

`reviewDecision` is legitimately `null` on a PR nobody has been asked to review yet. `null` is **not** `CHANGES_REQUESTED`, so it stays eligible — do not treat a missing decision as a blocker.

Draft status is **not** an eligibility criterion. A draft still verifies fine; carry the flag into the task text instead, because `/verify-pr`'s Phase 0 acts on it and a draft verdict is advice rather than a merge decision.

## Step 2 — Show the queue and ask how many

Print every eligible PR with **number, title, author, and why it qualifies** — the assignee state, the thread count, the review decision. Show the excluded ones too, one line each with the reason; the exclusions are the part the user is most likely to disagree with, and they cannot correct a filter they cannot see.

Then **ask how many to dispatch**. Default conservatively. Do not assume "all of them", and do not offer "all" as the recommended option.

Each dispatched unit runs `/verify-pr`, which runs the full e2e tier — the most expensive gate in this repo, spawning real binaries and hitting real LLM APIs for tens of minutes (CLAUDE.md rule 5). How much of that to spend at once is the user's call, not yours. Several e2e suites running at once on one machine also contend for resources, which is its own source of timing noise and shows up as phantom flakes in every one of them.

A smaller batch is also what makes step 4's risk note worth writing properly. Three tailored tasks beat eight generic ones.

## Step 3 — Re-check state immediately before each dispatch

Re-query each PR **right before dispatching that PR**, not once up front for the whole batch. PRs move while a queue is being worked: in the session this skill came from, one PR had been closed at listing time and was reopened later, and two others were merged between listing and review.

Running `scan.sh` gives you the fresh state and step 4's file buckets in one read-only call:

```bash
bash .claude/skills/verify-pr/scan.sh <n>
```

It runs from the main checkout, creates nothing, and touches no worktree. Read `PR_STATE`, `PR_DRAFT`, `PR_HEAD_BRANCH`, and `PR_AUTHOR` from it. If `PR_STATE` is no longer `OPEN`, **skip that PR and say so** in the final report — do not silently drop it, and do not quietly substitute the next PR down the queue to keep the count the user asked for.

Yes, the dispatched agent will run `scan.sh` again as its own Phase 0. That duplication is intentional and nearly free: it is a handful of read-only API calls, and it is what lets you write a tailored risk note without reading the diff yourself.

## Step 4 — Compose a per-PR task, with a risk note

One dispatch per PR:

```bash
dot-agent-deck dispatch <name> --single --task "<text>"
```

For text this long, `--task-file <path>` (or `--task-file -` for stdin) reads it verbatim from a file and spares you a paragraph of shell quoting. The two flags are mutually exclusive.

The task text must be **self-contained**. The dispatched agent is a fresh process in a fresh worktree with none of this conversation in its context. It cannot ask you what you meant. It must carry the PR number, title, head branch, author, and draft flag; it must instruct the agent to execute `/verify-pr` and end with an explicit merge recommendation; and it must carry the merge-`main` instruction and the constraint block below.

**Reference skills and files by path, never by pasting their contents.** The worktree is a full checkout — `.claude/skills/verify-pr/SKILL.md` and `CLAUDE.md` are already in it. A pasted copy is a fork that goes stale the moment either file changes.

### The risk note

Every task carries a **RISK NOTE** of one to three sentences, tailored to what **this** PR touches. This is deliberately **not** a flat template. A generic "review this carefully" produces a generic review; naming the actual failure mode is what makes the agent look for it.

Derive it from `scan.sh`'s buckets, then write prose that names the real files:

| Bucket / flag from `scan.sh` | What the risk note should say |
|---|---|
| `EXEC_ON_CLONE` | These paths run as *you*, with your credentials, the moment you work in the worktree. Read their full diff from the main checkout before creating anything. |
| `CI_SECRETS` | `.github/**` executes in CI where repository secrets live. Phase 1b's checklist is the bar, not Phase 0's. |
| `RULE_12_TRIGGERED=true` | Daemon / protocol / orchestration / hooks. Run CLAUDE.md rule 12's cross-version contract check; a same-wire change of *meaning* is a break too, and needs a `PROTOCOL_VERSION` bump or a `.breaking.md` fragment. |
| `RULE_4_TRIGGERED=true` | A user-visible TUI surface. See it yourself via `/run-dot-agent-deck`; confirm a `.snap` update reflects intended rendering rather than a regression pinned in place. |
| `TESTS` | Check each test was genuinely **fixed, not weakened**. A test that now passes by asserting less, sleeping longer, widening a matcher, or skipping the real condition is a defect, not a fix. |
| `DEPS` | Read the version delta and the exact-pin comments in `Cargo.toml`; a lockfile-only bump still ships transitively. |
| Destructive operations (worktree/branch removal, file deletion, reaping) | Scrutinise the safety gates. Be specific about **what could be destroyed if a gate is wrong** — name the path or ref, not "data loss". |
| `DOCS` / `DOCS_DEVELOP` | Rules 10 and 11: no hard-wrapped prose; developer docs stay under `docs/develop/` and out of `site/sidebars.js`. |

A PR usually hits more than one row. Merge them into prose rather than pasting the table.

### Task template

```
Verify PR #<n> and recommend whether to merge it.

PR #<n>: <title>
Head branch: <headRefName> · Author: <login><, DRAFT> · Repo: <owner>/<repo>

Execute the /verify-pr skill for PR <n>. Its instructions are at
.claude/skills/verify-pr/SKILL.md in this worktree — follow every phase and end
with exactly one verdict from its five-verdict vocabulary (MERGE / MERGE WITH
FOLLOW-UP / REQUEST CHANGES / DO NOT MERGE / BLOCKED — CANNOT VERIFY), plus a
short paragraph on what drove it. Read CLAUDE.md in the worktree root first and
follow it.

MERGE MAIN BEFORE VERIFYING. /verify-pr's setup.sh merges origin/main into the
PR checkout for you — what CI tests is that merge commit, not the bare PR head,
so a PR green in isolation can still break main. If setup.sh reports
MERGE_RESULT=conflict it ABORTS the merge and leaves the worktree at the bare
head; do not stop there and report "merge result unverified". Resolve the
conflicts yourself where the intent of both sides is clear, then verify the
resolved tree, and state in your report exactly which files you resolved and
what you chose. Where the intent is NOT clear, stop and ask the user rather
than guessing. Keep the resolution LOCAL until it is authorized — see the push
constraint below.

RISK NOTE: <one to three tailored sentences, per the table above>

Constraints:
- Do NOT merge, approve, or post any comment or review to GitHub WITHOUT the
  user's explicit say-so. With the user's explicit say-so in this pane, all
  three are permitted — the user is watching and can authorize.
- Never push to the PR's branch, with ONE exception: the conflict resolution
  above, pushed only on the user's explicit say-so. Nothing else ever — no
  fixes, no review suggestions, no formatting, no rebases. If the PR is from a
  fork, read PR_MAINTAINER_CAN_MODIFY from scan.sh first; when it is false you
  have no write access to that branch and the push will fail.
- GitHub blocks self-approval. You run as <ME>, so you CANNOT approve a PR
  authored by <ME>.<if author == ME:> This PR is authored by <ME>, so approval
  is impossible whatever the verdict — deliver the recommendation and leave the
  approval to the other maintainer.
- ORDER MATTERS when you both push and approve. This repo's ruleset sets
  dismiss_stale_reviews_on_push: true, so a push AFTER an approval voids that
  approval. Push the conflict resolution FIRST, then approve — and approve LAST
  overall, after every review thread is resolved. Approving first only means
  approving twice.
```

## Step 5 — Guardrails, in every task text

The two constraints above are **verbatim requirements**, not paraphrasable guidance. They go in every task, every time:

- **Do NOT merge, approve, or post any comment or review to GitHub WITHOUT the user's explicit say-so.** With the user's explicit say-so in the pane, all three are permitted — the user is watching and can authorize. This is narrower than `/verify-pr`'s own rule 3, which forbids posting outright; a dispatched pane is an interactive surface where the user can grant permission in the moment, and the task text is what tells the agent that door exists.
- **Never push to the PR's branch, except a conflict resolution on the user's explicit say-so.** That one exception exists because the merge with `origin/main` is part of verifying, not a change of scope: a conflicted merge is exactly the case where the review cannot proceed until someone resolves it, and throwing that resolution away to report "unverified" wastes the whole run. It stays narrow on purpose — the resolution and nothing else. A fix the agent thinks the author should have made, a formatting pass, a rebase, are all still forbidden however green they look, because the deliverable is a recommendation and the author owns their branch.

## Step 5b — Merging `main` first, and the mechanics around it

**Merging `origin/main` before verifying is not optional.** CI tests the merge commit, so a PR that is green against its own base can still break `main`; `/verify-pr`'s `setup.sh` already performs the merge, and its `MERGE_RESULT` is the signal. What the task text adds is what to do when that comes back `conflict`: `setup.sh` aborts the merge and parks the worktree at the bare PR head, and `/verify-pr` alone would stop there with a **REQUEST CHANGES** and an unverified merge result. The dispatched agent resolves instead — where both sides' intent is clear — verifies the resolved tree, and reports what it chose. Ambiguity is a question for the user, never a guess: a wrong resolution is a defect the agent introduced into someone else's branch.

The next three are **facts about GitHub and this repo's settings**, not policy — encode them so the agent knows them up front instead of discovering them as an API error halfway through:

- **GitHub blocks self-approval.** An agent running as the current user cannot approve that user's own PRs. When a queued PR is authored by the runner — which happens constantly here, since maintainers dispatch reviews of their own work for the *verdict* rather than the approval — say so in the task text explicitly.
- **`dismiss_stale_reviews_on_push: true`** is set on this repo's `main-protected` ruleset, so any push after an approval voids it (CLAUDE.md rule 8). This collides directly with the conflict-resolution push, so the task text spells the ordering out: **push the resolution first, then approve.** Approving first does not fail loudly — it silently un-approves when the push lands, and the PR sits looking reviewed-and-waiting until someone notices the count went back to zero.
- **Pushing to a fork needs `maintainerCanModify`.** `scan.sh` emits it as `PR_MAINTAINER_CAN_MODIFY`; when it is `false` the maintainer has no write access to that head branch and the push fails no matter who authorized it. Then the resolution goes in the report as instructions for the author instead.

## Step 6 — Naming and collisions

Default the unit name to **`verify-pr-<number>-<MMDD>`** — e.g. `verify-pr-465-0810` from `date +%m%d`. The date suffix is what makes a second look at the same PR a week later collision-free by construction.

Check the branch is free **before** dispatching:

```bash
git show-ref --verify --quiet "refs/heads/agent/dispatch-<name>" && echo TAKEN || echo FREE
```

`dispatch` derives `agent/dispatch-<name>` for the branch and `../<repo>-dispatch-<name>` for the worktree, and refuses on either collision — `worktree ... is already claimed` when the directory is live, `branch ... already exists` when a previous unit's worktree was removed but its branch survived.

**If the name is taken, default to a NEW name.** Never auto-remove anything. Three reasons, all verified:

- **"Pull latest" into the existing worktree is a category error.** A dispatch worktree is a fresh branch off `main` holding the *agent's* workspace — it is not a checkout of the PR. `/verify-pr` creates its own separate `../<repo>-pr-<n>` checkout for that. There is nothing in a dispatch worktree to pull, and pulling would move the ground under a possibly-running process.
- **"Remove and recreate" does not free the name.** `git worktree remove` keeps the branch, and so does `dot-agent-deck worktree reclaim` (PR #427). Since `dispatch` refuses while `agent/dispatch-<name>` exists, freeing a name takes a second step, `git branch -D`. A single `worktree remove` leaves you with the same refusal and one less workspace.
- **Removing a worktree can destroy the only copy of an unread review.** See step 7 — verdicts live in panes, not in files.

Only when the user **explicitly asks for a clean re-run**: do the two-step, in this order, and **show exactly what is about to be destroyed** first — the worktree path, the branch name, and `git log --oneline agent/dispatch-<name> ^origin/main` so any committed work in there is visible before it goes. Confirm the prior unit has finished and that its verdict has been read.

```bash
git worktree remove ../<repo>-dispatch-<name>   # worktree BEFORE branch
git branch -D agent/dispatch-<name>
git worktree prune
```

Worth knowing: **`dot-agent-deck worktree reclaim` will never clean these up.** Its gate returns `Verdict::Keep("no pull request found for this branch")` for a branch with no PR (`src/worktree_reclaim.rs:134`), and a dispatch branch never has one. So dispatch worktrees accumulate outside every existing cleanup path, and the pile is removed by hand or not at all. `git worktree list` is how you see how big it has got.

## Step 7 — No storage layer

**Do not have dispatched agents write verdicts to files.** This was considered and deliberately rejected.

The user watches the panes, so **the agent's final message is the report**. A file adds a path convention to agree on, a cleanup burden nothing owns, and a second copy that can disagree with the pane. (`/verify-pr` already writes its own report to `target/verify-pr/pr-<n>-report.md` in *its* checkout — that is its business, and this skill neither depends on it nor extends it.)

The consequence is accepted: a verdict exists **only in its pane**, so closing that tab or removing that worktree loses it. That is precisely why step 6 defaults to a new name instead of removing anything, and why the removal path insists the verdict has been read first.

## Step 8 — Report honestly

**`dispatch` is fire-and-forget. There is no return edge.** Results do not come back to this pane, and nothing here will ever notice a unit finishing.

Report, per dispatched unit: the PR number and title, the unit name, and the worktree path `../<repo>-dispatch-<name>`. Then point the user at **each unit's own tab on the deck** — that tab is where the verdict will appear.

Also report, plainly: any PR skipped at step 3 because it was no longer open, any PR excluded at step 1 and why, and the number dispatched against the number the user asked for if they differ.

**Never write anything that implies results will report back here** — no "I'll let you know when they finish", no "waiting for the verdicts", no summary table with an empty Verdict column waiting to be filled. There is no mechanism behind any of those sentences. If the user wants a consolidated view later, they ask each pane, or re-attach to it.
