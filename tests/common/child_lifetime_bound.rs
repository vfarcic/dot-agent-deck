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
//! # Why this is its own file rather than a function in `common/mod.rs`
//!
//! It started as one, and a function in `common/mod.rs` can only be called by a
//! file that links `common`. Three of the files that reach a bare
//! `AgentPtyRegistry` do not, and not by oversight — `tests/rehydration.rs`,
//! `tests/daemon_protocol.rs` and `tests/shell_activity.rs` avoid `mod common;`
//! deliberately, because `tests/common/mod.rs` is ~420 KB of PTY/vt100 harness
//! and pulling it into a fast-tier crate to reach one `set_var` is a real
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
//! and `common::init_test_env()` calls the same [`arm`] — one implementation and
//! one SAFETY argument rather than one copy per spawn-owning crate.
//!
//! **Self-contained, for the same reason `src/test_temp.rs` is** (issue #474):
//! this file is compiled into every crate that `#[path]`-includes it, where
//! `crate::` names that *test binary's* own root and nothing this repository
//! defines is in scope. It uses `std` plus one public constant from the library
//! by its extern-crate path; no `crate::` or file-scope `super::` path may
//! appear here.
//!
//! Enforced by linkage-check rule 10: a file under `tests/` that constructs an
//! `AgentPtyRegistry` or calls `run_daemon_with` must arm the bound, so the next
//! spawn site to be written cannot silently repeat the gap.

use std::sync::OnceLock;

/// How long a process spawned out of a test process may live before its own
/// backstop ends it, in seconds. Matches the value `TuiDeck` and `DaemonProc`
/// pin explicitly after their `env_clear`, and the 300 s figure
/// `docs/develop/e2e-temp-dirs.md` reasons about when it sets the reaper's
/// 10-minute floor at 2x the orphan cap.
///
/// Also the CEILING, not just the default — see [`clamped`].
const CHILD_MAX_LIFETIME_SECS: u64 = 300;

/// The cap this process should pin, given whatever was already in `ambient`.
///
/// A **shorter** ambient value wins: several tests pin their own (`wrap_io.rs`
/// at 120 s, the fd-table probe at 10 s) precisely so nothing they mint can
/// outlive the case that made it, and that is deliberate.
///
/// A **longer** one does not, and neither does an unparseable or zero one. Both
/// are replaced by [`CHILD_MAX_LIFETIME_SECS`], because 300 s is not a
/// preference here — it is the number `cargo xtask clean-e2e-tmp`'s deletion
/// safety rests on. That reaper reaps a root whose owning test process is dead
/// once the root is 10 minutes old, and it picks 10 minutes as *2x this cap*
/// (`docs/develop/e2e-temp-dirs.md`, "The 10-minute floor on dead owners"). So
/// an exported `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS=3600` does not merely
/// widen a test-only bound: it silently converts the reaper's margin into a
/// deficit, and `--apply` can then delete a root out from under a descendant
/// still nominally entitled to write there for another 50 minutes. Measured
/// before this clamp existed: that exact value was accepted end to end and the
/// arming regression test passed with it.
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

/// Pin the cap in this process's environment, once.
///
/// A shorter ambient cap is kept; anything else — absent, zero, unparseable, or
/// above the 300 s ceiling — is replaced. See [`clamped`] for why the ceiling is
/// enforced rather than merely defaulted.
///
/// Idempotent: the `OnceLock` makes repeat calls free.
///
/// Deliberately NOT done through `.cargo/config.toml`'s `[env]`: that has no
/// per-subcommand scoping, so it would apply to `cargo run` as well and hand a
/// developer a deck whose daemon self-terminates at 300 s. Nor through
/// `.config/nextest.toml`'s `[env]`, which does not exist — nextest has no such
/// key at top level or per profile and *silently ignores* one (re-measured on
/// cargo-nextest 0.9.143: both `[env]` and `[profile.default] env = {…}` are
/// accepted without error and reach no test process).
///
/// # Safety
///
/// `std::env::set_var` is `unsafe` in edition 2024 because it races any thread
/// concurrently *reading* the environment, in Rust or in C, and that is a
/// data race rather than merely a lost update. State the residual plainly:
///
/// - The `OnceLock` bounds this to **one** write per process. It serialises
///   calls to this function and **excludes nothing else** — it is load-bearing
///   for idempotence and decorative for thread safety.
/// - One process per test (every gate here runs under `cargo nextest run`) does
///   **not** imply one thread per process. Several callers reach this from
///   inside a multi-threaded Tokio runtime whose workers already exist —
///   `tests/delegate_prompt_injection.rs`'s `#[tokio::test(flavor =
///   "multi_thread")]` body, and `common::spawn_inprocess_daemon`. A worker
///   calling `getenv` concurrently with this write is unsound, and nothing here
///   prevents it.
/// - What is actually true is weaker and worth having anyway: this is a single,
///   idempotent, setup-time write performed before the calling test spawns
///   anything, of a value no library thread in this process reads. That is the
///   same profile `common::detach_from_any_live_deck`'s `remove_var` has run on
///   since it was written — an **inherited** argument, not a proof.
///
/// Kept rather than replaced because the alternatives were measured and cost
/// more than they buy: a registry/`SpawnOptions` child-environment overlay
/// applied at `agent_pty::spawn` reaches only children of that call, and 4 armed
/// test files spawn the deck binary through a raw `std::process::Command` that
/// would silently lose the cap. Tracked for a real fix by **issue #678**, which
/// covers this call and `detach_from_any_live_deck`'s `remove_var` together
/// because they share the defect and the second is where the argument came
/// from. Call it during a test's setup, before it spawns anything.
pub fn arm() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let var = dot_agent_deck::agent_pty::DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS;
        let ambient = std::env::var(var).ok();
        let Some(value) = clamped(ambient.as_deref()) else {
            return;
        };
        // SAFETY: see the `# Safety` section above. Not a proof — a stated
        // residual: at most once per process, during test setup, of a value no
        // thread in this process reads, under a one-process-per-test runner.
        unsafe { std::env::set_var(var, value) };
    });
}
