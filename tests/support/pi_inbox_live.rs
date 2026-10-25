//! Real interactive Pi, isolated configuration, and genuine CLI delegates.
#![allow(dead_code)]

use crate::{common, inbox};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub const BUDGET: Duration = Duration::from_secs(120);
pub const CONTENT: &str = "PI_INBOX_OK_42c7";
pub const WINDOW_MS: u64 = 2000;

#[derive(Default)]
pub struct Options {
    pub absent_auth: bool,
    pub short_windows: bool,
    pub register_delay_ms: u64,
    pub doorbell_only: bool,
    pub stale: bool,
    pub obstructed: bool,
}

/// Only these variables survive the absent-auth Pi launch. This also excludes
/// future provider keys instead of depending on a blacklist staying current.
const AUTHLESS_ENV: &[&str] = &[
    "HOME",
    "PATH",
    "TERM",
    "COLORTERM",
    "LANG",
    "PI_CODING_AGENT_DIR",
    "DOT_AGENT_DECK_BIN",
    "DOT_AGENT_DECK_EXE",
    "DOT_AGENT_DECK_SOCKET",
    "DOT_AGENT_DECK_ATTACH_SOCKET",
    "DOT_AGENT_DECK_PANE_ID",
    "DOT_AGENT_DECK_AGENT_ID",
    "DOT_AGENT_DECK_HOOK_TOKEN",
    "DOT_AGENT_DECK_PANE_CAPABILITY",
    "DOT_AGENT_DECK_INBOX_SPOOL",
    "DOT_AGENT_DECK_VIA_DAEMON",
    "DOT_AGENT_DECK_STATE_DIR",
    "DOT_AGENT_DECK_TEST_CONFIG_ROOT",
    "DOT_AGENT_DECK_TEST_CONFIG_WRITE",
    "DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS",
    "DECK_TEST_PI_EVENTS",
    "DECK_TEST_PI_TIMELINE",
    "DECK_TEST_REAL_BIN",
];

/// `common::check_pi_available`: Pi installed, a credential for
/// `common::pi_test_model()`'s provider, and a one-turn probe on that model.
pub fn available() -> Result<(), String> {
    common::check_pi_available()
}

pub struct Pi {
    pub pane: inbox::Pane,
    pub deck: common::TuiDeck,
    pub work: PathBuf,
    next: std::cell::Cell<usize>,
    pub diagnostics: PathBuf,
    pub agent_dir: PathBuf,
    pub daemon_events: common::EventSub,
    observer_stop: Arc<AtomicBool>,
    observer: Option<std::thread::JoinHandle<()>>,
}

impl Pi {
    pub fn new() -> Self {
        Self::with_options(Options::default())
    }
    pub fn with_options(options: Options) -> Self {
        common::install_credential_redaction();
        let mut builder = common::TuiDeck::builder()
            // The repair case needs 200-column cards: the sidebar takes one
            // third of the terminal and its wide grid has three columns.
            .with_pty_size(
                if options.obstructed { 1800 } else { 200 },
                if options.obstructed { 30 } else { 50 },
            )
            .with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
            .with_env("DOT_AGENT_DECK_LOG", "pi-inbox-live.log")
            .with_env("DOT_AGENT_DECK_INBOX_ARM_GRACE_MS", "10000")
            .with_env("DOT_AGENT_DECK_DOORBELL_TICK_MS", "25")
            .with_env("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000")
            .with_env(
                "DOT_AGENT_DECK_INBOX_ACK_WAIT_MS",
                if options.short_windows {
                    "2000"
                } else {
                    "180000"
                },
            )
            .with_env("DOT_AGENT_DECK_INBOX_NOT_STARTED_MS", WINDOW_MS.to_string())
            // A held task shows its reason after five seconds, not two
            // minutes (`inbox/pi/020`).
            .with_env("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "5000")
            .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "600");
        builder = if options.absent_auth {
            builder.without_agent_credentials()
        } else {
            // Seeds `<HOME>/.pi/agent/auth.json`, the `PI_CODING_AGENT_DIR`
            // the Pi pane below is given.
            builder.with_imported_pi_credentials()
        };
        let deck = builder.launch_with_fixture("minimal");
        assert!(deck.wait_for_grid_string_within("No active agents", BUDGET));
        let work = deck.workdir().join("project");
        std::fs::create_dir_all(&work).unwrap();
        let diagnostics = diagnostics_dir();
        eprintln!("Pi live diagnostics: {}", diagnostics.display());
        let agent_dir = if options.stale || options.obstructed {
            work.join("child-pi-agent")
        } else {
            deck.home_dir().join(".pi/agent")
        };
        std::fs::create_dir_all(&agent_dir).unwrap();
        let child_home = if options.stale || options.obstructed {
            let path = work.join("child-home");
            std::fs::create_dir_all(&path).unwrap();
            path
        } else {
            deck.home_dir().to_owned()
        };
        let child_agent_dir = if options.stale {
            PathBuf::from("child-pi-agent")
        } else {
            agent_dir.clone()
        };
        if options.stale {
            common::import_pi_credentials(&agent_dir).unwrap();
            let target = agent_dir.join("extensions/dot-agent-deck");
            std::fs::create_dir_all(&target).unwrap();
            std::fs::write(
                target.join("index.ts"),
                "throw new Error('STALE_PI_TEST_FIXTURE');",
            )
            .unwrap();
        }
        let observer = agent_dir.join("extensions/inbox-test-observer.ts");
        std::fs::create_dir_all(observer.parent().unwrap()).unwrap();
        // Passive observation only: delivery, acknowledgement and watcher
        // lifetime remain entirely the bundled extension's responsibility.
        std::fs::write(
            &observer,
            include_str!("../fixtures/pi-inbox/observe-native.ts"),
        )
        .unwrap();
        if options.obstructed {
            std::fs::write(
                agent_dir.join("extensions/dot-agent-deck"),
                "structural obstruction",
            )
            .unwrap();
        }
        let cli_observer = write_cli_observer(&diagnostics);
        use std::os::unix::fs::PermissionsExt;
        let installed = deck.home_dir().join(".local/bin/dot-agent-deck");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        if !installed.exists()
            && std::fs::hard_link(common::contained_deck_binary(), &installed).is_err()
        {
            std::fs::copy(common::contained_deck_binary(), &installed).unwrap();
        }
        let path = format!(
            "{}:{}",
            installed.parent().unwrap().display(),
            std::env::var("PATH").unwrap()
        );
        let command = if options.absent_auth {
            let assignments = AUTHLESS_ENV
                .iter()
                .map(|key| format!("{key}=\"${{{key}}}\""))
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                "exec env -i {assignments} pi --provider anthropic --model claude-haiku-4-5 --approve"
            )
        } else {
            format!("exec pi {} --approve", common::pi_test_model().cli_args())
        };
        let role_command = common::capability_export_command(&command);
        std::fs::write(work.join(".dot-agent-deck.toml"), format!(
            "[[orchestrations]]\nname = \"pi-inbox\"\n[[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"./delegate-loop.sh\"\nstart = true\n[[orchestrations.roles]]\nname = \"coder\"\ncommand = {role_command:?}\nagent = \"pi\"\nclear = false\n"
        )).unwrap();
        let script = work.join("delegate-loop.sh");
        std::fs::write(&script, "#!/bin/sh\ni=1\nwhile :; do\n while [ ! -f \"delegate-$i.go\" ]; do sleep 0.1; done\n dot-agent-deck delegate --to coder --task-file \"delegate-$i.md\" > \"delegate-$i.log\" 2>&1\n printf '%s' \"$?\" > \"delegate-$i.exit\"\n i=$((i+1))\ndone\n").unwrap();
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let daemon_events = deck.subscribe_events();
        if options.absent_auth {
            assert!(
                !agent_dir.join("auth.json").exists(),
                "no auth.json supplied before Pi exec"
            );
        }
        let reply = inbox::attach(
            &deck,
            json!({"op":"start-agent",
            "command":role_command,"cwd":work,"agent_type":"pi","rows":40,"cols":140,
            "env":[["DOT_AGENT_DECK_PANE_ID","pi-inbox-coder"],["PATH",path],
                ["HOME",child_home],["PI_CODING_AGENT_DIR",child_agent_dir],["DECK_TEST_PI_EVENTS",diagnostics.join("native.jsonl")],
                ["DOT_AGENT_DECK_BIN",cli_observer],["DECK_TEST_REAL_BIN",common::contained_deck_binary()],
                ["DECK_TEST_PI_TIMELINE",diagnostics.join("timeline.jsonl")],
                ["DECK_TEST_DOORBELL_ONLY",if options.doorbell_only {"1"} else {"0"}],
                ["DOT_AGENT_DECK_INBOX_TEST_REGISTER_DELAY_MS",options.register_delay_ms.to_string()]],
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
        let observer_stop = Arc::new(AtomicBool::new(false));
        let stop = observer_stop.clone();
        let socket = deck.hook_socket_path().to_owned();
        let attach_socket = deck.attach_socket_path().to_owned();
        let sampled_agent = pane.agent.clone();
        let request = inbox::request(&pane, "inbox_list");
        let samples = diagnostics.join("samples.jsonl");
        let observer = std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};
            let mut output = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(samples)
                .unwrap();
            while !stop.load(Ordering::Relaxed) {
                let sample = || -> std::io::Result<Value> {
                    let mut stream = std::os::unix::net::UnixStream::connect(&socket)?;
                    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
                    writeln!(stream, "{request}")?;
                    let mut line = String::new();
                    BufReader::new(stream).read_line(&mut line)?;
                    let listing: Value = serde_json::from_str(&line)?;
                    let response = common::attach_request_on(
                        &attach_socket,
                        &dot_agent_deck::daemon_protocol::AttachRequest::ListAgents,
                    )
                    .map_err(std::io::Error::other)?;
                    let cards = serde_json::to_value(response)?;
                    let card = cards["agent_records"]
                        .as_array()
                        .and_then(|records| records.iter().find(|r| r["id"] == sampled_agent))
                        .cloned()
                        .ok_or_else(|| std::io::Error::other("sampled Pi generation missing"))?;
                    Ok(json!({"inbox":listing,"card":card}))
                };
                let value = match sample() {
                    Ok(listing) => json!({"at":chrono::Utc::now().to_rfc3339(),"listing":listing}),
                    Err(error) => json!({"error":error.to_string()}),
                };
                writeln!(
                    output,
                    "{}",
                    common::redact_credentials_for_output(&value.to_string())
                )
                .unwrap();
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let result = Self {
            pane,
            deck,
            work,
            next: std::cell::Cell::new(1),
            diagnostics,
            agent_dir,
            daemon_events,
            observer_stop,
            observer: Some(observer),
        };
        if options.obstructed {
            return result;
        }
        assert!(
            common::wait_until(BUDGET, || result
                .events()
                .iter()
                .any(|event| event["type"] == "session_start")),
            "Pi observer must see session_start; {}",
            result.evidence()
        );
        if options.register_delay_ms == 0 {
            result.registered();
        }
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
        read_jsonl(&self.diagnostics.join("native.jsonl"))
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
        self.save_diagnostics();
        format!(
            "diagnostics={}\ninbox={}\ngrid={}\nlog={}",
            self.diagnostics.display(),
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
        let mut parents = vec![self.pid()];
        let real_binary = common::contained_deck_binary();
        let expected_argv0 = real_binary.to_string_lossy();
        while let Some(parent) = parents.pop() {
            let Ok(tasks) = std::fs::read_dir(format!("/proc/{parent}/task")) else {
                continue;
            };
            for task in tasks.flatten() {
                for child in std::fs::read_to_string(task.path().join("children"))
                    .unwrap_or_default()
                    .split_whitespace()
                {
                    if let Ok(pid) = child.parse::<i32>() {
                        parents.push(pid);
                    }
                    let cmd = std::fs::read(format!("/proc/{child}/cmdline")).unwrap_or_default();
                    if cmd.split(|b| *b == 0).next() == Some(expected_argv0.as_bytes())
                        && cmd.split(|b| *b == 0).any(|arg| arg == b"inbox")
                        && cmd.split(|b| *b == 0).any(|arg| arg == b"wait")
                    {
                        // The forked pipe observer still has the launcher's
                        // argv. Require the exec'd binary's argv[0] too.
                        found.push(child.parse().unwrap());
                    }
                }
            }
        }
        found.sort();
        found.dedup();
        found
    }

    pub fn timeline(&self) -> Vec<Value> {
        read_jsonl(&self.diagnostics.join("timeline.jsonl"))
    }
    pub fn registered(&self) {
        inbox::armed(&self.deck, &self.pane, 1);
        assert!(
            common::wait_until(BUDGET, || self
                .timeline()
                .iter()
                .any(|event| !event["registered"].is_null())),
            "receipt registration must reach the real Pi receiver; {}",
            self.evidence()
        );
    }
    pub fn samples(&self) -> Vec<Value> {
        read_jsonl(&self.diagnostics.join("samples.jsonl"))
    }
    pub fn message(&self, id: &str) -> Value {
        inbox::listing(&self.deck, &self.pane)["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap()
            .clone()
    }
    pub fn reports(&self, stage: &str, id: &str) -> Vec<Value> {
        self.timeline()
            .into_iter()
            .filter(|v| {
                v[stage]
                    .as_array()
                    .is_some_and(|refs| refs.iter().any(|r| r["id"] == id))
            })
            .collect()
    }
    pub fn wait_report(&self, stage: &str, id: &str) {
        assert!(
            common::wait_until(BUDGET, || !self.reports(stage, id).is_empty()),
            "missing {stage} for {id}; {}",
            self.evidence()
        );
    }
    pub fn assert_receipt_order(&self, id: &str) {
        let timeline = self.timeline();
        for commit in self.reports("committed", id) {
            for attempt in commit["committed"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["id"] == id)
            {
                let same = |r: &Value| {
                    r["id"] == attempt["id"] && r["a"] == attempt["a"] && r["e"] == attempt["e"]
                };
                let possession = timeline
                    .iter()
                    .position(|v| {
                        v["possess"]["held"]
                            .as_array()
                            .is_some_and(|a| a.iter().any(&same))
                    })
                    .expect("attempt possessed before commit");
                let handed = timeline
                    .iter()
                    .position(|v| v["handed"].as_array().is_some_and(|a| a.iter().any(&same)))
                    .expect("attempt handed before commit");
                let committed = timeline.iter().position(|v| v == &commit).unwrap();
                assert!(
                    possession < handed && handed < committed,
                    "possession -> handed -> committed: {timeline:?}"
                );
            }
        }
        assert!(
            !self.reports("committed", id).is_empty(),
            "commit report for {id}"
        );
    }
    pub fn hold_tool(&self) {
        self.prompt(&format!("Run this exact bash command:\n```bash\nprintf started > '{0}/held-start'; deadline=$((SECONDS+180)); while [ ! -f '{0}/held-release' ] && [ $SECONDS -lt $deadline ]; do sleep 0.1; done\n```\nAfter it returns, reply HELD-DONE and end your turn.",self.work.display()));
        common::wait_for_file_trimmed_eq(&self.work.join("held-start"), "started", BUDGET).unwrap();
        assert!(
            common::wait_until(BUDGET, || self
                .events()
                .iter()
                .any(|e| e["type"] == "tool_execution_start")),
            "real tool must start; {}",
            self.evidence()
        );
        assert!(
            common::wait_until(BUDGET, || self.record()["live"]["status"] == "Working"),
            "real Pi card must be Working; {}",
            self.evidence()
        );
    }
    pub fn release_tool(&self) {
        std::fs::write(self.work.join("held-release"), "").unwrap();
    }
    pub fn append_task(&self, name: &str) -> String {
        self.delegate_body(&format!("Use bash to run exactly: printf '{CONTENT}' >> '{}'. Reply APPENDED-{name} and end your turn.",self.work.join(name).display()))
    }
    pub fn record(&self) -> Value {
        inbox::attach(&self.deck, json!({"op":"list-agents"}))["agent_records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == self.pane.agent)
            .cloned()
            .unwrap_or(Value::Null)
    }
    pub fn screen(&self) -> String {
        let r = self.record();
        let mut parser = dot_agent_deck::vt_grid::parser(
            r["rows"].as_u64().unwrap_or(40) as u16,
            r["cols"].as_u64().unwrap_or(140) as u16,
            0,
        );
        parser.process(&common::pane_snapshot_on(
            self.deck.attach_socket_path(),
            &self.pane.agent,
        ));
        parser.screen().contents()
    }
    pub fn observe_interval(&self, span: Duration, mut check: impl FnMut()) {
        let initial = self.samples().len();
        let until = std::time::Instant::now() + span;
        while std::time::Instant::now() < until {
            check();
            std::thread::sleep(Duration::from_millis(100));
        }
        let samples = self.samples();
        assert!(
            samples.len() > initial,
            "sample worker must advance; {}",
            self.evidence()
        );
        assert!(
            samples.iter().all(|s| s["error"].is_null()),
            "sample worker errors: {samples:?}"
        );
        assert!(
            self.timeline()
                .iter()
                .all(|event| event["direction"] != "error"),
            "CLI observer must remain healthy; {}",
            self.evidence()
        );
        assert!(
            !self.observer.as_ref().unwrap().is_finished(),
            "sample worker ended early"
        );
        self.save_diagnostics();
    }
    pub fn assert_authless(&self) {
        // Pi 0.84.4's FileAuthStorage.ensureFileExists creates {} at boot.
        // Before exec the constructor asserted absence; after boot require
        // this empty container, without logging credential names or values.
        if let Ok(bytes) = std::fs::read(self.agent_dir.join("auth.json")) {
            let auth: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(
                auth.as_object().is_some_and(|object| object.is_empty()),
                "Pi's scratch credential store must remain empty"
            );
        }
        #[cfg(target_os = "linux")]
        {
            let environment = std::fs::read(format!("/proc/{}/environ", self.pid())).unwrap();
            for entry in environment.split(|b| *b == 0).filter(|v| !v.is_empty()) {
                let key = std::str::from_utf8(entry.split(|b| *b == b'=').next().unwrap()).unwrap();
                assert!(
                    AUTHLESS_ENV.contains(&key),
                    "unexpected child environment NAME: {key}"
                );
            }
        }
    }
    fn save_diagnostics(&self) {
        for (name, text) in [
            ("deck.txt", self.deck.snapshot_grid()),
            ("deck.log", self.log()),
            (
                "inbox.json",
                inbox::listing(&self.deck, &self.pane).to_string(),
            ),
            (
                "daemon-events.json",
                serde_json::to_string(&self.daemon_events.snapshot()).unwrap(),
            ),
        ] {
            let _ = std::fs::write(
                self.diagnostics.join(name),
                common::redact_credentials_for_output(&text),
            );
        }
        let _ = std::fs::write(
            self.diagnostics.join("pane.txt"),
            common::redact_credentials_for_output(&self.screen()),
        );
    }
}

impl Drop for Pi {
    fn drop(&mut self) {
        self.observer_stop.store(true, Ordering::Relaxed);
        if let Some(observer) = self.observer.take() {
            let _ = observer.join();
        }
        self.save_diagnostics();
    }
}

pub fn diagnostics_dir() -> PathBuf {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".dot-agent-deck/pi-live-runs")
        .join(format!(
            "{}-{}-{}",
            std::thread::current().name().unwrap_or("pi"),
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
    std::fs::create_dir_all(&path).unwrap();
    path
}
pub fn read_jsonl(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
        .map(|line| {
            serde_json::from_str(line).expect("complete observer record must be valid JSON")
        })
        .collect()
}

pub fn write_cli_observer(directory: &std::path::Path) -> PathBuf {
    let path = directory.join("observe-cli.py");
    std::fs::write(
        directory.join("observer-config.json"),
        json!({"real":common::contained_deck_binary(),"timeline":directory.join("timeline.jsonl")})
            .to_string(),
    )
    .unwrap();
    std::fs::write(&path, include_str!("../fixtures/pi-inbox/observe-cli.py")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}
