//! SC-10's failing-first TUI contract. The proposed, test-only I/O seam and
//! provisional keys/words are documented in fixtures/release-check/tui.md.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::daemon_protocol::AttachResponse;
use dot_agent_deck::event::{AgentEvent, AgentType, EventType};
use dot_agent_deck::inbox_hub::InboxSummary;
use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::state::{AppState, SessionStatus};
use dot_agent_deck::ui::render_release_check_key_sequence_to_buffers;
use ratatui::buffer::Buffer;
use serde_json::{Value, json};
use spec::spec;

const SESSION: &str = "check-session";

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn esc() -> KeyEvent {
    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
}

fn enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
}

fn state(cause: &str) -> AppState {
    let mut state = AppState::default();
    state.register_pane("check-pane".into());
    state.apply_event(AgentEvent {
        session_id: SESSION.into(),
        agent_type: AgentType::ClaudeCode,
        event_type: EventType::SessionStart,
        tool_name: None,
        tool_detail: None,
        cwd: Some("/project".into()),
        timestamp: chrono::DateTime::from_timestamp(1_767_225_600, 0).unwrap(),
        user_prompt: None,
        metadata: HashMap::new(),
        pane_id: Some("check-pane".into()),
        agent_id: Some("check-agent".into()),
        agent_version: Some("2.1.313".into()),
        schema_version: None,
        live_target: None,
        background: None,
        background_revision: None,
    });
    state.sessions.get_mut(SESSION).unwrap().status = SessionStatus::Idle;
    let summary: InboxSummary = serde_json::from_value(json!({
        "queued":1,"fetched":0,"undeliverable":0,"wake":"doorbell",
        "wake_hold":"cannot_type", "hold_detail":{
            "cause":format!("release_check_{cause}"),
            "agent":if cause == "no_reference" { "codex" } else { "claude" },
            "release":"2.1.313","fact":"layout","reason":"no_process_containment"
        }
    }))
    .unwrap();
    state.apply_inbox_summary("check-pane", "check-agent", &summary);
    state
}

fn row(release: &str, identity: &str, result: Value, progress: &str) -> Value {
    let mut row = json!({"agent":"claude","release":release,"identity":identity,
        "state":result,"progress":progress,"queued_at_ms":1767225600000_i64});
    if progress != "complete" {
        row["queue_position"] = json!(2);
    }
    row
}

fn rows(result: Value, progress: &str) -> Vec<Value> {
    vec![row("2.1.313", "0123456789abcdef", result, progress)]
}

fn snapshot(rows: &[Value]) -> AttachResponse {
    serde_json::from_value(json!({"ok":true,"release_checks":rows})).unwrap()
}

/// Remove only padding, empty rows and the rectangular frame. Keep the
/// renderer's row order, cursor, labels, wrapping and buttons as the oracle.
fn panel(buffer: &Buffer) -> String {
    let area = buffer.area();
    let lines: Vec<String> = (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect();
    let top = lines
        .iter()
        .position(|line| line.contains("Release checks"))
        .unwrap_or_else(|| panic!("Details did not open:\n{}", lines.join("\n")));
    let left = lines[top]
        .chars()
        .position(|c| c == '┌' || c == '╭')
        .expect("Details has its own rectangular frame");
    let width = lines[top]
        .chars()
        .skip(left)
        .position(|c| c == '┐' || c == '╮')
        .expect("Details has a right border")
        + 1;
    let mut content = vec!["Release checks".to_owned()];
    for line in &lines[top + 1..] {
        let row: String = line.chars().skip(left).take(width).collect();
        if row.starts_with(['└', '╰']) {
            break;
        }
        let row = row.trim_matches(|c: char| c == '│' || c.is_whitespace());
        if !row.is_empty() {
            content.push(row.to_owned());
        }
    }
    content.join("\n")
}

fn draw(cause: &str, records: &[Value], keys: &[KeyEvent]) -> Vec<Buffer> {
    render_release_check_key_sequence_to_buffers(
        &KeybindingConfig::default(),
        &state(cause),
        SESSION,
        &snapshot(records),
        "auto",
        keys,
        (120, 32),
        &mut |request: Value| -> Result<Value, String> {
            panic!("opening, selecting or dismissing Details must not execute {request}")
        },
    )
}

/// Scenario: Open Details from an agent held while a check is queued or running, and from each completed journal state, including an unavailable check. Snapshot the selected release, identity, progress, queue position, the explanation and the available keys; also open no-reference and off holds that have no journal row.
#[spec("inbox/release-check/004")]
#[test]
fn release_check_004_details_states_and_controls() {
    for (name, cause, result, progress) in [
        ("queued", "running", Value::Null, "queued"),
        ("running", "running", Value::Null, "running"),
        ("passed", "running", json!("passed"), "complete"),
        ("differs", "differs", json!("differs"), "complete"),
        (
            "inconclusive",
            "inconclusive",
            json!("inconclusive"),
            "complete",
        ),
        (
            "unavailable",
            "unavailable",
            json!("unavailable"),
            "complete",
        ),
        ("revoked", "tripped", json!("revoked"), "complete"),
        ("forgotten", "off", json!("forgotten"), "complete"),
    ] {
        let frames = draw(cause, &rows(result, progress), &[key('v')]);
        assert_eq!(frames.len(), 2, "initial frame and frame after Details key");
        insta::assert_snapshot!(format!("details_{name}"), panel(frames.last().unwrap()));
    }
    for cause in ["no_reference", "off"] {
        let frames = draw(cause, &[], &[key('v')]);
        insta::assert_snapshot!(format!("details_{cause}"), panel(frames.last().unwrap()));
    }
}

/// Scenario: Open Details with three records, two of them for the same release but different executable identities. Move down and back up, snapshot which identity is selected, and close with Escape; browsing must execute no action and closing must restore the original card.
#[spec("inbox/release-check/004")]
#[test]
fn release_check_004_details_selects_a_record_and_escape_restores_the_card() {
    let records = vec![
        row("2.1.313", "0123456789abcdef", json!("passed"), "complete"),
        row("2.1.313", "fedcba9876543210", json!("revoked"), "complete"),
        row(
            "2.1.310",
            "aaaaaaaaaaaaaaaa",
            json!("forgotten"),
            "complete",
        ),
    ];
    let frames = draw(
        "tripped",
        &records,
        &[key('v'), key('j'), key('j'), key('k'), esc()],
    );
    assert_eq!(frames.len(), 6);
    insta::assert_snapshot!("details_selected_identity", panel(&frames[4]));
    assert_eq!(frames.first(), frames.last(), "Escape restores the card");
}

/// A deterministic stand-in for the external command boundary, not the key
/// reducer. Its replies are fed back through the real TUI response handling.
struct Backend {
    records: Vec<Value>,
    calls: Vec<Value>,
    mode: String,
    exported_to: Option<String>,
}

impl Backend {
    fn new(result: Value, progress: &str) -> Self {
        Self {
            records: rows(result, progress),
            calls: vec![],
            mode: "auto".into(),
            exported_to: None,
        }
    }

    fn execute(&mut self, request: Value) -> Result<Value, String> {
        self.calls.push(request.clone());
        match request["op"].as_str().expect("command operation") {
            "release-check-run" => {
                assert_eq!(request["agent"], "claude");
                self.records[0]["progress"] = json!("queued");
            }
            "release-check-cancel" => {
                self.records[0]["progress"] = json!("complete");
            }
            "release-check-forget" => {
                assert_eq!(request["agent"], "claude");
                assert_eq!(request["release"], self.records[0]["release"]);
                self.records[0]["state"] = json!("forgotten");
            }
            "release-check-export" => {
                assert_eq!(request["agent"], "claude");
                assert_eq!(request["release"], self.records[0]["release"]);
                self.exported_to =
                    Some(request["dir"].as_str().expect("chosen destination").into());
            }
            "release-check-mode" => {
                self.mode = request["mode"].as_str().expect("chosen mode").into();
            }
            op => panic!("unexpected external command {op}"),
        }
        Ok(
            json!({"ok":true,"release_checks":self.records,"mode":self.mode,
            "exported_to":self.exported_to}),
        )
    }

    fn run(&mut self, cause: &str, keys: &[KeyEvent]) -> Vec<Buffer> {
        let snapshot = snapshot(&self.records);
        let mode = self.mode.clone();
        render_release_check_key_sequence_to_buffers(
            &KeybindingConfig::default(),
            &state(cause),
            SESSION,
            &snapshot,
            &mode,
            keys,
            (120, 32),
            &mut |request: Value| self.execute(request),
        )
    }
}

/// Scenario: Re-check from the hold line and from the agent's detail view, including approval in ask mode, then open Details and cancel a running check. The accepted operation must change the visible progress or display the cancellation answer, with one command per press.
#[spec("inbox/release-check/004")]
#[test]
fn release_check_004_recheck_approve_and_cancel_have_visible_results() {
    for (name, keys, mode) in [
        ("recheck_hold", vec![key('R'), key('v')], "auto"),
        ("recheck_agent", vec![key('p'), key('R'), key('v')], "auto"),
        ("approve_ask", vec![key('p'), key('R'), key('v')], "ask"),
    ] {
        let mut backend = Backend::new(json!("inconclusive"), "complete");
        backend.mode = mode.into();
        let frames = backend.run("inconclusive", &keys);
        assert_eq!(backend.calls.len(), 1, "one accepted Re-check/approval");
        assert_eq!(backend.records[0]["progress"], "queued");
        insta::assert_snapshot!(name, panel(frames.last().unwrap()));
    }
    let mut backend = Backend::new(Value::Null, "running");
    let frames = backend.run("running", &[key('v'), key('c')]);
    assert_eq!(backend.calls.len(), 1, "one accepted cancellation");
    assert_eq!(backend.records[0]["progress"], "complete");
    insta::assert_snapshot!("cancelled", panel(frames.last().unwrap()));
}

/// Scenario: Select the second of two releases in Details and forget it. The first record stays passed, the selected record becomes forgotten and the answer remains visible with that release and identity.
#[spec("inbox/release-check/004")]
#[test]
fn release_check_004_forget_acts_on_the_selected_release() {
    let mut backend = Backend::new(json!("differs"), "complete");
    backend.records.insert(
        0,
        row("2.1.310", "aaaaaaaaaaaaaaaa", json!("passed"), "complete"),
    );
    let initial = snapshot(&backend.records);
    let mut calls = 0;
    let frames = render_release_check_key_sequence_to_buffers(
        &KeybindingConfig::default(),
        &state("differs"),
        SESSION,
        &initial,
        "auto",
        &[key('v'), key('j'), key('f')],
        (120, 32),
        &mut |request: Value| {
            calls += 1;
            assert_eq!(
                request,
                json!({"op":"release-check-forget","agent":"claude","release":"2.1.313"})
            );
            backend.records[1]["state"] = json!("forgotten");
            Ok(json!({"ok":true,"release_checks":backend.records}))
        },
    );
    assert_eq!(calls, 1);
    assert_eq!(backend.records[0]["state"], "passed");
    insta::assert_snapshot!("forgot_selected", panel(frames.last().unwrap()));
}

/// Scenario: Press Export in Details and inspect the destination prompt before typing a directory containing spaces and confirming. Escape must dismiss an unsubmitted prompt without exporting; confirmation must submit the chosen directory once and leave its answer visible.
#[spec("inbox/release-check/004")]
#[test]
fn release_check_004_export_asks_for_a_directory_and_can_be_cancelled() {
    let mut backend = Backend::new(json!("passed"), "complete");
    let frames = backend.run("running", &[key('v'), key('e')]);
    assert!(
        backend.calls.is_empty(),
        "opening a prompt writes no export"
    );
    insta::assert_snapshot!("export_prompt", panel(frames.last().unwrap()));
    let cancelled = backend.run("running", &[key('v'), key('e'), esc()]);
    assert!(backend.calls.is_empty(), "Escape writes no export");
    insta::assert_snapshot!("export_cancelled", panel(cancelled.last().unwrap()));
    let dir = "/sandbox/check exports";
    let mut keys = vec![key('v'), key('e')];
    keys.extend(dir.chars().map(key));
    keys.push(enter());
    let frames = backend.run("running", &keys);
    assert_eq!(backend.calls.len(), 1);
    assert_eq!(backend.exported_to.as_deref(), Some(dir));
    insta::assert_snapshot!("exported", panel(frames.last().unwrap()));
}

/// Scenario: In the agent's detail settings, choose ask, off and auto with the mode key. Each accepted setting is shown by name; a refused Re-check shows its error and leaves the held record unchanged, rather than claiming a check started.
#[spec("inbox/release-check/004")]
#[test]
fn release_check_004_mode_settings_and_refusal_are_visible() {
    let mut backend = Backend::new(Value::Null, "running");
    for (name, keys) in [
        ("mode_ask", vec![key('p'), key('M')]),
        ("mode_off", vec![key('p'), key('M')]),
        ("mode_auto", vec![key('p'), key('M')]),
    ] {
        let frames = backend.run("running", &keys);
        insta::assert_snapshot!(name, agent_settings(frames.last().unwrap()));
    }
    assert_eq!(
        backend
            .calls
            .iter()
            .map(|c| c["mode"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["ask", "off", "auto"]
    );
    let initial = snapshot(&rows(json!("inconclusive"), "complete"));
    let frames = render_release_check_key_sequence_to_buffers(
        &KeybindingConfig::default(),
        &state("inconclusive"),
        SESSION,
        &initial,
        "auto",
        &[key('v'), key('R')],
        (120, 32),
        &mut |_: Value| -> Result<Value, String> {
            Err("This daemon cannot run release checks.".into())
        },
    );
    insta::assert_snapshot!("refused", panel(frames.last().unwrap()));
}

fn agent_settings(buffer: &Buffer) -> String {
    let area = buffer.area();
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .filter(|line| line.contains("Release checks:"))
        .map(|line| {
            line.trim_matches(|c: char| c == '│' || c.is_whitespace())
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
