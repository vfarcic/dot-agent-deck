# Checking a Windows compile locally, before CI

## The gap this closes

`cargo test-fast`, `cargo test-e2e` and `cargo test-e2e-live` only ever compile for the host, which on every current dev machine is Linux. CI's `build-windows` job (`.github/workflows/ci.yml`) is therefore the *first* thing that ever compiles the tree for `x86_64-pc-windows-msvc` — so a Windows-only compile break is invisible locally and shows up as a red PR minutes later.

The break is almost always the same shape: a **fast-tier** test file calls a `tests/common/mod.rs` helper that is `#[cfg(unix)]` (because it is built on `std::os::unix::net::UnixStream` or `libc`). The `e2e_*.rs` suites never hit this, because the Windows job runs `cargo nextest run` **without** either e2e feature, so none of those targets is compiled there at all. Issue #502 does not change that: `build-windows` names no e2e feature, so the files that now also require `e2e-live` stay out for the same reason as the rest. Fast-tier files are.

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

## When `build-windows` fails a test you cannot run

The check above catches **compile** breaks. It runs no tests, and nobody here develops on Windows, so a Windows-only **test** failure is first seen in CI, after a push. That is the moment "flaky, re-run it" is most tempting, and `CLAUDE.md` rule 6 already says the red is yours whoever caused it. What a rule cannot give you is recognition of the shape — and in August and September 2026 the same three shapes failed `build-windows` repeatedly, each instance fixed and closed on its own, with nothing collecting them in one place:

| fixed | where | shape | what failed |
| --- | --- | --- | --- |
| 2026-08-12 | #511 | path | a `clean_tmp` test passed `/scratch/one` to `--root`, which production checks with `Path::is_absolute()` |
| 2026-09-02 | PR #830, #831 | path | the palette guard compared a walked path stringified with the native separator against a forward-slashed literal (`25b39b53`); #831 is the identical construction in `rel_to_root`, found before it failed |
| 2026-09-07 | #851 | timing | `dispatch_018`/`019` asserted PTY arrival after a fixed `sleep(75ms)`; #850 and #892 are the same shape in the same module |
| 2026-09-14 | PR #1063 | line endings | the `SKILL.md` frontmatter gate matched `"---\n"` against a CRLF checkout |
| 2026-09-16 | #1102 | timing | a cross-runtime wait bounded by 1000 `yield_now` iterations — about 4 ms of wall clock |
| 2026-09-16 | PR #1126 | line endings | the selection-capture guard scanned `include_str!` text for a `\n`-joined marker (`7b891592`) |

### Line endings — eliminated at the checkout

**What happened.** The Windows runner's git converted text files to CRLF at checkout. The compiler never noticed — rustc normalizes CRLF to LF when it reads a source file, even inside a string literal — but `include_str!` and `std::fs::read_to_string` hand a test the bytes as checked out, so a scanner looking for `"\n#[cfg(test)]\nmod tests {"` or `"---\n"` matched nothing there and everything on Unix.

**Why it no longer recurs.** `.gitattributes` opens with `* text=auto eol=lf`: every file git detects as text is checked out with LF on every platform, whatever `core.autocrlf` says. Adding it rewrote no blob — every text file in the index that has a line ending was already stored LF, none CRLF or mixed, and no `.bat`/`.cmd`/`.ps1` is tracked that would want a CRLF carve-out — which `git add --renormalize .` confirmed by staging `.gitattributes` alone. You can check the effect without Windows: `git -c core.autocrlf=true archive HEAD desktop/src-tauri/src/selection_capture.rs | tar -xO | tr -cd '\r' | wc -c` prints `0`, where the same command against `7b891592`, the commit before the attribute, prints `293`.

**What it does not cover.** The attribute governs the files git checks out. Text that reaches a test by another route keeps the line endings its producer chose — a file a Windows program wrote, a subprocess's output, a PTY's output (where the terminal layer turns `\n` into `\r\n` by default). A Windows clone checked out before the attribute landed also keeps its CRLF working files until they are checked out again. So the `replace("\r\n", "\n")` in the existing scanners (`selection_capture.rs`, `embedded_pane.rs`, `skill_frontmatter.rs`, `issue_labeler_memory.rs`, `issue_labeler_policy.rs`) stays, as defense in depth; a new scanner over multi-line text should do the same.

**Reproducing one on Linux.** Convert the file the test reads to CRLF, run the test, and restore the file:

```sh
sed -i 's/$/\r/' desktop/src-tauri/src/lib.rs    # run it once — a second pass doubles the CR
cargo test-fast no_module_grows_a_new_raw_read_of_the_applied_selection
git checkout -- desktop/src-tauri/src/lib.rs
```

The crate still compiles, for the rustc reason above, so the conversion changes exactly what the test reads and nothing else — which is the Windows condition. That example is PR #1126's: with the guard as it was before `7b891592` it fails with `build-windows`'s own message (`lib.rs must hold exactly one #[cfg(test)] mod tests { … found 0`), and with the fix it passes. Restore explicitly rather than trusting git to show you the file is still converted: with the attribute in place `git diff` reports no change for it and only warns that `CRLF will be replaced by LF the next time Git touches it`. For a lasting guard, encode the condition as a test over a CRLF fixture string, as `7b891592` and PR #1063 both did, and have that test also assert the un-normalized fixture fails — otherwise it can silently stop reproducing anything.

### Timing — a wait bounded by anything but a deadline

**The shape.** A test waits for something another thread, task, runtime or process has to do, and bounds the wait by something whose wall-clock length depends on the machine rather than by a deadline:

- a fixed `sleep` followed by an assertion that the thing has **already** happened — bytes arrived, a message was delivered, a task finished (#850, #851, #892);
- a loop bounded by an iteration count whose iterations do not park the thread — `yield_now`, a spin, `try_recv` or `is_finished` with no sleep (#1102). `yield_now` reschedules only the waiting task; it hands another runtime's worker thread nothing, so 1000 iterations were ~4 ms on an idle 16-core box and fewer under load.

**Why Windows tends to lose first.** `build-windows` runs every fast-tier test that compiles for Windows in parallel (2339 of them in PR #1126's run) with nextest `retries = 0`, and #1102 notes that Windows' default scheduling quantum alone is ~15.6 ms — four times the budget that loop really had. The shape is not Windows-specific: #1102 reproduced on Linux, as written, at 4x CPU oversubscription, and #851 did too once its sleep was shrunk.

**Reproducing one on Linux.** Oversubscribe the cores and run the test repeatedly. One way, from a single shell:

```sh
for _ in $(seq $((4 * $(nproc)))); do (while :; do :; done) & done
for i in $(seq 20); do cargo test-fast <test-name> || break; done
kill $(jobs -p)
```

On a box other agents share, that load slows their builds too, so keep it brief. #1102's loop exhausted its iterations once in ten runs at 4x. A test that survives this is not proven safe — a fixed sleep is a bet on scheduling — but one that fails under it has shown you the Windows failure without Windows.

**The fix.** Wait for the event itself, bounded by time:

- a content-keyed poll against a deadline, reusing the module's helper where one exists — `src/spawn.rs`'s `wait_for_detached_payload_echo`, `wait_for_echo_bytes` and `wait_for_further_payload_echo` poll every 10–20 ms against an 8 s deadline;
- otherwise `tokio::time::timeout(Duration::from_secs(5), async { loop { if ready() { break; } tokio::time::sleep(Duration::from_millis(5)).await; } })`. The sleep genuinely parks the thread, which is what lets another runtime's worker be scheduled.

A sleep is still right where it **is** the measurement: a window after which the test asserts something did **not** happen. That can false-pass on a loaded box but cannot flake red, and `3fb32ac6` deliberately left those in place.

**Where nothing lints for it.** `cargo xtask linkage-check` rule 5 forbids raw `std::thread::sleep` / `tokio::time::sleep` and `for _ in 0..N {` polling — but only in `tests/e2e_*.rs`, which `build-windows` never compiles. The timing failures in the table were all in the fast tier (`src/` unit tests and the desktop crate), which rule 5 does not scan.

### Paths — Unix path semantics in code that runs on Windows

**The shape.** Two constructions that are right on Unix and wrong on Windows:

- a Unix absolute literal — `"/scratch/one"`, `"/tmp/x"` — fed to something that resolves it. `Path::is_absolute()` is false for a leading `/` on Windows, which needs a drive prefix (#511).
- a path stringified with the native separator — `to_string_lossy()`, `display()` or `to_str()` of a relative or walked path — and then compared with, keyed against, or displayed beside forward-slashed strings. On Windows it reads `tests\e2e_foo.rs` (PR #830, #831).

The rule #831 drew from it: **a walked path is safe to compare, and unsafe to stringify.** `Path`/`PathBuf` equality, `starts_with` and `ends_with` compare component by component and do not care which separator produced them.

**The fix.** For a test literal, give each platform its own — a `#[cfg(windows)]` drive-prefixed path beside the `#[cfg(not(windows))]` Unix one, as #511 did. For a path that has to become a string, build it from `Path::components` joined with `/`, as `slash_path` in `xtask/linkage-check/src/paths.rs` does — not with `replace('\\', "/")`, because on Unix a backslash is a legal character in a file name and the rewrite would name a file that does not exist.

**Safe by construction.** Code under `#[cfg(unix)]`, in a file with `#![cfg(unix)]`, or in a `tests/e2e_*.rs` file cannot fail this way on `build-windows`, which does not compile it. The sweep below found at least 428 of the tree's 995 hardcoded Unix-absolute path literals in such code, and changing those buys nothing on Windows.

**Reproducing one on Linux.** There is no conversion trick for this one. Reason from the two facts above, and use `scripts/windows-cross-check.sh` to at least type-check a cfg-split fix against the Windows target.

### Sweeping for more of them

PR #1126 swept `src/`, `tests/`, `desktop/src-tauri/src/` and `xtask/` for all three shapes. The candidate lists came from these commands, run from the repository root; every hit was then read in context and classified:

```sh
D="src tests desktop/src-tauri/src xtask"
# timing: sleeps, range loops, attempt/retry counters, yields
grep -rnE --include='*.rs' '\bsleep(_ms|_until)?\s*\(' $D
grep -rnE --include='*.rs' 'for\s+\(?_?\w*\)?\s+in\s+(0|1)\.\.=?\s*[0-9A-Za-z_(]' $D
grep -rnE --include='*.rs' '(while\s+\w*(attempt|retr|tries|tried|iter|spins|polls)\w*\s*<)|(for\s+\w*(attempt|retr|tries|try|iter|poll)\w*\s+in\s)|(\b\w*(attempt|retr|tries|tried|iter|spins|polls)\w*\s*(>=|>|==)\s*[0-9A-Z_])' $D
grep -rnE --include='*.rs' 'yield_now|spin_loop' $D
# paths: Unix-absolute literals, literal "/" joins, stringified paths
grep -rnE --include='*.rs' '"/(tmp|home|var|usr|etc|Users|opt|root|srv|scratch|nonexistent|private|mnt|dev|proc|bin|run)(/|")' $D
grep -rnE --include='*.rs' '(format!\("[^"]*\{[^}]*\}/\{)|(\.join\("/"\))|(push_str\("/"\))|(push\(./.\))|(\+ "/")|(concat!\([^)]*"/")' $D
grep -rnE --include='*.rs' '(strip_prefix\([^)]*\)[^;]*\.(to_string_lossy|to_str|display))|(\.display\(\)\.to_string\(\))|(to_string_lossy\(\)\.(into_owned|to_string)\(\))' $D
```

Most hits are safe, and the useful part is knowing which safe-looking ones are not, and which alarming-looking ones are fine:

- **A `sleep` inside a loop bounded by an `Instant` deadline is a poll, not this shape.** Neither is a sleep followed by an assertion that something did *not* happen, nor a count-bounded loop whose every iteration sleeps a fixed duration — its wall-clock floor is N × d whatever the machine speed.
- **A count-bounded `yield_now` loop does not scale with machine speed on a `start_paused` current-thread runtime** where every task it waits on runs on that same thread and waits on no I/O — the number of polls needed is then fixed. It is #1102's shape the moment one of those tasks lives on another thread or runtime, or waits on a PTY, a socket or a child process.
- **`Command::spawn` with a `pre_exec` closure does not return until the child execs** — std blocks on a close-on-exec status pipe — and a grandchild forked inside `pre_exec` inherits that pipe. So when the grandchild `setsid`s and then `execv`s without closing that descriptor first, a poll that "only" waits for its pid to be listed is not racing either step: `tests/shell_activity.rs` looked like that race and was not (a 300 ms delay injected before the grandchild's `setsid` made `spawn()` itself take 300 ms longer, and both tests stayed green).
- **A timing site is not confirmed unsafe until shrinking its margin fails it.** Cut the sleep to 0, or slow the thing being waited for, and run the test a few times. Of the sweep's candidates, `pane_input_032` failed 3 of 3 with its sleeps at 0 and `build_gate.rs` 3 of 3 with its readiness sleep at 0, while `idle_worker_015` stayed green 3 of 3 — so its sleeps are not load-bearing on an idle box, and any remaining risk is under load only.
- **For paths, what `build-windows` compiles is what matters,** and every test it compiles ran on PR #1126 (2339 tests, 0 skipped, nextest `fail-fast = false`). A Windows-compiled test whose path handling breaks one of its assertions fails deterministically, so it cannot be a latent flake; the latent cases are in code whose Windows output nothing asserts yet — #831's was production code in `xtask`.
