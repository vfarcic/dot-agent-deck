# The Orphan Reaper

`scripts/reap-orphans.sh` kills processes this machine's agent tooling has left behind. It is machine-level and knows nothing about the deck — no panes, no roles, no daemon protocol, only `/proc`. Install it as a systemd **user** timer with `scripts/install-reaper-timer.sh` (no root required).

It is a **net, not a fix.** The leak that motivated it ([issue #1015](https://github.com/vfarcic/dot-agent-deck/issues/1015)) was fixed at the source by deleting the polling MCP server it came from, so for that cause the reaper should never fire again. It exists because that was not the only orphan on the box, and will not be the last.

## What it found the day it was written

Three orphaned processes, none related to each other, on a machine whose owner believed everything was idle:

- an orphaned `telegram-mcp-bot` at **~104% of one core for 1 day 12 hours** — about 78% of all user CPU on the box, holding 860 MB RSS against 63 MB for its healthy siblings. It ignored `SIGTERM` and needed `SIGKILL`.
- a `cc1` compiler stuck **3d20h**, whose working directory was a dispatch worktree that had since been deleted
- an e2e escapee from `agent_lifetime_bound::a_setsid_escapee_is_still_bounded_by_the_cap`, carrying `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS=1` and alive **3d21h** — roughly 340,000x its own cap, and a live instance of the gap CLAUDE.md rule 14 already documents: an escapee is bounded *"for as long as its own reaper lives"*, and that run's reaper had died.

Only the first was making noise. The other two were pure clutter, which is the point: nobody goes looking for a 0% CPU orphan.

## The three rules

Every rule is scoped to processes that are **both owned by the invoking user and orphaned (`PPid 1`)**. Nothing else is ever a candidate.

| rule | default | what it matches |
| --- | --- | --- |
| `mcp` | **on** | a positively identified stdio MCP server, orphaned, older than `--min-age` (default 10 min) |
| `stale` | **off** | any remaining orphan older than `--stale-age` (default 6h). `--include-stale` |
| `spin` | **off** | any orphan sustaining CPU at or above `--cpu` (default 50% of one core) across two samples. `--include-spin` |

`mcp` is the only rule the timer runs, and it is a **two-part positive identification**: the executable must be a node runtime (`/proc/<pid>/comm` or `argv[0]`'s basename) **and** the command line must name a known package. Matching a bare substring such as `mcp-server` anywhere in the command line would reap any unrelated script whose path merely contains it, at 0% CPU — the same defect class as the never-kill list below, pointing the other way. It matches on **shape, not CPU**, which is why it still covers the incident this script was written for: a wedged server is caught whether it is spinning or quiet. An orphaned MCP server is usually quiet (~63 MB, 0% CPU) and still immortal, because its polling loop holds the event loop open forever.

**Match against the full command line, never a truncated one.** An earlier revision matched the first 100 characters — the same string it truncated for display — and silently missed every package name sitting past the cut. npx cache paths are long, so that is the common case rather than an edge one; it was caught by a test fixture whose name fell one character past the boundary. Truncation is for printing only.

`spin` is **off by default and the timer does not use it.** It cannot tell a wedged agent from a detached build, backup or compute job you started on purpose, and the installed timer runs `--apply` — so as a default rule it would eventually SIGKILL somebody's deliberate overnight job. It remains the right tool interactively, when you are looking at a specific runaway. It samples CPU **twice**, because a single reading cannot distinguish a spin from a burst.

`stale` is **opt-in for the same reason, only more so.** "Old and orphaned" is a weak signal. On the machine it was written for, a dry run with `--include-stale` immediately surfaced a deliberate 4-day measurement script — correctly, which is exactly why it does not run by default.

## Why dry-run is the default

Everything the reaper does is irreversible, so it follows `cargo xtask clean-e2e-tmp`: it reports and exits unless you pass `--apply`. Run it bare first and read the list.

```bash
scripts/reap-orphans.sh                  # dry run, mcp only
scripts/reap-orphans.sh --include-spin   # dry run, + the CPU rule
scripts/reap-orphans.sh --include-stale  # dry run, + the age catch-all
scripts/reap-orphans.sh --apply          # actually reap
```

The installed timer **does** pass `--apply` — a timer that only ever dry-runs reports to nobody. It runs the default rule set only: neither `--include-spin` nor `--include-stale`, so nothing is ever killed automatically except a positively identified orphaned MCP server. What it did is in the journal:

```bash
scripts/install-reaper-timer.sh --status
journalctl --user -u dot-agent-deck-reap-orphans
```

## The never-kill list, and the bug that shaped it

Some processes are `PPid 1` legitimately and must never be touched. The one that matters most is **`dot-agent-deck daemon serve`**, which is orphaned *by design* — it `setsid`s away from the TUI that spawned it. Its in-memory role maps (`pane_role_map`, `pane_orchestration_map`, and the rest) exist nowhere else, so killing it abandons every running orchestration; CLAUDE.md rule 15 has the full cost. The list also covers `ssh-agent`, `gpg-agent`, the systemd/dbus/pipewire family, terminal multiplexers, display servers, and the container and network daemons.

**That list is matched against the executable name — `/proc/<pid>/comm` and `argv[0]`'s basename — never against the whole command line.** Matching the full cmdline looks more thorough and is actively wrong: it exempts any process whose *path* merely contains one of those words. This was found the only way such things are: a test spinner refused to be detected because its scratch directory sat under a checkout named `dot-agent-deck`. A real orphan under any path containing `cron`, `docker` or `screen` would have been silently immune.

## Escalation is identity-checked

`SIGTERM` first, then `SIGKILL` five seconds later for anything still alive — the original spinner ignored `SIGTERM` and needed it. But a pid that is alive five seconds later is not necessarily the *same* process: the original may have exited and the number been reused. `SIGKILL` is unblockable, so escalating on the number alone can destroy an innocent bystander. The reaper records each selected process's `starttime` and re-checks it immediately before escalating, skipping the kill and saying so if it changed.

This is a narrower thing than the recycled-PID inference [E2E temp directories](e2e-temp-dirs.md) rejects, and does not contradict it: that one tries to infer recycling across a reboot by ordering a process start against a directory timestamp, which has to bridge the wall clock. This one compares a value it read itself, seconds earlier, within one boot.

## What it deliberately does not do

It does not reap by age alone unless you ask, it does not touch other users' processes, and it does not try to identify *why* a process was orphaned. It also has no recycled-PID logic, and for the same reason `cargo xtask clean-e2e-tmp` has none — see [E2E temp directories](e2e-temp-dirs.md), where that reasoning is worked through properly.

For the e2e harness's own leftovers — temp roots, not processes — `cargo xtask clean-e2e-tmp` remains the right tool. The two are complements: it reaps directories, this reaps processes.

## Removing it

```bash
scripts/install-reaper-timer.sh --remove
```

The units are generated from templates in `scripts/systemd/` at install time, because a systemd unit needs an absolute path and this repo can live anywhere. If you move or re-clone the checkout, re-run the installer — the old unit will point at a path that no longer exists.

**Installing from a linked git worktree is the case that actually catches people**, because trying it out from a feature branch is the natural thing to do. The unit will point at the worktree and break silently — into the journal — the moment that worktree is removed. The installer warns when it detects this and installs anyway; re-run it from the main checkout once the branch is merged.
