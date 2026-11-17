#![cfg(all(feature = "e2e", unix))]

//! Antigravity started from a shell in a deck pane, rather than as the pane's
//! command: its own hooks name it, and the deck then reads its OSC 7501 status.

dot_agent_deck_test_harness::link!();
#[path = "common/questions.rs"]
mod questions;

use common::TuiDeck;
use serde_json::{Value, json};
use spec::spec;
use std::cell::Cell;
use std::time::Duration;

const QUESTION: &str = "Approve Bash: rm -f probe.txt";

struct Shell {
    deck: TuiDeck,
    step: Cell<usize>,
}

impl Shell {
    /// A deck whose one pane runs a plain `sh`, at whose prompt the test
    /// starts the `agy` stand-in, recording the opt-in the pane gave it.
    fn launch() -> Self {
        let deck = TuiDeck::builder()
            .without_agent_credentials()
            .with_pty_size(240, 50)
            .with_env(
                "AGY_TEST_DECK_BIN",
                common::contained_deck_binary().display().to_string(),
            )
            .with_continue_session("", "sh")
            .launch_with_fixture("antigravity");
        deck.wait_for_string("[Command Mode Ctrl+D]");
        // Typed at the shell's prompt; bytes typed before it is drawn wait
        // in the terminal for the shell to read them.
        deck.send_bytes(b"printf %s \"$AGY_CLI_OSC7501\" > osc-env; ./agy\r");
        assert!(
            common::wait_for_path(
                &deck.workdir().join("replay-ready"),
                Duration::from_secs(15)
            ),
            "stand-in never started from the shell:\n{}",
            deck.snapshot_grid()
        );
        deck.send_bytes(b"\x04");
        deck.wait_for_string("Dir:");
        Self {
            deck,
            step: Cell::new(0),
        }
    }

    fn replay(&self, actions: Value) {
        let step = self.step.get();
        self.step.set(step + 1);
        let request = self.deck.workdir().join(format!("request-{step}.json"));
        let temporary = request.with_extension("tmp");
        std::fs::write(&temporary, actions.to_string()).expect("write replay request");
        std::fs::rename(temporary, request).expect("publish complete replay request");
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

    /// The Antigravity card shows `label` and `detail`.
    fn card(&self, label: &str, detail: &str, moment: &str) {
        assert!(
            self.deck
                .wait_for_grid_predicate_within(Duration::from_secs(15), |grid| {
                    antigravity_title_shows(grid, label) && grid.contains(detail)
                }),
            "{moment}: the Antigravity card should show {label:?} with {detail:?}:\n{}",
            self.diagnostic()
        );
    }

    fn claude_tool(&self, moment: &str) {
        let records = common::agent_records_on(self.deck.attach_socket_path());
        assert_eq!(records.len(), 1, "{moment}: expected one shell card");
        let active_tool = records[0]
            .live
            .as_ref()
            .and_then(|snapshot| snapshot.active_tool.as_ref());
        assert_eq!(
            active_tool.map(|tool| (tool.name.as_str(), tool.detail.as_deref())),
            Some(("Bash", Some("echo claude-control"))),
            "{moment}: the daemon snapshot must keep the Claude command active:\n{records:#?}\n{}",
            self.diagnostic()
        );
    }
}

/// Whether the dashboard has an Antigravity card whose title reads `status`.
fn antigravity_title_shows(grid: &str, status: &str) -> bool {
    grid.lines().any(|line| {
        let title = line.split(['┐', '┓']).next().unwrap_or(line);
        (title.starts_with('┌') || title.starts_with('┏'))
            && title.contains("Antigravity")
            && title.contains(status)
    })
}

/// Scenario: Start a plain shell in a deck pane and launch the Antigravity stand-in from its prompt; a permission signal before any hook changes nothing. After its hooks, permission shows Needs Input with every option keyboard-only, Allow once and Deny through the client are refused without sending bytes, and idle clears the question. Once a Claude-shaped hook takes over the shell, Bash with detail echo claude-control stays active and Working despite later Antigravity signals, with no foreign question on the card.
#[spec("antigravity/shell/001")]
#[test]
fn antigravity_shell_001_agy_started_from_a_shell_reports_needs_input_and_interrupt() {
    use dot_agent_deck::daemon_client::AnswerReport;
    use dot_agent_deck::question::{AnswerChannel, AnswerRefusal, OptionRole, QuestionKind};

    let shell = Shell::launch();
    assert_eq!(
        std::fs::read_to_string(shell.deck.workdir().join("osc-env")).unwrap(),
        "on",
        "every deck pane asks Antigravity for its status report"
    );
    // The pane's output alone never switches the reader on.
    shell.replay(json!([{"osc":"blocked","kind":"permission","msg":QUESTION}]));
    assert!(
        !shell
            .deck
            .wait_for_grid_predicate_within(Duration::from_secs(2), |grid| grid
                .contains("Needs Input")
                || grid.contains(QUESTION)),
        "a status signal before any hook named the agent must change nothing:\n{}",
        shell.diagnostic()
    );
    shell.replay(json!([{"osc":"idle"}]));
    shell.replay(json!([
        {"hook": "session-start.json"},
        {"hook": "pre-invocation.json"}
    ]));
    shell.card("Thinking", "Run the shell command:", "first prompt");
    shell.replay(json!([
        {"hook": "pre-tool-use-run-command.json", "payload": {
            "toolCall": {"id": "shell-permission", "name": "run_command",
                "args": {"CommandLine": "rm -f probe.txt"}}}},
        {"osc":"blocked","kind":"permission","msg":QUESTION}
    ]));
    shell.card("Needs Input", QUESTION, "permission prompt");
    let id = questions::agent_id(&shell.deck);
    let question = questions::pending(&shell.deck, &id, QuestionKind::Permission);
    assert_eq!(question.channel, AnswerChannel::Unsupported);
    assert_eq!(question.questions.len(), 1);
    let options = &question.questions[0].options;
    assert_eq!(
        options.len(),
        4,
        "the complete permission menu: {question:?}"
    );
    assert!(
        options.iter().all(|option| option.keyboard_only),
        "a shell-started agy's grants and Deny must all be keyboard-only: {question:?}"
    );
    let input_log = shell.deck.workdir().join("input.bytes");
    assert!(std::fs::read(&input_log).unwrap().is_empty());
    for role in [OptionRole::AllowOnce, OptionRole::Deny] {
        let option = options
            .iter()
            .find(|option| option.role == role)
            .expect("the measured menu must offer Allow once and Deny");
        assert_eq!(
            questions::answer(
                &shell.deck,
                &id,
                &question,
                vec![questions::selection(0, &[option.index])],
            ),
            AnswerReport::Refused(AnswerRefusal::KeyboardOnly),
            "a shell-started agy's {role:?} answer must be refused"
        );
        assert!(
            !common::wait_until(Duration::from_millis(500), || {
                !std::fs::read(&input_log).unwrap().is_empty()
            }),
            "refused {role:?} must send zero bytes to the stand-in:\n{}",
            shell.diagnostic()
        );
        shell.card("Needs Input", QUESTION, "refused client answer");
    }
    // An Esc at the prompt: agy fires no hook, only its idle signal.
    shell.replay(json!([{"osc":"idle"}]));
    shell.card("Idle", "Dir:", "interrupted");
    assert!(
        !shell.deck.snapshot_grid().contains(QUESTION),
        "the interrupted prompt's question must clear:\n{}",
        shell.diagnostic()
    );
    shell.replay(json!([{"hook":"pre-tool-use-run-command.json", "agent":"claude-code"}]));
    // The card retains its display name from the Antigravity session; the
    // new hook's command and Working status are the takeover control.
    assert!(
        shell
            .deck
            .wait_for_grid_predicate_within(Duration::from_secs(15), |grid| {
                grid.contains("Working")
                    && grid.contains("echo claude-control")
                    && !grid.contains("Needs Input")
                    && !grid.contains(QUESTION)
            }),
        "another agent's hook must take over the shell card:\n{}",
        shell.diagnostic()
    );
    shell.claude_tool("Claude hook takeover");
    for signal in [
        json!({"osc":"blocked","kind":"permission","msg":QUESTION}),
        json!({"osc":"idle"}),
        json!({"osc":"done","msg":"DONE"}),
        json!({"osc":"error"}),
    ] {
        shell.replay(json!([signal]));
        assert!(
            !shell
                .deck
                .wait_for_grid_predicate_within(Duration::from_millis(700), |grid| {
                    !grid.contains("Working")
                        || !grid.contains("echo claude-control")
                        || grid.contains("Needs Input")
                        || grid.contains(QUESTION)
                }),
            "Antigravity OSC after Claude took over must be inert:\n{}",
            shell.diagnostic()
        );
        shell.claude_tool("Antigravity OSC after Claude takeover");
    }
}
