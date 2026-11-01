#![cfg(all(feature = "e2e", target_os = "linux"))]

//! S-20: physical gate/release and epoch contracts, before S-21 makes those
//! numbers authoritative for mail. No assertion here requires mail delivery.
//!
//! Assumed e2e-only additions to `e2e-composer-control`:
//! * `epoch`: read-only `composer_epoch: {epoch, observed, last_foreign}`.
//! * `observer-status`: read-only `composer_observer: {scope}` (`pending`,
//!   `closed`, `open`), used as an observation barrier for the real scanner.
//! * `probe-request`: submit a normal first probe through the same scheduling,
//!   writer-thread admission, arm, claim and undo path as the live tick. Reply
//!   after refusal or the probe's physical write, not after helper completion;
//!   a refusal is `ok:true` (the operation ran), with no arm/fence/epoch move.
//!   `probe-request:retry` and `probe-request:lapse` ask the corresponding
//!   production paths, with their normal eligibility checks. First and retry
//!   requests consult the generation's persistent production `Schedule`: a
//!   due attempt refused by admission counts as `Schedule::failed` (initial
//!   backoff 30 s). Before it is due, either spelling returns
//!   `composer_probe: {outcome: "not_due"}` without scheduling a job, changing
//!   that schedule, publishing an arm, closing a fence or moving the epoch.
//!   Due requests report `outcome: "refused"` or `"written"` in that object.
//!   The lapse spelling keeps the production Claude-only, no-held-input
//!   exception; it is not a way to retry an admission refusal early. Requests
//!   neither create a fresh Schedule nor reset its failure accounting. These never
//!   manufacture a record, ignore permission, or act as `publish`/`fence`.
//! * `DOT_AGENT_DECK_E2E_PROBE_GATE=<single component under fixture HOME>`:
//!   a bounded `scheduled` barrier before the writer revalidates admission;
//!   write `scheduled.arrived`, wait for `scheduled.release`, and record
//!   `scheduled.timed-out` on expiry. It is enabled only when `<root>/phase`
//!   says `scheduled`; Drop releases it. It grants no admission or release.
//!
//! No release-authorizing output state is injected. F2 and the real helper
//! produce decisions, marker pairs, actual editor work and process exits.
//! Codex rows also assume the existing e2e assumption can bind this direct
//! native stand-in as Armable; the production launcher/interpreter binding
//! work stays in Q35-22. A missing e2e binding must be reported explicitly.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/live_segments.rs"]
mod live;

use live::{Fixture, WAIT};
use serde_json::{Value, json};
use spec::spec;
use std::fs;
use std::time::{Duration, Instant};

ctor::declarative::ctor! {
    // The fixture child is selected by its private environment before libtest
    // parses Codex's injected arguments. The normal test process returns.
    #[ctor(unsafe)]
    fn native_fixture_entry() {
        live::native_entry();
    }
}

fn wait_span(span: Duration) {
    let start = Instant::now();
    assert!(common::wait_until(
        span + common::load_scaled(WAIT),
        || start.elapsed() >= span
    ));
}

fn held_words() -> String {
    let words: Value = serde_json::from_str(include_str!("fixtures/inbox-display.json")).unwrap();
    words["inputHeld"]["checking"].as_str().unwrap().into()
}

fn has_held(snapshot: &Value) -> bool {
    snapshot.get("input_held").is_some_and(|v| !v.is_null())
        || snapshot
            .as_object()
            .is_some_and(|o| o.values().any(has_held))
}

/// Scenario: Launch the reviewed F2 composer under a native synthetic agent and type a draft followed by Ctrl+G without a deck probe. The real installed helper must run the user's editor with that draft and all three terminal descriptors, then the composer must accept following keys.
#[spec("inbox/probe/027")]
#[test]
fn probe_027_native_f2_control_runs_the_real_unarmed_editor() {
    let f = Fixture::start("claude");
    let mut human = f.connect();
    human.write("fixture-draft");
    f.expect(b"fixture-draft");
    human.write("\x07");
    let editor = live::read_json(&f.path("editor-started"));
    assert_eq!(
        editor["hex"], "666978747572652d6472616674",
        "real helper must pass the unarmed draft to the editor"
    );
    assert_eq!(editor["tty"], json!([true, true, true]));
    f.wait("helper-exited");
    human.write("AFTER");
    f.expect(b"fixture-draft\x07AFTER");
    f.stable(b"fixture-draft\x07AFTER");
}

/// Scenario: For each injected same-read, flush and dispatch-window key-loss mechanism, request a real check and type distinct keys in its delayed dispatch window. After the helper settles, every key must arrive once and in order, and a foreign acceptance during the check must keep the empty observation from clearing the epoch.
#[spec("inbox/probe/026")]
#[test]
fn probe_026_delayed_checks_preserve_typing_under_each_loss_mechanism() {
    for mechanism in ["H-chunk", "H-flush", "H-ignore"] {
        let f = Fixture::start("claude");
        f.setting("edit-mechanism", mechanism);
        f.flag("edit-delay");
        let before = f.epoch();
        f.probe();
        f.wait("edit-dispatched");
        let mut human = f.connect();
        human.write("FIRST");
        human.write("SECOND");
        f.wait_held();
        f.wait("helper-exited");
        f.expect(b"\x07FIRSTSECOND");
        f.stable(b"\x07FIRSTSECOND");
        let after = f.epoch();
        assert!(
            after.0 > before.0 && after.2 > before.2,
            "foreign acceptance must move the epoch: {before:?} -> {after:?}"
        );
        assert_eq!(
            after.1, before.1,
            "an observation cannot clear a check that accepted foreign input"
        );
    }
}

/// Scenario: Check an untouched empty box with no intervening input, then read the numbers twice and try a stale generation. A supported export chain permits the matching empty observation; an unsupported chain must report Unreadable and leave observed unchanged, and fails when strict verification is required. This empty control queues no producer, so it does not establish retention of pending typing or mail on an unsupported export.
#[spec("inbox/probe/026")]
#[test]
fn probe_026_empty_settlement_and_read_only_epoch_control() {
    // .config/nextest.toml selects this exact function name to show which
    // export-chain branch ran; update that override if this test is renamed.
    let f = Fixture::start("claude");
    f.flag("hold-return");
    let before = f.epoch();
    assert_eq!(f.epoch(), before, "reading the epoch must be inert");
    let stale = f.control_for(f.generation + 1, "epoch");
    assert_ne!(
        stale["ok"], true,
        "read-only still binds the current generation: {stale}"
    );
    f.probe();
    f.wait("helper-exited");
    let helper = live::read_json(&f.path("helper-started"));
    let export = std::path::Path::new(helper["path"].as_str().unwrap());
    let verified = dot_agent_deck::composer_probe::path_rule::verify_export(
        &mut dot_agent_deck::platform::fsperm::fs_view::LinuxFsView,
        export,
        // SAFETY: getuid has no failure mode.
        unsafe { libc::getuid() },
    );
    let unsupported = verified.err();
    eprintln!(
        "probe_026 export-chain branch: {} ({unsupported:?})",
        if unsupported.is_some() {
            "unsupported / Unreadable"
        } else {
            "supported / Empty"
        }
    );
    f.flag("resume-go");
    f.expect(b"\x07");
    if let Some(refusal) = unsupported {
        let trace = fs::read_to_string(f.path("helper-trace")).unwrap();
        assert!(
            trace.contains("Unreadable"),
            "refused {refusal:?}, helper trace:\n{trace}"
        );
        f.stable(b"\x07");
        let after = f.epoch();
        assert_eq!(
            after,
            (before.0 + 1, before.1, before.2),
            "an unreadable export must not claim an empty box"
        );
        assert!(
            std::env::var("DOT_AGENT_DECK_REQUIRE_REAL_E2E").as_deref() != Ok("1"),
            "UNVERIFIED: Empty settlement requires a supported export chain: {refusal:?}"
        );
    } else {
        assert!(
            common::wait_until(WAIT, || {
                let e = f.epoch();
                e.0 == before.0 + 1 && e.1 == e.0 && e.2 == before.2
            }),
            "the matching empty observation should cover exactly its own probe byte"
        );
    }
}

/// Scenario: Hold each sampled invoking Ctrl+G form behind a probe whose helper has exited but whose return is delayed, then type a line for the user's editor in the same frame. Completing the first segment must dispatch the user's candidate separately and deliver the suffix to the actual running editor, including split-read keyboard forms.
#[spec("inbox/probe/027")]
#[test]
fn probe_027_human_candidates_dispatch_alone_then_reach_the_user_editor() {
    for form in ["\x07", "\x1b[103;5u", "\x1b[27;5;103~", "\x1b[71;5u"] {
        for split in [false, true] {
            let f = Fixture::start("claude");
            f.flag("hold-return");
            f.setting("editor-mode", "read");
            f.setting("edit-mechanism", "H-chunk");
            f.probe();
            f.wait("helper-exited");
            let mut human = f.connect();
            if split && form.len() > 1 {
                human.write(&form[..2]);
                human.write(&format!("{}USER-EDITOR\n", &form[2..]));
            } else {
                human.write(&format!("{form}USER-EDITOR\n"));
            }
            f.wait_held();
            f.flag("resume-go");
            f.wait("editor-input");
            assert_eq!(
                fs::read(f.path("editor-input")).unwrap(),
                b"USER-EDITOR\n",
                "{form:?}, split={split}: the intended reader is the user's editor"
            );
            f.flag("editor-go");
        }
    }
}

/// Scenario: Make the composer drop every candidate, start a check and type keys behind it. The keys and held-input explanation must remain pending beyond the display timer, and replacing the generation must discard them without sending them to its successor.
#[spec("inbox/probe/028")]
#[test]
fn probe_028_an_ignored_candidate_holds_until_generation_replacement() {
    let mut f = Fixture::start("claude");
    f.flag("edit-drop");
    f.probe();
    let mut human = f.connect();
    human.write("HELD-UNTIL-RESPAWN");
    f.wait_held();
    wait_span(Duration::from_millis(350));
    assert!(
        has_held(&f.snapshot()),
        "the daemon must publish held input on the real agent snapshot"
    );
    f.stable(b"\x07");
    f.replace();
    f.stable(b"");
}

/// Scenario: On a synthetic Codex generation, stop the composer after the real helper exits and type during its check. Exit and the disabled-paste state alone must leave the keys held; the matching re-enable permits them once, after the editor interval's flush.
#[spec("inbox/probe/029")]
#[test]
fn probe_029_codex_requires_the_complete_matched_return() {
    let f = Fixture::start("codex");
    f.flag("hold-return");
    f.probe();
    f.wait("helper-exited");
    let mut human = f.connect();
    human.write("AFTER-CODEX-E1");
    f.wait_held();
    f.stable(b"\x07");
    f.flag("resume-go");
    f.expect(b"\x07AFTER-CODEX-E1");
    f.stable(b"\x07AFTER-CODEX-E1");
}

/// Scenario: Make the Codex stand-in emit its re-enable before a witnessed, still-pending real terminal input flush, then type during that window. The mode transition must release no keys into the flush; after the flush and the composer's next redraw marker, every typed key must arrive once.
#[spec("inbox/probe/051")]
#[test]
fn probe_051_codex_enable_before_a_gated_real_flush_cannot_release_typing() {
    let f = Fixture::start("codex");
    f.flag("gate-flush");
    f.probe();
    f.wait("flush-arrived");
    let mut human = f.connect();
    human.write("AFTER-THE-REAL-FLUSH");
    f.wait_held();
    f.stable(b"\x07");
    f.flag("flush-go");
    f.expect(b"\x07AFTER-THE-REAL-FLUSH");
    f.stable(b"\x07AFTER-THE-REAL-FLUSH");
}

/// Scenario: Ignore a Claude or Codex check once, let it lapse, and request a retry with and without held human input. Only Claude with nothing held may write a second probe and run its real helper; Codex and held-key cases retain the original unresolved check.
#[spec("inbox/probe/030")]
#[test]
fn probe_030_lapse_retry_depends_on_whether_keys_wait_behind_it() {
    for (kind, held) in [
        ("claude", false),
        ("claude", true),
        ("codex", false),
        ("codex", true),
    ] {
        let f = Fixture::start(kind);
        f.flag("edit-ignore-next");
        f.probe();
        f.expect(b"\x07");
        let mut human = f.connect();
        if held {
            human.write("WAITING");
            f.wait_held();
        }
        wait_span(
            dot_agent_deck::composer_probe::query_ledger::T_TRIGGER + Duration::from_millis(50),
        );
        let reply = f.control("probe-request:lapse");
        if held || kind == "codex" {
            assert_ne!(
                reply["composer_probe"]["outcome"], "written",
                "{kind}, held={held}: {reply}"
            );
            f.stable(b"\x07");
            assert!(
                !f.path("helper-started").exists(),
                "{kind}, held={held}: a refused lapse must not invoke the helper"
            );
        } else {
            f.wait("helper-exited");
            f.expect(b"\x07\x07");
            f.stable(b"\x07\x07");
        }
    }
}

/// Scenario: Drop the check byte in a Codex composer and submit human input behind the unresolved segment. It must remain absent from the terminal until generation retirement, and no withheld byte may appear in the replacement pane.
#[spec("inbox/probe/031")]
#[test]
fn probe_031_dropped_trigger_holds_human_input_through_retirement() {
    let mut f = Fixture::start("codex");
    f.flag("edit-drop");
    f.probe();
    let mut human = f.connect();
    human.write("HUMAN-NEVER-WRITTEN");
    f.wait_held();
    // The answer-specific companion below uses the existing question fixture;
    // this branch pins CX-H's physical human-input outcome independently.
    f.stable(b"\x07");
    f.replace();
    f.stable(b"");
}

/// Scenario: Enter a native candidate while a CPR is outstanding, request a first probe and an immediate retry, then answer CPR and wait through the production retry backoff. Even the due retry must refuse without writing or publishing an arm because the candidate was latched during CPR; a fresh generation admits the positive control.
#[spec("inbox/probe/032")]
#[test]
fn probe_032_native_backlog_latch_refuses_first_and_retry_then_respawn_recovers() {
    let mut f = Fixture::start("claude");
    f.flag("edit-drop");
    f.emit(b"\x1b[6n");
    let mut human = f.connect();
    human.write("\x07");
    f.expect(b"\x07");
    let page = f.page();
    let first = f.probe();
    assert_eq!(first["composer_probe"]["outcome"], "refused", "{first}");
    let refused_at = Instant::now();
    f.control("probe-request:retry");
    f.stable(b"\x07");
    assert_eq!(
        f.page(),
        page,
        "refused live attempts must not publish an arm"
    );
    human.write("\x1b[1;1R");
    f.expect(b"\x07\x1b[1;1R");
    wait_span(dot_agent_deck::composer_probe::query_ledger::T_TRIGGER + Duration::from_millis(50));
    f.probe();
    f.stable(b"\x07\x1b[1;1R");
    wait_span(
        (dot_agent_deck::composer_probe::probe::BACKOFF[0] + Duration::from_millis(100))
            .saturating_sub(refused_at.elapsed()),
    );
    let due = f.control("probe-request:retry");
    assert_eq!(
        due["composer_probe"]["outcome"], "refused",
        "a due retry must reach admission and find the CPR latch: {due}"
    );
    f.stable(b"\x07\x1b[1;1R");
    assert_eq!(f.page(), page, "the due refusal must publish no arm");
    f.replace();
    f.probe();
    f.wait("helper-exited");
    f.expect(b"\x07");
}

/// Scenario: Fill the outstanding query history with and without a native candidate and request a first probe. The native hazard must refuse that attempt without a page publication; the saturated ledger without a native candidate must still allow the control probe.
#[spec("inbox/probe/032")]
#[test]
fn probe_032_overflow_refuses_only_when_native_history_is_hazardous() {
    for native in [false, true] {
        let f = Fixture::start("claude");
        f.flag("edit-drop");
        let queries =
            b"\x1b[c".repeat(dot_agent_deck::composer_probe::query_ledger::LEDGER_SLOTS + 2);
        f.emit(&queries);
        let mut human = f.connect();
        if native {
            human.write("\x07");
            f.expect(b"\x07");
        }
        let page = f.page();
        f.probe();
        if native {
            f.stable(b"\x07");
            assert_eq!(f.page(), page);
        } else {
            f.expect(b"\x07");
            assert!(
                f.page().seg_seq > 0,
                "ledger saturation alone must still admit the probe"
            );
        }
    }
}

/// Scenario: Overflow the physical native-candidate FIFO while a CPR is pending, then let its quiet period pass and request a new probe. Neither the latched entries nor the counted overflow may disappear on a timer, so no probe byte or arm publication is allowed.
#[spec("inbox/probe/032")]
#[test]
fn probe_032_counted_native_fifo_overflow_never_ages_into_permission() {
    let f = Fixture::start("claude");
    f.flag("edit-drop");
    f.emit(b"\x1b[6n");
    let keys = "\x07".repeat(dot_agent_deck::composer_probe::query_ledger::HAZARD_SLOTS + 2);
    let mut human = f.connect();
    human.write(&keys);
    f.expect(keys.as_bytes());
    let page = f.page();
    wait_span(dot_agent_deck::composer_probe::query_ledger::T_TRIGGER + Duration::from_millis(50));
    f.probe();
    f.stable(keys.as_bytes());
    assert_eq!(
        f.page(),
        page,
        "counted native overflow must not grant an arm"
    );
}

/// Scenario: With the real TUI attached to the sandbox daemon, pause the check's real helper before its decision, type and watch the actual card. It must show the shared checking explanation while keys are held, clear it after the helper resumes and releases them, and show the shared not-delivered words if a second hold is retired.
#[spec("inbox/probe/046")]
#[test]
fn probe_046_attached_dashboard_shows_clears_and_retires_held_typing() {
    let mut f = Fixture::start("claude");
    f.hook_event("session_start");
    let tui = common::TuiDeck::builder()
        .with_pty_size(200, 50)
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            f.daemon.attach_socket.to_string_lossy(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            f.daemon.hook_socket.to_string_lossy(),
        )
        .launch_with_fixture("minimal");
    let pause = f.pause_helper();
    f.flag("hold-return");
    f.probe();
    f.wait("helper-started");
    let mut human = f.connect();
    human.write("VISIBLE-HELD");
    f.wait_held();
    // The card is narrower than the full sentence. Check its identifying
    // clause, which fits the card, rather than requiring an unwrapped tail.
    let checking = held_words().split(';').next().unwrap().to_owned();
    assert!(
        common::wait_until(WAIT, || tui.snapshot_grid().contains(&checking)),
        "attached card never showed held-input words; gate held={}, snapshot input_held={:?}:\n{}",
        f.held(),
        f.snapshot().get("input_held"),
        tui.snapshot_grid()
    );
    f.remove("pause-helper");
    pause.release();
    f.flag("resume-go");
    f.expect(b"\x07VISIBLE-HELD");
    assert!(
        common::wait_until(WAIT, || !tui.snapshot_grid().contains(&checking)),
        "settled hold stayed on attached card:\n{}",
        tui.snapshot_grid()
    );
    f.remove("hold-return");
    f.flag("edit-drop");
    f.probe();
    human.write("RETIRED-VISIBLE");
    f.wait_held();
    f.replace();
    let words: Value = serde_json::from_str(include_str!("fixtures/inbox-display.json")).unwrap();
    let retired = words["notDelivered"].as_str().unwrap();
    assert!(
        common::wait_until(WAIT, || {
            let grid = tui.snapshot_grid();
            grid.contains(retired) && grid.contains("typing")
        }),
        "retired held keys must be reported in attached TUI:\n{}",
        tui.snapshot_grid()
    );
    f.stable(b"");
}

/// Scenario: Invoke the user's editor through the installed helper or directly without that helper and read the epoch around its output pair. In the fast direct case, hold observer passes until the editor and native parent have acknowledged exit and return; the unmatched external edit must still move the foreign epoch once without an empty observation clearing it.
#[spec("inbox/probe/048")]
#[test]
fn probe_048_unreported_and_fast_external_edits_move_the_foreign_epoch() {
    for (reported, fast) in [(true, false), (false, false), (false, true)] {
        let f = Fixture::start("claude");
        if !reported {
            f.flag("lost-ipc");
        }
        let held_passes = fast.then(|| {
            f.control("observer-hold");
            // An already-running pass may finish after hold is requested.
            wait_span(Duration::from_millis(200));
            f.control("observer-status")["composer_observer"]["passes"]
                .as_u64()
                .unwrap()
        });
        f.flag("hold-return");
        let before = f.epoch();
        let mut human = f.connect();
        human.write("\x07");
        f.wait("editor-started");
        if !fast {
            wait_span(Duration::from_millis(100));
        }
        f.wait("helper-exited");
        f.flag("resume-go");
        f.wait("composer-return-1");
        if let Some(passes) = held_passes {
            let status = f.control("observer-status");
            assert_eq!(
                status["composer_observer"]["passes"], passes,
                "the editor exited before an observer pass: {status}"
            );
            f.control("observer-go");
        }
        assert!(
            common::wait_until(WAIT, || f.epoch().0 >= before.0 + 2),
            "native key and unmatched external edit must each count, including early exit"
        );
        let after = f.epoch();
        assert_eq!(
            after.0,
            before.0 + 2,
            "one native input plus one ExternalEdit producer"
        );
        assert_eq!(after.2, after.0, "external edit is foreign");
        assert_eq!(
            after.1, before.1,
            "an unreported editor never grants an empty observation"
        );
        f.stable(b"\x07");
    }
}

/// Scenario: Emit a complete mode pair before a Codex check, then incomplete or malformed markers after its real helper exits while its return is delayed. Those bytes must grant no release; the original composer's matching re-enable must let the keys through once.
#[spec("inbox/probe/049")]
#[test]
fn probe_049_mode_observations_and_malformed_pairs_are_not_release_authority() {
    for noise in [
        b"already-disabled".as_slice(),
        b"\x1b[?2004l",
        b"\x1b[?2004l\x1b[?2004;h",
    ] {
        let f = Fixture::start("codex");
        // Pre-trigger markers cannot be re-used for this segment.
        f.emit(b"\x1b[?2004l\x1b[?2004h");
        f.flag("hold-return");
        f.probe();
        f.wait("helper-exited");
        let mut human = f.connect();
        human.write("MATCHED-RETURN-ONLY");
        f.wait_held();
        f.emit(noise);
        f.stable(b"\x07");
        f.flag("resume-go");
        f.expect(b"\x07MATCHED-RETURN-ONLY");
        f.stable(b"\x07MATCHED-RETURN-ONLY");
    }
}

/// Scenario: Hold a human editor's helper after a failed spawn or an injected wait failure following a real editor start. A failed spawn retains the suffix through return and retirement under production Hold; an observed running user editor reads its released suffix before the wait-unknown exit drain, and those bytes must never reach the composer or its successor.
#[spec("inbox/probe/049")]
#[test]
fn probe_049_failed_editor_outcomes_stay_held_under_production_hold() {
    for outcome in ["spawn-failed", "wait-unknown"] {
        let mut f = Fixture::start("claude");
        f.flag("hold-return");
        f.probe();
        f.wait("helper-exited");
        // Q104b: a positively observed running user editor owns bytes
        // released before WaitUnknown is learned at the helper's exit drain.
        // Keep the helper alive at post-editor until that reader acknowledges
        // the suffix, so the positive release is witnessed, not inferred.
        let pause = if outcome == "wait-unknown" {
            f.setting("editor-mode", "read");
            f.setting("helper-pause-at", "post-editor");
            Some(f.pause_helper())
        } else {
            None
        };
        let mut human = f.connect();
        // A HumanG segment follows the already-complete probe.
        human.write(if pause.is_some() {
            "\x07OQ5-HELD\n"
        } else {
            "\x07OQ5-HELD"
        });
        f.wait_held();
        f.flag(&format!("{outcome}-2"));
        f.flag("resume-go-1");
        if let Some(pause) = pause {
            f.wait("editor-input");
            assert_eq!(
                fs::read(f.path("editor-input")).unwrap(),
                b"OQ5-HELD\n",
                "the released suffix belongs to the running user editor"
            );
            f.flag("editor-go");
            f.wait("editor-finished");
            f.remove("pause-helper");
            pause.release();
        }
        // Distinct witnesses and releases separate the second quick failure
        // from the first helper's completed return.
        f.wait("helper-exited-2");
        assert!(
            !fs::read(f.path("bytes"))
                .unwrap()
                .windows(b"OQ5-HELD".len())
                .any(|w| w == b"OQ5-HELD"),
            "{outcome}: exit alone released the pending suffix"
        );
        f.flag("resume-go-2");
        f.expect(b"\x07\x07");
        assert!(
            common::wait_until(WAIT, || f.screen().contains("Try \"fix the bug\"")),
            "the complete real return must be visible before the negative watch"
        );
        f.stable(b"\x07\x07");
        f.probe();
        f.stable(b"\x07\x07");
        f.replace();
        f.stable(b"");
    }
}

/// Scenario: Queue a human Ctrl+G behind a returning check, pause its helper after a short-lived editor starts with an injected wait failure, and type a suffix while that helper is still alive. First witness that the suffix remains held, then drain the helper and return the composer: those undelivered bytes must remain held until generation replacement.
#[spec("inbox/probe/049")]
#[test]
fn probe_049_wait_unknown_at_drain_retains_the_undelivered_suffix() {
    let mut f = Fixture::start("claude");
    f.flag("hold-return");
    f.probe();
    f.wait("helper-exited-1");
    let pause = f.pause_helper();
    f.setting("helper-pause-at", "post-editor");
    f.flag("wait-unknown-2");
    // The editor has no input reader and exits immediately. Unlike the
    // observed-running-editor control above, no suffix belongs to it.
    fs::copy("/bin/true", f.daemon.home.join("native/editor")).unwrap();
    let mut human = f.connect();
    human.write("\x07");
    f.wait_held();
    f.flag("resume-go-1");
    f.wait("helper-started-2");
    assert!(
        common::wait_until(WAIT, || fs::read_to_string(f.path("helper-trace"))
            .unwrap_or_default()
            .contains("pause post-editor")),
        "the second helper must pause after its wait failure"
    );
    human.write("STILL-HELD-AT-DRAIN");
    f.wait_held();
    f.expect(b"\x07\x07");
    f.stable(b"\x07\x07");
    assert!(
        f.held(),
        "the suffix must still be held before the helper exit drain"
    );
    f.remove("pause-helper");
    pause.release();
    f.wait("helper-exited-2");
    let trace = fs::read_to_string(f.path("helper-trace")).unwrap();
    assert!(trace.contains("wait-unknown 10"), "{trace}");
    f.flag("resume-go-2");
    f.wait("composer-return-2");
    f.stable(b"\x07\x07");
    assert!(
        f.held(),
        "WaitUnknown at drain must retain the undelivered suffix"
    );
    f.replace();
    f.stable(b"");
}

/// Scenario: For Claude and Codex under Hold and test-selected Authority, queue a human editor trigger and suffix behind a real returning check, then fail the editor's spawn. Each retains the suffix through helper exit; Authority releases it once after the matching return, while Hold retains it through replacement.
#[spec("inbox/probe/049")]
#[test]
fn probe_049_authority_failed_start_releases_only_after_the_matching_return() {
    for kind in ["claude", "codex"] {
        for policy in ["hold", "authority"] {
            let mut f = Fixture::start(kind);
            f.control(&format!("records-policy:{policy}"));
            f.flag("hold-return");
            f.probe();
            f.wait("helper-exited-1");
            f.flag("spawn-failed-2");
            let mut human = f.connect();
            human.write("\x07AFTER-FAILED-START");
            f.wait_held();
            f.flag("resume-go-1");
            f.wait("helper-exited-2");
            f.stable(b"\x07\x07");
            let trace = fs::read_to_string(f.path("helper-trace")).unwrap();
            assert!(trace.contains("branch Unarmed"), "{kind}/{policy}: {trace}");
            assert!(
                !f.path("editor-started").exists(),
                "a failed spawn has no editor reader"
            );
            f.flag("resume-go-2");
            f.wait("composer-return-2");
            if policy == "authority" {
                f.expect(b"\x07\x07AFTER-FAILED-START");
                f.stable(b"\x07\x07AFTER-FAILED-START");
                human.write("-NEXT");
                f.expect(b"\x07\x07AFTER-FAILED-START-NEXT");
                f.stable(b"\x07\x07AFTER-FAILED-START-NEXT");
            } else {
                f.stable(b"\x07\x07");
                assert!(f.held(), "{kind}/{policy}: the return grants no release");
                f.replace();
                f.stable(b"");
            }
        }
    }
}

/// Scenario: Admit a probe through the live writer and read its nonzero segment from the installed mapped page. A following count-only publication and a physical native candidate must retain that segment number while publishing the updated count and native number coherently.
#[spec("inbox/probe/059")]
#[test]
fn probe_059_live_page_retains_segment_across_count_and_native_publications() {
    let f = Fixture::start("claude");
    f.probe();
    f.wait("helper-exited");
    f.wait_open();
    let first = f.page();
    assert!(
        first.seg_seq > 0,
        "a real probe must publish its segment before its byte"
    );
    assert_eq!(first.count, 0, "the helper consumed its probe liability");
    f.control("publish");
    let raised = f.page();
    assert_eq!(raised.count, 1, "count-only raise publishes one liability");
    assert_eq!(
        raised.seg_seq, first.seg_seq,
        "count-only raise retains segment"
    );
    assert_eq!(raised.native_seq, first.native_seq);
    f.control("withdraw");
    let withdrawn = f.page();
    assert_eq!(withdrawn.count, 0, "withdraw publishes no liability");
    assert_eq!(withdrawn.seg_seq, first.seg_seq);
    assert_eq!(withdrawn.native_seq, first.native_seq);
    f.flag("edit-drop");
    let mut human = f.connect();
    human.write("\x07");
    f.expect(b"\x07\x07");
    let native = f.page();
    assert_eq!(
        native.count, 0,
        "native publication retains the withdrawn count"
    );
    assert_eq!(
        native.seg_seq, first.seg_seq,
        "native publication retains probe segment"
    );
    assert!(
        native.native_seq > first.native_seq,
        "native candidate must publish its own sequence"
    );
}

/// Scenario: Send whole retained OSC 10 BEL/ST replies or a raw native BEL, then request a first probe. The report controls admit immediately; the native case refuses, first/retry requests stay not due even after the quiet period, and an ordinary retry writes exactly one probe after the production backoff without a generation reset.
#[spec("inbox/probe/061")]
#[test]
fn probe_061_live_admission_reads_actual_osc_reply_and_native_histories() {
    for (reply, native) in [
        ("\x1b]10;rgb:ffff/ffff/ffff\x07", false),
        ("\x1b]10;rgb:ffff/ffff/ffff\x1b\\", false),
        ("\x07", true),
    ] {
        let f = Fixture::start("claude");
        f.flag("edit-drop");
        f.emit(b"\x1b]10;?\x1b\\");
        let mut human = f.connect();
        human.write(reply);
        f.expect(reply.as_bytes());
        let page = f.page();
        let first = f.probe();
        if native {
            assert_eq!(first["composer_probe"]["outcome"], "refused", "{first}");
            let refused_at = Instant::now();
            let epoch = f.epoch();
            assert_eq!(f.page(), page, "a refused attempt must not publish an arm");
            for operation in ["probe-request", "probe-request:retry"] {
                let early = f.control(operation);
                assert_eq!(early["composer_probe"]["outcome"], "not_due", "{early}");
            }
            f.stable(reply.as_bytes());
            wait_span(
                dot_agent_deck::composer_probe::query_ledger::T_TRIGGER + Duration::from_millis(50),
            );
            let early = f.control("probe-request:retry");
            assert_eq!(
                early["composer_probe"]["outcome"], "not_due",
                "quiet-period expiry must not bypass the persistent schedule: {early}"
            );
            f.stable(reply.as_bytes());
            assert_eq!(f.page(), page, "not-due requests must not publish an arm");
            assert_eq!(f.epoch(), epoch, "not-due requests must not move the epoch");
            assert!(!has_held(&f.snapshot()));
            assert_eq!(f.control("fence-status")["composer_fence"]["fenced"], false);
            let due_after =
                dot_agent_deck::composer_probe::probe::BACKOFF[0] + Duration::from_millis(100);
            wait_span(due_after.saturating_sub(refused_at.elapsed()));
            let eligible = f.control("probe-request:retry");
            assert_eq!(
                eligible["composer_probe"]["outcome"], "written",
                "not-due requests must not restart the refusal backoff: {eligible}"
            );
        } else {
            assert_eq!(first["composer_probe"]["outcome"], "written", "{first}");
        }
        let expected = [reply.as_bytes(), b"\x07"].concat();
        f.expect(&expected);
        f.stable(&expected);
        assert!(
            f.page().seg_seq > 0,
            "eligible probes publish their segment"
        );
    }
}

/// Scenario: Queue ordinary mail for an idle synthetic agent whose empty box has been touched by a harmless key, with its next edit trigger configured to be ignored. Without a test request, the production tick must write a probe byte and publish a nonzero segment; this witnesses submission, without helper completion or mail delivery.
#[spec("inbox/probe/059")]
#[test]
fn probe_059_automatic_tick_submits_a_real_probe() {
    let f = Fixture::start("claude");
    f.hook_event("session_start");
    f.hook_event("idle");
    f.flag("edit-ignore-next");
    let mut human = f.connect();
    human.write("x\x7f");
    f.expect(b"x\x7f");
    f.enqueue();
    assert!(
        common::wait_until(WAIT, || f.input().contains(&7)),
        "the ordinary doorbell tick never submitted a probe; input {:?}",
        f.input()
    );
    assert!(
        f.page().seg_seq > 0,
        "automatic checks publish a nonzero segment"
    );
}

/// Scenario: Leave a draft and let a physical native Ctrl+G make the first probe refuse, then queue mail and witness its pending retry deadline. Advance that monotonic deadline with the schedule control; without another probe request or a generation reset, the production tick must reach writer admission, write one check byte and run its real installed helper.
#[spec("inbox/probe/059")]
#[test]
fn probe_059_automatic_retry_after_a_due_admission_failure() {
    let f = Fixture::with_env("claude", &[("DOT_AGENT_DECK_E2E_PROBE_GATE", "probe-gate")]);
    f.hook_event("session_start");
    f.hook_event("idle");
    f.flag("edit-drop");
    let mut human = f.connect();
    // Keep ordinary mail held by the pre-Q35-21 draft policy. This row
    // asserts retry submission, not that the new epoch already gates rings.
    human.write("draft\x07");
    f.expect(b"draft\x07");
    let first = f.probe();
    assert_eq!(first["composer_probe"]["outcome"], "refused", "{first}");
    assert_eq!(f.page().seg_seq, 0, "a refused check publishes no segment");
    let root = f.daemon.home.join("probe-gate");
    fs::create_dir(&root).unwrap();
    // Record the writer's arrival before its admission check without pausing
    // it. The marker's event time survives late polling or helper startup.
    fs::write(root.join("scheduled.release"), "").unwrap();
    fs::write(root.join("phase"), "scheduled").unwrap();
    f.remove("edit-drop");
    f.enqueue();
    f.stable(b"draft\x07");
    let schedule = f.control("schedule-status");
    assert_eq!(schedule["composer_schedule"]["due"], false, "{schedule}");
    assert_eq!(schedule["composer_schedule"]["failures"], 1, "{schedule}");
    assert!(!root.join("scheduled.arrived").exists(), "no early retry");
    // Schedule advancement does not age the physical native-key hazard.
    wait_span(dot_agent_deck::composer_probe::query_ledger::T_TRIGGER + Duration::from_millis(50));
    f.control("schedule-advance:30000");
    assert!(
        common::wait_until(WAIT, || root.join("scheduled.arrived").exists()),
        "the automatic retry never reached writer admission; input {:?}",
        f.input()
    );
    assert!(
        !root.join("scheduled.timed-out").exists(),
        "the observation must not delay writer admission"
    );
    f.wait("helper-exited-1");
    f.expect(b"draft\x07\x07");
    f.stable(b"draft\x07\x07");
    assert!(
        f.page().seg_seq > 0,
        "the automatic retry publishes its own segment"
    );
}

/// Scenario: Queue ordinary mail behind a draft and fail the live writer's check bytes, advancing each pending retry deadline through the third failure and one more. The production tick must continue attempting checks while Broken, then write one probe and run its helper when the fault is cleared and the next deadline is advanced.
#[spec("inbox/probe/059")]
#[test]
fn probe_059_automatic_retries_continue_through_broken() {
    let f = Fixture::start("claude");
    f.hook_event("session_start");
    f.hook_event("idle");
    let mut human = f.connect();
    human.write("draft");
    f.expect(b"draft");
    f.control("writer-fault:jobs=any,skip=0,accept=0");
    let first = f.probe();
    assert_eq!(first["composer_probe"]["outcome"], "refused", "{first}");
    assert_eq!(
        first["composer_probe"]["reason"], "the probe's write failed",
        "{first}"
    );
    f.enqueue();
    for (failures, delay) in [(1, 30000), (2, 120000), (3, 600000), (4, 1800000)] {
        assert!(common::wait_until(
            WAIT,
            || f.control("schedule-status")["composer_schedule"]["failures"] == failures
        ));
        let status = f.control("schedule-status");
        assert_eq!(
            status["composer_schedule"]["broken"],
            failures >= 3,
            "{status}"
        );
        assert_eq!(status["composer_schedule"]["due"], false, "{status}");
        assert_eq!(
            f.control("writer-status")["composer_writer"]["failed_writes"],
            failures,
            "each due retry must actually reach the writer"
        );
        f.stable(b"draft");
        if failures == 4 {
            f.control("writer-fault:off");
        }
        f.control(&format!("schedule-advance:{delay}"));
    }
    f.wait("helper-exited-1");
    f.expect(b"draft\x07");
    f.stable(b"draft\x07");
    assert!(
        f.page().seg_seq > 0,
        "the repaired writer publishes its actual probe"
    );
}

/// Scenario: Send OSC 10 and 11 reports, whole or byte-split and BEL- or ST-ended, before requesting a live probe, with each query retained before ledger saturation or dropped after it. A split or unretained BEL is a physical native candidate and refuses the first check without publication; other reports permit exactly one real probe byte.
#[spec("inbox/probe/061")]
#[test]
fn probe_061_live_slot_split_and_saturation_permutations() {
    use dot_agent_deck::composer_probe::query_ledger::LEDGER_SLOTS;
    for slot in [10, 11] {
        for retained in [true, false] {
            for bel in [true, false] {
                for split in [true, false] {
                    let f = Fixture::start("claude");
                    f.flag("edit-drop");
                    let query = format!("\x1b]{slot};?\x1b\\");
                    if retained {
                        f.emit(query.as_bytes());
                    }
                    f.emit(&b"\x1b[c".repeat(LEDGER_SLOTS + 1));
                    if !retained {
                        f.emit(query.as_bytes());
                    }
                    let reply = format!(
                        "\x1b]{slot};rgb:1/22/333{}",
                        if bel { "\x07" } else { "\x1b\\" }
                    );
                    let mut human = f.connect();
                    if split {
                        for (index, byte) in reply.as_bytes().iter().enumerate() {
                            human.write(std::str::from_utf8(std::slice::from_ref(byte)).unwrap());
                            // Physical-write acknowledgement forces separate acceptance
                            // and write histories even when the socket could coalesce.
                            f.expect(&reply.as_bytes()[..=index]);
                        }
                    } else {
                        human.write(&reply);
                        f.expect(reply.as_bytes());
                    }
                    let before = f.page();
                    let result = f.probe();
                    let candidate = bel && (split || !retained);
                    assert_eq!(
                        result["composer_probe"]["outcome"],
                        if candidate { "refused" } else { "written" },
                        "slot={slot}, retained={retained}, bel={bel}, split={split}: {result}"
                    );
                    let expected = if candidate {
                        reply.into_bytes()
                    } else {
                        [reply.as_bytes(), b"\x07"].concat()
                    };
                    f.expect(&expected);
                    f.stable(&expected);
                    if candidate {
                        assert_eq!(f.page(), before, "a native hazard publishes no arm");
                    } else {
                        assert!(
                            f.page().seg_seq > before.seg_seq,
                            "an admitted report control publishes its segment"
                        );
                    }
                }
            }
        }
    }
}

/// Scenario: During a delayed live check, send OSC 10/11 replies with retained or saturated-away queries, BEL or ST endings, and whole or split frames, followed by typing. With a second liability present, a candidate's Armed helper and real return must automatically complete HumanG, proven by a following Ctrl+G reaching its user's editor; report controls deliver the typing without a second helper.
#[spec("inbox/probe/061")]
#[test]
fn probe_061_in_check_osc_candidates_complete_without_manual_release() {
    use dot_agent_deck::composer_probe::query_ledger::LEDGER_SLOTS;
    for slot in [10, 11] {
        for retained in [true, false] {
            for bel in [true, false] {
                for split in [false, true] {
                    let f = Fixture::start("claude");
                    f.flag("hold-return");
                    f.setting("editor-mode", "read");
                    f.setting("edit-mechanism", "H-chunk");
                    f.probe();
                    f.wait("helper-exited-1");
                    let query = format!("\x1b]{slot};?\x1b\\");
                    if retained {
                        f.emit(query.as_bytes());
                    }
                    f.emit(&b"\x1b[c".repeat(LEDGER_SLOTS + 1));
                    if !retained {
                        f.emit(query.as_bytes());
                    }
                    let reply = format!(
                        "\x1b]{slot};rgb:1/22/333{}",
                        if bel { "\x07" } else { "\x1b\\" }
                    );
                    let candidate = bel && (split || !retained);
                    if candidate {
                        // A real count-only publication makes this HumanG
                        // helper-only. An Unarmed editor-bearing segment
                        // intentionally cannot complete under production Hold.
                        f.control("publish");
                    }
                    if bel && !candidate {
                        // F2 normally treats every BEL as Ctrl+G. Configure
                        // the report control as an inert terminal reply;
                        // candidate histories keep its ordinary edit map.
                        f.setting("reported-osc", &reply);
                    }
                    let mut human = f.connect();
                    if split {
                        human.write(&reply[..reply.len() - 1]);
                        human.write(&format!("{}OSC-EDITOR\n", &reply[reply.len() - 1..]));
                    } else if !candidate {
                        // A whole retained report is one whole attach frame;
                        // appending text would make its BEL a candidate.
                        human.write(&reply);
                        human.write("OSC-EDITOR\n");
                    } else {
                        human.write(&format!("{reply}OSC-EDITOR\n"));
                    }
                    // §3.7: the matched Claude decision already released
                    // ordinary typing, including a split report's prefix.
                    // Only the next candidate must wait for this return.
                    if candidate {
                        f.wait_held();
                    }
                    f.flag("resume-go-1");
                    let context =
                        format!("slot={slot}, retained={retained}, bel={bel}, split={split}");
                    if candidate {
                        f.wait("helper-exited-2");
                        f.flag("resume-go-2");
                        f.wait("composer-return-2");
                        let expected =
                            [b"\x07".as_slice(), reply.as_bytes(), b"OSC-EDITOR\n"].concat();
                        f.expect(&expected);
                        f.stable(&expected);
                        assert!(
                            !f.path("editor-started").exists(),
                            "{context}: Armed runs no user editor"
                        );
                        // Ordinary typing could pass even behind a merely
                        // released segment. A new candidate cannot dispatch
                        // until the preceding HumanG really completed.
                        human.write("\x07");
                        f.wait("editor-started");
                        human.write("NEXT-EDITOR\n");
                        f.wait("editor-input");
                        assert_eq!(
                            fs::read(f.path("editor-input")).unwrap(),
                            b"NEXT-EDITOR\n",
                            "{context}: the next candidate proves automatic completion"
                        );
                        f.flag("editor-go");
                        f.wait("helper-exited-3");
                        f.flag("resume-go-3");
                        f.wait("composer-return-3");
                    } else {
                        let expected =
                            [b"\x07".as_slice(), reply.as_bytes(), b"OSC-EDITOR\n"].concat();
                        f.expect(&expected);
                        f.stable(&expected);
                        assert!(
                            !f.path("helper-started-2").exists(),
                            "{context}: a report must not invoke an editor"
                        );
                    }
                    human.write("AFTER-OSC");
                    let expected = if candidate {
                        [
                            b"\x07".as_slice(),
                            reply.as_bytes(),
                            b"OSC-EDITOR\n\x07AFTER-OSC",
                        ]
                        .concat()
                    } else {
                        [
                            b"\x07".as_slice(),
                            reply.as_bytes(),
                            b"OSC-EDITOR\nAFTER-OSC",
                        ]
                        .concat()
                    };
                    f.expect(&expected);
                    f.stable(&expected);
                }
            }
        }
    }
}

/// Scenario: Pause a live probe after scheduling, run the native agent's real unarmed helper until its writer scope is open, then let the writer attempt the probe. The newly unsafe scope must cause refusal without a probe byte, a page change or a surviving fence.
#[spec("inbox/probe/059")]
#[test]
fn probe_059_scope_changed_after_live_scheduling_refuses_the_writer() {
    let f = Fixture::with_env("claude", &[("DOT_AGENT_DECK_E2E_PROBE_GATE", "probe-gate")]);
    let root = f.daemon.home.join("probe-gate");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("phase"), "scheduled").unwrap();
    let before = f.page();
    std::thread::scope(|threads| {
        let request = threads.spawn(|| f.probe());
        assert!(
            common::wait_until(Duration::from_secs(6), || {
                assert!(
                    !request.is_finished(),
                    "live probe returned before the scheduled barrier (the assumed probe request/gate is missing)"
                );
                root.join("scheduled.arrived").exists()
            }),
            "live writer never reached its scheduled barrier"
        );
        let export = f
            .daemon
            .home
            .join("tmp")
            .join(format!("claude-{}", unsafe { libc::getuid() }))
            .join("claude-prompt-scope.md");
        fs::create_dir_all(export.parent().unwrap()).unwrap();
        fs::write(&export, "").unwrap();
        f.command(json!({"action":"invoke","path":export}));
        assert!(
            common::wait_until(Duration::from_secs(3), || f.scope() == "open"),
            "the real native editor invocation must open scope before writer admission"
        );
        fs::write(root.join("scheduled.release"), "").unwrap();
        request.join().unwrap();
    });
    assert!(
        !root.join("scheduled.timed-out").exists(),
        "the seam must not time out into a false refusal"
    );
    f.stable(b"");
    assert_eq!(
        f.page(),
        before,
        "refusal after live scheduling must not publish an arm"
    );
    let mut human = f.connect();
    human.write("NO-STRAY-FENCE");
    f.expect(b"NO-STRAY-FENCE");
}

/// Scenario: Pause the real helper before its arm load until the observer has actually seen Pending, then type behind the check and emit foreign return bytes. No key may escape before its decision; after the helper resumes Armed and the original composer returns, the keys must arrive once and a subsequent empty check must be admitted.
#[spec("inbox/probe/057")]
#[test]
fn probe_057_pending_to_armed_recovers_held_keys_and_a_second_probe() {
    let f = Fixture::start("claude");
    let paused = f.pause_helper();
    f.flag("hold-return");
    f.probe();
    f.wait("helper-started");
    assert!(
        common::wait_until(WAIT, || f.scope() == "pending"),
        "scan must see the paused helper before its decision"
    );
    let mut human = f.connect();
    human.write("RECOVERED");
    f.wait_held();
    f.emit(b"\x1b[?2004l\x1b[?2004h");
    f.stable(b"\x07");
    f.remove("pause-helper");
    paused.release();
    f.wait("helper-exited-1");
    f.flag("resume-go-1");
    f.expect(b"\x07RECOVERED");
    human.write("\x15");
    f.expect(b"\x07RECOVERED\x15");
    assert!(
        common::wait_until(WAIT, || f.screen().contains("Try \"fix the bug\"")),
        "clear must be redrawn before the next check"
    );
    f.remove("hold-return");
    f.probe();
    f.wait("helper-exited-2");
    f.expect(b"\x07RECOVERED\x15\x07");
    f.stable(b"\x07RECOVERED\x15\x07");
}

/// Scenario: Let the observer see a real helper paused before any decision, then kill only that fixture-captured helper PID. Its decisionless exit and a return-marker pair cannot turn the held keys or later probe into input; replacing the generation must still leave the successor empty.
#[spec("inbox/probe/057")]
#[test]
fn probe_057_decisionless_exit_keeps_scope_open_and_refuses_the_next_probe() {
    let mut f = Fixture::start("claude");
    let _pause = f.pause_helper();
    f.probe();
    let helper = live::read_json(&f.path("helper-started"));
    assert!(common::wait_until(WAIT, || f.scope() == "pending"));
    let mut human = f.connect();
    human.write("NO-DECISION-NEVER");
    f.wait_held();
    let pid = helper["pid"].as_u64().unwrap() as i32;
    // SAFETY: this PID was acknowledged by our native fixture after its own
    // spawn. It still lives in the paused helper at the Pending barrier.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    f.wait("helper-exited");
    assert!(common::wait_until(WAIT, || f.scope() == "open"));
    f.probe();
    f.stable(b"\x07");
    f.replace();
    f.stable(b"");
}

/// Scenario: Publish an arm and pause the actual installed helper while its socket is readable, then stop that captured helper after its decision has consumed the arm. The observer must kill the still-stopped, attested Armed helper with SIGKILL after its bound; its own alarm cannot execute while stopped, so the signal distinguishes the daemon's E2 path.
#[spec("inbox/probe/057")]
#[test]
fn probe_057_real_e2_kills_only_the_attested_armed_helper() {
    let f = Fixture::start("claude");
    let _pause = f.pause_helper();
    f.setting("helper-pause-at", "ready");
    f.control("publish");
    let mut human = f.connect();
    human.write("\x07");
    let helper = live::read_json(&f.path("helper-started"));
    assert!(
        common::wait_until(WAIT, || fs::read_to_string(f.path("helper-trace"))
            .unwrap_or_default()
            .contains("pause ready")),
        "the real helper must pause in its socket exchange"
    );
    assert!(
        common::wait_until(WAIT, || f.page().count == 0),
        "the observer must actually settle the Armed decision before the kill watch"
    );
    let pid = helper["pid"].as_u64().unwrap() as i32;
    // SAFETY: the native fixture acknowledged this helper PID, which is still
    // blocked at our FIFO. No process name or production PID is used.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    let result = live::read_json(&f.path("helper-result-1"));
    assert_eq!(
        result["signal"],
        libc::SIGKILL,
        "E2 must be the attested pidfd kill, not the helper alarm: {result}"
    );
    assert!(
        !f.path("editor-started").exists(),
        "an Armed helper runs no user editor"
    );
}

/// Scenario: Run a genuine unarmed editor that waits longer than the Armed helper's five-second kill bound. The daemon must leave both the user's editor and its waiting helper alive, then accept their ordinary completion when the editor is released.
#[spec("inbox/probe/057")]
#[test]
fn probe_057_e2_does_not_kill_a_running_user_editor() {
    let f = Fixture::start("claude");
    f.setting("editor-mode", "wait");
    let mut human = f.connect();
    human.write("\x07");
    f.wait("editor-started");
    wait_span(dot_agent_deck::composer_probe::observe::E2_AFTER + Duration::from_millis(150));
    assert!(
        !f.path("helper-exited").exists(),
        "an editor-bearing helper must not be enrolled in E2"
    );
    f.flag("editor-go");
    let result = live::read_json(&f.path("helper-result-1"));
    assert_eq!(
        result["code"], 0,
        "the real editor/helper return normally: {result}"
    );
    assert_eq!(result["signal"], Value::Null);
}

/// Scenario: Run an unarmed user editor that either leaves a genuine background descendant alive or loses its already-open report connection when the daemon rejects an oversized report. After the real helper exits and the composer returns, its unresolved scope must refuse a fresh check without writing another probe; the survivor exits on its private fixture file or a 30-second deadline. Hold policy already opens scope at the Unarmed decision, so these assertions do not isolate the later Survivors report or missing-report drain as the cause of scope remaining open.
#[spec("inbox/probe/057")]
#[test]
fn probe_057_background_survivor_and_lost_report_keep_scope_open() {
    for history in ["survivor", "lost-report"] {
        let f = Fixture::start("claude");
        if history == "survivor" {
            f.setting("editor-mode", "survivor");
        } else {
            // The invocation and response use the genuine helper connection.
            // Only its later report is oversized: the daemon closes while
            // reading that bounded frame, rather than bypassing the helper.
            f.setting("helper-report-sndbuf", "1024");
            f.setting("helper-report-pad", "1048576");
        }
        let mut human = f.connect();
        human.write("\x07");
        f.wait("editor-started");
        f.wait("helper-exited-1");
        let trace = fs::read_to_string(f.path("helper-trace")).unwrap();
        assert!(trace.contains("branch Unarmed"), "{history}: {trace}");
        if history == "survivor" {
            f.wait("survivor-pid");
            assert!(trace.contains("tree Survivors"), "{trace}");
        } else {
            assert!(
                trace.contains("report unsent"),
                "the actual report connection must fail: {trace}"
            );
        }
        assert!(
            common::wait_until(WAIT, || f.scope() == "open"),
            "{history}: unresolved scope must remain open"
        );
        // Outlast the native candidate's own quiet period: scope, rather
        // than the key alone, must keep the next check from being admitted.
        wait_span(
            dot_agent_deck::composer_probe::query_ledger::T_TRIGGER + Duration::from_millis(50),
        );
        let before = f.page();
        let result = f.probe();
        assert_eq!(
            result["composer_probe"]["outcome"], "refused",
            "{history}: {result}"
        );
        f.expect(b"\x07");
        f.stable(b"\x07");
        assert_eq!(
            f.page(),
            before,
            "{history}: a refused check publishes no arm"
        );
        assert!(
            !f.path("helper-started-2").exists(),
            "{history}: refusal runs no second helper"
        );
        f.flag("survivor-go");
    }
}

/// Scenario: Publish one real liability, then invoke the installed helper under either a shell tool or another copy of the agent executable. Neither non-owned invocation may consume that liability; the tool must run its editor, while the armed unknown-family invocation must refrain from opening one.
#[spec("inbox/probe/057")]
#[test]
fn probe_057_tool_and_unknown_family_helpers_leave_liability_unconsumed() {
    for parent in ["tool", "unknown"] {
        let f = Fixture::start("claude");
        f.setting("helper-parent", parent);
        f.control("publish");
        assert_eq!(f.page().count, 1);
        let mut human = f.connect();
        human.write("\x07");
        f.wait("helper-exited-1");
        let trace = fs::read_to_string(f.path("helper-trace")).unwrap();
        if parent == "tool" {
            assert!(
                trace.contains("branch Tool class PositivelyTool"),
                "{trace}"
            );
            f.wait("editor-finished");
        } else {
            assert!(trace.contains("branch Armed class Unknown"), "{trace}");
            assert!(!f.path("editor-started").exists(), "{trace}");
        }
        f.expect(b"\x07");
        f.stable(b"\x07");
        assert_eq!(
            f.page().count,
            1,
            "{parent}: a non-owned helper cannot settle the published liability"
        );
    }
}

/// Scenario: Select Authority policy and a one-record cap before observation, then keep a real unarmed editor/helper alive in that slot while a second native or tool helper arrives. A dropped undecided sighting or tool decision opens the writer scope, whereas an Armed helper-only decision leaves it closed; each live cap must refuse a new probe without a check byte, and a two-slot control keeps the undecided sighting Pending instead of opening scope.
#[spec("inbox/probe/057")]
#[test]
fn probe_057_live_cap_dropped_undecided_tool_and_armed_helpers() {
    for history in ["undecided", "tool", "armed"] {
        for cap in if history == "undecided" {
            &[1, 2][..]
        } else {
            &[1][..]
        } {
            let f = Fixture::start("claude");
            f.control("records-policy:authority");
            f.control(&format!("records-cap:{cap}"));
            f.setting("editor-mode", "wait");
            let mut human = f.connect();
            human.write("\x07");
            f.wait("editor-started");
            assert!(
                common::wait_until(WAIT, || f.scope() == "closed"),
                "Authority keeps the first observed running editor's scope closed"
            );
            let pause = (history == "undecided").then(|| f.pause_helper());
            // Publish a liability to enable the real census. For the Armed-only
            // row, let IPC provide that no-editor fact before any scan could
            // drop an undecided sighting of this same helper.
            if history == "armed" {
                f.control("observer-hold");
                wait_span(Duration::from_millis(200));
            }
            f.control("publish");
            let first = live::read_json(&f.path("editor-started"));
            let export = std::path::Path::new(first["path"].as_str().unwrap())
                .with_file_name("claude-prompt-00000000-0000-4000-8000-000000000057.md");
            fs::write(&export, "second helper").unwrap();
            let reply = f.command(json!({"action":"parallel-helper", "path":export,
            "tool":history == "tool", "pause":history == "undecided"}));
            assert!(
                reply["pid"].as_u64().is_some(),
                "a second real child: {reply}"
            );
            if history == "undecided" {
                let expected = if *cap == 1 { "open" } else { "pending" };
                assert!(
                    common::wait_until(WAIT, || f.scope() == expected),
                    "cap {cap}: the undecided scan must be {expected}"
                );
            } else {
                assert!(
                    common::wait_until(WAIT, || fs::read_to_string(f.path("parallel-trace"))
                        .unwrap_or_default()
                        .contains(if history == "tool" {
                            "branch Tool"
                        } else {
                            "branch Armed"
                        })),
                    "{history}: second helper must reach its actual decision"
                );
                assert_eq!(
                    f.scope(),
                    if history == "tool" { "open" } else { "closed" },
                    "{history}"
                );
            }
            // Once the native key's quiet deadline is past, this capped
            // history must still refuse a check while its first helper waits.
            wait_span(
                dot_agent_deck::composer_probe::query_ledger::T_TRIGGER + Duration::from_millis(50),
            );
            let before = f.page();
            let probe = f.probe();
            assert_eq!(
                probe["composer_probe"]["outcome"], "refused",
                "{history}: {probe}"
            );
            assert_eq!(f.page(), before, "{history}: refusal publishes no probe");
            f.stable(b"\x07");
            assert!(!f.path("helper-started-2").exists());
            if let Some(pause) = pause {
                f.remove("pause-helper");
                pause.release();
            }
            f.flag("editor-go");
            f.wait("parallel-result");
            f.control("observer-go");
            f.wait("helper-exited-1");
        }
    }
}
