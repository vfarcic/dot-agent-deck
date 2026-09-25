# PRD #627: Product category, target users, and positioning

**Status**: Candidate (Discovery)
**Issue**: [#627](https://github.com/vfarcic/dot-agent-deck/issues/627)
**Priority**: High
**Created**: 2026-08-21
**Interacts with**: [#176](https://github.com/vfarcic/dot-agent-deck/issues/176) (desktop GUI), [#174](https://github.com/vfarcic/dot-agent-deck/issues/174) (cross-project orchestration), [#220](https://github.com/vfarcic/dot-agent-deck/issues/220) (worktree dispatch)

## Opportunity

dot-agent-deck has grown from a terminal dashboard into a control plane with detach-resilient daemon-owned PTYs, normalized agent events, orchestration, worktree dispatch, scheduling, and SSH operation. Its public category, primary target user, and product promise have not been reconsidered with that expanded capability in view.

The market now contains a directly named [Agent Deck](https://github.com/asheshgoplani/agent-deck), terminal supervisors such as [Claude Squad](https://github.com/smtg-ai/claude-squad) and [dmux](https://github.com/standardagents/dmux), desktop workspaces such as [Conductor](https://www.conductor.build/) and [Superset](https://superset.sh/), and vendor control planes from [GitHub](https://github.com/features/copilot/agents), [Cursor](https://cursor.com/blog/cursor-3), [Anthropic](https://code.claude.com/docs/en/agent-teams), and [OpenAI](https://developers.openai.com/codex/cloud.md). Continuing without an explicit position risks incoherent roadmap choices and avoidable naming confusion.

## Target Users And Jobs

- Terminal-first developers supervising several coding agents on local machines or SSH hosts.
- Maintainers coordinating specialist agents while remaining responsible for review and integration.
- Platform and regulated teams that value local or self-hosted execution and provider independence.
- Users who need agent sessions to survive terminal closure, network interruption, and frontend replacement.

## Current Evidence

- The product already supports Claude Code, OpenCode, Pi, Codex, and Devin through one event model.
- Detach-resilient daemon-owned PTYs, scheduling, orchestration, and remote environments distinguish it from a simple session launcher.
- Parallel agents, worktrees, and dashboards are documented capabilities across [Claude teams](https://code.claude.com/docs/en/agent-teams), [Cursor](https://cursor.com/blog/cursor-3), [GitHub agents](https://github.com/features/copilot/agents), and the direct competitors linked above.
- Prior market research found practitioner reports that [reviewing parallel branches became the primary bottleneck](https://news.ycombinator.com/item?id=48897655) and that [parallel failures can consume substantial credits without useful output](https://news.ycombinator.com/item?id=49351539); these qualitative reports motivate, but do not substitute for, the evidence protocol below.
- The `dot-agent-deck` name is close enough to the direct competitor `agent-deck` to create search, recall, and category confusion.

## Hypotheses

- The strongest position is a local-first, provider-neutral operations layer for durable and verifiable coding-agent work.
- Terminal panes should be presented as an interface, not as the product's defining capability.
- The initial ideal customer is an individual power user or maintainer operating local and SSH-hosted agents, not a broad enterprise software factory buyer.
- A distinct public name may improve discovery and make the product promise easier to own.

## Questions To Answer

- Which user segment experiences the strongest recurring pain and retains after trying the product?
- Which shipped capabilities cause users to choose dot-agent-deck over tmux, agent-specific teams, or desktop workspaces?
- Should the category be described as an agent dashboard, supervisor, orchestrator, control plane, or operations layer?
- Does the naming collision create measurable acquisition or trust problems?
- Which future surfaces reinforce the chosen position, and which should be explicit non-goals?
- Is verification-first orchestration understandable and valuable before the verification workflow is implemented?

## Discovery Evidence Protocol

- Recruit at least six participants across at least two plausible target segments, with at least three participants who currently supervise two or more coding agents.
- Record each participant's current workflow, recurring pain, alternative tools, switching trigger, and response to unbranded positioning statements before showing product-specific language.
- Treat a position as a `Go` candidate only if at least four participants correctly explain its promise and at least three current multi-agent users independently identify the addressed problem as recurring.
- Treat a position as `Stop` or `Revise` if fewer than half of current multi-agent participants recognize the problem, if most interpret the promise as a generic terminal multiplexer, or if the proposed name remains confused with a direct competitor in unprompted recall.
- Preserve anonymized interview notes and the tested statements in the issue so the decision can be challenged later.

## Discovery Milestones

- [ ] Interview or collect structured feedback from current and prospective multi-agent users about jobs, alternatives, and reasons to adopt or stop using the product.
- [ ] Compare the current homepage and onboarding promise against actual shipped capabilities and the leading direct alternatives.
- [ ] Test a small set of category statements, target-user definitions, and names against comprehension, differentiation, and searchability.
- [ ] Define the product's durable principles and explicit non-goals, including its stance on local execution, provider neutrality, IDE functionality, and mandatory cloud services.
- [ ] Record a `Go`, `Revise`, or `Stop` decision and create focused implementation PRDs only for accepted positioning or naming changes.

## Evidence And Decision Criteria

A `Go` decision requires a target user, primary job, category statement, product promise, and non-goals that are supported by user evidence and clearly distinguish the project from both terminal supervisors and vendor-specific agent products. A rename requires evidence that the expected discovery and clarity gains justify migration cost.

## Non-Goals

- Rewriting the website or renaming the project during discovery.
- Selecting features solely because competitors advertise them.
- Claiming enterprise readiness or autonomous software-factory reliability without product evidence.
- Replacing separate implementation PRDs for accepted roadmap initiatives.

## Risks And Dependencies

- Current users may not represent the larger reachable market.
- Competitor categories and capabilities are changing quickly.
- Positioning ahead of delivered review and verification capabilities could overpromise.
- A rename affects packaging, repositories, docs, configuration, and existing users and must not be treated as a copy change.

## Candidate Outcomes

- **Go**: Adopt a tested position and create implementation PRDs for the minimum product and communication changes needed to substantiate it.
- **Revise**: Narrow the target user or run a second discovery round where evidence is inconclusive.
- **Stop**: Preserve the current name and positioning, recording why change would not improve adoption or roadmap quality.
