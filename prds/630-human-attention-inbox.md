# PRD #630: Human-attention inbox

**Status**: Candidate (Discovery)
**Issue**: [#630](https://github.com/vfarcic/dot-agent-deck/issues/630)
**Priority**: High
**Created**: 2026-08-21
**Builds on**: [#126](https://github.com/vfarcic/dot-agent-deck/issues/126) (agent-driven notifications and idle-worker detection), [#333](https://github.com/vfarcic/dot-agent-deck/issues/333) (orchestration tab status)
**Interacts with**: [#447](https://github.com/vfarcic/dot-agent-deck/issues/447) (waiting worker escalation), [#628](https://github.com/vfarcic/dot-agent-deck/issues/628) (durable work identity), [PRD #78](78-tab-level-status-indicators.md) (older tab-indicator concept), [PRD #18](done/18-permission-prompt-control.md) and [PRD #92](done/92-process-boundary-invariant-audit.md) (existing approval controls)

## Opportunity

The dashboard shows agent state and the notification work can signal selected events, but a user supervising several sessions still has to scan panes, tabs, terminal bells, and external messages to determine what needs action. Every visible session competes for equal attention even when one blocks an entire dependency chain and another can wait.

An attention inbox could present actionable events as a ranked queue, preserve enough context to make a decision, and jump directly to the relevant pane or work item. The discovery must determine whether a separate inbox is better than improving existing cards and tabs, and which prioritization can be deterministic rather than model-generated.

## Target Users And Jobs

- A developer supervising enough agents that scanning every pane is no longer practical.
- A user away from the terminal who returns to several accumulated requests and outcomes.
- An orchestrator operator deciding which blocked worker, approval, failure, or completed change to handle next.
- A future remote client that needs a concise actionable surface rather than a full terminal layout.

## Current Evidence

- PRD #126 introduced idle-worker detection and notification recipes but intentionally did not create a deck-owned notification center.
- PRD #333 and the dashboard cards expose status at the tab and pane level.
- PRDs #18 and #92 provide an existing `y`/`n` approval path by sending generic PTY input; they are an interaction baseline, not yet a strongly target-bound approval primitive.
- Issue #447 shows that a waiting worker can require action that is not routed to the person or orchestrator best able to resolve it.
- [Cursor](https://cursor.com/blog/cursor-3), [Superset](https://superset.sh/), and [Orca](https://onorca.dev/) emphasize notifications and direct navigation to sessions requiring input.
- Prior market research suggests attention routing and review queues are a greater bottleneck than launching additional agents; this claim remains a hypothesis to validate with the evidence protocol below.

## Hypotheses

- The primary unit should be an actionable attention event, not an agent notification log.
- Deterministic factors such as dependency impact, time waiting, explicit approval state, failure severity, and active cost burn can rank events without an LLM.
- Events need acknowledgement, snooze, resolution, and stale-event invalidation so the inbox does not become another noisy status view.
- The TUI can provide the authoritative first surface, with notification and remote clients consuming the same event model later.

## Questions To Answer

- Which events are truly actionable and which should remain ambient status?
- Should attention attach to panes, agents, orchestration roles, work items, or all of them through one identity?
- What ranking rules are understandable and predictable enough for users to trust?
- How should duplicate events from hooks, timers, process exits, and orchestrator reports collapse?
- What happens when the underlying condition resolves before the user opens the event?
- Which actions belong in the inbox: focus, answer, approve, retry, cancel, review, dismiss, or snooze?
- How should terminal, OS, chat, and future mobile notifications relate to the canonical inbox?

## Discovery Evidence Protocol

- Observe at least six runs with three or more concurrent sessions, including waiting, failure, completion, and stale-event cases, and record missed actions, duplicate signals, time-to-action, and notification count.
- Compare the current dashboard against inbox and sorted-dashboard prototypes using the same scripted event sets with at least five multi-agent users.
- Treat an inbox as a `Go` candidate only if at least three observed runs contain a missed or materially delayed action and the prototype reduces median time-to-correct-action by at least 30 percent without increasing total alerts.
- Choose `Revise` toward cards or tab indicators if they achieve comparable results, and choose `Stop` if users reliably identify the next action under the current surface.
- Include at least one stale target and pane-replacement scenario so fast navigation or approval cannot pass by acting on the wrong generation.

## Discovery Milestones

- [ ] Catalogue existing state transitions, bells, notifications, idle prompts, failures, and completion signals and classify them as actionable or informational.
- [ ] Observe or simulate multi-agent runs to identify attention failures, duplicate signals, and the information needed to act without first opening every pane.
- [ ] Compare an inbox, a sorted dashboard mode, and enhanced tab/card indicators through low-cost TUI prototypes or mockups.
- [ ] Define deterministic ranking, deduplication, acknowledgement, and stale-event semantics, including interactions with #126 and #447.
- [ ] Record a `Go`, `Revise`, or `Stop` decision and create implementation PRDs for the event model and chosen surfaces only if justified.

## Evidence And Decision Criteria

A `Go` decision requires evidence that users miss or delay important actions under the current dashboard, a bounded event taxonomy, deterministic lifecycle rules, and a surface that reduces time-to-action without increasing notification volume.

## Non-Goals

- Sending more notifications for every agent transition.
- Using an LLM to decide basic priority when deterministic state is available.
- Building team assignment, RBAC, or escalation policy in the first personal inbox.
- Replacing the detailed terminal pane where context or interaction requires it.

## Risks And Dependencies

- A noisy or stale inbox is worse than status cards because users learn to ignore it.
- Hook coverage differs by agent, so apparent priority can be biased toward better-instrumented adapters.
- Durable acknowledgement across restart may depend on the work identity explored in #628.
- Security-sensitive actions such as approval or prompt submission require stronger identity binding than read-only navigation.

## Candidate Outcomes

- **Go**: Define the canonical attention-event lifecycle and create implementation PRDs for the TUI surface and notification integration.
- **Revise**: Improve existing cards and tabs if a separate inbox does not reduce interaction cost.
- **Stop**: Retain current status and notification behavior, recording the usage evidence that did not justify another surface.
