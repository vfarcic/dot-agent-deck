//! L1 tests for the TUI's Upgrade all daemons dialog (issue #1600): the
//! dialog drawn at each step of a batch the shared
//! `daemon_upgrade_batch` functions report — the plan, a daemon's own
//! question, the run, and one line per deck at the end.
//!
//! The batch itself (which decks are upgraded, a failure not stopping it, one
//! question per busy daemon) is `daemon_upgrade_batch`'s unit tests and the
//! spawned CLI's `tests/e2e_remote_upgrade_all.rs`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::daemon_protocol::{RestartAgent, RestartStopSet};
use dot_agent_deck::daemon_upgrade::{
    RestartChoice, UpgradeOutcome, UpgradeProgress, UpgradeStage,
};
use dot_agent_deck::daemon_upgrade_batch::{
    BatchEvent, BatchPlan, BatchReport, DeckCheck, DeckReport, DeckResult, PlannedDeck, SkipReason,
};
use dot_agent_deck::state::OrchestrationRoleRecord;
use dot_agent_deck::upgrade_all_dialog::{
    Choice, Effect, Question, Update, UpgradeAllDialog, render,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use spec::spec;

fn draw(dialog: &mut UpgradeAllDialog) -> String {
    draw_at(dialog, 100, 30)
}

fn draw_at(dialog: &mut UpgradeAllDialog, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            render(frame, area, dialog);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let area = buffer.area;
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn older(deck: &str) -> PlannedDeck {
    PlannedDeck {
        deck: deck.into(),
        check: DeckCheck::Upgrade {
            from: "0.51.0".into(),
            to: "0.52.0".into(),
        },
    }
}

/// Four decks: two older, one current, one that could not be reached.
fn plan() -> BatchPlan {
    BatchPlan {
        decks: vec![
            older("mac-studio"),
            PlannedDeck {
                deck: "pi".into(),
                check: DeckCheck::Skip {
                    reason: SkipReason::Current {
                        version: "0.52.0".into(),
                    },
                },
            },
            older("build-box"),
            PlannedDeck {
                deck: "laptop".into(),
                check: DeckCheck::Unreachable {
                    reason: "ssh: connect to host laptop port 22: No route to host".into(),
                },
            },
        ],
    }
}

fn busy() -> RestartStopSet {
    RestartStopSet {
        agents: vec![
            RestartAgent {
                id: "a1".into(),
                label: "lead".into(),
                pane_id: Some("4".into()),
                cwd: Some("/home/u/project".into()),
            },
            RestartAgent {
                id: "a2".into(),
                label: "coder".into(),
                pane_id: Some("5".into()),
                cwd: None,
            },
        ],
        roles: vec![OrchestrationRoleRecord {
            pane_id: "4".into(),
            role: "orchestrator".into(),
            orchestration: "release-team".into(),
            is_orchestrator: true,
        }],
    }
}

fn started(index: usize, deck: &str) -> Update {
    Update::Event(BatchEvent::Started {
        index,
        deck: deck.into(),
    })
}

fn finished(index: usize, report: DeckReport) -> Update {
    Update::Event(BatchEvent::Finished { index, report })
}

fn restarted() -> DeckResult {
    DeckResult::Ran {
        outcome: UpgradeOutcome::Restarted {
            from_version: "0.51.0".into(),
            to_version: "0.52.0".into(),
            stopped: RestartStopSet::default(),
        },
    }
}

/// Scenario: Open Upgrade all daemons on a deck list of four remote decks. The plan names each deck and what will happen to it — two to upgrade, one already current, one that could not be checked — and asks Upgrade or Cancel, starting on Cancel.
#[spec("upgrade/upgrade-all-dialog/001")]
#[test]
fn upgrade_all_dialog_001_the_plan_names_each_deck_and_starts_on_cancel() {
    let mut dialog = UpgradeAllDialog::new();
    let checking = draw(&mut dialog);
    assert!(
        checking.contains("Checking every remote deck on the deck list…"),
        "{checking}"
    );
    dialog.apply(&Update::Checked(Ok(plan())));
    let text = draw(&mut dialog);
    assert!(
        text.contains("> Cancel"),
        "Cancel is selected first: {text}"
    );
    insta::assert_snapshot!("upgrade_all_dialog_001_plan", text);
    assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Effect::Close);
}

/// Scenario: Start the batch and let the first daemon ask. The dialog shows the run's progress and that daemon's own question — its agents and orchestration role by name — with Keep current daemon selected, and Esc answers Keep.
#[spec("upgrade/upgrade-all-dialog/002")]
#[test]
fn upgrade_all_dialog_002_a_busy_daemon_asks_about_its_own_work() {
    let mut dialog = UpgradeAllDialog::new();
    dialog.apply(&Update::Checked(Ok(plan())));
    assert_eq!(dialog.choose(Choice::Upgrade), Effect::Start);
    dialog.apply(&started(0, "mac-studio"));
    dialog.apply(&Update::Event(BatchEvent::Progress {
        index: 0,
        deck: "mac-studio".into(),
        progress: UpgradeProgress {
            stage: UpgradeStage::Restarting,
            detail: None,
        },
    }));
    dialog.ask(Question {
        deck: "mac-studio".into(),
        at_stake: busy(),
        stale: false,
    });
    let text = draw(&mut dialog);
    assert!(text.contains("> Keep current daemon"), "{text}");
    insta::assert_snapshot!("upgrade_all_dialog_002_question", text);
    assert_eq!(
        dialog.handle_key(key(KeyCode::Esc)),
        Effect::Answer(RestartChoice::KeepCurrent)
    );
}

/// Scenario: While the second daemon upgrades, press Stop after this daemon. The dialog says it is stopping, offers no button, and the first deck's line already reads as kept running.
#[spec("upgrade/upgrade-all-dialog/003")]
#[test]
fn upgrade_all_dialog_003_stopping_waits_for_the_daemon_being_upgraded() {
    let mut dialog = UpgradeAllDialog::new();
    dialog.apply(&Update::Checked(Ok(plan())));
    dialog.choose(Choice::Upgrade);
    dialog.apply(&started(0, "mac-studio"));
    dialog.apply(&finished(
        0,
        DeckReport::new(
            "mac-studio",
            DeckResult::Ran {
                outcome: UpgradeOutcome::InstalledNotRestarted {
                    from_version: Some("0.51.0".into()),
                    installed_version: "0.52.0".into(),
                    reason: dot_agent_deck::daemon_upgrade::NotRestartedReason::KeptByUser {
                        at_stake: busy(),
                    },
                },
            },
        ),
    ));
    dialog.apply(&started(2, "build-box"));
    assert_eq!(dialog.selected(), Some(Choice::Stop));
    assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Effect::Stop);
    let text = draw(&mut dialog);
    assert!(text.contains("Stopping after this daemon…"), "{text}");
    insta::assert_snapshot!("upgrade_all_dialog_003_stopping", text);
}

/// Scenario: The batch ends. The dialog shows the counts and one line per deck — upgraded, skipped, failed, and not started because the user stopped — and Close.
#[spec("upgrade/upgrade-all-dialog/004")]
#[test]
fn upgrade_all_dialog_004_the_results_are_one_line_per_deck() {
    let mut dialog = UpgradeAllDialog::new();
    dialog.apply(&Update::Checked(Ok(plan())));
    dialog.choose(Choice::Upgrade);
    let report = BatchReport {
        decks: vec![
            DeckReport::new("mac-studio", restarted()),
            DeckReport::new(
                "pi",
                DeckResult::Skipped {
                    reason: SkipReason::Current {
                        version: "0.52.0".into(),
                    },
                },
            ),
            DeckReport::new("build-box", DeckResult::NotStarted),
            DeckReport::new(
                "laptop",
                DeckResult::Unreachable {
                    reason: "ssh: connect to host laptop port 22: No route to host".into(),
                },
            ),
        ],
    };
    dialog.apply(&Update::Finished(report));
    let text = draw(&mut dialog);
    assert!(
        text.contains("Upgrade all: 1 upgraded, 2 skipped, 1 failed of 4 remote decks."),
        "{text}"
    );
    insta::assert_snapshot!("upgrade_all_dialog_004_results", text);
    assert_eq!(dialog.buttons(), [Choice::Close]);
    assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Effect::Close);
}

/// Scenario: A busy daemon names more agents than the terminal can show. The dialog scrolls to the question, Restart now does nothing until PageDown has brought the last agent on screen, and Keep current daemon works at any time.
#[spec("upgrade/upgrade-all-dialog/005")]
#[test]
fn upgrade_all_dialog_005_restart_now_waits_until_every_agent_was_on_screen() {
    let mut dialog = UpgradeAllDialog::new();
    dialog.apply(&Update::Checked(Ok(plan())));
    dialog.choose(Choice::Upgrade);
    dialog.apply(&started(0, "mac-studio"));
    let many = RestartStopSet {
        agents: (0..30)
            .map(|i| RestartAgent {
                id: format!("a{i}"),
                label: format!("worker-{i:02}"),
                pane_id: Some(format!("{i}")),
                cwd: None,
            })
            .collect(),
        roles: Vec::new(),
    };
    dialog.ask(Question {
        deck: "mac-studio".into(),
        at_stake: many,
        stale: false,
    });
    let first = draw_at(&mut dialog, 100, 24);
    assert!(
        first.contains("Restarting the daemon on mac-studio stops 30 agents:"),
        "the question is scrolled into view: {first}"
    );
    assert!(
        !first.contains("worker-29"),
        "the list is longer than the screen: {first}"
    );
    assert!(
        first.contains("PgDn to read the whole list before Restart now"),
        "{first}"
    );
    assert!(!dialog.question_seen());
    assert_eq!(dialog.choose(Choice::RestartNow), Effect::None);

    while !dialog.question_seen() {
        dialog.handle_key(key(KeyCode::PageDown));
        let shown = draw_at(&mut dialog, 100, 24);
        if dialog.question_seen() {
            assert!(shown.contains("worker-29"), "{shown}");
        }
    }
    assert_eq!(
        dialog.choose(Choice::RestartNow),
        Effect::Answer(RestartChoice::RestartNow)
    );
}

/// A batch asking about 30 agents on mac-studio, the question just arrived.
fn asking_thirty() -> UpgradeAllDialog {
    let mut dialog = UpgradeAllDialog::new();
    dialog.apply(&Update::Checked(Ok(plan())));
    dialog.choose(Choice::Upgrade);
    dialog.apply(&started(0, "mac-studio"));
    dialog.ask(Question {
        deck: "mac-studio".into(),
        at_stake: RestartStopSet {
            agents: (0..30)
                .map(|i| RestartAgent {
                    id: format!("a{i}"),
                    label: format!("worker-{i:02}"),
                    pane_id: Some(format!("{i}")),
                    cwd: None,
                })
                .collect(),
            roles: Vec::new(),
        },
        stale: false,
    });
    dialog
}

/// Scenario: Press PageDown several times before the dialog draws again. The one frame drawn shows the end of the list, but the agents scrolled past were never on screen, so Restart now still does nothing until they have been.
#[spec("upgrade/upgrade-all-dialog/006")]
#[test]
fn upgrade_all_dialog_006_rows_scrolled_past_unseen_keep_restart_now_refused() {
    let mut dialog = asking_thirty();
    draw_at(&mut dialog, 100, 24);
    for _ in 0..10 {
        dialog.handle_key(key(KeyCode::PageDown));
    }
    let last = draw_at(&mut dialog, 100, 24);
    assert!(
        last.contains("worker-29"),
        "the end of the list is on screen: {last}"
    );
    assert!(
        !dialog.question_seen(),
        "the agents between the first frame and this one were never drawn"
    );
    assert_eq!(dialog.choose(Choice::RestartNow), Effect::None);
    // Paging back up through them, one frame at a time, shows every one.
    for _ in 0..10 {
        dialog.handle_key(key(KeyCode::PageUp));
        draw_at(&mut dialog, 100, 24);
    }
    assert!(dialog.question_seen());
    assert_eq!(
        dialog.choose(Choice::RestartNow),
        Effect::Answer(RestartChoice::RestartNow)
    );
}

/// Scenario: On a terminal so short that only a few rows of the list fit, each PageDown moves at most one screen, so paging to the end draws every agent on the way and Restart now becomes available only then.
#[spec("upgrade/upgrade-all-dialog/007")]
#[test]
fn upgrade_all_dialog_007_paging_on_a_short_terminal_skips_no_agent() {
    let mut dialog = asking_thirty();
    let mut drawn = draw_at(&mut dialog, 100, 13);
    let mut pages = 0;
    while !dialog.question_seen() {
        assert_eq!(dialog.choose(Choice::RestartNow), Effect::None);
        dialog.handle_key(key(KeyCode::PageDown));
        drawn.push_str(&draw_at(&mut dialog, 100, 13));
        pages += 1;
        assert!(pages < 100, "paging must reach the end: {drawn}");
    }
    for i in 0..30 {
        let name = format!("worker-{i:02}");
        assert!(
            drawn.contains(&name),
            "{name} was on screen before Restart now: {drawn}"
        );
    }
    assert_eq!(
        dialog.choose(Choice::RestartNow),
        Effect::Answer(RestartChoice::RestartNow)
    );
}
