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
