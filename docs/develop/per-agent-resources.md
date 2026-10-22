# Per-agent resource attribution

PRD #1692 asked whether the deck should say which agent is using the host's disk, CPU and memory, beside the host totals PRD #1258 ships ([host metrics](host-metrics.md)). This page is its accounting model, what that model cannot see as measured on Linux on 2026-10-22, and the decision that followed: **do not ship per-agent figures** with the mechanisms the deck has today. The host totals stay the answer. The PRD's issue carries the decision record.

## The accounting model

The unit is a pane, and its members are the processes a **parent-pid walk** reaches from the pane's PTY child, the same walk the shell-activity signal already does ([`src/platform/proc/scan.rs`](../../src/platform/proc/scan.rs)). Membership by process group or by session is wrong: Claude Code puts its Bash-tool shell in a session of its own, so it also leaves the pane's process group. On the live deck surveyed for this page, 101 of the 173 processes the walk reached sat in a different session from their pane's root, so a `killpg`-style group would have missed 58% of them.

| Resource | What the model counts | Source |
|---|---|---|
| CPU | the sum, over live members, of `utime + stime + cutime + cstime` | `/proc/<pid>/stat` |
| Memory | the sum of PSS over live members | `/proc/<pid>/smaps_rollup` |
| Disk, as space | the growth of the pane's working directory since the pane started | a directory walk |
| Disk, as writes | `write_bytes − cancelled_write_bytes` for the pane's root | `/proc/<pid>/io` |

**CPU has to include reaped children.** A build is thousands of processes that live for less than a sampling interval. Adding up the CPU of the members a sampler happens to see, every 2 seconds, saw 62–63% of a real `cargo build` and 2% of a loop of 200 short commands. The kernel adds a child's CPU to its parent's `cutime`/`cstime` when the parent waits for it, so the self-plus-reaped sum keeps the work of every child that a member reaped, and matched the cgroup's own figure to 0.1 s in every in-tree case measured.

**Memory has to be PSS, not RSS.** Summed RSS counts every shared page once per process that maps it: during a cargo build it was 2.0–2.1× the cgroup's peak, and across the live decks' panes it was 23.6 GiB against 15.8 GiB of PSS. PSS came within 11% of the cgroup peak.

The disk model has two candidates, because neither answers the question alone. Directory growth is what fills a disk, but it is per directory, not per agent. The I/O counter is per agent, but it counts writes anywhere and keeps counting a file that is later deleted: one build wrote 198 MiB through it while its working directory grew by 113 MiB.

## What the model cannot see

Measured on the shared dev box (Linux 7.0, 16 cores, load average between 30 and 57 when read, cgroup v2), each scenario run in its own `systemd-run --user --scope`, whose cgroup's `cpu.stat` and `memory.peak` served as ground truth.

| Blind spot | Measured | Effect |
|---|---|---|
| A descendant that double-forks (`( setsid cmd & )`, how a daemon detaches) | the walk saw 0.01 s of a 4.0 s burn; the process reparented to PID 1 | CPU, memory and I/O of the escaped process are all lost, past and future, and nothing in the walk says one escaped |
| The same, with its environment cleared (`env -i`) | the pane-id environment tag saw 0.0 s of 4.0 s; the cgroup saw all of it | tagging processes by `DOT_AGENT_DECK_PANE_ID` does not recover it |
| A descendant that only calls `setsid` | the walk saw all of it | not a blind spot for a parent-pid walk; it is one for a process-group walk |
| Several agents in one working directory | 12 of the 17 panes of the busiest live deck shared their working directory with another of its panes, in two worktrees of six panes each (orchestration roles share a worktree) | directory growth cannot be split between them |
| Shared build caches | a cold `cargo build` wrote 15 MiB into `CARGO_HOME`, an identical second build wrote 0 | the first agent pays for a cache the others use; directory growth misses it entirely |
| A linked worktree's git objects | they land in the main checkout's `.git` (`git rev-parse --git-common-dir`) | credited to whichever pane runs in the main checkout |
| Files on a tmpfs (`/tmp` here) | `write_bytes` read 0 for 100 MiB written to `/tmp`, 150 MiB for the same writes to disk | invisible to the I/O counter, and to PSS, since an unmapped tmpfs file belongs to no process; it is RAM |
| Unmapped page cache | in the cgroup's figure, not in PSS | PSS understates what a build costs the host's memory |

On the live deck, the escapes found at the time of the survey were small: 9 of the 182 processes carrying a deck pane id sat outside their pane's tree, all at PPID 1, holding 42 MB of RSS and about 0.1 s of CPU each. All nine were `dot-agent-deck` processes or test binaries. A second look a few minutes later found eleven others, and each carried either a real dispatched unit's pane id, inherited by a test an agent in that pane ran (`sched-dispatch-issue-1779-7`), or a fixture pane id a test sets on a deck it starts (`pane-cli-name`). So in this repository escapes are a continuous churn rather than an accident: a daemon the deck spawns lazily detaches to PID 1 by design, and the test suites start many. The 4-day orphan recorded in [e2e temp directories](e2e-temp-dirs.md) is the long-lived version of the same shape.

macOS was not measured. It has no `/proc` and no cgroups, so none of the sources above exist there, and nothing on this page says what the equivalent figures would be.

## What it costs

- **The directory walk**: `du -sb` took 29 s and 41 s cold on two real worktrees (98 GB, 183 GB, 117k and 140k files, almost all of it `target/`), and 2.1 s warm. That is far outside the 2-second cadence the host metrics are read at, so it would need its own background job and cache.
- **PSS**: `smaps_rollup` walks the target's memory map under its `mmap_lock`, which is the class of read issue #862 took out of the shell-activity sample because it made the sample's wall time depend on every other process's lock wait. Reading it for the 195 processes in the live decks' panes took 606 ms in total, with a median of 222 µs, a p95 of 15.5 ms and a worst read of 99 ms.
- **CPU and RSS** come from `/proc/<pid>/stat`, which takes no `mmap_lock` and costs about what the existing process-table sample costs.

## The decision

Do not ship per-agent figures with these mechanisms. The PRD's own success criterion is that an agent whose descendants escaped is not shown as using less than it does without saying so, and a parent-pid walk cannot tell that a descendant escaped, so it cannot say so. The CPU model is exact for work that stays in the tree, but the work that leaves it is exactly the long-lived kind (a server, a daemon, a background build) that a user looking for the culprit is most likely to be looking for. Disk fails differently: in this deck's normal shape, several agents share one worktree, so a per-agent disk figure has nothing to divide by, and the remedy for a full disk is cleaning a worktree's build output, which stopping an agent does not do.

## What would change it

Two mechanisms close the escape gap on Linux, and both are daemon changes:

- **A cgroup per pane.** A cgroup keeps a process that double-forks or clears its environment, and its counters include reaped children, page cache and tmpfs pages the pane touched. The daemon cannot create one where it runs today: on this box it lives in a login session's scope, `session-<n>.scope`, which is not delegated (`mkdir` under it is refused). It would have to start each pane with `systemd-run --user --scope`, which works here, needs a systemd user manager (so not in most containers, and not on macOS), and gets the `cpu`, `memory` and `pids` controllers, not `io`, which is what this box's user manager delegates.
- **The daemon as a child subreaper** (`PR_SET_CHILD_SUBREAPER`). An escaped process would then reparent to the daemon rather than to PID 1, so the daemon could find escapees as its own unexpected children and read only their environments for a pane id, a cost bounded by the number of escapes. One cleared with `env -i` would still be unattributed, but it could be counted and shown as such. It changes the parent of every orphaned descendant, which the deck's test-only orphan watchdog reads through `getppid`, and the daemon would have to reap every one of them, so it needs its own design.

Disk attributed to a **worktree** rather than an agent would answer the motivating question ("whose build took 165 GB?") without the sharing problem, since a dispatched unit is a worktree. That is a different feature from this PRD's, and the walk's cost above applies to it unchanged.

The measurement scripts were throwaway and are not in the tree; each row above names the source it read, which is enough to repeat it.
