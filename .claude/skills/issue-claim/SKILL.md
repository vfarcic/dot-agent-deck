---
name: issue-claim
description: 'Claim, honour and release the `in-progress` label that marks an issue as being worked in this repo. Claiming assigns the runner and adds the label, each read back; selecting skips every open issue that carries it and reports old claims as "claimed, possibly stale"; releasing removes it when the work stops without a PR, the issue is closed with evidence, the PR merges with `Refs`, or the PR closes unmerged. Use whenever you claim an issue for work (`/issue-queue` and `/prd-queue` before a dispatch, `/prd-start`), select issues to work, or handle the end of that work: a unit''s report or `ended … without reporting` notice in the dispatcher, and `/merge-pr` after a merge. Those skills point here instead of carrying their own copy.'
user-invocable: true
---

# Claim an issue, honour a claim, release it

This is where the claim on an issue is defined for every skill in this repo that takes an issue to work on. `/issue-queue` and `/prd-queue` (step 6 of each) claim before they dispatch and select around existing claims (their steps 2 and 3/4); `/prd-start` claims when a person or a unit starts a PRD; the dispatchers release when a unit's report arrives; `/merge-pr` releases after a merge. Each of those says *when* in its own flow to come here; *what* the claim is, and how to take, read and drop it, is here, so the copies cannot drift. Issue #2381 introduced it.

## Why a label, and not the assignee alone

**The assignee tells one account from another; it cannot tell two machines of one account apart.** The queues kept issues that were unassigned **or assigned to the runner**, so a deck on a second machine running as the same GitHub login read "assigned to me" as "mine, available" and dispatched the same issue again. The other in-flight checks do not close that gap: an open PR exists only once the unit opens one, and a unit's `agent/dispatch-*` branch reaches the remote only when the unit pushes, usually at PR time. For the first minutes to hours of every unit, nothing a second box could read said the issue was taken.

**The `in-progress` label is the marker, and it is excluded whoever the issue is assigned to.** The maintainer chose it on 2026-11-18 over two alternatives: pushing the unit's branch at dispatch time (branches are not to be pushed only to signal a claim to other agents) and a claim comment acting as the lock. The assignee stays part of the claim, because it is what the other maintainer's selection reads.

**Assignment was also never released**, which this skill now does: a unit that stopped without a PR left its issue assigned to the runner indefinitely (#1786 was still assigned on 2026-11-18, its work merged under `Refs`).

**The label is not the deck's own claim.** A scheduled issue dispatch (`src/issue_dispatch.rs`) claims with the `agent-deck/claimed` label and a claim comment, which the product writes and reads; this skill does not read or write that label, and the product does not read `in-progress`. [`docs/develop/work-identity.md`](../../../docs/develop/work-identity.md) lists both ledgers.

## Identity and the label

Resolve identity at runtime and pass `--repo` on every call, as the calling skill's step 1 does:

```bash
ME=$(gh api user --jq .login)
OWNER=$(gh repo view --json owner --jq .owner.login)
REPO=$(gh repo view --json name --jq .name)
```

The label was created on 2026-11-18 for issue #2381. If a fork or a new repository lacks it, create it once:

```bash
gh label list --repo "$OWNER/$REPO" --search in-progress --json name --jq '.[].name' | grep -qx in-progress \
  || gh label create in-progress --repo "$OWNER/$REPO" --color FBCA04 \
       --description "Claimed: a unit or a person is working this issue. See .claude/skills/issue-claim"
```

**Read the label from the issue's `labels`, not with `gh issue list --label in-progress`.** Measured on 2026-11-18: seconds after #2381 was labelled, `gh issue list --label in-progress` returned nothing while the same listing's `labels` field already showed the label. A filter that lags reports a fresh claim as absent, which is the one answer this skill exists to avoid.

## Claim

**Read the state immediately before the write, not from the selecting skill's listing**, write the assignee and the label in one edit, and read both back:

```bash
gh issue view <n> --repo "$OWNER/$REPO" --json state,assignees,labels \
  --jq '"\(.state) assignees=\([.assignees[].login]|join(",")) in-progress=\(any(.labels[]; .name=="in-progress"))"'
gh issue edit <n> --repo "$OWNER/$REPO" --add-assignee "$ME" --add-label in-progress
gh issue view <n> --repo "$OWNER/$REPO" --json state,assignees,labels \
  --jq '"\(.state) assignees=\([.assignees[].login]|join(",")) in-progress=\(any(.labels[]; .name=="in-progress"))"'
```

- **First read**: proceed only on `OPEN`, `assignees=` empty or exactly `$ME`, and `in-progress=false`. **Anything else is a collision: abort this candidate and report it**, do not resolve it. Never reassign an issue that already has someone else on it, and never remove a label someone else added in order to take the issue over. An `in-progress=true` on an issue assigned to `$ME` is the case this skill was written for: most likely a unit started from another machine running as the same account.
- **Second read**: `$ME` must appear in the assignees **and** `in-progress=true`. **Do not treat exit 0 as confirmation.** If either is missing, run the edit once more and read again; if it is still missing, do not start the work, release whatever half did take (below), and report it. Starting anyway runs the unit unclaimed for its whole life, which for a PRD can be hours.
- **An assignee other than `$ME` in the second read** is the cross-account race landing: report it as a collision, release your half, and do not dispatch.

**This narrows the race, it does not close it.** GitHub has no compare-and-swap for assignees or labels: adding either is additive, and adding a label the issue already carries succeeds without a change and records no `labeled` event (measured on #2381, 2026-11-18). Two machines of one account that both read `in-progress=false` and then both write will both read `in-progress=true` back, under the same login and with one `labeled` event between them, so neither can see from the issue that the other wrote too. The read right before the write shrinks that window from the minutes a selection pass takes to the time between two API calls; report a collision when you see one rather than treating the claim as a lock.

### A unit whose dispatcher already claimed

A dispatched unit does not claim its issue again: the dispatcher's claim covers it, and the label is already on the issue. A skill that claims as part of its own steps, such as `/prd-start` inside a `/prd-queue` unit, treats the label as its own when **both** hold: the issue is assigned to `$ME`, and the unit runs on the branch its dispatcher's name gives it (`git branch --show-current` reads `agent/dispatch-prd-<n>` or `agent/dispatch-issue-<n>`, optionally followed by `-<suffix>`, for this issue's `<n>`) or its task says the issue was claimed for it. Otherwise the label is someone else's claim: stop and tell the user.

## Select: skip every claimed issue, and report the old ones

**Exclude every open issue that carries `in-progress`, whoever it is assigned to.** The selecting skill's `jq` filter over its step-2 listing (`"$ISSUES"` in `/issue-queue`, `"$PRDS"` in `/prd-queue`) carries this line, after its assignee filter:

```jq
  | select((.labels|map(.name)|index("in-progress"))==null)
```

**Then list the claimed ones; they are not candidates, but each is reported.**

```bash
jq -r '.[] | select((.labels|map(.name)|index("in-progress")))
  | "\(.number)\t\([.assignees[].login]|join(","))\t\(.title)"' "$ISSUES"
```

For each, read when the label was added, from the issue's events (the `labeled` event), so the claim needs no comment of its own:

```bash
gh api "repos/$OWNER/$REPO/issues/<n>/events" --paginate \
  --jq '.[] | select(.event=="labeled" and .label.name=="in-progress") | .created_at' | tail -n 1
```

**A claim is "claimed, possibly stale" when all three hold:**

1. **No open PR works it**: none of the selecting skill's in-flight lists (`/issue-queue` steps 3a and 3b, `/prd-queue` steps 4a and 4b) names it.
2. **No live local unit works it**: `git worktree list --porcelain` names no `branch refs/heads/agent/dispatch-(issue|prd)-<n>` (optionally followed by `-<suffix>`) for it. This sees only this machine's units, which is what the third condition is for.
3. **The label was added more than 3 days ago.** A unit on another machine is invisible to the first two checks until it pushes, and a PRD unit can run for hours before it does; three days leaves room for a long PRD unit, and is short enough that a claim left by a machine that went away is surfaced within the week.

**Report each such row to the runner as `claimed, possibly stale`**, with the label's date, the assignees and what the three checks found, alongside the selecting skill's other exclusions. **The runner decides**; selection never takes a stale claim over silently and never drops one silently. If the runner says to take it, release it (below) and let the next selection pass pick it up as an ordinary candidate, claim included. Every other claimed issue is reported in one line with the in-flight exclusions (`claimed: #n, #m`), so the runner can see what the label held back.

**Issues assigned without the label, as every claim made before issue #2381 was, are ordinary candidates under the assignee rule as it already stood**: the queues keep an issue that is unassigned or assigned to the runner, so such an issue is a candidate for every machine of its assignee's account and stays out of the other maintainer's queue. Nothing releases those assignments automatically. A one-off cleanup is optional and the runner's call: `gh issue list --repo "$OWNER/$REPO" --state open --assignee "$ME" --limit 300 --json number,labels,title` lists them, and an issue with no open PR, no dispatch branch and no live unit can have `$ME` removed with `gh issue edit <n> --repo "$OWNER/$REPO" --remove-assignee "$ME"`.

## Release

**Remove the label and the runner's assignment together, and read it back:**

```bash
gh issue edit <n> --repo "$OWNER/$REPO" --remove-label in-progress --remove-assignee "$ME"
gh issue view <n> --repo "$OWNER/$REPO" --json state,assignees,labels \
  --jq '"\(.state) assignees=\([.assignees[].login]|join(",")) in-progress=\(any(.labels[]; .name=="in-progress"))"'
```

`in-progress=false` and no `$ME` in the assignees is a release; anything else, run it once more and report it if it persists. Removing a label the issue does not carry succeeds without a change (measured on 2026-11-18, with the label existing in the repository), so a release that someone else already did is harmless. **The assignment goes with the label** because it is the other half of the claim: an issue left assigned to the runner stays out of the other maintainer's queue, which is the "never released" half of issue #2381. `--remove-assignee "$ME"` removes only the runner; an assignment of another login is that person's, not the release's.

**When, and who does it.** Each moment belongs to the skill that is present at it:

| The work stopped because | Who releases | Where it says so |
| --- | --- | --- |
| a candidate was claimed and then not dispatched (a collision found later, a refused `dispatch`, the runner changed their mind) | the dispatcher, straight away | `/issue-queue` and `/prd-queue`, step 8's re-check before each dispatch |
| a unit's report arrives, or the deck delivers `Dispatched unit … ended (…) without reporting.` | the dispatcher, on that turn, by the rule below | `/issue-queue` and `/prd-queue`, step 9 |
| the unit's PR merged and the issue is still open, as it is after a PR that says `Refs #<n>` rather than `Closes #<n>` | whoever merged it | `/merge-pr`, after the read-back of the merge |
| the PR was closed unmerged | whoever closed it, unless the work goes on in another PR | here; no skill in this repo runs `gh pr close`, so for a PR a person closes, the dispatcher's rule below or the stale report above is the backstop |

**The dispatcher's rule on a report or an `ended` notice**: look the unit's PRs up by the branch its ledger recorded, `gh pr list --repo "$OWNER/$REPO" --head '<branch>' --state all --json number,state`. **An `OPEN` PR keeps the claim**: the work is in flight on it, and the merge or the close releases it. **In every other case, release**: no PR (the unit stopped without one, closed the issue with evidence, or crashed), a `MERGED` PR whose issue is still open, or a `CLOSED` one. An issue its PR closed needs nothing, because selection reads open issues only; releasing it anyway is harmless, and it keeps the label off the issue if it is ever reopened. For a unit dispatched for a duplicate or coupled pair, the dispatcher claimed each issue, so release each.

**`/merge-pr` finds the issues a merge may release from the head branch**, since a dispatched unit's branch is named for the issue it was cut for:

```bash
gh pr view <pr> --repo "$OWNER/$REPO" --json headRefName --jq .headRefName \
  | sed -nE 's#^agent/dispatch-(issue|prd)-([0-9]+)(-.*)?$#\2#p'
```

That reads the first number only, because a later one can be a date (`issue-<n>-<MMDD>`) rather than an issue; a second issue of a coupled pair comes from the unit's own task, when the unit is the one merging. For each issue found that is still `OPEN` and still carries `in-progress`, release it. A branch named some other way yields no number, and its claims wait for the dispatcher's rule or the stale report. That includes `/prd-start`'s `feature/prd-<n>-…` on purpose: a PRD worked by hand spans several PRs, each merged with `Refs`, and its claim is meant to outlive each of them.

## What this does not do

It does not lock anything: the read before the write narrows the race and does not close it. It does not see a unit on another machine except through the label, and it does not decide whether a stale claim's work is lost; the runner does. It does not change the product: no code under `src/` reads or writes the `in-progress` label (checked on 2026-11-18).
