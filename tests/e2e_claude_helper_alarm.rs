#![cfg(all(feature = "e2e", feature = "e2e-live", target_os = "linux"))]

//! M59, measured: the editor helper's armed alarm against a real agent, and
//! a real agent's long human edit through the helper's unarmed path (PRD
//! #1666 part 3, core §5.5 and §15).
//!
//! The harness is the stand-in M59 names: no daemon. The test creates an
//! instance whose helper is a copy of this build's deck binary, one Claude
//! generation in it, and a socket the test process owns (the instance's
//! recorded daemon) that accepts connections and never answers. A real
//! pinned Claude Code runs on a PTY of its own inside a deck pane of the TUI
//! harness, with its own HOME, `VISUAL`/`EDITOR` naming the helper and the
//! user's editor (the helper restores it) a stand-in that sleeps, writes a
//! sentinel and exits. Once Claude has started, the test writes its identity
//! into the generation, so the helper classifies Claude as the agent itself
//! (`Owned`), as it would under a real deck.
//!
//! The helper's own end is read from outside: it is Claude's child, so the
//! recorder watches its `/proc` entry at 1 ms, takes the wait status from
//! the zombie's `exit_code` when it sees the zombie before Claude reaps it,
//! and the helper's e2e trace for when it started and which branch it took.
//!
//! The capture root is printed and kept under `/var/tmp`.

mod common;

use dot_agent_deck::composer_probe::classifier::{FileId, Identity};
use dot_agent_deck::composer_probe::install::ProbeAgent;
use dot_agent_deck::composer_probe::instance::{DaemonId, Instance};
use serde_json::Value;
use spec::spec;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::time::Duration;

const RELEASE: &str = "2.1.317";
const GENERATION: &str = "3";

fn identity_of(pid: u32) -> Identity {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).unwrap();
    let meta = std::fs::metadata(format!("/proc/{pid}/exe")).unwrap();
    let root = exe.parent().unwrap().to_path_buf();
    let root_meta = std::fs::metadata(&root).unwrap();
    Identity {
        pid,
        start: dot_agent_deck::platform::proc::process_start_of(pid).unwrap(),
        exe: FileId {
            dev: meta.dev(),
            ino: meta.ino(),
        },
        install_root: root,
        install_root_id: FileId {
            dev: root_meta.dev(),
            ino: root_meta.ino(),
        },
        basename: exe.file_name().unwrap().to_os_string(),
    }
}

fn m59(cell: &str, armed: bool) -> Option<(std::path::PathBuf, Value)> {
    let key = format!("DOT_AGENT_DECK_CLAUDE_MENU_{}", RELEASE.replace('.', "_"));
    let Ok(binary) = std::env::var(&key) else {
        common::_skip_if_err(Err(format!("set {key} to the pinned executable")));
        return None;
    };
    let bin = tempfile::Builder::new()
        .prefix("dad-m59-bin-")
        .tempdir_in("/var/tmp")
        .unwrap();
    std::os::unix::fs::symlink(&binary, bin.path().join("claude")).unwrap();
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap()
    );
    let root = tempfile::Builder::new()
        .prefix("dad-m59-")
        .tempdir_in("/var/tmp")
        .unwrap()
        .keep();
    // The instance's ancestors must be trusted (not world-writable), which
    // `/var/tmp` is not: it lives in the harness's private temp root.
    let private = common::race_safe_tempdir();
    let instance = Instance::create(
        &private.path().join("editor"),
        DaemonId::this_process().unwrap(),
        // The build under test, through the harness's link outside the
        // checkout (`orchestration/dispatch/007`).
        &mut File::open(common::contained_deck_binary()).unwrap(),
    )
    .unwrap();
    let mut generation = instance
        .generation(GENERATION, Some(ProbeAgent::Claude), None)
        .unwrap();
    generation
        .arm()
        .publisher()
        .publish_native(u32::from(armed), 17, 23);
    // The instance's recorded daemon (this process) owns the socket, which
    // accepts and never answers.
    let socket = private.path().join("hook.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let out = root.join("capture");
    let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"));
    let script = root.join("claude_helper_alarm_measure.py");
    std::fs::write(
        &script,
        include_str!("support/claude_helper_alarm_measure.py"),
    )
    .unwrap();
    let editor = root.join("claude_editor_measure.py");
    std::fs::write(&editor, include_str!("support/claude_editor_measure.py")).unwrap();
    let command = format!(
        "exec python3 {} {cell} {} {} {} {GENERATION} {}",
        quote(&script),
        quote(&out),
        quote(instance.helper()),
        quote(&socket),
        quote(&editor),
    );
    let deck = common::TuiDeck::builder()
        .with_env("PATH", path)
        .with_env(
            "PYTHONPATH",
            std::env::var("PYTHONPATH").unwrap_or_default(),
        )
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "600")
        .with_pty_size(220, 60)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_continue_session("m59", command)
        .launch_with_fixture("minimal");
    assert!(
        common::wait_until(Duration::from_secs(60), || out.join("claude.pid").exists()
            || root.join("launcher-error.json").exists()),
        "the recorder did not start Claude; {}",
        deck.snapshot_grid()
    );
    assert!(
        !root.join("launcher-error.json").exists(),
        "recorder setup failed: {}",
        std::fs::read_to_string(root.join("launcher-error.json")).unwrap_or_default()
    );
    let pid: u32 = std::fs::read_to_string(out.join("claude.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    instance
        .write_identity(GENERATION, &identity_of(pid))
        .unwrap();
    std::fs::write(out.join("identity.ok"), b"").unwrap();
    assert!(
        common::wait_until(Duration::from_secs(480), || out
            .join("result.json")
            .exists()),
        "M59 {cell} did not finish; {}\n{}",
        out.display(),
        deck.snapshot_grid()
    );
    let result: Value =
        serde_json::from_slice(&std::fs::read(out.join("result.json")).unwrap()).unwrap();
    eprintln!("M59 {cell} evidence: {}", out.display());
    drop(deck);
    drop(generation);
    assert_eq!(result["release"], RELEASE);
    assert!(
        result["error"].is_null(),
        "M59 {cell}: {}; {}",
        result["error"],
        out.display()
    );
    let hooks = std::fs::read_to_string(out.join("hook.jsonl")).unwrap_or_default();
    assert_eq!(hooks.lines().count(), 0, "nothing may be submitted");
    Some((out, result))
}

fn trials(result: &Value) -> &Vec<Value> {
    result["trials"].as_array().unwrap()
}

fn branch(trial: &Value) -> String {
    trial["trace"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|e| e["event"].as_str().filter(|e| e.starts_with("branch ")))
        .unwrap_or("none")
        .to_string()
}

fn summary(trial: &Value) -> String {
    format!(
        "trial {}: branch={:?} life(from branch)={} s, zombie={} notice={} editors={} composer_after={:?}",
        trial["trial"],
        branch(trial),
        trial["helper_life_from_branch_s"],
        trial["zombie_status"],
        trial["notice_in_output"],
        trial["editor"].as_array().map_or(0, Vec::len) / 2,
        trial["composer_after"]
    )
}

/// Scenario: Run pinned Claude Code 2.1.317 with the deck's editor helper as its editor, the arm present and the helper stalled at its first ready poll, and press Ctrl+G on a draft ten times. Each time the helper must end about five seconds after it chose the armed branch, by its alarm, with no editor run, and Claude must keep the draft.
#[spec("inbox/probe/090")]
#[test]
fn probe_090_live_claude_317_armed_helper_alarm_ends_a_stalled_exchange() {
    skip_unless!(common::check_claude_available());
    let Some((out, result)) = m59("armed-stall", true) else {
        return;
    };
    assert_eq!(trials(&result).len(), 10);
    for trial in trials(&result) {
        eprintln!("M59a {}", summary(trial));
        assert!(
            branch(trial).starts_with("branch Armed"),
            "{}",
            out.display()
        );
        assert_eq!(trial["editor"].as_array().unwrap().len(), 0, "no editor");
        let life = trial["helper_life_from_branch_s"].as_f64().unwrap();
        assert!(
            (4.5..=5.5).contains(&life),
            "trial {}: the helper lived {life} s after its armed branch",
            trial["trial"]
        );
        if !trial["zombie_status"].is_null() {
            assert_eq!(trial["zombie_status"]["signal"], 14, "SIGALRM");
        }
        assert!(
            trial["composer_after"]
                .as_str()
                .unwrap_or_default()
                .contains(trial["draft"].as_str().unwrap()),
            "trial {}: the draft must remain; {}",
            trial["trial"],
            trial["screen_after"]
        );
    }
}

/// Scenario: Run pinned Claude Code 2.1.317 with the deck's editor helper as its editor and the arm present, with no stall, against the socket that never answers, and press Ctrl+G on a draft ten times. The helper's exchange runs out first; each time no editor runs, the helper's notice is written to the terminal, it ends before its alarm, and Claude keeps the draft.
#[spec("inbox/probe/090")]
#[test]
fn probe_090_live_claude_317_armed_helper_unanswered_exchange() {
    skip_unless!(common::check_claude_available());
    let Some((out, result)) = m59("armed-timeout", true) else {
        return;
    };
    assert_eq!(trials(&result).len(), 10);
    for trial in trials(&result) {
        eprintln!("M59t {}", summary(trial));
        assert!(
            branch(trial).starts_with("branch Armed"),
            "{}",
            out.display()
        );
        assert_eq!(trial["editor"].as_array().unwrap().len(), 0, "no editor");
        let life = trial["helper_life_from_branch_s"].as_f64().unwrap();
        assert!(life < 4.5, "trial {}: lived {life} s", trial["trial"]);
        assert!(
            trial["composer_after"]
                .as_str()
                .unwrap_or_default()
                .contains(trial["draft"].as_str().unwrap()),
            "trial {}: the draft must remain; {}",
            trial["trial"],
            trial["screen_after"]
        );
    }
}

/// Scenario: Run pinned Claude Code 2.1.317 with the deck's editor helper as its editor and no arm, the user's editor a stand-in that sleeps eight seconds, writes a sentinel and exits, and press Ctrl+G on a draft ten times, then once more with the editor exiting 37. Each time the helper must take the unarmed branch, outlive its would-be alarm while the editor runs, and end with the editor's status; Claude must show the sentinel after the editor exits zero.
#[spec("inbox/probe/090")]
#[test]
fn probe_090_live_claude_317_unarmed_helper_long_edit() {
    skip_unless!(common::check_claude_available());
    let Some((out, result)) = m59("unarmed-long", false) else {
        return;
    };
    assert_eq!(trials(&result).len(), 11);
    for trial in trials(&result) {
        eprintln!("M59b {}", summary(trial));
        let n = trial["trial"].as_u64().unwrap();
        assert!(
            branch(trial).starts_with("branch Unarmed"),
            "trial {n}: {}; {}",
            branch(trial),
            out.display()
        );
        let editor = trial["editor"].as_array().unwrap();
        assert_eq!(editor.len(), 2, "trial {n}: one editor start and exit");
        let rc = trial["editor_rc_planned"].as_i64().unwrap();
        let exit = trial["trace"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|e| e["event"].as_str().filter(|e| e.starts_with("exit ")))
            .unwrap_or_default()
            .to_string();
        assert_eq!(exit, format!("exit Status({rc})"), "trial {n}");
        if !trial["zombie_status"].is_null() {
            assert_eq!(trial["zombie_status"]["code"], rc, "trial {n}");
        }
        let life = trial["helper_life_from_branch_s"].as_f64().unwrap();
        assert!(life > 8.0, "trial {n}: the helper lived only {life} s");
        if rc == 0 {
            assert!(
                trial["composer_after"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(&format!("M59_EDITED_{n}")),
                "trial {n}: Claude must reimport the edit; {}",
                trial["screen_after"]
            );
        }
    }
}
