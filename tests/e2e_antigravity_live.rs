#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Real interactive Antigravity, isolated credentials, attached dashboard.
dot_agent_deck_test_harness::link!();
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;

#[path = "support/antigravity_live.rs"]
mod antigravity;
#[path = "common/questions.rs"]
mod questions;

use antigravity::{antigravity_test_model, check_antigravity_available, seed_home};
use common::TuiDeck;
use inbox::Pane;
use serde_json::json;
use spec::spec;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const BUDGET: Duration = Duration::from_secs(120);
const CONTENT: &str = "antigravity-real-work-91cf4a";
const REPORT: &str = "AGY-REAL-REPORT-2c6f19";

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

struct Agy {
    deck: TuiDeck,
    pane: Pane,
    orchestrator: Option<Pane>,
}

impl Agy {
    fn start(team: bool) -> Self {
        Self::start_with_permissions(team, true)
    }

    fn start_with_permissions(team: bool, bypass: bool) -> Self {
        let deck = TuiDeck::builder()
            .with_pty_size(220, 60)
            .with_env("PATH", common::path_with_binary_dir())
            .with_env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
            .with_env("DOT_AGENT_DECK_LOG", "antigravity-live.log")
            .launch_with_fixture("minimal");
        deck.wait_for_string("No active agents");
        seed_home(deck.home_dir(), deck.workdir()).expect("seed isolated Antigravity HOME");
        let binary = common::contained_deck_binary();
        let installed = deck.home_dir().join(".local/bin/dot-agent-deck");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        if !installed.exists() && std::fs::hard_link(binary, &installed).is_err() {
            std::fs::copy(binary, &installed).unwrap();
        }
        let install = Command::new(&installed)
            .args(["hooks", "install", "--agent", "antigravity"])
            .env_clear()
            .env("HOME", deck.home_dir())
            .env("PATH", common::path_with_binary_dir())
            .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
            .current_dir(deck.workdir())
            .output()
            .expect("run the shipped Antigravity hook installer");
        assert!(
            install.status.success(),
            "Antigravity hook installer failed: {}",
            String::from_utf8_lossy(&install.stderr)
        );
        // Explicitly approve tools for this bounded scratch-file scenario:
        // real 1.5.9 still asked to create a file with --mode auto alone.
        let command = common::capability_export_command(&format!(
            "exec agy --mode auto {} --model {}",
            if bypass {
                "--dangerously-skip-permissions"
            } else {
                ""
            },
            quote(&antigravity_test_model())
        ));
        let membership = |role: &str, index, start| {
            json!({
                "kind":"orchestration","name":"agy-real","role_name":role,
                "role_index":index,"is_start_role":start,"orchestration_id":"agy-real-instance",
                "orchestration_cwd":deck.workdir()
            })
        };
        let orchestrator = if team {
            let orch_command = route::command(deck.workdir());
            let config = format!(
                "[[orchestrations]]\nname = 'agy-real'\n[[orchestrations.roles]]\nname = 'orchestrator'\ncommand = {}\nstart = true\n[[orchestrations.roles]]\nname = 'coder'\nagent = 'antigravity'\ncommand = {}\nclear = false\n",
                serde_json::to_string(&orch_command).unwrap(),
                serde_json::to_string(&command).unwrap()
            );
            std::fs::write(deck.workdir().join(".dot-agent-deck.toml"), config).unwrap();
            deck.approve_project_trust(deck.workdir());
            Some(route::start(
                &deck,
                "agy-real-orchestrator",
                json!({"tab_membership":membership("orchestrator",0,true)}),
            ))
        } else {
            None
        };
        let mut request = json!({"op":"start-agent","command":command,"agent_type":"antigravity",
            "cwd":deck.workdir(),"rows":48,"cols":175,"display_name":"live worker",
            "env":[["DOT_AGENT_DECK_PANE_ID","agy-real-worker"]]});
        if team {
            request["tab_membership"] = membership("coder", 1, false);
        }
        let reply = inbox::attach(&deck, request);
        assert_eq!(reply["ok"], true, "start real Antigravity: {reply}");
        let agent = reply["id"].as_str().unwrap().to_owned();
        let token = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(common::recorded_hook_capability(deck.workdir(), &agent))
            .unwrap();
        let pane = Pane {
            id: "agy-real-worker".into(),
            agent,
            token,
            pid: 0,
            capture: PathBuf::new(),
            attach_socket: deck.attach_socket_path().into(),
        };
        let result = Self {
            deck,
            pane,
            orchestrator,
        };
        assert!(
            common::wait_until(BUDGET, || result.screen().contains("? for shortcuts")),
            "real agy must reach the seeded interactive composer: {}",
            result.evidence()
        );
        // agy 1.5.9 displays a first-run announcement even after onboarding.
        // Dismiss it before the user types. Leave the delegated worker's box
        // untouched so its first prompt is genuinely the deck's doorbell.
        if !team
            && result
                .screen()
                .contains("Auto is now the default agent mode")
        {
            let mut input =
                common::AttachInput::attach(result.deck.attach_socket_path(), &result.pane.agent);
            input.write("\x1b");
            assert!(
                common::wait_until(Duration::from_secs(10), || !result
                    .screen()
                    .contains("Auto is now the default agent mode")),
                "dismiss the Auto-mode announcement: {}",
                result.evidence()
            );
        }
        // Open the actual agent pane in the attached TUI (second card for a team).
        if team {
            let (col, row) = result.deck.wait_for_in_grid("agy-real");
            result.deck.click(col + 1, row);
            result.deck.wait_for_string("coder");
        }
        result.deck.send_bytes(if team { b"2" } else { b"1" });
        assert!(
            result
                .deck
                .wait_for_grid_string_within("? for shortcuts", BUDGET),
            "real agy must be visible in attached TUI: {}",
            result.evidence()
        );
        result
    }

    fn screen(&self) -> String {
        let records = common::agent_records_on(self.deck.attach_socket_path());
        let record = records
            .iter()
            .find(|record| record.id == self.pane.agent)
            .expect("real Antigravity record");
        let mut parser = dot_agent_deck::vt_grid::parser(record.rows, record.cols, 0);
        parser.process(&common::pane_snapshot_on(
            self.deck.attach_socket_path(),
            &self.pane.agent,
        ));
        parser.screen().contents()
    }
    fn evidence(&self) -> String {
        format!(
            "Pane:\n{}\nGrid:\n{}\nLog:\n{}",
            self.screen(),
            self.deck.snapshot_grid(),
            std::fs::read_to_string(self.deck.workdir().join("antigravity-live.log"))
                .unwrap_or_default()
        )
    }
    fn card(&self, status: &str) {
        assert!(
            self.deck
                .wait_for_grid_predicate_within(BUDGET, |grid| grid.lines().any(|line| {
                    let title = line.split(['┐', '┓']).next().unwrap_or(line);
                    (title.starts_with('┌') || title.starts_with('┏'))
                        && title.contains("Antigravity")
                        && title.contains(status)
                })),
            "Antigravity card must show {status}: {}",
            self.evidence()
        );
    }
}

/// Scenario: Type a directive into real interactive Antigravity through the attached TUI asking it to create a uniquely named sentinel. Its Antigravity card shows the submitted prompt, visibly goes Working and then Idle, and the sentinel contains the requested unique content.
#[spec("antigravity/live/001")]
#[test]
fn antigravity_live_001_attached_real_agent_creates_a_sentinel_with_visible_status() {
    skip_unless!(check_antigravity_available());
    let agy = Agy::start(false);
    let sentinel = agy.deck.workdir().join("agy-attached-sentinel-91cf4a.txt");
    assert!(!sentinel.exists());
    let prompt = format!(
        "AGY-LIVE-91cf4a: Create the file {} with exactly this content: {CONTENT}. Use your file or shell tool now. Then reply DONE and end your turn. This is the complete task; do not ask questions.",
        sentinel.display()
    );
    agy.deck.send_bytes(prompt.as_bytes());
    assert!(
        common::wait_until(Duration::from_secs(10), || agy
            .screen()
            .contains("agy-attached-sentinel-91cf4a.txt")),
        "prompt must be visible in real composer: {}",
        agy.evidence()
    );
    agy.deck.send_bytes(b"\r");
    assert!(
        agy.deck
            .wait_for_grid_predicate_within(Duration::from_secs(10), |grid| grid.lines().any(
                |line| (line.starts_with("│Prmt:") || line.starts_with("┃Prmt:"))
                    && line.contains("AGY-LIVE-91cf4a")
            )),
        "Antigravity card must show the submitted prompt from its native hook: {}",
        agy.evidence()
    );
    agy.card("Working");
    common::wait_for_file_trimmed_eq(&sentinel, CONTENT, BUDGET).unwrap_or_else(|why| {
        panic!(
            "real Antigravity must write sentinel: {why}; {}",
            agy.evidence()
        )
    });
    agy.card("Idle");
}

/// Scenario: Delegate a task from an orchestration's start role to a real interactive Antigravity worker before its first prompt. The worker creates the unique sentinel and reports its task id through the real work-done CLI, whose framed report is fetched by the orchestrator.
#[spec("antigravity/live/002")]
#[test]
fn antigravity_live_002_real_worker_receives_delegate_and_reports_work_done() {
    skip_unless!(check_antigravity_available());
    let agy = Agy::start(true);
    let orchestrator = agy.orchestrator.as_ref().unwrap();
    let sentinel = agy.deck.workdir().join("agy-delegate-sentinel-2c6f19.txt");
    assert!(!sentinel.exists());
    let task = format!(
        "Create {} containing exactly {CONTENT}. Then run this shell command, replacing TASK_ID with this framed message's id: {} work-done --task-id TASK_ID --task {REPORT}. This is the complete task; use tools now, do not ask questions, then end your turn.",
        sentinel.display(),
        quote(common::contained_deck_binary().to_str().unwrap())
    );
    let id = route::delegate(&agy.deck, orchestrator, &task, false);
    common::wait_for_file_trimmed_eq(&sentinel, CONTENT, BUDGET).unwrap_or_else(|why| {
        panic!(
            "real Antigravity must act on delegated {id}: {why}; {}",
            agy.evidence()
        )
    });
    let fetches = orchestrator.capture.with_extension("fetches");
    assert!(
        common::wait_until(BUDGET, || {
            std::fs::write(orchestrator.capture.with_extension("fetch-now"), "").unwrap();
            std::fs::read_to_string(&fetches).is_ok_and(|text| {
                text.contains(REPORT) && text.contains(&id) && text.contains("UNTRUSTED")
            })
        }),
        "orchestrator must receive task-scoped work-done report for {id}: {}",
        agy.evidence()
    );
    agy.card("Idle");
}

/// Scenario: Ask genuine interactive Antigravity to remove a uniquely named scratch sentinel without pre-approving its tools. Its permission menu and Needs Input card appear before the file is removed; answering Allow Once through the deck's question API runs the command, removes the sentinel and lets the turn finish Idle.
#[spec("question/live/009")]
#[test]
fn question_live_009_antigravity_permission_allow_once_continues() {
    use dot_agent_deck::daemon_client::AnswerReport;
    use dot_agent_deck::question::{AnswerChannel, OptionRole, QuestionKind};

    skip_unless!(check_antigravity_available());
    let agy = Agy::start_with_permissions(false, false);
    let sentinel = agy.deck.workdir().join("question_agy_permission_836ab.txt");
    std::fs::write(&sentinel, "permission sentinel\n").unwrap();
    let mut input = common::AttachInput::attach(agy.deck.attach_socket_path(), &agy.pane.agent);
    input.write("question_agy_permission_836ab: Use run_command exactly once to run `rm -f question_agy_permission_836ab.txt`. Do not use another tool or change the command. After success reply DONE and stop. If denied, stop without retrying.");
    assert!(common::wait_until(Duration::from_secs(10), || agy
        .screen()
        .contains("If denied")));
    input.write("\r");
    assert!(
        common::wait_until(BUDGET, || agy.screen().contains("4.")),
        "the real Antigravity permission menu must appear: {}",
        agy.evidence()
    );
    let q = questions::pending(&agy.deck, &agy.pane.agent, QuestionKind::Permission);
    assert_eq!(q.channel, AnswerChannel::Keys);
    assert_eq!(q.questions[0].options[0].role, OptionRole::AllowOnce);
    assert!(!q.questions[0].options[0].keyboard_only);
    assert!(sentinel.exists(), "the command ran before approval");
    assert_eq!(
        questions::answer(
            &agy.deck,
            &agy.pane.agent,
            &q,
            vec![questions::selection(0, &[1])]
        ),
        AnswerReport::Answered
    );
    assert!(
        common::wait_until(BUDGET, || !sentinel.exists()),
        "the approved real command did not remove its sentinel: {}",
        agy.evidence()
    );
    questions::finished(&agy.deck, &agy.pane.agent);
    agy.card("Idle");
}
