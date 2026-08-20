# PRD #626: Reviewable change delivery

**Status**: Candidate (Discovery)
**Issue**: [#626](https://github.com/vfarcic/dot-agent-deck/issues/626)
**Priority**: Medium
**Created**: 2026-08-21
**Builds on**: [#220](https://github.com/vfarcic/dot-agent-deck/issues/220) (worktree dispatch)
**Interacts with**: [#628](https://github.com/vfarcic/dot-agent-deck/issues/628) (durable work identity), [#236](https://github.com/vfarcic/dot-agent-deck/issues/236) (worktree removal and reclamation), [#425](https://github.com/vfarcic/dot-agent-deck/issues/425) (worktree ownership), [#468](https://github.com/vfarcic/dot-agent-deck/issues/468) (dispatch placement), [#174](https://github.com/vfarcic/dot-agent-deck/issues/174) (cross-project results)

## Opportunity

dot-agent-deck can spawn, coordinate, and monitor coding agents and isolate working-copy changes through worktrees, but it stops before the highest-friction part of parallel work: understanding what changed, verifying that it satisfies the request, reviewing risk, and safely turning branches into pull requests and merged changes. Users leave the deck and reconstruct this context through Git, editors, test logs, and issue trackers.

Direct competitors such as [Conductor](https://www.conductor.build/), [dmux](https://github.com/standardagents/dmux), [Claude Squad](https://github.com/smtg-ai/claude-squad), [Cursor](https://cursor.com/blog/cursor-3), and [GitHub agents](https://github.com/features/copilot/agents) offer parts of the diff-to-PR loop. Rebuilding an IDE would enter a crowded market and dilute the deck's operational focus. The candidate opportunity is a narrower, evidence-first delivery workflow that makes agent work reviewable and integrable while handing rich editing to existing tools.

## Target Users And Jobs

- A maintainer reviewing several agent branches without losing the original requirement or verification context.
- A developer deciding whether a completed worktree is ready to commit, push, open as a PR, retry, or discard.
- An orchestrator collecting child results and determining a safe integration order.
- A reviewer needing reproducible evidence rather than an agent's prose claim that tests passed.

## Current Evidence

- Dispatch and issue-dispatch already create worktrees and can instruct agents to open PRs, but the deck does not own review or delivery state.
- Work-done reports and orchestration context carry summaries without a general artifact or evidence contract.
- Existing PRDs anticipate returning PR links but do not define diff inspection, verification readiness, approval, or merge behavior.
- Prior market research and practitioner reports identify review and branch integration as limiting resources after execution is parallelized; this claim remains a hypothesis to validate against actual deck workflows below.
- The repository's own verification discipline demonstrates the value of explicit test commands, review findings, recordings, and merge gates.

## Hypotheses

- The deck should produce a review packet rather than a full editor.
- A review packet should connect requirements, changed files, diff summary, commands run, test artifacts, risks, unresolved questions, interventions, and the resulting commit or PR.
- `Ready for review` should be a distinct state with machine-checkable evidence, not an alias for process exit or agent self-report.
- Initial Git operations should be local and explicit; automatic merge should remain out of scope until integration policy is proven.
- Branch integration needs dependency, stale-base, and post-integration verification semantics rather than only a merge button.

## Questions To Answer

- What minimum evidence lets a reviewer decide without replaying the whole session?
- Which evidence can the deck observe directly, which must an agent declare, and which must be independently rerun?
- Should the TUI render a focused file/diff summary or hand off immediately to existing diff tools?
- What Git actions are safe and valuable: commit, push, PR creation, rebase, merge queue, archive, or cleanup?
- How are uncommitted changes, dirty worktrees, generated files, secrets, binary changes, and large diffs handled?
- What does readiness mean across different languages, repositories, and verification commands?
- How should branch dependencies and post-merge verification work when several agents target the same base?

## Discovery Evidence Protocol

- Sample at least eight completed agent branches across at least three repositories or task types, including clean success, verification failure, stale base, dependent branch, and abandoned work.
- Measure the current time and tool switches required for a maintainer to reconstruct intent, inspect risk, verify claims, and choose the next Git action.
- Test a review-packet and external-tool-handoff prototype with at least five maintainers using the same sampled branches.
- Treat the concept as a `Go` candidate only if the prototype reduces median decision time by at least 30 percent and four of five reviewers reach the correct disposition without a critical evidence omission.
- Choose `Revise` toward evidence export only if existing diff tools remain clearly superior, and choose `Stop` if current Git and editor workflows provide equivalent context with negligible reconstruction cost.

## Discovery Milestones

- [ ] Map the current path from dispatch completion to review, PR, merge, and cleanup, including where context or evidence is lost.
- [ ] Define a provider-neutral evidence model and classify each field as observed, agent-asserted, or independently verified.
- [ ] Prototype the smallest TUI review and external-tool handoff that can handle changed files, verification results, and risk without embedding an editor.
- [ ] Compare safe Git and GitHub action boundaries, including failure recovery, stale bases, dependent branches, and worktree retention.
- [ ] Record a `Go`, `Revise`, or `Stop` decision and split accepted work into evidence, review-surface, PR-delivery, and integration implementation PRDs as needed.

## Evidence And Decision Criteria

A `Go` decision requires a review packet that measurably reduces context reconstruction, a credible distinction between assertions and verified evidence, a bounded non-IDE interface, and Git operations whose failure and recovery semantics are explicit.

## Non-Goals

- Building a general-purpose code editor or replacing users' preferred diff tools.
- Treating agent-written summaries as independent verification.
- Automatically merging changes before dependency, conflict, approval, and rollback policy is understood.
- Solving hosted CI, code review, or source control for every provider in the first implementation.

## Risks And Dependencies

- The feature can become an incomplete IDE if its boundary is not enforced.
- Running verification can be expensive, destructive, environment-specific, or unsafe without repository policy.
- Git actions against dirty or foreign worktrees depend on ownership and retention guarantees from #236 and #425.
- Durable evidence and retries likely depend on the work identity explored in #628.

## Candidate Outcomes

- **Go**: Adopt an evidence-first delivery model and create smaller implementation PRDs for readiness, review, PR creation, and integration.
- **Revise**: Limit the feature to review packets and external-tool handoff if native Git delivery adds too much risk.
- **Stop**: Keep delivery outside the deck and document the integration conventions users should follow instead.
