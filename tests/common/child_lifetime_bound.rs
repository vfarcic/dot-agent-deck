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
//! orphans censused on one dev box with the oldest alive 9.4 days, each pinning
//! its e2e temp root `live-pid` against `cargo xtask clean-e2e-tmp` forever.
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
const CHILD_MAX_LIFETIME_SECS: &str = "300";

/// Pin the cap in this process's environment, once, if nothing pinned it first.
///
/// Set only when absent, so a test that pins its own (shorter) cap keeps it, and
/// so this stays idempotent: the `OnceLock` makes repeat calls free, and the
/// `var_os` check makes an ambient value win.
///
/// Deliberately NOT done through `.cargo/config.toml`'s `[env]`: that has no
/// per-subcommand scoping, so it would apply to `cargo run` as well and hand a
/// developer a deck whose daemon self-terminates at 300 s. Nor through
/// `.config/nextest.toml`'s `[env]`, which does not exist — nextest has no such
/// key at top level or per profile and *silently ignores* one, warning
/// `ignoring unknown configuration key: env` in the middle of a 3000-test run.
///
/// # Safety
///
/// `std::env::set_var` is `unsafe` in edition 2024 because it races any thread
/// concurrently reading the environment. Two things bound that here. The
/// `OnceLock` makes the write happen at most once per process. And every gate in
/// this repository runs the suite under `cargo nextest run`, which gives each
/// test its own process, so the only threads that can exist when this runs are
/// the calling test's own — the same argument `common::detach_from_any_live_deck`
/// has relied on for its `remove_var` since it was written, and the same one
/// `common::spawn_inprocess_daemon` already relies on by calling `init_test_env`
/// from inside a Tokio runtime. Call it during a test's setup, before it spawns
/// anything.
pub fn arm() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        if std::env::var_os(dot_agent_deck::agent_pty::DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS)
            .is_some()
        {
            return;
        }
        // SAFETY: see the `# Safety` section above — at most once per process,
        // during test setup, under a one-process-per-test runner.
        unsafe {
            std::env::set_var(
                dot_agent_deck::agent_pty::DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS,
                CHILD_MAX_LIFETIME_SECS,
            )
        };
    });
}
