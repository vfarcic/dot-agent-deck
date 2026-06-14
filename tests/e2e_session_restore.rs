#![cfg(feature = "e2e")]

//! PRD #89 Phase 2 — L2 (real-binary PTY) coverage for *auto-restore on
//! startup*.
//!
//! Phase 1 made the saved-session snapshot continuously fresh; Phase 2 makes
//! restoring it UNCONDITIONAL on every TUI startup — no `--continue` flag.
//! Precedence: try daemon hydration first; if hydration produced any panes the
//! daemon state wins and snapshot restore is skipped; if hydration produced
//! zero panes (fresh daemon / crash recovery), load and apply the disk
//! snapshot; if both are empty, land at an empty dashboard.
//!
//! These tests drive the REAL binary through a PTY with `DOT_AGENT_DECK_SESSION`
//! redirected to a test-owned path. No LLM tokens are spent — restored/spawned
//! panes run `sleep 600` (Agent: none).
//!
//! Decision 6: gated behind the `e2e` feature so `cargo test-fast` never
//! compiles it.

mod common;

use std::path::Path;
use std::time::Duration;

use common::TuiDeck;
use spec::spec;

/// Stage a saved-session `session.toml` at `session_file` describing each
/// `(name, command)` pane, all rooted at `dir` (which must already exist on
/// disk so the restore path's dir-exists check does not skip them). Hand-rolled
/// TOML mirroring `dot_agent_deck::config::SavedPane` — the multi-pane analogue
/// of the harness's private `write_continue_session_file`, but usable WITHOUT
/// `--continue` (we write only the file; the launch passes no flag).
fn stage_session_snapshot(session_file: &Path, dir: &Path, panes: &[(&str, &str)]) {
    let dir = dir.to_str().expect("snapshot dir is UTF-8");
    let mut s = String::new();
    for (name, command) in panes {
        s.push_str("[[panes]]\n");
        s.push_str(&format!("dir = \"{}\"\n", toml_basic_escape(dir)));
        s.push_str(&format!("name = \"{}\"\n", toml_basic_escape(name)));
        s.push_str(&format!("command = \"{}\"\n\n", toml_basic_escape(command)));
    }
    std::fs::write(session_file, s).expect("write staged session.toml");
}

/// Minimal TOML basic-string escape for the values we embed (filesystem paths
/// and short ASCII names) — backslash and double-quote only, which is all a
/// Linux tempdir path or a `restored-*` name can contain here.
fn toml_basic_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Hand-stage a `session.toml` describing a SINGLE saved pane that carries an
/// `[panes.orchestration]` block, using the EXACT serialized key names the coder
/// pinned for `OrchestrationSnapshot` (`version` / `roles` / `start_role_index`
/// / `orchestrator_prompt` / `config_name` / `project_path` /
/// `started_role_indices`). The daemon-empty restore path consumes this to
/// rebuild the orchestration tab (008) or to detect drift and fall back (009).
#[allow(clippy::too_many_arguments)]
fn stage_orchestration_snapshot(
    session_file: &Path,
    dir: &Path,
    pane_name: &str,
    command: &str,
    roles: &[&str],
    start_role_index: usize,
    orchestrator_prompt: &str,
    config_name: &str,
    project_path: &Path,
    started_role_indices: &[usize],
) {
    let dir = dir.to_str().expect("snapshot dir is UTF-8");
    let project_path = project_path.to_str().expect("project_path is UTF-8");
    let roles_list = roles
        .iter()
        .map(|r| format!("\"{}\"", toml_basic_escape(r)))
        .collect::<Vec<_>>()
        .join(", ");
    let started_list = started_role_indices
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut s = String::new();
    s.push_str("[[panes]]\n");
    s.push_str(&format!("dir = \"{}\"\n", toml_basic_escape(dir)));
    s.push_str(&format!("name = \"{}\"\n", toml_basic_escape(pane_name)));
    s.push_str(&format!("command = \"{}\"\n\n", toml_basic_escape(command)));
    s.push_str("[panes.orchestration]\n");
    s.push_str("version = 1\n");
    s.push_str(&format!("roles = [{roles_list}]\n"));
    s.push_str(&format!("start_role_index = {start_role_index}\n"));
    s.push_str(&format!(
        "orchestrator_prompt = \"{}\"\n",
        toml_basic_escape(orchestrator_prompt)
    ));
    s.push_str(&format!(
        "config_name = \"{}\"\n",
        toml_basic_escape(config_name)
    ));
    s.push_str(&format!(
        "project_path = \"{}\"\n",
        toml_basic_escape(project_path)
    ));
    s.push_str(&format!("started_role_indices = [{started_list}]\n"));
    std::fs::write(session_file, s).expect("write staged orchestration session.toml");
}

/// Write an orchestration `.dot-agent-deck.toml` into `project_dir`: a single
/// `[[orchestrations]]` named `config_name` whose roles are `(name, command)`
/// pairs in order, with the role at `start_idx` flagged `start = true`. The
/// staged snapshot's `config_name` + `project_path` point here so the restore
/// branch can re-resolve the `OrchestrationConfig` (008), or — when the names
/// no longer match — detect drift (009).
fn write_orchestration_config(
    project_dir: &Path,
    config_name: &str,
    roles: &[(&str, &str)],
    start_idx: usize,
) {
    let mut s = String::new();
    s.push_str("[[orchestrations]]\n");
    s.push_str(&format!(
        "name = \"{}\"\n\n",
        toml_basic_escape(config_name)
    ));
    for (i, (name, command)) in roles.iter().enumerate() {
        s.push_str("[[orchestrations.roles]]\n");
        s.push_str(&format!("name = \"{}\"\n", toml_basic_escape(name)));
        s.push_str(&format!("command = \"{}\"\n", toml_basic_escape(command)));
        if i == start_idx {
            s.push_str("start = true\n");
        }
        s.push('\n');
    }
    std::fs::write(project_dir.join(".dot-agent-deck.toml"), s)
        .expect("write orchestration .dot-agent-deck.toml");
}

/// Write a recorder "agent" script into `project_dir` and return its ABSOLUTE
/// path (to use as a role command). The script records that it started, self-
/// posts a synthetic `SessionStart` via the real `dot-agent-deck hook` path (the
/// readiness signal the orchestrator-prompt delivery gate waits on), then
/// appends every stdin line it receives to an ABSOLUTE `record-<role>.log` under
/// `project_dir` — so a replayed prompt surfaces as a recorded line, immune to
/// PTY echo AND independent of the role pane's working directory. Mirrors the
/// proven recorder pattern in `e2e_mode_seed_prompt.rs`.
fn write_recorder_agent(project_dir: &Path, role: &str) -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let script_path = project_dir.join(format!("agent-{role}.sh"));
    let started = project_dir.join(format!("started-{role}.log"));
    let record = project_dir.join(format!("record-{role}.log"));
    let body = format!(
        "#!/bin/sh\n\
         echo started >> \"{started}\"\n\
         printf '%s' '{{\"hook_event_name\":\"SessionStart\",\"session_id\":\"restore-{role}\"}}' \
         | \"{bin}\" hook claude-code >/dev/null 2>&1\n\
         while IFS= read -r l; do printf '%s\\n' \"$l\" >> \"{record}\"; done\n",
        started = started.display(),
        record = record.display(),
    );
    std::fs::write(&script_path, body).expect("write recorder agent script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod recorder agent script");
    }
    script_path
        .to_str()
        .expect("recorder script path is UTF-8")
        .to_string()
}

/// Scenario: Stage a `session.toml` describing two dashboard panes
/// (`restored-alpha`, `restored-beta`, both `sleep 600`) at the path
/// `DOT_AGENT_DECK_SESSION` points to, then launch the deck against a fresh
/// (empty) daemon with NO `--continue` flag. Auto-restore must recreate both
/// saved panes as dashboard cards without any flag. RED today: the snapshot
/// load is gated behind `if continue_session` in `run_tui`, so with no flag the
/// block never runs and neither saved pane appears — the dashboard stays at
/// "No active sessions".
#[spec("session/restore/001")]
#[test]
fn restore_001_no_flag_startup_restores_panes_from_snapshot() {
    // A test-owned snapshot dir the deck's `session_path()` reads via
    // `DOT_AGENT_DECK_SESSION`. It also doubles as the restored panes' working
    // directory — it exists on disk, so the restore path's `dir.is_dir()` guard
    // keeps both panes (rather than skipping them as missing-dir).
    let session_dir = common::race_safe_tempdir();
    let session_file = session_dir.path().join("session.toml");
    stage_session_snapshot(
        &session_file,
        session_dir.path(),
        &[
            ("restored-alpha", "sleep 600"),
            ("restored-beta", "sleep 600"),
        ],
    );

    // No `--continue` — `launch_with_fixture` only passes the flag when a
    // `with_continue_session(...)` was staged, which it was not. The daemon
    // this deck lazy-spawns is brand new (empty), so hydration yields nothing
    // and the disk snapshot is the only possible source of panes.
    let deck = TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            session_file.to_str().expect("session path is UTF-8"),
        )
        .launch_with_fixture("modes");

    // After Phase 2, both saved panes auto-restore as dashboard cards. Their
    // saved names appear in the card title rows (e.g. "1 restored-alpha").
    let restored = common::wait_until(Duration::from_secs(10), || {
        let grid = deck.snapshot_grid();
        grid.contains("restored-alpha") && grid.contains("restored-beta")
    });
    assert!(
        restored,
        "PRD #89 M2.1: launching with NO --continue and a 2-pane snapshot on disk must \
         auto-restore BOTH saved panes (`restored-alpha`, `restored-beta`) as dashboard \
         cards, but they never appeared. RED until the snapshot-load block in `run_tui` \
         is made unconditional (today it is gated on `continue_session`).\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Launch the deck against a fresh (empty) daemon with NO snapshot on
/// disk and NO `--continue` flag — the both-empty case. The deck must land on a
/// clean empty dashboard ("No active sessions") with no restore warning, and
/// remain interactive (Ctrl+N opens the new-pane directory picker). This locks
/// the post-Phase-2 invariant that making restore unconditional must still fall
/// through cleanly when there is nothing to restore from either source.
#[spec("session/restore/006")]
#[test]
fn restore_006_empty_daemon_and_no_snapshot_lands_on_clean_dashboard() {
    let session_dir = common::race_safe_tempdir();
    let session_file = session_dir.path().join("session.toml");
    // Nothing staged → `SavedSession::load()` returns the empty default.
    assert!(
        !session_file.exists(),
        "no snapshot must exist for the both-empty case, but one was found at {session_file:?}"
    );

    let deck = TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            session_file.to_str().expect("session path is UTF-8"),
        )
        .launch_with_fixture("modes");

    // Empty daemon + empty snapshot → the empty-dashboard placeholder.
    deck.wait_for_string("No active sessions");

    // No restore warning should be surfaced when there is nothing to restore.
    let grid = deck.snapshot_grid();
    assert!(
        !grid.contains("Warning:"),
        "the both-empty startup must not surface any restore warning, but the dashboard \
         shows one.\nFinal grid:\n{grid}"
    );

    // Interactive: the global Ctrl+N opens the new-pane directory picker.
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
}

/// Scenario: Stage an orchestration `.dot-agent-deck.toml` (`tdd-cycle` with an
/// `orchestrator`+`coder`+`reviewer` set, the orchestrator a recorder agent) in
/// a test-owned project dir, then hand-stage a `session.toml` whose single pane
/// carries a `[panes.orchestration]` block pointing `config_name`/`project_path`
/// at that dir (with `orchestrator_prompt = "Build the feature end to end"`,
/// `start_role_index = 0`). Launch against a fresh (empty) daemon with NO flag.
/// The daemon-empty restore must rebuild the orchestration tab: the `coder` and
/// `reviewer` role panes appear as deck cards in their saved order, and — unlike
/// warm hydration — the saved `orchestrator_prompt` is replayed to the start
/// (orchestrator) role, which the recorder captures (echo-immune). RED today:
/// there is no snapshot-fallback orchestration restore branch, so the saved pane
/// comes back as a single plain dashboard card and neither the role panes nor
/// the prompt replay ever materialize.
#[spec("session/restore/008")]
#[test]
fn restore_008_daemon_empty_snapshot_rebuilds_orchestration_tab() {
    // The orchestration config + the orchestrator recorder live in a test-owned
    // project dir the staged snapshot references, so `OrchestrationConfig`
    // re-resolution succeeds independently of the deck's own (fixture) cwd.
    let project_dir = common::race_safe_tempdir();
    let orchestrator_cmd = write_recorder_agent(project_dir.path(), "orchestrator");
    write_orchestration_config(
        project_dir.path(),
        "tdd-cycle",
        &[
            ("orchestrator", orchestrator_cmd.as_str()),
            ("coder", "sleep 600"),
            ("reviewer", "sleep 600"),
        ],
        0,
    );

    let session_dir = common::race_safe_tempdir();
    let session_file = session_dir.path().join("session.toml");
    stage_orchestration_snapshot(
        &session_file,
        project_dir.path(),
        "orchestrator",
        &orchestrator_cmd,
        &["orchestrator", "coder", "reviewer"],
        0,
        "Build the feature end to end",
        "tdd-cycle",
        project_dir.path(),
        &[0, 1],
    );

    // No `--continue` flag; the lazy-spawned daemon is brand new (empty), so the
    // disk snapshot is the only possible source — the snapshot-fallback path.
    let deck = TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            session_file.to_str().expect("session path is UTF-8"),
        )
        .launch_with_fixture("minimal");

    // The orchestration tab must be rebuilt AND shown (start cursor): its
    // non-start role panes render as deck cards by role name, in saved order.
    let rebuilt = common::wait_until(Duration::from_secs(15), || {
        let g = deck.snapshot_grid();
        g.contains("coder") && g.contains("reviewer")
    });
    assert!(
        rebuilt,
        "PRD #89 M2b.3: a daemon-empty launch with an orchestration snapshot on disk must \
         REBUILD the orchestration tab — the `coder` and `reviewer` role panes must appear as \
         deck cards — but they never did.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    // Saved display order: `coder` precedes `reviewer` in the role deck.
    let grid = deck.snapshot_grid();
    let coder_row = deck.find_in_grid("coder").map(|(_, r)| r);
    let reviewer_row = deck.find_in_grid("reviewer").map(|(_, r)| r);
    assert!(
        matches!((coder_row, reviewer_row), (Some(c), Some(rv)) if c < rv),
        "the rebuilt role panes must appear in the SAVED order (coder before reviewer), but \
         found coder at row {coder_row:?} and reviewer at row {reviewer_row:?}.\nFinal grid:\n{grid}"
    );

    // start_role_index honored + orchestrator_prompt replayed: the saved prompt
    // is delivered to the START (orchestrator) role pane and recorded. The
    // snapshot-fallback path replays it (M2b.3), unlike warm hydration
    // (session/restore/007), so this line proves both the prompt replay and that
    // the start role was identified from `start_role_index`.
    let record = project_dir.path().join("record-orchestrator.log");
    let replayed = common::wait_for_file_substr_count(
        &record,
        "Build the feature end to end",
        1,
        Duration::from_secs(15),
    );
    assert!(
        replayed,
        "PRD #89 M2b.3: the saved `orchestrator_prompt` must be replayed to the start \
         (orchestrator) role on the snapshot-fallback path, but it was never delivered \
         (no recorded line at {record:?}).\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Stage an orchestration `.dot-agent-deck.toml` whose orchestration is
/// named `renamed-orch`, then hand-stage a `session.toml` whose
/// `[panes.orchestration]` block still references the OLD `config_name =
/// "tdd-cycle"` (a config-drift: the orchestration was renamed/removed). Launch
/// against a fresh (empty) daemon with NO flag. The restore must NOT build a
/// half-broken orchestration tab: the saved pane falls back to a PLAIN dashboard
/// card (its saved name `orchestrator`, no `coder`/`reviewer` role panes), and a
/// clear `session_warnings` message NAMING the missing orchestration
/// (`tdd-cycle`) is surfaced — flushed to stderr at teardown, so we detach-quit
/// and scan the byte stream. RED today: there is no snapshot-fallback restore
/// branch, so no drift is detected and no warning is ever emitted.
#[spec("session/restore/009")]
#[test]
fn restore_009_orchestration_config_drift_warns_and_falls_back_to_plain_pane() {
    let project_dir = common::race_safe_tempdir();
    // The project config exists, but the orchestration was renamed — so the
    // snapshot's `config_name = "tdd-cycle"` no longer resolves.
    write_orchestration_config(
        project_dir.path(),
        "renamed-orch",
        &[("orchestrator", "sleep 600"), ("coder", "sleep 600")],
        0,
    );

    let session_dir = common::race_safe_tempdir();
    let session_file = session_dir.path().join("session.toml");
    stage_orchestration_snapshot(
        &session_file,
        project_dir.path(),
        "orchestrator",
        "sleep 600",
        &["orchestrator", "coder", "reviewer"],
        0,
        "Build the feature end to end",
        "tdd-cycle", // missing now — the config was renamed to `renamed-orch`
        project_dir.path(),
        &[0, 1],
    );

    let deck = TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            session_file.to_str().expect("session path is UTF-8"),
        )
        .launch_with_fixture("minimal");

    // Fallback: the saved orchestrator pane returns as a PLAIN dashboard card
    // (its saved name), never an orchestration tab.
    let fell_back = common::wait_until(Duration::from_secs(10), || {
        deck.snapshot_grid().contains("orchestrator")
    });
    assert!(
        fell_back,
        "PRD #89 M2b.3 drift: a snapshot whose orchestration no longer resolves must restore \
         the saved pane as a PLAIN dashboard card (`orchestrator`), but it never appeared.\n\
         Final grid:\n{}",
        deck.snapshot_grid()
    );

    // It must be a PLAIN pane, NOT a half-broken orchestration tab: the other
    // roles must not have been spawned.
    let grid = deck.snapshot_grid();
    assert!(
        !grid.contains("reviewer"),
        "config drift must fall back to a plain pane, never a half-broken orchestration tab — \
         but a `reviewer` role pane was rebuilt.\nFinal grid:\n{grid}"
    );

    // The drift must surface a clear warning NAMING the missing orchestration.
    // `session_warnings` are flushed to stderr at teardown, so detach-quit and
    // scan the cumulative byte stream. RED today: no drift branch → no warning.
    //
    // The restored pane auto-focuses (PaneInput), where Ctrl+C is forwarded to
    // the pane; detach to Normal mode first so Ctrl+C reaches the global quit.
    deck.send_keys(b"\x04"); // Ctrl+D → detach to Normal mode
    deck.wait_for_absence("[Detach Ctrl+D]"); // pane no longer focused
    deck.send_keys(b"\x03"); // Ctrl+C → quit-confirm modal
    deck.wait_for_string("Quit dot-agent-deck?");
    deck.send_keys(b"\r"); // Enter → Detach (default) → clean teardown + flush
    deck.wait_for_stream_string("tdd-cycle");
}
