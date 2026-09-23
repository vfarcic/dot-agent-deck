# PRD #1258: Resource-aware decks — daemon-served utilisation, and a deck recommendation when starting an agent

**Status**: Draft — not started
**Priority**: Medium
**Created**: 2026-09-23
**Issue**: [#1258](https://github.com/vfarcic/dot-agent-deck/issues/1258)

## Problem Statement

Nothing in the deck knows what its host has left, so nothing can say whether starting another agent is a good idea.

**The failure is already documented, and it does not look like a resource problem.** CLAUDE.md rule 14 exists because a full disk surfaces as `error: linking with 'cc' failed` with nothing pointing at the filesystem, and because a tmpfs holding a few GB takes that memory from `rustc` and gets a compile reaped with `signal: 9, SIGKILL`. An agent that meets either blames its own task. The same shape appears under CPU contention: #415 measured 6–7 of 40 e2e files failing in parallel against 40/40 at `-j 1`, and #351, #322, #701, #818 and #364 are the same contention recorded from different angles.

**It is not rare, and it is not theoretical.** In one working session on 2026-09-22/23, free disk went from 404G to 239G while three review units built, one dispatch worktree held **153G**, and a single PR's review worktree cost between 25G and 59G. The `/issue-queue` skill already carries a hand-maintained rule of thumb — "below ~100G free: do not dispatch", "the largest `target/` observed was 108G" — which is exactly the knowledge this PRD proposes the deck should hold instead of a document.

**With several decks connected there is no basis for choosing one.** The desktop holds many daemons at once (`links: HashMap<EndpointIdentity, Arc<TrustedDaemon>>`), and a remote deck's host is one the user cannot see at all. Today the deck a new agent lands on is whichever one happens to be selected.

## Solution Overview

Each daemon serves **its own host's** utilisation, and the clients present it:

1. **The daemon measures and serves.** A capability-gated request returns disk, CPU load and memory for the host the daemon runs on. It is the only component that can: the TUI may be attached over ssh, and the desktop may be on a different machine entirely from the deck it is driving.
2. **The TUI shows one host**, in an overlay on a keybinding — the host of the daemon it is attached to, because a TUI attaches to exactly one at a time (`Endpoint` is a single `Local`/`Remote` choice, not a map).
3. **The desktop compares decks**, because it is the component that holds several. Cross-deck comparison belongs here and nowhere else.
4. **Starting an agent recommends a deck and defaults to it.** The New agent flow (PRD #1223) and `dispatch` pre-select a deck that has headroom, say why, and let the user choose otherwise. Advisory, never automatic.

Four commitments shape it.

**Disk leads.** CPU and memory are supporting evidence; free space where build trees land is the number that would actually have changed a decision in every incident above. "Free disk" is meaningless as one number, so the daemon reports it **per path that matters**: the deck's working root, the parent directory dispatch worktrees are created in, and the temp root the e2e harness uses.

**One measurement, not a third copy.** `machine_load_per_cpu` already exists twice — `src/test_budget.rs` and `tests/common/mod.rs` — and #1245 has just taught both of them macOS. A third implementation is the drift that #960 and #1133 were each about. This PRD consolidates rather than adds.

**No second timer.** The daemon already runs an idle monitor on a 500 ms tick with a `MAX_TABLE_AGE` freshness bound, and #1237 has just made that sampling deterministic. Host metrics are sampled on demand into a cache with a stated maximum age, on the same discipline — not on a new independent clock.

**The recommendation is a headroom test, not a score.** Cost here is bursty: a cold build saturates every core for minutes and then stops, so an instantaneous average ranks badly. A deck qualifies when it has at least one unit's disk headroom and is under a load ceiling; the answer is a qualified/not-qualified verdict with the reason, and "no deck qualifies" is a first-class outcome that says what is short rather than picking the least bad.

## What the user sees

**In the TUI.** A keybinding opens an overlay over the current deck: disk free and total for each watched path, load per core against the core count, memory used and available, and how old the sample is. Escape closes it. It describes the host this deck runs on, and says so — a user attached to a remote deck is looking at the remote machine.

**In the desktop.** Each deck on the overview carries its own utilisation, so several hosts are visible at once, and a deck whose daemon is too old to answer says exactly that instead of showing zeros.

**When starting an agent.** The deck step of the New agent flow marks each deck with whether it has room, defaults to a deck that does, and states the reason next to it ("412G free, load 1.2/16"). Choosing a deck with no headroom is possible and warns rather than blocks. `dispatch` prints the same verdict for the deck it is about to use.

## Scope

### In scope

- A daemon-side sampler and a capability-gated request returning host disk, CPU load and memory.
- Consolidating the two existing `machine_load_per_cpu` implementations into the one the daemon uses.
- The TUI overlay and its keybinding, with the customisation path every other binding has.
- The desktop's per-deck display and cross-deck comparison.
- The headroom verdict, and its use as a default in the New agent flow and in `dispatch`.
- Degradation against a deck whose daemon does not advertise the capability.

### Out of scope

- **Per-agent attribution** — deferred to iteration 3 and argued there rather than assumed. It is materially harder than host totals: an agent's descendants `setsid` out of its process group, which CLAUDE.md rule 14 records as measured (an escapee found at `PPID 1` four days after its owner died), so a naive per-pane rollup undercounts in exactly the case that matters — an agent that started a build. Iteration 1 and 2 show the *host's* numbers for a selected agent and label them as such.
- **Automatic placement.** The recommendation never routes a spawn by itself. The heuristic is unproven; a wrong advisory costs a glance, a wrong auto-route costs a run and is invisible until it fails.
- **History, graphs, alerting.** One current sample, no time series, no thresholds that notify.
- **Windows.** The daemon reports `Unsupported` there (`docs/installation.md`), so the sampler is Unix.
- **Per-process memory accounting for the deck itself**, cgroup/container awareness, and GPU.

## Design decisions and constraints

- **No `PROTOCOL_VERSION` bump.** The request is a new `AttachRequest` variant gated on a new capability (`CAP_HOST_METRICS`), with the check in the client library rather than in each caller — the documented exception in `src/daemon_protocol.rs` and the `focus-gained` precedent (PRD #1105). Rule 12's question is answered explicitly in the PR, and the cross-version test is run. A **semantic** break is not expected, but the question is asked rather than assumed.
- **The daemon serves every fact.** Clients derive no path and read no `/proc` of their own, which is PRD #819's rule and what linkage-check rule 12 guards.
- **Numbers, not paths.** The response carries free/total bytes per *named role* (`working_root`, `worktree_parent`, `temp_root`), not absolute paths: a remote deck's directory layout is not the client's business, and a path is the part of this that could leak something.
- **Freshness is explicit.** Every response states the age of its sample, and the client shows it. A stale number presented as current is the defect class this repo keeps finding.
- **Degradation is designed, not discovered.** A deck that does not advertise the capability shows "not available from this deck" everywhere it would otherwise show numbers, and never qualifies or disqualifies itself in the recommendation.

## Milestones

### Iteration 1 — the daemon knows, and one client shows

- [ ] **M1 — The sampler and the verb.** Host disk (per named role), load per core, and memory, sampled into a cache with a stated max age and served by a capability-gated `AttachRequest`. Protocol and socket tests for the bounds, the capability gate, and the freshness field. Rule 12's contract question answered in the PR.
- [ ] **M2 — One load measurement in the tree.** The daemon's implementation becomes the only one; `src/test_budget.rs` and `tests/common/mod.rs` consume it instead of carrying their own, with #1245's macOS behaviour preserved and its macOS-only test still passing.
- [ ] **M3 — The TUI overlay.** A keybinding opens it, it shows the attached deck's host with the sample age, and it is customisable like every other binding. L1 render tests, a catalog entry, and the keyboard-shortcuts doc updated.

### Iteration 2 — several decks, and the recommendation

- [ ] **M4 — The desktop surface.** Per-deck utilisation on the overview, several hosts at once, and the "not available from this deck" state for an older daemon. A Playwright spec for the surface.
- [ ] **M5 — The headroom verdict.** A qualified/not-qualified answer per deck with its reason, derived from disk headroom and a load ceiling, both configurable and both defaulting to values taken from the measurements in this document rather than invented. "No deck qualifies" states what is short.
- [ ] **M6 — Recommend and default.** The New agent flow's deck step and `dispatch` pre-select a qualifying deck and show the reason; choosing another warns and proceeds. An L2 test for the TUI/dispatch path and a Playwright spec for the desktop one.

### Iteration 3 — verified, documented, and the harder half

- [ ] **M7 — Real-agent scenario and docs.** A lane-2 test that starts a real agent through the recommending flow (rule 4's bar: as a user actually uses it), a user-facing docs page, and the `docs/develop/` note covering the sampler's cost and the capability's degradation.
- [ ] **M8 — Per-agent attribution, or the argument against it.** A stated accounting model for a pane's process tree, its measured blind spot (descendants that `setsid` away), and either an implementation that names its own limits or a recorded decision not to ship one.

## Risks

- **The heuristic is wrong on first contact.** Mitigated by being advisory and by stating the reason next to the verdict, so a bad call is visible and arguable rather than silent. M5's defaults come from measured numbers in this repo, not from taste.
- **Sampling costs more than it saves.** A `statvfs` per watched path plus a load read is cheap, but doing it per render is not. The cache and its max age are the mitigation, and the PR states the measured cost.
- **A third timer creeps in.** The daemon's timing paths have produced several flakes (#1133, #818, #364). M1 samples on demand rather than on a new clock, and any test for it is event-sequenced, per #1237's pattern.
- **Portability.** Load is Linux+macOS (unified by #1245); disk is `statvfs` on both; memory differs most (`/proc/meminfo` vs `host_statistics64`). The sampler degrades per field rather than per platform: a field it cannot read is absent, not zero.
- **Remote decks make this more valuable and more surprising.** The numbers describe the daemon's host, which is the point, and every surface says whose host it is.

## Open questions

1. **Where do the watched paths come from?** The deck's working root is known; the worktree parent is `..` by convention (`../<repo>-dispatch-<name>`); the temp root may be moved by `DAD_E2E_TMPDIR`. Are these three fixed roles, or configurable?
2. **What is "one unit's disk headroom"?** The `/issue-queue` skill says ~90G from observed `target/` sizes of 70–108G. Is that the default, and is it per-deck configurable?
3. **Does `dispatch` warn or refuse below the floor?** This PRD says warn. A refusal with an override flag is the alternative.
4. **Does the TUI overlay show remote decks the user has configured**, or strictly the attached one? Strictly attached is the assumption here.

## Success criteria

- A daemon reports its host's disk, load and memory, with a sample age, and an older daemon degrades visibly rather than silently.
- The TUI overlay and the desktop surface both show it, and both name whose host it is.
- Starting an agent defaults to a deck with headroom and says why, and "no deck qualifies" explains what is short.
- `PROTOCOL_VERSION` is unchanged, and rule 12's cross-version test has been run and recorded.
- Exactly one load-average implementation remains in the tree.
- The sampler's cost is measured and stated, and no new timer was added.
