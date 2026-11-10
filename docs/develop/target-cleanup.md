# Idle `target/` cleanup

`cargo xtask clean-targets` (issue #2107) deletes the cargo `target/` directory of every worktree of this repository that nothing is using and that has not been written recently. It exists because, before it, a worktree's build tree went away only with the worktree, and a long-lived one grows with every feature set it is built under.

## Why it exists

Measured before it existed:

- On the shared Linux box, `dot-agent-deck-dispatch-prd-1666` reached **319 GB**, and `/home` went from 531 to 711 GB used in 24 hours on 2026-10-31 (#1980's round-1 telemetry, R36).
- On a macOS laptop running a two-day dispatch loop (2026-11-09), an idle finished unit's `target/` was routinely 15–145 GB. A script deleting idle trees when the disk was over a threshold reclaimed 60–160 GB at a time. That script is the prior art for this command.
- `/issue-queue` measured, on 2026-09-19, five finished units holding 292 GB between them while three units were running.

## What it deletes

For each worktree that `git worktree list --porcelain` names, the main checkout included, it considers the one path `<worktree>/target`. Bare, prunable and locked worktrees are skipped. That path is deleted only when every check below passes:

| check | kept when |
| --- | --- |
| it is cargo's | it is a symlink, not a directory, or carries neither `CACHEDIR.TAG` (with the standard signature) nor `.rustc_info.json` |
| nothing holds it | a process this user can see has a file under it open, mapped or as its executable, unless that process is `rust-analyzer` |
| no build is starting | a build tool (`cargo`, `rustc`, `rustdoc`, `clippy-driver`, a linker, `sccache`, …) is running with its working directory anywhere in the worktree |
| it is idle | anything under it was modified within `--min-idle` (default 30 minutes). Every entry is checked, not only the top levels. |
| it is big enough | it is smaller than `--min-size` (default 0, so any size) |
| the check ran | `lsof` is missing or listed nothing. Then every tree is kept. |

Sources, branches and uncommitted work are never in scope: it removes `<worktree>/target` and the trees it parked itself (below), nothing else. The checkout you run it from is never reclaimed either, because the `xtask` binary runs from that checkout's `target/`.

**Why `.rustc_info.json` as well as `CACHEDIR.TAG`.** Cargo writes the tag only when it creates `target/` itself. A tree that something else created first never gets one: on this laptop a 56 GB `target/` whose first entry was `target/nextest` had none. Cargo writes `.rustc_info.json` at a build directory's root when it caches rustc's version information, which it does by default.

**Why `rust-analyzer` does not count.** It keeps proc-macro libraries under `target/` mapped for as long as the editor session lasts, so counting it would keep every tree an editor has open. The `cargo check` it runs is a build tool like any other and does count while it runs. Deleting a tree under a running `rust-analyzer` means it rebuilds one: on the laptop 13–24 GB came back within hours in finished units' sessions. That is not prevented, only bounded by the next sweep.

**What it does not see.** Processes of other users (`lsof` as an ordinary user lists only that user's), a build tool missing from the list whose working directory is in the worktree but which has opened nothing under `target/` yet, and a `CARGO_TARGET_DIR` pointed outside the worktree (that directory is not a `<worktree>/target`, so it is never considered).

## How it deletes

Each planned tree is decided again in full immediately before it is deleted: holders, marker, idle and size. The plan can be minutes old by then, because the walks that measure the trees take time, and a short build can have started and finished in between. The tree is then renamed out of the worktree into `<git common dir>/dad-clean-targets/` (the main checkout's `.git/dad-clean-targets/`) and removed from there. A build that starts after the rename gets a new, empty `target/` instead of a half-deleted one.

**Why there.** Not inside the worktree, because `.gitignore` ignores `/target` only, so a renamed tree there would be untracked files to a `git add -A` while it is being removed. Not beside the worktree either: a parent directory shared with other repositories cannot tell this command's leftovers from theirs. Only this command writes the parking directory, so a tree that an interrupted run left there is found by its location, even when the interrupted removal already took its markers, and the next run removes it after an open-file check. The parking directory is refused if it is a symlink. A worktree on another filesystem than the common dir cannot be renamed into it, so its tree is kept and the error is reported.

A build that starts between the recheck and the rename can still lose its tree. The window is the time two system calls take, and the build then fails rather than producing a wrong result.

## Running it

```bash
cargo xtask clean-targets                                  # dry run: what would be deleted, and why each other tree is kept
cargo xtask clean-targets --apply                          # delete every idle tree
cargo xtask clean-targets --apply --min-idle 120 --min-size 60   # delete only big trees idle for two hours
```

It runs on macOS and Linux and needs `lsof` (`scripts/box/bootstrap.sh` installs it on a box). Run it locally even when `DAD_REMOTE_EXEC=rwx` is set: it acts on this machine's disk, so running it in an RWX sandbox would look at the sandbox's.

## Where it runs

| where | command | why there |
| --- | --- | --- |
| `/issue-queue` step 5, before **every** dispatch (and so `/code-cleanup`, whose step 2 applies that rule) | `--apply --min-idle 120 --min-size 60`, then, below ~100G free, `--apply` | the dispatch loop is where most trees here are made and grow, and a session is present to see what it deleted. The first command bounds growth whether or not the disk is short, which is what the 319 GB tree needed: no filesystem was near full when it grew. |
| `/tag-release` step 5, the cleanup step that also runs on its own | dry run, the user confirms, then `--apply` | the periodic sweep issue #2107 suggested: it catches trees left behind on a box where no queue runs |

**Why these thresholds.** The always-on sweep deletes a tree only when it is at least 60 GiB **and** nothing wrote to it for two hours. Two hours lets a unit waiting on CI or on a review round keep its incremental build. 60 GiB is a little under the median finished tree of the 2026-09-19 loop (~70 GB) and a little over half its largest (108 GB), so growth past that is cut while a tree of ordinary size keeps its cache. Below ~100G free the queue refuses to dispatch anyway, and then a cold rebuild in some idle worktree costs less than the stalled loop, so any idle tree goes.

**Why not a systemd timer.** The orphan reaper runs on a timer (`scripts/install-reaper-timer.sh`), but a timer is Linux-only, and this command is Rust, so a timer would also have to build the `xtask` binary first. A tree grows while something builds in it, and on this repository that is mostly dispatched units and the `rust-analyzer` sessions beside them, so the loop is where the sweep pays most. Trees built outside a loop are covered by `/tag-release`'s cleanup.

**What it costs.** Incremental build time in a reclaimed worktree: its next build starts cold. The idle and size thresholds keep that to trees nothing has written to for two hours, or any idle tree once the disk is short.
