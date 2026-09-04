# The build gate: bounding concurrent linking across worktrees

Issue #863. On a machine where several agents each build the workspace in their own worktree, nothing bounded how much building happened at once. This page is what the gate does, where it sits, why it sits there rather than in the dispatch scheduler, and how to turn it off.

## The measurement that caused it

Two concurrent `cargo test --no-run --workspace` runs from separate dispatch worktrees, plus six `claude` agents and one `opencode`, with a third build starting later, on a 16-core / 27 GiB box:

| | value |
| --- | --- |
| load average | 24-28 on 16 cores |
| PSI `cpu` | `some avg300=0.08` — the CPU was **idle** |
| PSI `io` | `some avg300=67.40`, **`full avg300=65.95`** |
| `dm-0` utilisation | 100%, `aqu-sz` 48, `w_await` 56-160 ms |
| peak concurrent `rustc` / linkers | 17 / 22 |
| processes in `D` state | 13, almost all `ld` |

`full avg300=65.95` means that for roughly two-thirds of a five-minute window, every runnable task on the box was blocked on disk. `ld` then invoked the OOM killer, which took out `systemd-resolve`, `apparmor_parser`, `upowerd` and `wpa_supplicant` and left seven failed units including `chrony`.

**Read that with PSI, not with load average.** Load average counts `D`-state tasks, so a box wedged on disk and a box saturating its CPUs look identical through it. `cpu some avg300=0.08` next to `io full avg300=65.95` is the whole diagnosis, and no amount of load-average watching would have produced it. `/proc/pressure/io` and `/proc/pressure/memory` are the files to read.

Disk *space* was never the constraint (237 G of 914 G used) and neither was tmpfs (2.9 G of 14 G).

## What the gate is

`scripts/build-gate.sh` is a slot semaphore over `flock(2)`. `scripts/link-gate.sh` is the seam that uses it: `.cargo/config.toml` points `target.x86_64-unknown-linux-gnu.linker` at it, so rustc runs it instead of `cc` for every link on Linux, and it takes one slot from a machine-wide pool before exec'ing the real linker driver with rustc's argv untouched.

Cargo resolves that path relative to the directory containing `.cargo`, so each worktree points at its own copy of the script — but the pool lives at a fixed path outside every worktree (`/tmp/dad-build-gate-<uid>/link`), so all of them contend for the *same* slots. That is the whole point: the quantity to bound is concurrency **across** worktrees.

That path is deliberately literal rather than derived from `$TMPDIR` or `$XDG_RUNTIME_DIR`. Both vary per process — this repository's own e2e harness relocates `TMPDIR`, and `XDG_RUNTIME_DIR` is per login session — so deriving the pool from either would hand two agents two private pools that each look like they are working while bounding nothing. If the directory cannot be made usable the gate degrades to an ungated run rather than falling back to some other location, for the same reason: a pool at a path other builds will not look in is worse than no pool.

## Why the linker, and not whole builds or the scheduler

**Not the dispatch scheduler.** The deck never invokes cargo — `grep -rn '"cargo"' src/` finds nothing. The scheduler spawns an *agent* into a worktree, and the agent decides on its own when and how often to build, so the scheduler cannot see a build to bound. What it could bound is agents, and `max_per_run` (#194) already does that. Agent count is also a poor proxy for build load in both directions: an agent spends most of its life reading and editing, while one agent running `cargo test-e2e` links about a hundred binaries by itself. And the storm includes builds no scheduler can see — a person in the main checkout, a `bacon` watch loop, rust-analyzer.

**Not whole `cargo` invocations.** A cross-process lock around each build would have bounded the storm too, but by serialising builds that mostly are not linking: a second agent would wait out a ten-minute compile to reach a link it could have run immediately. It is the same objection the issue raises against a shared `CARGO_TARGET_DIR` — concurrent builds serialise instead of running.

**The linker.** Linking is where this workspace's I/O and memory actually go: it links roughly a hundred test binaries, each statically pulling the whole dependency graph's debug info. Measured on the box above, one relink of every test binary in a *single* worktree peaked at **60 concurrent linker processes holding 8.3 GB resident** — one build, on its own, already accounting for most of a 27 GiB machine. Compilation, by contrast, is CPU-bound and cheap to run wide, and the CPU was the one resource with headroom. Bounding links bounds the resource that ran out and leaves compilation unbounded — the gate touches no `rustc` invocation that is not linking.

Neither cargo nor rustc offers a link-job limit to reuse. `-j` and `build.jobs` bound one invocation, and unrelated cargo processes share no jobserver, so the limit has to be built.

### Gating something other than links

`scripts/build-gate.sh` is a general slot semaphore and the linker is only its first caller, so a whole `cargo` invocation can be bounded the same way when that is what you want:

```
scripts/build-gate.sh --pool build --jobs 2 -- cargo test-fast
```

Pools are independent — `--pool build` and the linker's `--pool link` do not share slots — and any pool name of `[A-Za-z0-9_-]` works. This is the coarse form of the same idea, and it comes with the serialisation cost described above, which is why nothing is wired to it by default.

## The slot count, and how it is chosen

The default is derived from the machine rather than pinned to the box this was measured on: **one slot per 4 GiB of RAM, never more than the core count, never fewer than one**. Memory is the binding constraint — the failure was an OOM, and each link pipeline on this workspace held about 690 MB at peak — so the budget is a memory budget. On the 27 GiB / 16-core box that is 6 slots, roughly a 4 GB link budget, against the 22 concurrent linkers measured during the incident.

On a 4-core / 16 GiB CI runner the clamp resolves to 4 slots against cargo's own default `-j 4`, so the gate should be inert there and CI keeps building at full width — though that is arithmetic on a runner spec rather than a measurement, and a larger runner or a changed default moves it. `DAD_LINK_JOBS=0` in the job is the lever if it ever bites.

Override it with `DAD_LINK_JOBS=N`. The right value is a memory budget: multiply the slots by ~700 MB and leave room for the agents.

## What it costs, and what it buys

Measured on the same box, relinking every test binary after touching `src/lib.rs`, four runs alternating the gate off and on. **Two other dispatch agents were building unbounded in their own worktrees throughout**, which is why the table separates what is attributable to this worktree from what is not — and why the wall-clock column is reported rather than concluded from.

| run | gate | this worktree: peak linker processes | peak resident linker memory | other worktrees' linkers, mean / peak | wall | box `io full avg10`, mean |
| --- | --- | --- | --- | --- | --- | --- |
| A | off | 60 | 8286 MB | – / ~12 | 223 s | 68.67 |
| B | on, 6 slots | 24 | 4129 MB | – / ~43 | 386 s | 59.79 |
| C | off | 60 | 8611 MB | 7.6 / 20 | 447 s | 70.45 |
| D | on, 6 slots | 24 | 3794 MB | 36.7 / 80 | 371 s | 65.50 |

**What the gate demonstrably does** is the first two columns, and they reproduce exactly: this worktree's peak linker process count is 60 ungated and 24 gated in *both* pairs, and its peak resident linker memory falls from 8.3-8.6 GB to 3.8-4.1 GB. That is the mechanism working — 6 slots, each link pipeline being a `cc` that spawns `collect2` that spawns `ld`, so 6 pipelines is ~24 processes. Verified live during a three-way build storm: with three worktrees compiling at once, this one sat at exactly its 6 slots.

**What these numbers do not establish** is an effect on wall-clock time or on box-wide pressure, and the table is laid out to make that visible rather than to bury it. Background load varied about fivefold between C and D — the gated run D faced a foreign linker mean of 36.7 against ungated C's 7.6 — so neither the wall column nor the PSI column isolates the gate. Both gated runs happen to show lower mean `io full` and D was faster than C despite far heavier competition, but on this evidence that is a coincidence of scheduling, not a result. **A single build's wall time under the gate is a real open question**: bounding links can only slow a build that is link-bound, and a rebuild that is *nothing but* links is the worst case for it. Nothing here measured that in isolation, because nothing here had an isolated box to measure it on.

The more important limitation is structural. **A box where every build is gated was never measured, because only this worktree carries the change** — the other two agents were building against `main`. What the pool guarantees is machine-wide by construction (one fixed directory, `flock(2)` across unrelated processes, covered by the tests) rather than by measurement: once every checkout has this, N concurrent builds draw from the same slots, so the machine-wide link budget stays at ~6 pipelines instead of scaling with N. Ungated, it scales — three builds at ~8.5 GB of linkers apiece is more than a 27 GiB box has, which is the OOM in the report.

## How it degrades

The gate is an optimisation on a shared box. The outcomes it must never produce are a build that fails, or one that hangs, for a reason the gate invented. Every rung below runs the command **ungated** rather than failing it:

- `scripts/build-gate.sh` missing or not executable — a partial checkout, a `noexec` mount — and `link-gate.sh` execs the real linker directly;
- no `flock` on PATH;
- the pool directory cannot be created or is not writable;
- `DAD_LINK_JOBS=0` or `off`;
- a `DAD_LINK_JOBS` that is not a number (it says so on stderr, then builds), or one above 1024, which is not a bound;
- the whole-run wait budget expires — after `DAD_BUILD_GATE_WAIT` seconds (default 900) without a slot it warns and proceeds. This is what bounds the hang: even a pool wedged by some future bug costs a delay, never a red build.

**There is no stale-lock path, by construction.** `flock(2)` locks are held by an open file description and released by the kernel when the last descriptor closes — on SIGKILL, on OOM-kill, on a power cut. A slot can never be left held by a process that is gone, which is exactly the failure a pid-in-a-file lock has and this does not. Nothing reads, writes, validates or expires a lock's contents; the slot files stay empty and are never unlinked. This matters more than usual here, because the storm the gate exists for ends in an OOM kill.

Waiters scan slots from a random offset and, when all are busy, block on a randomly chosen one with a short timeout before rescanning, so no waiter queues permanently behind one long-held slot while a different one frees.

## Escape hatches

| variable | effect |
| --- | --- |
| `DAD_LINK_JOBS=0` | run every link ungated — the kill switch |
| `DAD_LINK_JOBS=N` | use N link slots instead of the computed default |
| `DAD_LINKER=clang` | use a different linker driver |
| `DAD_BUILD_GATE_DIR=<path>` | put the pool somewhere else (per-pool subdirectories are created under it). Every build that should share a bound has to agree on this, so set it everywhere or nowhere |
| `DAD_BUILD_GATE_WAIT=<seconds>` | how long a link may wait for a slot before giving up and running ungated |

`DAD_LINKER` exists because the `linker` key in `.cargo/config.toml` overrides a `linker` set in a personal `~/.cargo/config.toml` — repository config beats `$CARGO_HOME` config — so anyone who had chosen their own linker driver needs it back.

## Platform scope

Linux only, deliberately. The `linker` key is set for `x86_64-unknown-linux-gnu` alone, so `build-windows` and `build-macos` select different target triples and never read it: a POSIX shell script cannot become the difference between a green and a red required check on a platform with no shell to run it. The Linux `build` job does go through the gate; see the slot-count section for why it is expected to be inert there and what to do if it is not.

Adding or removing the `linker` key changes every rustc command line, so the first build after either one is a full rebuild. That is a one-time cost per target directory.

## What was deliberately left undone

Issue #863 lists four candidate mitigations. Two are implemented here — the cross-worktree cap and the link-parallelism bound, which turned out to be the same mechanism at the right granularity. The other two are recorded in **#864**, along with a third lever #863 did not list, rather than shipped. All three are about not doing the work *twice*, where the gate is about not doing it all *at once*; none of them removes the need for a bound.

- **`sccache`** (option 3) and a **shared `CARGO_TARGET_DIR`** (option 4) are both about not doing the work twice, where the gate is about not doing it all at once. They are complementary to the gate, not alternatives to it, and the issue asks for them to be measured against each other rather than assumed. A shared target directory in particular is not a free win: cargo takes a lock on the target directory, so concurrent builds serialise rather than run, and per-crate artifacts live at a deterministic path, so two worktrees on different branches overwrite each other's crate and test-binary outputs and rebuild them repeatedly.
- **Debug info** is the untouched lever with the largest measured effect on both numbers in the issue — the 63 G + 39 G of build artifacts and the linker memory. Every crate is compiled `-C debuginfo=2` and every one of the ~100 test binaries statically links the whole graph's DWARF. `debug = "line-tables-only"` in a `[profile.dev]` section would cut both substantially while keeping file-and-line backtraces, at the cost of variable inspection under a debugger. That is a repo-wide trade-off about how people debug, not a resource bound, which is why it is written down in #864 instead of taken unilaterally.
