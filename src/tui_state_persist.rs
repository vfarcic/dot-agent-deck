//! PRD #76 M2.11: persist TUI organizational state across reconnect.
//!
//! When a TUI in external-daemon mode reconnects after an ssh drop, the
//! daemon hands back a flat list of agent ids (`list_agents`). All the
//! organizational metadata the user built up — pane display names, the cwd
//! each pane was started in — lived only in the previous TUI process's
//! RAM and is gone. The dashboard then shows "1 1", "2 2", "3 3" instead
//! of the names the user assigned.
//!
//! This module owns a small JSON state file on the VM filesystem that the
//! TUI rewrites on every organizational change and reads back on
//! bootstrap. The daemon stays narrow (PTY supervisor); the file lives on
//! the VM, so a different laptop reconnecting reproduces the same view.
//!
//! ## Scope (v1)
//!
//! The persisted schema covers only what is strictly needed to make the
//! visible flat list match what the user had:
//!
//! - per-agent display name
//! - per-agent cwd (so future mode/orchestration restoration has the
//!   context it needs)
//!
//! Mode-tab and orchestration-tab *structural* restoration is **not** in
//! v1: rebuilding those would require new `TabManager` entry points that
//! "attach an existing daemon pane to a tab" instead of spawning new
//! panes (the current `open_mode_tab` / `open_orchestration_tab` both
//! spawn). That refactor is deferred — see the M2.11 entry in
//! `prds/76-remote-agent-environments.md`.
//!
//! ## Concurrency (v1)
//!
//! Same-user advisory only. If two laptops reconnect to the same VM
//! simultaneously, both TUIs are live and both rewrite the file —
//! last-writer-wins. This is the same shape tmux has and is accepted for
//! v1.
//!
//! ## File semantics
//!
//! - Path: `<state_dir>/tui-state.json` (same `state_dir()` helper as the
//!   daemon socket).
//! - Mode: 0o600 on the final file (same-user-only).
//! - Writes are atomic: write to `tui-state.json.tmp`, fsync, rename. A
//!   crash mid-write leaves the previous good file in place.
//! - Errors during writes are tolerated: the TUI must not die because the
//!   state dir is read-only or the disk is full — failures are logged at
//!   debug and dropped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Filename within `state_dir()` that holds the persisted state.
pub const STATE_FILENAME: &str = "tui-state.json";

/// Suffix appended to the path for atomic writes.
const TMP_SUFFIX: &str = ".tmp";

/// Debounce window for coalescing rapid edits into a single disk write.
/// Tuned for human-interactive churn: renaming a pane and immediately
/// renaming another shouldn't burn two fsyncs.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Current on-disk schema version. Bump when an incompatible change is
/// made; older readers will see the version mismatch and treat the file
/// as missing (the user re-organizes once, the new TUI writes v2).
pub const SCHEMA_VERSION: u32 = 1;

/// Per-agent organizational metadata persisted across reconnects.
///
/// Keyed by daemon agent id (which survives ssh drops) rather than local
/// pane id (which is reassigned per TUI process — though M2.x's
/// `pane_id_env` preservation does keep it stable when present).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiPaneState {
    /// Name shown in tab labels, dashboard headers, and recent-events
    /// lists. Falls back to the agent id if absent.
    pub display_name: String,
    /// Working directory the agent was launched in. Optional because
    /// rehydrated panes seeded by the agent-id-only fallback don't have
    /// it; carried through so future structural restoration can use it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// On-disk schema. `version` is the first field so a future reader can
/// `serde_json::from_slice` enough of the head to discriminate without
/// loading the rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiPersistedState {
    pub version: u32,
    #[serde(default)]
    pub panes: HashMap<String, TuiPaneState>,
}

impl Default for TuiPersistedState {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            panes: HashMap::new(),
        }
    }
}

impl TuiPersistedState {
    /// Drop entries whose agent_id is not in `live_agent_ids`. Returns
    /// `true` if any entry was removed (the caller should rewrite the
    /// file to garbage-collect).
    pub fn reconcile_with_live(&mut self, live_agent_ids: &[String]) -> bool {
        let live: std::collections::HashSet<&String> = live_agent_ids.iter().collect();
        let before = self.panes.len();
        self.panes.retain(|id, _| live.contains(id));
        self.panes.len() != before
    }
}

/// Read `path` and parse the persisted state. Returns `Default::default()`
/// (empty map, current version) when:
///
/// - the file does not exist (first run on this VM)
/// - the file is unreadable (permissions, transient I/O — TUI must not
///   die on these)
/// - the file is not valid JSON (a half-written or hand-edited file —
///   same behavior as missing; the next dirty write rewrites it)
/// - the schema version does not match `SCHEMA_VERSION`
///
/// All failure paths log at `tracing::debug` so the cause is observable
/// when diagnosing rehydration weirdness.
pub fn load(path: &Path) -> TuiPersistedState {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return TuiPersistedState::default();
        }
        Err(err) => {
            tracing::debug!(
                path = %path.display(),
                error = %err,
                "tui_state_persist::load: read failed, treating as empty"
            );
            return TuiPersistedState::default();
        }
    };
    match serde_json::from_slice::<TuiPersistedState>(&bytes) {
        Ok(state) if state.version == SCHEMA_VERSION => state,
        Ok(state) => {
            tracing::debug!(
                path = %path.display(),
                found = state.version,
                expected = SCHEMA_VERSION,
                "tui_state_persist::load: schema version mismatch, treating as empty"
            );
            TuiPersistedState::default()
        }
        Err(err) => {
            tracing::debug!(
                path = %path.display(),
                error = %err,
                "tui_state_persist::load: invalid JSON, treating as empty"
            );
            TuiPersistedState::default()
        }
    }
}

/// Atomically write `state` to `path`. Writes to `<path>.tmp` with mode
/// 0o600, fsyncs, then renames. On Unix the rename is atomic within the
/// same directory, so a concurrent reader sees either the old file or
/// the new file — never a half-written one.
///
/// Returns the I/O error on failure. Callers that don't care (the
/// debounced writer) log at debug and drop.
pub fn write_atomic(path: &Path, state: &TuiPersistedState) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "state file path has no parent directory",
        )
    })?;
    if !parent.exists() {
        // Best-effort. If creation fails the open below surfaces the
        // real error.
        let _ = std::fs::create_dir_all(parent);
    }

    let tmp_name = match path.file_name() {
        Some(name) => {
            let mut s = name.to_os_string();
            s.push(TMP_SUFFIX);
            s
        }
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "state file path has no file name",
            ));
        }
    };
    let tmp_path: PathBuf = parent.join(tmp_name);

    let json = serde_json::to_vec_pretty(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)?;
        file.write_all(&json)?;
        file.sync_all()?;
        // `file` drops here, closing the fd.
    }

    // Rename is atomic on Unix when src and dst are in the same dir.
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Build the canonical state-file path. Tests can override the location
/// via the `DOT_AGENT_DECK_STATE_DIR` env var, same as the daemon socket.
pub fn state_file_path() -> PathBuf {
    crate::config::state_dir().join(STATE_FILENAME)
}

/// Debounced background writer. The UI thread calls [`PersistWriter::submit`]
/// from end-of-event-loop sampling; submits within `DEBOUNCE` of one
/// another are coalesced into a single fsynced write.
///
/// `PersistWriter` is cheap to construct and the writer task it spawns
/// terminates cleanly when `self` is dropped (the channel sender closes
/// and the task observes EOF). Holding it for the lifetime of the TUI is
/// the intended usage.
pub struct PersistWriter {
    tx: mpsc::UnboundedSender<TuiPersistedState>,
    _join: tokio::task::JoinHandle<()>,
}

impl PersistWriter {
    /// Spawn a debounced writer that flushes to `path`. `handle` is the
    /// runtime the writer task lives on — the TUI passes the tokio
    /// runtime handle it already keeps for stream-backed panes.
    pub fn spawn(path: PathBuf, handle: tokio::runtime::Handle) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<TuiPersistedState>();
        let join = handle.spawn(async move {
            loop {
                // Wait for the first dirty signal. EOF here = sender
                // dropped = TUI exiting; cleanly stop.
                let mut pending = match rx.recv().await {
                    Some(state) => state,
                    None => break,
                };
                // Coalesce: keep draining as long as new submits arrive
                // within DEBOUNCE of the last one. The most recent state
                // wins; intermediate ones are dropped because they would
                // produce the same on-disk result anyway.
                loop {
                    let deadline = tokio::time::Instant::now() + DEBOUNCE;
                    tokio::select! {
                        biased;
                        next = rx.recv() => match next {
                            Some(state) => { pending = state; }
                            None => {
                                // Sender closed mid-debounce: flush
                                // whatever we have, then exit. Losing
                                // this last write because of a tight
                                // race would silently desync the file
                                // from the in-memory state.
                                flush(&path, &pending);
                                return;
                            }
                        },
                        _ = tokio::time::sleep_until(deadline) => break,
                    }
                }
                flush(&path, &pending);
            }
        });
        Self { tx, _join: join }
    }

    /// Queue a state snapshot to be persisted. Never blocks; intermediate
    /// snapshots are coalesced. Returns `false` if the writer task has
    /// exited (channel closed) — the caller can drop it and move on.
    pub fn submit(&self, state: TuiPersistedState) -> bool {
        self.tx.send(state).is_ok()
    }
}

fn flush(path: &Path, state: &TuiPersistedState) {
    if let Err(err) = write_atomic(path, state) {
        tracing::debug!(
            path = %path.display(),
            error = %err,
            "tui_state_persist::flush: write_atomic failed, dropping"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TuiPersistedState {
        let mut panes = HashMap::new();
        panes.insert(
            "agent-1".to_string(),
            TuiPaneState {
                display_name: "auditor".to_string(),
                cwd: Some("/tmp/work".to_string()),
            },
        );
        panes.insert(
            "agent-2".to_string(),
            TuiPaneState {
                display_name: "coder".to_string(),
                cwd: None,
            },
        );
        TuiPersistedState {
            version: SCHEMA_VERSION,
            panes,
        }
    }

    #[test]
    fn write_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILENAME);
        let state = sample();
        write_atomic(&path, &state).expect("write");
        let loaded = load(&path);
        assert_eq!(loaded, state);
    }

    #[test]
    fn load_missing_returns_empty_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let loaded = load(&path);
        assert_eq!(loaded, TuiPersistedState::default());
        assert!(loaded.panes.is_empty());
        assert_eq!(loaded.version, SCHEMA_VERSION);
    }

    #[test]
    fn load_corrupt_returns_empty_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILENAME);
        std::fs::write(&path, b"not json at all {{{").unwrap();
        let loaded = load(&path);
        assert_eq!(loaded, TuiPersistedState::default());
    }

    #[test]
    fn load_wrong_version_returns_empty_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILENAME);
        std::fs::write(&path, br#"{"version":999,"panes":{}}"#).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.version, SCHEMA_VERSION);
        assert!(loaded.panes.is_empty());
    }

    #[test]
    fn reconcile_drops_stale_entries() {
        let mut state = sample();
        let live = vec!["agent-1".to_string()];
        let changed = state.reconcile_with_live(&live);
        assert!(changed);
        assert_eq!(state.panes.len(), 1);
        assert!(state.panes.contains_key("agent-1"));
        assert!(!state.panes.contains_key("agent-2"));
    }

    #[test]
    fn reconcile_no_change_when_all_live() {
        let mut state = sample();
        let live = vec!["agent-1".to_string(), "agent-2".to_string()];
        let changed = state.reconcile_with_live(&live);
        assert!(!changed);
        assert_eq!(state.panes.len(), 2);
    }

    #[test]
    fn write_atomic_preserves_previous_on_tmp_clash() {
        // If a stale .tmp exists from a prior crashed write, the new
        // write must still succeed (truncate semantics) and leave a
        // valid final file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILENAME);
        let tmp = dir.path().join(format!("{STATE_FILENAME}{TMP_SUFFIX}"));
        std::fs::write(&tmp, b"garbage from previous crash").unwrap();

        let state = sample();
        write_atomic(&path, &state).expect("write succeeds despite stale tmp");
        let loaded = load(&path);
        assert_eq!(loaded, state);
        // The stale .tmp has been replaced by the rename.
        assert!(!tmp.exists(), ".tmp should be consumed by rename");
    }

    #[test]
    fn write_atomic_does_not_destroy_existing_on_serialize_failure() {
        // serde_json::to_vec_pretty cannot fail for our schema, but we
        // can simulate the "atomic write protects the existing file"
        // invariant by aborting mid-flow: write a good file, then
        // write_atomic with a different state — confirm the file is the
        // new state and that no .tmp lingers. (The atomic-write
        // invariant we're really protecting is "no half-written file is
        // visible at the canonical path"; the rename guarantees that.)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILENAME);
        let original = sample();
        write_atomic(&path, &original).unwrap();
        assert_eq!(load(&path), original);

        let mut next = original.clone();
        next.panes
            .get_mut("agent-1")
            .unwrap()
            .display_name
            .push_str("-renamed");
        write_atomic(&path, &next).unwrap();
        assert_eq!(load(&path), next);

        let tmp = dir.path().join(format!("{STATE_FILENAME}{TMP_SUFFIX}"));
        assert!(!tmp.exists(), ".tmp should not linger after success");
    }

    #[test]
    fn write_atomic_uses_0o600_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILENAME);
        write_atomic(&path, &sample()).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "state file must be user-only readable");
    }
}
