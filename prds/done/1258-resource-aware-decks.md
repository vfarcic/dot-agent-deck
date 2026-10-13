# PRD #1258: Resource-aware decks — daemon-served host utilisation, judged by health levels

**Status**: Complete (2026-10-11) — M1–M4, the health levels and the docs shipped in PR #1672. The headroom verdict and the deck recommendation moved to PRD #1264, per-agent attribution to PRD #1692 (see the 2026-10-11 decision record).
**Priority**: Medium
**Created**: 2026-09-23
**Issue**: [#1258](https://github.com/vfarcic/dot-agent-deck/issues/1258)
**Related**: [#1264](https://github.com/vfarcic/dot-agent-deck/issues/1264) (cross-deck dispatch — now owns the headroom verdict and the deck recommendation, and consumes this PRD's served numbers), [#1692](https://github.com/vfarcic/dot-agent-deck/issues/1692) (per-agent resource attribution, moved out of this PRD)

## Problem Statement

Nothing in the deck knows what its host has left, so nothing can say whether starting another agent is a good idea.

**The failure is already documented, and it does not look like a resource problem.** CLAUDE.md rule 14 exists because a full disk surfaces as `error: linking with 'cc' failed` with nothing pointing at the filesystem, and because a tmpfs holding a few GB takes that memory from `rustc` and gets a compile reaped with `signal: 9, SIGKILL`. An agent that meets either blames its own task. The same shape appears under CPU contention: #415 measured 6–7 of 40 e2e files failing in parallel against 40/40 at `-j 1`, and #351, #322, #701, #818 and #364 are the same contention recorded from different angles.

**It is not rare, and it is not theoretical.** In one working session on 2026-09-22/23, free disk went from 404G to 239G while three review units built, one dispatch worktree held **153G**, and a single PR's review worktree cost between 25G and 59G. The `/issue-queue` skill already carries a hand-maintained rule of thumb — "below ~100G free: do not dispatch", "the largest `target/` observed was 108G" — which is exactly the knowledge this PRD proposes the deck should hold instead of a document.

**With several decks connected there is no basis for choosing one.** The desktop holds many daemons at once (`links: HashMap<EndpointIdentity, Arc<TrustedDaemon>>`), and a remote deck's host is one the user cannot see at all. Today the deck a new agent lands on is whichever one happens to be selected.

## Solution Overview

Each daemon serves **its own host's** utilisation, judged against that deck's own thresholds, and the clients present it:

1. **The daemon measures, judges and serves.** A capability-gated request returns disk, CPU load and memory for the host the daemon runs on, and a health level (`ok`, `warn`, `critical`) beside each number. It is the only component that can: the TUI may be attached over ssh, and the desktop may be on a different machine entirely from the deck it is driving.
2. **The TUI shows one host**, in an overlay on a keybinding — the host of the daemon it is attached to, because a TUI attaches to exactly one at a time (`Endpoint` is a single `Local`/`Remote` choice, not a map).
3. **The desktop shows every deck's host at once**, because it is the component that holds several: a small button beside each deck's name, coloured by that deck's worst level, opening the numbers.

Recommending a deck when an agent starts — the headroom verdict, and its use as a default — was the fourth part of this PRD and moved to PRD #1264 on 2026-10-11, where choosing among decks is the whole subject (see the decision record).

Four commitments shape it.

**Disk leads.** CPU and memory are supporting evidence; free space where build trees land is the number that would actually have changed a decision in every incident above. "Free disk" is meaningless as one number, so the daemon reports it **per path that matters**: the deck's working root, the parent directory dispatch worktrees are created in, and the temp root the e2e harness uses.

**One measurement, not a third copy.** `machine_load_per_cpu` already exists twice — `src/test_budget.rs` and `tests/common/mod.rs` — and #1245 has just taught both of them macOS. A third implementation is the drift that #960 and #1133 were each about. This PRD consolidates rather than adds.

**No second timer.** The daemon already runs an idle monitor on a 500 ms tick with a `MAX_TABLE_AGE` freshness bound, and #1237 has just made that sampling deterministic. Host metrics are sampled on demand into a cache with a stated maximum age, on the same discipline — not on a new independent clock.

**The daemon judges, not the client.** Each daemon judges each reading against thresholds relative to its own host's totals — disk free as a share of that filesystem, available memory as a share of the total, load per core — and serves the level beside the number. Clients colour and label from the level and never recompute it, so every client of one deck agrees and a deck's own thresholds apply to its own host. A level is a per-reading health signal, not a judgement of whether the deck can take another unit: that is PRD #1264's headroom verdict, which needs a unit's expected footprint this PRD does not have.

## What the user sees

**In the TUI.** A keybinding (`m` by default) opens an overlay over the current deck: disk free and total for each watched path, load per core against the core count, memory used and available, and how old the sample is. Each judged value is green, yellow or red for `ok`, `warn` and `critical`, and a non-ok value is followed by `(warning)` or `(critical)`, so colour is never the only signal. Escape closes it. It describes the host this deck runs on, and says so — a user attached to a remote deck is looking at the remote machine.

**In the desktop.** Each deck's header carries a small **Host** button beside the deck's name, coloured by the worst level among that deck's readings and naming it in words (`Host: critical`, and in its tooltip and accessible name). It opens the same rows in a popover that Escape and a click elsewhere close. Several hosts are visible at once, one button per deck, and a deck whose daemon is too old to answer opens a popover saying "not available from this deck" instead of showing zeros. An older daemon that sends numbers without levels shows them uncoloured, never as `ok`.

**Configuring it.** The thresholds are per deck, in that deck's `config.toml` under `[host_metrics]` (or `dot-agent-deck config set host_metrics.<key> <value>`):

| key | default | the level is… |
| --- | --- | --- |
| `disk_free_warn_percent` | 15 | `warn` below this % of the filesystem free |
| `disk_free_critical_percent` | 5 | `critical` below this % free |
| `memory_available_warn_percent` | 20 | `warn` below this % of memory available |
| `memory_available_critical_percent` | 10 | `critical` below this % available |
| `load_per_core_warn` | 0.8 | `warn` above this 1-minute load per core |
| `load_per_core_critical` | 1.5 | `critical` above this load per core |

An invalid value warns in the daemon's log and falls back to its own default.

## Scope

### In scope

- A daemon-side sampler and a capability-gated request returning host disk, CPU load and memory, on Linux and macOS.
- Consolidating the two existing `machine_load_per_cpu` implementations into the one the daemon uses.
- The TUI overlay and its keybinding, with the customisation path every other binding has.
- The desktop's per-deck display, several decks at once.
- Per-reading health levels, computed by each daemon against per-deck thresholds and served beside the numbers; both clients colour from them, with a word beside every non-ok value.
- Degradation against a deck whose daemon does not advertise the capability, and against one that serves no levels.
- The user-facing docs (`docs/session-management.md`, `docs/desktop/dashboard.md`) and the maintainer note (`docs/develop/host-metrics.md`).

### Out of scope

- **The headroom verdict and the deck recommendation** — whether a deck has room for one more unit, and defaulting the New agent flow and `dispatch` to a deck that does. Moved to [PRD #1264](https://github.com/vfarcic/dot-agent-deck/issues/1264) on 2026-10-11 with the open questions that only served them.
- **Per-agent attribution** — moved to [PRD #1692](https://github.com/vfarcic/dot-agent-deck/issues/1692) on 2026-10-11. It is materially harder than host totals: an agent's descendants `setsid` out of its process group, which CLAUDE.md rule 14 records as measured (an escapee found at `PPID 1` four days after its owner died), so a naive per-pane rollup undercounts in exactly the case that matters — an agent that started a build.
- **History, graphs, alerting.** One current sample, no time series, and no level that notifies: a level colours a value someone is looking at.
- **Windows.** The daemon reports `Unsupported` there (`docs/installation.md`), so the sampler is Unix.
- **Per-process memory accounting for the deck itself**, cgroup/container awareness, and GPU.

## Design decisions and constraints

- **No `PROTOCOL_VERSION` bump.** The request is a new `AttachRequest` variant gated on a new capability (`CAP_HOST_METRICS`), with the check in the client library rather than in each caller — the documented exception in `src/daemon_protocol.rs` and the `focus-gained` precedent (PRD #1105). Rule 12's question is answered explicitly in the PR, and the cross-version test is run. A **semantic** break is not expected, but the question is asked rather than assumed.
- **The daemon serves every fact.** Clients derive no path and read no `/proc` of their own, which is PRD #819's rule and what linkage-check rule 12 guards. The health level is one of those facts: the daemon serves it, and no client implements the rule that produces it.
- **Levels are additive and optional on the wire.** `level` on each disk and `load_level` / `memory_level` on the reply are optional fields of the existing reply, the additive rung of CLAUDE.md rule 18, so no `PROTOCOL_VERSION` bump. A field whose number is absent has no level. A client meeting a level it does not know reads it as none.
- **Numbers, not paths.** The response carries free/total bytes per *named role* (`working_root`, `worktree_parent`, `temp_root`), not absolute paths: a remote deck's directory layout is not the client's business, and a path is the part of this that could leak something.
- **Freshness is explicit.** Every response states the age of its sample, and the client shows it. A stale number presented as current is the defect class this repo keeps finding.
- **Degradation is designed, not discovered.** A deck that does not advertise the capability shows "not available from this deck" everywhere it would otherwise show numbers.

## Milestones

### Iteration 1 — the daemon knows, and one client shows

- [x] **M1 — The sampler and the verb.** Host disk (per named role), load per core, and memory, sampled into a cache with a stated max age and served by a capability-gated `AttachRequest`. Protocol and socket tests for the bounds, the capability gate, and the freshness field. Rule 12's contract question answered in the PR.
- [x] **M2 — One load measurement in the tree.** The daemon's implementation becomes the only one; `src/test_budget.rs` and `tests/common/mod.rs` consume it instead of carrying their own, with #1245's macOS behaviour preserved and its macOS-only test still passing.
- [x] **M3 — The TUI overlay.** A keybinding opens it, it shows the attached deck's host with the sample age, and it is customisable like every other binding. L1 render tests, a catalog entry, and the keyboard-shortcuts doc updated.

### Iteration 2 — several decks, and the health levels

- [x] **M4 — The desktop surface.** Per-deck utilisation on the overview, several hosts at once, and the "not available from this deck" state for an older daemon. A Playwright spec for the surface. Reshaped on 2026-10-11 from a panel on each deck into a compact button beside the deck's name that opens the numbers in a popover.
- [x] **M5 — Health levels.** Each daemon judges disk free per role, available memory and load per core against per-deck thresholds (`[host_metrics]`, defaults 15/5 %, 20/10 % and 0.8/1.5) and serves an `ok` / `warn` / `critical` level beside each number as an additive optional field; invalid thresholds warn and fall back per value. Both clients colour from it — the TUI with named terminal colours, the desktop with its theme's green/amber/red families — and write `(warning)` or `(critical)` beside every non-ok value; the desktop's button takes the worst. An older daemon's numbers show uncoloured.
- [x] **M6 — Docs.** The user-facing pages (`docs/session-management.md`, `docs/desktop/dashboard.md`) and the maintainer note on the sampler's cost and the capability's degradation (`docs/develop/host-metrics.md`). This was the docs half of the former M7.

The former M5 (the headroom verdict), M6 (recommend and default) and M7's real-agent recommending-flow test moved to PRD #1264, and M8 (per-agent attribution) to PRD #1692 — see the decision record.

## Risks

- **The thresholds are wrong for a given host.** Mitigated by making them per deck and by levels that only colour a value someone is looking at, so a bad default costs a glance and is changed in one config file.
- **Sampling costs more than it saves.** A `statvfs` per watched path plus a load read is cheap, but doing it per render is not. The cache and its max age are the mitigation, and the PR states the measured cost.
- **A third timer creeps in.** The daemon's timing paths have produced several flakes (#1133, #818, #364). M1 samples on demand rather than on a new clock, and any test for it is event-sequenced, per #1237's pattern.
- **Portability.** Load is Linux+macOS (unified by #1245); disk is `statvfs` on both; memory differs most (`/proc/meminfo` on Linux; `sysctl hw.memsize` and `host_statistics64` on macOS, with available memory as Activity Monitor's complement of "Memory Used"). The sampler degrades per field rather than per platform: a field it cannot read is absent, not zero.
- **Remote decks make this more valuable and more surprising.** The numbers describe the daemon's host, which is the point, and every surface says whose host it is.

## Open questions

1. **Where do the watched paths come from?** Settled for this PRD: three fixed roles (`working_root`, `worktree_parent`, `temp_root`), not configurable.
4. **Does the TUI overlay show remote decks the user has configured**, or strictly the attached one? Settled: strictly the attached one.

Questions 2 (one unit's disk headroom), 3 (whether `dispatch` warns or refuses below the floor) and 5 (the default available-memory floor) served only the headroom verdict and moved with it to PRD #1264 on 2026-10-11.

## Success criteria

- A daemon reports its host's disk, load and memory, with a sample age, on Linux and macOS, and an older daemon degrades visibly rather than silently.
- The TUI overlay and the desktop surface both show it, and both name whose host it is.
- Each reading's health level is computed and served by the daemon against that deck's thresholds, both clients colour from it with a word beside every non-ok value, and no client recomputes it.
- `PROTOCOL_VERSION` is unchanged, and rule 12's cross-version test has been run and recorded.
- Exactly one load-average implementation remains in the tree.
- The sampler's cost is measured and stated, and no new timer was added.

## Decision record

### 2026-10-11 — scope reshaped after the maintainer tested the branch on macOS

- **Health levels, computed by the daemon.** Each value is judged relative to the host's own totals and colour-coded: disk free per role (warn < 15 %, critical < 5 % of that filesystem), memory available (warn < 20 %, critical < 10 % of the total) and load per core (warn > 0.8, critical > 1.5). The daemon serves the level beside the number as an additive optional field, configurable per deck under `[host_metrics]`, invalid values falling back to the default with a warning. Clients never recompute a level; an older daemon's numbers show uncoloured, not as `ok`. Colour is never the only signal: a non-ok value carries `(warning)` or `(critical)` in both clients (rule 22). The desktop uses its theme's colour families, the TUI named terminal colours.
- **Memory on macOS.** The sampler reads it: the total from `sysctl hw.memsize`, available memory as the total less Activity Monitor's "Memory Used" (App Memory + Wired + Compressed) from `host_statistics64(HOST_VM_INFO64)`.
- **The desktop panel became a button.** The per-deck panel took too much of the overview; a compact button beside the deck's name, coloured by the worst level, opens the numbers in a popover.
- **Plain letters in command mode.** Shipped in the same PR at the maintainer's request, though not part of this PRD's subject: each default `Ctrl+<letter>` global action also answers to the plain letter in command mode (`d`, `n`, `w`, `t`, `e`, `z`; `n` still denies a pending permission first; no `l`, no `c`), only while the action is on its default chord and the letter is free.
- **Scope cut to M1–M4, the health levels and the docs.** The headroom verdict (former M5), recommend-and-default (former M6), the real-agent recommending-flow test (former M7's other half) and open questions 2, 3 and 5 moved to [PRD #1264](https://github.com/vfarcic/dot-agent-deck/issues/1264), which already chooses among decks and owns the footprint estimate; #1264 now depends on this PRD only for the served numbers. Per-agent attribution (former M8) moved to [PRD #1692](https://github.com/vfarcic/dot-agent-deck/issues/1692). This PRD is complete with PR #1672.

## Progress log

### 2026-10-11 — health levels, macOS memory, the desktop button, plain letters (PR #1672, round 2)

- **Health levels**: `HealthLevel` and the optional `level` / `load_level` / `memory_level` fields in `src/daemon_protocol.rs`; `HealthThresholds` and `apply_health_levels` in `src/host_metrics.rs`, read per sample from `config.toml`'s `[host_metrics]` table (`DashboardConfig::host_metrics`, with `config set/get host_metrics.<key>`). No `PROTOCOL_VERSION` bump, no `CONTRACT_BREAKS` entry. Tests: `protocol/host-metrics/012`–`014`, `dashboard/host-metrics/010`, the shared copy fixture's level sample, the desktop's `HostMetricsButton.test.tsx` and `host-metrics.spec.ts`.
- **macOS memory**: `read_memory` on macOS, with `the_memory_is_measurable_on_macos` run by `build-macos` and the formula unit-tested on every platform.
- **Desktop**: `HostMetricsButton` replaces the per-deck panel.
- **Keys**: `PLAIN_LETTER_ALIASES` and `KeybindingConfig::plain_alias` in `src/keybindings.rs`; tests `keybindings/letters/001`–`003`, help and button-bar snapshots updated.

### 2026-10-10 — M1–M4 (branch `agent/dispatch-prd-1258`)

- **M1**: `src/host_metrics.rs` samples per-role disk (`working_root`, `worktree_parent`, `temp_root`; `statvfs` at the nearest existing ancestor), load per core and memory (Linux `/proc/meminfo`; absent on macOS) into a single-flight cache with `HOST_METRICS_MAX_AGE` = 2 s and no new timer. The wire types live in `src/daemon_protocol.rs`: `AttachRequest::HostMetrics` gated on `CAP_HOST_METRICS` (Unix only), an additive optional `AttachResponse::host_metrics`, and client-side reply bounds. `PROTOCOL_VERSION` is unchanged; there is no `CONTRACT_BREAKS` entry and no `.breaking.md`. Measured cost: a cold sample has a median of 105 µs and a cache hit takes 221 ns. Tests: `protocol/host-metrics/001`–`010`.
- **M2**: `host_metrics::machine_load_per_cpu` is the only load implementation; `src/test_budget.rs` and `tests/common/mod.rs` consume it, with #1245's macOS behaviour and test preserved.
- **M3**: the TUI overlay on the customisable `m` binding (`[dashboard] host_metrics`), Esc closes it. Tests: `dashboard/host-metrics/001`–`008`, `keybindings/remap/004`, `dashboard/help/002`. `dashboard/host-metrics/004` is a lane-1 PTY test with no agent, so it is not a reel clip.
- **M4**: the desktop per-deck panel on the overview, shown for each connected deck, plus the "not available from this deck" state; `desktop/e2e/host-metrics.spec.ts`. The same words come from the shared fixture `tests/fixtures/host-metrics-copy.json`, read by both clients' tests (rule 22).
- **Open questions settled for M1–M4**: Q1: three fixed roles, not yet configurable. Q4: strictly the attached deck. Q2, Q3 and Q5 belong to M5/M6 and remain open.
- **Reds met under load**: fixed in this branch, except `status_blocked_019`, quarantined pending #1664 (a hook-listener ordering race). #1665 tracks a real `codex` on the e2e login-shell PATH.
