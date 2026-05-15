//! PRD #76 M2.11 — TUI organizational state persistence across reconnect.
//!
//! These tests pin the contract of [`tui_state_persist`]:
//!
//! - The persisted file round-trips display names and cwds through
//!   `write_atomic` / `load`.
//! - `reconcile_with_live` drops entries whose daemon agent_id is no
//!   longer alive (the daemon closed the agent — the stale name must
//!   not silently re-attach to a future unrelated agent).
//! - Corrupt / missing files do not crash the TUI — they surface as an
//!   empty default state, and rehydration falls back to using the
//!   agent_id as the display name.
//! - Atomic write protects the previous good file: writes go via
//!   `<path>.tmp` + fsync + rename so a crash mid-write leaves the
//!   previous file intact.
//! - `LocalDeck` mode never reads or writes the file (in-process daemon
//!   dies with the TUI; persistence buys nothing).
//!
//! The unit tests inside `src/tui_state_persist.rs` cover the same
//! invariants at the module level. The integration tests here focus on
//! behavior visible from outside the module — particularly the
//! controller-mode gating and the writer/reader interaction through the
//! real on-disk file.

#![cfg(unix)]

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::tui_state_persist::{
    PersistWriter, SCHEMA_VERSION, STATE_FILENAME, TuiPaneState, TuiPersistedState, load,
    write_atomic,
};

fn make_state(entries: &[(&str, &str, Option<&str>)]) -> TuiPersistedState {
    let mut panes = HashMap::new();
    for (agent_id, name, cwd) in entries {
        panes.insert(
            (*agent_id).to_string(),
            TuiPaneState {
                display_name: (*name).to_string(),
                cwd: cwd.map(|c| c.to_string()),
            },
        );
    }
    TuiPersistedState {
        version: SCHEMA_VERSION,
        panes,
    }
}

#[test]
fn roundtrip_preserves_display_name_and_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILENAME);

    let state = make_state(&[
        ("agent-alpha", "auditor", Some("/work/audit")),
        ("agent-beta", "coder", None),
    ]);
    write_atomic(&path, &state).expect("write");
    let loaded = load(&path);
    assert_eq!(loaded, state, "state must round-trip exactly");

    // Both fields survive.
    let alpha = loaded.panes.get("agent-alpha").unwrap();
    assert_eq!(alpha.display_name, "auditor");
    assert_eq!(alpha.cwd.as_deref(), Some("/work/audit"));
    let beta = loaded.panes.get("agent-beta").unwrap();
    assert_eq!(beta.display_name, "coder");
    assert!(beta.cwd.is_none());
}

#[test]
fn reconcile_drops_stale_then_file_rewritten_to_match() {
    // Scenario: the previous TUI session left 5 entries on disk. On
    // reconnect the daemon's `list_agents` returns only 3 live ids —
    // two agents were closed in the prior session before exit. The
    // reconcile path must drop the stale entries from the in-memory
    // map *and* rewrite the file so the dropped names can't silently
    // re-attach to a future unrelated agent reusing the id.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILENAME);

    let initial = make_state(&[
        ("a1", "auditor", Some("/work/a1")),
        ("a2", "coder", Some("/work/a2")),
        ("a3", "reviewer", None),
        ("a4", "old-1", Some("/tmp")),
        ("a5", "old-2", None),
    ]);
    write_atomic(&path, &initial).unwrap();

    let mut state = load(&path);
    let live = vec!["a1".to_string(), "a2".to_string(), "a3".to_string()];
    let changed = state.reconcile_with_live(&live);
    assert!(changed, "two stale entries should have been dropped");
    assert_eq!(state.panes.len(), 3);
    assert!(state.panes.contains_key("a1"));
    assert!(state.panes.contains_key("a2"));
    assert!(state.panes.contains_key("a3"));
    assert!(!state.panes.contains_key("a4"));
    assert!(!state.panes.contains_key("a5"));

    write_atomic(&path, &state).unwrap();

    // Re-read from disk: the GC must be persisted, not just in memory.
    let reread = load(&path);
    assert_eq!(reread.panes.len(), 3);
    assert!(!reread.panes.contains_key("a4"));
    assert!(!reread.panes.contains_key("a5"));
}

#[test]
fn fallback_when_state_file_missing_yields_agent_id_as_display_name() {
    // Today's pre-M2.11 behavior: when no persisted state exists, every
    // hydrated pane uses its agent_id as the display name. This must
    // remain the behavior on a first run, an XDG_STATE_HOME wipe, or
    // any other "no file" scenario.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.json");
    let loaded = load(&path);
    assert!(loaded.panes.is_empty(), "missing file → empty map");

    // Simulate the bootstrap loop: for each live agent_id, fall back
    // to agent_id as display_name when not in the persisted map.
    let live_ids = ["agent-1", "agent-2", "agent-3"];
    for id in live_ids {
        let display = loaded
            .panes
            .get(id)
            .map(|p| p.display_name.clone())
            .unwrap_or_else(|| id.to_string());
        assert_eq!(display, id, "fallback should be the agent_id itself");
    }
}

#[test]
fn corrupt_file_falls_back_to_empty_no_crash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILENAME);
    std::fs::write(&path, b"\xff\xfeNOT-JSON garbage{{}").unwrap();
    let loaded = load(&path);
    assert!(loaded.panes.is_empty());
    assert_eq!(loaded.version, SCHEMA_VERSION);
}

#[test]
fn atomic_write_does_not_destroy_existing_on_subsequent_writes() {
    // The atomic-write contract: a reader at any point in time sees
    // either the old file or the new file, never a half-written one.
    // The rename-into-place pattern guarantees this on Unix. We can't
    // easily mid-fail an OS rename in a portable test, so we instead
    // assert the file is always parseable after a sequence of writes
    // and that no `.tmp` linger after success.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILENAME);
    let tmp = dir.path().join(format!("{STATE_FILENAME}.tmp"));

    let s1 = make_state(&[("a", "one", None)]);
    write_atomic(&path, &s1).unwrap();
    assert_eq!(load(&path), s1);
    assert!(!tmp.exists());

    let s2 = make_state(&[("a", "two", Some("/x"))]);
    write_atomic(&path, &s2).unwrap();
    assert_eq!(load(&path), s2);
    assert!(!tmp.exists(), "tmp must be consumed by rename");

    let s3 = make_state(&[("a", "two", Some("/x")), ("b", "added", None)]);
    write_atomic(&path, &s3).unwrap();
    assert_eq!(load(&path), s3);
    assert!(!tmp.exists());
}

#[test]
fn stale_tmp_does_not_block_subsequent_write() {
    // A prior crashed write may have left a `.tmp` file behind. The
    // next write must succeed (truncate semantics on open) and the
    // rename must consume the stale tmp.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILENAME);
    let tmp = dir.path().join(format!("{STATE_FILENAME}.tmp"));
    std::fs::write(&tmp, b"leftover from a crashed previous write").unwrap();

    let state = make_state(&[("a1", "name", None)]);
    write_atomic(&path, &state).expect("write through stale tmp");
    let loaded = load(&path);
    assert_eq!(loaded, state);
    assert!(!tmp.exists());
}

#[test]
fn write_atomic_returns_error_when_parent_unwritable() {
    // Auditor invariant: the TUI must not die when the state dir is
    // read-only (`write_atomic` returns Err, the caller logs at debug
    // and drops the write — see `tui_state_persist::flush`). Make a
    // 0o500 dir so creating the tmp file fails and confirm the existing
    // file (if any) is left intact.
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    let path = locked.join(STATE_FILENAME);

    // Seed a good file, then drop write perms on the parent.
    let original = make_state(&[("a", "one", None)]);
    write_atomic(&path, &original).unwrap();
    let mut perms = std::fs::metadata(&locked).unwrap().permissions();
    perms.set_mode(0o500);
    std::fs::set_permissions(&locked, perms).unwrap();

    // Attempt a write — must fail without panicking and without
    // destroying the previous file.
    let next = make_state(&[("a", "two", Some("/x"))]);
    let result = write_atomic(&path, &next);
    let still_have_original = path.exists();

    // Restore perms so the temp dir can clean up.
    let mut restore = std::fs::metadata(&locked).unwrap().permissions();
    restore.set_mode(0o700);
    std::fs::set_permissions(&locked, restore).unwrap();

    assert!(
        result.is_err(),
        "write must surface the underlying I/O error so callers can log"
    );
    assert!(
        still_have_original,
        "atomic write must not destroy the existing file on failure"
    );
    let loaded = load(&path);
    assert_eq!(
        loaded, original,
        "previous good content must remain on disk"
    );
}

#[test]
fn local_deck_mode_does_not_persist() {
    // In `LocalDeck` the in-process daemon dies with the TUI: the agents
    // it knows about are gone the next time the binary starts. Reading
    // a persisted file in that mode would surface ghost names with no
    // matching live agents; writing it would litter the disk with state
    // that's never useful. The controller exposes `is_remote()` as the
    // single gate — `false` here means the TUI skips both load and the
    // PersistWriter spawn entirely.
    let ctrl = EmbeddedPaneController::new();
    assert!(
        !ctrl.is_remote(),
        "default (LocalDeck) controller must report !is_remote"
    );
    assert!(
        ctrl.runtime_handle().is_none(),
        "LocalDeck controller has no daemon runtime handle to share with the writer"
    );
    assert!(
        ctrl.stream_agent_ids().is_empty(),
        "LocalDeck controller has no stream-backed panes to persist"
    );
}

#[test]
fn remote_deck_mode_reports_is_remote_and_exposes_runtime_handle() {
    // The complement to the LocalDeck no-op test: `RemoteDeckLocal`
    // does opt into persistence. We exercise this from the same
    // synchronous test context the TUI bootstrap uses (no agents
    // attached yet, but the mode is right).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _guard = rt.enter();
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("attach.sock");
    let ctrl = EmbeddedPaneController::with_remote_deck(sock, rt.handle().clone());
    assert!(ctrl.is_remote());
    assert!(ctrl.runtime_handle().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_writer_debounces_and_writes() {
    // Submit a burst of state snapshots in rapid succession. The
    // debounced writer should coalesce them into a single fsynced write
    // landing the *last* state — not whichever intermediate state
    // happened to win a race.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILENAME);

    let writer = PersistWriter::spawn(path.clone(), tokio::runtime::Handle::current());

    let first = make_state(&[("a", "v1", None)]);
    let second = make_state(&[("a", "v2", None)]);
    let last = make_state(&[("a", "final", Some("/work"))]);
    assert!(writer.submit(first));
    assert!(writer.submit(second));
    assert!(writer.submit(last.clone()));

    // The debounce window is ~250ms; give it a generous slack to flush
    // on slow CI without making the test pointlessly long.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let loaded = load(&path);
    assert_eq!(loaded, last, "writer must land the most recent submission");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_writer_flushes_on_drop_after_pending_submit() {
    // The writer task observes EOF on the channel when `PersistWriter`
    // is dropped. A submit immediately before drop must still reach
    // disk — losing it would silently desync the file from in-memory
    // state at TUI shutdown.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(STATE_FILENAME);

    let writer = PersistWriter::spawn(path.clone(), tokio::runtime::Handle::current());
    let state = make_state(&[("a", "shutdown-state", Some("/x"))]);
    assert!(writer.submit(state.clone()));
    // Drop without waiting for the debounce: the writer task should
    // flush the last pending state on its EOF branch.
    drop(writer);

    // Give the runtime a moment to drive the writer's flush.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if path.exists() && load(&path) == state {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "writer must flush pending state on drop; last on disk = {:?}",
        std::fs::read_to_string(&path).ok()
    );
}
