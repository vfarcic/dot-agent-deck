//! Arms the wrapped-agent lifetime bound for every child a test process will
//! ever spawn (issue #668).
//!
//! `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` is what gates `wrap`'s
//! `arm_wrap_self_defense` (which bounds the *wrapper*) and, since #661,
//! `arm_child_group_backstop` (which forks a reaper holding the same deadline
//! for the *child's* group, so an uncatchable `SIGKILL` of the wrapper cannot
//! strand it). Both are deliberately env-gated so a production wrapper forks
//! nothing and behaves exactly as before.
//!
//! Rows 1 and 2 of the suite's spawn table already pin it: `TuiDeck` at its
//! `env_clear`, and `DaemonProc` at its. The in-process `AgentPtyRegistry` path
//! did not, and it is the one 15 test files spawn agents through — so the
//! wrapped stand-ins they mint had #661's mechanism *mis-armed* rather than
//! unreachable. Measured on a live wrapper spawned by `delegate_007`: zero
//! `MAX_LIFETIME` matches anywhere in its `/proc/<pid>/environ`, and 221 such
//! orphans censused on one dev box with the oldest alive 9.4 days, each still
//! holding a working directory the tooling had already deleted. (They do not
//! pin that root `live-pid`: `clean-e2e-tmp` keys on the TEST process's pid in
//! the root's name, not on the orphan, so the root is reapable once its owning
//! test dies and the floor passes. The cost is the process itself — unkillable,
//! retaining deleted inodes, and polluting every later `ps`.)
//!
//! One variable, one site, and it covers spawn shapes that do not exist yet:
//! `agent_pty::spawn` scrubs *named* deck vars but does not `env_clear`, so a
//! value set once in this process is inherited by every child of every shape —
//! bare `cat`, wrapped Codex, recorder script, real agent. That is what makes
//! this a one-line fix rather than the per-`SpawnOptions` whack-a-mole it looks
//! like.
//!
//! **The value is written before `main`** (issue #678), by the constructor
//! [`arm_before_main`], because that is the only point at which writing the
//! environment is sound without a proof about every thread in the process.
//! [`arm`] writes nothing; it checks the constructor ran.
//!
//! # Why this is its own file rather than a function in `common/mod.rs`
//!
//! It started as one, and a function in `common/mod.rs` can only be called by a
//! file that links `common`. Three of the files that reach a bare
//! `AgentPtyRegistry` do not, and not by oversight — `tests/rehydration.rs`,
//! `tests/daemon_protocol.rs` and `tests/shell_activity.rs` avoid `mod common;`
//! deliberately, because `tests/common/mod.rs` is ~420 KB of PTY/vt100 harness
//! and pulling it into a fast-tier crate to reach one variable is a real
//! compile cost for no coverage. (`tests/rehydration.rs` and
//! `tests/daemon_protocol.rs` already `#[path]`-include `src/test_temp.rs` for
//! exactly the same reason.) So the arming lives here, in a file small enough
//! for any test binary to include on its own:
//!
//! ```ignore
//! #[path = "common/child_lifetime_bound.rs"]
//! mod child_lifetime_bound;
//! ```
//!
//! and `tests/common/mod.rs` declares it as a module too — so every test binary that
//! links either one gets the same constructor, one implementation and one SAFETY
//! argument rather than one copy per spawn-owning crate.
//!
//! **Self-contained, for the same reason `src/test_temp.rs` is** (issue #474):
//! this file is compiled into every crate that `#[path]`-includes it, where
//! `crate::` names that *test binary's* own root and nothing this repository
//! defines is in scope. It uses `std`, the `ctor` dev-dependency, and one
//! public constant from the library, each by its extern-crate path; no
//! `crate::` or file-scope `super::` path may appear here.
//!
//! Enforced by linkage-check rule 10: a file under `tests/` that constructs an
//! `AgentPtyRegistry` or calls `run_daemon_with` must arm the bound, so the next
//! spawn site to be written cannot silently repeat the gap.

use std::sync::OnceLock;

/// How long a process spawned out of a test process may live before its own
/// backstop ends it, in seconds. Matches the value `TuiDeck` and `DaemonProc`
/// pin explicitly after their `env_clear`.
///
/// Not the number the reaper's floor is derived from — that is
/// `clean_tmp::MAX_PINNED_ORPHAN_CAP_SECS` (900 s), the longest cap any test
/// pins, and it is deliberately larger than this default. Issue #679: the floor
/// was documented as "2x the orphan cap" while pointing at *this* 300 s, which
/// left it 300 s short of the longest pin actually in the tree. See [`clamped`]
/// for why this file cannot bound that pin, and linkage-check rule 11 for what
/// does.
///
/// Also the CEILING for whatever this process inherited, not just the default.
/// It bounds *ambient* values and nothing else — a harness path that re-pins the
/// variable after an `env_clear` bypasses it by design. See [`clamped`].
const CHILD_MAX_LIFETIME_SECS: u64 = 300;

/// The cap this process should pin, given whatever was already in `ambient`.
///
/// A **shorter** ambient value wins: several tests pin their own (`wrap_io.rs`
/// at 120 s, the fd-table probe at 10 s) precisely so nothing they mint can
/// outlive the case that made it, and that is deliberate.
///
/// A **longer** one does not, and neither does an unparseable or zero one. Both
/// are replaced by [`CHILD_MAX_LIFETIME_SECS`], because the cap is not a
/// preference here — it is what `cargo xtask clean-e2e-tmp`'s deletion safety
/// rests on. That reaper reaps a root whose owning test process is dead once
/// the root is 30 minutes old, and it picks 30 minutes as *2x the longest cap
/// any test pins* (`docs/develop/e2e-temp-dirs.md`, "The 30-minute floor on
/// dead owners"). So an exported
/// `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS=3600` does not merely widen a
/// test-only bound: it silently converts the reaper's margin into a deficit,
/// and `--apply` can then delete a root out from under a descendant still
/// nominally entitled to write there for another half hour. Measured before
/// this clamp existed: that exact value was accepted end to end and the arming
/// regression test passed with it.
///
/// # The ceiling is an AMBIENT-value guarantee, and only that
///
/// This function sees only what the *test process* inherited, so that is the
/// whole of what it can bound. A harness path that pins the variable explicitly
/// after an `env_clear` never reaches it: `TuiDeck` rebuilds the child
/// environment from its own pinned list (300 s among it) and then applies the
/// builder's `extra_env` **last**, so a test's `with_env` overwrites that pin
/// and is passed to the child verbatim. That ordering is deliberate and in use:
/// #665 pins **900** on `orchestration_dispatch_002` so the daemon outlives
/// that test's own 300 s work budget, without which the failure dump reports
/// `NO PANE — never spawned at all` for roles the same dump renders alive
/// (issue #663). Measured on the live processes: both the deck and its
/// lazily-spawned daemon carry `900`.
///
/// So state the property precisely. "Every descendant of a test process stops
/// within [`CHILD_MAX_LIFETIME_SECS`]" holds for **ambient** values and is not
/// universal — which is why the reaper's floor is no longer derived from this
/// number at all. Issue #679: while it was, the floor sat at 600 s (2x 300)
/// against a tree in which one test already pinned 900, so between 600 s and
/// 900 s that test's daemon outlived the floor and `--apply` could delete the
/// root under it. The floor is now 2x `clean_tmp::MAX_PINNED_ORPHAN_CAP_SECS`
/// — the longest cap *written anywhere under `tests/`*, 900 s — and
/// linkage-check rule 11 fails the build if a pin exceeds it, so the guarantee
/// the reaper needs is the one that is actually checked. This clamp keeps the
/// narrower job it can do: bounding what a contributor's shell hands in.
///
/// None of which makes the ceiling decorative: it covers the case that was
/// actually measured — an exported `…=3600` in a contributor's shell, which
/// reaches *every* root that shell mints and was accepted end to end before this
/// existed.
///
/// Returned as an owned `String` rather than a `&'static str` so the
/// shorter-wins case can re-pin the value it found.
pub fn clamped(ambient: Option<&str>) -> Option<String> {
    match ambient {
        // Nothing pinned yet: pin the default.
        None => Some(CHILD_MAX_LIFETIME_SECS.to_string()),
        Some(raw) => match raw.trim().parse::<u64>() {
            // In range and shorter (or equal) — leave it exactly as it is, so a
            // test that pinned `120` keeps reading `120`.
            Ok(secs) if (1..=CHILD_MAX_LIFETIME_SECS).contains(&secs) => None,
            // Out of range, zero, or not a number at all: overwrite. `0` and a
            // garbage string both parse to "no cap" at every consumer
            // (`daemon::parse_max_lifetime_secs` returns `None`), which is the
            // unbounded case this whole mechanism exists to remove.
            _ => Some(CHILD_MAX_LIFETIME_SECS.to_string()),
        },
    }
}

/// What [`arm_before_main`] left behind for [`arm`] to check: `Some` once the
/// constructor has run in this process.
static ARMED_BEFORE_MAIN: OnceLock<()> = OnceLock::new();

ctor::declarative::ctor! {
    /// Pin the cap in this process's environment before `main` (issue #678).
    ///
    /// A shorter ambient cap is kept; anything else — absent, zero,
    /// unparseable, or above the 300 s ceiling — is replaced. See [`clamped`]
    /// for why the ceiling is enforced rather than merely defaulted, and for
    /// what it deliberately does not reach: a child whose environment `TuiDeck`
    /// rebuilds from scratch never sees this process's value at all, so a
    /// test's own `with_env` cap is passed through unclamped (issue #679).
    ///
    /// **Why a constructor.** `std::env::set_var` is `unsafe` in edition 2024
    /// because it races any thread concurrently *reading* the environment, in
    /// Rust or in C. This used to run inside [`arm`], from test setup, and the
    /// argument for it ("nextest gives each test its own process") did not
    /// establish what it claimed: one process per test is not one thread per
    /// process, and several callers reached it from inside a multi-threaded
    /// Tokio runtime whose workers already existed. A constructor runs before
    /// `main` — before libtest starts the thread a test runs on, and before any
    /// code in the test binary can start one — so there is no other thread to
    /// race. That is the same guarantee `common::detach_before_main` relies on
    /// for the deck endpoint variables (issue #1473).
    ///
    /// Every test binary that links `tests/common/mod.rs` or `#[path]`-includes
    /// this file runs it, whether or not a test calls [`arm`] — so arming no
    /// longer depends on call ordering inside the test at all.
    ///
    /// Deliberately NOT done through `.cargo/config.toml`'s `[env]`: that has
    /// no per-subcommand scoping, so it would apply to `cargo run` as well and
    /// hand a developer a deck whose daemon self-terminates at 300 s. Nor
    /// through `.config/nextest.toml`'s `[env]`, which does not exist — nextest
    /// has no such key at top level or per profile and *silently ignores* one
    /// (re-measured on cargo-nextest 0.9.143: both `[env]` and
    /// `[profile.default] env = {…}` are accepted without error and reach no
    /// test process).
    ///
    /// Prints nothing: stderr is not promised to be usable before `main`.
    #[ctor(unsafe)]
    fn arm_before_main() {
        let var = dot_agent_deck::agent_pty::DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS;
        if let Some(value) = clamped(std::env::var(var).ok().as_deref()) {
            // SAFETY: a constructor runs before `main`, so before libtest or
            // any code in this binary has started a thread, and nothing can
            // read the environment while it is being written. (Measured on
            // Linux: a test binary links only libc, libm and libgcc_s, none of
            // which starts a thread from a constructor of its own.)
            unsafe { std::env::set_var(var, value) };
        }
        let _ = ARMED_BEFORE_MAIN.set(());
    }
}

/// Check that the cap was pinned before `main`. Writes nothing.
///
/// The pinning itself is [`arm_before_main`], a constructor that has already
/// run by the time any test body does — so this call is no longer what arms
/// the bound, and its position in a test does not matter. It stays for two
/// reasons. It is the marker linkage-check rule 10 looks for in a file that
/// spawns agents, and a call to it cannot compile unless the file actually
/// includes this module, which is what brings the constructor in. And it fails
/// loudly if the constructor did not run on some platform, rather than letting
/// every wrapped child spawn unbounded in silence.
pub fn arm() {
    assert!(
        ARMED_BEFORE_MAIN.get().is_some(),
        "the wrapped-child lifetime bound was not armed before `main`: \
         `child_lifetime_bound::arm_before_main` never ran in this process, so \
         {} is not pinned and a wrapped stand-in that outlives its wrapper is never \
         reaped (issues #668, #678)",
        dot_agent_deck::agent_pty::DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS
    );
}
