#![cfg(feature = "e2e")]

//! Issue #250 `lifecycle/version/001` — a build environment that pre-sets
//! `DAD_VERSION` / `DAD_BUILD_ID` produces a binary that reports those values,
//! and *changing* either one invalidates the cached build.
//!
//! This is the only tier that can observe the whole chain: `build.rs` reading
//! the injected env, the `cargo:rerun-if-env-changed` directives that let a
//! *changed* injection take effect at all, and the `env!()` sites in
//! `src/main.rs` / `src/daemon_protocol.rs` that surface it. The pure resolver
//! tests in `tests/build_version.rs` cover the precedence and validation rules
//! themselves.
//!
//! # Cost — read this before assuming the test is hung
//!
//! It runs **three** real `cargo build`s, all into the same scratch
//! `CARGO_TARGET_DIR` under `target/`:
//!
//! 1. `(VERSION_A, BUILD_ID_A)` — the cold one. Into a scratch dir nothing has
//!    built in yet this compiles the bin's whole dependency graph: **224
//!    packages**, 280 compilation units (201 for the target, 79 host ones for
//!    build scripts and proc macros). Not the 613 issue #928 quotes — 613 is
//!    `cargo metadata --all-features` for the *workspace*, which counts the
//!    Tauri desktop crate and every dev-dependency, none of which this build
//!    touches.
//! 2. `(VERSION_B, BUILD_ID_A)` — only `DAD_VERSION` changed.
//! 3. `(VERSION_B, BUILD_ID_B)` — only `DAD_BUILD_ID` changed.
//!
//! Builds 2 and 3 re-run the build script, **recompile this package** — a
//! changed `cargo:rustc-env` invalidates the lib and the bin, not merely the
//! link — and relink, which is exactly why the scratch dir is stable and
//! shared rather than a fresh tempdir per step. The widened `slow-timeout`
//! override for this test in `.config/nextest.toml` exists for the cold case.
//!
//! ## What it costs, and what actually drives that (issue #928)
//!
//! The single largest factor is **how busy the machine is** — by more than the
//! whole rest of this section put together. Measured on one 16-core dev box,
//! `--jobs` capped to 8 as `nested_build_jobs` caps it by default, the three nested builds back
//! to back:
//!
//! | machine | cold | warm | scratch dir |
//! | --- | --- | --- | --- |
//! | quiet (load 3-8) | 65.0s | 24.0s | 1.8 GB |
//! | three other agents building (load 13-18) | 400.9s | 142.3s | 1.8 GB |
//! | quiet, `CARGO_INCREMENTAL=0` as CI sets it | 98.5s | — | 1.3 GB |
//! | busy, `CARGO_INCREMENTAL=0` (load 14-22) | 455.6s | 182.0s | 1.3 GB |
//!
//! That is a **6x** spread on identical work and identical flags. The whole
//! test through `cargo test-e2e version_001` measured 395.3s cold on the busy
//! box; on GitHub `ubuntu-latest`, across 13 `e2e-deterministic` runs read from
//! their own logs (2026-09-06 to 2026-09-09), 71.9s to 305.4s — 4.2x — with
//! two further runs that had no restored cache at 351.1s and 369.9s. So do not
//! read one slow run as a regression, and do not tune this test against a
//! single measurement.
//!
//! ## What CI actually pays (issue #2109)
//!
//! Issue #928 argued about whether CI's `Swatinem/rust-cache` covered the
//! scratch dir (it did: its cleanup recurses into a nested target dir and
//! keeps the dependencies). That stopped mattering when the e2e tier moved to a
//! nextest archive run in shards: this test runs alone in the `version` shard
//! (`scripts/e2e-shard.sh`), and neither backend's shard restores a cache
//! holding this scratch dir, so **every CI run is the cold case** — the whole dependency graph, then this
//! package (140k lines under `src/`) three times. Two ways under that were
//! considered and rejected: collapsing the builds would stop pinning either
//! `rerun-if-env-changed` directive individually (changing both variables at
//! once proves neither), and seeding the scratch dir from the main
//! `target/debug` would mean a test whose job is to prove a BUILD behaves
//! correctly starts from another build's output.
//!
//! ## What the nested build pins (issue #2109)
//!
//! Three settings make the build cheaper without touching what it proves —
//! none of them changes what `build.rs` reads, which directive fires, or which
//! `env!()` sites the binary carries:
//!
//! - **`CARGO_INCREMENTAL=1`.** GitHub's shard exports `CARGO_INCREMENTAL=0`
//!   (`dtolnay/rust-toolchain` sets it), which also drops this
//!   package from 256 codegen units to 16 — and this package's compile, three
//!   times over, is most of the work. Dependencies are never built
//!   incrementally, so it costs them nothing. It is the quiet-box table's
//!   65.0s against 98.5s above.
//! - **`CARGO_PROFILE_DEV_DEBUG=0`.** The produced binary is run for
//!   `--version` and `daemon hello`, neither of which reads debug info. Set in
//!   the environment rather than declared as a profile in `Cargo.toml`: no CI
//!   cache holds this dir any more, so there is no cache key it would have to
//!   rotate. A local scratch dir built before this rebuilds once.
//! - **`DAD_E2E_NESTED_BUILD_JOBS`** overrides the `--jobs` cap below. The
//!   `version` shard sets it to `all` (every CPU the runner's quota allows), since the cap exists to protect
//!   tests running beside this one and in that shard there are none; on a
//!   4-vCPU runner the cap halved the cold build's parallelism.
//!
//! Measured on a 14-core macOS dev box (2026-11-09), `CARGO_INCREMENTAL=0`
//! exported as CI has it, scratch dir deleted before each run, the whole test:
//!
//! | | busy (load 11-35) | quiet (load 3-5), via `e2e-shard.sh version` |
//! | --- | --- | --- |
//! | before (7 jobs, outer env's incremental and debug) | 189s, 189s | 109.3s |
//! | the two pinned settings, cap kept (7 jobs) | 132s | — |
//! | the two pinned settings, uncapped (14 jobs) | 102s | 58.7s |
//!
//! Three containment choices worth keeping:
//!
//! - The build goes to its **own** `CARGO_TARGET_DIR`, so it can neither
//!   clobber the `target/debug/dot-agent-deck` that every other e2e test spawns
//!   nor force a full rebuild of it afterwards (`rerun-if-env-changed` would
//!   otherwise invalidate the build script on the very next plain `cargo
//!   build`).
//! - The nested build's parallelism is **capped at half the machine's cores**
//!   (`--jobs`), because it runs while nextest is executing the rest of the e2e
//!   suite and a full-parallel nested compile can starve the timed tests around
//!   it into flaking. That premise used to be mostly false and is now true: see
//!   the next point. `DAD_E2E_NESTED_BUILD_JOBS` lifts it where the test runs
//!   alone (above).
//! - nextest schedules this test near the **front** of its queue, behind only
//!   the exclusive reservations (`priority` in `.config/nextest.toml`), instead
//!   of at its natural binary-name position about 70% of the way down. It was the last of ~2,680 tests to finish in 13
//!   of 13 CI runs *and* the last to start, at t = 148.6-156.9s every time, so
//!   the job's whole test wall clock was that offset plus this test's duration.
//!   The full measurement is in that file's comment.
//!
//! Decision 6: gated behind the `e2e` feature so `cargo test-fast` never
//! compiles it.

dot_agent_deck_test_harness::link!();

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use spec::spec;

/// Values a packager would inject. Deliberately unlike any real tag of this
/// project — and unlike the `0.1.0` placeholder that is the bug's signature —
/// so a passing assertion cannot be a coincidence.
const VERSION_A: &str = "42.7.13";
const BUILD_ID_A: &str = "42.7.13-ginjected0";
/// The second pair. Each is changed on its own, one build apart, so a single
/// missing `cargo:rerun-if-env-changed` line fails the test: changing both at
/// once would prove neither, since either one alone re-runs the whole script.
const VERSION_B: &str = "58.1.2";
const BUILD_ID_B: &str = "58.1.2-ginjected1";

/// The `Cargo.toml` placeholder a source build wrongly reported before this fix.
const PLACEHOLDER_VERSION: &str = "0.1.0";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The isolated `CARGO_TARGET_DIR` every injected build writes into.
///
/// No other build in the repository targets it — `grep -r version-injection-e2e`
/// finds this file and the comments in `.config/nextest.toml` that describe it.
fn scratch_target_dir() -> PathBuf {
    let base = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"));
    base.join("version-injection-e2e")
}

/// The rustc host triple, e.g. `x86_64-unknown-linux-gnu`.
///
/// The build is pinned to this target explicitly rather than inheriting
/// whatever `CARGO_BUILD_TARGET` / `[build] target` the environment carries:
/// a configured or cross target would put the artifact under
/// `<scratch>/<triple>/debug/` instead of `<scratch>/debug/` (an
/// environment-dependent failure) and might not even produce a binary this
/// machine can run. Pinning it makes both the path and the runnability
/// deterministic.
fn host_target() -> String {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let out = Command::new(&rustc)
        .arg("-vV")
        .output()
        .unwrap_or_else(|e| panic!("failed to run `{rustc} -vV`: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap_or_else(|| panic!("`{rustc} -vV` did not report a host triple:\n{stdout}"))
        .trim()
        .to_string()
}

/// The variable that lifts the `--jobs` cap when this test is known to run
/// alone. `scripts/e2e-shard.sh` sets it for CI's `version` shard, whose only
/// test this is, so the cap would protect nothing there (issue #2109).
const NESTED_BUILD_JOBS_ENV: &str = "DAD_E2E_NESTED_BUILD_JOBS";

/// The nested build's `--jobs`. `DAD_E2E_NESTED_BUILD_JOBS=all` uses every CPU
/// this process may use, a positive number uses that many, and anything else
/// (unset included) caps it at half, at least one — see the cost note at the
/// top of the file for why the nested build is capped by default.
///
/// `all` rather than a count from the shell: `std::thread::available_parallelism`
/// honours a cgroup CPU quota, which `getconf _NPROCESSORS_ONLN` / `nproc` on a
/// quota-limited runner may not, so a count computed in the script could
/// oversubscribe the very shard it is meant to speed up.
fn nested_build_jobs() -> usize {
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    match std::env::var(NESTED_BUILD_JOBS_ENV)
        .ok()
        .as_deref()
        .map(str::trim)
    {
        Some("all") => available,
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|&n| n > 0)
            .unwrap_or((available / 2).max(1)),
        None => (available / 2).max(1),
    }
}

/// Build `dot-agent-deck` with `version` / `build_id` pre-set in the build
/// environment, into the shared scratch target dir. Returns the path of the
/// produced binary.
fn build_with_injected(version: &str, build_id: &str) -> PathBuf {
    let scratch = scratch_target_dir();
    let target = host_target();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let jobs = nested_build_jobs().to_string();

    let out = Command::new(&cargo)
        .args([
            "build",
            "--bin",
            "dot-agent-deck",
            "--target",
            &target,
            "--jobs",
            &jobs,
        ])
        .current_dir(workspace_root())
        .env("CARGO_TARGET_DIR", &scratch)
        .env("DAD_VERSION", version)
        .env("DAD_BUILD_ID", build_id)
        // Issue #2109: the two settings that make this build cheaper without
        // touching what it proves. Neither changes what `build.rs` sees or
        // which `rerun-if-env-changed` directive fires; both are pinned here so
        // the outer environment (CI exports `CARGO_INCREMENTAL=0`) cannot
        // choose them. See "What the nested build pins" at the top of the file.
        .env("CARGO_INCREMENTAL", "1")
        .env("CARGO_PROFILE_DEV_DEBUG", "0")
        // Cargo passes its jobserver to child processes through this variable;
        // handing it to a nested cargo makes it warn about an unusable
        // jobserver fd. Dropping it just costs the child its own job slots —
        // which `--jobs` above bounds anyway.
        .env_remove("CARGO_MAKEFLAGS")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `{cargo} build`: {e}"));

    assert!(
        out.status.success(),
        "building with DAD_VERSION={version} / DAD_BUILD_ID={build_id} injected must succeed, \
         got {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let bin = scratch
        .join(&target)
        .join("debug")
        .join(format!("dot-agent-deck{}", std::env::consts::EXE_SUFFIX));
    assert!(
        bin.is_file(),
        "the injected build should have produced {}",
        bin.display()
    );
    bin
}

/// Run the freshly built binary under a scrubbed, isolated environment and
/// return its combined stdout+stderr. Notably the injection vars are NOT set
/// here — a build-time value that only reappears because the runtime env
/// still carries it would prove nothing.
fn run_binary(bin: &Path, args: &[&str]) -> String {
    let home = common::race_safe_tempdir();
    let h = home.path();
    let mut cmd = Command::new(bin);
    cmd.args(args);
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("HOME", h);
    cmd.env("TERM", "xterm-256color");
    cmd.env("DOT_AGENT_DECK_SOCKET", h.join("hook.sock"));
    cmd.env("DOT_AGENT_DECK_ATTACH_SOCKET", h.join("attach.sock"));
    cmd.env("DOT_AGENT_DECK_STATE_DIR", h.join("state"));
    cmd.env("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "1");
    cmd.env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "30");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let out = cmd.output().expect("spawn the injected dot-agent-deck");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "`dot-agent-deck {}` should exit 0, got {:?}\noutput:\n{combined}",
        args.join(" "),
        out.status.code()
    );
    drop(home);
    combined
}

/// Assert that `bin` reports exactly `version` / `build_id` on both surfaces
/// that matter: `--version` (what a packager and the remote pre-flight read)
/// and `daemon hello` (the handshake, which carries both values on the wire).
fn assert_reports(bin: &Path, step: &str, version: &str, build_id: &str) {
    let version_out = run_binary(bin, &["--version"]);
    assert!(
        version_out.contains(version),
        "{step}: `--version` must report the injected DAD_VERSION \
         `{version}`.\noutput:\n{version_out}"
    );
    assert!(
        !version_out.contains(PLACEHOLDER_VERSION),
        "{step}: `--version` must not fall back to the `{PLACEHOLDER_VERSION}` \
         CARGO_PKG_VERSION placeholder when a version was injected.\noutput:\n{version_out}"
    );

    let hello_out = run_binary(bin, &["daemon", "hello"]);
    let hello: serde_json::Value = serde_json::from_str(hello_out.trim())
        .unwrap_or_else(|e| panic!("{step}: `daemon hello` should print JSON ({e}):\n{hello_out}"));
    assert_eq!(
        hello["daemon_version"].as_str(),
        Some(version),
        "{step}: the hello handshake must advertise the injected DAD_VERSION.\noutput:\n{hello_out}"
    );
    assert_eq!(
        hello["build_version"].as_str(),
        Some(build_id),
        "{step}: the hello handshake must advertise the injected DAD_BUILD_ID, not one composed \
         from git.\noutput:\n{hello_out}"
    );
}

/// Scenario: Rebuild the `dot-agent-deck` binary into a scratch target dir three
/// times with different `DAD_VERSION` / `DAD_BUILD_ID` values pre-set in the
/// build environment, and after each build run the produced binary with those
/// vars absent from its runtime environment. The first build must report the
/// injected `42.7.13` / `42.7.13-ginjected0` (not the `0.1.0` placeholder, not
/// the checkout's git tag) on both `--version` and `daemon hello`; then changing
/// only `DAD_VERSION`, and afterwards only `DAD_BUILD_ID`, must each be picked
/// up by the next build rather than served from cache.
#[spec("lifecycle/version/001")]
#[test]
fn version_001_injected_build_env_reaches_the_binary() {
    // Step 1 — the injection is honoured at all (the cold build; see the cost
    // note at the top of this file).
    let bin = build_with_injected(VERSION_A, BUILD_ID_A);
    assert_reports(&bin, "initial injected build", VERSION_A, BUILD_ID_A);

    // Step 2 — change ONLY DAD_VERSION, in the SAME target dir. Without
    // `cargo:rerun-if-env-changed=DAD_VERSION` cargo has no reason to re-run the
    // build script (no source file and no watched git path moved), so the binary
    // would still report VERSION_A and this assertion fails. The unchanged build
    // id must survive as-is.
    let bin = build_with_injected(VERSION_B, BUILD_ID_A);
    assert_reports(
        &bin,
        "after changing only DAD_VERSION",
        VERSION_B,
        BUILD_ID_A,
    );

    // Step 3 — now change ONLY DAD_BUILD_ID, again in the same target dir. Same
    // argument for the second rerun directive; the version must stay at
    // VERSION_B.
    let bin = build_with_injected(VERSION_B, BUILD_ID_B);
    assert_reports(
        &bin,
        "after changing only DAD_BUILD_ID",
        VERSION_B,
        BUILD_ID_B,
    );
}
