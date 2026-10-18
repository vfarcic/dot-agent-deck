#![cfg(all(feature = "e2e", unix))]
//! Generated OpenCode plugins against a fake API, real inbox CLI and daemon.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;
use inbox::*;
use serde_json::{Value, json};
use spec::spec;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn install_cli(daemon: &common::DaemonProc) -> std::process::Output {
    let installed = daemon.home.join(".local/bin/dot-agent-deck");
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    if !installed.exists()
        && std::fs::hard_link(env!("CARGO_BIN_EXE_dot-agent-deck"), &installed).is_err()
    {
        std::fs::copy(env!("CARGO_BIN_EXE_dot-agent-deck"), &installed).unwrap();
    }
    Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .env_clear()
        .env("HOME", &daemon.home)
        .env("PATH", std::env::var("PATH").unwrap())
        .env("XDG_CONFIG_HOME", daemon.home.join(".config"))
        .env("DOT_AGENT_DECK_TEST_CONFIG_WRITE", "1")
        .env("DOT_AGENT_DECK_TEST_CONFIG_ROOT", &daemon.home)
        .current_dir(&daemon.home)
        .args(["hooks", "install", "--agent", "opencode"])
        .output()
        .unwrap()
}

struct OpenCode {
    pane: Pane,
    daemon: common::DaemonProc,
    base: PathBuf,
    /// With `{"offerPool": …}`: a stand-in orchestrator whose orchestration
    /// offers jobs in that pool to the OpenCode pane.
    orchestrator: Option<Pane>,
}
impl OpenCode {
    fn new(route: &str, config: Value) -> Self {
        let path = std::env::var("PATH").unwrap();
        let mut env = vec![
            ("PATH", path.as_str()),
            (
                "DOT_AGENT_DECK_INBOX_ACK_WAIT_MS",
                config["ackWaitMs"].as_str().unwrap_or("1500"),
            ),
            ("DOT_AGENT_DECK_INBOX_ARM_GRACE_MS", "3500"),
            (
                "DOT_AGENT_DECK_INBOX_MAX_DELIVER",
                config["maxDeliver"].as_str().unwrap_or("5"),
            ),
        ];
        // PRD #1666 §4: a short keepalive makes POSSESS_TTL three of it, so a
        // test can outlive several TTLs in seconds.
        if let Some(ms) = config["keepaliveMs"].as_str() {
            env.push(("DOT_AGENT_DECK_INBOX_KEEPALIVE_MS", ms));
        }
        if let Some(ms) = config["publishDelayMs"].as_str() {
            env.push(("DOT_AGENT_DECK_INBOX_TEST_PUBLISH_DELAY_MS", ms));
        }
        // The stand-in orchestrator's offer is posted from the test process,
        // outside its pane's agent.
        if config["offerPool"].is_string() {
            env.push(("DOT_AGENT_DECK_INBOX_TEST_UNBOUND_COMPONENTS", "1"));
        }
        let daemon = daemon(&env);
        let installed = daemon.home.join(".local/bin/dot-agent-deck");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        if !installed.exists()
            && std::fs::hard_link(env!("CARGO_BIN_EXE_dot-agent-deck"), &installed).is_err()
        {
            std::fs::copy(env!("CARGO_BIN_EXE_dot-agent-deck"), &installed).unwrap();
        }
        let output = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .env_clear()
            .env("HOME", &daemon.home)
            .env("PATH", &path)
            .env("XDG_CONFIG_HOME", daemon.home.join(".config"))
            .env("DOT_AGENT_DECK_TEST_CONFIG_WRITE", "1")
            .env("DOT_AGENT_DECK_TEST_CONFIG_ROOT", &daemon.home)
            .args(["hooks", "install", "--agent", "opencode"])
            .output()
            .unwrap();
        assert!(output.status.success(), "installer: {output:?}");
        let root = daemon.home.join(".config/opencode");
        let plugin = if config["server"] == true {
            root.join("plugin/dot-agent-deck.js")
        } else {
            root.join("dot-agent-deck-tui.js")
        };
        assert!(
            plugin.exists(),
            "S8 must install the real generated TUI plugin at {}",
            plugin.display()
        );
        let module = root.join("fixture-plugin.mjs");
        std::fs::copy(plugin, &module).unwrap();
        if config["preServer"] == true {
            std::fs::copy(
                root.join("plugin/dot-agent-deck.js"),
                module.with_extension("mjs.server.mjs"),
            )
            .unwrap();
        }
        let driver = daemon.home.join("api.mjs");
        std::fs::write(&driver, include_str!("fixtures/opencode-inbox/api.mjs")).unwrap();
        let base = daemon.home.join("opencode-fixture");
        std::fs::write(base.with_extension("config"), config.to_string()).unwrap();
        let command = common::capability_export_command(&format!(
            "exec node '{}' '{}' '{}' '{route}'",
            driver.display(),
            module.display(),
            base.display()
        ));
        let membership = |role: &str, index, start| {
            json!({"kind":"orchestration", "name":"offers", "role_name":role,
            "role_index":index, "is_start_role":start, "orchestration_id":"offers-fixture",
            "orchestration_cwd":daemon.cwd()})
        };
        let pool = config["offerPool"].as_str();
        let delay = config["publishDelayMs"].as_str();
        let mut start = json!({"op":"start-agent","command":command,"cwd":daemon.home,
            "agent_type":"opencode","rows":40,"cols":140,
            "env":[["DOT_AGENT_DECK_PANE_ID","opencode-fixture"],["PATH",path]],"display_name":"opencode fixture"});
        if let Some(ms) = delay {
            start["env"]
                .as_array_mut()
                .unwrap()
                .push(json!(["DOT_AGENT_DECK_INBOX_TEST_PUBLISH_DELAY_MS", ms]));
        }
        // PRD #1666 §3: the watcher publishes each commit receipt and drops its
        // control, as if its stream died between the two.
        if config["dropCommitted"] == true {
            start["env"]
                .as_array_mut()
                .unwrap()
                .push(json!(["DOT_AGENT_DECK_INBOX_TEST_DROP_COMMITTED", "1"]));
        }
        // PRD #1666 §4: a replacement watcher's rebound caps reach its
        // publisher this long after the `rebound` line.
        if let Some(ms) = config["rebindDelayMs"].as_str() {
            start["env"]
                .as_array_mut()
                .unwrap()
                .push(json!(["DOT_AGENT_DECK_INBOX_TEST_REBIND_DELAY_MS", ms]));
        }
        // PRD #1666 §4: every watcher respawn waits this long, and every
        // watcher reads its registration this long after it arrives.
        if let Some(ms) = config["backoffMs"].as_str() {
            start["env"]
                .as_array_mut()
                .unwrap()
                .push(json!(["DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS", ms]));
        }
        if let Some(ms) = config["registerDelayMs"].as_str() {
            start["env"]
                .as_array_mut()
                .unwrap()
                .push(json!(["DOT_AGENT_DECK_INBOX_TEST_REGISTER_DELAY_MS", ms]));
        }
        if let Some(pool) = pool {
            // With `{"keepWorker": true}` a delegation to the worker role
            // reaches the OpenCode pane instead of restarting it.
            let clear = if config["keepWorker"] == true {
                "clear = false\n"
            } else {
                ""
            };
            std::fs::write(
                daemon.cwd().join(".dot-agent-deck.toml"),
                format!("[[orchestrations]]\nname = 'offers'\n[[orchestrations.roles]]\nname = 'orchestrator'\ncommand = 'cat'\nstart = true\n[[orchestrations.roles]]\nname = 'worker'\ncommand = 'cat'\ntake_offers = ['{pool}']\n{clear}"),
            )
            .unwrap();
            start["tab_membership"] = membership("worker", 1, false);
        }
        let reply = attach(&daemon, start);
        assert_eq!(reply["ok"], true, "plugin host: {reply}");
        let agent = reply["id"].as_str().unwrap().to_string();
        let token = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(common::recorded_hook_capability(&daemon.home, &agent))
            .unwrap();
        let pane = Pane {
            id: "opencode-fixture".into(),
            agent,
            token,
            pid: 0,
            capture: base.with_extension("bytes"),
            attach_socket: daemon.attach_socket.clone(),
        };
        let orchestrator = pool.map(|_| {
            route::start(
                &daemon,
                "offers-orchestrator",
                json!({"tab_membership": membership("orchestrator", 0, true)}),
            )
        });
        let result = Self {
            pane,
            daemon,
            base,
            orchestrator,
        };
        result.wait_kind("ready");
        result
    }
    fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(self.base.with_extension("events"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
    fn wait_kind(&self, kind: &str) {
        assert!(
            common::wait_until(WAIT, || self.events().iter().any(|e| e["kind"] == kind)),
            "plugin must emit {kind}; events={:?}\nPane:{}",
            self.events(),
            screen(&self.daemon, &self.pane)
        );
    }
    fn control(&self, value: Value) {
        writeln!(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.base.with_extension("control"))
                .unwrap(),
            "{value}"
        )
        .unwrap();
    }
    fn post(&self) -> String {
        enqueue(&self.daemon, &self.pane, BODY)
    }
    fn prompts(&self) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|e| e["kind"] == "prompt")
            .map(|e| e["request"].clone())
            .collect()
    }
    fn assistant(&self, parent: &str) {
        let session = self
            .events()
            .iter()
            .find(|event| event["kind"] == "user-message" && event["id"] == parent)
            .map(|event| event["sessionID"].clone())
            .unwrap_or(json!("shown"));
        self.control(json!({"event":"message.updated","properties":{"info":{"id":format!("assistant-{parent}"),"role":"assistant","parentID":parent,"sessionID":session}}}));
    }
    /// The control lines the plugin wrote to its watchers, parsed.
    fn controls(&self) -> Vec<Value> {
        self.events()
            .iter()
            .filter(|e| e["kind"] == "stdin")
            .filter_map(|e| e["text"].as_str().map(str::to_owned))
            .flat_map(|text| text.lines().map(str::to_owned).collect::<Vec<_>>())
            .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
            .collect()
    }
    /// PRD #1666 §3: the attempts of `id` the plugin reported committed.
    fn commits(&self, id: &str) -> usize {
        self.controls()
            .iter()
            .filter_map(|c| c["committed"].as_array().cloned())
            .flatten()
            .filter(|at| at["id"] == id)
            .count()
    }
    /// PRD #1666 §6.1: the prompts of `id` marked as a possible repeat.
    fn marked(&self, id: &str) -> usize {
        let line = dot_agent_deck::inbox_display::repeat_line(id);
        self.prompts()
            .iter()
            .filter(|p| {
                p["parts"][0]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains(&line))
            })
            .count()
    }
    /// The watchers the plugin started, their argv.
    fn watchers(&self) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|e| e["kind"] == "spawn" && e["args"][1][1] == "wait")
            .map(|e| e["args"][1].clone())
            .collect()
    }
    /// The session `sessionID` reports `status` (`busy` or `idle`), once the
    /// plugin host has emitted it.
    fn session_status(&self, session: &str, status: &str) {
        let before = self.emitted("session.status");
        self.control(json!({"event":"session.status","properties":{"sessionID":session,"status":{"type":status}}}));
        assert!(
            common::wait_until(WAIT, || self.emitted("session.status") > before),
            "the host must emit session.status {status}"
        );
    }
    fn emitted(&self, kind: &str) -> usize {
        self.events()
            .iter()
            .filter(|e| e["kind"] == "emitted" && e["type"] == kind)
            .count()
    }
    fn message_state(&self, id: &str) -> Value {
        listing(&self.daemon, &self.pane)["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .map(|m| m["state"].clone())
            .unwrap_or(Value::Null)
    }
    /// Over `span`, the message is never undeliverable and never prompted a
    /// second time.
    fn held_through(&self, id: &str, span: Duration) {
        assert!(
            !common::wait_until(span, || self.message_state(id) == "undeliverable"
                || self.prompts().len() != 1),
            "a held message must outlive ack-wait × max_deliver with one prompt: {:?}",
            self.events()
        );
    }
    fn acknowledge(&self, id: &str) {
        self.wait_kind("user-message");
        assert!(common::wait_until(WAIT, || self
            .events()
            .iter()
            .filter(|e| e["kind"] == "user-message")
            .count()
            >= self.prompts().len()));
        let events = self.events();
        let parent = events
            .iter()
            .rev()
            .find(|e| e["kind"] == "user-message")
            .unwrap()["id"]
            .as_str()
            .unwrap();
        self.assistant(parent);
        state(&self.daemon, &self.pane, id, "acknowledged");
        let message = listing(&self.daemon, &self.pane)["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap()
            .clone();
        assert_eq!(message["ack_how"], "plugin_confirmed");
        assert!(
            !self
                .events()
                .iter()
                .any(|e| e["kind"] == "COMPOSER-TOUCHED"),
            "plugin touched composer"
        );
    }
}

/// Scenario: Deliver two tasks to a fake OpenCode home route through the generated TUI plugin. Create and navigate once, use the argv model, and acknowledge only when the matching assistant child appears.
#[spec("inbox/opencode/001")]
#[test]
fn opencode_001_home_creates_navigates_delivers_serially_and_acks() {
    let op = OpenCode::new("home", json!({}));
    armed(&op.daemon, &op.pane, 1);
    let first = op.post();
    let second = op.post();
    assert!(
        common::wait_until(WAIT, || op.prompts().len() == 2),
        "two deliveries: {:?}",
        op.events()
    );
    let events = op.events();
    let kinds: Vec<_> = events
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .filter(|k| ["create", "navigate", "prompt"].contains(k))
        .collect();
    assert_eq!(kinds, ["create", "navigate", "prompt", "prompt"]);
    for prompt in op.prompts() {
        assert_eq!(prompt["sessionID"], "created-1");
        assert_eq!(
            prompt["model"],
            json!({"providerID":"fixture","modelID":"argv-model"})
        );
        assert!(
            prompt["parts"][0]["text"]
                .as_str()
                .unwrap()
                .contains("[dot-agent-deck message m-")
        );
    }
    op.assistant("unrelated-human-message");
    assert!(
        !common::wait_until(Duration::from_millis(250), || listing(&op.daemon, &op.pane)
            ["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["state"] == "acknowledged")),
        "wrong assistant parent must not ack"
    );
    assert!(common::wait_until(WAIT, || op
        .events()
        .iter()
        .filter(|e| e["kind"] == "user-message")
        .count()
        == 2));
    let parents: Vec<_> = op
        .events()
        .into_iter()
        .filter(|e| e["kind"] == "user-message")
        .map(|e| e["id"].as_str().unwrap().to_owned())
        .collect();
    for parent in parents {
        op.assistant(&parent);
    }
    state(&op.daemon, &op.pane, &first, "acknowledged");
    state(&op.daemon, &op.pane, &second, "acknowledged");
}

/// Scenario: Exercise the full TUI and mini plugins with a pending task in another session, then three tasks in the current session and a human message between them. One assistant naming the third task must acknowledge the three tasks once without acknowledging the human or other session, and the tasks must not redeliver.
#[spec("inbox/opencode/023")]
#[test]
fn opencode_023_coalesced_turn_acks_prior_deck_messages_in_its_session_once() {
    for mini in [false, true] {
        assert_coalesced_turn_acks_once(mini);
    }
}

fn assert_coalesced_turn_acks_once(mini: bool) {
    let config = if mini {
        json!({"ackWaitMs":"10000", "server":true, "argv":["--mini"]})
    } else {
        json!({"ackWaitMs":"10000"})
    };
    let op = OpenCode::new("session", config);
    armed(&op.daemon, &op.pane, 1);
    let other = op.post();
    assert!(common::wait_until(WAIT, || op.events().iter().any(
        |e| e["kind"] == "user-message" && e["sessionID"] == "shown"
    )));
    if mini {
        op.control(json!({"event":"session.created","properties":{"info":{
            "id":"root","directory":op.daemon.home
        }}}));
        assert!(common::wait_until(WAIT, || op.events().iter().any(
            |e| e["kind"] == "emitted" && e["properties"]["info"]["id"] == "root"
        )));
    } else {
        op.control(json!({"route":{"name":"session","params":{"sessionID":"root"}}}));
        op.wait_kind("navigate");
    }

    let mut ids = Vec::new();
    for n in 0..3 {
        ids.push(op.post());
        assert!(
            common::wait_until(WAIT, || op
                .events()
                .iter()
                .filter(|e| e["kind"] == "user-message" && e["sessionID"] == "root")
                .count()
                == n + 1),
            "three delivered prompts in root: {:?}",
            op.events()
        );
        if n == 0 {
            op.control(json!({"event":"message.updated","properties":{"info":{
                "id":"human-between-deck-messages","role":"user","sessionID":"root"
            }}}));
            assert!(common::wait_until(WAIT, || op
                .events()
                .iter()
                .any(|e| e["kind"] == "emitted"
                    && e["properties"]["info"]["id"]
                        == "human-between-deck-messages")));
        }
    }
    let parents: Vec<_> = op
        .events()
        .into_iter()
        .filter(|e| e["kind"] == "user-message" && e["sessionID"] == "root")
        .map(|e| e["id"].as_str().unwrap().to_owned())
        .collect();
    let parent = parents.last().unwrap();
    op.assistant(parent);
    // The directly named parent is the control: the assistant event reached the plugin.
    state(&op.daemon, &op.pane, &ids[2], "acknowledged");
    assert!(
        common::wait_until(Duration::from_secs(2), || {
            let listed = listing(&op.daemon, &op.pane);
            ids.iter().all(|id| {
                listed["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["id"] == *id && m["state"] == "acknowledged")
            })
        }),
        "coalesced assistant must acknowledge all three deck messages in its session; ids={ids:?}, inbox={}, events={:?}",
        listing(&op.daemon, &op.pane),
        op.events()
    );
    // OpenCode can update an assistant repeatedly; each deck id must still ack once.
    op.assistant(parent);
    assert!(
        !common::wait_until(Duration::from_secs(11), || ids.iter().any(|id| op
            .prompts()
            .iter()
            .filter(|p| p["parts"][0]["text"].as_str().unwrap().contains(id))
            .count()
            > 1)),
        "acknowledged siblings must not redeliver: {:?}",
        op.events()
    );
    let listed = listing(&op.daemon, &op.pane);
    let messages = listed["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4, "human must not become a deck message");
    let other_message = messages.iter().find(|m| m["id"] == other).unwrap();
    assert_ne!(
        other_message["state"], "acknowledged",
        "other session must remain pending: {listed}"
    );
    for id in &ids {
        let message = messages.iter().find(|m| m["id"] == *id).unwrap();
        assert_eq!(message["ack_how"], "plugin_confirmed");
        assert_eq!(message["deliveries"], 1, "no redelivery: {message}");
    }
    // PRD #1666 §3: each sibling's attempt is committed once.
    for id in &ids {
        assert_eq!(
            op.commits(id),
            1,
            "each sibling commits once: {:?}",
            op.controls()
        );
    }
}

/// Scenario: Deliver while the fake TUI shows an existing session or a nested child view. Stay in the shown root session or navigate from the child to its root, copying that target's last user model and agent; navigation must wait while a dialog is open.
#[spec("inbox/opencode/002")]
#[test]
fn opencode_002_session_in_place_and_child_to_root() {
    for (route, target, navigate) in [("session", "shown", false), ("child", "root", true)] {
        let op = OpenCode::new(
            route,
            json!({"dialogOpen":navigate,"messages":{(target):[{"role":"user","model":{"providerID":"fixture","modelID":"session-model"},"agent":"session-agent"}]},"lastUser":{"id":"old-human","sessionID":"old-root","role":"user","model":{"providerID":"fixture","modelID":"wrong-process-model"},"agent":"wrong-process-agent"}}),
        );
        armed(&op.daemon, &op.pane, 1);
        let id = op.post();
        if navigate {
            assert!(
                !common::wait_until(Duration::from_millis(500), || op
                    .events()
                    .iter()
                    .any(|e| e["kind"] == "navigate" || e["kind"] == "prompt")),
                "a delivery that requires navigation must wait for the open dialog: {:?}",
                op.events()
            );
            op.control(json!({"dialogOpen":false}));
        }
        op.wait_kind("prompt");
        let prompts = op.prompts();
        assert_eq!(prompts[0]["sessionID"], target);
        assert_eq!(prompts[0]["model"]["modelID"], "session-model");
        assert_eq!(prompts[0]["agent"], "session-agent");
        assert_eq!(
            op.events().iter().any(|e| e["kind"] == "navigate"),
            navigate
        );
        assert!(!op.events().iter().any(|e| e["kind"] == "create"));
        op.acknowledge(&id);
    }
}

/// Scenario: Deliver from home with a process-observed user selection, an argv selection, or no selection. The generated plugin must use the fallback in that order and omit both model and agent when none is known.
#[spec("inbox/opencode/003")]
#[test]
fn opencode_003_model_and_agent_fallback_chain() {
    for (config, model, agent) in [
        (
            json!({"lastUser":{"id":"human","role":"user","sessionID":"old-root","model":{"providerID":"fixture","modelID":"observed"},"agent":"observed-agent"}}),
            Some("observed"),
            Some("observed-agent"),
        ),
        (
            json!({"argv":["-m","fixture/argv-model"]}),
            Some("argv-model"),
            None,
        ),
        (json!({"argv":[]}), None, None),
    ] {
        let op = OpenCode::new("home", config);
        let id = op.post();
        op.wait_kind("prompt");
        let prompt = &op.prompts()[0];
        assert_eq!(prompt["model"]["modelID"].as_str(), model);
        assert_eq!(prompt["agent"].as_str(), agent);
        op.acknowledge(&id);
    }
}

/// Scenario: Import two copies of the installed TUI plugin into one fake process. Only one watcher and one stream may arm, and a task must produce just one prompt.
#[spec("inbox/opencode/004")]
#[test]
fn opencode_004_double_load_guard_arms_once() {
    let op = OpenCode::new("home", json!({"doubleLoad":true}));
    armed(&op.daemon, &op.pane, 1);
    let id = op.post();
    op.wait_kind("prompt");
    assert_eq!(
        op.events().iter().filter(|e| e["kind"] == "spawn").count(),
        1
    );
    assert_eq!(op.prompts().len(), 1);
    op.acknowledge(&id);
}

/// Scenario: Deliver without an assistant child, then kill the watcher, close its input, or dispose the fake TUI. Nothing commits and nothing is requeued. A killed watcher is respawned and its registration resumes the attempt the context still holds (`retained`), with no repeat; a closed input or a dispose means the context is done, so its stream ends, the message stays taken and nothing prompts it again.
#[spec("inbox/opencode/005")]
#[test]
fn opencode_005_a_lost_watcher_resumes_and_a_closed_one_requeues_nothing() {
    let op = OpenCode::new("home", json!({}));
    let id = op.post();
    op.wait_kind("prompt");
    state(&op.daemon, &op.pane, &id, "fetched");
    op.control(json!({"killWatcher":true}));
    op.wait_kind("exit");
    assert!(
        common::wait_until(WAIT, || op.watchers().len() >= 2),
        "the watcher respawns: {:?}",
        op.events()
    );
    assert!(
        op.watchers()[1]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "--resumed"),
        "the respawned watcher says its callbacks survived: {:?}",
        op.watchers()
    );
    assert!(
        common::wait_until(WAIT, || op
            .controls()
            .iter()
            .any(|c| c["resume_answers"][0]["answer"] == "retained")),
        "the held attempt is resumed, retained: {:?}",
        op.controls()
    );
    assert!(
        !common::wait_until(Duration::from_millis(1500), || op.prompts().len() > 1),
        "a retained attempt is not repeated: {:?}",
        op.events()
    );
    state(&op.daemon, &op.pane, &id, "fetched");
    op.acknowledge(&id);

    for control in [json!({"eof":true}), json!({"dispose":true})] {
        let op = OpenCode::new("home", json!({}));
        let id = op.post();
        op.wait_kind("prompt");
        op.control(control.clone());
        op.wait_kind("exit");
        armed(&op.daemon, &op.pane, 0);
        assert!(
            !common::wait_until(Duration::from_millis(1500), || op.message_state(&id)
                != "fetched"
                || op.prompts().len() > 1),
            "{control}: a closed stream requeues nothing: {}",
            listing(&op.daemon, &op.pane)
        );
        assert_eq!(
            op.watchers().len(),
            1,
            "{control}: a done context starts no watcher"
        );
    }
}

/// Scenario: Leave a delivered task without an assistant child across many keepalives. The plugin keeps it in every possession snapshot, so it is never repeated and never undeliverable; once OpenCode answers it, it is acknowledged, with one prompt.
#[spec("inbox/opencode/006")]
#[test]
fn opencode_006_a_held_delivery_is_never_repeated() {
    let op = OpenCode::new("session", json!({"keepaliveMs":"300","maxDeliver":"2"}));
    let id = op.post();
    op.wait_kind("prompt");
    assert!(
        !common::wait_until(Duration::from_secs(4), || op.prompts().len() > 1
            || op.message_state(&id) != "fetched"),
        "a possessed delivery is never repeated: {:?}",
        op.events()
    );
    assert!(
        op.controls()
            .iter()
            .filter(|c| c["possess"].is_object())
            .count()
            >= 6,
        "a snapshot answers each keepalive: {:?}",
        op.controls()
    );
    op.acknowledge(&id);
    assert_eq!(op.prompts().len(), 1);
}

/// Scenario: Load the generated server plugin with mini, full-TUI, run, serve and attach argv. Only the main-thread mini process arms a native watcher; the full TUI's worker keeps a doorbell-only receiver that is never armed, and run, serve and attach start none.
#[spec("inbox/opencode/007")]
#[test]
fn opencode_007_server_inbox_is_mini_only() {
    let worker = OpenCode::new(
        "session",
        json!({"server":true,"worker":true,"argv":["--mini"]}),
    );
    assert!(
        !common::wait_until(Duration::from_millis(500), || listing(
            &worker.daemon,
            &worker.pane
        )["armed"]
            != 0),
        "a worker-thread server plugin must never arm, even with --mini argv"
    );
    // PRD #1666 §4: the full TUI's server context keeps a doorbell-only
    // receiver for its substitutions, which is never armed.
    let watchers = worker.watchers();
    assert_eq!(watchers.len(), 1, "{:?}", worker.events());
    assert!(
        watchers[0]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "--doorbell-only"),
        "{watchers:?}"
    );
    drop(worker);
    for (argv, expected) in [
        (json!(["--mini"]), 1),
        (json!(["--mini", "/fixture/project"]), 1),
        (json!([]), 0),
        (json!(["run"]), 0),
        (json!(["serve"]), 0),
        (json!(["attach", "http://fixture"]), 0),
    ] {
        let op = OpenCode::new("session", json!({"server":true,"argv":argv}));
        if expected == 1 {
            armed(&op.daemon, &op.pane, 1);
        } else {
            assert!(
                !common::wait_until(Duration::from_millis(500), || listing(&op.daemon, &op.pane)
                    ["armed"]
                    != 0),
                "server argv must not arm: {argv}"
            );
        }
        assert_eq!(
            op.events().iter().filter(|e| e["kind"] == "spawn").count(),
            expected
        );
    }
}

/// Scenario: Load mini's server component, create a new root followed by a child, and deliver to the latest root. The stream must identify opencode_mini and request repaint after the assistant-child acknowledgement and at deck-turn completion.
#[spec("inbox/opencode/008")]
#[test]
fn opencode_008_mini_targets_root_and_requests_ack_and_idle_repaint() {
    let op = OpenCode::new(
        "session",
        json!({"server":true,"argv":["--mini","-m","fixture/mini-model"]}),
    );
    armed(&op.daemon, &op.pane, 1);
    for info in [
        json!({"id":"mini-new","directory":op.daemon.home}),
        json!({"id":"mini-child","parentID":"mini-new","directory":op.daemon.home}),
    ] {
        op.control(json!({"event":"session.created","properties":{"info":info}}));
    }
    assert!(common::wait_until(WAIT, || op.events().iter().any(
        |e| e["kind"] == "emitted" && e["properties"]["info"]["id"] == "mini-child"
    )));
    let id = op.post();
    op.wait_kind("prompt");
    assert_eq!(listing(&op.daemon, &op.pane)["component"], "opencode_mini");
    let prompt = &op.prompts()[0];
    assert_eq!(prompt["sessionID"], "mini-new");
    assert_eq!(prompt["model"]["modelID"], "mini-model");
    op.acknowledge(&id);
    let repaints = || {
        op.events()
            .iter()
            .filter(|e| {
                e["kind"] == "stdin"
                    && e["text"].as_str().is_some_and(|s| {
                        s.lines().any(|l| {
                            serde_json::from_str::<Value>(l)
                                .ok()
                                .is_some_and(|v| v["repaint"] == true)
                        })
                    })
            })
            .count()
    };
    assert!(
        common::wait_until(WAIT, || repaints() == 1),
        "ack must repaint: {:?}",
        op.events()
    );
    op.control(json!({"event":"session.idle","properties":{"sessionID":"mini-new"}}));
    assert!(
        common::wait_until(WAIT, || repaints() == 2),
        "idle must repaint: {:?}",
        op.events()
    );
}

/// Scenario: Send repaint controls from mini and three other components to a PTY size probe. Only mini may shrink and restore the PTY; its scrollback and recorded geometry must stay unchanged, and a genuine resize during the gap must win.
#[spec("inbox/opencode/009")]
#[test]
fn opencode_009_only_mini_stream_nudges_without_losing_scrollback_or_geometry() {
    for component in [
        "opencode_plugin",
        "pi_extension",
        "claude_waiter",
        "opencode_mini",
    ] {
        let daemon = daemon_unbound(&[("DOT_AGENT_DECK_REPAINT_NUDGE_GAP_MS", "300")]);
        let script = daemon.home.join("size-probe.py");
        let base = daemon.home.join("probe");
        std::fs::write(&script,r#"import os,sys,tty,termios,signal,json,time
tty.setraw(0,termios.TCSANOW)
base=sys.argv[1]
def size(*_):
    s=os.get_terminal_size(0)
    with open(base+'.sizes','a') as f: f.write(json.dumps({'rows':s.lines,'cols':s.columns,'time':time.monotonic()})+'\n')
signal.signal(signal.SIGWINCH,size)
size()
with open(base+'.pid','w') as f:f.write(str(os.getpid()))
os.write(1,b'SCROLLBACK-KEEP-61b0\r\n')
while os.read(0,4096):pass
"#).unwrap();
        let reply = attach(
            &daemon,
            json!({"op":"start-agent","command":common::capability_export_command(&format!("exec python3 '{}' '{}'",script.display(),base.display())),
            "cwd":daemon.home,"rows":40,"cols":140,"env":[["DOT_AGENT_DECK_PANE_ID","mini-size-probe"]]}),
        );
        assert_eq!(reply["ok"], true);
        let agent = reply["id"].as_str().unwrap().to_string();
        assert!(common::wait_until(WAIT, || base
            .with_extension("pid")
            .exists()));
        let token = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(common::recorded_hook_capability(&daemon.home, &agent))
            .unwrap();
        let pane = Pane {
            id: "mini-size-probe".into(),
            agent,
            token,
            pid: std::fs::read_to_string(base.with_extension("pid"))
                .unwrap()
                .parse()
                .unwrap(),
            capture: base.with_extension("bytes"),
            attach_socket: daemon.attach_socket.clone(),
        };
        assert!(common::wait_until(WAIT, || screen(&daemon, &pane)
            .contains("SCROLLBACK-KEEP-61b0")));
        let before = common::pane_snapshot_on(&daemon.attach_socket, &pane.agent);
        let sizes = || {
            std::fs::read_to_string(base.with_extension("sizes"))
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .collect::<Vec<_>>()
        };
        let mut req = request(&pane, "inbox_wait");
        req["component"] = json!(component);
        req["mode"] = json!("stream");
        req["pid"] = json!(pane.pid);
        req["component_version"] = json!("size-probe");
        // PRD #1666 §11: the OpenCode components declare the floor.
        if component.starts_with("opencode") {
            req["adapter_integration"] = json!(2);
        }
        let mut stream = connect(&daemon, &req);
        armed(&daemon, &pane, 1);
        writeln!(stream.get_mut(), "{{\"repaint\":true}}").unwrap();
        if component == "opencode_mini" {
            assert!(
                common::wait_until(WAIT, || sizes().len() >= 3),
                "mini repaint must shrink and restore through nudge_repaint; sizes={:?}",
                sizes()
            );
            let observed = sizes();
            assert_eq!(observed[1]["cols"], 139);
            assert_eq!(observed[2]["cols"], 140);
            assert!(
                observed[2]["time"].as_f64().unwrap() - observed[1]["time"].as_f64().unwrap()
                    >= 0.25
            );
            assert_eq!(
                common::pane_snapshot_on(&daemon.attach_socket, &pane.agent),
                before,
                "nudge must keep scrollback bytes"
            );
            let records = attach(&daemon, json!({"op":"list-agents"}));
            let record = records["agent_records"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == pane.agent)
                .unwrap();
            assert_eq!(record["cols"], 140);
            assert_eq!(record["rows"], 40);
            // A burst while one nudge is active coalesces to one additional nudge.
            writeln!(stream.get_mut(), "{{\"repaint\":true}}").unwrap();
            assert!(common::wait_until(WAIT, || sizes().len() >= 4));
            stream
                .get_mut()
                .write_all(&b"{\"repaint\":true}\n".repeat(3))
                .unwrap();
            assert!(common::wait_until(WAIT, || sizes().len() >= 7));
            assert!(
                !common::wait_until(Duration::from_millis(500), || sizes().len() > 7),
                "repaint burst must coalesce: {:?}",
                sizes()
            );
            writeln!(stream.get_mut(), "{{\"repaint\":true}}").unwrap();
            assert!(common::wait_until(WAIT, || sizes().len() >= 8));
            let resized = attach(
                &daemon,
                json!({"op":"resize","id":pane.agent,"rows":42,"cols":132}),
            );
            assert_eq!(resized["ok"], true);
            assert!(common::wait_until(WAIT, || sizes().len() >= 9));
            assert!(
                !common::wait_until(
                    Duration::from_millis(650),
                    || sizes().last().unwrap()["cols"] != 132
                ),
                "restore must not overwrite a real resize that happened during the gap"
            );
            assert_eq!(
                sizes().last().unwrap()["cols"],
                132,
                "real resize during the nudge wins at restore"
            );
        } else {
            assert!(
                !common::wait_until(Duration::from_millis(650), || sizes().len() != 1),
                "non-mini stream must ignore repaint: {component} {:?}",
                sizes()
            );
        }
    }
}

/// Scenario: Announce OpenCode's full or mini component without keeping a stream armed. After the grace expires, the generic fixed doorbell must still reach the pane and the inbox may be fetched normally.
#[spec("inbox/opencode/010")]
#[test]
fn opencode_010_full_and_mini_unarmed_fall_back_after_grace() {
    for component in ["opencode_plugin", "opencode_mini"] {
        let daemon = daemon_unbound(&[]);
        let pane = pane(&daemon, "unarmed-opencode");
        status(&daemon, &pane, "idle", "opencode");
        let mut req = request(&pane, "inbox_announce");
        req["component"] = json!(component);
        req["wake"] = json!("native");
        req["pid"] = json!(pane.pid);
        req["component_version"] = json!("fallback-fixture");
        assert_eq!(hook(&daemon, req)["accepted"], true);
        let id = enqueue(&daemon, &pane, BODY);
        unchanged(&pane, Duration::from_millis(250));
        ring(&daemon, &pane, &id);
        fetch(&daemon, &pane, true);
        state(&daemon, &pane, &id, "acknowledged");
    }
}

/// Scenario: Submit fixed doorbells through the generated server plugin in the full TUI's worker thread and inspect the original text-parts array. The substitution hands the message over, bound to the server context's own receiver: an unrelated answer commits nothing and nothing repeats it, the matching answer acknowledges it, recalling a handled doorbell leaves newer work untouched, and one coalesced assistant turn acknowledges two intercepted tasks around an intervening human message, each once.
#[spec("inbox/opencode/011")]
#[test]
fn opencode_011_doorbell_intercepts_and_acks_matching_assistant_only() {
    // Hardening M2: interception runs only in the managed interactive
    // OpenCode — the full TUI's worker or `--mini` — never under `run`.
    let op = OpenCode::new("session", json!({"server":true,"worker":true}));
    // The server context's receiver is registered once it sends a snapshot.
    assert!(common::wait_until(WAIT, || op
        .controls()
        .iter()
        .any(|c| c["possess"].is_object())));
    let id = op.post();
    op.control(json!({"doorbell":fixed_line(&id)}));
    op.wait_kind("intercepted");
    let events = op.events();
    let event = events.iter().find(|e| e["kind"] == "intercepted").unwrap();
    assert_eq!(
        event["available"], true,
        "the server plugin must intercept chat.message"
    );
    let text = event["output"]["parts"][0]["text"].as_str().unwrap();
    assert!(text.contains("[dot-agent-deck message") && text.contains(BODY));
    assert!(!text.contains("waiting, run: dot-agent-deck inbox fetch"));
    state(&op.daemon, &op.pane, &id, "fetched");
    op.assistant("unrelated-human-message");
    assert!(
        !common::wait_until(Duration::from_millis(2500), || op.message_state(&id)
            != "fetched"),
        "an unrelated answer neither commits nor requeues the substitution"
    );
    op.assistant("fallback-user");
    state(&op.daemon, &op.pane, &id, "acknowledged");
    assert_eq!(op.commits(&id), 1, "{:?}", op.controls());
    let next = op.post();
    op.control(json!({"doorbell":fixed_line(&id),"userID":"history-user"}));
    assert!(common::wait_until(WAIT, || op
        .events()
        .iter()
        .filter(|e| e["kind"] == "intercepted")
        .count()
        == 2));
    op.assistant("history-user");
    state(&op.daemon, &op.pane, &next, "queued");
    let history = op
        .events()
        .into_iter()
        .rfind(|e| e["kind"] == "intercepted")
        .unwrap();
    assert!(
        !history["output"]["parts"].to_string().contains(&next),
        "recalled doorbell must not fetch a newer id"
    );
    assert!(
        !history["output"]["parts"]
            .to_string()
            .contains("waiting, run: dot-agent-deck inbox fetch"),
        "a recalled handled doorbell must not ask the model to fetch a newer task"
    );

    let op = OpenCode::new("session", json!({"server":true,"worker":true}));
    // The server context's receiver is registered once it sends a snapshot.
    assert!(common::wait_until(WAIT, || op
        .controls()
        .iter()
        .any(|c| c["possess"].is_object())));
    let first = op.post();
    op.control(json!({"doorbell":fixed_line(&first),"userID":"coalesced-first"}));
    state(&op.daemon, &op.pane, &first, "fetched");
    op.control(json!({"event":"message.updated","properties":{"info":{
        "id":"human-between-intercepted","role":"user","sessionID":"shown"
    }}}));
    let second = op.post();
    op.control(json!({"doorbell":fixed_line(&second),"userID":"coalesced-last"}));
    assert!(common::wait_until(WAIT, || op.events().iter().any(
        |e| e["kind"] == "user-message" && e["id"] == "coalesced-last"
    )));
    state(&op.daemon, &op.pane, &second, "fetched");
    op.assistant("coalesced-last");
    state(&op.daemon, &op.pane, &second, "acknowledged");
    state(&op.daemon, &op.pane, &first, "acknowledged");
    op.assistant("coalesced-last");
    assert!(
        !common::wait_until(Duration::from_secs(3), || {
            listing(&op.daemon, &op.pane)["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["state"] != "acknowledged" || m["deliveries"] != 1)
        }),
        "coalesced intercepted tasks must stay acknowledged without redelivery: {}",
        listing(&op.daemon, &op.pane)
    );
    assert_eq!(
        (op.commits(&first), op.commits(&second)),
        (1, 1),
        "each commits once"
    );
}

/// Scenario: Install through the real CLI into a commented TUI config twice. The exact entry must be inserted once, user bytes retained, and the second install must preserve inode and timestamp.
#[spec("inbox/opencode/012")]
#[test]
fn opencode_012_cli_installs_jsonc_once() {
    use std::os::unix::fs::MetadataExt;
    let daemon = daemon(&[]);
    let root = daemon.home.join(".config/opencode");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("tui.json");
    let before = "{/*user*/\"theme\":\"dark\",\"plugin\":[\"other\",]}\n";
    std::fs::write(&path, before).unwrap();
    let output = install_cli(&daemon);
    assert!(output.status.success(), "{output:?}");
    let entry = serde_json::to_string(&root.join("dot-agent-deck-tui.js")).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before.replacen("[", &format!("[{entry}, "), 1)
    );
    let first = std::fs::metadata(&path).unwrap();
    assert!(install_cli(&daemon).status.success());
    let second = std::fs::metadata(path).unwrap();
    assert_eq!(
        (first.ino(), first.mtime(), first.mtime_nsec()),
        (second.ino(), second.mtime(), second.mtime_nsec())
    );
}

/// Scenario: Explicitly install with a symlinked tui.json. The CLI must report the unchanged file, a refusal reason and the manual plugin path, exit nonzero, and leave the link target intact; an unarmed pane must still get the fallback doorbell.
#[spec("inbox/opencode/013")]
#[test]
fn opencode_013_cli_refusal_is_actionable_and_falls_back() {
    let daemon = daemon_unbound(&[]);
    let root = daemon.home.join(".config/opencode");
    std::fs::create_dir_all(&root).unwrap();
    let target = daemon.home.join("user-tui.json");
    std::fs::write(&target, "{\"theme\":\"dark\"}").unwrap();
    let path = root.join("tui.json");
    std::os::unix::fs::symlink(&target, &path).unwrap();
    let output = install_cli(&daemon);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "{\"theme\":\"dark\"}"
    );
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !output.status.success(),
        "explicit unsafe config install must exit nonzero; stdout={stdout}"
    );
    assert!(
        stdout.contains("Left ")
            && stdout.contains("unchanged")
            && stdout.contains("symlink")
            && stdout.contains(path.to_str().unwrap())
            && stdout.contains(root.join("dot-agent-deck-tui.js").to_str().unwrap()),
        "refusal must be actionable: {stdout}"
    );
    let pane = pane(&daemon, "refused-opencode");
    status(&daemon, &pane, "idle", "opencode");
    let id = enqueue(&daemon, &pane, BODY);
    ring(&daemon, &pane, &id);
    fetch(&daemon, &pane, true);
    state(&daemon, &pane, &id, "acknowledged");
}

/// Scenario: Load the server plugin and emit its initial Idle, delay the TUI plugin by two seconds, and queue a spawn-time task during that gap. Its early native announcement must hold the doorbell until the TUI stream arms and delivers the task with no input into the composer.
#[spec("inbox/opencode/014")]
#[test]
fn opencode_014_server_announces_before_idle_and_delayed_tui_arms() {
    let op = OpenCode::new("home", json!({"preServer":true}));
    let id = op.post();
    unchanged(&op.pane, Duration::from_millis(1000));
    state(&op.daemon, &op.pane, &id, "queued");
    armed(&op.daemon, &op.pane, 1);
    op.wait_kind("prompt");
    op.acknowledge(&id);
    assert!(
        bytes(&op.pane).is_empty(),
        "spawn-time native wake must never type a doorbell"
    );
}

/// Scenario: The shown OpenCode session is busy with a long turn when a deck task arrives, so the TUI plugin's prompt waits in OpenCode's queue. Across many keepalives — far longer than the deck's possession window — the plugin's snapshots keep holding it: no second prompt, no repeat, never undeliverable. When OpenCode answers the queued prompt the task is acknowledged.
#[spec("inbox/opencode/024")]
#[test]
fn opencode_024_a_prompt_queued_behind_a_long_turn_is_possessed_not_repeated() {
    let op = OpenCode::new("session", json!({"maxDeliver":"2","keepaliveMs":"300"}));
    armed(&op.daemon, &op.pane, 1);
    op.session_status("shown", "busy");
    let id = op.post();
    op.wait_kind("prompt");
    op.held_through(&id, Duration::from_secs(5));
    op.acknowledge(&id);
    assert_eq!(op.prompts().len(), 1);
}

/// Scenario: The OpenCode pane is a worker that takes jobs from a pool, and its shown session is busy with a long turn when the orchestrator offers it a job, so the TUI plugin's prompt waits in OpenCode's queue. Across many keepalives the job stays possessed with this worker: one prompt, never back in its pool. When OpenCode answers the queued prompt, the commit claims the job.
#[spec("inbox/opencode/027")]
#[test]
fn opencode_027_an_offer_queued_behind_a_long_turn_stays_assigned() {
    let op = OpenCode::new("session", json!({"offerPool":"review","keepaliveMs":"300"}));
    armed(&op.daemon, &op.pane, 1);
    op.session_status("shown", "busy");
    let orch = op.orchestrator.as_ref().unwrap();
    let mut req = request(orch, "inbox_offer");
    req["pool"] = json!("review");
    req["body"] = json!(BODY);
    req["claim_ttl_ms"] = json!(120_000);
    let reply = hook(&op.daemon, req);
    let id = reply["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the offer is accepted: {reply}"))
        .to_string();
    op.wait_kind("prompt");
    assert!(
        !common::wait_until(Duration::from_secs(5), || {
            op.message_state(&id) != "fetched" || op.prompts().len() != 1
        }),
        "a possessed job stays with its holder with one prompt: state {}, events {:?}",
        op.message_state(&id),
        op.events()
    );
    op.wait_kind("user-message");
    let events = op.events();
    let parent = events
        .iter()
        .rev()
        .find(|e| e["kind"] == "user-message")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    op.assistant(&parent);
    state(&op.daemon, &op.pane, &id, "claimed");
    assert_eq!(op.prompts().len(), 1, "the job ran once");
}

/// Scenario: A deck task waits behind a busy OpenCode session, then the person aborts: the session goes idle without answering it. The plugin drops it from its snapshots, and the deck prompts it again, marked as a possible repeat. A second task waits behind another long turn and the person reverts its prompt (OpenCode removes the message); it is prompted again too. Each is acknowledged once answered.
#[spec("inbox/opencode/025")]
#[test]
fn opencode_025_an_aborted_or_removed_queued_prompt_is_reinjected() {
    let op = OpenCode::new("session", json!({}));
    armed(&op.daemon, &op.pane, 1);
    op.session_status("shown", "busy");
    let aborted = op.post();
    op.wait_kind("user-message");
    op.session_status("shown", "idle");
    assert!(
        common::wait_until(WAIT, || op.prompts().len() >= 2),
        "an aborted queued prompt must be prompted again: {:?}",
        op.events()
    );
    assert_eq!(
        op.marked(&aborted),
        1,
        "the repeat is marked as a possible repeat"
    );
    op.acknowledge(&aborted);

    op.session_status("shown", "busy");
    let removed = op.post();
    let prompts = op.prompts().len();
    assert!(common::wait_until(WAIT, || op.prompts().len() > prompts));
    assert!(common::wait_until(WAIT, || op
        .events()
        .iter()
        .filter(|e| e["kind"] == "user-message")
        .count()
        >= op.prompts().len()));
    let user = op
        .events()
        .iter()
        .rev()
        .find(|e| e["kind"] == "user-message")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    op.control(
        json!({"event":"message.removed","properties":{"sessionID":"shown","messageID":user}}),
    );
    let after = op.prompts().len();
    assert!(
        common::wait_until(WAIT, || op.prompts().len() > after),
        "a removed queued prompt must be prompted again: {:?}",
        op.events()
    );
    op.acknowledge(&removed);
}

/// Scenario: Under `opencode --mini` the server plugin starts a deck task in a session busy with a long turn. Across many keepalives it stays possessed, with no second prompt and no undeliverable task, and is acknowledged once answered. A second task queued behind another long turn is prompted again, marked, once the session goes idle without answering it.
#[spec("inbox/opencode/026")]
#[test]
fn opencode_026_mini_possesses_a_queued_prompt_and_repeats_an_aborted_one() {
    let op = OpenCode::new(
        "session",
        json!({"server":true,"argv":["--mini"],"maxDeliver":"2","keepaliveMs":"300"}),
    );
    armed(&op.daemon, &op.pane, 1);
    op.session_status("shown", "busy");
    let held = op.post();
    op.wait_kind("prompt");
    op.held_through(&held, Duration::from_secs(5));
    op.acknowledge(&held);

    op.session_status("shown", "busy");
    let aborted = op.post();
    assert!(common::wait_until(WAIT, || op.prompts().len() >= 2));
    op.session_status("shown", "idle");
    assert!(
        common::wait_until(WAIT, || op.prompts().len() >= 3),
        "an aborted queued prompt must be prompted again: {:?}",
        op.events()
    );
    assert_eq!(op.marked(&aborted), 1);
    op.acknowledge(&aborted);
}

/// Scenario: The plugin's event loop stalls for longer than three keepalives while its watcher lives on. The watcher, hearing no snapshot, closes the stream; once the loop runs again the plugin respawns its watcher, whose registration resumes the attempt the context still holds, so it is not repeated and is acknowledged when answered.
#[spec("inbox/opencode/033")]
#[test]
fn opencode_033_a_stalled_adapter_is_dropped_by_its_watcher_and_resumes() {
    let op = OpenCode::new("session", json!({"keepaliveMs":"300"}));
    let id = op.post();
    op.wait_kind("prompt");
    op.control(json!({"stall": 2500}));
    op.wait_kind("stalled");
    assert!(
        common::wait_until(WAIT, || op.events().iter().any(|e| e["kind"] == "exit")),
        "the watcher closes a stream its adapter stopped answering: {:?}",
        op.events()
    );
    assert!(common::wait_until(WAIT, || op.watchers().len() >= 2));
    assert!(
        common::wait_until(WAIT, || op
            .controls()
            .iter()
            .any(|c| c["resume_answers"][0]["answer"] == "retained")),
        "{:?}",
        op.controls()
    );
    assert!(
        !common::wait_until(Duration::from_millis(1500), || op.prompts().len() > 1),
        "not repeated: {:?}",
        op.events()
    );
    op.acknowledge(&id);
}

/// Scenario: The agent starts on a deck message and the watcher's receipt publish takes two seconds — several keepalives. Throughout, the watcher keeps the attempt in each snapshot it forwards, so it never lapses or repeats; the commit then settles it with one prompt.
#[spec("inbox/opencode/034")]
#[test]
fn opencode_034_a_delayed_receipt_publish_is_held_across_keepalives() {
    let op = OpenCode::new(
        "session",
        json!({"keepaliveMs":"300","publishDelayMs":"2000"}),
    );
    let id = op.post();
    op.wait_kind("user-message");
    let parent = op
        .events()
        .iter()
        .rev()
        .find(|e| e["kind"] == "user-message")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    op.assistant(&parent);
    assert!(
        !common::wait_until(Duration::from_millis(1500), || op.prompts().len() > 1),
        "an in-flight publish is still possessed: {:?}",
        op.events()
    );
    state(&op.daemon, &op.pane, &id, "acknowledged");
    assert_eq!(op.prompts().len(), 1);
}

/// Scenario: Under `opencode --mini`, OpenCode re-runs the server plugin while a deck message is handed over and unanswered. The new instance knows nothing of it and answers the resume `unknown`, so the deck gives it one repeat on the same generation, marked as a possible repeat; the answer to that acknowledges it.
#[spec("inbox/opencode/035")]
#[test]
fn opencode_035_an_unknown_reload_gets_one_marked_repeat() {
    let op = OpenCode::new("session", json!({"server":true,"argv":["--mini"]}));
    armed(&op.daemon, &op.pane, 1);
    let id = op.post();
    op.wait_kind("prompt");
    op.control(json!({"reload": true}));
    op.wait_kind("reloaded");
    assert!(
        common::wait_until(WAIT, || op.prompts().len() >= 2),
        "the reloaded instance is handed a repeat: {:?}",
        op.events()
    );
    assert_eq!(
        op.marked(&id),
        1,
        "the repeat is marked: {:?}",
        op.prompts()
    );
    assert!(
        op.controls()
            .iter()
            .any(|c| c["resume_answers"][0]["answer"] == "unknown"),
        "{:?}",
        op.controls()
    );
    op.acknowledge(&id);
}

/// Scenario: The full TUI runs both contexts: the server plugin's doorbell-only receiver and the TUI plugin's receiver. The TUI's watcher is killed and respawns; its attempt is resumed and retained, and the server context's receiver is untouched — a doorbell substitution afterwards still binds to it and settles on its answer.
#[spec("inbox/opencode/036")]
#[test]
fn opencode_036_server_and_tui_contexts_reconnect_separately() {
    let op = OpenCode::new("home", json!({"preServer":true}));
    armed(&op.daemon, &op.pane, 1);
    let delivered = op.post();
    op.wait_kind("prompt");
    let tui_watchers = || op.watchers().iter().filter(|w| w[3] == "opencode").count();
    assert!(
        op.watchers().iter().any(|w| w[3] == "opencode-mini"
            && w.as_array().unwrap().iter().any(|a| a == "--doorbell-only")),
        "the server context keeps its own doorbell-only receiver: {:?}",
        op.watchers()
    );
    op.control(json!({"killWatcher":true}));
    assert!(
        common::wait_until(WAIT, || tui_watchers() >= 2),
        "the TUI watcher respawns: {:?}",
        op.events()
    );
    assert!(
        common::wait_until(WAIT, || op
            .controls()
            .iter()
            .any(|c| c["resume_answers"][0]["answer"] == "retained")),
        "{:?}",
        op.controls()
    );
    assert!(
        !common::wait_until(Duration::from_millis(1000), || op.prompts().len() > 1),
        "the TUI attempt is not repeated"
    );
    op.acknowledge(&delivered);
}

/// The last user message the plugin host recorded.
fn last_user(op: &OpenCode) -> String {
    op.events()
        .iter()
        .rev()
        .find(|e| e["kind"] == "user-message")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Scenario: Under `opencode --mini`, a deck message is prompted and answered, so the plugin reports it committed; its watcher publishes the commit receipt but the control never reaches the deck. OpenCode then re-runs the server plugin, whose new instance knows nothing of the message and answers the resume `unknown`. The deck finds the published receipt and acknowledges the message, and OpenCode is never prompted with it a second time.
#[spec("inbox/opencode/041")]
#[test]
fn opencode_041_a_published_commit_whose_control_was_lost_settles_after_a_reload() {
    let op = OpenCode::new(
        "session",
        json!({"server":true,"argv":["--mini"],"dropCommitted":true}),
    );
    armed(&op.daemon, &op.pane, 1);
    let id = op.post();
    op.wait_kind("user-message");
    op.assistant(&last_user(&op));
    assert!(
        common::wait_until(WAIT, || op.commits(&id) >= 1),
        "the plugin reports the commit: {:?}",
        op.controls()
    );
    op.control(json!({"reload": true}));
    op.wait_kind("reloaded");
    state(&op.daemon, &op.pane, &id, "acknowledged");
    assert!(
        !common::wait_until(Duration::from_millis(1500), || op.prompts().len() > 1),
        "no extra native call: {:?}",
        op.prompts()
    );
}

/// Scenario: The TUI plugin is handed a deck message and prompts it; its watcher is killed and respawns, and the new watcher's registration resumes the attempt, which the context retained. When OpenCode then answers, the replacement watcher publishes the commit receipt with the attempt's caps, rebound to it, while its control never reaches the deck. The deck settles the message from that receipt, with one prompt.
#[spec("inbox/opencode/042")]
#[test]
fn opencode_042_a_replacement_watcher_publishes_a_retained_attempts_receipt() {
    let op = OpenCode::new("session", json!({"dropCommitted":true,"keepaliveMs":"300"}));
    armed(&op.daemon, &op.pane, 1);
    let id = op.post();
    op.wait_kind("user-message");
    op.control(json!({"killWatcher":true}));
    assert!(common::wait_until(WAIT, || op.watchers().len() >= 2));
    assert!(
        common::wait_until(WAIT, || op
            .controls()
            .iter()
            .any(|c| c["resume_answers"][0]["answer"] == "retained")),
        "{:?}",
        op.controls()
    );
    op.assistant(&last_user(&op));
    state(&op.daemon, &op.pane, &id, "acknowledged");
    assert_eq!(
        op.prompts().len(),
        1,
        "settled from the receipt: {:?}",
        op.prompts()
    );
}

/// Scenario: The agent starts on a deck message while the watcher's `handed` receipt publish is still in flight, so the `handed` and `committed` publishes overlap, each taking two seconds. The `handed` publish finishing first does not drop the attempt from the snapshots while the `committed` one is still being written: across several keepalives it is never repeated, and the commit settles it with one prompt.
#[spec("inbox/opencode/043")]
#[test]
fn opencode_043_overlapping_publishes_keep_the_attempt_possessed() {
    let op = OpenCode::new(
        "session",
        json!({"keepaliveMs":"300","publishDelayMs":"2000"}),
    );
    let id = op.post();
    op.wait_kind("user-message");
    // The `handed` publish is in flight; the commit starts most of the way
    // through it, so the two overlap and the first finishes first.
    assert!(
        !common::wait_until(Duration::from_millis(1200), || op.prompts().len() > 1),
        "not repeated while the `handed` publish is in flight: {:?}",
        op.events()
    );
    op.assistant(&last_user(&op));
    assert!(
        !common::wait_until(Duration::from_millis(2500), || op.prompts().len() > 1),
        "an attempt with a publish still in flight is possessed: {:?}",
        op.events()
    );
    state(&op.daemon, &op.pane, &id, "acknowledged");
    assert_eq!(op.prompts().len(), 1);
}

/// Scenario: The OpenCode pane is a worker taking jobs from a pool. The orchestrator offers it a job; OpenCode records the prompt and then the call fails, as a dropped connection does after OpenCode took it. The plugin does not report the job not handed: it stays with this worker, never back in its pool, and is prompted again marked as a possible repeat; when that one is answered the job is claimed.
#[spec("inbox/opencode/044")]
#[test]
fn opencode_044_an_offer_whose_call_failed_after_it_began_stays_bound() {
    let op = OpenCode::new(
        "session",
        json!({"offerPool":"review","keepaliveMs":"300","throwAfterPrompt":1}),
    );
    armed(&op.daemon, &op.pane, 1);
    let orch = op.orchestrator.as_ref().unwrap();
    let mut req = request(orch, "inbox_offer");
    req["pool"] = json!("review");
    req["body"] = json!(BODY);
    req["claim_ttl_ms"] = json!(120_000);
    let reply = hook(&op.daemon, req);
    let id = reply["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the offer is accepted: {reply}"))
        .to_string();
    assert!(
        common::wait_until(WAIT, || op.prompts().len() >= 2),
        "the job is prompted again: {:?}",
        op.events()
    );
    assert_eq!(
        op.marked(&id),
        1,
        "the repeat is marked: {:?}",
        op.prompts()
    );
    assert!(
        !op.controls()
            .iter()
            .any(|c| c["not_handed"].as_array().is_some_and(|n| !n.is_empty())),
        "no not-handed witness: {:?}",
        op.controls()
    );
    assert_eq!(op.message_state(&id), "fetched", "still this worker's");
    op.assistant(&last_user(&op));
    state(&op.daemon, &op.pane, &id, "claimed");
}

/// Press Cancel on `id` for the OpenCode fixture's agent, at the revision the
/// daemon names when the first press is stale.
fn cancel(op: &OpenCode, id: &str) -> Value {
    let press = |revision: u64| {
        attach(
            &op.daemon,
            json!({"op":"inbox-cancel","pane_id":op.pane.id,"agent_id":op.pane.agent,
                "message_id":id,"revision":revision,"request_id":"cancel-1"}),
        )
    };
    let first = press(0);
    match first["inbox_action"]["outcome"].as_str() {
        Some("stale") => press(first["inbox_action"]["revision"].as_u64().unwrap()),
        _ => first,
    }
}

/// Scenario: The OpenCode pane is a worker taking jobs from a pool, and every receipt its watcher publishes takes three seconds. A job is offered to it, prompted, and answered, so the plugin reports it committed; while that receipt is still being written, the user cancels the job. The plugin answers the revoke `already_handed`, never `revoked_ack`, and the job ends cancelled on this worker instead of going back to its pool.
#[spec("inbox/opencode/047")]
#[test]
fn opencode_047_a_cancel_racing_a_commits_publication_keeps_the_offer_bound() {
    let op = OpenCode::new(
        "session",
        json!({"offerPool":"review","publishDelayMs":"3000"}),
    );
    armed(&op.daemon, &op.pane, 1);
    let orch = op.orchestrator.as_ref().unwrap();
    let mut req = request(orch, "inbox_offer");
    req["pool"] = json!("review");
    req["body"] = json!(BODY);
    req["claim_ttl_ms"] = json!(120_000);
    let reply = hook(&op.daemon, req);
    let id = reply["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the offer is accepted: {reply}"))
        .to_string();
    op.wait_kind("user-message");
    op.assistant(&last_user(&op));
    assert!(
        common::wait_until(WAIT, || op.commits(&id) >= 1),
        "the plugin reports the commit: {:?}",
        op.controls()
    );
    let pressed = cancel(&op, &id);
    assert_eq!(pressed["inbox_action"]["outcome"], "applied", "{pressed}");
    let answered = |what: &str| {
        op.controls()
            .iter()
            .filter_map(|c| c[what].as_array().cloned())
            .flatten()
            .any(|at| at["id"] == id.as_str())
    };
    assert!(
        common::wait_until(WAIT, || answered("already_handed")
            || answered("revoked_ack")),
        "the revoke is answered: {:?}",
        op.controls()
    );
    assert!(
        !answered("revoked_ack"),
        "a committed job is no not-handed witness: {:?}",
        op.controls()
    );
    assert!(
        common::wait_until(WAIT, || op.message_state(&id) == "undeliverable"),
        "the job ends cancelled on this worker: {}",
        op.message_state(&id)
    );
    assert!(
        !common::wait_until(Duration::from_millis(4000), || op.message_state(&id)
            != "undeliverable"),
        "and never goes back to its pool: {}",
        op.message_state(&id)
    );
    assert_eq!(op.prompts().len(), 1);
}

/// Scenario: The TUI plugin is handed a deck message and prompts it; its watcher is killed and respawns, the new registration resumes the attempt, and the context answers that it retained it. The deck's answer, with the attempt's caps, reaches the new watcher's publisher two seconds late, and OpenCode answers the message inside that window, so the plugin reports it committed before the caps are there; the commit's control never reaches the deck. The new watcher waits for the caps and publishes the commit receipt, and the deck settles the message from it, with one prompt.
#[spec("inbox/opencode/048")]
#[test]
fn opencode_048_a_commit_reported_before_the_rebound_caps_waits_for_them() {
    let op = OpenCode::new(
        "session",
        json!({"dropCommitted":true,"keepaliveMs":"300","rebindDelayMs":"2000"}),
    );
    armed(&op.daemon, &op.pane, 1);
    let id = op.post();
    op.wait_kind("user-message");
    op.control(json!({"killWatcher":true}));
    assert!(common::wait_until(WAIT, || op.watchers().len() >= 2));
    assert!(
        common::wait_until(WAIT, || op
            .controls()
            .iter()
            .any(|c| c["resume_answers"][0]["answer"] == "retained")),
        "{:?}",
        op.controls()
    );
    op.assistant(&last_user(&op));
    assert!(
        common::wait_until(WAIT, || op.commits(&id) >= 1),
        "the plugin reports the commit: {:?}",
        op.controls()
    );
    state(&op.daemon, &op.pane, &id, "acknowledged");
    assert_eq!(
        op.prompts().len(),
        1,
        "settled from the replacement watcher's receipt: {:?}",
        op.prompts()
    );
}

/// PRD #1666 §4: the pid of the first watcher the plugin started.
fn first_watcher_pid(op: &OpenCode) -> Value {
    op.events()
        .into_iter()
        .find(|e| e["kind"] == "spawn" && e["args"][1][1] == "wait")
        .expect("a watcher was started")["pid"]
        .clone()
}

/// The resume answers the plugin wrote to its watchers.
fn resume_answers(op: &OpenCode) -> Vec<Value> {
    op.controls()
        .into_iter()
        .filter_map(|c| c["resume_answers"].as_array().cloned())
        .flatten()
        .collect()
}

/// Scenario: The TUI plugin is handed a deck message and prompts it; its watcher is killed, and every respawn waits three seconds. OpenCode answers the message while no watcher runs, so the plugin has nowhere to report the commit yet. It keeps the report: the replacement's registration asks for the attempt back, the plugin answers that it retained it and then reports the commit, whose control never reaches the deck. The replacement publishes the commit receipt and the deck settles the message from it, with one prompt.
#[spec("inbox/opencode/049")]
#[test]
fn opencode_049_a_commit_reported_while_no_watcher_runs_waits_for_the_next() {
    let op = OpenCode::new("session", json!({"dropCommitted":true,"backoffMs":"3000"}));
    armed(&op.daemon, &op.pane, 1);
    let id = op.post();
    op.wait_kind("user-message");
    let first = first_watcher_pid(&op);
    op.control(json!({"killWatcher":true}));
    assert!(
        common::wait_until(WAIT, || op
            .events()
            .iter()
            .any(|e| e["kind"] == "exit" && e["pid"] == first)),
        "the watcher exits"
    );
    let before = op.emitted("message.updated");
    op.assistant(&last_user(&op));
    assert!(common::wait_until(WAIT, || op.emitted("message.updated") > before));
    assert_eq!(op.watchers().len(), 1, "answered during the backoff");
    assert_eq!(op.commits(&id), 0, "no watcher to report to yet");
    assert!(
        common::wait_until(WAIT, || op.commits(&id) >= 1),
        "the plugin reports the commit to the replacement: {:?}",
        op.controls()
    );
    assert!(
        resume_answers(&op)
            .iter()
            .any(|a| a["id"] == id && a["answer"] == "retained"),
        "the committed attempt is retained: {:?}",
        op.controls()
    );
    state(&op.daemon, &op.pane, &id, "acknowledged");
    assert_eq!(
        op.prompts().len(),
        1,
        "settled from the replacement watcher's receipt: {:?}",
        op.prompts()
    );
}

/// Scenario: The TUI plugin is handed a deck message and prompts it; its watcher is killed, and the replacement reads its registration two and a half seconds late, inside the deck's eight-second resume window. OpenCode answers the message after the replacement started but before it registered. The plugin holds the commit report until the registration has asked for the attempt back and been answered `retained`, then reports it, its control never reaching the deck; the replacement publishes the commit receipt and the deck settles the message from it, with one prompt.
#[spec("inbox/opencode/049")]
#[test]
fn opencode_049_a_commit_reported_before_the_replacement_registers_waits_for_it() {
    let op = OpenCode::new(
        "session",
        json!({"dropCommitted":true,"registerDelayMs":"2500","ackWaitMs":"8000"}),
    );
    armed(&op.daemon, &op.pane, 1);
    let id = op.post();
    op.wait_kind("user-message");
    op.control(json!({"killWatcher":true}));
    assert!(common::wait_until(WAIT, || op.watchers().len() >= 2));
    let before = op.emitted("message.updated");
    op.assistant(&last_user(&op));
    assert!(common::wait_until(WAIT, || op.emitted("message.updated") > before));
    assert!(
        resume_answers(&op).is_empty(),
        "answered before the replacement registered: {:?}",
        op.controls()
    );
    assert!(
        common::wait_until(WAIT, || op.commits(&id) >= 1),
        "the plugin reports the commit once registered: {:?}",
        op.controls()
    );
    // The plugin answers the replacement's resume in the step that reads its
    // registration, so that answer is the registration's mark in the
    // transcript: no commit may come before it.
    let controls = op.controls();
    let registered = controls
        .iter()
        .position(|c| {
            c["resume_answers"]
                .as_array()
                .is_some_and(|answers| answers.iter().any(|a| a["id"] == id))
        })
        .unwrap_or_else(|| panic!("the replacement asks for the attempt back: {controls:?}"));
    let committed = controls
        .iter()
        .position(|c| {
            c["committed"]
                .as_array()
                .is_some_and(|attempts| attempts.iter().any(|at| at["id"] == id))
        })
        .expect("the commit was reported");
    assert!(
        committed > registered,
        "a commit reached the replacement before it registered: {controls:?}"
    );
    assert!(
        resume_answers(&op)
            .iter()
            .any(|a| a["id"] == id && a["answer"] == "retained"),
        "the committed attempt is retained: {:?}",
        op.controls()
    );
    state(&op.daemon, &op.pane, &id, "acknowledged");
    assert_eq!(
        op.prompts().len(),
        1,
        "settled from the replacement watcher's receipt: {:?}",
        op.prompts()
    );
}

/// Scenario: The TUI plugin is handed a deck message and prompts it; its watcher is killed, and the replacement reads its registration two and a half seconds late, inside the deck's eight-second resume window. A commit for the attempt reaches the replacement's stdin directly, before that registration is read, and its control never reaches the deck. The replacement waits for its registration and the attempt's rebound caps instead of failing for want of a spool, publishes the commit receipt, and the deck settles the message from it, with one prompt.
#[spec("inbox/opencode/050")]
#[test]
fn opencode_050_a_stage_reaching_a_watcher_before_its_registration_waits_for_it() {
    let op = OpenCode::new(
        "session",
        json!({"dropCommitted":true,"registerDelayMs":"2500","ackWaitMs":"8000"}),
    );
    armed(&op.daemon, &op.pane, 1);
    let id = op.post();
    op.wait_kind("user-message");
    let may = op
        .controls()
        .into_iter()
        .find(|c| c["may"]["id"] == id)
        .expect("the final hand-over check");
    let attempt = json!({"id": id, "a": may["may"]["a"], "e": may["may"]["e"]});
    op.control(json!({"killWatcher":true}));
    assert!(common::wait_until(WAIT, || op.watchers().len() >= 2));
    op.control(json!({"rawStdin": {"committed": [attempt]}}));
    op.wait_kind("raw-stdin");
    assert!(
        resume_answers(&op).is_empty(),
        "reached the replacement before it registered: {:?}",
        op.controls()
    );
    state(&op.daemon, &op.pane, &id, "acknowledged");
    assert_eq!(
        op.prompts().len(),
        1,
        "settled from the replacement watcher's receipt: {:?}",
        op.prompts()
    );
}

/// Scenario: The TUI plugin is handed a deck message and prompts it, and every receipt publish takes a second and a half. OpenCode answers the message, the plugin reports the commit, and the context is disposed at once, so the plugin stops its watcher while that publish is still in flight. The watcher finishes the publish and reports the commit before it exits, and the deck acknowledges the message, with one prompt.
#[spec("inbox/opencode/051")]
#[test]
fn opencode_051_a_commit_reported_at_shutdown_reaches_the_deck() {
    let op = OpenCode::new("session", json!({"publishDelayMs":"1500"}));
    armed(&op.daemon, &op.pane, 1);
    let id = op.post();
    op.wait_kind("user-message");
    op.assistant(&last_user(&op));
    assert!(
        common::wait_until(WAIT, || op.commits(&id) >= 1),
        "the plugin reports the commit: {:?}",
        op.controls()
    );
    op.control(json!({"dispose":true}));
    op.wait_kind("disposed");
    state(&op.daemon, &op.pane, &id, "acknowledged");
    assert_eq!(op.prompts().len(), 1, "{:?}", op.prompts());
}

/// Scenario: A task is delegated to the OpenCode worker and prompted, and every receipt publish takes three seconds. The user cancels it; the plugin answers that it was already handed, and the task ends cancelled. OpenCode then answers it, so the plugin reports the commit, and the watcher is killed with SIGKILL while that publish is in flight. The replacement watcher's registration hands back the attempt's commit authority: the plugin sends the commit again, the replacement publishes it, the orchestrator is told the task was delivered after all despite Cancel, the receipts are not reported degraded, and no line the plugin reads carries a cap.
#[spec("inbox/opencode/052")]
#[test]
fn opencode_052_a_cancelled_tasks_commit_survives_its_watcher_killed_mid_publish() {
    let op = OpenCode::new(
        "session",
        json!({"offerPool":"review","keepWorker":true,"publishDelayMs":"3000"}),
    );
    armed(&op.daemon, &op.pane, 1);
    let orch = op.orchestrator.as_ref().unwrap();
    let mut req = request(orch, "delegate");
    req["task"] = json!(BODY);
    req["to"] = json!(["worker"]);
    req["timestamp"] = json!(chrono::Utc::now());
    let reply = hook(&op.daemon, req);
    let id = reply["message_ids"][0]
        .as_str()
        .unwrap_or_else(|| panic!("the delegation is accepted: {reply}"))
        .to_string();
    op.wait_kind("user-message");

    let pressed = cancel(&op, &id);
    assert_eq!(pressed["inbox_action"]["outcome"], "applied", "{pressed}");
    assert!(
        common::wait_until(WAIT, || op
            .controls()
            .iter()
            .filter_map(|c| c["already_handed"].as_array().cloned())
            .flatten()
            .any(|at| at["id"] == id.as_str())),
        "the revoke is answered already handed: {:?}",
        op.controls()
    );
    assert!(
        common::wait_until(WAIT, || op.message_state(&id) == "undeliverable"),
        "the task ends cancelled"
    );
    op.assistant(&last_user(&op));
    assert!(
        common::wait_until(WAIT, || op.commits(&id) >= 1),
        "the plugin reports the commit: {:?}",
        op.controls()
    );
    let first = first_watcher_pid(&op);
    op.control(json!({"killWatcher":true}));
    assert!(
        common::wait_until(WAIT, || op
            .events()
            .iter()
            .any(|e| e["kind"] == "exit" && e["pid"] == first)),
        "the watcher is killed mid-publish"
    );
    assert!(
        common::wait_until(WAIT, || op.commits(&id) >= 2),
        "the plugin sends the commit again to the replacement: {:?}",
        op.controls()
    );

    let corrected = || {
        inbox::listing(&op.daemon, orch)["messages"]
            .as_array()
            .is_some_and(|messages| {
                messages
                    .iter()
                    .any(|m| m["subject"] == "task delivered after all")
            })
    };
    assert!(
        common::wait_until(Duration::from_secs(20), corrected),
        "the orchestrator is told the task was delivered after all: {:?}",
        inbox::listing(&op.daemon, orch)
    );
    let fetched = inbox::fetch(&op.daemon, orch, true).to_string();
    assert!(
        fetched.contains("delivered after all, despite Cancel"),
        "the correction says despite Cancel: {fetched}"
    );
    let records = attach(&op.daemon, json!({"op":"list-agents"}));
    let record = records["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == op.pane.agent.as_str())
        .unwrap_or_else(|| panic!("the worker's record: {records}"));
    assert!(
        record["inbox"]["receipts_degraded"].is_null(),
        "the replacement had the cap, so nothing is degraded: {record}"
    );
    let read: Vec<String> = op
        .events()
        .into_iter()
        .filter(|e| e["kind"] == "stdout")
        .filter_map(|e| e["text"].as_str().map(str::to_owned))
        .collect();
    assert!(
        read.iter().any(|text| text.contains("\"correctable\"")),
        "the replacement's registration names the attempt: {read:?}"
    );
    assert!(
        read.iter()
            .all(|text| !text.contains("\"caps\"") && !text.contains("\"commit\":\"")),
        "a cap reached the plugin: {read:?}"
    );
}
