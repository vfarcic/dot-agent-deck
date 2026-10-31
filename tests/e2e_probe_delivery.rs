#![cfg(all(feature = "e2e", target_os = "linux"))]

//! S-21: production mail decisions after the epoch becomes authoritative.
//! Uses S-20's read-only observations and ordinary scheduling requests. No
//! test operation supplies an empty observation, opens a gate or forces a ring.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/live_segments.rs"]
mod live;

use live::{Fixture, WAIT};
use serde_json::{Value, json};
use spec::spec;
use std::fs;
use std::time::Duration;

ctor::declarative::ctor! {
    #[ctor(unsafe)]
    fn native_fixture_entry() {
        live::native_entry();
    }
}

const ENV: &[(&str, &str)] = &[
    ("DOT_AGENT_DECK_E2E_ASSUME_CODEX_HEAD_ECHO", "0.164.0"),
    ("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000"),
    ("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "100"),
];

fn fixture(kind: &'static str) -> Fixture {
    let f = Fixture::with_env(kind, ENV);
    f.hook_event("session_start");
    f.hook_event("idle");
    f
}

fn attached(f: &Fixture) -> common::TuiDeck {
    common::TuiDeck::builder()
        .with_pty_size(200, 50)
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            f.daemon.attach_socket.to_string_lossy(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            f.daemon.hook_socket.to_string_lossy(),
        )
        .launch_with_fixture("minimal")
}

fn rows(f: &Fixture, suffix: &str) -> Vec<Value> {
    fs::read_to_string(f.path(suffix))
        .unwrap_or_default()
        .lines()
        .map(|s| serde_json::from_str(s).expect("fixture JSON row"))
        .collect()
}

fn mail_turns(f: &Fixture, id: &str) -> Vec<String> {
    rows(f, "turns")
        .iter()
        .filter_map(|row| row["line"].as_str())
        .filter(|line| inbox::is_ring_line(line, id))
        .map(str::to_owned)
        .collect()
}

fn handovers(f: &Fixture) -> Vec<Value> {
    rows(
        f,
        if f.kind == "codex" {
            "hooks"
        } else {
            "fetches"
        },
    )
}

fn delivered(f: &Fixture, id: &str) -> String {
    assert!(
        common::wait_until(Duration::from_secs(40), || mail_turns(f, id).len() == 1),
        "one submitted notice for {id}; bytes {:?}; screen {}",
        f.input(),
        f.screen()
    );
    // The synthetic composer really runs the fetch CLI/hook. Its receipt is
    // the delivery oracle, rather than a claimed observer or routing state.
    assert!(
        common::wait_until(WAIT, || handovers(f)
            .iter()
            .any(|row| row.to_string().contains(id)
                && row.to_string().contains("TEST-ONLY-WAITING-MAIL"))),
        "the notice must cause actual inbox delivery: {:?}",
        handovers(f)
    );
    let line = mail_turns(f, id).remove(0);
    let before = f.input();
    f.stable(&before);
    assert_eq!(
        mail_turns(f, id).as_slice(),
        std::slice::from_ref(&line),
        "the mail notice must be submitted once"
    );
    assert_eq!(
        handovers(f)
            .iter()
            .filter(|row| row.to_string().contains(id))
            .count(),
        1,
        "the message must occur in one actual hand-over"
    );
    assert!(!String::from_utf8_lossy(&f.input()).contains("TEST-ONLY-WAITING-MAIL"));
    line
}

fn hold(f: &Fixture) -> Value {
    f.snapshot()["inbox"]["wake_hold"].clone()
}

fn wait_hold(f: &Fixture, reason: &str) {
    assert!(
        common::wait_until(WAIT, || hold(f) == reason),
        "card must project {reason}, got {}; bytes {:?}",
        f.snapshot(),
        f.input()
    );
}

fn touched_empty(f: &Fixture) {
    let mut human = f.connect();
    human.write("x\x7f");
    f.expect(b"x\x7f");
}

/// Scenario: Type a human draft through the attached TUI and queue mail, then submit the draft. This synthetic composer fetches only on notice lines so the test can require a check after the user's Enter, followed by a separate notice and one actual mail fetch.
#[spec("inbox/doorbell/009")]
#[test]
fn doorbell_009_attached_draft_enter_releases_the_doorbell() {
    let f = fixture("claude");
    let tui = attached(&f);
    tui.send_keys(b"1");
    tui.wait_for_string("Claude Code");
    f.flag("fetch-notice-only");
    tui.send_keys(inbox::DRAFT.as_bytes());
    f.expect(inbox::DRAFT.as_bytes());
    let id = f.enqueue();
    f.wait("helper-exited-1");
    let draft_check = format!("{}\x07", inbox::DRAFT);
    f.stable(draft_check.as_bytes());
    tui.send_keys(b"\r");
    let line = delivered(&f, &id);
    f.expect(format!("{draft_check}\r\x07{line}\r").as_bytes());
    assert_eq!(rows(&f, "turns")[0]["line"], inbox::DRAFT);
    tui.wait_for_string(&id);
    tui.wait_for_string(&line[..8]);
}

/// Scenario: Leave a draft in the attached Claude stand-in while mail waits and a real helper check reads it as non-empty. During a 1500 ms observation no notice may appear; Ctrl+U must be followed by a fresh empty check and a notice, without submitting the draft.
#[spec("inbox/doorbell/010")]
#[test]
fn doorbell_010_draft_has_no_timeout_cap() {
    let f = fixture("claude");
    let tui = attached(&f);
    tui.send_keys(b"1");
    tui.wait_for_string("Claude Code");
    tui.send_keys(inbox::DRAFT.as_bytes());
    f.expect(inbox::DRAFT.as_bytes());
    let id = f.enqueue();
    f.wait("helper-exited-1");
    let checked = format!("{}\x07", inbox::DRAFT);
    f.stable(checked.as_bytes());
    let at = std::time::Instant::now();
    assert!(
        !common::wait_until(Duration::from_millis(1500), || f.input()
            != checked.as_bytes()),
        "draft was force-submitted after {:?}: {:?}",
        at.elapsed(),
        f.input()
    );
    tui.send_keys(b"\x15");
    let line = delivered(&f, &id);
    f.expect(format!("{checked}\x15\x07{line}\r").as_bytes());
    assert!(
        rows(&f, "turns")
            .iter()
            .all(|row| row["line"] != inbox::DRAFT)
    );
}

/// Scenario: After a first notice is delivered, press Left in the attached synthetic Claude composer's empty box. With no further keypress, the next message requires a real check and then a notice and actual fetch; navigation alone cannot supply the empty observation.
#[spec("inbox/doorbell/010")]
#[test]
fn doorbell_010_attached_left_in_empty_editor_does_not_strand_task() {
    let f = fixture("claude");
    let tui = attached(&f);
    tui.send_keys(b"1");
    tui.wait_for_string("Claude Code");
    let control = f.enqueue();
    let first = delivered(&f, &control);
    tui.send_keys(b"\x1b[D");
    f.expect(format!("{first}\r\x1b[D").as_bytes());
    let id = f.enqueue();
    let second = delivered(&f, &id);
    f.expect(format!("{first}\r\x1b[D\x07{second}\r").as_bytes());
}

/// Scenario: Deliver a first notice, then type a fresh draft through the attached TUI and clear it after the message has been fetched. The completed ring sends no further byte, the human draft is not submitted, and a later message requires a new check rather than reusing the earlier ring's observation.
#[spec("inbox/doorbell/011")]
#[test]
fn doorbell_011_a_submitted_ring_sends_nothing_after_a_new_human_draft() {
    let f = fixture("claude");
    let tui = attached(&f);
    tui.send_keys(b"1");
    tui.wait_for_string("Claude Code");
    let id = f.enqueue();
    let first = delivered(&f, &id);
    tui.send_keys(inbox::DRAFT.as_bytes());
    let typed = format!("{first}\r{}", inbox::DRAFT);
    f.expect(typed.as_bytes());
    f.stable(typed.as_bytes());
    tui.send_keys(b"\x15");
    let cleared = format!("{typed}\x15");
    f.expect(cleared.as_bytes());
    f.stable(cleared.as_bytes());
    let next = f.enqueue();
    let line = delivered(&f, &next);
    f.expect(format!("{cleared}\x07{line}\r").as_bytes());
    assert!(
        rows(&f, "turns")
            .iter()
            .all(|row| row["line"] != inbox::DRAFT)
    );
}

/// Scenario: Hold two queued reports behind a draft in an attached Claude pane. Clearing the draft requires a new check and one coalesced notice, whose real CLI fetch returns the reports in arrival order while neither report body is typed into the terminal.
#[spec("inbox/doorbell/012")]
#[test]
fn doorbell_012_drafted_batch_fetch_preserves_arrival_order() {
    let f = fixture("claude");
    let tui = attached(&f);
    tui.send_keys(b"1");
    tui.wait_for_string("Claude Code");
    tui.send_keys(inbox::DRAFT.as_bytes());
    f.expect(inbox::DRAFT.as_bytes());
    let first = f.enqueue_body("first-worker-report-1666");
    f.wait("helper-exited-1");
    let second = f.enqueue_body("second-worker-report-1666");
    let checked = format!("{}\x07", inbox::DRAFT);
    f.stable(checked.as_bytes());
    tui.send_keys(b"\x15");
    assert!(
        common::wait_until(WAIT, || handovers(&f)
            .iter()
            .any(|r| r.to_string().contains(&first) && r.to_string().contains(&second))),
        "one fetch must hand over both reports: {:?}",
        handovers(&f)
    );
    let fetches = handovers(&f);
    assert_eq!(fetches.len(), 1);
    let output = fetches[0]["stdout"].as_str().unwrap();
    assert!(
        output.find("first-worker-report-1666").unwrap()
            < output.find("second-worker-report-1666").unwrap()
    );
    let lines = mail_turns(&f, &first);
    assert_eq!(lines.len(), 1);
    f.expect(format!("{checked}\x15\x07{}\r", lines[0]).as_bytes());
    assert!(!String::from_utf8_lossy(&f.input()).contains("worker-report-1666"));
    let grid = tui.snapshot_grid();
    assert!(
        !grid.contains("first-worker-report-1666") && !grid.contains("second-worker-report-1666")
    );
}

/// Scenario: Send two Escape keys while Claude is working, then complete the physical key sequence with a typed-and-deleted character and report the turn idle. The Escapes keep the box touched, so the waiting message requires a real empty check before its notice is submitted.
#[spec("inbox/doorbell/019")]
#[test]
fn doorbell_019_an_interrupts_escapes_do_not_hold_doorbell() {
    let f = fixture("claude");
    f.hook_event("thinking");
    let mut human = f.connect();
    human.write("\x1b");
    f.expect(b"\x1b");
    human.write("\x1bx\x7f");
    f.expect(b"\x1b\x1bx\x7f");
    f.hook_event("idle");
    let id = f.enqueue();
    let line = delivered(&f, &id);
    f.expect(format!("\x1b\x1bx\x7f\x07{line}\r").as_bytes());
}

/// Scenario: Leave text followed by Escape keys in Claude's composer and report repeated idle events. A real check must retain the draft and withhold the notice; clearing the draft requires another check before delivery, regardless of the repeated idle reports.
#[spec("inbox/doorbell/019")]
#[test]
fn doorbell_019_drafts_hold_doorbell_and_an_idle_agents_escapes_defer_to_its_screen() {
    let f = fixture("claude");
    let mut human = f.connect();
    human.write("unsent\x1b\x1bx\x7f");
    f.expect(b"unsent\x1b\x1bx\x7f");
    f.hook_event("idle");
    f.hook_event("idle");
    let id = f.enqueue();
    f.wait("helper-exited-1");
    f.stable(b"unsent\x1b\x1bx\x7f\x07");
    assert!(mail_turns(&f, &id).is_empty());
    human.write("\x15");
    let line = delivered(&f, &id);
    f.expect(format!("unsent\x1b\x1bx\x7f\x07\x15\x07{line}\r").as_bytes());
}

/// Scenario: Let the real helper read a retained Claude draft, then complete doorbell policy ticks at controlled ages of 5 seconds, 10 seconds, just under a day and a day. Those ages must submit no notice; clearing the draft still requires a fresh empty check before one actual delivery.
#[spec("inbox/doorbell/040")]
#[test]
fn doorbell_040_a_retained_draft_holds_at_every_controlled_age() {
    let scratch = common::harness_tempdir().unwrap();
    let log = scratch.path().join("doorbell.log");
    let mut env = ENV.to_vec();
    env.push(("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_DIR", "ring-barriers"));
    env.push(("DOT_AGENT_DECK_LOG", log.to_str().unwrap()));
    let f = Fixture::with_env("claude", &env);
    f.hook_event("session_start");
    f.hook_event("idle");
    let controls = f.daemon.home.join("ring-barriers");
    fs::create_dir_all(&controls).unwrap();
    let mut human = f.connect();
    human.write(inbox::DRAFT);
    f.expect(inbox::DRAFT.as_bytes());
    let id = f.enqueue();
    f.wait("helper-exited-1");
    let checked = format!("{}\x07", inbox::DRAFT);
    for age_ms in [5_000, 10_000, 86_399_000, 86_400_000] {
        fs::write(controls.join("clock-ms"), age_ms.to_string()).unwrap();
        assert!(
            common::wait_until(WAIT, || fs::read_to_string(controls.join("clock-observed"))
                .is_ok_and(|s| s == age_ms.to_string())),
            "policy tick at {age_ms}"
        );
        assert!(
            !common::wait_until(Duration::from_millis(250), || {
                let bytes = f.input();
                !bytes.starts_with(checked.as_bytes())
                    || bytes[inbox::DRAFT.len()..].iter().any(|b| *b != 7)
            }),
            "age {age_ms} wrote something other than checks: {:?}",
            f.input()
        );
        assert!(
            mail_turns(&f, &id).is_empty(),
            "age {age_ms} admitted a notice"
        );
    }
    wait_hold(&f, "possible_draft");
    let raised = fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("doorbell hold") && line.contains("Some(PossibleDraft)"))
        .count();
    assert_eq!(raised, 1, "the unchanged hold reason is raised once");
    fs::remove_file(controls.join("clock-ms")).unwrap();
    f.wait_open();
    let before_clear = f.input();
    human.write("\x15");
    let line = delivered(&f, &id);
    let mut expected = before_clear;
    expected.extend_from_slice(format!("\x15\x07{line}\r").as_bytes());
    f.expect(&expected);
}

/// Scenario: Make Codex keep a typed draft when Enter is pressed, then queue a message. Its real helper must read the retained draft and leave mail held; after the user clears the draft, a second completed check must precede the staged notice and actual prompt-hook delivery.
#[spec("inbox/codex/027")]
#[test]
fn codex_027_a_draft_kept_on_enter_holds_the_doorbell() {
    let f = fixture("codex");
    f.flag("keep");
    let mut human = f.connect();
    human.write(&format!("{}\r", inbox::DRAFT));
    f.expect(format!("{}\r", inbox::DRAFT).as_bytes());
    let id = f.enqueue();
    f.wait("helper-exited-1");
    let checked = format!("{}\r\x07", inbox::DRAFT);
    f.stable(checked.as_bytes());
    assert!(mail_turns(&f, &id).is_empty());
    f.remove("keep");
    human.write("\x15");
    let line = delivered(&f, &id);
    f.expect(format!("{checked}\x15\x07{line}\r").as_bytes());
}

/// Scenario: Type and delete a draft in an idle Claude composer, then queue mail. The real check must precede the single submitted notice and its real inbox fetch; deleting the draft alone is not permission to ring.
#[spec("inbox/probe/024")]
#[test]
fn probe_024_touched_empty_claude_is_checked_then_rung() {
    let f = fixture("claude");
    touched_empty(&f);
    let id = f.enqueue();
    let line = delivered(&f, &id);
    f.expect(format!("x\x7f\x07{line}\r").as_bytes());
    let next = f.enqueue();
    let second = delivered(&f, &next);
    f.expect(format!("x\x7f\x07{line}\r{second}\r").as_bytes());
}

/// Scenario: Type and delete a draft in an idle native Codex stand-in with the e2e observer and head-echo assumptions. A complete real check must finish before the single notice is submitted and Codex's prompt hook hands over its mail.
#[spec("inbox/probe/024")]
#[test]
fn probe_024_touched_empty_codex_is_checked_then_rung() {
    let f = fixture("codex");
    touched_empty(&f);
    let id = f.enqueue();
    let line = delivered(&f, &id);
    f.expect(format!("x\x7f\x07{line}\r").as_bytes());
    let next = f.enqueue();
    let second = delivered(&f, &next);
    f.expect(format!("x\x7f\x07{line}\r{second}\r").as_bytes());
}

/// Scenario: Queue mail behind three spaces in Claude's composer and let the helper actually read them. No notice may be submitted; after the user deletes the spaces, a second check must read the now-empty box before delivery.
#[spec("inbox/probe/025")]
#[test]
fn probe_025_observed_spaces_hold_and_deletion_requires_another_check() {
    let f = fixture("claude");
    let mut human = f.connect();
    human.write("   ");
    f.expect(b"   ");
    let id = f.enqueue();
    f.wait("helper-exited-1");
    f.stable(b"   \x07");
    assert!(mail_turns(&f, &id).is_empty());
    wait_hold(&f, "possible_draft");
    human.write("\x7f\x7f\x7f");
    let line = delivered(&f, &id);
    f.expect(format!("   \x07\x7f\x7f\x7f\x07{line}\r").as_bytes());
}

/// Scenario: Type two frames during each delayed, lossy Claude check while mail waits. The keys must survive in order without a notice mixed into them; after the user clears those keys, a fresh check must precede delivery.
#[spec("inbox/probe/026")]
#[test]
fn probe_026_typing_during_a_check_keeps_mail_held_until_a_fresh_empty_read() {
    for mechanism in ["H-chunk", "H-flush", "H-ignore"] {
        let f = fixture("claude");
        touched_empty(&f);
        f.setting("edit-mechanism", mechanism);
        f.flag("edit-delay");
        let id = f.enqueue();
        f.wait("edit-dispatched");
        let mut human = f.connect();
        human.write("FIRST");
        human.write("SECOND");
        f.wait_held();
        assert!(
            common::wait_until(WAIT, || f.input().starts_with(b"x\x7f\x07FIRSTSECOND")),
            "the held frames must arrive once and in order: {:?}",
            f.input()
        );
        assert!(mail_turns(&f, &id).is_empty());
        f.remove("edit-delay");
        human.write("\x15");
        let line = delivered(&f, &id);
        let bytes = f.input();
        let tail = bytes.strip_prefix(b"x\x7f\x07FIRSTSECOND").unwrap();
        let (before_clear, after_clear) = tail.split_at(
            tail.iter()
                .position(|b| *b == 21)
                .expect("the clear reaches the composer"),
        );
        assert!(
            before_clear.iter().all(|b| *b == 7),
            "only further checks may follow the held frames: {bytes:?}"
        );
        let checks = after_clear[1..]
            .strip_suffix(format!("{line}\r").as_bytes())
            .unwrap();
        assert!(
            !checks.is_empty() && checks.iter().all(|b| *b == 7),
            "a fresh check must follow the clear before the notice: {bytes:?}"
        );
    }
}

/// Scenario: Let a touched empty Claude drop the first automatic check without holding human input. A normal lapse retry must run the helper, read the empty box, and deliver the waiting mail once without a person pressing another key.
#[spec("inbox/probe/030")]
#[test]
fn probe_030_an_unattended_claude_lapse_still_delivers_once() {
    let f = fixture("claude");
    touched_empty(&f);
    f.flag("edit-ignore-next");
    let id = f.enqueue();
    f.expect(b"x\x7f\x07");
    let line = delivered(&f, &id);
    f.expect(format!("x\x7f\x07\x07{line}\r").as_bytes());
}

/// Scenario: Hold a Codex check's return after its real helper exits, then queue mail. Neither helper exit nor the incomplete mode return may submit a notice; the original composer completing E1 must permit one notice and one actual hand-over.
#[spec("inbox/probe/031")]
#[test]
fn probe_031_mail_waits_for_e1_then_delivers_once() {
    let f = fixture("codex");
    touched_empty(&f);
    f.flag("hold-return");
    f.probe();
    f.wait("helper-exited-1");
    let id = f.enqueue();
    f.stable(b"x\x7f\x07");
    assert!(mail_turns(&f, &id).is_empty());
    f.flag("resume-go");
    let line = delivered(&f, &id);
    f.expect(format!("x\x7f\x07{line}\r").as_bytes());
}

/// Scenario: Emit a cursor-position query and accept a native Ctrl+G while that query is outstanding, then queue mail. The attached TUI must explain that the box cannot be checked, and no automatic check or notice may follow that native key.
#[spec("inbox/probe/032")]
#[test]
fn probe_032_native_query_hazard_projects_cannot_check_on_the_card() {
    let f = fixture("claude");
    let tui = attached(&f);
    f.flag("edit-drop");
    f.emit(b"\x1b[6n");
    let mut human = f.connect();
    human.write("\x07");
    f.expect(b"\x07");
    let id = f.enqueue();
    wait_hold(&f, "cannot_check");
    let words: Value = serde_json::from_str(include_str!("fixtures/inbox-display.json")).unwrap();
    let expected = words["wakeHolds"]["cannot_check"].as_str().unwrap();
    assert!(
        common::wait_until(WAIT, || tui.snapshot_grid().contains(expected)),
        "{}",
        tui.snapshot_grid()
    );
    f.stable(b"\x07");
    assert!(mail_turns(&f, &id).is_empty());
}

/// Scenario: Without an editor-probe assumption, type and delete text in a measured Claude composer and queue mail. The daemon and attached TUI must show needs-fetch, and further clearing must not authorize a check or a notice before a prompt fetches the message.
#[spec("inbox/probe/037")]
#[test]
fn probe_037_absent_claude_holds_with_needs_fetch_until_a_prompt() {
    absent("claude", "needs_fetch");
}

/// Scenario: Without an editor-probe or head-echo assumption, type and delete text in a measured Codex composer and queue mail. The daemon and attached TUI must retain the turn-end/prompt route's cannot-type explanation, with no check or notice; an actual user prompt can still fetch the message.
#[spec("inbox/probe/037")]
#[test]
fn probe_037_absent_codex_holds_with_its_prompt_route_reason() {
    absent("codex", "cannot_type");
}

fn absent(kind: &'static str, reason: &str) {
    let f = Fixture::absent(kind, &[("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "100")]);
    f.hook_event("session_start");
    f.hook_event("idle");
    let tui = attached(&f);
    touched_empty(&f);
    let id = f.enqueue();
    wait_hold(&f, reason);
    let words: Value = serde_json::from_str(include_str!("fixtures/inbox-display.json")).unwrap();
    let expected = words["wakeHolds"][reason].as_str().unwrap();
    assert!(
        common::wait_until(WAIT, || tui.snapshot_grid().contains(expected)),
        "{}",
        tui.snapshot_grid()
    );
    let mut human = f.connect();
    human.write("\x15");
    f.expect(b"x\x7f\x15");
    f.stable(b"x\x7f\x15");
    assert!(mail_turns(&f, &id).is_empty());
    human.write("user-prompt\r");
    assert!(
        common::wait_until(WAIT, || handovers(&f)
            .iter()
            .any(|r| r.to_string().contains(&id))),
        "the native prompt route must still deliver: {:?}",
        handovers(&f)
    );
    assert!(
        !f.input().contains(&7),
        "production Absent must not send a probe"
    );
}

/// Scenario: Type and delete text in OpenCode and Pi panes with their native receipt streams registered, then queue a message. Each native stream must hand over its message without a probe or notice, and its card must not acquire a composer-check reason.
#[spec("inbox/probe/037")]
#[test]
fn probe_037_absent_opencode_and_pi_keep_native_delivery() {
    for (kind, component) in [("open_code", "opencode_plugin"), ("pi", "pi_extension")] {
        let daemon = inbox::daemon_unbound(&[]);
        let pane = inbox::pane_with(&daemon, kind, json!({"agent_type":kind,"profile":false}));
        inbox::status(&daemon, &pane, "idle", kind);
        let _human = inbox::input(&daemon, &pane, "x\x7f");
        let mut stream = inbox::receipt_stream_as(&daemon, &pane, component);
        let id = inbox::enqueue(&daemon, &pane, inbox::BODY);
        let message = inbox::receipt_handout(&mut stream, &id);
        assert_eq!(message["id"], id);
        assert!(message.to_string().contains(inbox::BODY));
        let records = inbox::attach(&daemon, json!({"op":"list-agents"}));
        let record = records["agent_records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == pane.agent)
            .unwrap();
        assert_eq!(record["inbox"]["wake"], "native", "{record}");
        assert!(record["inbox"]["wake_hold"].is_null(), "{record}");
        inbox::unchanged(&pane, Duration::from_millis(250));
        assert_eq!(inbox::bytes(&pane), b"x\x7f");
    }
}

/// Scenario: Type and delete text in a Devin pane with no editor observer, queue mail, then fetch it through the ordinary CLI receipt path. Its existing prompt route and cannot-type reason must remain available without any check or notice being typed.
#[spec("inbox/probe/037")]
#[test]
fn probe_037_absent_devin_keeps_its_prompt_fetch_route() {
    let daemon = inbox::daemon_unbound(&[("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "100")]);
    let pane = inbox::pane_with(
        &daemon,
        "devin-absent",
        json!({"agent_type":"devin","profile":false}),
    );
    inbox::status(&daemon, &pane, "idle", "devin");
    let mut announcement = inbox::request(&pane, "inbox_announce");
    announcement["component"] = json!("devin_hooks");
    announcement["component_version"] = json!("synthetic-absent");
    announcement["wake"] = json!("turn_end");
    announcement["pid"] = json!(pane.pid);
    assert_eq!(inbox::hook(&daemon, announcement)["accepted"], true);
    let _human = inbox::input(&daemon, &pane, "x\x7f");
    let id = inbox::enqueue(&daemon, &pane, inbox::BODY);
    assert!(
        common::wait_until(WAIT, || {
            let records = inbox::attach(&daemon, json!({"op":"list-agents"}));
            records["agent_records"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == pane.agent && r["inbox"]["wake_hold"] == "cannot_type")
        }),
        "Devin's prompt route must retain cannot_type"
    );
    let records = inbox::attach(&daemon, json!({"op":"list-agents"}));
    let record = records["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == pane.agent)
        .unwrap();
    assert_eq!(record["inbox"]["wake"], "turn_end", "{record}");
    assert!(
        !matches!(
            record["inbox"]["wake_hold"].as_str(),
            Some("needs_fetch" | "cannot_check")
        ),
        "{record}"
    );
    assert!(inbox::fetch(&daemon, &pane, true).to_string().contains(&id));
    assert_eq!(inbox::bytes(&pane), b"x\x7f");
}

/// Scenario: Interrupt a real notice after its line by replacing its screen with an unrecognised frame, making the attempt suspend. A user clear and, separately, a user submit must recover the doorbell, but the next automatic notice still needs a real check of the touched empty box.
#[spec("inbox/probe/047")]
#[test]
fn probe_047_submit_and_clear_recover_suspension_but_do_not_grant_empty() {
    for submit in [false, true] {
        let mut env = ENV.to_vec();
        env.extend_from_slice(&[
            ("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "200"),
            ("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_PANE", "live-segments"),
            ("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_DIR", "ring-barriers"),
        ]);
        let f = Fixture::with_env("claude", &env);
        f.hook_event("session_start");
        f.hook_event("idle");
        // The existing after-line barrier applies only after the first
        // attempt. Deliver a control before arming it for the next message.
        let control = f.enqueue();
        delivered(&f, &control);
        let root = f.daemon.home.join("ring-barriers");
        fs::create_dir_all(&root).unwrap();
        let budget = inbox::InterleaveBudget::start(&root);
        fs::write(root.join("after-line.armed"), "").unwrap();
        let original = f.enqueue();
        assert!(
            common::wait_until(inbox::wait_budget(), || root
                .join("after-line.reached")
                .exists()),
            "ordinary notice never reached its line: {:?}",
            f.input()
        );
        let first = f.input();
        assert!(
            !first.ends_with(b"\r"),
            "the deck must not have submitted its line yet"
        );
        f.emit(b"\x1b[2J\x1b[HUNRECOGNISED-SCREEN");
        fs::write(root.join("after-line.release"), "").unwrap();
        wait_hold(&f, "uncertain_input");
        f.stable(&first);
        drop(budget);
        let mut human = f.connect();
        human.write(if submit { "\r" } else { "\x15" });
        let id = if submit {
            assert!(
                common::wait_until(WAIT, || handovers(&f)
                    .iter()
                    .any(|r| r.to_string().contains(&original))),
                "the user's own submit must hand over the original mail"
            );
            f.enqueue()
        } else {
            original
        };
        let line = delivered(&f, &id);
        let mut expected = first;
        expected.extend_from_slice(if submit { b"\r" } else { b"\x15" });
        expected.extend_from_slice(format!("\x07{line}\r").as_bytes());
        f.expect(&expected);
    }
}

/// Scenario: Establish an empty observation, then ask the synthetic composer to run an actual external edit through the helper and, separately, directly without reporting; the editor preserves its empty export. No human byte intervenes, so the foreign edit alone must void the observation and require another physical check before mail is delivered.
#[spec("inbox/probe/048")]
#[test]
fn probe_048_external_edit_requires_the_next_attempt_to_check_again() {
    for reported in [true, false] {
        let f = fixture("claude");
        f.probe();
        f.wait("helper-exited-1");
        f.wait_open();
        let before = f.epoch();
        f.setting("editor-mode", "preserve");
        if !reported {
            f.flag("lost-ipc");
        }
        f.flag("external-edit-request");
        f.wait("editor-finished");
        f.wait("helper-exited-2");
        f.wait("external-edit-returned");
        f.emit(b"\x1b]0;external-edit-cycle-finished\x07");
        assert!(
            common::wait_until(WAIT, || f.epoch().0 > before.0),
            "the real external edit must be observed before mail is queued"
        );
        f.remove("lost-ipc");
        f.stable(b"\x07");
        let id = f.enqueue();
        let line = delivered(&f, &id);
        f.expect(format!("\x07\x07{line}\r").as_bytes());
    }
}

/// Scenario: Ask the synthetic composer to invoke the user's actual editor through the installed helper and directly without it, without sending any terminal input. The preserve-mode editor must receive an empty export, finish successfully and return an empty composer, establishing the fixture used by the delivery-authority test.
#[spec("inbox/probe/048")]
#[test]
fn probe_048_external_edit_fixture_preserves_empty_without_human_input() {
    for reported in [true, false] {
        let f = Fixture::start("claude");
        f.setting("editor-mode", "preserve");
        if !reported {
            f.flag("lost-ipc");
        }
        f.flag("external-edit-request");
        let editor = live::read_json(&f.path("editor-started"));
        assert_eq!(editor["hex"], "", "the actual exported file is empty");
        f.wait("editor-finished");
        f.wait("helper-exited-1");
        f.wait("external-edit-returned");
        // An OSC title is inert display output, not a mode marker or query.
        // Seeing it in the bus orders the composer's preceding redraw too.
        f.emit(b"\x1b]0;external-edit-cycle-finished\x07");
        assert!(
            common::wait_until(WAIT, || f.screen().contains("Try \"fix the bug\"")),
            "preserving the file must return an empty composer: {}",
            f.screen()
        );
        f.stable(b"");
    }
}
