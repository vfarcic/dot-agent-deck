# PRD #1264: Cross-deck dispatch — daemons connect to each other, so a dispatcher can place units on another machine's deck

**Status**: Draft — not started
**Priority**: Medium
**Created**: 2026-09-24
**Issue**: [#1264](https://github.com/vfarcic/dot-agent-deck/issues/1264)
**Depends on**: [#1258](https://github.com/vfarcic/dot-agent-deck/issues/1258) (daemon-served host utilisation and the headroom verdict — needed for the deck list's resource numbers and recommendation, not for the mesh or for explicit targeting)
**Builds on**: [#220](https://github.com/vfarcic/dot-agent-deck/issues/220) (the `dispatch` verb, dispatcher mode, and the dispatch return edge), [#76](https://github.com/vfarcic/dot-agent-deck/issues/76) (remote environments and the ssh transport), [#120](https://github.com/vfarcic/dot-agent-deck/issues/120) (daemon-owned clones and worktrees), [#1223](https://github.com/vfarcic/dot-agent-deck/issues/1223) (the per-deck `default_dir`)
**Interacts with**: [#468](https://github.com/vfarcic/dot-agent-deck/issues/468) (split dispatch into placement and spawning), [#174](https://github.com/vfarcic/dot-agent-deck/issues/174) (cross-project dispatch and return-edge addressing), [#1048](https://github.com/vfarcic/dot-agent-deck/issues/1048) (projects as deck-scoped state), [#634](https://github.com/vfarcic/dot-agent-deck/issues/634) (execution isolation and agent authority), [#632](https://github.com/vfarcic/dot-agent-deck/issues/632) (multi-user team control plane — explicitly not this)

## Problem Statement

A dispatcher can only fill the machine it runs on.

**`dispatch` is single-daemon by construction.** `handle_dispatch` (`src/dispatch.rs`) cuts a worktree as a sibling of the dispatcher's own checkout (`../<repo>-dispatch-<slug>`) on that host's filesystem, spawns the unit through that daemon's PTY registry, and records the return route in that daemon's `DispatchReturns` so the unit's completion is injected back into the dispatcher's pane. `DispatchContext` carries a local `working_dir`, the local registry and the local worktree registry, and nothing that names another deck.

**Daemons do not know about each other.** A TUI attaches to exactly one daemon (`Endpoint` is a single `Local`/`Remote` choice). The desktop holds several at once, but it is a client: it cannot be what an unattended dispatcher depends on, because a dispatcher is meant to keep working when nobody has the app open. A remote deck is reached through an `ssh -N -L` tunnel that a *client* opens; no daemon opens one.

**The cost is measured, not hypothetical.** PRD #1258 records one session in which free disk fell from 404G to 239G while three units built on one host, and a single dispatch worktree held 153G. `/issue-queue` carries a hand-maintained "below ~100G free: do not dispatch" rule. With a second machine configured as a remote deck and idle, the dispatcher still had nowhere else to put work.

## Solution Overview

Every deck knows every other deck. When a dispatcher dispatches, it shows the user the decks with their current resources and a recommendation, defaulting to its own deck, and the chosen deck places the code, runs the unit, and sends the report back.

Because every runtime decision is made in the daemon, the client does not matter: a dispatcher started from the TUI, from the desktop, or by a schedule dispatches across decks the same way. The desktop's role is **configuration** — it is the one place the user adds and removes decks — not a broker at run time.

Six commitments shape it.

**1. One deck list, configured in the desktop, held by every daemon.** The user adds and removes decks in the desktop. The desktop pushes the complete list to every daemon through a capability-gated request, and each daemon stores it in a state file of its own. `remotes.toml` is not reused: it is the user's hand-edited `connect` list, and a file two writers edit is the #828 defect class. A CLI command drives the same request, so a TUI-only user is not excluded; the desktop is the convenient place, not the only one.

**2. The list is kept current by events, not by a timer.** The list carries a revision: a counter plus the id of the writer (a desktop, or the CLI) that produced it. A writer makes each edit on top of the newest revision it has seen, with the counter one higher. The desktop pushes on every add, remove or edit; and on startup and on every reconnect to a deck, it compares that deck's stored revision with its own and pushes if the deck is behind. A deck that was offline when the list changed therefore catches up the moment it is reachable. A deck keeps the higher of two revisions, comparing the counter first and the writer id second, so when two desktops (two laptops) edit from the same starting revision without seeing each other, they produce two distinct revisions and every deck that has received both keeps the same one, whichever arrived first. The losing edit is not merged, and it is not dropped silently either: a desktop that finds a deck holding a revision it did not author adopts that list before its next edit, and names each entry whose value differs from what its own last edit set, as changed on another desktop since. An hourly push would add nothing these events do not already cover, and is not planned.

**3. Health is measured by the daemons, pairwise, at the moment it matters.** The desktop does not probe decks on anyone's behalf: "the desktop can reach B" says nothing about whether A can, an hour-old "B is up" is worthless for a choice made now, and a desktop-driven check stops when the app closes. Instead, when a dispatcher asks where to send a unit, its daemon asks every listed deck for its current resources over its own link — which it must do anyway, since the numbers the user chooses from have to be fresh — and a deck that does not answer is listed as unreachable with the reason. A link a daemon holds also tells it when it drops. The desktop shows health by asking each daemon for its view of the others, rendered as an A→B matrix. No health timer is added, on PRD #1258's no-second-timer discipline.

**4. The link is opened by the dispatcher's daemon, and the report comes back over it.** Deck A (the dispatcher's) connects to deck B over ssh. B never connects to A: the report returns over A's link, the way a daemon already streams events to any client attached to it. That is what lets a laptop, which usually nothing else can reach, dispatch to a server. If the link drops, the unit keeps running on B and B holds the report; A, which knows where B is from its list, reconnects and claims what is outstanding under its dispatch ids. B answers each id with its report, *still running*, or *no report* — evicted after being held too long, or never known — and A shows *no report* in the dispatcher's pane as a lost outcome naming the unit and the deck, so a claim never ends in silence.

**5. The user chooses the deck, with the dispatcher's own deck as the default.** The dispatcher presents the deck list — each deck's CPU load, memory, disk and running units, and the deck's own recommendation — and asks. The recommendation is the dispatcher's own deck unless the task's expected footprint does not fit there given the current workload, in which case it is the deck with the most headroom that does fit, with the reason stated. Asking is the default and also the strongest defence against a prompt-injected dispatcher; two ways past it exist for the cases where asking is wrong (see *What the user sees*).

**6. The chosen deck places the code the same way a local dispatch does.** It looks for a checkout of the repository, matched by `origin` URL rather than directory name; clones one if there is none; fetches rather than pulls; and cuts the worktree beside that checkout exactly as `derive_dispatch_paths` does locally, from the same base commit a local dispatch would use.

**The contract grows by capability-gated additions, with no `PROTOCOL_VERSION` bump** — the deck-list push, inbound dispatch, and report claim/acknowledge are new `AttachRequest` variants withheld until the peer advertises the matching capability, checked in the client library (the `focus-gained` precedent, PRD #1105, and `src/daemon_protocol.rs`'s documented exception). A deck that does not advertise them is listed as ineligible with that reason. Rule 12's question is answered in each PR rather than assumed.

## What the user sees

**Managing decks, in the desktop.** A deck-management view lists every deck, lets the user add and remove them, and carries a per-deck switch, **accepts dispatched units**: a deck with it off stays in the list and can still dispatch out, but is never offered as a target. It shows the A→B reachability matrix as the daemons report it, and for each deck the version of the list it holds. Adding a deck also sets up its keys (below), so the user configures the whole mesh from one screen.

**Keys, set up once from the desktop.** Each host generates its own ssh keypair on request, so no private key leaves its machine. The desktop collects the public keys and installs each on the other hosts through the ssh access it already has, restricted so that it reaches only the mesh endpoint and names the deck it belongs to (see *The link reaches mesh operations only* under Design decisions; verified in M2, not assumed). `dot-agent-deck remote doctor` gains the checks an unattended link needs and an interactive `connect` does not: a key the daemon can use with no `SSH_AUTH_SOCK` and no passphrase prompt, and a route that resolves *from that host* — an ssh-config alias or a jump host defined on the laptop means nothing on a server, so a list entry carries explicit routes: a default route, plus a route per source deck wherever that deck reaches it differently (another address, user, jump host, key or socket path). Every daemon still receives the same list, so commitment 2's revision covers routes too; each daemon connects with the route set for itself as the source, and with the default where none is.

**A deck nothing can reach.** A laptop without an ssh server is still in the list and can dispatch out; it is marked as unreachable from the others and is never offered as their target.

**Dispatching.** The dispatcher runs `dispatch --list-targets`, which now returns every deck with its live resources, eligibility, running units and the recommendation, rendered by the deck. The dispatcher shows that output to the user verbatim rather than retelling the numbers, and asks. The user picks; the dispatcher runs `dispatch <name> --deck <deck> --task "..."`. The acknowledgement in its pane names the deck, the base commit, and the reason for the recommendation ("dispatched `fix-auth` to deck `gpu-box`, cut from `main at c701932`: this deck has 38G free, the repo's units need ~90G").

**The two ways past asking.** The user can answer "use the recommendation for the rest of this session", after which the dispatcher dispatches with `--deck recommended` and reports each choice. A dispatcher with nobody to ask — one started by a schedule — uses `--deck recommended` from the start and reports what it chose. In both cases the per-deck cap on inbound units still applies.

**Watching, from the TUI.** The TUI stays attached to one daemon. The dispatcher's pane shows the acknowledgement and, later, the completion report, exactly as for a local unit. The unit's own pane is on the other deck and is not shown in a TUI attached to the dispatcher's; the user reaches it with `dot-agent-deck connect <deck>` or the desktop. Showing remote units inside a single-daemon TUI is out of scope.

**Watching, from the desktop.** The unit appears under its own deck like any other agent and names the dispatcher, and the deck, that started it.

**When the link drops.** The unit keeps running. The dispatcher's pane says the route to that deck is down rather than going quiet, and the report arrives when the link returns — or, when the target no longer holds it, the pane says the outcome was lost.

## How the chosen deck places the code

1. **Find a checkout** of the repository on that host, matched by the dispatcher checkout's `origin` URL, in the deck's `default_dir` (PRD #1223) and the directories that deck's agents have run in. When several match, the first under `default_dir` wins and the acknowledgement names which was used.
2. **If there is none, clone** into `default_dir`, reusing the clone-if-missing path `src/issue_dispatch_run.rs` already has. A deck with no `default_dir` and no match is ineligible for that repository, with the reason, rather than cloning somewhere arbitrary. The host needs its own git credentials for a private repository; a clone that fails is a refusal that says so.
3. **Fetch, never pull.** A pull would change the checkout's working tree, and that checkout may be on another branch, have uncommitted work, or be in use by an agent at that moment. `git fetch` brings the latest commits without touching it.
4. **Cut from the same base a local dispatch would.** Local dispatch cuts from the dispatcher's current `HEAD` (the base `describe_dispatch_base` reports). A remote dispatch uses that same commit when it is reachable from `origin` after the fetch. When it is not — unpushed work — it falls back to `origin`'s default branch and says so in the acknowledgement, so the difference is visible at the one moment the user is looking. Copying unpushed work across is out of scope.
5. **Create the worktree beside the checkout** with the same `derive_dispatch_paths` naming and branch scheme as a local dispatch, and spawn the unit into it.

## Scope

### In scope

- The versioned deck list: its daemon-side state file, the capability-gated push, the desktop's push-on-change / on-startup / on-reconnect logic, and the CLI equivalent.
- Key setup from the desktop, the per-deck accept switch, and the `remote doctor` checks for unattended links.
- Daemon-held outbound links, pairwise reachability, and the desktop's matrix.
- Inbound dispatch: checkout discovery, clone, fetch, base selection, worktree, spawn, and every refusal.
- The return edge across daemons: a report held on the target until acknowledged, the claim on reconnect, and **the dispatcher's outstanding remote dispatches persisted across its own daemon's restart** (see Design decisions).
- `dispatch --list-targets` with live resources and the recommendation; `--deck <name>` and `--deck recommended`; the repository footprint estimate; the dispatcher-mode seed teaching all of it (mechanics only, per PRD #220's seed rule).
- The desktop's deck-management view and the remote unit linked to its dispatcher; the TUI acknowledgement naming the deck.
- Degradation against a deck too old to advertise the capabilities.

### Out of scope

- **Showing remote units inside a TUI.** It would change the TUI's one-daemon model and is a separate decision.
- **Daemons passing the list to each other.** The desktop (or the CLI) is the single writer; gossip would add a question of whose list wins that the revision order otherwise answers.
- **A desktop-driven health poller.** Health is measured by the daemons at dispatch time (commitment 3).
- **Multi-user or shared decks.** Every deck belongs to the same user. Tenancy and authorisation between people are PRD #632's question.
- **Moving a running unit between decks.** A worktree is a pre-spawn decision (PRD #220); so is the deck.
- **Copying unpushed work** to another deck.
- **Orchestrations whose roles span decks.** A dispatched unit — single agent or orchestration — lives entirely on one deck.
- **Windows** as either end, since the daemon reports `Unsupported` there, and **Kubernetes-transport decks** until PRD #81 provides the transport.

## Design decisions and constraints

- **The daemon owns the runtime decision** (rule 18). A desktop broker was rejected because cross-deck dispatch would then work only while the app is open, and not from the TUI at all.
- **Knowing about a deck is not permission to use it.** Every deck knows every other, and each deck's accept switch decides whether it is offered as a target — including the dispatcher's own deck, which the user may turn off (a laptop that should never build). With it off, the recommendation becomes the default.
- **The footprint estimate comes from evidence, not from the agent's guess.** In order: a footprint the repository declares in `.dot-agent-deck.toml`; otherwise the measured size of earlier dispatch worktrees of the same repository on that deck; otherwise PRD #1258's configured headroom floor. The dispatcher may argue with the recommendation from what it knows about the task, but the numbers come from the deck.
- **Disk and memory are hard limits; CPU is soft.** Running out of disk or memory makes a build fail, so a shortfall there moves the recommendation off the dispatcher's deck. High CPU load only makes it slower, so on its own it lowers a deck in the ranking but does not disqualify it.
- **The dispatcher's pending remote dispatches must survive its daemon's restart.** Locally this has not mattered: a daemon restart stops the local units with it. A remote unit outlives the dispatcher daemon's restart, and `DispatchReturns` is in memory, so without persistence the restarted daemon has forgotten it asked and the report held on the other deck has no claimant. The route is persisted with the same identity gate delivery already uses — a report is refused, not injected, if the pane is now held by a different agent.
- **The link reaches mesh operations only, and the target knows which deck it came from without taking the request's word for it.** The attach socket is not a privilege boundary: any client reaching it can `StartAgent` an arbitrary command as the daemon's user (the trust-boundary note in `src/daemon_protocol.rs`), and the accept switch and the inbound cap are checks on the mesh requests, not on that. A key that forwarded to B's attach socket would therefore give whoever holds it arbitrary execution on B, and that includes every agent on A: agents run as the same user as A's daemon, so a key the daemon can use unattended is a key they can read. File permissions cannot close that; bounding what the key reaches can. The link therefore terminates at an endpoint on B that serves only the mesh requests (the list push, resource queries, inbound dispatch, report claim and acknowledge), each subject to B's accept switch and cap, and B takes the originating deck's identity from the key the ssh layer authenticated, not from a field in the request. The candidate mechanism is one `authorized_keys` line per peer deck carrying `restrict` and a forced command that runs a mesh-only stdio bridge naming that deck, in place of Unix-socket forwarding for this link; M2 verifies it, or the alternative it settles on, rather than assuming it. What remains is stated rather than hidden: an agent on A can still use A's key to send mesh requests to B as A, so within B's accept switch and cap it can dispatch there without the user being asked — the reach `--deck recommended` already gives it.
- **The report travels, the files do not.** The target deck reads the unit's `work-done --task-file` report on its own filesystem and sends its contents; neither deck writes into the other's tree.
- **The unit runs with its own deck's agent configuration and credentials.** What crosses the link is the deck list, resource numbers, the task text, the unit name, the repository URL and base commit, and the report. The PR states this list and a test pins it.
- **Every refusal names what was missing**: an unknown deck, a deck not accepting units, a deck too old, an unreachable deck, no checkout and no `default_dir`, a failed clone, no deck with room. A dispatch that could not happen must never look like one that did.
- **Cleanup stays on the deck that owns the worktree**, under PRD #220's `KeepIfDirty` policy when the unit's tab closes there. Closing the dispatcher evicts the return route, as it does locally today, and does not stop the unit.
- **No experimental flag** (CLAUDE.md rule 9; decided by the maintainer on 2026-09-25). The new user-visible surfaces — the desktop deck-management view and its reachability matrix, the per-deck accept switch, `dispatch --list-targets` listing other decks, and `dispatch --deck` — ship visible by default rather than behind PRD #139's `experimental` flag. The desktop app is itself still in an experimental phase without real users yet, so a second gate on top of it would mostly hide the surfaces from the people currently trying the app. Revisit this if the desktop leaves alpha before these surfaces ship. None of rule 9's wrapper, changelog note or `graduate-*` follow-up applies. The accept switch remains the per-deck control over whether a deck takes remote units at all.

## Milestones

### Iteration 1 — the mesh

- [ ] **M1 — The deck list.** A daemon-side state file, the revisioned capability-gated push (a deck keeps the higher revision, counter then writer id), the desktop pushing on change, on startup and on reconnect, and the CLI equivalent. Protocol tests for the revision order and the gate; a desktop test for the reconnect catch-up; a test in which two writers edit from the same starting revision and their pushes reach the decks in both orders, every deck ends on the same list, and the losing desktop names the entries changed elsewhere.
- [ ] **M2 — Keys and routes.** Per-host keypair generation on request, public-key installation from the desktop, the `authorized_keys` restriction (`restrict` plus a forced mesh-only command naming the peer deck, or the alternative M2 settles on) verified to reach nothing but the mesh endpoint — including a test that a request over the link cannot reach `StartAgent` or any other non-mesh request — explicit routes per entry with per-source overrides, and the new `remote doctor` checks.
- [ ] **M3 — Links and pairwise health.** A daemon opens and holds outbound links to listed decks, reports each one's state (connected, unreachable with the reason, ineligible with the reason), and serves its view to the desktop, which renders the A→B matrix. A test seam links two sandboxed daemons on one host without ssh.

### Iteration 2 — dispatching across decks

- [ ] **M4 — Inbound dispatch.** Checkout discovery by `origin` URL, clone into `default_dir`, fetch, base selection with the stated fallback, worktree via the local naming scheme, spawn, the accept switch and per-deck cap, and a test per refusal.
- [ ] **M5 — The return edge across daemons.** Reports held on the target until acknowledged and bounded in count and age; the claim on reconnect, answered per dispatch id, with an evicted or unknown id surfaced in the dispatcher's pane as a lost outcome and a test for each; the dispatcher's outstanding routes persisted and reclaimed after its daemon restarts, identity-gated; a dropped link surfaced in the dispatcher's pane.
- [ ] **M6 — Choosing the deck.** `dispatch --list-targets` with live resources and the recommendation, the footprint estimate, `--deck <name>` and `--deck recommended`, and the dispatcher-mode seed teaching ask-by-default, "use the recommendation for the rest of this session", and the unattended path. Local dispatch with no flag is unchanged, pinned by a test. The resource numbers need PRD #1258's M1; the per-deck verdict needs its M5, which must accept the caller's footprint.

### Iteration 3 — surfaces and verification

- [ ] **M7 — Client surfaces.** The desktop's deck-management view (list, add/remove, accept switch, matrix, list versions) and the remote unit linked to its dispatcher; the TUI acknowledgement naming the deck, base and reason. Playwright specs for the desktop, L1 tests for the TUI rendering (rule 4).
- [ ] **M8 — End-to-end and real-agent tests.** An L2 PTY-attached test with two sandboxed daemons: a stand-in dispatcher dispatches to the other deck, the worktree lands on that deck's side, and the report arrives in the dispatcher's pane, including across a restart of the dispatcher's daemon. A lane-2 test in which a real Haiku dispatcher lists the decks, dispatches with `--deck`, and a real Haiku unit on the other deck discovers a uniquely-named sentinel — rule 4's bar, as a user actually uses it.
- [ ] **M9 — Docs, contract check and security review.** A user-facing page under `docs/` (managing decks, keys, the accept switch, what crosses the link, the ways past asking), a `docs/develop/` note on the list's versioning, the link lifecycle and the test seam, rule 12's cross-version run with an older deck, and a written review of the authority this grants an agent.

## Risks

- **It widens what a prompt-injected dispatcher can reach** — from its own host to every deck that accepts units. Asking the user is the default and is the main control; `--deck recommended` removes it, so the per-deck cap, the originating deck and dispatcher named on the unit's card and in the target's log, and M9's review carry the unattended case. The cap is a control only because the link reaches mesh operations and nothing else (Design decisions); a link to B's attach socket would let an agent holding A's key bypass it. M9's review applies #634's findings rather than assuming them away.
- **Key setup is the most security-sensitive thing the desktop has done.** It writes to `authorized_keys` on every host. Mitigated by per-host keys that never leave their host, each restricted to a forced command that reaches only the mesh endpoint, removal of a deck's key from every other host when the deck is removed, and M2 verifying the restriction rather than asserting it.
- **Unattended ssh is not interactive ssh.** A daemon started by systemd or at login may have no agent socket and cannot unlock a passphrase-protected key. Mitigated by the dedicated per-host key and by a link state that says exactly this.
- **Routes are relative to the host that uses them.** Mitigated by explicit routes with per-source overrides and by `remote doctor` checking resolution from the host itself; the matrix makes a bad route visible.
- **Version skew between decks is routine.** Capability gates make an older deck ineligible with a reason rather than failing mid-dispatch; rule 12's run covers the pairing.
- **The footprint estimate is wrong on first contact** for a repository with no declaration and no history. It falls back to #1258's floor, the recommendation states its reason, and the user is asked by default, so a bad estimate costs a glance.
- **A lost report.** A deck that never comes back strands its held report. It is bounded (issue #590 is the local version of that debt), and the target's log records the drop. When A does reconnect after the report was evicted, the claim's answer puts the loss in the dispatcher's pane rather than only in B's log.
- **Interaction with #468.** If local dispatch becomes "spawn into a directory the agent placed", the remote case still needs the target deck to place, because the agent cannot run `git` there. Whichever lands second reconciles the flag surface.

## Open questions

1. **How does a dispatcher know it is unattended?** A schedule-spawned dispatcher could be told by the deck (its seed or an environment variable set at spawn), or the schedule could pass `--deck recommended` explicitly. The first is automatic; the second keeps the choice visible in the schedule's config.
2. **What bounds the held report** — the same limits as the local delivery ledger (issue #527), or its own?
3. **Should the measured footprint history be shared across decks**, so a repository's first dispatch to a new deck benefits from its size on another?

## Design record

**2026-09-24 — shape settled with the user.** The first draft had each deck opt in separately in its own config, `--deck auto` choosing without asking, and the target cloning from `origin` and cutting from the pushed base. Revised in discussion:

- **Configured in one place.** The user asked for decks to be configured once, in the desktop. Adopted as the versioned list the desktop pushes; reachability and credentials cannot be copied between hosts, so the desktop sets up per-host keys instead and the daemons measure reachability themselves.
- **Every daemon knows every other.** Adopted, with the accept switch separating knowledge from permission.
- **Ask the user, local by default, recommend otherwise when the task needs more than this machine has.** Adopted as the default, with two ways past asking (session-wide, and unattended) because a burst of dispatches or a scheduled dispatcher makes asking every time wrong. "Needs more" is taken from evidence rather than an agent's guess, and only disk and memory disqualify.
- **Find the repo, clone if absent, pull main, then cut the worktree as local dispatch does.** Adopted with two changes the user accepted: fetch instead of pull, so no one's checkout is touched; and the same base commit a local dispatch uses, falling back to `origin`'s default branch only for unpushed work, so one verb does not start units from different places depending on the deck.
- **Whether the dispatcher's own deck is offered depends on the user's choice.** Adopted as the per-deck accept switch.
- **Periodic propagation from the desktop** (proposed hourly) replaced by push-on-change, on-startup and on-reconnect with a version check, which covers the offline-deck case sooner; and **health** moved from the desktop to the daemons, measured pairwise at dispatch time.
- **Found during the discussion:** the dispatcher's pending remote dispatches must be persisted, because a remote unit outlives a restart of the dispatcher's daemon.

**2026-09-24 — review of PR #1265.** Four gaps closed: the list's revision orders concurrent writers (commitment 2); routes carry per-source overrides; the link reaches mesh operations only, with the originating deck's identity bound by its key rather than claimed in the request; and a report evicted before its claim is shown to the dispatcher as a lost outcome.

## Success criteria

- A deck added in the desktop appears in every other deck's list, including a deck that was offline at the time, once it reconnects; two desktops editing concurrently leave every deck on the same list, and the desktop whose edit lost says so.
- A dispatcher started from the TUI, from the desktop, and by a schedule each dispatch a unit to another deck, and the report arrives in the dispatcher's pane — including after the dispatcher's daemon restarts mid-unit.
- The user is shown every deck's live resources and a recommendation, the dispatcher's own deck is the default unless the footprint does not fit, and the reason is stated.
- The target deck uses an existing checkout when there is one, never changes its working tree, and cuts from the same base a local dispatch would, or says why not.
- Local dispatch with no flag is unchanged, pinned by a test.
- Every refusal names what was missing; no failed dispatch is reported as a success.
- `PROTOCOL_VERSION` is unchanged, rule 12's cross-version run is recorded, and an older deck is reported ineligible rather than failing mid-dispatch.
- A real Haiku dispatcher and unit complete the flow across two decks in a lane-2 test.
