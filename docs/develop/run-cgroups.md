# A cgroup per agent run (issue #1897)

Since issue #1897, a daemon whose systemd unit is delegated starts each agent in a cgroup of its own, and the resource collector (PRD #1859) reads that cgroup's `cpu.stat`, `memory.current` and `memory.stat` next to its environment grouping. This page records how the daemon gets a cgroup subtree it may write, why that is the `daemon install` unit with `Delegate=yes` and not one of the alternatives, what it measured, and what it leaves out. The user-facing side is in [`docs/telemetry.md`](../telemetry.md#count-detached-work-exactly-linux) and [`docs/daemon-service.md`](../daemon-service.md#what-the-service-changes); the code is `src/run_cgroup.rs` (daemon) and `src/telemetry/collector/cgroup.rs` (collector).

## Why

Environment grouping (`docs/develop/telemetry.md`, "Why environment grouping and not a cgroup per run") is exact for work that stays in a run's process tree, and undercounts short-lived work that detaches from it: a double-forked 4 s burn was 22% visible on average at the 15 s default, and one that also cleared its environment about 0.5%. A cgroup holds every process forked inside it, whatever it does with its parent or environment, and reading it costs microseconds against the `/proc` sweep's hundreds of milliseconds.

## The decision: `Delegate=yes` on the unit `daemon install` writes

**Decided 2026-11-21 by the issue #1897 unit, under the dispatch brief's rule that a decision needing the maintainer is recorded and not shipped.** It was judged not to need one, for these reasons:

- It is the issue's own proposal: "a delegated subtree under the daemon's unit".
- It changes no install procedure. `daemon install` writes one more line, and a reinstall rewrites the unit as it always did.
- It changes no process lifetime. Every cgroup the daemon makes is inside its unit's cgroup, so `systemctl --user stop`, a crash under `Restart=on-failure`, and the supervised `restart-daemon` exit (`SUPERVISED_RESTART_EXIT`) all kill exactly what they killed before.
- Every daemon not under such a unit starts its agents exactly as before. Its one new step is a `systemctl show` at start when its cgroup is a systemd unit, such as a login session's scope.

The options that would also cover a lazy-spawned daemon change lifetimes, and are left for the maintainer ([below](#options-not-taken)).

### Evidence, on the dev box (2026-11-21, systemd 259, kernel 7.0, cgroup v2)

- **The production daemon** (lazy-spawned) ran in `/user.slice/user-1000.slice/session-8038.scope`: owned by root, `Delegate=no`, `KillMode=control-group`. `mkdir` there as the user: `Permission denied`. The installed `dot-agent-deck.service` was inactive.
- **The user manager** `user@1000.service` is `Delegate=yes`, `DelegateControllers=cpu memory pids`, owned by the user, and carries `user.delegate=1`. So any unit it runs can be delegated.
- **A transient user unit with `Delegate=yes`** (`systemd-run --user -p Delegate=yes`): the process moved itself to a leaf, enabled `+cpu +memory +pids` in the unit's `cgroup.subtree_control`, made `run-a`, and moved a child into it, all unprivileged. A double-forked burn with its environment cleared was fully in `run-a/cpu.stat`. `systemctl --user stop` killed a `setsid` survivor in `run-a`, and systemd removed the whole subtree.
- **Ownership is not delegation.** A transient user unit *without* `Delegate=` also had a user-owned cgroup, and neither unit's cgroup carried an xattr; only `user@1000.service` did. So the daemon cannot tell delegation by testing whether it can write, or by a marker. It asks systemd: `systemctl [--user] show -p Delegate --value <unit>`, `--user` when the path runs through `user@<uid>.service`. Writing into a cgroup systemd still manages would fight systemd's own changes to it, which is the reason for asking at all.

## How it works

At `daemon serve` start, before any agent exists (`run_cgroup::init`, off the executor):

1. Opt-out: `DOT_AGENT_DECK_RUN_CGROUPS=0` stops here.
2. `/proc/self/cgroup` must be a single `0::<path>` line (pure cgroup v2). Hybrid and v1 hosts stop here.
3. The path's last component must be a `.service` or `.scope` unit. A daemon already in `dot-agent-deck-daemon` (one an earlier daemon started there) uses the parent. A daemon started inside another deck's agent sits in `run-<id>`, which is no unit, so it places nothing.
4. systemd must answer `yes` for that unit's `Delegate`, within 3 s.
5. The daemon moves itself into `<unit>/dot-agent-deck-daemon`, because cgroup v2 forbids processes in a cgroup that hands controllers to its children. Then it enables those of `cpu memory pids` the unit has. A process still left in the unit's cgroup makes that `EBUSY`. It is tried 10 times 100 ms apart, then once a minute by the sweep thread, and logged with the pids and names still there. In the first sandbox run something did linger (it had gone by the time it was looked for). The login-shell `PATH` capture that runs before `daemon serve` is the likely source, which is not established. Until it succeeds, a run's cgroup still has `cpu.stat`, which is in every cgroup, but no `memory.current`.

Any failure is logged at `info` ("agent runs share the daemon's cgroup: <why>") and leaves every agent where it would have been.

On each spawn (`agent_pty::spawn_with_dir`, next to `AgentProcessGroup::adopt`), `run_cgroup::place` makes `run-<deck.run.id>` (or `agent-<pid>` when telemetry is off and there is no run), and writes the child's pid into it. portable-pty 0.8.1 gives the spawn no hook before `exec`, so the move comes after the child started. Whatever the agent forked in that window stays behind, so the daemon's leaf is then searched for processes in the agent's session (portable-pty starts every agent as a session leader) or below it in ancestry, and those are moved too, up to three passes. What this can miss is a process that left the agent's session and was reparented inside those microseconds. A refused write is logged at `warn` and the agent keeps running in the daemon's cgroup.

A sweep thread removes `run-*` / `agent-*` cgroups that two sweeps 60 s apart found unpopulated (`cgroup.events`), so a run's cgroup outlives its last process by one to two minutes, longer than the collector's 30 s maximum cadence. `rmdir` refuses a cgroup that became populated in between. The module signals no agent and not the daemon; the one process it ever kills is its own `systemctl show` child, after 3 s with no answer.

### What the collector does with it

For each live run, `collector::cgroup::read` reads the `/proc/<pid>/cgroup` of up to four of its members, oldest first, and takes the first one in a cgroup named `run-<that run's id>`. It sends `deck.run.cgroup.cpu.time` (from `usage_usec`, cumulative from the run's start, or the delta since the previous read) and `deck.run.cgroup.memory` (`current` and `anon`) on the run's resource, beside the environment-grouped metrics, which are unchanged. `deck.collector.schema_version` went from `1` to `2` for the new metrics. A run with no cgroup gets nothing new. An ended run is not read, so what its detached processes use after the agent exits is not sent.

## Measured (local, not in the RWX sandbox, whose cgroup setup is not this machine's)

The branch daemon ran in a transient user unit (`systemd-run --user -p Delegate=yes`) with its own sockets, state directory, `HOME`, log and working directory under `/var/tmp`, and a dead OTLP endpoint so runs carried ids. Agents were started with `schedule run-now`, which goes through the daemon's real spawn path. The collector ran from the same build.

| Run | What the agent did | Ground truth: the burns' own `utime + stime` | `deck.run.cpu.time` (environment) | `deck.run.cgroup.cpu.time` |
| --- | --- | --- | --- | --- |
| 1 | one double-forked burn with its environment cleared; one `--once` sample after it ended | the same loop as run 2's burn A, 8.06 s | 0.0 s | 8.21 s |
| 2 | after 20 s, two double-forked burns at once: A with its environment cleared, B with it kept; sampled at `--interval 15` throughout | A 8.06 s + B 7.07 s = 15.13 s | 0.0 s at every sample | 15.18 s from the first sample after the burns |

The cgroup's small excess over the burns is the agent's own shell, `sleep` and `cat`. Both burns ran between two samples, so environment grouping saw neither. Run 2's B is the "22% on average" case of #1859, here landing in a phase where it was seen 0%.

Teardown and restart, same setup:

- **`systemctl --user stop`** of the delegated unit: none of the agents' processes (shell, `sleep`, the run's leftovers) survived, and the unit's cgroup subtree was gone.
- **`kill -9` of the daemon under `Restart=on-failure`**: none of the agents survived. systemd restarted the unit (`NRestarts=1`), and the new daemon moved into its leaf again and logged the subtree. No run cgroup was left over.
- **Fail-soft**: the same build in a transient unit without `Delegate=` logged `systemd has not delegated dad-1897-nodeleg.service (Delegate=no)`, made no cgroup and started its agent normally. A daemon run from the login session logged `systemd has not delegated session-8038.scope (Delegate=no)`.

## Options not taken

Each would also cover a daemon that is not the `daemon install` service. That covers this box's production daemon, which is lazy-spawned. Each changes when processes die, so each is the maintainer's call.

- **A transient scope per agent from the user manager** (`StartTransientUnit`, the same thing `systemd-run --user --scope` does, which is what #1859 measured with). It needs no delegation, but the agents leave the daemon's cgroup. Under the service, `systemctl stop` would then no longer kill them unless each scope is bound to the service (`BindsTo=`/`PartOf=`), and a lazy daemon's agents would outlive a logout that kills the session (`KillUserProcesses=yes`). It also costs a D-Bus round trip per spawn.
- **A lazy daemon re-homing itself** into a delegated transient scope of the user manager at start. That covers every daemon, but moves the daemon itself out of the login session, so it outlives a logout that kills the session today. It also interacts with the unsupervised `restart-daemon` successor, which would start in that scope.
- **Moving the child before `exec`** (`clone3` with `CLONE_INTO_CGROUP`, or a `pre_exec` write) would close the post-spawn window above. portable-pty 0.8.1 exposes neither, the same limitation #1233 records for the working directory.

## What it does not cover

- Linux with systemd and cgroup v2 only; macOS has no cgroups (`docs/develop/telemetry.md`, "Why not macOS").
- A service installed by an earlier release has no `Delegate=yes` until `daemon install` is run again and the service restarted (which stops its agents).
- Memory an agent touched before its move stays charged to the daemon's leaf, because cgroup v2 does not move charges with a process.
- The cgroup is per run, not per pane: a respawned generation gets a new run, and so a new cgroup.
