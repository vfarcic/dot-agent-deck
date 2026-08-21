# PRD #631: Authenticated remote companion for steering and approvals

**Status**: Candidate (Discovery)
**Issue**: [#631](https://github.com/vfarcic/dot-agent-deck/issues/631)
**Priority**: Low
**Created**: 2026-08-21
**Builds on**: [#76](https://github.com/vfarcic/dot-agent-deck/issues/76) (remote agent environments), [#93](https://github.com/vfarcic/dot-agent-deck/issues/93) (always-external daemon), [#345](https://github.com/vfarcic/dot-agent-deck/issues/345) (remote doctor)
**Depends on discovery in**: [#634](https://github.com/vfarcic/dot-agent-deck/issues/634) (execution isolation and agent authority)
**Interacts with**: [#176](https://github.com/vfarcic/dot-agent-deck/issues/176) (desktop GUI), [#630](https://github.com/vfarcic/dot-agent-deck/issues/630) (attention inbox), [#318](https://github.com/vfarcic/dot-agent-deck/issues/318), [#401](https://github.com/vfarcic/dot-agent-deck/issues/401), [#543](https://github.com/vfarcic/dot-agent-deck/issues/543) (event provenance and authority)

## Opportunity

Remote environments let the TUI reach a daemon through SSH, and daemon-owned agents survive local disconnection. Users still need a terminal and full TUI session to inspect status, answer a prompt, approve a gate, retry work, or cancel a run. A lightweight authenticated companion could make long-running local or remote work steerable from a browser or phone without turning execution into a mandatory hosted service.

[Cursor](https://cursor.com/blog/cursor-3), [Orca](https://onorca.dev/), [Codexia](https://github.com/milisp/codexia), [GitHub agents](https://github.com/features/copilot/agents), and hosted coding agents increasingly provide asynchronous mobile or web interaction. The deck's client-independent daemon boundary makes another client plausible, but its current same-user local trust model is not a network authorization model. This discovery must treat identity, transport, revocation, and action safety as prerequisites rather than frontend details.

## Target Users And Jobs

- A developer who receives an attention signal while away from the terminal and wants to inspect and answer it safely.
- A user supervising agents on an SSH devbox whose laptop sleeps or changes networks.
- A maintainer who needs concise status and approval controls rather than terminal fidelity on a phone.
- A future desktop or team client that needs an authenticated client-neutral control contract.

## Current Evidence

- PRD #76 deliberately relies on SSH authentication and excludes additional multi-user authorization.
- The attach protocol already supports several non-TUI clients but is designed for trusted same-user IPC.
- PRD #176 explicitly excludes browser and mobile delivery while identifying the daemon as a client-neutral source of truth.
- Open issues #318, #401, and #543 show that event provenance and authority are not yet strong enough to expose blindly over a network.
- PRD #126 proves that out-of-band notification is valuable but intentionally leaves interaction in the primary agent or terminal channel.

## Hypotheses

- The highest-value companion surface is an attention queue plus narrow actions, not terminal streaming or a mobile IDE.
- Remote access should be optional, local-first, and deployable without a vendor-operated execution cloud.
- Read-only status, prompt submission, approval, retry, and cancellation need separate authorization and confirmation policies.
- A secure relay may be a later convenience layer, but direct self-hosted access and SSH-based options should remain viable.
- The remote API should be client-neutral so web, mobile, desktop, and automation clients do not create separate business logic.

## Questions To Answer

- Which remote actions deliver enough value to justify the new attack surface?
- Can SSH forwarding and short-lived local credentials satisfy the initial use case without a public listener?
- What identity, pairing, token storage, expiration, revocation, replay protection, and audit semantics are required?
- How does a remote client prove that a prompt, pane, work item, or approval target is still the same generation it displayed?
- Which actions require reauthentication or confirmation because they can execute code, spend money, or destroy work?
- Is terminal streaming necessary, or can the companion rely on structured status, snapshots, evidence, and targeted responses?
- How do notifications deep-link into a self-hosted companion without leaking repository or task data?
- Where is the boundary between this candidate and the desktop GUI in #176?

## Discovery Evidence Protocol

- Interview at least six users who run agents on SSH hosts or leave them unattended, including at least three who encounter an away-from-terminal decision weekly.
- Record the exact remote action, urgency, current workaround, frequency, and consequence of waiting before showing a proposed companion.
- Threat-model and prototype read-only status plus one reversible action; no mutating action can graduate with an unresolved high-severity identity, replay, stale-target, or credential-storage finding.
- Treat the companion as a `Go` candidate only if at least three participants need a supported action weekly and five of six can complete the prototype flow without terminal access or target confusion.
- Choose `Revise` toward SSH reconnect or notification deep links if remote mutation does not justify its risk, and choose `Stop` if observed users can wait or use SSH without meaningful cost.

## Discovery Milestones

- [ ] Define the smallest away-from-terminal journeys and rank read-only and mutating actions by value and risk.
- [ ] Threat-model direct network access, SSH-forwarded access, device pairing, optional relay, stolen credentials, replay, and stale-target actions.
- [ ] Audit the existing protocol and provenance issues to identify which boundaries can be reused and which require a separate authenticated service.
- [ ] Prototype a read-only status and one reversible action through the safest plausible transport, measuring operational burden and usability.
- [ ] Record a `Go`, `Revise`, or `Stop` decision and create separate implementation PRDs for authentication, API, notification deep links, and companion UI if accepted.

## Evidence And Decision Criteria

A `Go` decision requires a common away-from-terminal job that users cannot solve adequately through SSH and notifications, a credible threat model, generation-safe action semantics, and a deployment path that preserves optional local and self-hosted operation.

## Non-Goals

- Exposing the existing trusted local socket directly to a network.
- Building a mobile code editor or reproducing full terminal fidelity in the first companion.
- Making hosted relay or cloud execution mandatory.
- Adding multi-user team sharing, RBAC, or organization policy in this personal remote-access candidate.
- Deferring current provenance vulnerabilities because a stronger future protocol is planned.

## Risks And Dependencies

- Prompt submission and approval can trigger arbitrary agent actions with repository and credential access.
- A stale mobile view can target a replaced pane or different agent generation unless identity is end-to-end.
- Public exposure changes the security and compatibility obligations of the daemon substantially.
- Attention and evidence surfaces may depend on #630 and #626 to be useful without terminal streaming.

## Candidate Outcomes

- **Go**: Select a narrow authenticated boundary and create implementation PRDs for security foundations, API, and the minimum companion surface.
- **Revise**: Improve SSH reconnect and notification deep links if a separate remote API does not justify its risk.
- **Stop**: Keep remote steering terminal-only and document the security and maintenance reasons.
