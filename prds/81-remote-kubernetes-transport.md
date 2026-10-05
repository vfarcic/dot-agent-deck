# PRD #81: Run decks in Kubernetes, managed from the desktop

**Status**: Not started — rewritten 2026-10-04; the first milestone is an analysis of the current state
**Priority**: Medium
**Created**: 2026-05-09 (rewritten 2026-10-04)
**GitHub Issue**: [#81](https://github.com/vfarcic/dot-agent-deck/issues/81)
**Depends on**: [#1258](https://github.com/vfarcic/dot-agent-deck/issues/1258) (resource-aware decks — each daemon serves its host's disk, CPU and memory, and the desktop recommends a deck when starting an agent; choosing where a unit runs, locally, over SSH or in a cluster, builds on it)
**Inputs to M1**: [#635](https://github.com/vfarcic/dot-agent-deck/issues/635) (execution budgets and stop conditions), [#927](https://github.com/vfarcic/dot-agent-deck/issues/927), [#864](https://github.com/vfarcic/dot-agent-deck/issues/864) and [#906](https://github.com/vfarcic/dot-agent-deck/issues/906) (cold builds, shared build caches, build cost — the dominant per-unit cost)
**Related**: [#632](https://github.com/vfarcic/dot-agent-deck/issues/632) (users and access control for decks — everything about *who* may do what is deferred there), [#634](https://github.com/vfarcic/dot-agent-deck/issues/634) (execution isolation and agent authority — agent credentials inside a pod), [#631](https://github.com/vfarcic/dot-agent-deck/issues/631) (authenticated remote boundary)

## Why this was rewritten

The original PRD (May 2026) predates the desktop app, multi-deck support, daemon-owned dispatch and orchestration, and the hook-provenance work of October 2026, and its own 2026-06-14 validation note already called its transport design stale. Its technical conclusions are removed rather than updated: the first milestone below re-derives the design from the code as it is now. The original text is in git history (`git log -p -- prds/81-remote-kubernetes-transport.md`).

## Problem

A deck today runs on the user's own machine, or on a host reached over SSH. Every agent it hosts competes for that one machine. Running several dispatched units at once on one box saturates it: on 2026-10-03/04, nine units on a 16-core host ran at load averages of 100–230, and several units attributed test failures to CPU starvation. There is no way to put a unit's work on separate, disposable compute, and no way for the desktop app to create, find or manage decks running in a cluster.

## Requirements (decided with the maintainer, 2026-10-04)

These are the product decisions this PRD is built on. Everything else is open.

1. **A product feature for any repository**, not something specific to this project's own development workflow: it is reached through the product's own surfaces (the desktop app, the `dispatch` verb), not through this repo's skills.
2. **Two kinds of deck in a cluster:**
   - a **long-lived deck** — a full daemon hosting any number of agents, which the user works in and returns to, like a local or SSH deck;
   - **one deck per dispatched unit** — a daemon hosting exactly that unit, either a single agent or one orchestration team (an orchestration's agents share one daemon, which holds their roles and delegations), removed when the unit is done.
3. **The desktop app manages them:** create a deck in the cluster, see what is running there, connect to it, operate its agents, and see each deck's lifecycle (running, done, failed). Decks created by a dispatch from another deck are shown in relation to the deck that created them.
4. **Decks others started are discovered too:** a user whose credentials allow it sees and can open decks in the cluster that someone else (or another deck) started, without those decks having to announce themselves.
5. **Connections are only ever opened from the user's machine.** The cluster never connects back to the laptop. Data may flow both ways over a connection the desktop opened (prompts out, events and state back), but nothing in the cluster initiates contact with the user's machine.
6. **Access model for this PRD: whoever can access the cluster resources holding the decks can manage every deck there.** Finer-grained access (who may view versus operate, attribution, ownership, audit) is out of scope and belongs to #632.
7. **Work continues without the laptop:** a deck keeps working while the desktop is closed or the laptop sleeps, and a per-unit deck is cleaned up when its work is done even if the desktop never reconnects.

## Out of scope

- User management, roles, read-only access, attribution of prompts and spend to people — #632.
- Provisioning VMs on cloud providers. SSH already reaches existing VMs; per-provider VM creation is a long tail this PRD does not take on.
- One daemon hosting agents that run in other pods (an agent's PTY, hook socket and worktree live with its daemon; splitting them would mean rebuilding the daemon remotely).

## Technical options raised in discussion — to be evaluated in M1, not decided

The 2026-10-04 discussion raised these as candidates. They are recorded so M1 can weigh them against the current code, not as conclusions:

- **Per-unit decks as Kubernetes Jobs**, with `ttlSecondsAfterFinished` for cleanup without the laptop, `activeDeadlineSeconds` for runaways, and a `ResourceQuota` on the namespace. This only works if the daemon actually exits when its unit is done, and today it would not: the daemon's idle shutdown requires no live agents, while a unit that reports `work-done --done` keeps its agents running, so the Job would never finish and TTL cleanup would never start. How a unit's completion tears down its agents and ends the deck is an open question below, not something the existing idle shutdown provides.
- **Discovery through the Kubernetes API** using the user's kubeconfig: decks carry labels (and annotations for creator, unit, repository, parent deck), and the desktop lists and watches them, so the cluster itself is the registry and Kubernetes RBAC bounds what a user sees.
- **Connections through the API server** (`port-forward` or `exec`), opened by the desktop, carrying the existing attach protocol.
- **Shipped access scaffolding:** a namespace-scoped Role for the user's credentials and a `NetworkPolicy` isolating decks from each other.
- **A CRD and controller** (an `AgentDeck` resource the desktop creates) as a possible later phase, for richer lifecycle: pre-warmed decks, shared build caches, policy.
- **Measured cost of one daemon per unit** (2026-10-04, this project's dev host): a freshly started daemon is about 35–50 MB resident and about 1% CPU, against roughly 400 MB per Claude Code agent and 360–825 MB per OpenCode agent; the daemon hosting about 40 agents for 14.5 hours was 171 MB. The dominant per-unit costs are the agents and, for compiled projects, the cold build.

## Open questions for M1

- What, in the current code, already supports this (SSH remotes, the remote registry and its reserved `kubernetes` type, the desktop's multi-deck connections, daemon idle shutdown, dispatch, capability tokens and hook provenance), and what is missing?
- How agents in a pod authenticate to their providers and to the repository (API keys or tokens as secrets) — the question #634 is discovering — and what that means for a credential stored in a cluster.
- How a per-unit deck gets the repository and returns its result (clone, push a branch, open a PR), and how build caches are shared so each unit does not pay a cold build.
- How a per-unit deck ends when its work is done: what tears down its agents on completion (a single agent and an orchestration team alike), so the daemon exits and the cluster can clean up without the laptop. The existing idle shutdown does not cover this, because it requires no live agents.
- How the desktop presents many short-lived decks without overwhelming the deck list.
- What the user-facing messages that currently point at this PRD (`src/connect.rs`: "kubernetes remotes are not yet supported (planned in PRD #81)") should say as the work lands.
- CLAUDE.md rules 12 and 18: which daemon or protocol changes this needs, and how an older daemon and a newer desktop interoperate.

## Milestones

- [ ] **M1 — Current-state analysis and design, decided with the maintainer.** Starts once #1258 has landed, and reads #635, #927, #864 and #906 for what they measured or decided about budgets and build cost. Survey the code and docs listed in the open questions, evaluate the options above against it, and record the chosen design, its milestones, and the rule 9 experimental-flag answer in this PRD. Nothing is built before this is agreed.
- [ ] **M2 onward — defined by M1.** Expected areas: a deck image and its Kubernetes resources; the long-lived deck; per-unit decks for dispatch; discovery and management in the desktop; tests (including a real cluster such as kind) and user docs.
