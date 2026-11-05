#![cfg(all(feature = "e2e", feature = "e2e-live", target_os = "linux"))]

//! M59 on Codex, measured (PRD #1666 part 3, U4): the editor helper's armed
//! alarm against a real Codex, and a real Codex's long human edit through
//! the helper's unarmed path. The Codex twin of
//! `tests/e2e_claude_helper_alarm.rs` (`inbox/probe/090`).
//!
//! The harness is the stand-in M59 names: no daemon. The test creates an
//! instance whose helper is a copy of this build's deck binary, one Codex
//! generation in it, and a socket the test process owns (the instance's
//! recorded daemon) that accepts connections and never answers. The pinned
//! native Codex runs on a PTY of its own through
//! `tests/support/codex_helper_alarm_measure.py`, with no credential (the
//! sandbox, dummy key and loopback Responses server of `inbox/probe/092`),
//! `VISUAL`/`EDITOR` naming the helper and the user's editor (the helper
//! restores it) a stand-in that sleeps, writes a sentinel and exits. Its
//! `editor` directory is created 0700 before it starts, as the deck does
//! (option P). Once Codex has started, the test writes its identity into the
//! generation, so the helper classifies Codex as the agent itself (`Owned`).
//!
//! The helper's own end is read from outside: the recorder watches its
//! `/proc` entry at 1 ms, takes the wait status from the zombie's
//! `exit_code` when it sees the zombie before Codex reaps it, and reads the
//! helper's e2e trace for when it started and which branch it took. The
//! capture root is printed and kept under `/var/tmp`.

mod common;

use dot_agent_deck::composer_probe::classifier::{FileId, Identity};
use dot_agent_deck::composer_probe::install::ProbeAgent;
use dot_agent_deck::composer_probe::instance::{DaemonId, Instance};
use serde_json::Value;
use spec::spec;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::time::Duration;

const GENERATION: &str = "3";
const DEFAULT_CODEX: &str =
    "/var/tmp/dad-cxhe-codex-0.164.0/x64/package/vendor/x86_64-unknown-linux-musl/bin/codex";

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

fn m59(cell: &str, armed: bool) -> Option<(PathBuf, Value)> {
    let codex = std::env::var_os("DOT_AGENT_DECK_CODEX_0_164_0")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CODEX));
    if !codex.is_file() || !std::path::Path::new("/usr/bin/bwrap").is_file() {
        common::_skip_if_err(Err::<(), _>(format!(
            "the pinned Codex 0.164.0 (DOT_AGENT_DECK_CODEX_0_164_0, else {DEFAULT_CODEX}) and \
             /usr/bin/bwrap are required"
        )));
        return None;
    }
    let parent = tempfile::Builder::new()
        .prefix("dad-codex-m59-")
        .tempdir_in("/var/tmp")
        .unwrap()
        .keep();
    let root = parent.join("run");
    // The instance's ancestors must be trusted (not world-writable), which
    // `/var/tmp` is not: it lives in the harness's private temp root.
    let private = common::race_safe_tempdir();
    let instance = Instance::create(
        &private.path().join("editor"),
        DaemonId::this_process().unwrap(),
        &mut File::open(common::contained_deck_binary()).unwrap(),
    )
    .unwrap();
    let mut generation = instance
        .generation(GENERATION, Some(ProbeAgent::Codex), None)
        .unwrap();
    generation
        .arm()
        .publisher()
        .publish_native(u32::from(armed), 17, 23);
    let socket = private.path().join("hook.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let manifest = env!("CARGO_MANIFEST_DIR");
    let recorder = std::process::Command::new("/usr/bin/python3")
        .arg("-I")
        .arg(format!(
            "{manifest}/tests/support/codex_helper_alarm_measure.py"
        ))
        .arg(cell)
        .arg(&root)
        .arg(&codex)
        .arg(instance.helper())
        .arg(&socket)
        .arg(GENERATION)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let recorder = std::cell::RefCell::new(recorder);
    let ended = || recorder.borrow_mut().try_wait().unwrap();
    let out = root.join("capture");
    assert!(
        common::wait_until(Duration::from_secs(60), || out.join("codex.pid").exists()
            || ended().is_some()),
        "the recorder did not start Codex"
    );
    let pid: u32 = std::fs::read_to_string(out.join("codex.pid"))
        .unwrap_or_else(|_| panic!("the recorder ended early: {}", root.display()))
        .trim()
        .parse()
        .unwrap();
    instance
        .write_identity(GENERATION, &identity_of(pid))
        .unwrap();
    std::fs::write(out.join("identity.ok"), b"").unwrap();
    let status = std::cell::RefCell::new(None);
    assert!(
        common::wait_until(Duration::from_secs(480), || {
            *status.borrow_mut() = ended();
            status.borrow().is_some()
        }),
        "the recorder did not finish: {}",
        root.display()
    );
    let status = status.into_inner().unwrap();
    assert!(status.success(), "recorder: {status}; {}", root.display());
    let result: Value =
        serde_json::from_slice(&std::fs::read(out.join("result.json")).unwrap()).unwrap();
    eprintln!("M59 Codex {cell} evidence: {}", out.display());
    drop(generation);
    assert_eq!(result["release"], "0.164.0");
    assert!(
        result["error"].is_null(),
        "M59 {cell}: {}; {}",
        result["error"],
        out.display()
    );
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

fn events(trial: &Value) -> Vec<&str> {
    trial["trace"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["event"].as_str())
        .collect()
}

fn summary(trial: &Value) -> String {
    format!(
        "trial {}: branch={:?} life(from branch)={} s, zombie={} notice={} codex_error={:?} after_end={} last_trace={:?} editors={} composer_after={:?}",
        trial["trial"],
        branch(trial),
        trial["helper_life_from_branch_s"],
        trial["zombie_status"],
        trial["notice_in_output"],
        trial["codex_error_line"],
        trial["codex_error_after_end"],
        events(trial).last(),
        trial["editor"].as_array().map_or(0, Vec::len) / 2,
        trial["composer_after"]
    )
}

fn draft_kept(trial: &Value) -> bool {
    trial["composer_after"].as_str().unwrap_or_default() == trial["draft"].as_str().unwrap()
}

/// Scenario: Run the pinned Codex 0.164.0 with the deck's editor helper as its editor, the arm present and the helper stalled at its first ready poll, and press Ctrl+G on a draft ten times. Each time the helper must end about five seconds after it chose the armed branch, by its alarm, with no editor run, and Codex must keep the draft.
#[spec("inbox/probe/095")]
#[test]
fn probe_095_live_codex_164_armed_helper_alarm_ends_a_stalled_exchange() {
    let Some((out, result)) = m59("armed-stall", true) else {
        return;
    };
    assert_eq!(trials(&result).len(), 10);
    let mut seen = 0;
    for trial in trials(&result) {
        eprintln!("M59a {}", summary(trial));
        assert_eq!(trial["helper_parent_is_codex"], true, "{}", out.display());
        assert!(
            branch(trial).starts_with("branch Armed"),
            "{}",
            out.display()
        );
        assert_eq!(trial["editor"].as_array().unwrap().len(), 0, "no editor");
        let life = trial["helper_life_from_branch_s"].as_f64().unwrap();
        assert!(
            (4.5..=5.5).contains(&life),
            "trial {}: {life} s",
            trial["trial"]
        );
        if !trial["zombie_status"].is_null() {
            assert_eq!(trial["zombie_status"]["signal"], 14, "SIGALRM");
        }
        let trace = events(trial);
        assert_eq!(trace.last().copied(), Some("pause ready"), "{trace:?}");
        assert!(
            !trace
                .iter()
                .any(|e| e.starts_with("armed ") || e.starts_with("exit ")),
            "an orderly end was traced: {trace:?}"
        );
        // Codex's own line, when it names the end, must name the alarm.
        let line = trial["codex_error_line"].as_str().unwrap_or_default();
        let alarm = line.contains("SIGALRM") || line.contains("signal: 14");
        assert!(
            trial["zombie_status"]["signal"] == 14
                || (alarm && trial["codex_error_after_end"] == true),
            "trial {}: SIGALRM seen neither in the zombie nor in Codex's line {line:?}",
            trial["trial"]
        );
        seen += usize::from(alarm);
        assert!(
            draft_kept(trial),
            "trial {}: {}",
            trial["trial"],
            trial["screen_after"]
        );
    }
    eprintln!("M59a Codex's own line named the alarm in {seen}/10");
}

/// Scenario: Run the pinned Codex 0.164.0 with the deck's editor helper as its editor and the arm present, with no stall, against the socket that never answers, and press Ctrl+G on a draft ten times. The helper's exchange runs out first; each time no editor runs, the helper's notice reaches the terminal, it ends before its alarm with status 1 (Codex's failed-editor path, row T of the helper's return table), Codex draws its `Failed to open editor` line for that status, and keeps the draft.
#[spec("inbox/probe/095")]
#[test]
fn probe_095_live_codex_164_armed_helper_unanswered_exchange() {
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
        let trace = events(trial);
        let last = trace.last().copied().unwrap_or_default();
        assert!(
            last.starts_with("armed ") && last.contains("ArmedExit") && last.contains("status: 1"),
            "trial {}: {trace:?}",
            trial["trial"]
        );
        assert_eq!(trial["notice_in_output"], true, "trial {}", trial["trial"]);
        if !trial["zombie_status"].is_null() {
            assert_eq!(trial["zombie_status"]["code"], 1);
        }
        // Row T exits 1 on Codex, so Codex takes its failed-editor path and
        // reimports nothing.
        assert_eq!(
            trial["codex_error_after_end"], true,
            "trial {}",
            trial["trial"]
        );
        assert_eq!(
            trial["codex_error_line"].as_str(),
            Some("■ Failed to open editor: editor exited with status exit status: 1"),
            "trial {}",
            trial["trial"]
        );
        assert!(
            draft_kept(trial),
            "trial {}: {}",
            trial["trial"],
            trial["screen_after"]
        );
    }
}

/// Scenario: Run the pinned Codex 0.164.0 with the deck's editor helper as its editor and no arm, the user's editor a stand-in that sleeps eight seconds, writes a sentinel and exits, and press Ctrl+G on a draft ten times, then once more with the editor exiting 37. Each time the helper takes the unarmed branch, outlives its would-be alarm while the editor runs, and ends with the editor's status; Codex shows the sentinel after each zero and keeps the draft after the 37.
#[spec("inbox/probe/095")]
#[test]
fn probe_095_live_codex_164_unarmed_helper_long_edit() {
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
        assert_eq!(trial["editor"].as_array().unwrap().len(), 2, "trial {n}");
        let rc = trial["editor_rc_planned"].as_i64().unwrap();
        let exit = events(trial)
            .into_iter()
            .find(|e| e.starts_with("exit "))
            .unwrap_or_default()
            .to_string();
        assert_eq!(exit, format!("exit Status({rc})"), "trial {n}");
        if !trial["zombie_status"].is_null() {
            assert_eq!(trial["zombie_status"]["code"], rc, "trial {n}");
        }
        let life = trial["helper_life_from_branch_s"].as_f64().unwrap();
        assert!(life > 8.0, "trial {n}: the helper lived only {life} s");
        if rc == 0 {
            assert_eq!(
                trial["composer_after"].as_str(),
                Some(format!("M59_EDITED_{n}").as_str()),
                "trial {n}: {}",
                trial["screen_after"]
            );
        } else {
            assert!(draft_kept(trial), "trial {n}: {}", trial["screen_after"]);
        }
    }
}
