# PRD #629: Dependency-aware execution policy

**Status**: Candidate (Discovery)
**Issue**: [#629](https://github.com/vfarcic/dot-agent-deck/issues/629)
**Priority**: Medium
**Created**: 2026-08-21
**Builds on**: [#58](https://github.com/vfarcic/dot-agent-deck/issues/58) (multi-role orchestration)
**Interacts with**: [#628](https://github.com/vfarcic/dot-agent-deck/issues/628) (durable work graph), [#174](https://github.com/vfarcic/dot-agent-deck/issues/174) (cross-project DAG), [#220](https://github.com/vfarcic/dot-agent-deck/issues/220) (dispatch), [#634](https://github.com/vfarcic/dot-agent-deck/issues/634) (agent authority and overrides), [#635](https://github.com/vfarcic/dot-agent-deck/issues/635) (execution budgets)

## Opportunity

dot-agent-deck makes parallel execution and orchestration possible, but the system does not decide when parallelism is safe or worthwhile. Dependencies, likely file conflicts, stale bases, landing order, and concurrency are currently encoded in prompts, individual commands, or not at all.

Parallelism is not universally beneficial. [Anthropic's multi-agent research](https://www.anthropic.com/engineering/built-multi-agent-research-system) reports substantial coordination and token overhead and notes that coding tasks with shared context and dependencies are less naturally parallel than breadth-first research. A deterministic execution policy could recommend serialization and make integration safer, but an overambitious scheduler could duplicate agent reasoning and create opaque behavior.

## Target Users And Jobs

- A developer deciding whether several tasks can run concurrently without creating avoidable review or merge debt.
- An orchestrator dispatching dependent work in a predictable order.
- A maintainer integrating several branches that may share files, APIs, generated artifacts, or a moving base.
- A cross-project workflow that needs cycle, readiness, and dependency semantics beyond prompt prose.

## Current Evidence

- PRD #58 enables role fan-out but leaves planning decisions primarily to the orchestrator prompt.
- PRD #174 explicitly models cross-project work as a DAG and defers deeper cycle and nesting policy.
- PRD #220 deliberately avoids making the dispatcher a general planner.
- Worktree isolation prevents direct working-copy collisions but does not prevent conflicting branches, duplicated work, stale assumptions, or an unsafe landing order.
- Review and integration remain serial human bottlenecks after execution is parallelized, a hypothesis this discovery will test on actual deck workflows.

## Hypotheses

- The deck should enforce declared dependency invariants deterministically while agents propose plans and decomposition.
- The system should sometimes recommend or require serial execution when dependencies or overlap make parallelism wasteful.
- Initial conflict signals can combine explicit dependencies, repository paths, changed-file overlap, and stale-base state without claiming semantic certainty.
- Scheduling and integration should expose the reason for every defer, serialization, or readiness decision.
- The discovery can proceed against current dispatch and orchestration identities while informing whether #628 needs a general work graph.

## Questions To Answer

- Which decisions belong to deterministic deck policy and which should remain agent or human judgment?
- How are dependencies declared, discovered, changed, and validated?
- Which cycles and nested-dispatch patterns can be prevented mechanically?
- Can file overlap, changed interfaces, and stale-base signals predict enough integration risk to influence scheduling?
- When should a dependency block spawn, block delivery, or only block integration?
- How are ready, blocked, superseded, and invalidated dependency states explained to users and agents?
- What override and approval model keeps policy useful without becoming obstructive?
- Which identity does a dependency edge connect before #628 is decided?

## Discovery Evidence Protocol

- Sample at least twelve real or reconstructed task sets containing independent work, direct dependencies, shared-file conflicts, stale bases, nested dispatch, and intentionally serial work.
- Record the current execution order, conflicts, rework, review delay, and whether a maintainer could have identified the risk before spawn or only before integration.
- Compare a minimal explicit-DAG policy and a signal-assisted recommendation model against the actual outcomes without using future information unavailable at decision time.
- Treat the candidate as `Go` only if deterministic policy prevents or identifies at least three observed conflict or rework cases without incorrectly blocking more than one clearly independent task set.
- Choose `Revise` toward advisory warnings if hard blocking has unacceptable false positives, and choose `Stop` if prompt-level planning and Git already expose the relevant decisions reliably.

## Discovery Milestones

- [ ] Collect representative task graphs and failure cases involving unnecessary parallelism, dependency waits, stale assumptions, duplicated work, and integration conflicts.
- [ ] Define the smallest dependency vocabulary that can operate across current orchestration, dispatch, and cross-project concepts without assuming #628's outcome.
- [ ] Separate enforceable invariants, configurable policy, advisory signals, and agent judgment, with an explanation contract for each decision.
- [ ] Simulate or prototype policy against the sampled serial, parallel, nested, stale-base, and conflicting task sets.
- [ ] Record a `Go`, `Revise`, or `Stop` decision and split accepted dependency, scheduling, conflict, and integration behavior into implementation PRDs.

## Evidence And Decision Criteria

A `Go` decision requires observed cases where dependency policy prevents measurable conflict or rework, understandable explanations, acceptable false-positive behavior, and an identity model that works before or independently of a universal work graph.

## Non-Goals

- Maximizing concurrent agent count.
- Replacing agent reasoning with a universal task planner.
- Enforcing token, monetary, attempt, or elapsed-time budgets, which belong to #635.
- Building a cluster scheduler or enterprise quota system.
- Automatically merging branches as part of execution scheduling.

## Risks And Dependencies

- Incorrect dependency state can block useful work or create false confidence about safe parallelism.
- File overlap is only a proxy and can miss semantic conflicts across different files.
- Cross-project cycles and nested dispatch require stable identity across current and future work models.
- Integration behavior must preserve dirty worktrees and existing lifecycle safety guarantees.

## Candidate Outcomes

- **Go**: Define an explainable dependency-policy core and create incremental implementation PRDs for declaration, scheduling, conflict signals, and integration ordering.
- **Revise**: Ship advisory dependency and conflict visibility before enforcing execution order.
- **Stop**: Keep planning in orchestrator prompts and rely on Git plus human integration judgment.
