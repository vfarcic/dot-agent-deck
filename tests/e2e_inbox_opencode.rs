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
        let mut start = json!({"op":"start-agent","command":command,"cwd":daemon.home,
            "agent_type":"opencode","rows":40,"cols":140,
            "env":[["DOT_AGENT_DECK_PANE_ID","opencode-fixture"],["PATH",path]],"display_name":"opencode fixture"});
        if let Some(pool) = pool {
            std::fs::write(
                daemon.cwd().join(".dot-agent-deck.toml"),
                format!("[[orchestrations]]\nname = 'offers'\n[[orchestrations.roles]]\nname = 'orchestrator'\ncommand = 'cat'\nstart = true\n[[orchestrations.roles]]\nname = 'worker'\ncommand = 'cat'\ntake_offers = ['{pool}']\n"),
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
    /// The `hold` lines the plugin's watcher wrote for `id`.
    fn holds(&self, id: &str) -> usize {
        self.events()
            .iter()
            .filter(|e| e["kind"] == "stdin")
            .filter_map(|e| serde_json::from_str::<Value>(e["text"].as_str()?.trim()).ok())
            .filter(|line| {
                line["hold"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|held| held == id))
            })
            .count()
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
    let acks: Vec<Value> = op
        .events()
        .iter()
        .filter(|e| e["kind"] == "stdin")
        .filter_map(|e| e["text"].as_str())
        .flat_map(str::lines)
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|v| v["ack"].as_array().cloned())
        .flatten()
        .collect();
    assert_eq!(
        acks.len(),
        3,
        "exactly three deck acknowledgements: {acks:?}"
    );
    for id in &ids {
        assert_eq!(
            acks.iter().filter(|v| **v == *id).count(),
            1,
            "each sibling must ack once: {acks:?}"
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

/// Scenario: Deliver without emitting an assistant child, then close the watcher input or dispose the fake TUI. No acknowledgement may occur; the watcher must exit and the daemon must requeue the delivery.
#[spec("inbox/opencode/005")]
#[test]
fn opencode_005_stdin_eof_and_dispose_exit_and_requeue_unacked() {
    for control in [json!({"eof":true}), json!({"dispose":true})] {
        let op = OpenCode::new("home", json!({}));
        let id = op.post();
        op.wait_kind("prompt");
        state(&op.daemon, &op.pane, &id, "fetched");
        op.control(control);
        op.wait_kind("exit");
        armed(&op.daemon, &op.pane, 0);
        state(&op.daemon, &op.pane, &id, "queued");
    }
}

/// Scenario: Leave a delivered task without an assistant child. It must remain unacknowledged and be redelivered after the accelerated acknowledgement deadline.
#[spec("inbox/opencode/006")]
#[test]
fn opencode_006_aborted_delivery_redelivers_without_ack() {
    let op = OpenCode::new("session", json!({}));
    let id = op.post();
    op.wait_kind("prompt");
    assert!(
        common::wait_until(WAIT, || op.prompts().len() >= 2),
        "unacked delivery must redeliver: {:?}",
        op.events()
    );
    assert!(
        listing(&op.daemon, &op.pane)["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["state"] != "acknowledged")
    );
    op.acknowledge(&id);
}

/// Scenario: Load the generated server plugin with mini, full-TUI, run, serve and attach argv. Only the main-thread mini process may arm a native watcher.
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
    assert!(!worker.events().iter().any(|event| event["kind"] == "spawn"));
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

/// Scenario: Submit fixed doorbells through the generated server plugin in the full TUI's worker thread and inspect the original text-parts array. An aborted delivery must redeliver, recalling a handled doorbell must leave newer work untouched, and one coalesced assistant turn must acknowledge two intercepted tasks around an intervening human message.
#[spec("inbox/opencode/011")]
#[test]
fn opencode_011_doorbell_intercepts_and_acks_matching_assistant_only() {
    // Hardening M2: interception runs only in the managed interactive
    // OpenCode — the full TUI's worker or `--mini` — never under `run`.
    let op = OpenCode::new("session", json!({"server":true,"worker":true}));
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
    state(&op.daemon, &op.pane, &id, "queued");
    op.control(json!({"doorbell":fixed_line(&id),"userID":"retry-user"}));
    assert!(common::wait_until(WAIT, || op
        .events()
        .iter()
        .filter(|e| e["kind"] == "intercepted")
        .count()
        == 2));
    state(&op.daemon, &op.pane, &id, "fetched");
    op.assistant("unrelated-retry-message");
    assert!(
        !common::wait_until(Duration::from_millis(250), || listing(&op.daemon, &op.pane)
            ["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == id && m["state"] == "acknowledged")),
        "an unrelated assistant parent must not acknowledge the retry"
    );
    op.assistant("retry-user");
    state(&op.daemon, &op.pane, &id, "acknowledged");
    let next = op.post();
    op.control(json!({"doorbell":fixed_line(&id),"userID":"history-user"}));
    assert!(common::wait_until(WAIT, || op
        .events()
        .iter()
        .filter(|e| e["kind"] == "intercepted")
        .count()
        == 3));
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

    // Hold receipt long enough to make the first task remain fetched while
    // the second is intercepted; a receipt deadline is not the signal under test.
    let op = OpenCode::new(
        "session",
        json!({"server":true,"worker":true,"ackWaitMs":"10000"}),
    );
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
        !common::wait_until(Duration::from_secs(11), || {
            listing(&op.daemon, &op.pane)["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["state"] != "acknowledged" || m["deliveries"] != 1)
        }),
        "coalesced intercepted tasks must stay acknowledged without redelivery: {}",
        listing(&op.daemon, &op.pane)
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

/// Scenario: The shown OpenCode session is busy with a long turn when a deck task arrives, so the TUI plugin's prompt waits in OpenCode's queue. For four ack-waits — twice the deck's hand-over limit — every redelivery is answered with hold: no second prompt, and the task is never reported undeliverable. When OpenCode answers the queued prompt the task is acknowledged.
#[spec("inbox/opencode/024")]
#[test]
fn opencode_024_a_prompt_queued_behind_a_long_turn_is_held_not_reinjected() {
    let op = OpenCode::new("session", json!({"maxDeliver":"2","ackWaitMs":"3000"}));
    armed(&op.daemon, &op.pane, 1);
    op.session_status("shown", "busy");
    let id = op.post();
    op.wait_kind("prompt");
    op.held_through(&id, Duration::from_millis(12000));
    assert!(
        op.holds(&id) >= 2,
        "each redelivery answers hold: {:?}",
        op.events()
    );
    op.acknowledge(&id);
    assert_eq!(op.prompts().len(), 1);
}

/// Scenario: The OpenCode pane is a worker that takes jobs from a pool, and its shown session is busy with a long turn when the orchestrator offers it a job, so the TUI plugin's prompt waits in OpenCode's queue. For four ack-waits every redelivery of the job is answered with hold: there is no second prompt, and the job stays with this worker instead of going back to its pool. When OpenCode answers the queued prompt, the plugin's acknowledgement claims the job.
#[spec("inbox/opencode/027")]
#[test]
fn opencode_027_an_offer_queued_behind_a_long_turn_is_held_and_stays_assigned() {
    let op = OpenCode::new("session", json!({"offerPool":"review","ackWaitMs":"3000"}));
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
        !common::wait_until(Duration::from_millis(12000), || {
            let state = op.message_state(&id);
            !(state == "queued" || state == "fetched") || op.prompts().len() != 1
        }),
        "a held job stays with its holder with one prompt: state {}, events {:?}",
        op.message_state(&id),
        op.events()
    );
    assert!(
        op.holds(&id) >= 2,
        "each redelivery answers hold: {:?}",
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

/// Scenario: A deck task waits behind a busy OpenCode session, then the person aborts: the session goes idle without answering it. The deck's next redelivery prompts it again. A second task waits behind another long turn and the person reverts its prompt (OpenCode removes the message); its redelivery is prompted again too. Each is acknowledged once answered.
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

/// Scenario: Under `opencode --mini` the server plugin starts a deck task in a session busy with a long turn. Each redelivery over four ack-waits is answered with hold, with no second prompt and no undeliverable task, and the task is acknowledged once answered. A second task queued behind another long turn is prompted again once the session goes idle without answering it.
#[spec("inbox/opencode/026")]
#[test]
fn opencode_026_mini_holds_a_queued_prompt_and_reinjects_an_aborted_one() {
    let op = OpenCode::new(
        "session",
        json!({"server":true,"argv":["--mini"],"maxDeliver":"2","ackWaitMs":"3000"}),
    );
    armed(&op.daemon, &op.pane, 1);
    op.session_status("shown", "busy");
    let held = op.post();
    op.wait_kind("prompt");
    op.held_through(&held, Duration::from_millis(12000));
    assert!(
        op.holds(&held) >= 2,
        "each redelivery answers hold: {:?}",
        op.events()
    );
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
    op.acknowledge(&aborted);
}
