# Checking a Windows compile locally, before CI

## The gap this closes

`cargo test-fast` and `cargo test-e2e` only ever compile for the host, which on every current dev machine is Linux. CI's `build-windows` job (`.github/workflows/ci.yml`) is therefore the *first* thing that ever compiles the tree for `x86_64-pc-windows-msvc` — so a Windows-only compile break is invisible locally and shows up as a red PR minutes later.

The break is almost always the same shape: a **fast-tier** test file calls a `tests/common/mod.rs` helper that is `#[cfg(unix)]` (because it is built on `std::os::unix::net::UnixStream` or `libc`). The `e2e_*.rs` suites never hit this, because the Windows job runs `cargo nextest run` **without** `--features e2e`, so those targets are not compiled there at all. Fast-tier files are.

PRD #126 hit it exactly this way: `tests/idle_worker_detector.rs` called `common::attach_request_on`, and `build-windows` failed with `error[E0425]: cannot find function attach_request_on in module common`.

## The check

`cargo check` type-checks without linking, so no MSVC linker is needed — a Linux host can verify the whole workspace, tests included:

```sh
rustup target add x86_64-pc-windows-msvc   # one-time
scripts/windows-cross-check.sh
```

Extra arguments pass through to `cargo check`, so `scripts/windows-cross-check.sh --features e2e` also covers the L2 suites (which CI's Windows job does *not* compile — see above).

Three details, all handled inside the script, are the difference between this working and appearing impossible:

- **Use rustup's toolchain, not devbox's.** The devbox/nix `cargo`/`rustc` on `PATH` are Linux-only — they ship no `x86_64-pc-windows-msvc` `rust-std`, so the run dies early with a misleading `error[E0463]: can't find crate for core` *even though `rustup target add` reports the target installed*. `rustup` installed it into `~/.rustup/toolchains/...`, which the nix toolchain never consults. Pin both `RUSTC` and the `cargo` binary to the rustup toolchain.
- **Shim `lib.exe`.** devbox exports `AR=ar` and `CC=gcc` globally, and `cc-rs` picks those up even for an MSVC target. `ring`'s build script then hands GNU `ar` MSVC-style flags and it aborts on `ar: invalid option -- ':'`. A tiny shim that rewrites `-out:X`/`-nologo` into `ar crs X …` is enough: `cargo check` never links, so the archive only has to exist. (This is the "cross-compile `lib.exe`→`ar` shim" the `build-windows` comment in `ci.yml` mentions as a Linux-only artifact — `windows-latest` archives natively and needs none of it.)
- **Use a separate `CARGO_TARGET_DIR`.** The rustup and nix toolchains are different rustc versions; sharing `target/` makes each run invalidate the other's cache and forces a full rebuild of the next `cargo test-fast`.

Warm, this completes in about ten seconds.

## Reading the result

Only **errors** matter. The Windows job runs `cargo clippy -- -D warnings` *without* `--all-targets` — deliberately, so a test-only lint cannot fail Windows-only and be unreproducible on the Linux pre-push gate (PRD #42 review S1). Test-target warnings (`tests/delegate_prompt_injection.rs` has had a few unused-import/unused-const warnings on Windows for a long while) do not fail CI and are not something this check asks you to fix.

## Fixing what it finds

Prefer **per-item** `#[cfg(unix)]` on the genuinely Unix-specific helpers, keeping the rest of the file compiling on Windows. PRD #42 M8 deliberately replaced a wholesale `#![cfg(unix)]` with per-item gating on five fast-tier files for exactly this reason: a blanket gate silently throws away real Windows coverage. `tests/e2e_pane_send_result.rs` and `tests/e2e_scheduler_manager.rs` show the per-item shape.

Reach for a **file-level `#![cfg(unix)]`** only when *every* test in the file is Unix-bound anyway, so there is no Windows coverage left to preserve. The tell is a harness that spawns a POSIX-shell PTY stub — `stty`, `printf`, `trap '' TERM`, `exec cat`, or any `SpawnOptions.env` pinning `SHELL=/bin/sh`. Note that `platform::shell::default_shell` returns an injected `SHELL` override **verbatim on every platform**, so a pinned `/bin/sh` is not quietly remapped to `cmd.exe` on Windows — it is spawned, fails, and the harness panics. Gating only the socket helpers in such a file trades a compile error for a runtime panic. `src/agent_pty.rs`'s `#[cfg(all(test, unix))] mod spawn_tests` and `tests/daemon_protocol.rs` are the precedents; `tests/idle_worker_detector.rs` is the PRD #126 case.

When you do gate a whole file, confirm `tests/CATALOG.md` already records those specs as `Platform coverage: mac+linux`, so the gate matches documented intent instead of silently dropping coverage. A Windows port of the PTY + named-pipe harness is tracked by #164 (M10).
