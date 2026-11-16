#![cfg(all(feature = "e2e", unix))]

//! Captured Antigravity hook/OSC replays against the real daemon and TUI.
//! The stand-in prints no status text: assertions read the dashboard card.

dot_agent_deck_test_harness::link!();
#[path = "common/questions.rs"]
mod questions;

use common::TuiDeck;
use serde_json::{Value, json};
use spec::spec;
use std::cell::Cell;
use std::time::{Duration, Instant};

const QUESTION: &str = "Approve Bash: rm -f probe.txt";
const LABELS: [&str; 7] = [
    "Thinking",
    "Working",
    "Needs Input",
    "Blocked",
    "Idle",
    "Compacting",
    "Error",
];

struct Replay {
    deck: TuiDeck,
    step: Cell<usize>,
}

impl Replay {
    fn launch(command: &str) -> Self {
        let deck = TuiDeck::builder()
            .without_agent_credentials()
            .with_pty_size(240, 50)
            .with_env(
                "AGY_TEST_DECK_BIN",
                common::contained_deck_binary().display().to_string(),
            )
            .with_continue_session("", command)
            .launch_with_fixture("antigravity");
        deck.wait_for_string("[Command Mode Ctrl+D]");
        assert!(
            common::wait_for_path(
                &deck.workdir().join("replay-ready"),
                Duration::from_secs(15)
            ),
            "stand-in never started:\n{}",
            deck.snapshot_grid()
        );
        deck.send_bytes(b"\x04");
        deck.wait_for_string("Dir:");
        Self {
            deck,
            step: Cell::new(0),
        }
    }

    fn send(&self, actions: Value) {
        let step = self.step.get();
        self.step.set(step + 1);
        let request = self.deck.workdir().join(format!("request-{step}.json"));
        let temporary = request.with_extension("tmp");
        std::fs::write(&temporary, actions.to_string()).expect("write replay request");
        std::fs::rename(temporary, request).expect("publish complete replay request");
    }

    fn replay(&self, actions: Value) {
        self.send(actions);
        let step = self.step.get() - 1;
        assert!(
            common::wait_for_path(
                &self.deck.workdir().join(format!("replayed-{step}")),
                Duration::from_secs(15)
            ),
            "stand-in never replayed step {step}:\n{}",
            self.diagnostic()
        );
    }

    fn diagnostic(&self) -> String {
        format!(
            "{}\nhook CLI replay:\n{}",
            self.deck.snapshot_grid(),
            std::fs::read_to_string(self.deck.workdir().join("hook-replay.log"))
                .unwrap_or_default()
        )
    }

    fn card(&self, label: &str, detail: &str, moment: &str) {
        assert!(
            self.deck
                .wait_for_grid_predicate_within(Duration::from_secs(15), |grid| {
                    shows_only(grid, label) && grid.contains(detail)
                }),
            "{moment}: card should show {label:?} with {detail:?}:\n{}",
            self.diagnostic()
        );
    }

    fn cleared_question(&self, label: &str, moment: &str) {
        assert!(
            self.deck
                .wait_for_grid_predicate_within(Duration::from_secs(15), |grid| {
                    shows_only(grid, label) && !grid.contains(QUESTION)
                }),
            "{moment}: card should show {label:?} without the old question:\n{}",
            self.diagnostic()
        );
    }
}

fn shows_only(grid: &str, label: &str) -> bool {
    grid.contains(label)
        && LABELS
            .iter()
            .filter(|other| **other != label)
            .all(|other| !grid.contains(other))
}

/// Scenario: Restore an Antigravity stand-in and replay its captured first-prompt, command-start, command-end and ordinary Stop hooks. The dashboard card shows Thinking with the submitted prompt, Working with the running command and finally Idle.
#[spec("antigravity/status/001")]
#[test]
fn antigravity_status_001_hooks_show_prompt_command_and_turn_end() {
    let replay = Replay::launch("./agy");
    replay.replay(json!([
        {"hook": "session-start.json"},
        {"hook": "pre-invocation.json"}
    ]));
    replay.card("Thinking", "Run the shell command:", "PreInvocation");
    replay.replay(json!([{"hook": "pre-tool-use-run-command.json"}]));
    replay.card(
        "Working",
        "run_command",
        "PreToolUse names the running tool",
    );
    replay.card("Working", "echo agy-probe-1", "PreToolUse command detail");
    replay.replay(json!([{"hook": "post-tool-use-run-command.json"}]));
    replay.replay(json!([{"hook": "stop.json"}]));
    replay.card("Idle", "", "Stop NO_TOOL_CALL");
}

/// Scenario: An Antigravity stand-in replays the captured Stop hook from a failed model request without printing its error to the terminal. The dashboard card reads Error and includes the hook's failure text.
#[spec("antigravity/status/002")]
#[test]
fn antigravity_status_002_error_stop_shows_its_error_text() {
    let replay = Replay::launch("./agy");
    replay.replay(json!([{"hook": "stop-error.json"}]));
    replay.card("Error", "generating and executing", "Stop ERROR");
}

/// Scenario: An Antigravity stand-in writes captured permission OSC signals followed by working or idle; the dashboard shows Needs Input with the decoded question and then clears it. For each measured permission option, the attached pane draws the real menu, the deck offers the same options and an answer sends the matching digit without Enter, with always-allow requiring confirmation, before the pane visibly resumes and the question clears.
#[spec("antigravity/status/003")]
#[test]
fn antigravity_status_003_permission_question_decodes_and_clears() {
    use dot_agent_deck::daemon_client::{AnswerReport, DaemonClient};
    use dot_agent_deck::question::{AnswerChannel, AnswerRefusal, OptionRole};

    let replay = Replay::launch("./agy");
    for recovery in ["working", "idle"] {
        replay.replay(json!([{"osc": "blocked", "kind": "permission", "msg": QUESTION}]));
        replay.card("Needs Input", QUESTION, "OSC permission");
        replay.replay(json!([{"osc": recovery}]));
        replay.cleared_question(
            if recovery == "working" {
                "Thinking"
            } else {
                "Idle"
            },
            recovery,
        );
    }

    // Measured against real agy 1.5.9 in a seeded scratch HOME on
    // 2026-11-16. permission-menu.json records its screen-only labels;
    // the OSC payload carries the question, never the option list.
    let menu: Value =
        serde_json::from_str(include_str!("fixtures/antigravity/permission-menu.json")).unwrap();
    for selected in 1..=4 {
        let replay = Replay::launch("./agy");
        replay.replay(json!([
            {"permission_menu": true},
            {"osc": "blocked", "kind": "permission", "msg": QUESTION}
        ]));
        replay.card("Needs Input", QUESTION, "measured permission menu");
        let id = questions::agent_id(&replay.deck);
        assert!(
            common::wait_until(Duration::from_secs(15), || {
                questions::snapshot(&replay.deck, &id)
                    .is_some_and(|live| live.pending_question.is_some())
            }),
            "Antigravity permission OSC must offer a PendingQuestion with its measured menu; live={:?}\n{}",
            questions::snapshot(&replay.deck, &id),
            replay.diagnostic()
        );
        let q = questions::snapshot(&replay.deck, &id)
            .unwrap()
            .pending_question
            .unwrap();
        assert_eq!(q.kind, dot_agent_deck::question::QuestionKind::Permission);
        assert_eq!(q.channel, AnswerChannel::Keys);
        assert_eq!(q.questions.len(), 1);
        assert_eq!(q.questions[0].prompt, QUESTION);
        assert!(!q.questions[0].multi_select);
        let options = &q.questions[0].options;
        assert_eq!(options.len(), 4, "the measured menu: {q:?}");
        for (option, expected) in options.iter().zip(menu["options"].as_array().unwrap()) {
            assert_eq!(
                option.index,
                expected["key"].as_str().unwrap().parse::<u32>().unwrap()
            );
            assert_eq!(option.label, expected["label"].as_str().unwrap());
            assert!(
                !option.keyboard_only,
                "measured option must be answerable: {option:?}"
            );
            assert!(!option.maybe_hidden);
        }
        assert_eq!(options[0].role, OptionRole::AllowOnce);
        assert_eq!(options[1].role, OptionRole::AllowAlways);
        assert_eq!(options[2].role, OptionRole::AllowAlways);
        assert_eq!(options[3].role, OptionRole::Deny);
        for option in &options[1..3] {
            assert!(
                option.scope.as_ref().is_some_and(|scope| !scope.is_empty()),
                "always allow must name what is approved: {option:?}"
            );
            assert!(
                option.scope.as_ref().unwrap().contains("rm -f probe.txt"),
                "scope must include the measured command prefix: {option:?}"
            );
        }
        assert!(options[1].scope.as_ref().unwrap().contains("conversation"));
        assert!(options[2].scope.as_ref().unwrap().contains("settings.json"));
        replay.deck.send_bytes(b"1");
        for expected in menu["options"].as_array().unwrap() {
            replay
                .deck
                .wait_for_string(expected["label"].as_str().unwrap());
        }
        let before = std::fs::read(replay.deck.workdir().join("input.bytes"))
            .unwrap_or_default()
            .len();
        let answers = vec![questions::selection(0, &[selected])];
        if (2..=3).contains(&selected) {
            assert_eq!(
                questions::answer(&replay.deck, &id, &q, answers.clone()),
                AnswerReport::Refused(AnswerRefusal::AlwaysNotConfirmed)
            );
            assert!(!replay.deck.workdir().join("permission-answer.txt").exists());
        }
        let client = DaemonClient::new(replay.deck.attach_socket_path().to_path_buf());
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(client.answer_question_while(|| true, &id, &q.id, q.revision, answers, true))
            .expect("answer the measured Antigravity permission");
        assert_eq!(result, AnswerReport::Answered);
        replay
            .deck
            .wait_for_string(&format!("AGY-PERMISSION-ANSWER-{selected}"));
        assert_eq!(
            std::fs::read_to_string(replay.deck.workdir().join("permission-answer.txt")).unwrap(),
            selected.to_string()
        );
        let captured = std::fs::read(replay.deck.workdir().join("input.bytes")).unwrap();
        assert_eq!(
            &captured[before..],
            selected.to_string().as_bytes(),
            "one measured digit, no Enter"
        );
        replay.deck.send_bytes(b"\x04");
        replay.cleared_question("Idle", "the selected permission answer");
        assert!(
            questions::snapshot(&replay.deck, &id)
                .unwrap()
                .pending_question
                .is_none()
        );
    }

    // File creation is a two-option menu, not Bash's four-option menu.
    // Its OSC wording was measured alongside the menu on agy 1.5.9.
    for selected in [1, 2] {
        let replay = Replay::launch("./agy");
        let prompt = format!(
            "Approve Edit: Write: {}",
            replay.deck.workdir().join("question-create.txt").display()
        );
        replay.replay(json!([{"permission_menu": "create", "prompt": prompt}]));
        let id = questions::agent_id(&replay.deck);
        let q = questions::pending(
            &replay.deck,
            &id,
            dot_agent_deck::question::QuestionKind::Permission,
        );
        assert_eq!(q.channel, AnswerChannel::Keys);
        let options = &q.questions[0].options;
        assert_eq!(
            options.len(),
            2,
            "file creation has no always-allow options: {q:?}"
        );
        assert_eq!(options[0].label, "Yes, allow creation");
        assert_eq!(options[0].role, OptionRole::AllowOnce);
        assert_eq!(options[1].label, "No, deny creation");
        assert_eq!(options[1].role, OptionRole::Deny);
        assert!(options.iter().all(|option| !option.keyboard_only));
        replay.deck.send_bytes(b"1");
        replay.deck.wait_for_string("1. Yes, allow creation");
        replay.deck.wait_for_string("2. No, deny creation");
        let before = std::fs::read(replay.deck.workdir().join("input.bytes"))
            .unwrap_or_default()
            .len();
        assert_eq!(
            questions::answer(
                &replay.deck,
                &id,
                &q,
                vec![questions::selection(0, &[selected])]
            ),
            AnswerReport::Answered
        );
        replay
            .deck
            .wait_for_string(&format!("AGY-PERMISSION-ANSWER-{selected}"));
        let captured = std::fs::read(replay.deck.workdir().join("input.bytes")).unwrap();
        assert_eq!(&captured[before..], selected.to_string().as_bytes());
    }

    // The captured folder-trust dialog has different labels and navigation.
    // Do not misrepresent it as an answerable Bash permission menu.
    let replay = Replay::launch("./agy");
    replay.replay(json!([{"osc": "blocked", "kind": "permission", "msg": "Trust this folder?"}]));
    replay.card("Needs Input", "Trust this folder?", "folder trust");
    let id = questions::agent_id(&replay.deck);
    assert!(
        questions::snapshot(&replay.deck, &id)
            .unwrap()
            .pending_question
            .is_none_or(|question| !question.has_answerable_option())
    );
}

/// Scenario: An Antigravity stand-in starts a tool through its hook and emits OSC working; the card stays Working with the command. After idle, OSC working shows Thinking before idle, done or error ends the turn without a Stop hook; the card shows Idle for idle and done, or Error with its decoded text.
#[spec("antigravity/status/004")]
#[test]
fn antigravity_status_004_osc_interrupt_done_and_error_need_no_stop_hook() {
    let replay = Replay::launch("./agy");
    replay.replay(json!([{"hook": "pre-tool-use-run-command.json"}]));
    replay.card("Working", "echo agy-probe-1", "PreToolUse command detail");
    replay.replay(json!([{"osc": "working"}]));
    assert!(
        !replay
            .deck
            .wait_for_grid_predicate_within(Duration::from_millis(700), |grid| {
                !shows_only(grid, "Working") || !grid.contains("echo agy-probe-1")
            }),
        "OSC working must preserve the hook-started tool's Working card:\n{}",
        replay.diagnostic()
    );
    replay.replay(json!([{"osc": "idle"}]));
    replay.card("Idle", "", "OSC idle after tool");
    for ending in ["idle", "done", "error"] {
        replay.replay(json!([{"osc": "working"}]));
        replay.card("Thinking", "", "OSC working before turn ends");
        replay.replay(json!([{"osc": ending}]));
        replay.card(
            if ending == "error" { "Error" } else { "Idle" },
            if ending == "error" {
                "There was a network issue"
            } else {
                ""
            },
            ending,
        );
    }
}

/// Scenario: Replay the measured startup auth signal followed by idle after 60 ms while watching the dashboard, then leave the same auth signal pending. The short launch transient never appears as Needs Input or Blocked; a persistent sign-in requirement does appear with its decoded question.
#[spec("antigravity/status/005")]
#[test]
fn antigravity_status_005_launch_auth_transient_is_hidden_but_persistent_auth_is_visible() {
    let replay = Replay::launch("./agy");
    // Measurements section 0 row 16: authenticated starts emit auth 10–60 ms
    // before idle. Use the longest observed gap, without prescribing a debounce.
    replay.send(json!([
        {"osc": "blocked", "kind": "auth"},
        {"pause_ms": 60},
        {"osc": "idle"}
    ]));
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        let grid = replay.deck.snapshot_grid();
        assert!(
            !grid.contains("Needs Input") && !grid.contains("Blocked"),
            "the measured 60 ms launch auth transient must not flash Blocked:\n{grid}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    replay.card("Idle", "", "authenticated launch ready");
    replay.replay(json!([{"osc": "blocked", "kind": "auth"}]));
    replay.card(
        "Needs Input",
        "Sign in required",
        "persistent auth requirement",
    );
    replay.replay(json!([{"osc": "idle"}]));
    replay.card("Idle", "", "sign-in completed");
}

/// Scenario: A stand-in launched as Claude Code uses a legitimate tool hook to put its card in Working, then emits Antigravity's permission, idle, done and error OSC signals. Its card keeps showing the Claude command and Working throughout, without displaying the foreign question or an error.
#[spec("antigravity/status/006")]
#[test]
fn antigravity_status_006_other_agent_ignores_osc7501() {
    let replay = Replay::launch("./claude");
    replay.replay(json!([{"hook": "pre-tool-use-run-command.json", "agent": "claude-code"}]));
    replay.card("Working", "echo claude-control", "Claude control hook");
    for action in [
        json!({"osc": "blocked", "kind": "permission", "msg": QUESTION}),
        json!({"osc": "idle"}),
        json!({"osc": "done"}),
        json!({"osc": "error"}),
    ] {
        replay.replay(json!([action]));
        assert!(
            !replay
                .deck
                .wait_for_grid_predicate_within(Duration::from_millis(700), |grid| {
                    !shows_only(grid, "Working")
                        || !grid.contains("echo claude-control")
                        || grid.contains(QUESTION)
                }),
            "a Claude Code card must ignore Antigravity OSC status:\n{}",
            replay.diagnostic()
        );
    }
}
