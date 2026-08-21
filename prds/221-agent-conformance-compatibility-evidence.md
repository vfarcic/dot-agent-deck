# PRD #221: Agent conformance and compatibility evidence

**Status**: Candidate (Discovery)
**Issue**: [#221](https://github.com/vfarcic/dot-agent-deck/issues/221)
**Priority**: Medium
**Created**: 2026-08-21
**Builds on**: [#20](https://github.com/vfarcic/dot-agent-deck/issues/20) (multi-agent strategies), [#201](https://github.com/vfarcic/dot-agent-deck/issues/201) (Pi integration)
**Interacts with**: [#234](https://github.com/vfarcic/dot-agent-deck/issues/234) (hookless screen-state observation), [#211](https://github.com/vfarcic/dot-agent-deck/issues/211) (Gemini), [#212](https://github.com/vfarcic/dot-agent-deck/issues/212) (Aider), [#502](https://github.com/vfarcic/dot-agent-deck/issues/502) (e2e reliability), [#164](https://github.com/vfarcic/dot-agent-deck/issues/164) (Windows release validation)

## Opportunity

dot-agent-deck supports several agents through native hooks, plugins, extensions, wrappers, and fallback observation, but support is currently understood through implementation details and scattered tests. Users cannot quickly determine which lifecycle states, prompt metadata, tool activity, delegation directions, restoration behavior, or platforms are genuinely validated for each agent.

Issue #221 originally proposed a real-agent orchestrator-by-worker interoperability matrix. That matrix remains useful evidence, but a full N-by-N run is expensive, flaky, and mostly repeats agent-agnostic deck routing. The larger opportunity is a conformance model that defines support levels, tests the capabilities each integration claims, makes adapter regressions visible, and publishes only evidence that is repeatable enough to trust.

## Target Users And Jobs

- A user choosing an agent combination and needing to know which deck capabilities will work.
- A contributor adding or updating an adapter and needing a mechanical contract and test target.
- A maintainer detecting upstream CLI or hook changes before they silently degrade status or routing.
- A future external adapter author evaluating whether extension can be supported safely.

## Current Evidence

- The compiled registry and strategy seam normalize materially different integrations behind `AgentEvent`.
- Existing tests cover many worker directions and real-agent scenarios but do not produce one authoritative capability report.
- The original #221 design correctly notes that a full real N-by-N matrix has poor cost and flake characteristics and that the orchestrator axis is largely deck-redundant.
- Pi currently reports lifecycle without the same prompt and tool metadata as richer adapters, as tracked in #622.
- Hookless agents and redrawing TUIs require the separate observation work in #234 before new adapters such as #211 and #212 can claim comparable status fidelity.
- Emerging standards such as [ACP](https://agentclientprotocol.com/overview/introduction) may reduce integration cost but do not replace behavior-level conformance testing.

## Hypotheses

- Agent support should be expressed as tested capabilities, not a binary supported or unsupported label.
- A small deterministic contract test suite plus reduced real-agent matrix can catch more regressions than a large flaky full matrix.
- The registry should declare capabilities that tests verify, preventing documentation from drifting away from behavior.
- Public compatibility evidence should be generated from repeatable results only after the e2e reliability work in #502 establishes a credible measurement process.
- Runtime adapter extensibility should remain a separate decision from documenting and testing the existing compiled strategy seam.

## Questions To Answer

- What are the stable conformance dimensions: lifecycle, readiness, waiting, prompt metadata, tools, submission confirmation, delegation, restoration, remote operation, and platform support?
- Which permission, filesystem, network, sandbox, credential, and tool-authority capabilities can each adapter and execution posture report or enforce?
- Which dimensions can be tested with deterministic stand-ins and which require real agents?
- What reduced interop matrix gives high routing coverage at acceptable cost and flake probability?
- How are upstream version ranges, degraded modes, partial support, and temporary failures represented?
- Should declared capabilities be compile-time registry data, generated metadata, or test-discovered output?
- What evidence quality is required before publishing a compatibility matrix or reliability score?
- Does ACP provide a useful adapter path for any current or planned agent without weakening lifecycle fidelity?
- Is a community adapter boundary desirable after the conformance contract is stable, or should integrations remain compiled and curated?

## Discovery Evidence Protocol

- Define the claimed capability set for every shipped agent strategy, then require each claim to name a deterministic, PTY, or real-agent test that can disprove it.
- Run the reduced interoperability set at least twenty times across available agents and record false failures, skips, runtime, and model cost separately from genuine compatibility failures.
- Treat the default matrix as viable only if its false-failure rate is below 5 percent per combination and its cost remains acceptable for the documented pre-PR tier; otherwise revise the set or keep it opt-in.
- Treat public compatibility reporting as `Go` only when every published cell is generated from a versioned claim and recent evidence, with unavailable and degraded states shown rather than omitted.
- Choose `Stop` for public reporting if #502 cannot establish repeatability, while preserving any internal conformance contract that still improves adapter maintenance.

## Discovery Milestones

- [ ] Inventory every lifecycle and orchestration capability claimed or observed for each supported integration strategy and arbitrary commands.
- [ ] Define capability levels and map each to deterministic, PTY, or real-agent evidence, including explicit degraded and unavailable states.
- [ ] Design and trial the reduced default interop matrix from the original #221 proposal, with an opt-in full matrix and isolated retries.
- [ ] Determine how registry declarations, generated compatibility output, docs, upstream versions, and #502's reliability constraints stay synchronized.
- [ ] Record a `Go`, `Revise`, or `Stop` decision and create implementation PRDs for conformance metadata, harness changes, publishing, or adapter extensibility as justified.

## Evidence And Decision Criteria

A `Go` decision requires a capability taxonomy that distinguishes current agents meaningfully, tests whose cost and flake rate fit the repository's gates, and a generated report that cannot claim more support than the underlying evidence proves.

## Non-Goals

- Running the full real-agent N-by-N matrix on every CI or pre-PR invocation.
- Ranking model intelligence or coding quality.
- Claiming cross-platform support that #164 or equivalent platform evidence has not verified.
- Introducing runtime plugins before the capability and trust contracts are understood.
- Blocking all releases when an optional external agent service is unavailable.

## Risks And Dependencies

- Real-agent behavior and authentication can change without a deck code change.
- A public matrix can become misleading if tests are flaky, stale, or run only manually.
- Strategy-specific richness means a lowest-common-denominator contract could hide useful distinctions.
- #234, #502, and #164 constrain which claims can become authoritative.

## Candidate Outcomes

- **Go**: Adopt the capability contract and create implementation PRDs for registry metadata, conformance tests, generated reporting, and optional adapter extension.
- **Revise**: Keep the reduced interop matrix and internal compatibility report without publishing reliability claims.
- **Stop**: Preserve per-agent tests and hand-maintained docs, recording why a shared contract would not improve reliability or contributor experience.
