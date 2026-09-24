# PRD #1264: Cross-deck dispatch — daemons connect to each other, so a dispatcher can place units on another machine's deck

**Status**: Draft — not started
**Priority**: Medium
**Created**: 2026-09-24
**Issue**: [#1264](https://github.com/vfarcic/dot-agent-deck/issues/1264)
**Depends on**: [#1258](https://github.com/vfarcic/dot-agent-deck/issues/1258) (daemon-served host utilisation and the headroom verdict — needed for automatic deck choice, not for explicit targeting)
**Builds on**: [#220](https://github.com/vfarcic/dot-agent-deck/issues/220) (the `dispatch` verb, dispatcher mode, and the dispatch return edge), [#76](https://github.com/vfarcic/dot-agent-deck/issues/76) (remote environments and the ssh transport), [#120](https://github.com/vfarcic/dot-agent-deck/issues/120) (daemon-owned clones and worktrees)
**Interacts with**: [#468](https://github.com/vfarcic/dot-agent-deck/issues/468) (split dispatch into placement and spawning), [#174](https://github.com/vfarcic/dot-agent-deck/issues/174) (cross-project dispatch and return-edge addressing), [#634](https://github.com/vfarcic/dot-agent-deck/issues/634) (execution isolation and agent authority), [#632](https://github.com/vfarcic/dot-agent-deck/issues/632) (multi-user team control plane — explicitly not this)

## Problem Statement

A dispatcher can only fill the machine it runs on.

**`dispatch` is single-daemon by construction.** `handle_dispatch` (`src/dispatch.rs`) cuts a worktree as a sibling of the dispatcher's own checkout (`../<repo>-dispatch-<slug>`) on that host's filesystem, spawns the unit through that daemon's PTY registry, and records the return route in that daemon's `DispatchReturns` so the unit's completion is injected back into the dispatcher's pane. `DispatchContext` carries a local `working_dir`, the local registry and the local worktree registry, and nothing that names another deck.

**Daemons do not know about each other.** A TUI attaches to exactly one daemon (`Endpoint` is a single `Local`/`Remote` choice). The desktop holds several at once, but it is a client: it cannot be the thing an unattended dispatcher depends on, because a dispatcher is meant to keep working when nobody has the app open. A remote deck is reached through an `ssh -N -L` tunnel that a *client* opens; no daemon opens one.

**The cost is measured, not hypothetical.** PRD #1258 records one session in which free disk fell from 404G to 239G while three units built on one host, and a single dispatch worktree held 153G. `/issue-queue` carries a hand-maintained "below ~100G free: do not dispatch" rule. With a second machine configured as a remote deck and idle, the dispatcher still had nowhere else to put work.

## Solution Overview

The dispatcher's daemon opens an **outbound link to a peer deck** and dispatches the unit there. The peer places the code, runs the unit under its own daemon, and sends the completion report back over the same link.

Because every decision is made in the daemon, the client does not matter: a dispatcher started from the TUI, from the desktop, or by a schedule dispatches across decks identically, and neither client needs to change for the dispatch itself to work.

Five commitments shape it.

**The link is one-directional and initiated by the dispatcher's daemon.** Deck A (the dispatcher's) connects to deck B using the same ssh transport and `remotes.toml` entry shape a client uses today. B never connects to A. The report returns over A's link, so a laptop behind NAT can dispatch to a server without the server being able to reach the laptop. The daemon becomes a client of another daemon; the client library it needs (`DaemonClient`, `remote_tunnel`) already lives in the same crate.

**Local stays the default, and automatic is opt-in per call.** `dispatch` with no deck behaves exactly as it does today. `--deck <name>` targets a named peer. `--deck auto` asks each eligible deck — the local one included — for PRD #1258's headroom verdict and picks one that qualifies; when none does, it refuses and says what is short rather than picking the least bad. PRD #1258 keeps its own New agent flow advisory; this PRD differs because the caller is an agent with nobody to advise, and the opt-in is the caller naming `auto` explicitly.

**The peer places the code, not the dispatching agent.** The agent is not on B and cannot run `git worktree add` there. B resolves the repository from the dispatcher checkout's `origin` URL into a daemon-owned clone — the clone-if-missing / fetch path `src/issue_dispatch_run.rs` already has — and cuts the worktree from the dispatcher's base commit. That commit must be reachable from `origin`; a dispatch whose base is unpushed is refused with the remedy ("push `<branch>` first"), never silently rebased onto something else.

**Both ends opt in.** A peer is dispatchable from A only when A's config names it as a dispatch peer, and B accepts only when its own config allows inbound dispatch. Reachable is not the same as authorised: an ssh key that lets a user `connect` to B does not by itself let an agent on A start agents on B.

**The contract grows by capability-gated additions, with no `PROTOCOL_VERSION` bump.** Inbound dispatch and report acknowledgement are new `AttachRequest` variants that A withholds until B advertises the matching capability, checked in the client library — the `focus-gained` precedent (PRD #1105) and `src/daemon_protocol.rs`'s documented exception. A peer that does not advertise it is listed as ineligible with that reason. Rule 12's question is answered in each PR rather than assumed.

## What the user sees

**Configuring a peer.** On the dispatcher's host, an existing remote (`dot-agent-deck remote add`) is marked as a dispatch peer; on the peer's host, inbound dispatch is enabled. `dot-agent-deck remote doctor <name>` gains checks for what an unattended link needs that an interactive `connect` does not — above all a key the daemon can use with no `SSH_AUTH_SOCK` and no passphrase prompt.

**Dispatching.** A dispatcher's `dispatch --list-targets` output names the peer decks alongside the local one, each with its eligibility and, where #1258 is present, its headroom verdict and reason. The dispatcher runs `dispatch <name> --deck gpu-box --task "..."` or `--deck auto`. The acknowledgement in its pane names the deck the unit landed on ("dispatched `fix-auth` to deck `gpu-box`: 412G free, load 1.2/16").

**Watching, from the TUI.** The TUI stays attached to one daemon. The dispatcher's pane shows the acknowledgement and, later, the completion report, exactly as for a local unit. The unit's own pane is on B and is not shown in a TUI attached to A; the user reaches it with `dot-agent-deck connect gpu-box` or the desktop. Showing remote units inside a single-daemon TUI is out of scope.

**Watching, from the desktop.** When B is also connected in the desktop, the unit appears under B's group like any other agent, and names the dispatcher on A that started it.

**When the link drops.** The unit keeps running on B. B holds the completion report until A reconnects and acknowledges it, bounded in count and age, and A's dispatcher pane says the route to B is down rather than going quiet.

## Scope

### In scope

- A daemon-held outbound peer link: configuration, the ssh transport, capability handshake, reconnection, and status.
- Inbound dispatch on the peer: authorisation, repository resolution into a daemon-owned clone, the pushed-base check, worktree creation, and spawn.
- The return edge across daemons: a report held on the peer until acknowledged, identity-gated delivery on the dispatcher's side, and the dropped-link behaviour.
- `dispatch --deck <name|auto>`, peer decks in `dispatch --list-targets`, and the dispatcher-mode seed teaching the flag (mechanics only, per PRD #220's seed rule).
- The dispatch acknowledgement naming the deck, and the desktop linking a remote unit to its dispatcher.
- Degradation against a peer too old to advertise the capability.

### Out of scope

- **Showing remote units inside a TUI.** It would change the TUI's one-daemon model and is a separate decision.
- **Multi-user or shared decks.** Every deck here belongs to the same user, reached with that user's own ssh identity. Tenancy, identity and authorisation between people are PRD #632's question.
- **Moving a running unit between decks.** A worktree is a pre-spawn decision (PRD #220); so is the deck.
- **Copying unpushed work.** The base must be on `origin`. Shipping a bundle or a patch series to the peer is a possible follow-up, not v1.
- **Dispatching orchestrations whose roles span decks.** A dispatched unit — single agent or orchestration — lives entirely on one deck.
- **Windows** as either end, since the daemon reports `Unsupported` there.
- **Kubernetes-transport peers** until PRD #81 provides the transport.

## Design decisions and constraints

- **The daemon owns the decision** (rule 18). The alternative — the desktop brokering between decks — was rejected because it makes cross-deck dispatch work only while the app is open, and because it would leave the TUI with no way to do it.
- **A peer link is a client connection, and B treats it as one.** B's existing attach handling, event stream and capability gates apply; what is new is the verbs, not a second protocol.
- **The report travels, the files do not.** B reads the unit's `work-done --task-file` report on its own filesystem and sends its contents; A writes nothing into B's tree and B nothing into A's.
- **The unit runs with B's agent configuration and credentials.** What crosses the link is the task text, the unit name, the repository URL and base commit, and the report. The PR states this list and its test pins it.
- **Every refusal names what was missing**: an unknown deck, a peer that did not opt in, a peer too old, an unpushed base, a clone that failed, no deck with headroom. A dispatch that could not happen must never look like one that did.
- **Cleanup stays on the deck that owns the worktree.** B removes the worktree under PRD #220's `KeepIfDirty` policy when the unit's tab closes on B. Closing the dispatcher on A evicts the return route, as it does locally today, and does not stop the unit.

## Milestones

### Iteration 1 — explicit targeting works end to end

- [ ] **M1 — The peer link.** A daemon opens, holds and re-establishes an outbound link to each configured dispatch peer over the existing ssh transport, performs the capability handshake, and reports each peer's state (connected, unreachable with the reason, ineligible with the reason). `remote doctor` checks the unattended-key requirement. Protocol and socket tests for the handshake and the capability gate; a test seam lets the harness link two sandboxed daemons on one host without ssh.
- [ ] **M2 — Inbound dispatch on the peer.** B authorises the request, resolves the repository into a daemon-owned clone, verifies the base commit is reachable from `origin`, cuts the worktree and spawns the unit. Each refusal in the design list above has a test.
- [ ] **M3 — The return edge across daemons.** B holds the completion report until A acknowledges it, bounded in count and age; A delivers it identity-gated into the dispatcher's pane, exactly as a local report; a dropped link is surfaced in the dispatcher's pane and the report arrives on reconnect.
- [ ] **M4 — `dispatch --deck <name>`.** The flag, peer decks in `dispatch --list-targets`, the acknowledgement naming the deck, and the dispatcher-mode seed. Local dispatch unchanged when the flag is absent, with a test that pins it.

### Iteration 2 — automatic choice, surfaces, and verification

- [ ] **M5 — `--deck auto`.** Uses PRD #1258's daemon-side headroom verdict, asked of each eligible deck over its link; picks a qualifying deck and states why; refuses with what is short when none qualifies. Blocked on #1258's M1 and M5.
- [ ] **M6 — Client surfaces.** The desktop shows a remote unit under its own deck and links it to the dispatcher that started it; the TUI's dispatcher pane shows the deck and the link state. A Playwright spec for the desktop and L1 tests for the TUI rendering (rule 4).
- [ ] **M7 — End-to-end and real-agent tests.** An L2 PTY-attached test with two sandboxed daemons: a stand-in dispatcher dispatches to the peer, the unit's worktree lands on the peer's side, and the report arrives in the dispatcher's pane. A lane-2 test in which a real Haiku dispatcher runs `dispatch --deck <peer>` and a real Haiku unit on the peer discovers a uniquely-named sentinel — the rule 4 bar, as a user actually uses it.
- [ ] **M8 — Docs, contract check and security review.** A user-facing page under `docs/` (configuring peers, both opt-ins, what crosses the link), a `docs/develop/` note on the link's lifecycle and test seam, rule 12's cross-version run with an older peer, and a written review of the authority this grants an agent (see Risks).

## Risks

- **It widens what a prompt-injected dispatcher can reach.** Today such an agent can start agents on its own host; after this, on every peer. Mitigations: both-ends opt-in, a per-peer cap on concurrent inbound units, the originating deck and dispatcher named in the peer's log and on the unit's card, and M8's review, which is where #634's execution-isolation findings are applied rather than assumed away.
- **Unattended ssh is not interactive ssh.** A daemon started by systemd or at login may have no agent socket, and a passphrase-protected key cannot be unlocked by it. Mitigated by `remote doctor`'s check and by a peer state that says exactly this instead of "unreachable".
- **Version skew between peers is routine.** Two machines are upgraded at different times. Capability gates make an older peer ineligible with a reason rather than a failure mid-dispatch; rule 12's run covers the pairing.
- **Disk on the peer is spent by a clone the user did not make.** The daemon-owned clone lives where #120's already do and is subject to the same cleanup; the docs name the location and the size to expect.
- **A lost report.** A link that never comes back strands the report on B. It is bounded rather than unbounded (issue #590 is the local version of that debt), and B's log records the drop.
- **Interaction with #468.** If dispatch becomes "spawn into a directory the agent placed", the remote case still needs the peer to place, because the agent cannot run `git` there. This PRD's placement is scoped to the remote case so the two do not conflict; whichever lands second reconciles the flag surface.

## Open questions

1. **Where does the peer list live** — in `remotes.toml` beside the ssh entry, or in the deck's own config? And is B's inbound opt-in a single switch, or an allowlist of originating decks?
2. **How does B identify the repository** — by `origin` URL alone, or should a peer be able to map a URL to an existing checkout on B instead of cloning?
3. **Should `--deck auto` ever consider only peers**, so a dispatcher can be told "never fill this laptop"?
4. **What bounds the held report** — the same limits as the local delivery ledger (issue #527), or its own?

## Success criteria

- A dispatcher started from the TUI, from the desktop, and by a schedule each dispatch a unit to a named peer deck, and the report arrives in the dispatcher's pane.
- `--deck auto` picks a deck with headroom, says why, and refuses with the shortfall when none qualifies.
- Local dispatch with no flag is unchanged, pinned by a test.
- Every refusal names what was missing; no failed dispatch is reported as a success.
- `PROTOCOL_VERSION` is unchanged, rule 12's cross-version run is recorded, and an older peer is reported ineligible rather than failing mid-dispatch.
- A real Haiku dispatcher and unit complete the flow across two decks in a lane-2 test.
