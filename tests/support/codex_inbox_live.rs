//! Shared setup for real interactive Codex inbox tests, never the host config.
#![allow(dead_code)]

use crate::{common, inbox};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

pub const BUDGET: Duration = Duration::from_secs(120);

pub fn prepare(deck: &common::TuiDeck) {
    let home = deck.home_dir().join(".codex");
    let binary = common::contained_deck_binary();
    let installed = deck.home_dir().join(".local/bin/dot-agent-deck");
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    if !installed.exists() && std::fs::hard_link(binary, &installed).is_err() {
        std::fs::copy(binary, &installed).unwrap();
    }
    // Complete the app-server migration and hook trust probe BEFORE the TUI
    // starts. A fresh CODEX_HOME otherwise races two SQLite initializers.
    let last = std::cell::RefCell::new(String::new());
    let ready = common::wait_until(BUDGET, || {
        let output = std::process::Command::new(binary)
            .args(["hooks", "install", "--agent", "codex"])
            .env_clear()
            .env("HOME", deck.home_dir())
            .env("CODEX_HOME", &home)
            .env("XDG_CONFIG_HOME", deck.home_dir().join(".config"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    installed.parent().unwrap().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("DOT_AGENT_DECK_TEST_CONFIG_WRITE", "1")
            .env("DOT_AGENT_DECK_TEST_CONFIG_ROOT", deck.workdir())
            .current_dir(deck.workdir())
            .output()
            .unwrap();
        *last.borrow_mut() = format!("{output:?}");
        if !output.status.success() {
            return false;
        }
        let config: toml::Value =
            toml::from_str(&std::fs::read_to_string(home.join("config.toml")).unwrap()).unwrap();
        config
            .get("hooks")
            .and_then(|hooks| hooks.get("state"))
            .and_then(toml::Value::as_table)
            .is_some_and(|table| {
                ["session_start", "user_prompt_submit", "stop"]
                    .iter()
                    .all(|event| {
                        table.iter().any(|(key, value)| {
                            key.contains(&format!(":{event}:"))
                                && value.get("trusted_hash").is_some()
                        })
                    })
            })
    });
    assert!(
        ready,
        "Codex fixture must finish trusting all deck hooks before starting the TUI; {}",
        last.borrow()
    );
}

pub struct Codex {
    pub pane: inbox::Pane,
    pub deck: common::TuiDeck,
}

impl Codex {
    pub fn new(folder_dialog: bool) -> Self {
        let deck = common::TuiDeck::builder()
            .with_pty_size(180, 50)
            .with_imported_codex_credentials()
            .with_env("DOT_AGENT_DECK_LOG", "codex-inbox-live.log")
            .with_env("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000")
            .launch_with_fixture("minimal");
        assert!(
            deck.wait_for_grid_string_within("No active agents", BUDGET),
            "scratch dashboard must start before launching Codex: {}",
            deck.snapshot_grid()
        );
        prepare(&deck);
        if folder_dialog {
            // The harness normally trusts this fixture. Remove only that
            // scratch project entry, preserving the hooks' trusted hashes.
            let config_path = deck.home_dir().join(".codex/config.toml");
            let mut config: toml::Value =
                toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
            config.as_table_mut().unwrap().remove("projects");
            std::fs::write(config_path, toml::to_string(&config).unwrap()).unwrap();
        }
        let command = common::capability_export_command(&format!(
            "exec codex --model {} --sandbox workspace-write --ask-for-approval never -c 'sandbox_workspace_write.network_access=true' -c 'model_reasoning_effort=\"low\"'",
            common::codex_test_model()
        ));
        let reply = inbox::attach(
            &deck,
            json!({"op":"start-agent", "command":command,
            "agent_type":"codex", "cwd":deck.workdir(), "rows":40,"cols":140,
            "display_name":"codex-inbox-live", "env":[
                ["CODEX_HOME",deck.home_dir().join(".codex")],
                ["DOT_AGENT_DECK_PANE_ID","codex-inbox-live"]]}),
        );
        assert_eq!(reply["ok"], true, "wrapped Codex spawn: {reply}");
        let agent = reply["id"].as_str().unwrap().to_string();
        let token = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(common::recorded_hook_capability(deck.workdir(), &agent))
            .unwrap();
        let pane = inbox::Pane {
            id: "codex-inbox-live".into(),
            agent,
            token,
            pid: 0,
            capture: PathBuf::new(),
            attach_socket: deck.attach_socket_path().into(),
        };
        let result = Self { pane, deck };
        if folder_dialog {
            result.wait_screen("Trust this folder?");
        } else {
            result.wait_screen("? for shortcuts");
        }
        result.deck.wait_for_string("codex-inbox-live");
        result.deck.send_bytes(b"1");
        assert!(
            result.deck.wait_for_grid_string_within(
                if folder_dialog {
                    "Trust this folder?"
                } else {
                    "? for shortcuts"
                },
                BUDGET
            ),
            "real Codex surface must be visible in attached deck; {}",
            result.evidence()
        );
        result
    }

    pub fn screen(&self) -> String {
        let record = self.record();
        let rows = record["rows"].as_u64().unwrap() as u16;
        let cols = record["cols"].as_u64().unwrap() as u16;
        let mut parser = dot_agent_deck::vt_grid::parser(rows, cols, 0);
        parser.process(&common::pane_snapshot_on(
            self.deck.attach_socket_path(),
            &self.pane.agent,
        ));
        parser.screen().contents()
    }
    pub fn wait_screen(&self, needle: &str) {
        assert!(
            common::wait_until(BUDGET, || self.screen().contains(needle)),
            "Codex must show {needle:?}; {}",
            self.evidence()
        );
    }
    pub fn log(&self) -> String {
        std::fs::read_to_string(self.deck.workdir().join("codex-inbox-live.log"))
            .unwrap_or_default()
    }
    pub fn evidence(&self) -> String {
        format!(
            "Pane:\n{}\nGrid:\n{}\nInbox:{}\nLog:\n{}",
            self.screen(),
            self.deck.snapshot_grid(),
            inbox::listing(&self.deck, &self.pane),
            self.log()
        )
    }
    pub fn post(&self, name: &str) -> String {
        inbox::enqueue(
            &self.deck,
            &self.pane,
            &format!(
                "Run exactly this shell command: printf '%s' 'codex-inbox-action' > '{}'. Then reply DONE and end your turn.",
                // PRD #1666: the absolute path, not "the current directory".
                self.deck.workdir().join(name).display()
            ),
        )
    }
    pub fn done(&self, id: &str, name: &str) {
        common::wait_for_file_trimmed_eq(
            &self.deck.workdir().join(name),
            "codex-inbox-action",
            BUDGET,
        )
        .unwrap_or_else(|why| panic!("Codex must act on {id}: {why}; {}", self.evidence()));
        inbox::state(&self.deck, &self.pane, id, "acknowledged");
        assert!(
            common::wait_until(BUDGET, || self.record()["live"]["status"] == "Idle"),
            "Codex must finish its visible reply after writing the sentinel; {}",
            self.evidence()
        );
        self.deck.send_bytes(b"\x04");
        self.deck.wait_for_string("[New Agent n/Ctrl+N]");
    }
    pub fn input(&self, bytes: &str) -> common::AttachInput {
        let mut input =
            common::AttachInput::attach(self.deck.attach_socket_path(), &self.pane.agent);
        input.write(bytes);
        input
    }
    pub fn record(&self) -> Value {
        inbox::attach(&self.deck, json!({"op":"list-agents"}))["agent_records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == self.pane.agent)
            .unwrap()
            .clone()
    }
}
