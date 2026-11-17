#![cfg(all(feature = "e2e", unix))]

//! Measured Antigravity hook envelopes and orchestration delivery, no model.
dot_agent_deck_test_harness::link!();
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;

use common::TuiDeck;
use inbox::Pane;
use serde_json::{Value, json};
use spec::spec;
use std::cell::Cell;
use std::time::Duration;

const TASK: &str = "AGY-ROLE-TASK-c294: perform the delegated operation";
const REPORT: &str = "AGY-ROLE-REPORT-e871: delegated work complete";

struct Agy {
    pane: Pane,
    deck: TuiDeck,
    step: Cell<usize>,
}

impl Agy {
    fn start(team: bool) -> (Self, Option<Pane>) {
        let deck = TuiDeck::builder()
            .without_agent_credentials()
            .with_pty_size(240, 60)
            .with_env("DOT_AGENT_DECK_DOORBELL_TICK_MS", "25")
            .with_env("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "100")
            .launch_with_fixture("antigravity");
        deck.wait_for_string("No active agents");
        let command = common::capability_export_command("exec ./agy");
        let membership = |role: &str, index, start| {
            json!({
                "kind":"orchestration", "name":"agy-route", "role_name":role,
                "role_index":index, "is_start_role":start, "orchestration_id":"agy-route-instance",
                "orchestration_cwd":deck.workdir()
            })
        };
        let orchestrator = if team {
            let orchestrator_command = route::command(deck.workdir());
            let config = format!(
                "[[orchestrations]]\nname = 'agy-route'\n[[orchestrations.roles]]\nname = 'orchestrator'\ncommand = {}\nstart = true\n[[orchestrations.roles]]\nname = 'coder'\nagent = 'antigravity'\ncommand = {}\nclear = false\n",
                serde_json::to_string(&orchestrator_command).unwrap(),
                serde_json::to_string(&command).unwrap()
            );
            std::fs::write(deck.workdir().join(".dot-agent-deck.toml"), config).unwrap();
            deck.approve_project_trust(deck.workdir());
            Some(route::start(
                &deck,
                "agy-orchestrator",
                json!({
                    "tab_membership":membership("orchestrator", 0, true)
                }),
            ))
        } else {
            None
        };
        let mut request = json!({"op":"start-agent", "command":command,
            "agent_type":"antigravity", "cwd":deck.workdir(), "rows":50, "cols":180,
            "display_name":"Antigravity worker",
            "env":[["DOT_AGENT_DECK_PANE_ID","agy-worker"],
                ["AGY_TEST_DECK_BIN",common::contained_deck_binary()]]});
        if team {
            request["tab_membership"] = membership("coder", 1, false);
        }
        let reply = inbox::attach(&deck, request);
        assert_eq!(reply["ok"], true, "start Antigravity role: {reply}");
        let agent = reply["id"].as_str().unwrap().to_owned();
        assert!(common::wait_for_path(
            &deck.workdir().join("replay-pid"),
            common::child_boot_budget()
        ));
        let token = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(common::recorded_hook_capability(deck.workdir(), &agent))
            .unwrap();
        let pane = Pane {
            id: "agy-worker".into(),
            agent,
            token,
            pid: std::fs::read_to_string(deck.workdir().join("replay-pid"))
                .unwrap()
                .parse()
                .unwrap(),
            capture: deck.workdir().join("input.bytes"),
            attach_socket: deck.attach_socket_path().into(),
        };
        (
            Self {
                pane,
                deck,
                step: Cell::new(0),
            },
            orchestrator,
        )
    }

    fn replay(&self, actions: Value) {
        let n = self.step.get();
        self.step.set(n + 1);
        let request = self.deck.workdir().join(format!("request-{n}.json"));
        let temp = request.with_extension("tmp");
        std::fs::write(&temp, actions.to_string()).unwrap();
        std::fs::rename(temp, request).unwrap();
        assert!(
            common::wait_for_path(
                &self.deck.workdir().join(format!("replayed-{n}")),
                Duration::from_secs(15)
            ),
            "replay step {n} stalled: {}",
            self.diagnostic()
        );
    }

    fn hook(&self, fixture: &str, payload: Value, tag: &str) -> String {
        self.replay(json!([{"hook":fixture,"payload":payload,"tag":tag}]));
        let result = self.result(tag);
        assert_eq!(result["code"], 0, "hook {fixture} must succeed: {result}");
        result["stdout"].as_str().unwrap().into()
    }

    fn result(&self, tag: &str) -> Value {
        serde_json::from_str(
            &std::fs::read_to_string(self.deck.workdir().join(format!("{tag}.json"))).unwrap(),
        )
        .unwrap()
    }

    fn diagnostic(&self) -> String {
        format!(
            "{}\nhooks:\n{}\ninbox: {}",
            inbox::screen(&self.deck, &self.pane),
            std::fs::read_to_string(self.deck.workdir().join("hook-replay.log"))
                .unwrap_or_default(),
            inbox::listing(&self.deck, &self.pane)
        )
    }

    fn no_input(&self, moment: &str) {
        assert!(
            !common::wait_until(Duration::from_millis(500), || !inbox::bytes(&self.pane)
                .is_empty()),
            "doorbell typed {moment}: {:?}\n{}",
            inbox::bytes(&self.pane),
            self.diagnostic()
        );
    }

    fn committed_once(&self, id: &str) {
        inbox::state(&self.deck, &self.pane, id, "acknowledged");
        let list = inbox::listing(&self.deck, &self.pane);
        let message = list["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap();
        assert_eq!(message["deliveries"], 1, "one receipt delivery: {message}");
        assert_eq!(
            message["ack_how"], "plugin_confirmed",
            "hook committed its receipt: {message}"
        );
    }

    fn ring(&self) -> String {
        assert!(
            !self.deck.workdir().join("hook-replay.log").exists(),
            "first-prompt readiness must not depend on an earlier hook"
        );
        self.ring_next_turn()
    }

    fn ring_next_turn(&self) -> String {
        self.replay(json!([{"osc":"idle"}]));
        self.delivery()
    }

    fn delivery(&self) -> String {
        assert!(
            common::wait_for_path(
                &self.deck.workdir().join("doorbell-answer.json"),
                Duration::from_secs(20)
            ),
            "idle Antigravity must receive its doorbell: {}",
            self.diagnostic()
        );
        self.result("doorbell-answer")["stdout"]
            .as_str()
            .unwrap()
            .into()
    }

    fn visibly_delivered_once(&self, id: &str, body: &str) {
        let stdout = self.delivery();
        let answer: Value = serde_json::from_str(&stdout).expect("doorbell PreInvocation JSON");
        framed(
            answer["injectSteps"][0]["systemMessage"]["systemMessage"]
                .as_str()
                .expect("nested system message"),
            id,
            body,
        );
        assert!(
            common::wait_for_pane_text_on(
                self.deck.attach_socket_path(),
                &self.pane.agent,
                body,
                Duration::from_secs(10)
            ),
            "delegated task must be visible in the worker pane: {}",
            self.diagnostic()
        );
        self.committed_once(id);
        assert!(
            !common::wait_until(Duration::from_secs(1), || {
                std::fs::read_to_string(self.deck.workdir().join("submitted.jsonl"))
                    .unwrap_or_default()
                    .lines()
                    .count()
                    != 1
            }),
            "idle worker must receive exactly one doorbell: {}",
            self.diagnostic()
        );
        self.committed_once(id);
    }
}

fn framed(text: &str, id: &str, body: &str) {
    assert!(
        text.contains(&format!("[dot-agent-deck message {id} delivery 1.1 from "))
            && text.contains(body)
            && text.contains(&format!("[end of dot-agent-deck message {id}]")),
        "hook must hand over the framed task exactly once: {text:?}"
    );
    assert_eq!(text.matches(body).count(), 1, "duplicate task: {text}");
}

/// Scenario: Queue a task for an Antigravity stand-in before its first prompt and let the deck ring its idle composer. Its first invocation receives the measured nested system message; a later invocation with the same submitted line leaves a second task queued and the first delivered once.
#[spec("antigravity/inbox/001")]
#[test]
fn antigravity_inbox_001_doorbell_uses_nested_injection_without_reentry_delivery() {
    let (agy, _) = Agy::start(false);
    let id = inbox::enqueue(&agy.deck, &agy.pane, TASK);
    let stdout = agy.ring();
    let answer: Value = serde_json::from_str(&stdout).expect("PreInvocation JSON");
    let text = answer["injectSteps"][0]["systemMessage"]["systemMessage"]
        .as_str()
        .unwrap_or_else(|| panic!("measured nested PreInvocation envelope missing: {stdout:?}"));
    assert_eq!(
        answer,
        json!({"injectSteps":[{"systemMessage":{"systemMessage":text}}]})
    );
    framed(text, &id, TASK);
    agy.committed_once(&id);
    let submitted: Value = serde_json::from_str(
        std::fs::read_to_string(agy.deck.workdir().join("submitted.jsonl"))
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    let second = inbox::enqueue(&agy.deck, &agy.pane, "AGY-REENTRY-MUST-STAY-QUEUED");
    let again = agy.hook(
        "pre-invocation.json",
        json!({"invocationNum":1,"lastUserInput":submitted["line"]}),
        "reentry",
    );
    assert!(
        again.trim().is_empty() || again.trim() == "{}",
        "reentry must inject nothing: {again}"
    );
    agy.committed_once(&id);
    inbox::state(&agy.deck, &agy.pane, &second, "queued");
}

/// Scenario: While Antigravity is working, queue a task and replay its ordinary Stop hook; it continues with the framed task exactly once without typing into the pane. At execution ten, a queued task is retained without a continue response and reaches the next doorbell turn instead.
#[spec("antigravity/inbox/002")]
#[test]
fn antigravity_inbox_002_busy_stop_continues_with_framed_mail_once() {
    let (agy, _) = Agy::start(false);
    agy.hook("session-start.json", json!({}), "start");
    assert!(
        common::wait_until(Duration::from_secs(10), || inbox::listing(
            &agy.deck, &agy.pane
        )["wake"]
            == "turn_end"),
        "SessionStart must announce turn-end wake"
    );
    agy.hook("pre-invocation.json", json!({}), "working");
    let id = inbox::enqueue(&agy.deck, &agy.pane, TASK);
    let stdout = agy.hook("stop.json", json!({}), "stop");
    let answer: Value = serde_json::from_str(&stdout).unwrap_or_else(|error|
        panic!("busy Antigravity Stop needs decision:continue with framed reason: {error}; stdout={stdout:?}"));
    let reason = answer["reason"].as_str().expect("Stop reason");
    assert_eq!(answer, json!({"decision":"continue","reason":reason}));
    framed(reason, &id, TASK);
    agy.committed_once(&id);
    let again = agy.hook("stop.json", json!({"executionNum":1}), "stop-again");
    assert!(
        again.trim().is_empty() || again.trim() == "{}",
        "empty reentered Stop: {again}"
    );
    agy.committed_once(&id);
    agy.no_input("during busy hand-over");

    agy.hook("pre-invocation.json", json!({}), "limit-working");
    let limited = inbox::enqueue(&agy.deck, &agy.pane, "AGY-LIMIT-RETAINED-e841");
    let stdout = agy.hook("stop.json", json!({"executionNum":10}), "limit-stop");
    assert!(
        stdout.trim().is_empty() || stdout.trim() == "{}",
        "executionNum 10 cannot continue; queued mail must wait for the next turn: {stdout:?}"
    );
    inbox::state(&agy.deck, &agy.pane, &limited, "queued");
    let stdout = agy.ring_next_turn();
    let answer: Value = serde_json::from_str(&stdout).expect("next doorbell PreInvocation JSON");
    framed(
        answer["injectSteps"][0]["systemMessage"]["systemMessage"]
            .as_str()
            .unwrap(),
        &limited,
        "AGY-LIMIT-RETAINED-e841",
    );
    agy.committed_once(&limited);
}

/// Scenario: Queue mail while Antigravity works and replay an error Stop. It hands over nothing and leaves the task queued, so the next successful Stop can deliver that same task with a continue decision.
#[spec("antigravity/inbox/003")]
#[test]
fn antigravity_inbox_003_error_does_not_take_pending_mail() {
    let (agy, _) = Agy::start(false);
    agy.hook("session-start.json", json!({}), "start");
    agy.hook("pre-invocation.json", json!({}), "working");
    let id = inbox::enqueue(&agy.deck, &agy.pane, TASK);
    let error = agy.hook("stop-error.json", json!({}), "error");
    assert!(
        error.trim().is_empty() || error.trim() == "{}",
        "ERROR must not hand over: {error}"
    );
    inbox::state(&agy.deck, &agy.pane, &id, "queued");
    let stdout = agy.hook("stop.json", json!({"executionNum":1}), "recovered");
    let answer: Value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!(
            "mail retained on ERROR must reach the next ordinary Stop: {error}; stdout={stdout:?}"
        )
    });
    assert_eq!(answer["decision"], "continue");
    framed(answer["reason"].as_str().unwrap(), &id, TASK);
    agy.committed_once(&id);
}

/// Scenario: Queue mail for an idle Antigravity stand-in whose first doorbell echo has a seven-column gap although every input byte arrived. The deck submits nothing from that mismatched attempt, withdraws its composer text, and later delivers and acknowledges that same message exactly once.
#[spec("antigravity/inbox/004")]
#[test]
fn antigravity_inbox_004_misrendered_echo_is_withdrawn_and_retried_once() {
    let (agy, _) = Agy::start(false);
    agy.replay(json!([{"misrender_echo_once":true}]));
    let id = inbox::enqueue(&agy.deck, &agy.pane, "AGY-ECHO-RECOVER-e286");
    agy.replay(json!([{"osc":"idle"}]));
    assert!(
        common::wait_for_path(
            &agy.deck.workdir().join("mismatched-echo.json"),
            Duration::from_secs(20)
        ),
        "stand-in must misrender the first doorbell: {}",
        agy.diagnostic()
    );
    assert!(
        common::wait_for_path(
            &agy.deck.workdir().join("echo-withdrawn.json"),
            Duration::from_secs(15)
        ),
        "mismatched doorbell echo must be withdrawn without submitting: echo={} input={:?}\n{}",
        std::fs::read_to_string(agy.deck.workdir().join("mismatched-echo.json")).unwrap(),
        String::from_utf8_lossy(&inbox::bytes(&agy.pane)),
        agy.diagnostic()
    );
    assert_eq!(
        agy.result("echo-withdrawn")["submitted_before_withdrawal"],
        0,
        "the mismatched attempt must not press Enter"
    );
    assert!(
        common::wait_for_path(
            &agy.deck.workdir().join("doorbell-answer.json"),
            Duration::from_secs(35)
        ),
        "withdrawn doorbell must retry and deliver its retained mail: {}",
        agy.diagnostic()
    );
    let answer: Value =
        serde_json::from_str(agy.result("doorbell-answer")["stdout"].as_str().unwrap()).unwrap();
    framed(
        answer["injectSteps"][0]["systemMessage"]["systemMessage"]
            .as_str()
            .unwrap(),
        &id,
        "AGY-ECHO-RECOVER-e286",
    );
    agy.committed_once(&id);
    assert!(
        common::wait_for_pane_text_on(
            agy.deck.attach_socket_path(),
            &agy.pane.agent,
            "AGY-ECHO-RECOVER-e286",
            Duration::from_secs(10)
        ),
        "recovered mail must be visible in the pane: {}",
        agy.diagnostic()
    );
    // Span several of this fixture's 100 ms ring intervals. Both the input
    // log and receipt must still show exactly one successful hand-over.
    assert!(
        !common::wait_until(Duration::from_secs(1), || {
            std::fs::read_to_string(agy.deck.workdir().join("submitted.jsonl"))
                .unwrap_or_default()
                .lines()
                .count()
                != 1
        }),
        "recovery must submit exactly one faithful doorbell: {}",
        agy.diagnostic()
    );
    agy.committed_once(&id);
}

/// Scenario: A person answers a permission dialog in an Antigravity worker through the deck's attached input path and the turn finishes Idle. A task queued afterwards appears in the worker pane exactly once, without another manual keystroke.
#[spec("antigravity/inbox/005")]
#[test]
fn antigravity_inbox_005_human_permission_answer_does_not_disable_later_doorbells() {
    let (agy, _) = Agy::start(false);
    agy.replay(json!([
        {"permission_menu":true},
        {"osc":"blocked", "kind":"permission"}
    ]));
    assert!(common::wait_for_pane_text_on(
        agy.deck.attach_socket_path(),
        &agy.pane.agent,
        "1. Yes, run command",
        Duration::from_secs(10)
    ));
    let _person = inbox::input(&agy.deck, &agy.pane, "1");
    assert!(
        common::wait_for_pane_text_on(
            agy.deck.attach_socket_path(),
            &agy.pane.agent,
            "AGY-PERMISSION-ANSWER-1",
            Duration::from_secs(10)
        ),
        "person's answer must clear the dialog and restore the composer: {}",
        agy.diagnostic()
    );
    assert!(common::wait_until(Duration::from_secs(10), || {
        std::fs::read_to_string(agy.deck.workdir().join("osc-history.jsonl")).is_ok_and(|history| {
            history
                .lines()
                .last()
                .is_some_and(|line| serde_json::from_str::<Value>(line).unwrap()["osc"] == "idle")
        })
    }));
    let body = "AGY-AFTER-HUMAN-ANSWER-c872";
    let id = inbox::enqueue(&agy.deck, &agy.pane, body);
    agy.visibly_delivered_once(&id, body);
}

/// Scenario: A task queued while a genuine numbered permission menu is visible is not typed into that menu, even with an idle status signal. When the menu clears, numbered task output above an empty ready composer does not prevent the task from appearing in the worker pane exactly once.
#[spec("antigravity/inbox/006")]
#[test]
fn antigravity_inbox_006_numbered_output_allows_delivery_but_permission_menu_holds_it() {
    let (agy, _) = Agy::start(false);
    agy.replay(json!([{"permission_menu":true}, {"osc":"idle"}]));
    assert!(common::wait_for_pane_text_on(
        agy.deck.attach_socket_path(),
        &agy.pane.agent,
        "1. Yes, run command",
        Duration::from_secs(10)
    ));
    let body = "AGY-NUMBERED-OUTPUT-45fd";
    let id = inbox::enqueue(&agy.deck, &agy.pane, body);
    agy.no_input("while the numbered permission menu is visible despite idle");
    inbox::state(&agy.deck, &agy.pane, &id, "queued");
    agy.replay(json!([
        {"output":"1. fix the lint gate\n2. verify the delegated operation"},
        {"redraw_composer":true}
    ]));
    assert!(common::wait_for_pane_text_on(
        agy.deck.attach_socket_path(),
        &agy.pane.agent,
        "1. fix the lint gate",
        Duration::from_secs(10)
    ));
    agy.visibly_delivered_once(&id, body);
}

/// Scenario: A person types a draft into an idle Antigravity worker and pending delegation leaves that draft untouched. In fresh workers they answer the folder-trust dialog with Enter or Down, Down, Enter through attached input; each subsequent delegated task visibly arrives and is acknowledged exactly once.
#[spec("antigravity/inbox/007")]
#[test]
fn antigravity_inbox_007_trust_dialog_answers_allow_delegation_but_a_draft_holds_it() {
    // Control: genuine composer input must continue to hold the doorbell.
    {
        let (agy, orchestrator) = Agy::start(true);
        agy.replay(json!([{"osc":"idle"}]));
        let _person = inbox::input(&agy.deck, &agy.pane, "x");
        assert!(common::wait_for_pane_text_on(
            agy.deck.attach_socket_path(),
            &agy.pane.agent,
            "> x",
            Duration::from_secs(10)
        ));
        let before = inbox::bytes(&agy.pane);
        let id = route::delegate(&agy.deck, &orchestrator.unwrap(), TASK, false);
        assert!(
            !common::wait_until(Duration::from_secs(1), || inbox::bytes(&agy.pane) != before),
            "delegation must leave the person's draft alone: {}",
            agy.diagnostic()
        );
        inbox::state(&agy.deck, &agy.pane, &id, "queued");
    }
    for (answer, label) in [("\r", "Enter"), ("\x1b[B\x1b[B\r", "Down, Down, Enter")] {
        eprintln!("folder-trust answer case: {label}");
        let (agy, orchestrator) = Agy::start(true);
        agy.replay(json!([{"trust_dialog":true}]));
        assert!(common::wait_for_pane_text_on(
            agy.deck.attach_socket_path(),
            &agy.pane.agent,
            "> Yes, I trust this folder",
            Duration::from_secs(10)
        ));
        let _person = inbox::input(&agy.deck, &agy.pane, answer);
        assert!(
            common::wait_for_pane_text_on(
                agy.deck.attach_socket_path(),
                &agy.pane.agent,
                "AGY-FOLDER-TRUSTED",
                Duration::from_secs(10)
            ),
            "{label} must dismiss the trust dialog: {}",
            agy.diagnostic()
        );
        let id = route::delegate(&agy.deck, &orchestrator.unwrap(), TASK, false);
        agy.visibly_delivered_once(&id, TASK);
    }
}

/// Scenario: Subscribe to an Antigravity pane's completed replies: a workspace transcript publishes nothing, while a plain Stop with a sanitized transcript in its conversation's HOME directory publishes its final text once. A Stop handed queued mail returns continue without publishing an interim reply, and the final continued Stop publishes the new reply once.
#[spec("voice/reading-reply/007")]
#[test]
fn reading_reply_007_antigravity_transcript_publishes_only_finished_turns() {
    use dot_agent_deck::daemon_client::{DaemonClient, GatedQuery};

    let (agy, _) = Agy::start(false);
    agy.hook("session-start.json", json!({}), "reading-start");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = DaemonClient::new(agy.deck.attach_socket_path().to_path_buf());
    let GatedQuery::Answered(mut replies) = runtime
        .block_on(client.subscribe_turn_replies(&agy.pane.agent))
        .expect("subscribe to Antigravity reading replies")
    else {
        panic!("daemon must advertise turn-replies");
    };
    let conversation = "00000000-0000-4000-8000-000000000001";
    let transcript = agy.deck.home_dir().join(format!(
        ".gemini/antigravity-cli/brain/{conversation}/.system_generated/logs/transcript_full.jsonl"
    ));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let captured = include_str!("fixtures/antigravity/transcript-full-after-continue.jsonl");
    // Retain the measured USER_INPUT/PLANNER_RESPONSE records for the plain
    // turn, without the later SYSTEM_MESSAGE and continued reply.
    std::fs::write(
        &transcript,
        captured.lines().take(4).collect::<Vec<_>>().join("\n") + "\n",
    )
    .unwrap();
    agy.hook("pre-invocation.json", json!({}), "reading-plain-start");
    let outside = agy.deck.workdir().join("reading-transcript.jsonl");
    std::fs::copy(&transcript, &outside).unwrap();
    agy.hook(
        "stop-before-continue.json",
        json!({"transcriptPath":outside,"conversationId":conversation}),
        "reading-workspace-stop",
    );
    runtime.block_on(async {
        assert!(
            tokio::time::timeout(Duration::from_millis(500), replies.next_reply())
                .await
                .is_err(),
            "a workspace transcript must publish no reply"
        );
    });
    agy.hook("pre-invocation.json", json!({}), "reading-valid-start");
    agy.hook(
        "stop-before-continue.json",
        json!({"transcriptPath":transcript,"conversationId":conversation}),
        "reading-plain-stop",
    );
    let plain = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), replies.next_reply())
            .await
            .expect("plain Stop must publish its transcript reply")
            .expect("decode plain reply")
            .expect("reading stream remains live")
    });
    assert_eq!(plain.agent_id, agy.pane.agent);
    assert_eq!(plain.pane_id, agy.pane.id);
    assert_eq!(plain.reply.text, "FIRST-REPLY-3307");
    assert!(!plain.reply.failed);
    runtime.block_on(async {
        assert!(
            tokio::time::timeout(Duration::from_millis(500), replies.next_reply())
                .await
                .is_err(),
            "plain turn reply must arrive once"
        );
    });

    agy.hook("pre-invocation.json", json!({}), "reading-busy-start");
    let id = inbox::enqueue(&agy.deck, &agy.pane, "AGY-READING-CONTINUE-78bc");
    let stdout = agy.hook(
        "stop-before-continue.json",
        json!({"transcriptPath":transcript,"conversationId":conversation}),
        "reading-continue",
    );
    let answer: Value = serde_json::from_str(&stdout).expect("continue response");
    assert_eq!(answer["decision"], "continue");
    framed(
        answer["reason"].as_str().unwrap(),
        &id,
        "AGY-READING-CONTINUE-78bc",
    );
    agy.committed_once(&id);
    runtime.block_on(async {
        assert!(
            tokio::time::timeout(Duration::from_millis(500), replies.next_reply())
                .await
                .is_err(),
            "a Stop handed a continue must publish no interim reading reply"
        );
    });
    std::fs::write(&transcript, captured).unwrap();
    agy.hook(
        "pre-invocation-after-continue.json",
        json!({}),
        "reading-reentry",
    );
    agy.hook(
        "stop-after-continue.json",
        json!({"transcriptPath":transcript,"conversationId":conversation}),
        "reading-final-stop",
    );
    let continued = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), replies.next_reply())
            .await
            .expect("completed continued turn must publish its final reply")
            .expect("decode continued reply")
            .expect("reading stream remains live")
    });
    assert_eq!(continued.agent_id, agy.pane.agent);
    assert_eq!(continued.pane_id, agy.pane.id);
    assert_eq!(continued.reply.text, "CONTINUED-REPLY-8812");
    assert!(!continued.reply.failed);
    assert!(continued.sequence > plain.sequence);
    runtime.block_on(async {
        assert!(
            tokio::time::timeout(Duration::from_millis(500), replies.next_reply())
                .await
                .is_err(),
            "continued turn reply must arrive once"
        );
    });
}

/// Scenario: Delegate from an orchestration to its Antigravity role before its first prompt, holding launch auth and then a permission dialog. Neither accepts input; once OSC reports idle, the role displays its task in the pane scrollback through the doorbell and its real work-done CLI sends the report back to the orchestrator.
#[spec("orchestration/route/003")]
#[test]
fn route_003_antigravity_role_waits_for_idle_and_reports_work_done() {
    let (agy, orchestrator) = Agy::start(true);
    let orchestrator = orchestrator.unwrap();
    std::fs::write(agy.deck.workdir().join("complete-delegate"), "").unwrap();
    let id = route::delegate(&agy.deck, &orchestrator, TASK, false);
    agy.no_input("before the first OSC idle");
    agy.replay(json!([{"osc":"blocked","kind":"auth"},{"pause_ms":60},{"osc":"blocked","kind":"permission"}]));
    agy.no_input("while permission blocks the launch");
    agy.ring();
    let submitted: Value = serde_json::from_str(
        std::fs::read_to_string(agy.deck.workdir().join("submitted.jsonl"))
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        submitted["state"], "idle",
        "doorbell entered a blocked composer: {submitted}"
    );
    assert!(
        !String::from_utf8_lossy(&inbox::bytes(&agy.pane)).contains(TASK),
        "task content must travel through the hook"
    );
    // The framed work-done instructions can push the task above a 40-row
    // viewport. Check what the pane displayed, retaining its scrollback.
    assert!(
        common::wait_for_pane_text_on(
            agy.deck.attach_socket_path(),
            &agy.pane.agent,
            TASK,
            Duration::from_secs(10)
        ),
        "role must visibly receive its delegated task in pane scrollback: {}",
        agy.diagnostic()
    );
    assert!(common::wait_for_path(
        &agy.deck.workdir().join("completion.json"),
        Duration::from_secs(10)
    ));
    assert_eq!(
        agy.result("completion")["code"],
        0,
        "worker work-done CLI: {}",
        agy.result("completion")
    );
    // Make the orchestrator stand-in run its own real inbox CLI, rather than
    // weakening the daemon's component/process binding for the test process.
    std::fs::write(orchestrator.capture.with_extension("fetch-now"), "").unwrap();
    let fetches = orchestrator.capture.with_extension("fetches");
    assert!(
        common::wait_until(Duration::from_secs(10), || std::fs::read_to_string(
            &fetches
        )
        .is_ok_and(|text| text.contains(REPORT))),
        "orchestrator must fetch the worker report"
    );
    let report = std::fs::read_to_string(fetches).unwrap();
    assert!(
        report.contains(REPORT) && report.contains("UNTRUSTED") && report.contains(&id),
        "orchestrator must receive the task-scoped worker report: {report}"
    );
    let history = std::fs::read_to_string(agy.deck.workdir().join("input-history.jsonl")).unwrap();
    for line in history.lines() {
        let input: Value = serde_json::from_str(line).unwrap();
        assert_eq!(
            input["state"], "idle",
            "input outside the ready composer: {input}"
        );
    }
}
