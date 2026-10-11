use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::common;
use crate::common::{EventSub, TuiDeck};
use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_AGENT_ID, DOT_AGENT_DECK_PANE_ID};
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::event::{AgentType, EventType};
use dot_agent_deck::hook_provenance::DOT_AGENT_DECK_PANE_CAPABILITY;
use serde_json::{Value, json};

pub const PANE: &str = "background-synthetic-pane";
const SESSION: &str = "background-synthetic-session";

pub struct Fixture {
    pub deck: TuiDeck,
    pub runtime: tokio::runtime::Runtime,
    pub client: DaemonClient,
    pub agent_id: String,
    pub capability: String,
    pub events: EventSub,
}

impl Fixture {
    pub fn new(agent_type: AgentType) -> Self {
        let deck = TuiDeck::launch_with_fixture("minimal");
        deck.wait_for_string("No active agents");
        // Each file is an independently controllable qualifying POSIX
        // session. The Bash argv carries the measured Claude fingerprint;
        // Python is only the parent stand-in, never a real agent.
        std::fs::write(deck.workdir().join("background-parent.py"), r#"
import pathlib, subprocess, time
children = {}
try:
    while True:
        for name in ('one', 'two'):
            enabled = pathlib.Path(name + '.run').exists()
            child = children.get(name)
            if enabled and child is None:
                child = subprocess.Popen(['bash', '-c', 'while :; do sleep 1; done', 'shell-snapshots/snapshot-test && eval '], start_new_session=True)
                children[name] = child
                pathlib.Path(name + '.pid').write_text(str(child.pid))
            elif not enabled and child is not None:
                import os, signal
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
                del children[name]
        time.sleep(.05)
finally:
    for child in children.values():
        child.kill()
        child.wait()
"#).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let client = DaemonClient::new(deck.attach_socket_path().to_path_buf());
        let events = deck.subscribe_events();
        let stand_in = common::capability_export_command("exec python3 background-parent.py");
        let command = if agent_type == AgentType::Codex {
            // The Codex wrapper needs one program with arguments. Keep the
            // capability export and exec together inside that program rather
            // than putting shell syntax directly after `wrap --`.
            std::fs::write(deck.workdir().join("background-parent.sh"), &stand_in).unwrap();
            "sh background-parent.sh".to_string()
        } else {
            stand_in
        };
        let agent_id = runtime
            .block_on(client.start_agent(StartAgentOptions {
                command: Some(command),
                cwd: Some(deck.workdir().to_string_lossy().into_owned()),
                display_name: Some("background-synthetic".into()),
                env: vec![(DOT_AGENT_DECK_PANE_ID.into(), PANE.into())],
                agent_type: Some(agent_type),
                ..Default::default()
            }))
            .unwrap();
        let capability = runtime
            .block_on(common::recorded_hook_capability(deck.workdir(), &agent_id))
            .unwrap();
        Self {
            deck,
            runtime,
            client,
            agent_id,
            capability,
            events,
        }
    }

    pub fn hook(&self, agent: &str, name: &str, extra: Value) -> Value {
        let mut payload =
            json!({"session_id": SESSION, "hook_event_name": name, "cwd": self.deck.workdir()});
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let before = self.events.snapshot().len();
        let mut child = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .args(["hook", "--agent", agent])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.deck.home_dir())
            .env("DOT_AGENT_DECK_SOCKET", self.deck.hook_socket_path())
            .env(DOT_AGENT_DECK_PANE_ID, PANE)
            .env(DOT_AGENT_DECK_AGENT_ID, &self.agent_id)
            .env(DOT_AGENT_DECK_PANE_CAPABILITY, &self.capability)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "hook failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Receipt barrier scoped to the actual event kind and subagent id;
        // monitor frames may interleave with hooks on the same stream.
        let wanted = match name {
            "Stop" => EventType::Idle,
            "UserPromptSubmit" => EventType::Thinking,
            "PreToolUse" => EventType::ToolStart,
            "SessionStart" => EventType::SessionStart,
            "SubagentStart" => EventType::SubagentStart,
            "SubagentStop" => EventType::SubagentStop,
            _ => panic!("unsupported fixture hook {name}"),
        };
        let subagent = payload.get("agent_id").and_then(Value::as_str);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(event) = self
                .events
                .snapshot()
                .into_iter()
                .skip(before)
                .find(|event| {
                    event.agent_id.as_deref() == Some(&self.agent_id)
                        && event.event_type == wanted
                        && subagent.is_none_or(|id| {
                            event
                                .metadata
                                .get(dot_agent_deck::event::SUBAGENT_ID_METADATA_KEY)
                                .is_some_and(|value| value == id)
                        })
                })
            {
                return serde_json::to_value(event).unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "hook {name} was not broadcast: {:?}",
                self.events.snapshot()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn live(&self) -> Value {
        let records = self.runtime.block_on(self.client.list_agents()).unwrap();
        serde_json::to_value(
            records
                .into_iter()
                .find(|r| r.id == self.agent_id)
                .expect("live agent")
                .live,
        )
        .unwrap()
    }

    pub fn command(&self, name: &str, enabled: bool) {
        let path = self.deck.workdir().join(format!("{name}.run"));
        if enabled {
            std::fs::write(path, "start").unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
    }

    pub fn wait_commands(&self, count: u64) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let live = self.live();
            let got = live["background"]["commands"].as_u64().unwrap_or(0);
            if got == count {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "expected {count} background commands, got {live}; grid:\n{}",
                self.deck.snapshot_grid()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn assert_subagents(&self, count: u64) -> Value {
        let live = self.live();
        assert_eq!(
            live["background"]["subagents"].as_u64().unwrap_or(0),
            count,
            "daemon's post-transition aggregate: {live}"
        );
        if count == 0 {
            assert!(
                live["background"].is_null(),
                "no work must yield None: {live}"
            );
        }
        live
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for name in ["one", "two"] {
            if let Ok(pid) =
                std::fs::read_to_string(self.deck.workdir().join(format!("{name}.pid")))
                && let Ok(pid) = pid.parse::<i32>()
            {
                // SAFETY: this is a session leader created by this fixture,
                // reported through its private pid file. No global matching.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
        }
    }
}

impl Fixture {
    pub fn wait_count_event(&self, before: usize, count: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let seen = self.events.snapshot();
            let matched = seen
                .iter()
                .skip(before)
                .filter(|event| {
                    event.agent_id.as_deref() == Some(&self.agent_id)
                        && event.event_type == EventType::ShellBusy
                })
                .any(|event| {
                    serde_json::to_value(event).unwrap()["background"]["commands"] == json!(count)
                });
            if matched {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "count-only change {count} was not broadcast: {seen:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    pub fn assert_no_repeated_shell_events(&self) {
        let baseline = self.events.snapshot().len();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            assert_eq!(self.live()["status"], json!("Idle"));
            let extra: Vec<_> = self
                .events
                .snapshot()
                .into_iter()
                .skip(baseline)
                .filter(|event| {
                    event.agent_id.as_deref() == Some(&self.agent_id)
                        && matches!(
                            event.event_type,
                            EventType::ShellBusy | EventType::ShellIdle
                        )
                })
                .collect();
            assert!(
                extra.is_empty(),
                "identical reporting Idle+busy sample was re-emitted: {extra:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    pub fn assert_successor_clean(&self, before: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let live = self.live();
            assert!(
                live["background"].is_null(),
                "old spawn's sample leaked to successor: {live}"
            );
            assert_eq!(live["status"], json!("Idle"));
            for event in self
                .events
                .snapshot()
                .into_iter()
                .skip(before)
                .filter(|event| event.agent_id.as_deref() == Some(&self.agent_id))
            {
                let wire = serde_json::to_value(event).unwrap();
                assert_eq!(
                    wire["background"]["commands"].as_u64().unwrap_or(0),
                    0,
                    "stale observation was restamped with successor identity"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
