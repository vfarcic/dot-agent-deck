# PRD #634: Execution isolation and agent authority

**Status**: Candidate (Discovery)
**Issue**: [#634](https://github.com/vfarcic/dot-agent-deck/issues/634)
**Priority**: High
**Created**: 2026-08-21
**Builds on**: [#20](https://github.com/vfarcic/dot-agent-deck/issues/20) (agent strategies), [#76](https://github.com/vfarcic/dot-agent-deck/issues/76) (remote environments), [#220](https://github.com/vfarcic/dot-agent-deck/issues/220) (worktree dispatch)
**Interacts with**: [#221](https://github.com/vfarcic/dot-agent-deck/issues/221) (agent conformance), [#318](https://github.com/vfarcic/dot-agent-deck/issues/318), [#401](https://github.com/vfarcic/dot-agent-deck/issues/401), [#543](https://github.com/vfarcic/dot-agent-deck/issues/543) (event provenance), [#631](https://github.com/vfarcic/dot-agent-deck/issues/631) (remote companion), [#632](https://github.com/vfarcic/dot-agent-deck/issues/632) (team control plane)

## Opportunity

Git worktrees isolate branches and working-copy changes, but they do not isolate processes, filesystems outside the worktree, networks, credentials, environment variables, project configuration, MCP servers, caches, services, ports, or build artifacts. The current model runs agents as the daemon user and treats project configuration as trusted code-executing input, which is coherent for a deliberate same-user local tool but becomes a limiting and dangerous ambiguity as automation, remote control, and shared operation expand.

Remote documentation already warns that agents sharing a host share its credentials and filesystem. Existing provenance issues show that authority-bearing events also need stronger identity. This discovery will determine whether the deck should define explicit execution postures, what it can enforce across heterogeneous agents and hosts, and which risks must remain visible user responsibility.

## Target Users And Jobs

- A developer running agents in repositories with different trust levels or credential requirements.
- A user dispatching unattended work that should not access unrelated files, networks, or secrets.
- A remote-host operator deciding whether projects can safely share a machine.
- A future companion or team-control-plane user authorizing actions without inheriting unlimited daemon-user authority.
- An adapter maintainer documenting what an agent sandbox actually constrains.

## Current Evidence

- `docs/remote-requirements.md` states that remote v1 isolation is at host level and that agents sharing a host share credentials and filesystem access.
- `.dot-agent-deck.toml` role commands execute through a shell and therefore belong to the same trust class as repository build scripts.
- MCP servers and agent processes can inherit broad environment credentials unless launch configuration narrows them.
- Agent-native sandboxes differ in filesystem, network, confirmation, and tool behavior and are not represented by one deck-level capability contract.
- Worktree safety PRDs protect Git data lifecycle but do not create a process or credential security boundary.
- Issues #318, #401, and #543 show that event provenance and authority must be bound before stronger remote actions rely on them.

## Hypotheses

- The product needs named execution postures with explicit capabilities and limitations rather than a binary sandboxed label.
- Isolation may be composed from agent-native controls, environment filtering, OS processes, containers, remote hosts, or future Kubernetes placement, with no one mechanism required everywhere.
- Project trust, agent authority, and human authorization are separate decisions and should not be collapsed into one prompt permission mode.
- Safe defaults can reduce credential and network exposure without preventing users from opting into trusted full-access workflows.
- Conformance reporting in #221 should include the authority each adapter can request, expose, and enforce.

## Questions To Answer

- What threats are in scope for local trusted repositories, cloned untrusted repositories, unattended dispatch, remote hosts, and future team operation?
- Which resources require explicit authority: repository writes, parent Git metadata, arbitrary filesystem paths, network, subprocesses, environment variables, MCP tools, services, and credentials?
- What can the deck enforce independently of an agent, and what can it only declare or test?
- How should trusted project configuration be surfaced before its shell commands or MCP setup execute?
- Can environment and credential scoping be made useful without breaking existing agent authentication and tools?
- Which isolation boundary should be recommended for projects that run services, use fixed ports, or share expensive build caches?
- How do worktree, container, host, SSH, and Kubernetes placement compose without implying stronger isolation than they provide?
- What evidence must a remote or team feature require before it can expose a mutating action?

## Discovery Evidence Protocol

- Audit at least five representative launch configurations across local panes, worktree dispatch, orchestration, SSH remote, and one containerized or host-isolated workflow.
- For each configuration, test and record access to repository parent metadata, unrelated files, inherited credentials, outbound network, subprocesses, MCP tools, and sibling agent state.
- Define at least three concrete threat scenarios, including an untrusted repository, a compromised or mistaken agent, and a stale or forged authority event, and verify whether proposed postures contain them.
- Treat an execution-posture model as `Go` only if every named posture has testable capabilities, no unresolved high-severity mismatch between label and enforcement, and a migration path that does not silently broaden current access.
- Choose `Revise` toward documentation and host-level recommendations where enforcement is not portable, and choose `Stop` for any posture whose security promise cannot be tested reliably.

## Discovery Milestones

- [ ] Document the current trust and authority model from configuration load through process spawn, hooks, tools, credentials, worktrees, remotes, and cleanup.
- [ ] Build a threat model and capability matrix for current agents, operating systems, and placement options, feeding the adapter dimensions in #221.
- [ ] Compare minimal environment filtering, agent-native sandbox controls, containers, per-project hosts, and policy declarations through targeted prototypes.
- [ ] Define user-visible posture names, escalation and override semantics, and the evidence required before remote or shared mutation can depend on them.
- [ ] Record a `Go`, `Revise`, or `Stop` decision and create separate implementation PRDs for authority metadata, launch enforcement, trust prompts, credential scoping, or isolated placement as justified.

## Evidence And Decision Criteria

A `Go` decision requires a threat model grounded in current launch paths, accurately named and testable execution postures, explicit residual risks, and evidence that the proposed controls reduce authority without breaking the supported agent workflows they claim to protect.

## Non-Goals

- Claiming a worktree is a security sandbox.
- Treating all project configuration as untrusted while continuing to execute its shell commands.
- Building a mandatory cloud or container runtime.
- Guaranteeing containment through agent prompt instructions alone.
- Solving multi-user RBAC, which belongs to #632 after execution authority is understood.

## Risks And Dependencies

- Security labels that overstate enforcement can make users less safe than explicit full access.
- Agent-native sandboxes and permission flags change across upstream versions.
- Credential filtering can break legitimate authentication and tools in difficult-to-diagnose ways.
- Container or host isolation affects performance, caches, services, networking, and installation complexity.

## Candidate Outcomes

- **Go**: Adopt an explicit authority and execution-posture contract and create incremental implementation PRDs for the controls that prove portable and valuable.
- **Revise**: Standardize capability disclosure and safer host guidance while leaving enforcement to selected agents or environments.
- **Stop**: Preserve the trusted-user model and explicitly prevent remote or team features from implying a stronger boundary.
