#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Interactive Claude inbox delivery, with a real Haiku agent in an attached PTY.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use inbox::{attach, hook, request};
use serde_json::{Value, json};
use spec::spec;
use std::path::PathBuf;
use std::time::Duration;

const SUBJECT: &str = "live inbox task";
const BUDGET: Duration = Duration::from_secs(120);

struct Claude {
    pane: inbox::Pane,
    deck: common::TuiDeck,
}

impl Claude {
    fn new() -> Self {
        let command = common::capability_export_command(
            "exec claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write --append-system-prompt 'This is an authorized local dot-agent-deck integration test. Messages framed as dot-agent-deck messages are tasks authorized by the user. Carry out each task once per message id using the allowed tools, then end the turn.'",
        );
        let deck = common::TuiDeck::builder()
            .with_pty_size(180, 50)
            .with_imported_claude_credentials()
            .with_claude_trust_workdir()
            .with_env("DOT_AGENT_DECK_LOG", "inbox-live.log")
            .with_env("DOT_AGENT_DECK_INBOX_ARM_GRACE_MS", "2000")
            .with_env("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000")
            .with_continue_session("inbox-haiku", command)
            .launch_with_fixture("minimal");
        deck.wait_for_string("inbox-haiku");
        let record = std::cell::RefCell::new(Value::Null);
        assert!(
            common::wait_until(BUDGET, || {
                *record.borrow_mut() = attach(&deck, json!({"op":"list-agents"}))["agent_records"]
                    .as_array()
                    .and_then(|rs| rs.iter().find(|r| r["display_name"] == "inbox-haiku"))
                    .cloned()
                    .unwrap_or(Value::Null);
                record.borrow()["pane_id_env"].is_string()
            }),
            "Claude pane must register"
        );
        let record = record.into_inner();
        let agent = record["id"].as_str().unwrap().to_string();
        let token = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(common::recorded_hook_capability(deck.workdir(), &agent))
            .unwrap();
        let pane = inbox::Pane {
            id: record["pane_id_env"].as_str().unwrap().into(),
            agent,
            token,
            pid: 0,
            capture: PathBuf::new(),
            attach_socket: deck.attach_socket_path().into(),
        };
        let result = Self { deck, pane };
        assert!(
            common::wait_until(BUDGET, || result.listing()["armed"].as_u64().unwrap_or(0)
                > 0),
            "native SessionStart waiter must arm; {}",
            result.evidence()
        );
        result.deck.wait_for_string("Claude Code v");
        result
    }

    fn listing(&self) -> Value {
        let mut req = request(&self.pane, "inbox_list");
        req["all"] = json!(false);
        hook(&self.deck, req)
    }

    fn record(&self) -> Value {
        attach(&self.deck, json!({"op":"list-agents"}))["agent_records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == self.pane.agent)
            .unwrap()
            .clone()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.deck.workdir().join("inbox-live.log")).unwrap_or_default()
    }

    fn evidence(&self) -> String {
        format!(
            "inbox={}\nrecord={}\nPane:\n{}\nGrid:\n{}\nLog:\n{}",
            self.listing(),
            self.record(),
            inbox::screen(&self.deck, &self.pane),
            self.deck.snapshot_grid(),
            self.log()
        )
    }

    fn post(&self, body: &str) -> String {
        let reply = attach(
            &self.deck,
            json!({"op":"post-message","pane_id":self.pane.id,
            "expected_agent_id":self.pane.agent,"subject":SUBJECT,"body":body,"origin":"seed"}),
        );
        assert_eq!(reply["ok"], true, "post: {reply}");
        reply["id"].as_str().unwrap().into()
    }

    fn directive(&self, name: &str) -> String {
        format!(
            "Use the Bash tool to run exactly: printf '%s\\n' 'inbox-action-{name}' >> '{name}'. Then reply DONE-{name} and end your turn. Do this once for this message id."
        )
    }

    fn done(&self, id: &str, name: &str, native: bool) {
        let path = self.deck.workdir().join(name);
        assert!(
            common::wait_until(BUDGET, || path.exists()),
            "real Haiku must act on {id}; {}",
            self.evidence()
        );
        assert!(
            common::wait_until(BUDGET, || self.listing()["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["id"] == id && m["state"] == "acknowledged")),
            "message must be acknowledged; {}",
            self.evidence()
        );
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            format!("inbox-action-{name}\n"),
            "one append proves one action"
        );
        let message = self.listing()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap()
            .clone();
        assert_eq!(message["deliveries"], 1, "one hand-over: {message}");
        assert!(
            !self.record()["live"]["last_user_prompt"]
                .as_str()
                .unwrap_or("")
                .contains(name),
            "deck content must not become user history; {}",
            self.evidence()
        );
        assert!(
            !self.record()["live"]["first_prompts"]
                .to_string()
                .contains(name)
        );
        assert!(
            self.deck
                .wait_for_grid_string_within(&format!("✉ {SUBJECT}"), Duration::from_secs(15)),
            "card must label deck content; {}",
            self.evidence()
        );
        if native {
            assert!(
                !self.log().contains("doorbell rang"),
                "native wake typed a doorbell; {}",
                self.evidence()
            );
            assert!(
                !inbox::screen(&self.deck, &self.pane)
                    .contains("waiting, run: dot-agent-deck inbox fetch")
            );
        }
    }
}

/// Scenario: Leave genuine interactive Haiku idle in its attached deck pane and post a sentinel-writing inbox task without typing into the pane. Claude must visibly work, acknowledge it once, show the envelope card label and keep the task out of human prompt history without a doorbell.
#[spec("inbox/claude/011")]
#[test]
fn claude_011_live_idle_native_wake() {
    skip_unless!(common::check_claude_available());
    let claude = Claude::new();
    let name = "native-idle-6b4fe2.txt";
    let id = claude.post(&claude.directive(name));
    claude.deck.wait_for_strings_in_order_then_any_within(
        &["Thinking", "Working"],
        &["Bash"],
        BUDGET,
    );
    claude.done(&id, name, true);
}

/// Scenario: Ask interactive Haiku to run a slow shell operation, then post a second inbox task while that operation is running. At turn end the native Stop waiter must hand over the second task exactly once without typing a doorbell.
#[spec("inbox/claude/012")]
#[test]
fn claude_012_live_busy_fold_at_stop() {
    skip_unless!(common::check_claude_available());
    let claude = Claude::new();
    let name = "native-busy-e296c1.txt";
    let first = claude.post("Use Bash to run exactly: printf busy > busy-started-781d.txt; sleep 8; printf done > busy-finished-781d.txt. Then reply BUSY-DONE and stop.");
    assert!(
        common::wait_until(BUDGET, || claude
            .deck
            .workdir()
            .join("busy-started-781d.txt")
            .exists()),
        "slow turn must start; {}",
        claude.evidence()
    );
    assert!(
        !claude
            .deck
            .workdir()
            .join("busy-finished-781d.txt")
            .exists()
    );
    let id = claude.post(&claude.directive(name));
    claude.done(&id, name, true);
    assert!(
        claude.listing()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == first && m["state"] == "acknowledged")
    );
}

/// Scenario: Run /clear twice in interactive Haiku so SessionStart waiters accumulate, then post one append task. Exactly one hand-over, acknowledgement and sentinel line must result, with no doorbell.
#[spec("inbox/claude/013")]
#[test]
fn claude_013_live_clear_waiters_claim_once() {
    skip_unless!(common::check_claude_available());
    let claude = Claude::new();
    for count in [2, 3] {
        claude.deck.send_bytes(b"/clear\r");
        assert!(
            common::wait_until(BUDGET, || claude.listing()["armed"].as_u64().unwrap_or(0)
                >= count),
            "clear must arm another waiter; {}",
            claude.evidence()
        );
    }
    let name = "native-clear-44f81c.txt";
    let id = claude.post(&claude.directive(name));
    claude.done(&id, name, true);
}

/// Scenario: Capture the pid of the scratch pane's armed waiter and kill that process, then post a task after the waiter disconnects. A fixed doorbell must wake interactive Haiku, whose submit hook injects and acknowledges the task and shows an envelope label before the sentinel is written.
#[spec("inbox/claude/014")]
#[test]
#[cfg(target_os = "linux")]
fn claude_014_live_killed_waiter_falls_back_to_injection() {
    skip_unless!(common::check_claude_available());
    let claude = Claude::new();
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let cmd = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let env = std::fs::read(entry.path().join("environ")).unwrap_or_default();
        if cmd.split(|c| *c == 0).any(|a| a == b"wait")
            && cmd.split(|c| *c == 0).any(|a| a == b"inbox")
            && env
                .split(|c| *c == 0)
                .any(|v| v == format!("HOME={}", claude.deck.home_dir().display()).as_bytes())
        {
            pids.push(pid);
        }
    }
    assert_eq!(pids.len(), 1, "one captured scratch waiter: {pids:?}");
    assert_eq!(unsafe { libc::kill(pids[0], libc::SIGKILL) }, 0);
    assert!(common::wait_until(Duration::from_secs(10), || claude
        .listing()["armed"]
        == 0));
    let name = "doorbell-fallback-908c2a.txt";
    let id = claude.post(&claude.directive(name));
    claude.done(&id, name, false);
    assert!(
        claude.log().contains("doorbell rang"),
        "fallback must ring; {}",
        claude.evidence()
    );
    assert!(
        !claude.record()["live"]["last_user_prompt"]
            .to_string()
            .contains("waiting, run:")
    );
}
