//! SC-10's failing-first TUI contract. The proposed I/O seam and
//! provisional keys/words are documented in fixtures/release-check/tui.md.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::daemon_protocol::AttachResponse;
use dot_agent_deck::event::{AgentEvent, AgentType, EventType};
use dot_agent_deck::inbox_hub::InboxSummary;
use dot_agent_deck::keybindings::{ACTIONS, KeybindingConfig};
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
        agent_type: if cause == "no_reference" {
            AgentType::Codex
        } else {
            AgentType::ClaudeCode
        },
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
            "release":"2.1.313","fact":"layout",
            "reason":if cause == "unavailable" { "no_process_containment" } else { "launch" }
        }
    }))
    .unwrap();
    state.apply_inbox_summary("check-pane", "check-agent", &summary);
    state
}

fn row(release: &str, identity: &str, result: Value, progress: &str) -> Value {
    let mut row = json!({"agent":"claude","release":release,"identity":identity,
        "state":result,"progress":progress,"queued_at_ms":1767225600000_i64});
    match row["state"].as_str() {
        Some("differs" | "revoked") => row["fact"] = json!("layout"),
        Some("inconclusive") => row["reason"] = json!("launch"),
        Some("unavailable") => row["reason"] = json!("no_process_containment"),
        _ => {}
    }
    match progress {
        "running" => row["queue_position"] = json!(1),
        "queued" => row["queue_position"] = json!(2),
        _ => {}
    }
    row
}

fn rows(result: Value, progress: &str) -> Vec<Value> {
    vec![row("2.1.313", "0123456789abcdef", result, progress)]
}

fn snapshot(rows: &[Value]) -> AttachResponse {
    serde_json::from_value(json!({"ok":true,"release_checks":rows})).unwrap()
}

fn shared_words() -> Value {
    serde_json::from_str::<Value>(include_str!("fixtures/inbox-display.json")).unwrap()["releaseChecks"].clone()
}

fn record_description(record: &Value) -> String {
    let words = shared_words();
    let progress = record["progress"].as_str().unwrap();
    let name = if progress == "complete" {
        record["state"].as_str().unwrap()
    } else {
        progress
    };
    let diagnostic = |kind: &str, field: &str| {
        words[kind][record[field].as_str().unwrap_or("other")]
            .as_str()
            .unwrap_or_else(|| words[kind]["other"].as_str().unwrap())
    };
    words["details"][name]
        .as_str()
        .unwrap()
        .replace("{fact}", diagnostic("facts", "fact"))
        .replace("{reason}", diagnostic("reasons", "reason"))
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
        &["release_check"],
        &snapshot(records),
        if cause == "off" { "off" } else { "auto" },
        &[],
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
        ("forgotten", "inconclusive", json!("forgotten"), "complete"),
    ] {
        let records = rows(result, progress);
        let frames = draw(cause, &records, &[key('v')]);
        assert_eq!(frames.len(), 2, "initial frame and frame after Details key");
        assert!(panel(frames.last().unwrap()).contains(&record_description(&records[0])));
        insta::assert_snapshot!(format!("details_{name}"), panel(frames.last().unwrap()));
    }
    for cause in ["no_reference", "off"] {
        let frames = draw(cause, &[], &[key('v'), key('R')]);
        assert_eq!(
            frames[1], frames[2],
            "{cause} cannot offer an unusable Re-check"
        );
        insta::assert_snapshot!(format!("details_{cause}"), panel(frames.last().unwrap()));
    }
    for (name, position) in [("queue_last", Some(8)), ("queue_unknown", None)] {
        let mut records = rows(Value::Null, "queued");
        records[0].as_object_mut().unwrap().remove("queue_position");
        if let Some(position) = position {
            records[0]["queue_position"] = json!(position);
        }
        let frames = draw("running", &records, &[key('v')]);
        insta::assert_snapshot!(name, panel(frames.last().unwrap()));
    }
    for (name, result, field) in [
        ("unknown_fact", "differs", "fact"),
        ("unknown_reason", "inconclusive", "reason"),
    ] {
        let mut records = rows(json!(result), "complete");
        records[0][field] = json!("a_future_diagnostic");
        let frames = draw(result, &records, &[key('v')]);
        insta::assert_snapshot!(name, panel(frames.last().unwrap()));
    }
}

/// Scenario: Open Details with two identities of one release whose failures differ from each other and from the card's hold. Move between them and an older release, read each selected record's own explanation, and close with Escape to restore the original card without an operation.
#[spec("inbox/release-check/004")]
#[test]
fn release_check_004_details_selects_a_record_and_escape_restores_the_card() {
    let mut records = vec![
        row(
            "2.1.313",
            "0123456789abcdef",
            json!("inconclusive"),
            "complete",
        ),
        row(
            "2.1.313",
            "fedcba9876543210",
            json!("unavailable"),
            "complete",
        ),
        row(
            "2.1.310",
            "aaaaaaaaaaaaaaaa",
            json!("forgotten"),
            "complete",
        ),
    ];
    // Neither record belongs to the pane's tripped/layout diagnostic.
    records[0]["reason"] = json!("launch");
    records[1]["reason"] = json!("no_process_containment");
    let frames = draw(
        "tripped",
        &records,
        &[key('v'), key('j'), key('j'), key('k'), esc()],
    );
    assert_eq!(frames.len(), 6);
    assert!(panel(&frames[1]).contains(&record_description(&records[0])));
    assert!(panel(&frames[2]).contains(&record_description(&records[1])));
    insta::assert_snapshot!("details_selected_launch", panel(&frames[1]));
    insta::assert_snapshot!("details_selected_unavailable", panel(&frames[2]));
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
    trace_preview: String,
}

impl Backend {
    fn new(result: Value, progress: &str) -> Self {
        Self {
            records: rows(result, progress),
            calls: vec![],
            mode: "auto".into(),
            exported_to: None,
            trace_preview: "SCRUBBED-TRACE-PREVIEW".into(),
        }
    }

    fn execute(&mut self, request: Value) -> Result<Value, String> {
        self.calls.push(request.clone());
        match request["op"].as_str().expect("command operation") {
            "release-check-run" => {
                assert_eq!(request, json!({"op":"release-check-run","agent":"claude"}));
                if self.records.is_empty() {
                    self.records = rows(Value::Null, "queued");
                }
                self.records[0]["progress"] = json!("queued");
                self.records[0]["queue_position"] = json!(2);
            }
            "release-check-cancel" => {
                assert_eq!(request, json!({"op":"release-check-cancel"}));
                let running = self
                    .records
                    .iter_mut()
                    .find(|r| r["progress"] == "running")
                    .expect("a running check");
                running["progress"] = json!("complete");
                running.as_object_mut().unwrap().remove("queue_position");
            }
            "release-check-forget" => {
                assert_eq!(
                    request,
                    json!({"op":"release-check-forget","agent":"claude","release":"2.1.313"})
                );
                for record in &mut self.records {
                    if record["agent"] == request["agent"]
                        && record["release"] == request["release"]
                    {
                        record["state"] = json!("forgotten");
                    }
                }
            }
            "release-check-export-preview" => {
                assert_eq!(
                    request,
                    json!({"op":"release-check-export-preview","agent":"claude","release":"2.1.313"})
                );
                return Ok(json!({"ok":true,"files":[
                    {"name":"record.json","content":"{\"state\":\"passed\"}"},
                    {"name":"trace.txt","content":self.trace_preview},
                    {"name":"README","content":shared_words()["exportInspection"]}
                ]}));
            }
            "release-check-export" => {
                assert_eq!(request["agent"], "claude");
                assert_eq!(request["release"], "2.1.313");
                assert!(request.get("identity").is_none());
                self.exported_to =
                    Some(request["dir"].as_str().expect("chosen destination").into());
            }
            "release-check-mode" => {
                self.mode = request["mode"].as_str().expect("chosen mode").into();
                if self.mode == "off" {
                    for record in &mut self.records {
                        if record["progress"] == "running" || record["progress"] == "queued" {
                            record["progress"] = json!("complete");
                            record["state"] = json!("inconclusive");
                            record["reason"] = json!("runner_stopped");
                            record.as_object_mut().unwrap().remove("queue_position");
                        }
                    }
                }
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
            &["release_check"],
            &snapshot,
            &mode,
            &[],
            keys,
            (120, 32),
            &mut |request: Value| self.execute(request),
        )
    }
}

/// Scenario: Re-check from the hold line and from Details, including ask approval and an older selected record; the current identity starts checking. Cancel from the older selection asks first: Escape preserves the running check and Enter confirms, displaying the cancellation answer.
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
        if mode == "ask" {
            backend.records.clear();
        }
        let frames = backend.run(
            if mode == "ask" {
                "running"
            } else {
                "inconclusive"
            },
            &keys,
        );
        if mode == "ask" {
            let words: Value =
                serde_json::from_str(include_str!("fixtures/inbox-display.json")).unwrap();
            let approval = words["releaseChecks"]["card"]["approval_proposal"]
                .as_str()
                .unwrap()
                .replace("{agent}", "Claude Code")
                .replace("{release}", "2.1.313");
            let initial = (0..frames[0].area().height)
                .map(|y| {
                    (0..frames[0].area().width)
                        .map(|x| frames[0][(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                initial.contains(&approval),
                "ask without a queued check must offer approval:\n{initial}"
            );
        }
        assert_eq!(backend.calls.len(), 1, "one accepted Re-check/approval");
        insta::assert_snapshot!(name, panel(frames.last().unwrap()));
    }
    let mut backend = Backend::new(Value::Null, "running");
    backend.records.push(row(
        "2.1.310",
        "aaaaaaaaaaaaaaaa",
        json!("passed"),
        "complete",
    ));
    let asked = backend.run("running", &[key('v'), key('j'), key('c')]);
    assert!(
        backend.calls.is_empty(),
        "Cancel must ask before sending an operation"
    );
    assert!(panel(&asked[3]).contains(shared_words()["cancelConfirmation"].as_str().unwrap()));
    insta::assert_snapshot!("cancel_confirmation", panel(&asked[3]));
    let dismissed = backend.run("running", &[key('v'), key('j'), key('c'), esc()]);
    assert!(
        backend.calls.is_empty(),
        "Escape must keep the check running"
    );
    assert_eq!(
        dismissed[2], dismissed[4],
        "Escape restores the selected Details row"
    );
    insta::assert_snapshot!("cancel_confirmation_dismissed", panel(&dismissed[4]));
    let frames = backend.run(
        "running",
        &[key('v'), key('j'), key('c'), enter(), key('k')],
    );
    assert_eq!(backend.calls.len(), 1, "one accepted cancellation");
    insta::assert_snapshot!("cancelled", panel(frames.last().unwrap()));

    let mut backend = Backend::new(json!("inconclusive"), "complete");
    backend.records.push(row(
        "2.1.310",
        "aaaaaaaaaaaaaaaa",
        json!("passed"),
        "complete",
    ));
    let frames = backend.run("inconclusive", &[key('v'), key('j'), key('R'), key('k')]);
    assert_eq!(
        backend.calls,
        [json!({"op":"release-check-run","agent":"claude"})]
    );
    insta::assert_snapshot!(
        "recheck_current_from_old_selection",
        panel(frames.last().unwrap())
    );
}

/// Scenario: Open a release with one identity and then one with two identities beside an older release; show the release-wide Forget explanation only for the latter. Forget asks before acting: Escape preserves the records and Enter confirms, after which both selected-release records are forgotten and the other release stays passed.
#[spec("inbox/release-check/004")]
#[test]
fn release_check_004_forget_acts_on_the_selected_release() {
    let mut backend = Backend::new(json!("differs"), "complete");
    let scope = shared_words()["details"]["forget_release"]
        .as_str()
        .unwrap()
        .replace("{release}", "2.1.313");
    let single = backend.run("differs", &[key('v')]);
    let single_panel = panel(single.last().unwrap());
    assert!(single_panel.contains("[Forget f]"), "{single_panel}");
    assert!(
        !single_panel.contains(&scope),
        "one identity does not need the release-wide Forget explanation:\n{single_panel}"
    );
    assert!(backend.calls.is_empty());
    let single_asked = backend.run("differs", &[key('v'), key('f')]);
    assert!(backend.calls.is_empty(), "single-identity Forget also asks");
    assert!(
        panel(&single_asked[2]).contains(
            &shared_words()["forgetConfirmation"]
                .as_str()
                .unwrap()
                .replace("{release}", "2.1.313")
        )
    );
    backend.records.insert(
        0,
        row("2.1.310", "aaaaaaaaaaaaaaaa", json!("passed"), "complete"),
    );
    backend.records.push(row(
        "2.1.313",
        "fedcba9876543210",
        json!("passed"),
        "complete",
    ));
    let asked = backend.run("differs", &[key('v'), key('j'), key('j'), key('f')]);
    assert!(
        backend.calls.is_empty(),
        "Forget must ask before sending an operation"
    );
    insta::assert_snapshot!("forget_confirmation", panel(&asked[4]));
    let dismissed = backend.run("differs", &[key('v'), key('j'), key('j'), key('f'), esc()]);
    assert!(backend.calls.is_empty(), "Escape must preserve the records");
    assert_eq!(dismissed[3], dismissed[5]);
    insta::assert_snapshot!("forget_confirmation_dismissed", panel(&dismissed[5]));
    let frames = backend.run(
        "differs",
        &[
            key('v'),
            key('j'),
            key('j'),
            key('f'),
            enter(),
            key('k'),
            key('k'),
        ],
    );
    assert_eq!(backend.calls.len(), 1);
    assert!(
        panel(&frames[3]).contains(&scope),
        "several identities must show the release-wide Forget explanation"
    );
    insta::assert_snapshot!("forget_release_scope", panel(&frames[3]));
    insta::assert_snapshot!("forgot_selected", panel(&frames[5]));
    insta::assert_snapshot!("forgot_other_identity", panel(&frames[6]));
    insta::assert_snapshot!("forgot_preserves_other_release", panel(&frames[7]));

    // A stale f/e binding on an already-forgotten row must be inert.
    let mut backend = Backend::new(json!("forgotten"), "complete");
    let frames = backend.run("inconclusive", &[key('v'), key('f'), key('e')]);
    assert!(backend.calls.is_empty());
    assert_eq!(frames[1], frames[3]);
}

/// Scenario: Select either of two identities, open Export's file list and inspect each file before choosing a destination; Escape writes nothing and Enter confirms once. Recorded controls and invisible Unicode appear as printable escapes, and a long preview stays within 16,384 code points after escaping, with a marker stating the full exported file's byte count.
#[spec("inbox/release-check/004")]
#[test]
fn release_check_004_export_asks_for_a_directory_and_can_be_cancelled() {
    let mut backend = Backend::new(json!("passed"), "complete");
    backend.records.push(row(
        "2.1.313",
        "fedcba9876543210",
        json!("passed"),
        "complete",
    ));
    let frames = backend.run("running", &[key('v'), key('e')]);
    assert_eq!(backend.calls.len(), 1, "one read-only preview request");
    assert_eq!(backend.calls[0]["op"], "release-check-export-preview");
    assert!(backend.exported_to.is_none(), "preview writes nothing");
    insta::assert_snapshot!("export_preview", panel(frames.last().unwrap()));
    backend.calls.clear();
    let cancelled = backend.run(
        "running",
        &[
            key('v'),
            key('j'),
            key('e'),
            enter(),
            esc(),
            key('j'),
            enter(),
            esc(),
            key('j'),
            enter(),
            esc(),
            esc(),
        ],
    );
    for (name, index) in [
        ("export_record_inspected", 4),
        ("export_trace_inspected", 7),
        ("export_readme_inspected", 10),
    ] {
        insta::assert_snapshot!(name, panel(&cancelled[index]));
    }
    assert!(
        backend.exported_to.is_none(),
        "Escape after inspection writes nothing"
    );
    assert_eq!(
        backend.calls.len(),
        1,
        "inspection uses the returned preview, never a write"
    );
    insta::assert_snapshot!("export_cancelled", panel(cancelled.last().unwrap()));
    backend.calls.clear();
    let cancelled = backend.run("running", &[key('v'), key('e'), key('w'), esc()]);
    assert!(
        backend.exported_to.is_none(),
        "Escape from directory prompt writes nothing"
    );
    insta::assert_snapshot!("export_prompt", panel(&cancelled[3]));
    backend.calls.clear();
    let dir = "/sandbox/check exports";
    let mut keys = vec![key('v'), key('j'), key('e'), key('w')];
    keys.extend(dir.chars().map(key));
    keys.push(enter());
    let frames = backend.run("running", &keys);
    assert_eq!(
        backend.calls.len(),
        2,
        "preview read then one confirmed write"
    );
    assert_eq!(
        backend.calls[1],
        json!({"op":"release-check-export","agent":"claude","release":"2.1.313","dir":dir})
    );
    assert_eq!(backend.exported_to.as_deref(), Some(dir));
    insta::assert_snapshot!("exported", panel(frames.last().unwrap()));

    let hostile = concat!(
        "TRACE-BEGIN\n",
        "ESC:\x1b\n",
        "CSI:\x1b[2J\x1b[1;1H\x1b[31m\n",
        "OSC0:\x1b]0;TITLE\x07\n",
        "OSC52:\x1b]52;c;dGVzdA==\x1b\\\n",
        "EDIT:BEFORE\x0dAFTER\x08X\x09TAB\n",
        "C0:\x00\x08\x09\x0b\x0c\x0d\x1f\x7f\n",
        "C1:\u{0085}\u{009b}2J\u{009d}0;TITLE\u{009c}\n",
        "TRACE-END"
    );
    let escaped = concat!(
        "TRACE-BEGIN\n",
        "ESC:\\x1b\n",
        "CSI:\\x1b[2J\\x1b[1;1H\\x1b[31m\n",
        "OSC0:\\x1b]0;TITLE\\x07\n",
        "OSC52:\\x1b]52;c;dGVzdA==\\x1b\\\n",
        "EDIT:BEFORE\\x0dAFTER\\x08X\\x09TAB\n",
        "C0:\\x00\\x08\\x09\\x0b\\x0c\\x0d\\x1f\\x7f\n",
        "C1:\\x85\\x9b2J\\x9d0;TITLE\\x9c\n",
        "TRACE-END"
    );
    // Exercise every C0/C1 byte, including DEL, beyond the named attack cases.
    let mut all_controls = String::from("TRACE-BEGIN\n");
    let mut all_escaped = String::from("TRACE-BEGIN\n");
    for chunk in (0_u8..=0x9f)
        .filter(|byte| *byte != b'\n' && (*byte <= 0x1f || *byte >= 0x7f))
        .collect::<Vec<_>>()
        .chunks(8)
    {
        all_controls.push_str("BYTES:");
        all_escaped.push_str("BYTES:");
        for byte in chunk {
            all_controls.push(char::from(*byte));
            all_escaped.push_str(&format!("\\x{byte:02x}"));
        }
        all_controls.push('\n');
        all_escaped.push('\n');
    }
    all_controls.push_str("TRACE-END");
    all_escaped.push_str("TRACE-END");
    for (raw, printable) in [(hostile, escaped), (&all_controls, &all_escaped)] {
        let inspect = |content: &str| {
            let mut backend = Backend::new(json!("passed"), "complete");
            backend.trace_preview = content.into();
            let frames = backend.run("running", &[key('v'), key('e'), key('j'), enter()]);
            assert_eq!(backend.calls.len(), 1, "inspection is read-only");
            assert!(backend.exported_to.is_none());
            frames.last().unwrap().clone()
        };
        let control = inspect(printable);
        let actual = inspect(raw);
        let displayed = panel(&actual);
        for line in printable.lines() {
            assert!(
                displayed.contains(line),
                "missing inert trace line {line:?}:\n{displayed}"
            );
        }
        for y in actual.area.y..actual.area.bottom() {
            for x in actual.area.x..actual.area.right() {
                let symbol = actual[(x, y)].symbol();
                assert!(
                    !symbol.chars().any(char::is_control),
                    "control character in preview buffer cell ({x}, {y}): {symbol:?}"
                );
            }
        }
        assert_eq!(
            actual, control,
            "escaping trace controls must preserve the dialog and surrounding rows"
        );
    }

    // Category-based rule shared with SC-11: Cc except LF; Cf; Zl/Zp;
    // Zs except ASCII space; and Default_Ignorable_Code_Point (including
    // marks/variation selectors and Hangul fillers). Rust strings contain no
    // lone surrogates; the desktop pins those separately for UTF-16 input.
    for (raw, printable) in [
        (
            "a\u{061c}\u{200e}\u{200f}\u{202a}\u{202e}\u{2066}\u{2069}z",
            "a\\u{61c}\\u{200e}\\u{200f}\\u{202a}\\u{202e}\\u{2066}\\u{2069}z",
        ),
        (
            "a\u{200b}\u{200c}\u{200d}\u{2060}\u{feff}z",
            "a\\u{200b}\\u{200c}\\u{200d}\\u{2060}\\u{feff}z",
        ),
        (
            "a\u{00ad}\u{e0001}\u{e0061}\u{e007f}z",
            "a\\u{ad}\\u{e0001}\\u{e0061}\\u{e007f}z",
        ),
        (
            "a\u{2061}\u{2062}\u{2063}\u{2064}\u{206a}\u{206f}z",
            "a\\u{2061}\\u{2062}\\u{2063}\\u{2064}\\u{206a}\\u{206f}z",
        ),
        (
            "a\u{fff9}\u{fffa}\u{fffb}z",
            "a\\u{fff9}\\u{fffa}\\u{fffb}z",
        ),
        ("a\u{2028}b\u{2029}z", "a\\u{2028}b\\u{2029}z"),
        (
            "a\u{034f}\u{fe0f}\u{e0100}z",
            "a\\u{34f}\\u{fe0f}\\u{e0100}z",
        ),
        (
            "a\u{115f}\u{1160}\u{3164}\u{ffa0}z",
            "a\\u{115f}\\u{1160}\\u{3164}\\u{ffa0}z",
        ),
        (
            "a\u{00a0}\u{1680}\u{2000}\u{2007}\u{202f}\u{205f}\u{3000}z",
            "a\\u{a0}\\u{1680}\\u{2000}\\u{2007}\\u{202f}\\u{205f}\\u{3000}z",
        ),
        ("é界🙂\\literal\nsecond", "é界🙂\\literal\nsecond"),
    ] {
        let inspect = |text: &str| {
            let mut backend = Backend::new(json!("passed"), "complete");
            backend.trace_preview = text.into();
            let frames = backend.run("running", &[key('v'), key('e'), key('j'), enter()]);
            assert_eq!(backend.calls.len(), 1, "inspection is read-only");
            assert!(backend.exported_to.is_none());
            frames.last().unwrap().clone()
        };
        let actual = inspect(raw);
        let displayed = panel(&actual);
        for line in printable.lines() {
            assert!(
                displayed.contains(line),
                "missing inert Unicode line {line:?}:\n{displayed}"
            );
        }
        assert_eq!(
            actual,
            inspect(printable),
            "inert Unicode must preserve surrounding rows"
        );
        for y in actual.area.y..actual.area.bottom() {
            for x in actual.area.x..actual.area.right() {
                let symbol = actual[(x, y)].symbol();
                assert!(
                    !symbol.chars().any(char::is_control),
                    "control-bearing cell ({x},{y}): {symbol:?}"
                );
                for invisible in raw.chars().filter(|c| !printable.contains(*c)) {
                    assert!(
                        !symbol.contains(invisible),
                        "invisible character in cell ({x},{y}): {symbol:?}"
                    );
                }
            }
        }
    }

    let mut backend = Backend::new(json!("passed"), "complete");
    backend.trace_preview = format!("\u{202e}{}END_OF_HOSTILE_TRACE", "界".repeat(1_000_000));
    let full_bytes = backend.trace_preview.len();
    let initial = snapshot(&backend.records);
    // Room for the bounded inspection, including line wrapping. Requiring the
    // marker prevents viewport clipping from masquerading as truncation.
    let frames = render_release_check_key_sequence_to_buffers(
        &KeybindingConfig::default(),
        &state("running"),
        SESSION,
        &["release_check"],
        &initial,
        "auto",
        &[],
        &[key('v'), key('e'), key('j'), enter()],
        (240, 512),
        &mut |request| backend.execute(request),
    );
    let displayed = panel(frames.last().unwrap());
    let marker = shared_words()["previewTruncated"]
        .as_str()
        .unwrap()
        .replace("{bytes}", &full_bytes.to_string());
    let unwrapped = displayed.lines().collect::<String>();
    let readable = displayed.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        unwrapped.contains("\\u{202e}"),
        "escaped prefix must be visible"
    );
    assert!(
        readable.contains(&marker),
        "full-file marker missing: {displayed}"
    );
    assert!(!displayed.contains("END_OF_HOSTILE_TRACE"));
    // Exclude known dialog chrome; the budget includes the marker itself.
    let content = unwrapped
        .strip_prefix("Release checksExport Claude Code 2.1.313 (all records)trace.txt")
        .and_then(|text| text.strip_suffix("[Back Esc]"))
        .expect("inspection chrome surrounds the bounded text");
    assert!(
        content.chars().count() <= 16_384,
        "inspection exceeds 16,384 Unicode code points after escaping"
    );
    assert_eq!(backend.calls.len(), 1);
    assert!(backend.exported_to.is_none());
}

/// Scenario: Change auto to ask, then attempt off during a running check; Escape preserves it and Enter accepts off and displays the stopped result. A queued check turns off without asking, matching the desktop; switch back to auto and read a refused Re-check without a false success message.
#[spec("inbox/release-check/004")]
#[test]
fn release_check_004_mode_settings_and_refusal_are_visible() {
    let mut backend = Backend::new(Value::Null, "running");
    let frames = backend.run("running", &[key('p'), key('M')]);
    insta::assert_snapshot!("mode_ask", agent_settings(frames.last().unwrap()));
    backend.calls.clear();
    let frames = backend.run("running", &[key('p'), key('M'), esc(), key('v')]);
    assert!(
        backend.calls.is_empty(),
        "off requires confirmation while running"
    );
    insta::assert_snapshot!("mode_off_confirmation", panel(&frames[2]));
    insta::assert_snapshot!("mode_off_cancelled", agent_settings(&frames[3]));
    insta::assert_snapshot!("mode_off_cancelled_running", panel(frames.last().unwrap()));
    let frames = backend.run("running", &[key('p'), key('M'), enter(), key('v')]);
    insta::assert_snapshot!("mode_off", agent_settings(&frames[3]));
    insta::assert_snapshot!("mode_off_stopped_check", panel(frames.last().unwrap()));
    let frames = backend.run("off", &[key('p'), key('M')]);
    insta::assert_snapshot!("mode_auto", agent_settings(frames.last().unwrap()));
    assert_eq!(
        backend
            .calls
            .iter()
            .map(|c| c["mode"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["off", "auto"]
    );
    let initial = snapshot(&rows(json!("inconclusive"), "complete"));
    let frames = render_release_check_key_sequence_to_buffers(
        &KeybindingConfig::default(),
        &state("inconclusive"),
        SESSION,
        &["release_check"],
        &initial,
        "auto",
        &[],
        &[key('v'), key('R')],
        (120, 32),
        &mut |_: Value| -> Result<Value, String> {
            Err(
                "This daemon cannot run release checks. Upgrade the deck, or send it a prompt."
                    .into(),
            )
        },
    );
    insta::assert_snapshot!("refused", panel(frames.last().unwrap()));

    let mut queued = Backend::new(Value::Null, "queued");
    queued.mode = "ask".into();
    let frames = queued.run("running", &[key('p'), key('M'), esc(), key('v')]);
    assert_eq!(
        queued.calls,
        [json!({"op":"release-check-mode","mode":"off"})],
        "only running checks require Off confirmation"
    );
    assert!(agent_settings(&frames[2]).contains("off (checks stopped; local results unused)"));
    assert!(
        panel(frames.last().unwrap()).contains(
            shared_words()["reasons"]["runner_stopped"]
                .as_str()
                .unwrap()
        )
    );
}

/// Scenario: Connect to a daemon that does not advertise release checks and press the default Details, Re-check and mode keys on its held card and agent settings. Keep the ordinary card and an upgrade explanation visible without attempting any external operation.
#[spec("inbox/release-check/004")]
#[test]
fn release_check_004_absent_capability_withholds_controls() {
    for capabilities in [vec![], vec!["host_metrics"]] {
        let frames = render_release_check_key_sequence_to_buffers(
            &KeybindingConfig::default(),
            &state("inconclusive"),
            SESSION,
            &capabilities,
            &snapshot(&[]),
            "auto",
            &[],
            &[key('v'), key('R'), key('p'), key('v'), key('R'), key('M')],
            (120, 32),
            &mut |request| -> Result<Value, String> {
                panic!("unsupported daemon received an operation: {request}")
            },
        );
        assert_eq!(
            frames[0], frames[1],
            "Details must not open without the capability"
        );
        assert_eq!(frames[0], frames[2], "Re-check must be withheld");
        insta::assert_snapshot!("capability_absent", agent_settings(frames.last().unwrap()));
        assert_eq!(
            frames[3], frames[6],
            "unsupported controls remain inert in agent settings"
        );
    }
}

/// Scenario: Seed a restart notice, remap Details and Cancel, and press their old and new keys. The remapped Cancel asks and Enter confirms; old c stays inert in Details, while c after closing Details visibly dismisses the notice as a positive control.
#[spec("inbox/release-check/004")]
#[test]
fn release_check_004_keys_are_remappable_and_modal() {
    for (section, names) in [
        (
            "dashboard",
            &[
                "release_check_details",
                "release_check_run",
                "agent_details",
                "release_check_mode",
            ][..],
        ),
        (
            "release_check_details",
            &[
                "move_down",
                "move_up",
                "recheck",
                "cancel",
                "forget",
                "export",
                "close",
            ][..],
        ),
        ("release_check_export", &["open", "continue", "confirm"][..]),
    ] {
        for name in names {
            assert!(
                ACTIONS
                    .iter()
                    .any(|spec| spec.section.as_str() == section && spec.name == *name),
                "{section}.{name} must be a remappable ActionSpec"
            );
        }
    }
    let (config, warnings) = KeybindingConfig::from_toml_str(
        "[dashboard]\nrelease_check_details = \"F2\"\n[release_check_details]\ncancel = \"F3\"\n",
    )
    .expect("self-check actions belong to the remappable registry");
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut backend = Backend::new(Value::Null, "running");
    let initial = snapshot(&backend.records);
    let frames = render_release_check_key_sequence_to_buffers(
        &config,
        &state("running"),
        SESSION,
        &["release_check"],
        &initial,
        "auto",
        &["SC10-BACKGROUND-NOTICE"],
        &[
            key('v'),
            KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE),
            key('c'),
            KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE),
            enter(),
            esc(),
            KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE),
            key('c'),
        ],
        (120, 32),
        &mut |request| backend.execute(request),
    );
    assert_eq!(frames[0], frames[1], "old Details key is unbound");
    assert_eq!(
        frames[2], frames[3],
        "old Cancel key is inert inside the modal"
    );
    assert_eq!(backend.calls, [json!({"op":"release-check-cancel"})]);
    insta::assert_snapshot!("details_remapped_cancel", panel(&frames[2]));
    assert!(panel(&frames[4]).contains(shared_words()["cancelConfirmation"].as_str().unwrap()));
    insta::assert_snapshot!("cancelled_remapped", panel(&frames[5]));
    assert_eq!(
        frames[6], frames[7],
        "modal-only F3 cannot cancel behind Details"
    );
    let notice = "The deck restarted; these agents were stopped: SC10-BACKGROUND-NOTICE.";
    for index in [0, 6, 7] {
        assert!(
            buffer_text(&frames[index]).contains(notice),
            "background notice must survive modal c through closing Details, frame {index}"
        );
    }
    assert!(
        !buffer_text(&frames[8]).contains(notice),
        "positive control: dashboard c must dismiss the seeded notice"
    );
}

fn buffer_text(buffer: &Buffer) -> String {
    let area = buffer.area();
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
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
