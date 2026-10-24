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

Override it with `DAD_LINK_JOBS=N`. The right value is a memory budget: multiply the slots by ~700 MB and leave room for the agents, remembering that the desktop crate's links reach about 3 GiB under mold ([below](#what-mold-costs-in-memory)).

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
- the pool directory cannot be created, is not writable, is not a directory, or **is not owned by us** — `/tmp` is world-writable and the default path embeds only a uid, so another local user can create it first, and a squatted pool is refused rather than trusted;
- a slot path exists but is not a plain file. This one prevents a **hang** rather than an error, which is why it is checked rather than assumed: opening a FIFO for write blocks in `open(2)` until a reader arrives, before the wait budget exists and outside anything `flock -w` can time out (that timeout covers the lock, not the open), so a single FIFO planted at one of these paths would otherwise wedge every link on the machine forever;
- `DAD_LINK_JOBS=0` or `off`;
- a `DAD_LINK_JOBS` that is not a number (it says so on stderr, then builds), or one above 1024, which is not a bound;
- the whole-run wait budget expires — after `DAD_BUILD_GATE_WAIT` seconds (default 900) without a slot it warns and proceeds. This is what bounds the hang: even a pool wedged by some future bug costs a delay, never a red build.

**`scripts/link-gate.sh` itself is the one exception, and it is not a degradation — it is a build input.** `.cargo/config.toml` names it as the *linker*, so rustc resolves the path before it links anything and an absent file is a hard `error: linker '…/scripts/link-gate.sh' not found` on the first crate with a build script, with nothing of ours having run yet. Everything in the ladder above happens *inside* the two scripts, which presupposes the seam exists. Treat it like `build.rs`: on Linux this repository does not build without it. That is not hypothetical — it is how CI's `nix` job failed when the gate first landed, because `flake.nix`'s source fileset did not include `scripts/`, and it is why that fileset now names both files and why a test asserts `.cargo/config.toml` still points at them.

**There is no stale-lock path, by construction.** `flock(2)` locks are held by an open file description and released by the kernel when the last descriptor closes — on SIGKILL, on OOM-kill, on a power cut. A slot can never be left held by a process that is gone, which is exactly the failure a pid-in-a-file lock has and this does not. Nothing reads, writes, validates or expires a lock's contents; the slot files stay empty and are never unlinked. This matters more than usual here, because the storm the gate exists for ends in an OOM kill.

The pool is also created with a private umask, so a base we do own cannot then have slot entries planted in it. (The FIFO rung came from Greptile's P1 on PR #866, and it was a real defect: a bare append to a FIFO was measured blocking indefinitely.)

Waiters scan slots from a random offset and, when all are busy, block on a randomly chosen one with a short timeout before rescanning, so no waiter queues permanently behind one long-held slot while a different one frees.

## Escape hatches

| variable | effect |
| --- | --- |
| `DAD_LINK_JOBS=0` | run every link ungated — the kill switch |
| `DAD_LINK_JOBS=N` | use N link slots instead of the computed default |
| `DAD_LINKER=clang` | use a different linker driver. Also suppresses the mold injection below — naming your own driver means getting it unmodified |
| `DAD_NO_MOLD=1` | keep the default linker even where mold is installed, for bisecting a suspected linker difference. Only the exact value `1` opts out |
| `DAD_BUILD_GATE_DIR=<path>` | put the pool somewhere else (per-pool subdirectories are created under it). Every build that should share a bound has to agree on this, so set it everywhere or nowhere |
| `DAD_BUILD_GATE_WAIT=<seconds>` | how long a link may wait for a slot before giving up and running ungated |

`DAD_LINKER` exists because the `linker` key in `.cargo/config.toml` overrides a `linker` set in a personal `~/.cargo/config.toml` — repository config beats `$CARGO_HOME` config — so anyone who had chosen their own linker driver needs it back.

`DAD_NO_MOLD` exists because `link-gate.sh` prepends `-fuse-ld=mold` to rustc's argv when mold is on `PATH` (issue #906) — mold links this workspace's ~130 test binaries with far less I/O and in much less time (PR #926 measured 2.2× faster and 4.8× less I/O), though not with less memory ([below](#what-mold-costs-in-memory)). **Detected at run time rather than set in `RUSTFLAGS`**, and that is a failure mode rather than a preference: `-C link-arg=-fuse-ld=mold` in `.cargo/config.toml` would hard-fail every build on a host without mold with `cannot find -fuse-ld=mold`, including a plain `cargo build` outside a devbox shell. Detection keeps a mold-less host building exactly as before, which is the same contract every other rung of the gate honours — degrade to working, never fail for a reason the gate invented. `devbox.json` pins mold for a devbox shell; CI links with rust-lld instead ([below](#which-linker-each-environment-uses)).

The flag is **prepended**, not appended, because `-fuse-ld=` is last-wins: appending would silently override a `-fuse-ld` rustc itself passed instead of deferring to it. All of that is pinned by tests in `xtask/linkage-check/src/build_gate.rs`, which build a `PATH` holding only the tools the scripts use and so decide mold's presence themselves — the ambient `PATH` would otherwise assert the opposite condition inside a devbox shell than outside one.

### Which linker each environment uses

The prepended flag means mold applies only where rustc chooses no linker of its own, and that is not the same everywhere:

| where | toolchain | links with | why |
| --- | --- | --- | --- |
| a devbox shell | nixpkgs `rustc`, which ships no `rust-lld` (its `lib/rustlib/x86_64-unknown-linux-gnu/bin` holds only `rust-objcopy`) | **mold** | rustc passes no `-fuse-ld`, so the flag `link-gate.sh` prepends decides |
| CI: GitHub Actions and RWX | the official 1.99.0 toolchain (`dtolnay/rust-toolchain`, `rust-lang/install`), which bundles `rust-lld` | **rust-lld** | since Rust 1.90 rustc links `x86_64-unknown-linux-gnu` with its bundled LLD and passes `-fuse-ld=lld` itself, which wins over the prepended flag |
| any other host | whatever its `rustc` ships | rust-lld with an official toolchain of 1.90 or later, otherwise mold where it is on `PATH`, otherwise the system `ld` | the same two rules |

Measured on RWX on 2026-10-23 (issue #906): a default `cargo build` with that toolchain records `Linker: LLD 23.1.1` in its binary's `.comment` section, and an RWX run of `.rwx/ci.yml`'s `test` task with mold installed on `PATH` produced a `dot-agent-deck` whose `.comment` named no mold. GitHub Actions installs the same official toolchain through rustup, so the same default applies there; that half is inferred, not measured. So CI has had a fast linker without any CI change, and installing mold there would change nothing while rustc picks LLD. `readelf -p .comment <binary>` shows which linker produced a binary.

### What mold costs in memory

mold is faster than the system `ld` and also holds **more** memory per link, not less, so it is no reason to raise the slot count. Measured on 2026-10-23 in one worktree on the 16-core / 62 GiB dev box, relinking every test binary after touching `src/lib.rs`, each link run under `/usr/bin/time` with mold's `--no-fork` so its working process is the one counted:

| | links | summed link wall time | mean peak RSS | p90 | largest (a `dot-agent-deck-desktop` binary) |
| --- | --- | --- | --- | --- | --- |
| mold 3.0.0 | 257 | 311 s | 606 MiB | 1360 MiB | 3052 MiB in 10.6 s |
| GNU ld 2.46, BFD (`DAD_NO_MOLD=1`) | 256 | 1110 s | 330 MiB | 754 MiB | 1906 MiB in 34.6 s |

The box was shared with other dispatched units throughout (load average 23–47 during the mold build, 47–20 during the `ld` one), so treat the time ratio (about 3.6×) as approximate. Peak RSS depends far less on load. mold maps its input files, so part of that resident memory is file-backed page cache the kernel can reclaim, not all of it anonymous memory. The 4 GiB-per-slot budget still covers the largest link measured, so it stands as it is.

## Platform scope

Linux only, deliberately. The `linker` key is set for `x86_64-unknown-linux-gnu` alone, so `build-windows` and `build-macos` select different target triples and never read it: a POSIX shell script cannot become the difference between a green and a red required check on a platform with no shell to run it. The Linux `build` job does go through the gate; see the slot-count section for why it is expected to be inert there and what to do if it is not.

Adding or removing the `linker` key changes every rustc command line, so the first build after either one is a full rebuild. That is a one-time cost per target directory.

## Not doing the work twice: debug info, sccache, a shared target directory, seeded worktrees

Issue #863 listed four candidate mitigations and shipped two, the cross-worktree cap and the link-parallelism bound, which turned out to be the same mechanism at the right granularity. Issues #864 and #927 asked for the rest to be measured rather than assumed: `sccache`, a shared `CARGO_TARGET_DIR`, pre-warmed dispatch worktrees, and debug info, which #863 did not list. The gate is about not doing the work all *at once*; these are about not doing it *twice*, so none of them would remove the need for a bound.

**The outcome: split debug info is on for Linux, and the other three are declined.** `.cargo/config.toml` passes `-C split-debuginfo=unpacked` for `x86_64-unknown-linux-gnu`, which roughly halves a target directory and the largest link's memory while gdb still shows local variables. `sccache`, a shared target directory and seeding a worktree from a warm one each turned out to reuse nothing across worktrees as the repository is configured today, and the first two would cost more than they saved even if they did.

All of it was measured on 2026-10-23/24 on the 16-core / 62 GiB dev box, in a devbox shell (nixpkgs `rustc` 1.99.0, links by mold), sharing the box with other dispatched units throughout. Each number below carries the load average it was measured at; the wall-clock numbers move by about 20% with that load, and the sizes and memory peaks barely move at all. Peak link memory comes from running each link under `/usr/bin/time` with mold's `--no-fork`, through a `DAD_LINKER` wrapper, the same method as issue #906's mold measurement.

### What a fresh dispatch worktree's first gate costs

Issue #927 noted that this had been inferred from the dependency count rather than measured. A fresh worktree's first Rust gate, at the default `debug = 2`, measured over seven cold `cargo clippy` runs and six cold test builds. Some ran through `sccache`, whose compiles run under its server and so escape `/usr/bin/time`, so the CPU column comes from the runs without it:

| half | wall | CPU (user + sys) | units | `target/` afterwards |
| --- | --- | --- | --- | --- |
| `cargo clippy --workspace --all-targets --features e2e,e2e-live` | 2:33–3:39 | 1,389–1,570 s | 494 | 3.9 GB |
| `cargo nextest run --workspace --no-run` (the build half of `cargo test-fast`) | 5:10–6:00 | 2,765–2,771 s | 453 | 41 GB for both halves |

So a cold first gate is about **8 to 9.5 minutes of wall time and about 4,300 CPU-seconds** before any test runs (load averages 2–17 across those runs). The test run itself adds about 80 seconds.

**Most of that wall time is not the ~600 dependencies.** A worktree seeded with every dependency already built (the last subsection) recompiled only 12 units and still took 2:18 + 4:24 against 2:38 + 4:41 cold. The critical path is the workspace's own crates, the main `dot-agent-deck` library above all, and the ~260 links after it, so a lever that only skips the dependencies saves CPU but little wall time.

### Debug info

Each setting was measured as a cold test build into its own fresh target directory, then as a relink of every test binary after touching `src/lib.rs`, the per-task cost #863 measured. The cold builds ran at load averages of 7–28; the relinks ran three times each, interleaved, at 10–70. The default's cold build ran after `cargo clippy` had built the 41 build scripts and proc macros the two share, which gives it a small head start over the others:

| setting | `target/` after a test build | cold test build | relink of every test binary | link memory, mean / largest |
| --- | --- | --- | --- | --- |
| `debug = 2`, the default | 37 GB | 5:45 | 1:22–1:27 | 607 / 3035 MiB |
| line tables for dependencies only, `debug = 2` for the workspace | 30 GB | 5:37 | 0:54–1:24 | 505 / 2200 MiB |
| `debug = 1` | 22 GB | 5:04 | 0:55–1:16 | 397 / 1890 MiB |
| `debug = "line-tables-only"` | 15 GB | 4:21 | 0:46–1:04 | 318 / 1451 MiB |
| `debug = 2` with `split-debuginfo = "unpacked"` | 17 GB | 6:04 | 1:02–1:21 | 334 / 1551 MiB |

Line tables for the dependencies alone buy little because most of a test binary's DWARF is the workspace's: generic code from `tokio`, `serde` and the rest is instantiated inside the workspace crates and carries their debug level.

**`split-debuginfo = "unpacked"` is the one adopted, because it keeps the debugger.** rustc writes each object's DWARF to a `.dwo` file next to it and the linker no longer copies it into each binary, so the disk and link-memory savings come close to `line-tables-only`'s while the full debug info stays on disk for a debugger to read. Checked on a probe binary with gdb 17.2: with split debug info `break` and `print v` show the local variable as before, and with `line-tables-only` gdb answers `No symbol 'v' in current context`. A panic's backtrace keeps `file:line` under all three. `cargo test-fast` passed in full under both `unpacked` and `line-tables-only` (7671 passed, 2 skipped), so nothing in the fast tier depends on the debug info being inside the binary.

It is set as a Linux `rustflags` entry rather than in `[profile.dev]` because rustc accepts `unpacked` only on some targets: its target spec lists `packed` alone for Windows MSVC, and `rustc --target x86_64-pc-windows-msvc -C split-debuginfo=unpacked` fails with `` `-Csplit-debuginfo=unpacked` is unstable on this platform ``. On macOS `unpacked` is already the dev default. `cargo_config_splits_debuginfo_for_the_linux_triple_only` in `xtask/linkage-check/src/build_gate.rs` keeps it scoped. A `RUSTFLAGS` variable in the environment replaces the table's `rustflags`, so setting one turns this off for that build. Adding the flag changes every rustc command line, so each existing target directory pays one full rebuild, and CI's `Swatinem/rust-cache` keys include `.cargo/config.toml`, so the CI caches rotate once as well.

The flag reaches CI's Linux jobs too. The two that build an e2e archive differ. GitHub Actions builds its archive at `CARGO_PROFILE_DEV_DEBUG=0`, which leaves no debug info to split. RWX's (`.rwx/ci.yml`'s `e2e-build`, the one a pull request runs while `.github/ci-backend` says `rwx`) uses `line-tables-only`, and there the flag does change what ships. The binaries' line tables move into `.dwo` files, and a nextest archive carries none of them (checked on a probe crate: 0 of 54). A binary separated from its `.dwo` files still names each frame in a backtrace but loses its `file:line`; the same probe without split debug info kept it. A panic message's own location is compiled in and unaffected, and CI sets no `RUST_BACKTRACE`, so this shows only when someone reruns an RWX e2e partition with backtraces on. `RUSTFLAGS=""` in that task's environment would turn the flag off there.

### `sccache`

`sccache` 0.18.0 from nixpkgs, wired through `RUSTC_WRAPPER`, with a cold gate in one worktree and then a gate in a second worktree on a commit 12 behind:

| | Rust cache hits | other |
| --- | --- | --- |
| second worktree, `cargo clippy` | 0 of 571 | 262 calls not cacheable (`incremental`), 131 (`crate-type`) |
| second worktree, test build | 0 of 460 | 262 `incremental`, 64 `crate-type` |
| the same with `SCCACHE_BASEDIRS` listing both worktree roots | 0 | |

The C and assembler compiles from `cc` build scripts hit 100% (377 of 377); those are a small part of the build. With no Rust hit at all, dependencies and workspace crates split the same way, 0 and 0. Against a warm cache the second worktree's gate took 3:07 / 3:01 for `cargo clippy` and 5:18 / 5:34 for the test build (load averages 8–17), within the range the cold runs span. The wrapper's cold run of `cargo clippy` took 3:39, against 3:19 without it, and the cache cost 0.7 GB per worktree it served.

A probe on a single crate narrowed it down: a Rust cache entry was reused only by the same worktree building into the same target directory. The same worktree with a different target directory missed, and so did another worktree with the same target directory (which also differs in [its linker path](#why-nothing-is-reused-across-worktrees-today), so that probe does not separate the two). `SCCACHE_BASEDIRS` changed nothing for Rust, checked against an empty cache. Every dispatch worktree has its own path, so in these measurements `sccache` shared no Rust compile between them. What it can serve is the same worktree rebuilding from a cleaned target directory at the same path, which is rare here.

### Why nothing is reused across worktrees today

The `linker` key in `.cargo/config.toml` is a relative path, which cargo resolves against each checkout, so each worktree compiles with `-C linker=<its own path>/scripts/link-gate.sh`. That path evidently enters every unit's fingerprint, as three measurements show. Copying a warm target directory from one worktree into another and building there recompiled all 494 + 453 units and grew the copy to 77 GB. On a probe crate with a dependency, the same setup recompiled even the dependency in the second worktree, and with `linker = "cc"` it reused everything. So as configured, every artifact belongs to one worktree, whether the worktrees share a target directory, seed from one another or sit behind `sccache`.

That is a property of the gate's design: each worktree runs its own copy of the script, which is what makes a checkout's gate the code that checkout contains. Making the path identical in every worktree would change that, and that would only pay off if one of the levers below were worth having.

### A shared `CARGO_TARGET_DIR`

Cold `cargo clippy` in N worktrees on different commits started together, once with a target directory each and once with one shared:

| | separate target directories | one shared | `target/` total, separate / shared |
| --- | --- | --- | --- |
| N=2 | 221 s for both | 360 s: the second waited for the first's lock, then recompiled all 494 units | 7.6 GB / 7.6 GB |
| N=3 | 291 s for all three | 528 s: the builds ran one after another and each recompiled all 494 units | 12 GB / 11 GB |

Load averages rose from 4 to 20 during the N=2 pair and from 7 to 31 during the N=3 one. With nothing reusable, sharing a target directory serialises the builds and saves no disk.

**With a stable linker path it would be incorrect, not just slow.** Two worktrees of a probe crate on different commits, sharing one target directory, with the second checked out before the first built: building the second compiled nothing and produced the first commit's binary, with the first worktree's `CARGO_MANIFEST_DIR` compiled in. Cargo judges a workspace crate's sources by their mtimes against the artifact's, and an earlier checkout's sources are older than another worktree's newer build. A shared target directory is therefore declined on correctness grounds as well as on lock contention. Worktrees with different feature sets were not tested: cargo folds the feature set into each artifact's metadata hash, so those get separate files, and the decision does not rest on them.

### Pre-warmed (seeded) worktrees

To measure the best case, the linker path was made the same in two worktrees through `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER`, split debug info on. One worktree built cold, and its target directory was copied into a worktree six commits older, after touching that worktree's tracked files:

| | cold | seeded |
| --- | --- | --- |
| `cargo clippy` | 2:38, 1,408 CPU-s, 494 units | 2:18, 795 CPU-s, 12 units |
| test build | 4:41, 2,171 CPU-s, 453 units | 4:24, 1,563 CPU-s, 12 units |
| copying the 21 GB seed with `cp -a` | | 22 s with the seed in the page cache (the 44 GB `debug = 2` directory took 204 s) |

Load averages were 2–10 throughout. Seeding saves 28–44% of the CPU and 6–13% of the wall time, and costs a seed's worth of disk per worktree.

It also has two ways to go silently wrong, both measured on the probe crate:

- **A seed newer than the checkout serves stale code.** Seeded with `cp -a` into a worktree checked out before the seed was built, the build compiled nothing and ran the seed's commit; checked out after, it rebuilt correctly. A plain `cp -r` resets every mtime to the copy time, and served stale code even in a worktree checked out after the seed was built. Touching the new worktree's tracked files after seeding, as the run above did, forces the workspace crates to rebuild.
- **A hardlinked seed (`cp -al`) is corrupted by the build that uses it.** It copies instantly, but cargo rewrites fingerprint and dep-info files in place, so two interrupted builds in the seeded worktree changed 857 files in the original target directory.

**Declined for now.** The wall-time saving is small for the reason the first subsection gives, and getting the CPU saving needs three things: a linker path that is the same in every worktree, a seeding step in `dot-agent-deck dispatch`, and a protocol that keeps the seed older than the checkout. Re-open it with numbers if concurrent cold starts make CPU, rather than wall time or memory, the constraint on the box.
