#![cfg(all(feature = "e2e", target_os = "linux"))]

//! S-21: production mail decisions after the epoch becomes authoritative.
//! Uses S-20's read-only observations and ordinary scheduling requests. No
//! test operation supplies an empty observation, opens a gate or forces a ring.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/live_segments.rs"]
mod live;

use dot_agent_deck::composer_probe::{probe::BACKOFF, query_ledger::T_TRIGGER};
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
    // Core R3 §3.2 (coder-p3-q35-design-rev3.md:184): every
    // byte-carrying PtyOp touches; Left is not a D3/D3′ report exemption.
    // The production implementation is src/composer_probe/epoch.rs:15-25.
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

/// Scenario: Submit a first notice with the composer's fetch paused, type a fresh draft through the attached TUI, and independently fetch and acknowledge the queued mail while that draft remains present. Fetching and then clearing send no cleanup byte, the draft is never submitted, and a later message requires a fresh check before delivery.
#[spec("inbox/doorbell/011")]
#[test]
fn doorbell_011_a_submitted_ring_sends_nothing_after_a_new_human_draft() {
    let f = fixture("claude");
    let tui = attached(&f);
    tui.send_keys(b"1");
    tui.wait_for_string("Claude Code");
    f.flag("fetch-paused");
    let id = f.enqueue();
    assert!(common::wait_until(WAIT, || mail_turns(&f, &id).len() == 1));
    let first = mail_turns(&f, &id).remove(0);
    f.expect(format!("{first}\r").as_bytes());
    assert_eq!(f.snapshot()["inbox"]["queued"], 1);
    assert!(handovers(&f).is_empty());
    tui.send_keys(inbox::DRAFT.as_bytes());
    let typed = format!("{first}\r{}", inbox::DRAFT);
    f.expect(typed.as_bytes());
    f.stable(typed.as_bytes());
    assert!(f.screen().contains(inbox::DRAFT));
    let fetched = f.command(json!({"action":"fetch"}));
    assert_eq!(fetched["ok"], true, "{fetched}");
    assert!(
        fetched["stdout"].as_str().unwrap().contains(&id),
        "{fetched}"
    );
    assert!(common::wait_until(WAIT, || {
        let record = f.snapshot();
        record["inbox"]["queued"] == 0 && record["inbox"]["last_acked_at"].is_string()
    }));
    f.stable(typed.as_bytes());
    assert!(f.screen().contains(inbox::DRAFT));
    tui.send_keys(b"\x15");
    let cleared = format!("{typed}\x15");
    f.expect(cleared.as_bytes());
    f.stable(cleared.as_bytes());
    f.remove("fetch-paused");
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

/// Scenario: Send two Escape keys while synthetic Claude and Codex are working, then complete the physical key sequence with a typed-and-deleted character and report the turn idle. The Escapes keep the box touched, so the waiting message requires a real empty check before its notice is submitted.
#[spec("inbox/doorbell/019")]
#[test]
fn doorbell_019_claude_and_codex_interrupts_require_an_empty_check() {
    for kind in ["claude", "codex"] {
        let f = fixture(kind);
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
}

/// Scenario: Retain text typed before and after Escape keys while Claude is busy, then report Idle and queue mail; both drafts must survive a non-empty check and hold the notice until cleared and checked again. Separately, Escapes on an already-Idle Claude followed by duplicate Idle reports require an empty check before delivery.
#[spec("inbox/doorbell/019")]
#[test]
fn doorbell_019_claude_retained_draft_orders_and_idle_escapes_require_checks() {
    for (while_busy, once_idle) in [
        ("unsent\x1b\x1bx\x7f", ""),
        ("\x1b\x1bunsent", ""),
        ("", "\x1b\x1bx\x7f"),
    ] {
        let f = fixture("claude");
        f.hook_event("thinking");
        let mut human = f.connect();
        if !while_busy.is_empty() {
            human.write(while_busy);
            f.expect(while_busy.as_bytes());
        }
        f.hook_event("idle");
        if !once_idle.is_empty() {
            human.write(once_idle);
            f.expect(once_idle.as_bytes());
            f.hook_event("idle");
            f.hook_event("idle");
        }
        let id = f.enqueue();
        if !once_idle.is_empty() {
            let line = delivered(&f, &id);
            f.expect(format!("{once_idle}\x07{line}\r").as_bytes());
            continue;
        }
        f.wait("helper-exited-1");
        let checked = format!("{while_busy}\x07");
        f.stable(checked.as_bytes());
        assert!(mail_turns(&f, &id).is_empty());
        human.write("\x15");
        let line = delivered(&f, &id);
        f.expect(format!("{checked}\x15\x07{line}\r").as_bytes());
    }
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

/// Scenario: Make Codex keep a typed draft when Enter is pressed, then queue a message. The kept draft hides Codex's placeholder, so admission allows no check: the card says the box cannot be checked, the deck types nothing and the mail stays held. When the user's own Enter submits the draft, Codex's prompt hook hands the message over once, still with no deck byte.
#[spec("inbox/codex/027")]
#[test]
fn codex_027_a_draft_kept_on_enter_holds_the_doorbell() {
    let f = fixture("codex");
    f.flag("keep");
    let mut human = f.connect();
    let kept = format!("{}\r", inbox::DRAFT);
    human.write(&kept);
    f.expect(kept.as_bytes());
    let id = f.enqueue();
    // Admission's surface conjunct (R3 §2.3 item 4) needs Codex's dim
    // placeholder, which a kept draft hides: the check is refused, never
    // run, and the card names the hold (Q128b ruling (c)).
    wait_hold(&f, "cannot_check");
    f.stable(kept.as_bytes());
    assert!(mail_turns(&f, &id).is_empty());
    f.remove("keep");
    human.write("\r");
    let submitted = format!("{kept}\r");
    f.expect(submitted.as_bytes());
    assert!(
        common::wait_until(WAIT, || handovers(&f)
            .iter()
            .any(|row| row.to_string().contains(&id)
                && row.to_string().contains("TEST-ONLY-WAITING-MAIL"))),
        "the user's own prompt must hand over the held mail: {:?}",
        handovers(&f)
    );
    f.stable(submitted.as_bytes());
    assert!(mail_turns(&f, &id).is_empty(), "the deck typed no notice");
    assert_eq!(
        handovers(&f)
            .iter()
            .filter(|row| row.to_string().contains(&id))
            .count(),
        1,
        "the message must occur in one actual hand-over"
    );
}

/// Scenario: Deliver one message to an idle native Codex stand-in by its ring, then type that ring's own line back into Codex's composer, as a user would, and queue another. The deck adds no byte for 4 seconds, not even a check (the typed line hides Codex's placeholder) and the message stays held with its reason. After the user clears the composer, a check reads it empty and the deck rings once more, with a fresh token rather than the one the user typed back.
#[spec("inbox/codex/027")]
#[test]
fn codex_027_the_decks_own_line_typed_back_gets_no_byte_and_the_next_ring_a_fresh_token() {
    // Q128b ruling (iv): the second half of
    // `codex_027_a_doorbell_shaped_line_the_deck_did_not_type_gets_no_byte`,
    // through the route that still types a deck line after a user's keys:
    // an observer's check.
    let f = fixture("codex");
    let first = f.enqueue();
    let line = delivered(&f, &first);
    f.expect(format!("{line}\r").as_bytes());
    let mut human = f.connect();
    human.write(&line);
    let typed_back = format!("{line}\r{line}");
    f.expect(typed_back.as_bytes());
    let second = f.enqueue();
    wait_hold(&f, "cannot_check");
    assert!(
        !common::wait_until(Duration::from_secs(4), || f.input()
            != typed_back.as_bytes()),
        "nothing added to the deck's own line typed back: {:?}",
        f.input()
    );
    assert!(mail_turns(&f, &second).is_empty());
    human.write("\x15");
    let cleared = format!("{typed_back}\x15");
    f.expect(cleared.as_bytes());
    // The refused check backs the schedule off; advance its clock rather
    // than wait it out. It moves no epoch number and grants nothing.
    f.control(&format!("schedule-advance:{}", BACKOFF[0].as_millis()));
    let again = delivered(&f, &second);
    assert_ne!(again[..8], line[..8], "a fresh token");
    f.expect(format!("{cleared}\x07{again}\r").as_bytes());
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
    // The production schedule counts the dropped check as a lapse at
    // T_TRIGGER and makes its retry due after the first backoff step, so the
    // retry's own check is awaited for that long rather than inside
    // `delivered`'s window, which it would otherwise exhaust.
    let lapse = T_TRIGGER + BACKOFF[0] + live::WAIT;
    assert!(
        common::wait_until(lapse, || f.input().starts_with(b"x\x7f\x07\x07")),
        "the lapse retry must write its own check without another keypress: {:?}",
        f.input()
    );
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
    // The attached TUI resizes the pane, and a resize drops the replay ring
    // the emit barrier reads (`AgentBus::resize`), so emit only after it.
    let spawned = (json!(40), json!(140));
    assert!(
        common::wait_until(WAIT, || {
            let s = f.snapshot();
            (s["rows"].clone(), s["cols"].clone()) != spawned
        }),
        "the attached TUI must resize the pane before the query is emitted: {}",
        f.snapshot()
    );
    f.flag("edit-drop");
    f.emit(b"\x1b[6n");
    let mut human = f.connect();
    human.write("\x07");
    f.expect(b"\x07");
    let id = f.enqueue();
    wait_hold(&f, "cannot_check");
    let words: Value = serde_json::from_str(include_str!("fixtures/inbox-display.json")).unwrap();
    // Sidebar cards truncate notice rows. Check the diagnostic prefix here;
    // status/030 and the desktop card tests cover the full shared wording.
    let expected: String = words["wakeHolds"]["cannot_check"]
        .as_str()
        .unwrap()
        .chars()
        .take(40)
        .collect();
    assert!(
        common::wait_until(WAIT, || tui.snapshot_grid().contains(&expected)),
        "{}",
        tui.snapshot_grid()
    );
    f.stable(b"\x07");
    assert!(mail_turns(&f, &id).is_empty());
}

/// A deck whose panes' shell is bash with `BASH_ENV` naming a start-up file
/// that runs `rc`, with a `kind` stand-in in a pane: bash reads it for a
/// `-c` command, as zsh reads `.zshenv` and fish `config.fish`, and the deck
/// runs a pane line with blanks, every Codex line and a Claude Code command
/// with arguments, as `$SHELL -c` (the revcxact review's HIGH-1).
fn with_shell_rc(kind: &'static str, rc: &str) -> (Fixture, tempfile::TempDir) {
    let dir = common::harness_tempdir().expect("shell start-up file dir");
    let file = dir.path().join("rc.sh");
    fs::write(&file, rc).unwrap();
    let file = file.to_str().unwrap().to_owned();
    let mut env = ENV.to_vec();
    env.extend([("SHELL", "/bin/bash"), ("BASH_ENV", file.as_str())]);
    let f = Fixture::with_env(kind, &env);
    f.hook_event("session_start");
    f.hook_event("idle");
    (f, dir)
}

/// Scenario: Start a Codex stand-in in a deck whose shell's start-up file exports another `VISUAL` (`/bin/true`), as a zsh user's `.zshenv` would, type and delete text in its box and queue mail. The deck must type nothing, not even its check's Ctrl+G, because the editor the agent's Ctrl+G opens isn't the deck's helper: the card and the attached TUI say `cannot_check`, the log names the `Keymap(Editor)` hold, and the mail waits.
#[spec("inbox/probe/097")]
#[test]
fn probe_097_a_codex_whose_shell_sets_visual_is_held_without_a_check() {
    let (f, _rc) = with_shell_rc("codex", "export VISUAL=/bin/true\n");
    let tui = attached(&f);
    touched_empty(&f);
    let id = f.enqueue();
    wait_hold(&f, "cannot_check");
    let words: Value = serde_json::from_str(include_str!("fixtures/inbox-display.json")).unwrap();
    let expected: String = words["wakeHolds"]["cannot_check"]
        .as_str()
        .unwrap()
        .chars()
        .take(40)
        .collect();
    assert!(
        common::wait_until(WAIT, || tui.snapshot_grid().contains(&expected)),
        "{}",
        tui.snapshot_grid()
    );
    assert!(
        common::wait_until(WAIT, || f.log_text().contains("Keymap(Editor)")),
        "the hold names the editor: {}",
        f.log_text()
    );
    f.stable(b"x\x7f");
    assert!(
        !f.input().contains(&7),
        "no check's Ctrl+G: {:?}",
        f.input()
    );
    assert!(mail_turns(&f, &id).is_empty());
}

/// Scenario: The control for the test above: the same deck and Codex stand-in, the shell's start-up file exporting an unrelated variable instead. Typing and deleting text then queueing mail brings the deck's check, the box reads empty, and the notice is typed and handed over once.
#[spec("inbox/probe/097")]
#[test]
fn probe_097_a_codex_whose_shell_leaves_visual_alone_is_checked_then_rung() {
    let (f, _rc) = with_shell_rc("codex", "export DAD_UNRELATED=1\n");
    touched_empty(&f);
    let id = f.enqueue();
    let line = delivered(&f, &id);
    f.expect(format!("x\x7f\x07{line}\r").as_bytes());
    assert!(!f.log_text().contains("Keymap(Editor)"), "{}", f.log_text());
}

/// Scenario: Start a Claude Code stand-in, whose pane command has arguments, in a deck whose shell's start-up file exports another `VISUAL` (`/bin/true`), type and delete text in its box and queue mail. The deck must type nothing, not even its check's Ctrl+G, because the editor Claude Code's Ctrl+G opens isn't the deck's helper: the card says `cannot_check`, the log names the `Keymap(Editor)` hold, and the mail waits.
#[spec("inbox/probe/097")]
#[test]
fn probe_097_a_claude_whose_shell_sets_visual_is_held_without_a_check() {
    let (f, _rc) = with_shell_rc("claude", "export VISUAL=/bin/true\n");
    touched_empty(&f);
    let id = f.enqueue();
    wait_hold(&f, "cannot_check");
    assert!(
        common::wait_until(WAIT, || f.log_text().contains("Keymap(Editor)")),
        "the hold names the editor: {}",
        f.log_text()
    );
    f.stable(b"x\x7f");
    assert!(
        !f.input().contains(&7),
        "no check's Ctrl+G: {:?}",
        f.input()
    );
    assert!(mail_turns(&f, &id).is_empty());
}

/// Scenario: The Claude Code control: the same deck and stand-in with the start-up file exporting an unrelated variable. Typing and deleting text then queueing mail brings the deck's check, and the notice is typed and its mail fetched once.
#[spec("inbox/probe/097")]
#[test]
fn probe_097_a_claude_whose_shell_leaves_visual_alone_is_checked_then_rung() {
    let (f, _rc) = with_shell_rc("claude", "export DAD_UNRELATED=1\n");
    touched_empty(&f);
    let id = f.enqueue();
    let line = delivered(&f, &id);
    f.expect(format!("x\x7f\x07{line}\r").as_bytes());
    assert!(!f.log_text().contains("Keymap(Editor)"), "{}", f.log_text());
}

/// Scenario: Start a Codex stand-in whose `CODEX_HOME` is a `codex` folder inside the pane's working directory, where Codex's sandbox may write, type and delete text in its box and queue mail. The deck must type nothing, not even its check's Ctrl+G, and run no helper: the card and the attached TUI say `cannot_check`, the log names the `Keymap(CodexHome)` hold, and the mail waits. (Every other live Codex here uses the working directory's own `.codex`, the control.)
#[spec("inbox/probe/098")]
#[test]
fn probe_098_a_codex_home_inside_the_working_directory_is_held_without_a_check() {
    let f = live::Fixture::with_codex_home("codex", ENV, "codex");
    f.hook_event("session_start");
    f.hook_event("idle");
    let tui = attached(&f);
    touched_empty(&f);
    let id = f.enqueue();
    wait_hold(&f, "cannot_check");
    let words: Value = serde_json::from_str(include_str!("fixtures/inbox-display.json")).unwrap();
    let expected: String = words["wakeHolds"]["cannot_check"]
        .as_str()
        .unwrap()
        .chars()
        .take(40)
        .collect();
    assert!(
        common::wait_until(WAIT, || tui.snapshot_grid().contains(&expected)),
        "{}",
        tui.snapshot_grid()
    );
    assert!(
        common::wait_until(WAIT, || f.log_text().contains("Keymap(CodexHome)")),
        "the hold names Codex's home: {}",
        f.log_text()
    );
    f.stable(b"x\x7f");
    assert!(
        !f.input().contains(&7),
        "no check's Ctrl+G: {:?}",
        f.input()
    );
    assert!(!f.path("helper-exited-1").exists(), "no helper ran");
    assert!(mail_turns(&f, &id).is_empty());
}

/// A stand-in of `kind` started with `files` written into its HOME (also its
/// working directory) before the spawn, with its session started and idle.
fn with_home_files(kind: &'static str, files: &[(&str, &str)]) -> Fixture {
    let f = Fixture::with_home_files(kind, ENV, files);
    f.hook_event("session_start");
    f.hook_event("idle");
    f
}

/// What the held tests below assert once mail is queued behind a touched and
/// cleared box: `cannot_check` on the card (and, with `tui`, in an attached
/// TUI), a `Keymap(Editor)` hold in the log, nothing typed (not even a
/// check's Ctrl+G) and the mail still waiting. The Claude Code stand-in runs
/// without a TUI, as `probe_097`'s does: the TUI's resize leaves its frame
/// unreadable to admission (`Surface`), which would say `cannot_check`
/// whatever the hold.
fn held_for_the_editor(f: &Fixture, tui: bool) {
    let tui = tui.then(|| attached(f));
    touched_empty(f);
    let id = f.enqueue();
    wait_hold(f, "cannot_check");
    if let Some(tui) = tui {
        let words: Value =
            serde_json::from_str(include_str!("fixtures/inbox-display.json")).unwrap();
        let expected: String = words["wakeHolds"]["cannot_check"]
            .as_str()
            .unwrap()
            .chars()
            .take(40)
            .collect();
        assert!(
            common::wait_until(WAIT, || tui.snapshot_grid().contains(&expected)),
            "{}",
            tui.snapshot_grid()
        );
    }
    assert!(
        common::wait_until(WAIT, || f.log_text().contains("Keymap(Editor)")),
        "the hold names the editor: {}",
        f.log_text()
    );
    f.stable(b"x\x7f");
    assert!(
        !f.input().contains(&7),
        "no check's Ctrl+G: {:?}",
        f.input()
    );
    assert!(mail_turns(f, &id).is_empty());
}

/// Scenario: Start a Claude Code stand-in whose `~/.claude/settings.json` sets `VISUAL` in its `env` map, which Claude Code copies into its own environment as it starts, then type and delete text in its box and queue mail. The deck must type nothing, not even its check's Ctrl+G: the card says `cannot_check`, the log names the `Keymap(Editor)` hold, and the mail waits.
#[spec("inbox/probe/099")]
#[test]
fn probe_099_a_claude_whose_settings_env_sets_visual_is_held_without_a_check() {
    let f = with_home_files(
        "claude",
        &[(
            ".claude/settings.json",
            r#"{"env": {"VISUAL": "/bin/true"}}"#,
        )],
    );
    held_for_the_editor(&f, false);
}

/// Scenario: The control for the test above: the same stand-in with a `~/.claude/settings.json` that has no `env` map, and a comment the deck reads past (Claude Code 2.1.318 itself refuses a commented file and skips it). Typing and deleting text then queueing mail brings the deck's check, and the notice is typed and its mail fetched once.
#[spec("inbox/probe/099")]
#[test]
fn probe_099_a_claude_whose_settings_name_no_editor_is_checked_then_rung() {
    let f = with_home_files(
        "claude",
        &[(
            ".claude/settings.json",
            "{\n  // a person's own note\n  \"permissions\": {\"allow\": []}\n}\n",
        )],
    );
    touched_empty(&f);
    let id = f.enqueue();
    let line = delivered(&f, &id);
    f.expect(format!("x\x7f\x07{line}\r").as_bytes());
    assert!(!f.log_text().contains("Keymap(Editor)"), "{}", f.log_text());
}

/// Scenario: Start a Claude Code stand-in whose `~/.claude/settings.json` sets the deck's own `DOT_AGENT_DECK_EDITOR_GEN` in its `env` map, which Claude Code would copy into its environment and so point the deck's editor helper at a check that was never armed, then type and delete text in its box and queue mail. The deck must type nothing, not even its check's Ctrl+G: the card says `cannot_check`, the log names the `Keymap(Editor)` hold, and the mail waits.
#[spec("inbox/probe/099")]
#[test]
fn probe_099_a_claude_whose_settings_env_sets_a_deck_variable_is_held_without_a_check() {
    let f = with_home_files(
        "claude",
        &[(
            ".claude/settings.json",
            r#"{"env": {"DOT_AGENT_DECK_EDITOR_GEN": "1"}}"#,
        )],
    );
    held_for_the_editor(&f, false);
}

/// Scenario: Start a Codex stand-in whose home's `.env` file sets `VISUAL`, which Codex loads into its own environment as it starts, then type and delete text in its box and queue mail. The deck must type nothing, not even its check's Ctrl+G: the card and the attached TUI say `cannot_check`, the log names the `Keymap(Editor)` hold, and the mail waits.
#[spec("inbox/probe/100")]
#[test]
fn probe_100_a_codex_whose_home_env_file_sets_visual_is_held_without_a_check() {
    let f = with_home_files("codex", &[(".codex/.env", "export VISUAL=/bin/true\n")]);
    held_for_the_editor(&f, true);
}

/// Scenario: The control for the test above: the same Codex stand-in with a home `.env` file that sets only an unrelated variable. Typing and deleting text then queueing mail brings the deck's check, the box reads empty, and the notice is typed and handed over once.
#[spec("inbox/probe/100")]
#[test]
fn probe_100_a_codex_whose_home_env_file_names_no_editor_is_checked_then_rung() {
    let f = with_home_files("codex", &[(".codex/.env", "DAD_UNRELATED=1\n")]);
    touched_empty(&f);
    let id = f.enqueue();
    let line = delivered(&f, &id);
    f.expect(format!("x\x7f\x07{line}\r").as_bytes());
    assert!(!f.log_text().contains("Keymap(Editor)"), "{}", f.log_text());
}

/// Scenario: Start a Codex stand-in whose home's `.env` file sets the deck's own `DOT_AGENT_DECK_EDITOR_GEN`, which Codex would load into its environment and so point the deck's editor helper at a check that was never armed, then type and delete text in its box and queue mail. The deck must type nothing, not even its check's Ctrl+G: the card and the attached TUI say `cannot_check`, the log names the `Keymap(Editor)` hold, and the mail waits.
#[spec("inbox/probe/100")]
#[test]
fn probe_100_a_codex_whose_home_env_file_sets_a_deck_variable_is_held_without_a_check() {
    let f = with_home_files("codex", &[(".codex/.env", "DOT_AGENT_DECK_EDITOR_GEN=1\n")]);
    held_for_the_editor(&f, true);
}

/// Scenario: In a deck whose Claude Code doorbell is turned off (`DOT_AGENT_DECK_DOORBELL_CLAUDE=0`), so the Claude composer gets no helper and no check, type and delete text in it and queue mail. The daemon and attached TUI must show the cannot-type hold, and further clearing must not authorize a check or a notice before a prompt fetches the message.
#[spec("inbox/probe/037")]
#[test]
fn probe_037_turned_off_claude_holds_with_cannot_type_until_a_prompt() {
    // Deliberately waiter-less, including after Q35-25 installs the native
    // waiter: this control's no-waiter precondition must remain explicit.
    absent(
        "claude",
        &[(dot_agent_deck::doorbell_switch::CLAUDE_ENV, "0")],
        "cannot_type",
    );
}

/// Scenario: Start Claude Code through a launcher (the pane runs it via `env`), so the deck installs no helper and can't check its box, with no native waiter. Type and delete text in its composer and queue mail: the daemon and attached TUI must show the needs-fetch hold, and further clearing must not authorize a check or a notice before a prompt fetches the message.
#[spec("inbox/probe/037")]
#[test]
fn probe_037_launcher_claude_holds_with_needs_fetch_until_a_prompt() {
    // Deliberately waiter-less, as the turned-off case above.
    absent_holds(
        Fixture::absent_through(
            "claude",
            "env",
            &[("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "100")],
        ),
        "needs_fetch",
    );
}

/// Scenario: In a deck whose Codex doorbell is turned off (`DOT_AGENT_DECK_DOORBELL_CODEX=0`), so the Codex composer gets no helper and no check (since the Codex activation, that is what leaves a direct Codex unchecked), type and delete text in it and queue mail. The daemon and attached TUI must retain the turn-end/prompt route's cannot-type explanation, with no check or notice; an actual user prompt can still fetch the message.
#[spec("inbox/probe/037")]
#[test]
fn probe_037_turned_off_codex_holds_with_its_prompt_route_reason() {
    absent(
        "codex",
        &[(dot_agent_deck::doorbell_switch::CODEX_ENV, "0")],
        "cannot_type",
    );
}

fn absent(kind: &'static str, extra: &[(&str, &str)], reason: &str) {
    let mut env = vec![("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "100")];
    env.extend_from_slice(extra);
    absent_holds(Fixture::absent(kind, &env), reason);
}

fn absent_holds(f: Fixture, reason: &str) {
    f.hook_event("session_start");
    f.hook_event("idle");
    let tui = attached(&f);
    touched_empty(&f);
    let id = f.enqueue();
    wait_hold(&f, reason);
    let words: Value = serde_json::from_str(include_str!("fixtures/inbox-display.json")).unwrap();
    let expected: String = words["wakeHolds"][reason]
        .as_str()
        .unwrap()
        .chars()
        .take(40)
        .collect();
    assert!(
        common::wait_until(WAIT, || tui.snapshot_grid().contains(&expected)),
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

/// Scenario: Establish an empty observation, then ask the synthetic composer to run an actual external editor directly, without the helper, so nothing reports it; the editor preserves its empty export. No human byte intervenes, so the foreign edit alone must void the observation and require another physical check before mail is delivered.
#[spec("inbox/probe/048")]
#[test]
fn probe_048_external_edit_requires_the_next_attempt_to_check_again() {
    // Only the unreported edit (Q128b ruling (d)). An edit run through the
    // helper is an `Unarmed` editor run, which under `EC_POLICY = Hold`
    // opens the writer scope until retirement (amendment 6 §1.3), so no
    // next attempt could check at all: a different scenario from this
    // test's claim. Both forms moving the epoch is S-20's
    // `probe_048_unreported_and_fast_external_edits_move_the_foreign_epoch`.
    let f = fixture("claude");
    f.probe();
    f.wait("helper-exited-1");
    f.wait_open();
    let before = f.epoch();
    assert_eq!(
        before.1, before.0,
        "the initial check must establish a valid empty observation: {before:?}"
    );
    f.setting("editor-mode", "preserve");
    f.flag("lost-ipc");
    f.flag("external-edit-request");
    f.wait("editor-finished");
    f.wait("helper-exited-2");
    f.wait("external-edit-returned");
    f.emit(b"\x1b]0;external-edit-cycle-finished\x07");
    assert!(
        common::wait_until(WAIT, || f.epoch().0 > before.0),
        "the real external edit must be observed before mail is queued"
    );
    let after = f.epoch();
    assert_eq!(
        after.1, before.1,
        "external editing must leave the earlier observation behind: {before:?} -> {after:?}"
    );
    assert_ne!(
        after.0, after.1,
        "the external edit must invalidate that observation: {after:?}"
    );
    f.remove("lost-ipc");
    f.stable(b"\x07");
    let id = f.enqueue();
    let line = delivered(&f, &id);
    f.expect(format!("\x07\x07{line}\r").as_bytes());
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
        // epoch::output_notes (src/composer_probe/epoch.rs:280-288) accepts
        // only OSC 10/11 queries and ignores OSC 0. PrivateModeScanner
        // (src/terminal_modes.rs:98-100) skips ESC ] as a non-CSI prefix.
        // This literal title contains no embedded escape/mode directive.
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
