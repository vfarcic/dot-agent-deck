# Reviewable change delivery: the path from a finished unit to a merge

This page is the evidence for PRD #626's discovery: the path a dispatched unit's work takes today from its completion to review, a pull request, a merge and cleanup; where context and evidence are lost on the way; an evidence model that separates what the deck observes from what an agent asserts and what something else verified; a review-packet prototype; a comparison of which Git and GitHub actions the deck should own; and a recommendation for the maintainer's `Go`, `Revise` or `Stop` decision.

**This page describes `origin/main` at `1fe600e6` (v0.49.3, 2026-10-30).** PRD #1666 part 3 is reworking report delivery and the evidence and attention surfaces (the inbox, report recovery, the desktop's agent overview and its data bridge). Findings in those areas are marked *(part 3)* and are the ones most likely to move when it merges. PRD #628's [work identity](work-identity.md) page inventories the identities this path relies on, and this page does not repeat it.

## How the evidence was gathered

- **Code**: the source at `1fe600e6`, read by area (dispatch and its seed, `work-done` and the return to the caller, `close`, `worktree list` and `reclaim`, the PR monitor and both clients' PR surfaces) and by skill (`pr-create`, `merge-pr`, `land-prs`, `issue-queue`, `prd-queue`, `pr-review-queue`, `verify-pr`, `tag-release`'s cleanup, the agent PR reviewer's workflows). Nothing in the tree was changed to gather it.
- **Pull requests**: the 200 most recent pull requests from `agent/*` branches (created 2026-10-04 to 2026-10-30; 197 merged, 3 closed unmerged), every one of them against `main`; and, for the 60 most recent merged ones (2026-10-25 to 2026-10-30), their reviews, commits and bodies.
- **Sampled branches**: eleven pull requests chosen to cover the PRD's cases, ten of them run through the prototype below, plus the maintainer's live worktrees on 2026-10-30 as `dot-agent-deck worktree list` reports them.
- **Who measured**: an agent, from the records alone. The PRD asks for the time and tool switches a *maintainer* needs, and for a prototype trial with five maintainers; neither has happened, and the recommendation says what that leaves open.

## Premises re-checked

The PRD was written on 2026-08-21. Against the code at `1fe600e6`:

| Premise | Today |
| --- | --- |
| The deck "owns no review or delivery state" | Still true of review and delivery decisions. The deck does *watch* delivery: a daemon monitor polls `gh pr list --head` for every live session on a non-default branch and both clients show a PR badge (number, state, checks, review decision), and the TUI's `o` and the desktop's badge open the PR. |
| Work-done reports carry summaries with no evidence contract | Still true. `work-done` takes one free-text report (up to 1 MiB) and a mode; it has no field for a branch, commit, PR, test run or verdict. Since PRD #1666 part 3a it carries a task id for delegated tasks, and a dispatched unit's report reaches its caller as an inbox message marked untrusted *(part 3)*. |
| PRDs anticipate returning PR links without defining review, readiness, approval or merge | Superseded outside the deck. The repository's skills now define them: `pr-create` (verified PR, findings answered and resolved), the agent PR reviewer (approval), `merge-pr` (overlap check, pinned merge, watching `main`), `land-prs` (ordering, conflicts, stale approvals) and `tag-release`'s cleanup. None of the review, approval or merge steps is deck code. |
| Dispatch "can tell agents to open PRs" | The deck's own seeds do not ask for a PR. The skills that compose dispatch tasks (`issue-queue`, `prd-queue`) do. Scheduled issue dispatch passes the user's prompt template through and adds nothing about PRs or `work-done`. |
| #220, #236 and #425 are dependencies to build on | All three closed: dispatch with a return edge to the caller (#220, PRD #1589's `dot-agent-deck close`), reclaiming merged worktrees and never force-removing a dirty one (#236), and the ownership marker (#425). |
| #628 may supply durable work identity | It closed with **Revise**: a durable execution journal (#1889) and interrupted-work recovery (#1890), sequenced after part 3. No dependency graph. |
| #468 (placement) and #174 (cross-project results) | Both still open. |

## The path today

| Stage | What does it | What it produces | What is lost here |
| --- | --- | --- | --- |
| 1. Unit finishes | The unit, told by its seed to write a report file and run `work-done --done --task-file`. The deck's seed asks for "what you did, what the outcome was, and anything they have to act on" and nothing more structured. | A free-text report. | Anything the task did not ask for by name. The deck's seed asks for no branch, PR, tests or verdict; the skills' task text adds those, so their presence depends on which skill composed the task. |
| 2. Report reaches the caller | The daemon posts it to the dispatching agent's inbox as untrusted text, inlined up to 4000 characters, with the full text saved to a file in the unit's worktree when it is longer, and also kept in the daemon's retained report store *(part 3)*. | One inbox message. | The full report lives in the worktree that cleanup later removes. If the caller is gone, the report goes to the deck's recovery notices rather than to the work that asked for it *(part 3)*. A unit that exits without `--done` delivers nothing. |
| 3. Caller acts on it | The dispatching agent, under a skill. `issue-queue` tells it to verify the report's claims with `gh` rather than relay them; `prd-queue` and `pr-review-queue` still say nothing reports back to the dispatching pane, which has been false since the return edge shipped (#1081, 2026-09-15). | Whatever that agent does next. | Nothing records which claims were checked. |
| 4. PR opened | The unit, under `pr-create`: run the change's gates, push, open the PR with the tests named, wait for CI and the review apps, answer and resolve every finding. Median time from the first commit to the PR: 3 minutes. | The PR: its body carries the agent's evidence as prose. | The requirement link is a closing keyword in 34 of 60 PR bodies; 22 more mention an issue only in prose, and 4 name none. GitHub's `gh pr view` returns at most 100 files and 100 commits, so a tool that trusts it under-reports a large PR (#1898 changed 201 files in 132 commits). |
| 5. Automated review | Qodo on every push, Greptile once when the PR opens, then the agent PR reviewer (twice a day, or on dispatch). The reviewer reads the diff and the review apps' findings, takes CI as given, and casts `APPROVE`, `REQUEST_CHANGES` or `INSUFFICIENT`. A change to a deny-listed path gets a `needs-human-eye` label. | Inline threads, one verdict comment, a vote. | The reviewer does not run anything; "the verdict is the only artifact". A push after an approval dismisses it: 8 of 60 merged PRs lost an approval that way and needed another round. |
| 6. Merge | A person, or a skill acting for them (`merge-pr`, `land-prs`, an `issue-queue` bug-fix unit): approval for this head, required checks green, the overlap check against `main`, `gh pr merge --squash --match-head-commit`, then watching `main`'s push run for up to 45 minutes. | A squash commit on `main`. | The overlap check catches only files the PR touches. Ordering across several PRs is decided by `land-prs` at release time, not when the units were dispatched. |
| 7. Cleanup | `dot-agent-deck close` (refuses a unit that has not reported unless forced; removes the worktree under the policy recorded when it was created, which keeps a dirty dispatch worktree but force-removes a scheduled issue-dispatch one, dirty or not; keeps the branch); `dot-agent-deck worktree reclaim` (removes a worktree only when its PR is merged and its tree is clean, and without `--yes` only one the deck created; keeps the branch); `tag-release`'s cleanup (branches and worktrees after the user confirms). | A removed worktree. | Neither deck command checks unpushed commits. `reclaim` keys on the branch name, so a reused branch with two merged PRs reads `unresolvable` and is kept forever (`agent/dispatch-issue-1845`, PRs #1856 and #1923). A worktree with no PR is kept with "no pull request found", which says nothing about whether the work is in flight or abandoned. |

**Measured on the 197 merged PRs**: median 4.2 hours from opening to merge (quartiles 1.1 and 9.9 hours). On the 60-PR sample: median 1.0 hour from opening to the last approval, and 9 minutes from that approval to the merge; 3 commits, 4 files and 200 changed lines per PR at the median; 57 of 60 bodies name the tests that were run. No merged PR in the sample carried a `CHANGES_REQUESTED`, which is survivorship: a PR that met one was fixed before it merged.

**Tool surfaces a reviewer touches today** to decide on one unit, counted from the stages above rather than timed: the deck (the report in the inbox, the PR badge), the PR page (body, checks, Qodo's comment, the reviewer's verdict, the inline threads), the CI run when a check is red, and the worktree for anything not pushed. That is four surfaces, and the PR page is the one that carries nearly all of the evidence.

## Sampled branches

The PRD's protocol asks for at least eight completed branches across at least three task types, covering clean success, verification failure, stale base, dependent branch and abandoned work. All eleven are in this repository, so the "three repositories" half is not met; they span five task types. Ten went through the prototype generator; #1561 was read from its record only.

| PR | Task type | Case | What the record shows |
| --- | --- | --- | --- |
| #2027 | test fix | clean success | 1 file, no threads, approved on the first round. No issue linked. |
| #1996 | landing page (#1995) | verification failure | 7 review threads; two approvals dismissed by later pushes before the third stood. |
| #1925 | PRD #91 | review rounds, risky paths | Touches `tests/common/mod.rs`; cross-version job green; two approvals dismissed. |
| #1969 | flaky test (#1960) | stale base | Two merges from `main` into the branch before it landed. 22 of the 60 sampled PRs merged `main` in at least once. |
| #2021 | follow-up series for #1981 | dependent branches | One of ten branches for one issue in two days, each landing in turn on `main`; the dependency is carried by ordering and prose, not by a stacked PR. Each of the 300 most recent agent PRs targets `main`. |
| #1561 | PRD #1542 | dependent branch, abandoned | Closed unmerged: it conflicted with `main` in about 30 files and with a parallel PR (#1617) in 33. The overlap was found by a person five days after the PR opened; the branch was kept as a reference for its replacement. |
| #1855, #1807 | PRD discovery docs | abandoned after approval | Both approved, CI green, then closed by product decision. Readiness evidence was complete; the disposition was not a review question. |
| #1856, #1923 | two fixes for #1845 | branch reuse | Two attempts on one branch name, both merged. `worktree reclaim` keeps the leftover worktree indefinitely, and it is dirty with a scratch file. |
| #1898 | PRD #1666 part 3a | large change | 201 files, about 56,400 changed lines, 8 threads, cross-version green, approved. The PR body names no test command. |

## Evidence model

Each field is classed by who stands behind it: **observed** (the deck or a tool reads it directly from Git, the forge or its own state), **asserted** (an agent says so), or **verified** (something other than the author re-checked it). The last column is where the field lives today.

| Field | Class | Where it lives today |
| --- | --- | --- |
| Requirement (issue or PRD) | observed when linked by a closing keyword; asserted otherwise | the PR body; the branch name by convention (`agent/dispatch-issue-<n>`) |
| Branch, head commit, base | observed | Git, the PR |
| Changed files, size, diff | observed | Git, the PR (capped at 100 by `gh pr view`) |
| Risky paths (protocol, daemon, hooks, harness, workflows) | observed, by path rule | the reviewer's deny list and the PR author's rule-12 answer |
| Stale base / overlap with `main` | observed | computed by `merge-pr` at merge time only |
| Commands the author ran and their result | asserted | the PR body and the unit's report |
| Lane-2 (real-agent) tests | asserted; no CI job runs them, so only someone re-running them locally can verify them | the PR body |
| Reds met and how each was handled | asserted | the PR body |
| Required checks | verified | CI |
| Cross-version contract | verified when the `cross-version` job ran; asserted otherwise | CI, the PR body |
| Review findings and their resolution | observed (threads are resolved or not); the fix itself is asserted | the PR's threads |
| Approval | verified (by the agent reviewer, which reads but does not run) | the PR's reviews |
| Interventions (who stepped in, when) | not recorded | — |
| Unresolved questions and needs-input | asserted, in prose | the report, a notification |
| Disposition (merged, closed, replaced, kept for reference) | observed for merged and closed; the reason is asserted | the PR, its closing comment |
| Worktree state (dirty, owned, reclaimable) | observed | `dot-agent-deck worktree list` |

Two findings from the model. **The PR already is the review packet for this repository**: most observed and verified fields are on the PR page (overlap with `main` and worktree state are not), and the asserted ones are in its body, written to a convention the skills enforce. **What no surface does is mark the class**: a body line saying a test passed and a check that passed sit on the same page with nothing distinguishing them, and the deck's own report channel has no structure for either.

## Prototype

**Scratch packet generator.** A read-only script assembled a packet per PR from Git and the forge alone, tagging each field with its class (kept outside the repository, in the main checkout's ignored `.dot-agent-deck/prd-626-prototype/`). It produced a packet for each of the ten sampled PRs other than #1561; what it showed:

- The packet's observed and verified fields came from Git and the forge without the agent's help, in two `gh` calls and one `git log` per PR.
- The asserted fields could only be scraped from prose. A pattern for test commands found them in 9 of 10 bodies; a pattern for declared gaps ("UNVERIFIED", "not run", "skipped") matched in 9 of 10 too, mostly on boilerplate, so it carries no signal. An agent-declared field needs a structure, not a scraper.
- `gh pr view` truncated #1898 to 100 files and 100 commits without saying so; a packet has to page through the REST endpoints, as `merge-pr`'s overlap check already does.
- The requirement was missing from six of ten packets: five of those PRs say `Refs` rather than `Closes`, and one names no issue.

**TUI review and external-tool handoff, as a design.** It was not built as code. The surfaces it belongs on are reserved for PRD #1666 part 3: the desktop already has *Diff* and *Checks* tabs on an agent's tile (added with the desktop prototype in #416) that show "Diff data is not exposed by the daemon" and "Check results are not exposed by the daemon" in live mode, and their data comes through the desktop's bridge and types, which part 3 is changing; the report itself arrives through the inbox. A TUI-only module would also break the parity rule (CLAUDE.md rule 22). The smallest design that fits:

```text
┌ review: fix-auth (agent/dispatch-issue-1845) ─────────── PR #1923 MERGED ┐
│ requirement  #1845 (Refs only — not linked)                  [asserted] │
│ change       3 files  +113 −6   1 commit   base: 0 behind               │
│ risk paths   none                                                        │
│ checks       23 passed · 4 skipped · cross-version skipped  [verified]   │
│ review       approved at head · 1 thread, 0 open            [verified]   │
│ author ran   cargo test-fast; affected-checks               [asserted]   │
│ worktree     dirty (1 untracked) · owned · reclaim: keep                 │
├──────────────────────────────────────────────────────────────────────────┤
│ o open PR   d open diff in your tool   e open worktree in editor   c copy │
└──────────────────────────────────────────────────────────────────────────┘
```

- It is read-only, per unit, and opened from the unit's card. The deck draws no diff: `d` runs the user's configured diff tool (`git difftool`, or a command from configuration) against the merge base, and `e` opens the worktree in the user's editor. `c` copies the packet as Markdown for an issue or a chat.
- Its data is the daemon's existing PR monitor plus a per-worktree `git` read, which the daemon does not do today (the desktop's `DeckSnapshot.branch` is fixture-only for that reason). Exposing it to both clients is an additive optional field or a capability-gated request (CLAUDE.md rule 18).
- In the desktop, it fills the existing *Diff* and *Checks* tabs, and each action needs a voice command (CLAUDE.md rule 23).
- Asserted fields come from a structured section of the `work-done` report, not from the PR body.

## Action boundaries

Which Git and GitHub actions the deck should perform itself, judged on what fails, how it is recovered, and who does it today.

| Action | Who does it today | Failure and recovery | Recommendation |
| --- | --- | --- | --- |
| Show delivery state (PR, checks, review, threads, worktree) | the deck shows PR, checks and review decision; threads and worktree state only on GitHub and the CLI | read-only; a wrong answer is stale, not destructive | **deck** — extend what exists |
| Open the PR, a diff tool or an editor | the deck opens the PR | none beyond a missing tool | **deck** — the handoff |
| Commit | the agent | an agent that commits generated files or secrets is caught only in review | **agent**; the deck may warn on a dirty worktree at completion |
| Push, open a PR | the agent, under `pr-create` | auth, a rejected push, a pre-existing PR; the agent recovers in place | **agent / skill** — the deck uses the user's `gh` to read PRs and post issue claims, and has no need to push |
| Rebase or update a stale branch | `merge-pr` / `land-prs` (`gh pr update-branch`, merge commits, never a force-push) | conflicts need a person or an agent; an update-branch merge keeps the approval, a pushed commit dismisses it | **skill** |
| Merge | a person, or a skill acting for them, after an approval another actor cast | protected `main`; `--match-head-commit` stops a merge of an unreviewed head; a red `main` is watched afterwards | **forge and skill**; the deck stays out (PRD out of scope) |
| Order dependent branches | `land-prs` at release time; a person otherwise | late discovery: #1561 was abandoned over an overlap found five days in | **open question** — the one integration gap the sample shows |
| Remove a worktree | `close`, `worktree reclaim`, `tag-release` | `reclaim` keeps every dirty tree and removes a merged, clean one (one the deck did not create only with `--yes`); `close` keeps a dirty dispatch worktree but force-removes a scheduled issue-dispatch one, so uncommitted work there is lost; `close` and `reclaim` keep the branch | **deck** — exists; add an unpushed-commits check and handle a reused branch |
| Delete a branch | `tag-release`'s cleanup, after confirmation | a deleted unmerged branch loses work; the skill offers only branches reachable from `main` or equal to a merged PR's head | **skill** |

## Recommendation: Revise

**Recommended: `Revise` — limit the feature to evidence and delivery visibility with an external-tool handoff; no native Git delivery and no diff viewer in the deck.** This is a recommendation for the maintainer, who makes the decision.

- **Why not `Go`.** The `Go` bar is a measured cut of at least 30 percent in a maintainer's median decision time, with four of five reviewers reaching the right disposition. Neither was measured, and the sample does not suggest native delivery would help: in this repository the PR page already carries the evidence, the skills own push, PR creation, review, merge and cleanup with explicit failure handling, and an approved PR merges in 9 minutes at the median. The deck taking over commit, push or merge would duplicate that and add a forge credential and failure model it does not have.
- **Why not `Stop`.** Current tools do not give equivalent context at negligible cost, in three places the sample shows: no surface a reviewer reads marks which statements are an agent's claim and which a verified result, beyond the author's own wording; the report the caller gets is free text in the inbox while the evidence is on GitHub, so the caller re-derives it with `gh`; and worktrees whose state the CLI cannot explain (no PR, or two PRs on one branch) accumulate. All three are inside the deck's operational focus, and none needs an editor.
- **What the PRD's protocol leaves open.** The maintainer timing study and the five-reviewer trial have not run. The sample is one repository, whose skills supply most of today's evidence, so a repository without them would see far less; that argues for the deck carrying the evidence structure rather than leaving it to each repository's conventions.

Candidate follow-up PRDs for a `Revise`, for the maintainer to accept or drop; none has been created:

1. **Structured completion evidence**: an optional structured section in `work-done` (requirement, branch and head, commands run with their outcomes, gaps declared, open questions), carried to the caller with its class, sequenced after part 3 and #1889's journal.
2. **Delivery status in both clients**: the read-only packet above, from the PR monitor plus a per-worktree read, filling the desktop's *Diff* and *Checks* tabs and a TUI view, with the handoff actions and voice commands.
3. **Worktree disposition**: `worktree list` and `close` say whether a kept worktree has unpushed commits, and resolve a reused branch from its PRs' head commits rather than refusing.

Dependency ordering between parallel branches is the one integration problem the sample shows, and it is left out of these on purpose: one abandoned PR among the 200 sampled does not yet justify an integration queue.

## Incidental findings

- `prd-queue` and `pr-review-queue` still tell their dispatching agent that nothing reports back, which has been false since #1081.
- The return-edge section of [`dispatcher-mode.md`](dispatcher-mode.md) describes the report as a submitted pane turn; at `1fe600e6` it is an inbox message, and the one-line turn's composer is reached only from tests *(part 3)*.
- `prd-queue`'s single-agent task says the unit may arm auto-merge, while `pr-create` defaults to not arming it.
