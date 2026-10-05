# PRD #632: Users and access control for decks, wherever they run

**Status**: Placeholder for discussion — rewritten 2026-10-04; the first milestone is an analysis of the current state
**Priority**: Low
**Created**: 2026-08-21 (rewritten 2026-10-04)
**Issue**: [#632](https://github.com/vfarcic/dot-agent-deck/issues/632)
**Related**: [#81](https://github.com/vfarcic/dot-agent-deck/issues/81) (decks in Kubernetes, which assumes "whoever can access the cluster manages every deck" and defers everything finer to this PRD), [#631](https://github.com/vfarcic/dot-agent-deck/issues/631) (authenticated remote boundary), [#634](https://github.com/vfarcic/dot-agent-deck/issues/634) (execution isolation and agent authority), [#628](https://github.com/vfarcic/dot-agent-deck/issues/628) (durable work graph)

## Why this was rewritten

The original PRD (August 2026, titled "Self-hosted multi-user team control plane") predates the desktop app's multi-deck support, daemon-owned dispatch, and the October 2026 hook-provenance and capability-token work. Its hypotheses and architectural comparisons are removed rather than updated, and the first milestone re-derives the state of things from the code as it is now. The original text is in git history (`git log -p -- prds/632-self-hosted-multi-user-team-control-plane.md`).

## Problem

Decks can run on the user's machine, on hosts reached over SSH, and (per #81) in Kubernetes. As soon as more than one person can reach a deck, there is no notion of **who** is acting. Reaching a deck today means full control of it: whoever can open its socket — the owner locally, anyone with the SSH login, and under #81 anyone whose cluster credentials allow a connection — can type into every agent, stop them, and dispatch new work. That is acceptable for one person and is #81's explicit assumption; it is not acceptable once decks are shared.

## Questions to explore (raised 2026-10-04, not answered)

- **Identity:** how a deck knows which person a connection belongs to, for local, SSH and cluster placements alike.
- **Roles:** at least watching a deck versus operating it (prompting, stopping, dispatching); whether anything finer is needed.
- **Concurrent operators:** two people driving the same agent — awareness of who is typing, and how this relates to the existing command-entry lock (#393).
- **Attribution and spend:** a prompt sent to someone else's agent spends that person's provider credentials and credits under their identity; whether and how that is allowed, shown, or recorded.
- **Ownership and handoff:** who owns a deck or a unit, and how it passes to someone else.
- **Audit:** what is recorded about who did what.
- **Placement differences:** what each placement can already supply (local OS user, SSH identity, Kubernetes RBAC and service accounts) and whether one model can span all of them.

## Milestones

- [ ] **M1 — Current-state analysis.** Document how access works today for each placement (local socket, SSH remotes, the desktop's connections, capability tokens and hook provenance), what #81, #631 and #634 assume or decide, and where the gaps are.
- [ ] **M2 — Scope decided with the maintainer**, recorded here: which of the questions above become requirements, in what order, and whether this stays one PRD or splits.
- [ ] **M3 onward — defined by M2.**
