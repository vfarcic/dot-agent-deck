# PRD #632: Self-hosted multi-user team control plane

**Status**: Candidate (Discovery)
**Issue**: [#632](https://github.com/vfarcic/dot-agent-deck/issues/632)
**Priority**: Low
**Created**: 2026-08-21
**Builds on**: [#76](https://github.com/vfarcic/dot-agent-deck/issues/76) (per-user remote environments), [#93](https://github.com/vfarcic/dot-agent-deck/issues/93) (daemon source of truth)
**Depends on discovery in**: [#627](https://github.com/vfarcic/dot-agent-deck/issues/627) (target user and positioning), [#631](https://github.com/vfarcic/dot-agent-deck/issues/631) (authenticated remote boundary), [#634](https://github.com/vfarcic/dot-agent-deck/issues/634) (execution isolation and agent authority)
**Interacts with**: [#628](https://github.com/vfarcic/dot-agent-deck/issues/628) (durable work identity), [#633](https://github.com/vfarcic/dot-agent-deck/issues/633) (telemetry and accounting)

## Opportunity

The current daemon is intentionally a per-user trusted process. That simplicity is a strength for local terminal users, but it cannot safely provide shared work visibility, ownership, handoffs, execution pools, policy, budgets, or audit history for a team. Commercial systems from GitHub, Cursor, Devin, Factory, and Warp increasingly bundle those capabilities with hosted execution or enterprise plans.

An open self-hosted team layer could serve consultancies, platform teams, remote-devbox users, and regulated organizations that need provider-neutral agent operations without sending code or execution to a vendor cloud. It could also burden the project with identity, tenancy, policy, support, and distributed-systems responsibilities before personal workflows are proven. This discovery cannot advance unless #627 validates or deliberately revises the initial individual-user position, and it must choose a boundary that does not compromise the local product.

## Target Users And Jobs

- A development team sharing visibility into agent work without sharing Unix accounts or terminal sessions.
- A maintainer handing work and review responsibility to another person with an auditable history.
- A platform team enforcing allowed agents, models, networks, secrets, concurrency, and spending policy.
- A regulated organization operating workers and state inside its own infrastructure.

## Current Evidence

- PRD #76 and the local protocol explicitly assume same-user trust and exclude multi-user authorization.
- The daemon owns powerful PTYs and can execute commands with the user's environment and credentials.
- Current orchestration models several agent roles but not multiple human owners, approvers, or administrative roles.
- [GitHub](https://github.com/features/copilot/agents), [Devin](https://devin.ai/enterprise), [Factory](https://www.factory.ai/pricing), and [Warp](https://www.warp.dev/factories) monetize team visibility, policy, audit, managed or self-hosted workers, and access controls rather than only local agent panes.
- Authenticated remote actions and explicit execution authority are prerequisites for mutating team features; durable work history and trustworthy telemetry become prerequisites only for features that claim shared history, audit, accounting, or budgets.

## Hypotheses

- Team functionality should be a separate control-plane layer over per-user or isolated workers, not a mode that weakens the local daemon's trust assumptions.
- The first valuable collaboration capabilities are shared visibility, ownership, handoff, and review responsibility rather than centralized autonomous execution.
- Self-hosted deployment and provider neutrality can differentiate the project from mandatory vendor clouds.
- Local single-user operation should remain fully functional and open without the team service.
- Governance and managed connectivity may support sustainable commercial offerings without reselling model inference.

## Questions To Answer

- Which teams have a problem severe enough to deploy and operate a shared control plane?
- What is the tenancy boundary: organization, repository, project, worker, Unix user, or work item?
- Which identities act: humans, agents, workers, service accounts, and external issue or source-control systems?
- What RBAC, approval, audit, secrets, network, retention, and data-residency requirements are minimum rather than enterprise extras?
- Should workers initiate outbound connections to a control plane, or should the service connect inward?
- How are local subscriptions and credentials kept isolated when work is assigned by another user?
- Which state belongs centrally and which remains on the worker or in the repository?
- Can the layer be self-hosted simply enough for the target users, and what managed service would add value later?

## Discovery Evidence Protocol

- Interview at least six teams across at least two candidate segments, with respondents who own both the workflow problem and the authority to adopt or pilot a self-hosted tool.
- Record current collaboration failures, security requirements, deployment constraints, willingness to operate the service, and whether shared visibility alone would create value.
- Treat the candidate as `Go` only if #627 permits the team segment, at least three teams report the problem monthly or more often, and at least two commit to a bounded read-only pilot.
- Require the architecture review to resolve all high-severity tenant-isolation, worker-authority, and credential-boundary findings before any mutating shared action is planned.
- Choose `Revise` toward repository-mediated handoff or managed remote connectivity if teams reject operating a control plane, and choose `Stop` if no target team will pilot the minimum shared view.

## Discovery Milestones

- [ ] Interview target teams about current multi-agent collaboration, compliance constraints, willingness to self-host, and purchasing or adoption authority.
- [ ] Define human and machine actors, trust boundaries, tenancy, ownership, and audit requirements without reusing the per-user daemon assumptions.
- [ ] Compare architectures based on isolated workers, shared daemon state, repository-mediated coordination, and an outbound-connected control service.
- [ ] Prototype the smallest shared read-only work view using current identities after #627 permits the team segment; treat ownership handoff as a separate mutating step gated by #631 and #634.
- [ ] Record a `Go`, `Revise`, or `Stop` decision with an explicit local-product boundary and any follow-up product, security, deployment, or commercial PRDs.

## Evidence And Decision Criteria

A `Go` decision requires repeated team demand, a defensible tenant and worker isolation model, a self-hosted deployment that target users can operate, and proof that the shared layer does not make local single-user execution dependent on a service.

## Non-Goals

- Adding multiple trusted users to the current local socket protocol.
- Building enterprise administration before positioning, remote authorization, and execution authority are sound.
- Hosting customer source code or execution as the default design.
- Requiring a project-owned model gateway or reselling tokens.
- Assuming agent roles and human RBAC roles are interchangeable.

## Risks And Dependencies

- Multi-tenancy around code-executing agents has a much larger security and support burden than the current same-user model.
- Enterprise feature pressure can distract from individual workflow quality and open-source adoption.
- Central history and telemetry create privacy, retention, residency, and incident-response obligations.
- Interviews and a bounded read-only prototype may proceed after #627 permits the segment; mutating actions depend on #631 and #634, durable shared history depends on #628, and telemetry-dependent audit or budget features depend on #633.

## Candidate Outcomes

- **Go**: Define a separate self-hosted team architecture and create staged implementation PRDs beginning with shared visibility and handoff.
- **Revise**: Offer repository-mediated collaboration or managed remote connectivity without a full multi-user control plane.
- **Stop**: Keep the product explicitly personal and local-first, recording why team governance does not fit the project's strategy.
