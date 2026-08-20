# PRD #635: Execution budgets and stop conditions

**Status**: Candidate (Discovery)
**Issue**: [#635](https://github.com/vfarcic/dot-agent-deck/issues/635)
**Priority**: Medium
**Created**: 2026-08-21
**Builds on**: [#126](https://github.com/vfarcic/dot-agent-deck/issues/126) (idle-worker detection)
**Interacts with**: [#628](https://github.com/vfarcic/dot-agent-deck/issues/628) (durable work identity), [#629](https://github.com/vfarcic/dot-agent-deck/issues/629) (dependency-aware policy), [#633](https://github.com/vfarcic/dot-agent-deck/issues/633) (telemetry and cost accounting), [#634](https://github.com/vfarcic/dot-agent-deck/issues/634) (agent authority and external overrides), [#236](https://github.com/vfarcic/dot-agent-deck/issues/236) (dirty worktree safety)

## Opportunity

Long-running, repeatedly retried, nested, or unexpectedly expensive agent work has no consistent parent budget across heterogeneous agents and launch paths. Existing controls such as scheduled issue caps and idle-worker timeouts address specific cases but do not define elapsed-time, attempt, concurrency, token, monetary, resource, or change-size limits, nor what safely happens when a limit is reached.

Budgets can make unattended automation more predictable, but only if measurement provenance is honest and stopping preserves useful work. Token and monetary limits depend on credible telemetry, while elapsed time, attempts, and concurrency may be enforceable sooner. This discovery separates budget semantics from dependency scheduling so each can advance at the pace its evidence allows.

## Target Users And Jobs

- A developer preventing a mistaken or stuck run from consuming an entire subscription allowance or workday.
- An orchestrator bounding the aggregate work of children, retries, and nested dispatches.
- A user choosing pause, notify, preserve, retry, or cancel behavior when a limit is reached.
- A future team operator allocating scarce agent slots or remote workers without silently discarding work.

## Current Evidence

- Scheduled issue dispatch has a per-run cap, with semantic follow-ups tracked in #194.
- PRD #126 detects elapsed silence after delegation but does not stop work or enforce a total deadline.
- The daemon can stop panes, but safe termination, dirty worktree preservation, and child cleanup have known edge cases.
- Cross-agent token and monetary data is incomplete and must inherit provenance rules from #633 rather than assuming exact billing.
- [Anthropic's multi-agent research](https://www.anthropic.com/engineering/built-multi-agent-research-system) demonstrates that multi-agent execution can multiply token use substantially.

## Hypotheses

- Budgets should aggregate attempts and child executions under the work that authorized them, while remaining usable with current identities during discovery.
- Elapsed-time, attempt, and concurrency budgets can be explored independently of token and monetary accounting.
- Reaching a budget should normally pause and surface a decision before destructive cancellation.
- Every enforced limit needs explicit preservation, notification, override, and restart semantics.
- Unknown or estimated cost must never trigger a destructive hard stop as if it were exact.

## Questions To Answer

- Which limits solve observed problems: elapsed time, attempts, concurrency, tokens, money, CPU, memory, changed lines, or output volume?
- At what level does each budget attach before #628 is decided: pane, orchestration, dispatch, schedule, issue, or another current identity?
- How are child agents, retries, nested dispatches, and resumed sessions charged without double counting?
- What is the difference between notify, pause, graceful stop, force stop, and refuse-to-start?
- How is uncommitted work preserved and reported when a process stops?
- Which limits can be enforced with current measurements and which remain advisory until #633 advances?
- How do users override or extend a budget without agents granting themselves more authority?
- What happens after daemon restart or temporary loss of telemetry?

## Discovery Evidence Protocol

- Sample at least twelve runs containing normal completion, timeout, retry, nested work, high concurrency, interrupted work, and unavailable cost data.
- Record which proposed limits could have prevented real waste, which would have stopped productive work, and whether the required measurement was known at decision time.
- Prototype elapsed-time, attempt, and concurrency decisions first; test token or monetary limits only with #633 provenance attached.
- Treat a budget type as a `Go` candidate only if it would prevent at least two sampled waste cases, produces no destructive action from estimated or missing data, and preserves all dirty work in stop simulations.
- Choose `Revise` toward advisory notification where hard enforcement is unreliable, and choose `Stop` for limits that create more false stops than prevented waste in the sample.

## Discovery Milestones

- [ ] Catalogue current caps, timeouts, retries, stop paths, child lifecycles, and preservation behavior across manual, orchestration, dispatch, and schedule flows.
- [ ] Define candidate budget scopes, aggregation rules, measurement provenance, and the distinction between advisory and enforceable limits.
- [ ] Prototype non-destructive pause and graceful-stop behavior for elapsed time, attempts, and concurrency before cost-based enforcement.
- [ ] Simulate nested, restarted, missing-data, dirty-worktree, and override scenarios and record whether useful work remains recoverable.
- [ ] Record a `Go`, `Revise`, or `Stop` decision and create implementation PRDs per accepted budget type and stop behavior.

## Evidence And Decision Criteria

A `Go` decision requires an enforceable measurement, a clear scope and aggregation rule, user-visible explanation, safe preservation, override semantics, and sampled evidence that the limit prevents more waste than productive work.

## Non-Goals

- Dependency ordering or conflict prediction, which belongs to #629.
- Exact monetary enforcement when a provider exposes no exact usage data.
- Allowing an agent to raise its own budget without external authorization.
- Force-killing work as the default response to every exceeded limit.
- Building organization billing or enterprise quotas in the first implementation.

## Risks And Dependencies

- A stop path can lose work, orphan children, or leave external operations running.
- Missing or delayed telemetry can make cost limits inaccurate.
- Parent aggregation may remain awkward until #628 decides whether a durable work identity is justified.
- Budget pressure can encourage agents to optimize visible metrics rather than verified outcomes.

## Candidate Outcomes

- **Go**: Create staged implementation PRDs beginning with reliable non-monetary limits and non-destructive stop behavior.
- **Revise**: Provide visibility and notifications while deferring hard enforcement or exact cost budgets.
- **Stop**: Retain existing targeted caps and timeouts, documenting why broader budgets are unsafe or unhelpful.
