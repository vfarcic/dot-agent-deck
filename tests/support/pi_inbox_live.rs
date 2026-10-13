//! Real interactive Pi, isolated configuration, and genuine CLI delegates.
#![allow(dead_code)]

use crate::{common, inbox};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

pub const BUDGET: Duration = Duration::from_secs(120);
pub const CONTENT: &str = "PI_INBOX_OK_42c7";

pub fn available() -> Result<(), String> {
    common::install_credential_redaction();
    let output = Command::new("pi")
        .arg("--version")
        .output()
        .map_err(|e| format!("Pi unavailable: {e}"))?;
    if !output.status.success() {
        return Err("pi --version failed".into());
    }
    if std::env::var("ANTHROPIC_API_KEY").is_ok_and(|key| !key.trim().is_empty()) {
        Ok(())
    } else {
        Err("ANTHROPIC_API_KEY required for real Pi inbox tests".into())
    }
}

pub struct Pi {
    pub pane: inbox::Pane,
    pub deck: common::TuiDeck,
    pub work: PathBuf,
    next: std::cell::Cell<usize>,
}

impl Pi {
    pub fn new() -> Self {
        let deck = common::TuiDeck::builder()
            .with_pty_size(200, 50)
            .with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
            .with_env("DOT_AGENT_DECK_LOG", "pi-inbox-live.log")
            .with_env("DOT_AGENT_DECK_INBOX_ARM_GRACE_MS", "10000")
            .with_env("DOT_AGENT_DECK_DOORBELL_TICK_MS", "25")
            .with_env("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000")
            .with_env("DOT_AGENT_DECK_INBOX_ACK_WAIT_MS", "180000")
            .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "600")
            .launch_with_fixture("minimal");
        assert!(deck.wait_for_grid_string_within("No active agents", BUDGET));
        let work = deck.workdir().join("project");
        std::fs::create_dir_all(&work).unwrap();
        let agent_dir = deck.home_dir().join(".pi/agent");
        let observer = agent_dir.join("extensions/inbox-test-observer.ts");
        std::fs::create_dir_all(observer.parent().unwrap()).unwrap();
        // Passive observation only: delivery, acknowledgement and watcher
        // lifetime remain entirely the bundled extension's responsibility.
        std::fs::write(
            &observer,
            r#"import {appendFileSync} from 'node:fs';
export default function(pi) {
 const log=(type,event,ctx)=>appendFileSync(process.env.DECK_TEST_PI_EVENTS,
  JSON.stringify({type,pid:process.pid,session:ctx.sessionManager.getSessionId(),event})+'\n');
 for(const type of ['session_start','agent_start','message_start','agent_settled'])
  pi.on(type,(event,ctx)=>log(type,event,ctx));
}
"#,
        )
        .unwrap();
        let installed = deck.home_dir().join(".local/bin/dot-agent-deck");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        if !installed.exists()
            && std::fs::hard_link(env!("CARGO_BIN_EXE_dot-agent-deck"), &installed).is_err()
        {
            std::fs::copy(env!("CARGO_BIN_EXE_dot-agent-deck"), &installed).unwrap();
        }
        let path = format!(
            "{}:{}",
            installed.parent().unwrap().display(),
            std::env::var("PATH").unwrap()
        );
        let command = "exec pi --provider anthropic --model claude-haiku-4-5 --approve";
        let role_command = common::capability_export_command(command);
        std::fs::write(work.join(".dot-agent-deck.toml"), format!(
            "[[orchestrations]]\nname = \"pi-inbox\"\n[[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"./delegate-loop.sh\"\nstart = true\n[[orchestrations.roles]]\nname = \"coder\"\ncommand = {role_command:?}\nagent = \"pi\"\nclear = false\n"
        )).unwrap();
        let script = work.join("delegate-loop.sh");
        std::fs::write(&script, "#!/bin/sh\ni=1\nwhile :; do\n while [ ! -f \"delegate-$i.go\" ]; do sleep 0.1; done\n dot-agent-deck delegate --to coder --task-file \"delegate-$i.md\" > \"delegate-$i.log\" 2>&1\n printf '%s' \"$?\" > \"delegate-$i.exit\"\n i=$((i+1))\ndone\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let reply = inbox::attach(
            &deck,
            json!({"op":"start-agent",
            "command":role_command,"cwd":work,"agent_type":"pi","rows":40,"cols":140,
            "env":[["DOT_AGENT_DECK_PANE_ID","pi-inbox-coder"],["PATH",path],
                ["PI_CODING_AGENT_DIR",agent_dir],["DECK_TEST_PI_EVENTS",work.join("pi-events.jsonl")]],
            "display_name":"coder","tab_membership":{"kind":"orchestration","name":"pi-inbox",
                "role_name":"coder","role_index":1,"is_start_role":false,"orchestration_cwd":work,
                "orchestration_id":"pi-inbox-test"}}),
        );
        assert_eq!(reply["ok"], true, "Pi spawn: {reply}");
        let agent = reply["id"].as_str().unwrap().to_owned();
        let token = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(common::recorded_hook_capability(&work, &agent))
            .unwrap();
        let pane = inbox::Pane {
            id: "pi-inbox-coder".into(),
            agent,
            token,
            pid: 0,
            capture: PathBuf::new(),
            attach_socket: deck.attach_socket_path().into(),
        };
        let reply = inbox::attach(
            &deck,
            json!({"op":"start-agent",
            "command":common::capability_export_command("exec ./delegate-loop.sh"),
            "cwd":work,"rows":40,"cols":140,"env":[["DOT_AGENT_DECK_PANE_ID","pi-inbox-orchestrator"],["PATH",path]],
            "display_name":"orchestrator","tab_membership":{"kind":"orchestration","name":"pi-inbox",
                "role_name":"orchestrator","role_index":0,"is_start_role":true,"orchestration_cwd":work,
                "orchestration_id":"pi-inbox-test"}}),
        );
        assert_eq!(reply["ok"], true, "orchestrator fixture spawn: {reply}");
        let result = Self {
            pane,
            deck,
            work,
            next: std::cell::Cell::new(1),
        };
        assert!(
            common::wait_until(BUDGET, || result
                .events()
                .iter()
                .any(|event| event["type"] == "session_start")),
            "Pi observer must see session_start; {}",
            result.evidence()
        );
        inbox::armed(&result.deck, &result.pane, 1);
        result.deck.send_bytes(b"\t");
        result.deck.wait_for_string("coder");
        result.deck.send_bytes(b"2");
        assert!(
            result
                .deck
                .wait_for_grid_string_within("[Command Mode Ctrl+D]", BUDGET),
            "attach real Pi pane; {}",
            result.evidence()
        );
        result
    }

    pub fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(self.work.join("pi-events.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
    pub fn users(&self) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|e| e["type"] == "message_start" && e["event"]["message"]["role"] == "user")
            .collect()
    }
    pub fn pid(&self) -> i32 {
        self.events().last().unwrap()["pid"].as_i64().unwrap() as i32
    }
    pub fn session(&self) -> String {
        self.events().last().unwrap()["session"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    pub fn log(&self) -> String {
        std::fs::read_to_string(self.deck.workdir().join("pi-inbox-live.log")).unwrap_or_default()
    }
    pub fn evidence(&self) -> String {
        format!(
            "inbox={}\ngrid={}\nlog={}",
            inbox::listing(&self.deck, &self.pane),
            self.deck.snapshot_grid(),
            self.log()
        )
    }
    pub fn input(&self, text: &str) -> common::AttachInput {
        let mut input =
            common::AttachInput::attach(self.deck.attach_socket_path(), &self.pane.agent);
        input.write(text);
        input
    }
    pub fn prompt(&self, text: &str) {
        let mut input = self.input(text);
        // The editor wraps a long command. Its final word confirms that the
        // whole input arrived without requiring one contiguous screen line.
        let tail = text.split_whitespace().next_back().unwrap();
        assert!(
            common::wait_until(BUDGET, || {
                String::from_utf8_lossy(&common::pane_snapshot_on(
                    self.deck.attach_socket_path(),
                    &self.pane.agent,
                ))
                .contains(tail)
            }),
            "typed input must render before submit; {}",
            self.evidence()
        );
        input.write("\r");
    }
    pub fn idle(&self) {
        assert!(
            common::wait_until(BUDGET, || {
                let records = inbox::attach(&self.deck, json!({"op":"list-agents"}));
                records["agent_records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["id"] == self.pane.agent && r["live"]["status"] == "Idle")
            }),
            "Pi must finish; {}",
            self.evidence()
        );
    }
    pub fn delegate(&self, name: &str) -> String {
        self.delegate_body(&format!("Use the bash tool to run exactly: printf '{CONTENT}' > '{}'. Reply DONE-{name} and end your turn.",self.work.join(name).display()))
    }
    pub fn post_body(&self, subject: &str, body: &str) -> String {
        let reply = inbox::attach(
            &self.deck,
            json!({"op":"post-message", "pane_id":self.pane.id,
                "expected_agent_id":self.pane.agent, "subject":subject,
                "body":body, "origin":"seed"}),
        );
        assert_eq!(reply["ok"], true, "client inbox post failed: {reply}");
        reply["id"].as_str().unwrap().to_owned()
    }
    pub fn delegate_body(&self, body: &str) -> String {
        let n = self.next.get();
        self.next.set(n + 1);
        std::fs::write(self.work.join(format!("delegate-{n}.md")), body).unwrap();
        let before = inbox::listing(&self.deck, &self.pane)["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].clone())
            .collect::<Vec<_>>();
        std::fs::write(self.work.join(format!("delegate-{n}.go")), "").unwrap();
        let exit = self.work.join(format!("delegate-{n}.exit"));
        assert!(
            common::wait_until(BUDGET, || std::fs::read_to_string(&exit)
                .is_ok_and(|s| !s.is_empty())),
            "delegate CLI must finish; {}",
            self.evidence()
        );
        assert_eq!(
            std::fs::read_to_string(exit).unwrap(),
            "0",
            "delegate CLI failed: {:?}",
            std::fs::read_to_string(self.work.join(format!("delegate-{n}.log")))
        );
        let id = std::cell::RefCell::new(None);
        assert!(
            common::wait_until(BUDGET, || {
                *id.borrow_mut() = inbox::listing(&self.deck, &self.pane)["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|m| !before.contains(&m["id"]))
                    .and_then(|m| m["id"].as_str().map(str::to_owned));
                id.borrow().is_some()
            }),
            "delegate must enqueue; {}",
            self.evidence()
        );
        id.into_inner().unwrap()
    }
    pub fn done(&self, id: &str, name: &str) {
        common::wait_for_file_trimmed_eq(&self.work.join(name), CONTENT, BUDGET)
            .unwrap_or_else(|why| panic!("sentinel: {why}; {}", self.evidence()));
        inbox::state(&self.deck, &self.pane, id, "acknowledged");
        self.committed_once(id);
    }
    pub fn committed_once(&self, id: &str) {
        let matching = self
            .users()
            .into_iter()
            .filter(|e| {
                e["event"]
                    .to_string()
                    .contains(&format!("[dot-agent-deck message {id} "))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            1,
            "message must enter Pi once; {matching:?}; {}",
            self.evidence()
        );
        let listing = inbox::listing(&self.deck, &self.pane);
        let message = listing["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap();
        assert_eq!(
            message["deliveries"], 1,
            "native delivery must be taken once: {message}"
        );
        assert_eq!(
            message["ack_how"], "extension_confirmed",
            "native message must be acknowledged by Pi's message_start handler: {message}"
        );
    }
    pub fn watcher_pids(&self) -> Vec<i32> {
        let mut found = Vec::new();
        for task in std::fs::read_dir(format!("/proc/{}/task", self.pid()))
            .unwrap()
            .flatten()
        {
            for child in std::fs::read_to_string(task.path().join("children"))
                .unwrap_or_default()
                .split_whitespace()
            {
                let cmd = std::fs::read(format!("/proc/{child}/cmdline")).unwrap_or_default();
                if cmd.split(|b| *b == 0).any(|arg| arg == b"inbox")
                    && cmd.split(|b| *b == 0).any(|arg| arg == b"wait")
                {
                    found.push(child.parse().unwrap());
                }
            }
        }
        found.sort();
        found.dedup();
        found
    }
}
