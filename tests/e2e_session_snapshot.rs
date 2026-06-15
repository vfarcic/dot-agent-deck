#![cfg(feature = "e2e")]

//! PRD #89 Phase 4 — the two *fresh-start escape hatches*.
//!
//! Auto-restore is now the default (Phase 2/2b), so a user who wants to start
//! clean needs one obvious action. There are two:
//!
//!   * **Local / global (M4.2):** a `dot-agent-deck snapshot clear` subcommand
//!     deletes the local saved-session snapshot. `snapshot` is a subcommand GROUP
//!     with a `clear` action (decided in `.dot-agent-deck/prd-89-context.md`; NOT
//!     `reset`/`--reset`). The snapshot is a SINGLE GLOBAL file, so this is the one
//!     fresh-start action.
//!   * **Remote remove is registry-only (M4.1):** removing a registered deck
//!     (`dot-agent-deck remote remove <name>`) edits ONLY the remote registry and
//!     intentionally does NOT touch the global snapshot (decided Option 1 — there
//!     is no per-deck saved state to clear). The 002 guard pins exactly that.
//!
//! Both are thin real-binary subprocess spawns — no PTY drive is needed. Each
//! redirects `DOT_AGENT_DECK_SESSION` to a test-owned snapshot path (and
//! `DOT_AGENT_DECK_REMOTES` to a test-owned registry) so the spawn never reads
//! or mutates the developer's real session/registry, and so nothing escapes the
//! per-call tempdir.
//!
//! Decision 6: gated behind the `e2e` feature so `cargo test-fast` never
//! compiles it.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use spec::spec;

/// Path to the freshly-built binary under test (Cargo sets this at
/// integration-test build time).
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_dot-agent-deck")
}

/// Run the binary with `args` under an isolated, scrubbed environment so the
/// spawn can neither read the developer's real session/config/registry nor leak
/// a long-lived daemon: a per-call tempdir HOME + sockets + state dir, a short
/// idle-shutdown, and the test max-lifetime backstop. `extra_env` adds the
/// test-owned `DOT_AGENT_DECK_SESSION` / `DOT_AGENT_DECK_REMOTES` overrides.
/// stdin is `/dev/null`; stdout/stderr are captured. Returns the captured
/// `Output` (combined stdout + stderr text and the exit status). Mirrors
/// `e2e_cli_continue_removed.rs`'s `run_isolated`, generalized over env.
fn run_isolated(args: &[&str], extra_env: &[(&str, &Path)]) -> (std::process::Output, String) {
    let home = common::race_safe_tempdir();
    let h: &Path = home.path();
    let mut cmd = Command::new(bin());
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
    // Reap any daemon this spawn lazily starts quickly (no clients/agents) and
    // cap its lifetime as a backstop.
    cmd.env("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "1");
    cmd.env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "30");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let out = cmd.output().expect("spawn dot-agent-deck");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Keep `home` alive until after the child has fully exited.
    drop(home);
    (out, combined)
}

/// Stage a non-empty saved-session `session.toml` at `session_file` describing a
/// single dashboard pane, so the "is the snapshot gone afterward?" assertions
/// have a real file to clear (not a no-op on an absent file).
fn stage_nonempty_snapshot(session_file: &Path, dir: &Path) {
    let dir = dir.to_str().expect("snapshot dir is UTF-8");
    let s = format!(
        "[[panes]]\ndir = \"{}\"\nname = \"restored-pane\"\ncommand = \"sleep 600\"\n",
        dir.replace('\\', "\\\\").replace('"', "\\\"")
    );
    std::fs::write(session_file, s).expect("write staged session.toml");
}

/// Scenario: Stage a non-empty `session.toml` at the path
/// `DOT_AGENT_DECK_SESSION` points to, then run `dot-agent-deck snapshot clear`
/// as a plain subprocess. The new local fresh-start escape hatch must exit 0 and
/// delete the snapshot file, so a subsequent no-flag startup would land on an
/// empty dashboard. RED today: the `snapshot` subcommand group does not exist, so
/// `clap` rejects `snapshot` as an unrecognized subcommand (non-zero exit) and
/// the staged file is left untouched.
#[spec("session/snapshot/001")]
#[test]
fn snapshot_001_snapshot_clear_deletes_local_snapshot() {
    let session_dir = common::race_safe_tempdir();
    let session_file = session_dir.path().join("session.toml");
    stage_nonempty_snapshot(&session_file, session_dir.path());

    // Precondition: a real snapshot is on disk before we clear it.
    assert!(
        session_file.exists(),
        "the staged snapshot must exist before `snapshot clear`, but none was found at \
         {session_file:?}"
    );

    let (out, text) = run_isolated(
        &["snapshot", "clear"],
        &[("DOT_AGENT_DECK_SESSION", session_file.as_path())],
    );

    // The new subcommand must succeed. RED today: `snapshot` is not a declared
    // subcommand, so `clap` rejects it and the process exits non-zero.
    assert!(
        out.status.success(),
        "PRD #89 M4.2: `dot-agent-deck snapshot clear` must exit 0, but it exited {:?}. \
         RED until the `snapshot` subcommand group (with a `clear` action) is added to the \
         CLI — today `clap` rejects `snapshot` as an unrecognized subcommand.\noutput:\n{text}",
        out.status.code()
    );

    // …and it must actually delete the local snapshot (the fresh-start payoff).
    assert!(
        !session_file.exists(),
        "PRD #89 M4.2: `dot-agent-deck snapshot clear` must DELETE the local snapshot at \
         {session_file:?}, but the file is still present afterward.\noutput:\n{text}"
    );
}

/// Stage a `remotes.toml` registry at `remotes_file` carrying a single entry
/// named `name`, so `dot-agent-deck remote remove <name>` has a deck to remove
/// (the command errors on an unknown name). Hand-rolled TOML mirroring
/// `dot_agent_deck::remote::RemoteEntry`'s serialized shape.
fn stage_remote_registry(remotes_file: &Path, name: &str) {
    let s = format!(
        "[[remotes]]\nname = \"{name}\"\ntype = \"ssh\"\nhost = \"example.com\"\nport = 22\n\
         version = \"0.1.0\"\nadded_at = \"2026-06-14T00:00:00Z\"\n"
    );
    std::fs::write(remotes_file, s).expect("write staged remotes.toml");
}

/// Scenario: Register a remote deck `myhost` in a test-owned `remotes.toml`
/// (`DOT_AGENT_DECK_REMOTES`) and stage a non-empty `session.toml`
/// (`DOT_AGENT_DECK_SESSION`), then run `dot-agent-deck remote remove myhost`.
/// The snapshot is a single GLOBAL file, so `remote remove` is registry-only
/// (decided Option 1): it must exit 0 AND LEAVE the global snapshot intact —
/// same path, same contents — because there is no per-deck saved state to
/// clear. The one fresh-start action is `snapshot clear` (covered by 001).
#[spec("session/snapshot/002")]
#[test]
fn snapshot_002_remote_remove_is_registry_only_leaves_snapshot_intact() {
    let session_dir = common::race_safe_tempdir();
    let session_file = session_dir.path().join("session.toml");
    stage_nonempty_snapshot(&session_file, session_dir.path());

    // Capture the staged snapshot's exact contents so we can prove `remote
    // remove` left it byte-for-byte untouched, not merely that some file exists.
    let before =
        std::fs::read_to_string(&session_file).expect("read staged snapshot before remove");

    let remotes_dir = common::race_safe_tempdir();
    let remotes_file = remotes_dir.path().join("remotes.toml");
    stage_remote_registry(&remotes_file, "myhost");

    let (out, text) = run_isolated(
        &["remote", "remove", "myhost"],
        &[
            ("DOT_AGENT_DECK_SESSION", session_file.as_path()),
            ("DOT_AGENT_DECK_REMOTES", remotes_file.as_path()),
        ],
    );

    // The removal succeeds (the registry entry exists). This is a sanity guard so
    // an unrelated failure is not silently read as "snapshot preserved".
    assert!(
        out.status.success(),
        "`dot-agent-deck remote remove myhost` should exit 0 (the registry entry exists), \
         but it exited {:?}.\noutput:\n{text}",
        out.status.code()
    );

    // The decided behavior (Option 1): `remote remove` is registry-only and must
    // LEAVE the global snapshot intact — the file is still present afterward…
    assert!(
        session_file.exists(),
        "PRD #89 M4.1 (Option 1): `dot-agent-deck remote remove myhost` is registry-only and must \
         LEAVE the global snapshot at {session_file:?} intact, but the file is gone afterward. \
         There is no per-deck saved state to clear; `snapshot clear` is the one fresh-start \
         action.\noutput:\n{text}"
    );

    // …with its contents unchanged: remove never reads or rewrites the snapshot.
    let after = std::fs::read_to_string(&session_file).expect("read snapshot after remove");
    assert_eq!(
        before, after,
        "PRD #89 M4.1 (Option 1): `remote remove` must not modify the global snapshot, but its \
         contents at {session_file:?} changed.\noutput:\n{text}"
    );
}
