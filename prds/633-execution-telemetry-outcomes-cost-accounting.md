# PRD #633: Execution telemetry, outcomes, and cost accounting

**Status**: Candidate (Discovery)
**Issue**: [#633](https://github.com/vfarcic/dot-agent-deck/issues/633)
**Priority**: Medium
**Created**: 2026-08-21
**Builds on**: [#20](https://github.com/vfarcic/dot-agent-deck/issues/20) (multi-agent event model)
**Interacts with**: [#628](https://github.com/vfarcic/dot-agent-deck/issues/628) (durable work identity), [#176](https://github.com/vfarcic/dot-agent-deck/issues/176) (future rich usage views), [#221](https://github.com/vfarcic/dot-agent-deck/issues/221) (agent conformance), [#635](https://github.com/vfarcic/dot-agent-deck/issues/635) (budget enforcement)

## Opportunity

The deck observes lifecycle events, process state, delegation, elapsed time, and some agent-specific activity, but it does not provide a durable provider-neutral account of what a run consumed or achieved. Users cannot consistently attribute model spend, tokens, retries, elapsed time, resource use, intervention count, changed lines, verification results, or final outcome to one piece of work.

Cost data is especially uneven because agents expose different APIs and users may run subscriptions, API keys, local models, or opaque hosted plans. A credible telemetry system must distinguish directly measured values from estimates and unavailable data. This discovery will determine which metrics are useful and trustworthy, whether OpenTelemetry is an appropriate export seam, and how outcome data can improve decisions without becoming surveillance.

## Target Users And Jobs

- A developer identifying expensive, stuck, or repeatedly retried work before it wastes more time or credits.
- A maintainer comparing agent and workflow reliability on the repository's real tasks.
- An operator diagnosing why a work item failed despite substantial execution time.
- A future team administrator enforcing transparent budgets without mandating a model gateway.

## Current Evidence

- The unified event model already normalizes lifecycle semantics across several integration strategies.
- Agent-specific token and cost visibility varies and unknown commands provide much weaker telemetry.
- [Anthropic's multi-agent research](https://www.anthropic.com/engineering/built-multi-agent-research-system) reports large token amplification for multi-agent systems and warns that coding work is not always independently parallelizable.
- Direct competitor [Agent Deck](https://github.com/asheshgoplani/agent-deck) and commercial platforms such as [Factory](https://www.factory.ai/pricing) expose cost or usage views, making basic attribution an emerging expectation.
- [OpenTelemetry agent observability](https://opentelemetry.io/blog/2025/ai-agent-observability/) provides an emerging vocabulary and export ecosystem, but adopting it does not define the product's own outcome semantics.

## Hypotheses

- Every metric should carry provenance such as measured, provider-reported, estimated, agent-asserted, or unavailable.
- Work-item outcome, intervention count, verification state, and elapsed time are more actionable than token totals alone.
- The internal event and metric model should remain provider-neutral, with OpenTelemetry as an optional export rather than the source of truth.
- Local telemetry should default to private and bounded retention, with no mandatory hosted collection.
- Reliable telemetry can later support budgets, agent conformance reporting, and repository-specific workflow evaluation.

## Questions To Answer

- Which metrics can each existing adapter report accurately, and at what lifecycle points?
- How should subscription-based agents be represented when monetary cost is unknowable?
- What is the stable outcome taxonomy: completed, verified, rejected, abandoned, failed, timed out, superseded, or merged?
- How are retries, child agents, orchestrations, and cross-project dispatch aggregated without double counting?
- What local storage, retention, redaction, and export controls are appropriate?
- Which OpenTelemetry semantic conventions are stable enough to adopt, and where is a deck-specific schema necessary?
- Can telemetry be collected without intercepting prompts, source code, or secrets?
- Which metrics are useful for users rather than vanity dashboards?

## Discovery Evidence Protocol

- Capture at least twelve runs across at least three agent strategies, including success, failure, retry, interruption, orchestration, and a provider for which monetary cost is unavailable.
- For every candidate field, compare deck-observed, provider-reported, estimated, and user-visible values and record disagreement, overhead, and missing-data rates.
- Treat a telemetry field as viable only when its provenance is explicit and it is present or honestly unavailable in at least 90 percent of sampled runs without inspecting source or prompt content.
- Treat the candidate as `Go` only if at least three metrics change a real user decision in the sample, such as stopping, retrying, switching workflow, or investigating failure.
- Choose `Revise` toward a smaller outcome and intervention record if token or cost accounting remains unreliable, and choose `Stop` if the data is primarily diagnostic noise or creates unacceptable privacy exposure.

## Discovery Milestones

- [ ] Inventory observable lifecycle, usage, process, Git, verification, and intervention signals for every supported agent strategy and arbitrary commands.
- [ ] Define provenance, aggregation, privacy, retention, and missing-data semantics for a minimal provider-neutral execution record.
- [ ] Validate an outcome taxonomy against real successful, failed, retried, interrupted, and manually reviewed runs.
- [ ] Prototype local capture and optional OpenTelemetry export for a narrow metric set, measuring overhead and data leakage risk.
- [ ] Record a `Go`, `Revise`, or `Stop` decision and create implementation PRDs for storage, adapter instrumentation, user surfaces, or export only where evidence supports them.

## Evidence And Decision Criteria

A `Go` decision requires metrics that users can act on, explicit provenance and missing-data behavior, bounded overhead, privacy-safe defaults, and evidence that the model works across materially different agent integrations without a mandatory proxy or gateway.

## Non-Goals

- Reselling inference or routing all model traffic through the deck.
- Pretending estimates are exact provider billing data.
- Uploading prompts, code, or telemetry to a hosted service by default.
- Ranking models publicly from a small or biased task sample.
- Building budget enforcement before accounting semantics are trustworthy.

## Risks And Dependencies

- Adapter and provider data can change without notice or disagree with final billing.
- Metrics can incentivize cheap but low-quality outcomes if verification is not represented.
- Detailed traces can expose prompts, paths, repository names, or secrets.
- Durable attribution may depend on the work identity explored in #628.

## Candidate Outcomes

- **Go**: Adopt the minimal telemetry and outcome schema and split local storage, adapter capture, UI, and export into implementation PRDs.
- **Revise**: Limit the system to elapsed time, interventions, verification, and outcomes if token or monetary accounting is not reliable.
- **Stop**: Retain diagnostic logs only and document why product telemetry would be misleading or too sensitive.
