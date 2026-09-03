# Checking a Windows compile locally, before CI

## The gap this closes

`cargo test-fast`, `cargo test-e2e` and `cargo test-e2e-live` only ever compile for the host, which on every current dev machine is Linux. CI's `build-windows` job (`.github/workflows/ci.yml`) is therefore the *first* thing that ever compiles the tree for `x86_64-pc-windows-msvc` — so a Windows-only compile break is invisible locally and shows up as a red PR minutes later.

The break is almost always the same shape: a **fast-tier** test file calls a `tests/common/mod.rs` helper that is `#[cfg(unix)]` (because it is built on `std::os::unix::net::UnixStream` or `libc`). The `e2e_*.rs` suites never hit this, because the Windows job runs `cargo nextest run` **without** either e2e feature, so none of the 71 targets is compiled there at all. Issue #502 does not change that: `build-windows` names no e2e feature, so the 24 files that now also require `e2e-live` stay out for the same reason as the other 47. Fast-tier files are.

PRD #126 hit it exactly this way: `tests/idle_worker_detector.rs` called `common::attach_request_on`, and `build-windows` failed with `error[E0425]: cannot find function attach_request_on in module common`.

## The check

`cargo check` type-checks without linking, so no MSVC linker is needed — a Linux host can verify the whole workspace, tests included:

```sh
rustup target add x86_64-pc-windows-msvc   # one-time
scripts/windows-cross-check.sh
```

Extra arguments pass through to `cargo check`. Note that `--features e2e` is **not** a gate you can hold yourself to today: no `tests/e2e_*.rs` file carries a file-level `#![cfg(unix)]`, while the L2 harness helpers they all call (`spawn_daemon_serve`, `attach_request_on`, `agent_records_on`, `TuiDeck::subscribe_events`, …) are per-item `#[cfg(unix)]` — so that run reports dozens of `E0425`s that describe the L2 tier's standing Unix-only status, not anything you introduced. CI's Windows job does not compile those targets either (`cargo nextest run` **without** `--features e2e`), so nothing is being hidden. A Windows-clean L2 tier is part of #164; until then, run the check without extra features.

Three details, all handled inside the script, are the difference between this working and appearing impossible:

- **Use rustup's toolchain, not devbox's.** The devbox/nix `cargo`/`rustc` on `PATH` are Linux-only — they ship no `x86_64-pc-windows-msvc` `rust-std`, so the run dies early with a misleading `error[E0463]: can't find crate for core` *even though `rustup target add` reports the target installed*. `rustup` installed it into `~/.rustup/toolchains/...`, which the nix toolchain never consults. Pin both `RUSTC` and the `cargo` binary to the rustup toolchain.
- **Shim the C compiler and the archiver.** devbox exports `CC=gcc` and `AR=ar` globally and `cc-rs` honours both even for an MSVC target, so a native Linux toolchain gets handed a Windows cross-compile and both native-build stages break. On *compile*, `aws-lc-sys` — rustls' default `aws-lc-rs` provider — builds ~600 C files, and Linux gcc reads Linux system headers, so it dies on `unknown type name 'pthread_rwlock_t'`, because Windows has no pthreads. On *archive*, GNU `ar` is handed MSVC `lib.exe` flags and aborts on `ar: invalid option -- ':'`. Since `cargo check` never links, nothing ever reads either artefact — an object only has to be a valid archive member and an archive only has to exist — so the script fakes both: `CC` hands back one prebuilt empty object per compile (skipping the C build entirely), and `AR` rewrites `-out:X`/`-nologo` into `ar crs X …`. Both overrides are the per-target `CC_x86_64_pc_windows_msvc` / `AR_x86_64_pc_windows_msvc` spellings, so a build script compiling for the *host* still gets the real toolchain. The Rust half — type-checking against the target's pre-generated bindings — is untouched and real. (The `AR` half is the "cross-compile `lib.exe`→`ar` shim" the `build-windows` comment in `ci.yml` mentions as a Linux-only artifact; `windows-latest` compiles and archives natively and needs neither.)
- **Use a separate `CARGO_TARGET_DIR`.** The rustup and nix toolchains are different rustc versions; sharing `target/` makes each run invalidate the other's cache and forces a full rebuild of the next `cargo test-fast`. It defaults to `${XDG_CACHE_HOME:-~/.cache}/dot-agent-deck/win-check` — deliberately not under `/tmp`, which is a RAM-backed tmpfs on some machines, where this ~1 GB directory is charged against memory and swap rather than disk (it was found occupying 949 MB of a full 8 GB swap). Set `WINDOWS_CROSS_CHECK_TARGET_DIR` to put it elsewhere; delete the directory to force a cold rebuild.

Warm, this completes in about five seconds; cold, about fifteen.

Because the shims produce deliberately unusable native libraries, the script hardcodes the `check` subcommand. Do not repoint it at `build` or `test` — those link, and would link against garbage.

### Why CI runs this too

It rotted once, silently, for exactly one reason: **nothing in CI ran this script.** `build-windows` compiles natively on `windows-latest` and never invokes it, so a dependency-graph change could take the local check out with no red anywhere. #269 (reqwest 0.13, which swapped rustls' provider from `ring` to `aws-lc-rs`) did precisely that, and it went unnoticed until #368 diagnosed it.

So `ci.yml` now has a `windows-cross-check` job that runs this script on `ubuntu-latest`. It is **not** a second Windows code gate — `build-windows` owns that and does it properly, with a real build, clippy and tests on a real Windows runner. This job answers one question: *does the script itself still work?* It runs in parallel and the critical path is `build-macos`, so it costs roughly no wall-clock. Same reasoning as the `cargo xtask linkage-check` step, added after that check sat red on `main` unnoticed because it only ever ran by hand.

That job needs neither an MSVC toolchain nor devbox: the shims *supply* a toolchain rather than merely overriding devbox's `CC`/`AR` exports, so the script also works on a bare machine with both unset.

If a future dependency bump pulls in another native library the shims do not cover, that job goes red and the failure will look like a wall of C errors from a crate you have never heard of. The tell is that the compiler being invoked is plain `gcc` while the target is Windows. Fix it at the shim.

## Reading the result

Only **errors** matter. The Windows job runs `cargo clippy -- -D warnings` *without* `--all-targets` — deliberately, so a test-only lint cannot fail Windows-only and be unreproducible on the Linux pre-push gate (PRD #42 review S1). Test-target warnings (`tests/delegate_prompt_injection.rs` has had a few unused-import/unused-const warnings on Windows for a long while) do not fail CI and are not something this check asks you to fix.

The Linux `build` job is no longer symmetric with it: issue #407 moved that one to `cargo clippy --all-targets --features e2e -- -D warnings`, because the bare invocation type-checked no test file at all. Windows and macOS stayed on the narrow command for the reason above, and because the L2 tier is Unix-only in practice — `--features e2e` against the Windows target reports dozens of E0425s (#164). So a test-target lint is enforced on Linux only, which is the intended asymmetry, not drift.

## Fixing what it finds

Prefer **per-item** `#[cfg(unix)]` on the genuinely Unix-specific helpers, keeping the rest of the file compiling on Windows. PRD #42 M8 deliberately replaced a wholesale `#![cfg(unix)]` with per-item gating on five fast-tier files for exactly this reason: a blanket gate silently throws away real Windows coverage. `tests/e2e_pane_send_result.rs` and `tests/e2e_scheduler_manager.rs` show the per-item shape.

Reach for a **file-level `#![cfg(unix)]`** only when *every* test in the file is Unix-bound anyway, so there is no Windows coverage left to preserve. The tell is a harness that spawns a POSIX-shell PTY stub — `stty`, `printf`, `trap '' TERM`, `exec cat`, or any `SpawnOptions.env` pinning `SHELL=/bin/sh`. Note that `platform::shell::default_shell` returns an injected `SHELL` override **verbatim on every platform**, so a pinned `/bin/sh` is not quietly remapped to `cmd.exe` on Windows — it is spawned, fails, and the harness panics. Gating only the socket helpers in such a file trades a compile error for a runtime panic. `src/agent_pty.rs`'s `#[cfg(all(test, unix))] mod spawn_tests` and `tests/daemon_protocol.rs` are the precedents; `tests/idle_worker_detector.rs` is the PRD #126 case.

When you do gate a whole file, confirm `tests/CATALOG.md` already records those specs as `Platform coverage: mac+linux`, so the gate matches documented intent instead of silently dropping coverage. A Windows port of the PTY + named-pipe harness is tracked by #164 (M10).
