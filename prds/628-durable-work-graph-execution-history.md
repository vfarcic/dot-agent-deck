# PRD #628: Durable work graph and execution history

**Status**: Candidate (Discovery)
**Issue**: [#628](https://github.com/vfarcic/dot-agent-deck/issues/628)
**Priority**: High
**Created**: 2026-08-21
**Builds on**: [#120](https://github.com/vfarcic/dot-agent-deck/issues/120) (scheduled issue dispatch), [#140](https://github.com/vfarcic/dot-agent-deck/issues/140) (orchestration instance identity), [#220](https://github.com/vfarcic/dot-agent-deck/issues/220) (worktree dispatch)
**Interacts with**: [#174](https://github.com/vfarcic/dot-agent-deck/issues/174) (cross-project dispatch), [#468](https://github.com/vfarcic/dot-agent-deck/issues/468) (dispatch placement and spawning), [#236](https://github.com/vfarcic/dot-agent-deck/issues/236) (worktree lifecycle)

## Opportunity

The deck currently tracks panes, agent records, orchestration instances, delegation commissions, dispatch names, worktrees, branches, schedules, and issue claims through related but separate identities. A user can see live execution, but there is no durable parent record answering what work was requested, which executions attempted it, where its artifacts live, how it depends on other work, and what ultimately happened.

A durable work graph could support recovery, review evidence, cost attribution, dependency-aware dispatch, integration queues, and team handoff. It could also become an expensive abstraction that duplicates GitHub issues, Git branches, or agent-native session history. This discovery exists to determine whether the abstraction earns its cost and where its authority should stop, while related vertical discoveries proceed against existing identities and report what they actually need.

## Target Users And Jobs

- A developer returning after detaching or restarting the daemon who needs to reconstruct active and completed work.
- An orchestrator or dispatcher creating child work and waiting for durable outcomes.
- A reviewer tracing a change back to its request, attempts, environment, and evidence.
- A future remote or team client that cannot infer work identity from one TUI process's in-memory state.

## Current Evidence

- PRD #140 solved routing identity for concurrent orchestration instances, but routing identity is narrower than product work identity.
- PRD #120 uses issue claims, worktrees, and PR presence as an operational ledger without defining a general work object.
- PRD #220 and PRD #468 expose the distinction between placement, execution, and continuation.
- Worktree ownership and reclamation remain separate from the execution record that caused a worktree to exist.
- Products such as [Gas Town](https://github.com/gastownhall/gastown), GitHub agents, and hosted coding-agent systems make task identity and durable history central, but often with significantly more operational complexity.

## Hypotheses

- A work item should be the durable identity above sessions, panes, retries, and worktrees.
- Work items should form a directed graph with explicit parent, child, and dependency relationships rather than relying on prompt prose.
- Runtime state should survive daemon restart, while repository-portable workflow definitions may need a different storage boundary.
- GitHub issues and PRs should be linked external records, not the only source of work identity.
- A minimal append-only event history may be safer than persisting every mutable daemon structure.

## Questions To Answer

- What user-visible failures are caused by the absence of durable work identity today?
- Which fields and transitions are universal across manual panes, orchestrations, dispatches, and schedules?
- Is a work item authoritative, observational, or a projection over existing Git and issue-tracker objects?
- What is the minimum viable relationship model: parent-child, dependency, attempt, continuation, and replacement?
- Which state must survive daemon restart, machine restart, or movement to another host?
- Should local runtime state live in SQLite, an event journal, Git metadata, repository files, or a combination?
- How are deletion, retention, redaction, and backward compatibility handled?
- Can the abstraction be introduced without changing the TUI-to-daemon contract incompatibly?

## Discovery Evidence Protocol

- Sample at least eight completed or interrupted runs covering manual panes, orchestration, interactive dispatch, and scheduled issue dispatch, with at least one daemon or client interruption.
- Ask a maintainer to reconstruct request, attempts, current owner, workspace, result, and next action using current records; record missing or contradictory answers and time-to-reconstruction.
- Treat a general work identity as a `Go` candidate only if at least three distinct launch paths share unresolved identity needs and one minimal model answers them without replacing Git or issue-tracker authority.
- Prefer `Revise` to a narrower execution journal if history solves the observed failures without a graph, and choose `Stop` if existing Git, issue, and session records answer all sampled workflows reliably.
- Require vertical discoveries such as #626 and #633 to state their actual identity needs rather than accepting #628 as a prerequisite by assumption.

## Discovery Milestones

- [ ] Inventory every existing identity, lifecycle map, persisted file, and external record involved in manual panes, orchestration, dispatch, scheduling, and worktree cleanup.
- [ ] Document concrete recovery, attribution, and handoff scenarios that the current model cannot answer reliably.
- [ ] Compare a minimal work-item model against alternatives that reuse Git branches, GitHub issues, or an append-only event log without a new top-level object.
- [ ] Prototype the smallest persistence and migration approach needed to validate restart recovery and parent-child history without committing to a user interface.
- [ ] Record a `Go`, `Revise`, or `Stop` decision, including the proposed authority boundary and follow-up implementation PRDs.

## Evidence And Decision Criteria

A `Go` decision requires demonstrated user workflows that cannot be solved cleanly by existing identities, a minimal state model shared by at least three launch paths, a credible persistence and migration approach, and a clear rule for how Git and issue trackers relate to the work record.

## Non-Goals

- Designing the final review, cost, team, or mobile user interfaces.
- Replacing Git branches, worktrees, or issue trackers.
- Persisting raw prompts, credentials, or complete terminal histories by default without an explicit privacy decision.
- Implementing a general project-management system.

## Risks And Dependencies

- A broad work object can become a second source of truth that disagrees with GitHub or Git.
- Persistence creates schema migration, retention, privacy, and cross-version obligations.
- Incorrect parent or dependency identity can misroute automated actions rather than merely display stale information.
- Existing correctness work in #236, #425, #545, and #555 must not be deferred behind a new abstraction.

## Candidate Outcomes

- **Go**: Define the minimal durable model and split storage, protocol, migration, and user-surface work into implementation PRDs.
- **Revise**: Limit the effort to a durable execution journal or dispatch registry if a general work graph is not justified.
- **Stop**: Keep existing identities and document why Git and issue-tracker records are sufficient.
