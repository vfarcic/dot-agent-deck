//! Real OpenCode, scratch configuration and genuine CLI delegation.
#![allow(dead_code)]

use crate::{common, inbox};
#[path = "../opencode_composer.rs"]
mod composer;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub const BUDGET: Duration = Duration::from_secs(120);
pub const CONTENT: &str = "OPENCODE_INBOX_OK";

pub struct OpenCode {
    pub pane: inbox::Pane,
    pub deck: common::TuiDeck,
    pub work: PathBuf,
    next: std::cell::Cell<usize>,
    diagnostics: PathBuf,
    observer_stop: Arc<AtomicBool>,
    observer: Option<std::thread::JoinHandle<()>>,
}

pub fn available() -> Result<(), String> {
    common::install_credential_redaction();
    let result = Command::new("opencode")
        .arg("--version")
        .current_dir(std::env::temp_dir())
        .output()
        .map_err(|e| format!("OpenCode unavailable: {e}"))?;
    if result.status.success() {
        Ok(())
    } else {
        Err("OpenCode --version failed".into())
    }
}

impl OpenCode {
    pub fn new(mini: bool, older: usize, watcher: bool) -> Self {
        Self::with_clear(mini, older, watcher, false, false)
    }

    pub fn with_clear(
        mini: bool,
        older: usize,
        watcher: bool,
        clear: bool,
        launcher: bool,
    ) -> Self {
        let deck = common::TuiDeck::builder()
            .with_pty_size(180, 50)
            .with_imported_opencode_credentials()
            .with_env("DOT_AGENT_DECK_LOG", "opencode-inbox-live.log")
            .with_env("DOT_AGENT_DECK_INBOX_ARM_GRACE_MS", "5000")
            .with_env("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000")
            .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "600")
            .launch_with_fixture("minimal");
        assert!(deck.wait_for_grid_string_within("No active agents", BUDGET));
        let work = deck.workdir().join("project");
        std::fs::create_dir_all(&work).unwrap();
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
        let mut install = Self::command(&deck, "dot-agent-deck");
        install.args(["hooks", "install", "--agent", "opencode"]);
        let output = install.output().unwrap();
        assert!(output.status.success(), "scratch install: {output:?}");
        let root = deck.home_dir().join(".config/opencode");
        if !watcher {
            std::fs::write(root.join("tui.json"), "{}").unwrap();
        }
        // Observe the actual route and expose navigation for the child-view
        // precondition. This fixture neither delivers nor acknowledges tasks.
        let observer = root.join("test-route-observer.js");
        std::fs::write(&observer, r#"import {readFileSync,writeFileSync,renameSync,unlinkSync} from 'node:fs';
export default {id:'deck-test-route',tui:async(api)=>{
 const base=process.env.DECK_TEST_ROUTE;
 const timer=setInterval(()=>{try{
  writeFileSync(base+'.tmp',JSON.stringify({route:api.route.current,pid:process.pid}));renameSync(base+'.tmp',base);
  try {const id=readFileSync(base+'.navigate','utf8');unlinkSync(base+'.navigate');api.route.navigate('session',{sessionID:id});}catch{}
 }catch{}},100);
 api.lifecycle.onDispose(()=>clearInterval(timer));
}};
"#).unwrap();
        let config_path = root.join("tui.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        if config["plugin"].is_null() {
            config["plugin"] = json!([]);
        }
        config["plugin"]
            .as_array_mut()
            .unwrap()
            .push(json!(observer));
        std::fs::write(config_path, config.to_string()).unwrap();
        for _ in 0..older {
            let output = Self::command(&deck, "timeout")
                .args([
                    "120",
                    "opencode",
                    "run",
                    "--model",
                    common::opencode_test_model(),
                    "Reply with only the number equal to 800 plus 31. Do not use tools.",
                ])
                .output()
                .unwrap();
            assert!(
                output.status.success() && String::from_utf8_lossy(&output.stdout).contains("831"),
                "scratch prior session: {output:?}"
            );
        }
        let command = format!(
            "opencode --model {} --auto {}",
            common::opencode_test_model(),
            if mini { "--mini" } else { "" }
        );
        let command = if launcher {
            let script = work.join("run-worker.sh");
            std::fs::write(&script, format!("#!/bin/sh\nexec {command}\n")).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
            "./run-worker.sh".to_owned()
        } else {
            command
        };
        // A clear delegate respawns from this role command, rather than the
        // initial attach request. Capture the replacement's own capability too.
        let role_command = common::capability_export_command(&format!("exec {command}"));
        std::fs::write(work.join(".dot-agent-deck.toml"),format!(
            "[[orchestrations]]\nname = \"opencode-inbox\"\n[[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"./delegate-loop.sh\"\nstart = true\n[[orchestrations.roles]]\nname = \"coder\"\ncommand = {role_command:?}\nagent = \"opencode\"\nclear = {clear}\n"
        )).unwrap();
        let script = work.join("delegate-loop.sh");
        std::fs::write(&script, "#!/bin/sh\ni=1\nwhile :; do\n while [ ! -f \"delegate-$i.go\" ]; do sleep 0.1; done\n dot-agent-deck delegate --to coder --task-file \"delegate-$i.md\" > \"delegate-$i.log\" 2>&1\n printf '%s' \"$?\" > \"delegate-$i.exit\"\n i=$((i+1))\ndone\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The deck and both roles inherit scratch roots, never the host config.
        let extra = json!([
            ["PATH", path],
            ["XDG_CONFIG_HOME", deck.home_dir().join(".config")],
            ["XDG_DATA_HOME", deck.home_dir().join(".local/share")],
            ["XDG_STATE_HOME", deck.home_dir().join(".local/state")],
            ["XDG_CACHE_HOME", deck.home_dir().join(".cache")],
            [
                "DECK_TEST_ROUTE",
                deck.workdir().join("opencode-route.json")
            ]
        ]);
        // Start through the same public attach operation desktop clients use.
        let mut worker = json!({"op":"start-agent","command":common::capability_export_command(&format!("exec {command}")),"cwd":work,"agent_type":"opencode","rows":40,"cols":140,"env":extra,"display_name":"coder","tab_membership":{"kind":"orchestration","name":"opencode-inbox","role_name":"coder","role_index":1,"is_start_role":false,"orchestration_cwd":work,"orchestration_id":"opencode-inbox-test"}});
        worker["env"]
            .as_array_mut()
            .unwrap()
            .push(json!(["DOT_AGENT_DECK_PANE_ID", "opencode-inbox-coder"]));
        let reply = inbox::attach(&deck, worker);
        assert_eq!(reply["ok"], true, "worker spawn: {reply}");
        let agent = reply["id"].as_str().unwrap().to_owned();
        let token = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(common::recorded_hook_capability(&work, &agent))
            .unwrap();
        let pane = inbox::Pane {
            id: "opencode-inbox-coder".into(),
            agent,
            token,
            pid: 0,
            capture: PathBuf::new(),
            attach_socket: deck.attach_socket_path().into(),
        };
        let reply = inbox::attach(
            &deck,
            json!({"op":"start-agent","command":common::capability_export_command("exec ./delegate-loop.sh"),"cwd":work,"rows":40,"cols":140,"env":[["DOT_AGENT_DECK_PANE_ID","opencode-inbox-orchestrator"],["PATH",path]],"display_name":"orchestrator","tab_membership":{"kind":"orchestration","name":"opencode-inbox","role_name":"orchestrator","role_index":0,"is_start_role":true,"orchestration_cwd":work,"orchestration_id":"opencode-inbox-test"}}),
        );
        assert_eq!(reply["ok"], true, "orchestrator spawn: {reply}");
        let diagnostics = std::env::var_os("DOT_AGENT_DECK_OPENCODE_DIAGNOSTICS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".dot-agent-deck/opencode-live-runs")
            })
            .join(format!(
                "{}-{}-{}",
                std::thread::current().name().unwrap_or("opencode"),
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap()
            ));
        std::fs::create_dir_all(&diagnostics).unwrap();
        let observer_stop = Arc::new(AtomicBool::new(false));
        let stop = observer_stop.clone();
        let socket = deck.hook_socket_path().to_owned();
        let request = inbox::request(&pane, "inbox_list");
        let observations = diagnostics.join("first-observed.json");
        let observer = std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};
            let mut first = json!({});
            while !stop.load(Ordering::Relaxed) {
                let sample = || -> Option<Value> {
                    let mut stream = std::os::unix::net::UnixStream::connect(&socket).ok()?;
                    stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
                    stream
                        .set_write_timeout(Some(Duration::from_secs(1)))
                        .ok()?;
                    writeln!(stream, "{request}").ok()?;
                    let mut line = String::new();
                    BufReader::new(stream).read_line(&mut line).ok()?;
                    serde_json::from_str(&line).ok()
                };
                if let Some(listing) = sample() {
                    for message in listing["messages"].as_array().into_iter().flatten() {
                        let Some(id) = message["id"].as_str() else {
                            continue;
                        };
                        let now = json!(chrono::Utc::now().to_rfc3339());
                        if first[id].is_null() {
                            first[id] = json!({"enqueue_observed": now});
                        }
                        if message["state"] == "acknowledged" && first[id]["ack_observed"].is_null()
                        {
                            first[id]["ack_observed"] = now;
                        }
                        first[id]["latest"] = message.clone();
                    }
                    let _ = std::fs::write(
                        &observations,
                        common::redact_credentials_for_output(&first.to_string()),
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let result = Self {
            pane,
            deck,
            work,
            next: std::cell::Cell::new(1),
            diagnostics,
            observer_stop,
            observer: Some(observer),
        };
        result.wait_screen("Ask anything");
        if watcher {
            assert!(
                common::wait_until(
                    BUDGET,
                    || inbox::listing(&result.deck, &result.pane)["armed"]
                        .as_u64()
                        .unwrap_or(0)
                        > 0
                ),
                "watcher must arm; {}",
                result.evidence()
            );
        } else {
            assert_eq!(inbox::listing(&result.deck, &result.pane)["armed"], 0);
        }
        // Attach the deck's own terminal view for the cast and user assertions.
        result.deck.send_bytes(b"\t");
        result.deck.wait_for_string("coder");
        result.deck.send_bytes(b"2");
        assert!(
            result
                .deck
                .wait_for_grid_string_within("Ask anything", BUDGET),
            "real OpenCode must be visible: {}",
            result.evidence()
        );
        result
    }

    fn command(deck: &common::TuiDeck, bin: &str) -> Command {
        let mut command = Command::new(bin);
        command
            .env_clear()
            .env("HOME", deck.home_dir())
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    deck.home_dir().join(".local/bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("XDG_CONFIG_HOME", deck.home_dir().join(".config"))
            .env("XDG_DATA_HOME", deck.home_dir().join(".local/share"))
            .env("XDG_STATE_HOME", deck.home_dir().join(".local/state"))
            .env("XDG_CACHE_HOME", deck.home_dir().join(".cache"))
            .env("DOT_AGENT_DECK_TEST_CONFIG_WRITE", "1")
            .env("DOT_AGENT_DECK_TEST_CONFIG_ROOT", deck.workdir())
            .current_dir(deck.workdir().join("project"));
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            command.env("ANTHROPIC_API_KEY", key);
        }
        command
    }
    pub fn screen(&self) -> String {
        let record = self.record();
        let mut parser = vt100::Parser::new(
            record["rows"].as_u64().unwrap() as u16,
            record["cols"].as_u64().unwrap() as u16,
            0,
        );
        parser.process(&common::pane_snapshot_on(
            self.deck.attach_socket_path(),
            &self.pane.agent,
        ));
        parser.screen().contents()
    }
    pub fn wait_screen(&self, needle: &str) {
        assert!(
            common::wait_until(BUDGET, || self.screen().contains(needle)),
            "OpenCode must show {needle:?}; {}",
            self.evidence()
        );
    }
    pub fn show_message(&self, id: &str) {
        let opener = format!("[dot-agent-deck message {id}");
        let input = std::cell::RefCell::new(self.input(""));
        let previous = std::cell::RefCell::new(String::new());
        assert!(
            common::wait_until(BUDGET, || {
                let shown = self.screen();
                if shown.contains(&opener) {
                    return true;
                }
                if shown != *previous.borrow() {
                    *previous.borrow_mut() = shown;
                    input.borrow_mut().write("\x1b[5~");
                }
                false
            }),
            "scrolling the actual session must reveal the framed message; {}",
            self.evidence()
        );
        assert!(
            self.deck.wait_for_grid_string_within(&opener, BUDGET),
            "framed message must be visible in the attached deck; {}",
            self.evidence()
        );
        input.borrow_mut().write("\x1b[F");
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
    pub fn log(&self) -> String {
        std::fs::read_to_string(self.deck.workdir().join("opencode-inbox-live.log"))
            .unwrap_or_default()
    }
    pub fn evidence(&self) -> String {
        self.save_diagnostics();
        format!(
            "Pane:\n{}\nGrid:\n{}\nInbox:{}\nLog:\n{}",
            self.screen(),
            self.deck.snapshot_grid(),
            inbox::listing(&self.deck, &self.pane),
            self.log()
        )
    }
    fn save_diagnostics(&self) {
        // Only scratch-session content and public terminal/log surfaces, never
        // configuration or auth files. Every persisted byte uses the harness redactor.
        let snapshot = self.diagnostics.join(format!(
            "snapshot-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::create_dir_all(&snapshot).unwrap();
        for (name, text) in [
            ("transcript.json", self.transcript().to_string()),
            ("pane.txt", self.screen()),
            ("deck.txt", self.deck.snapshot_grid()),
            ("deck.log", self.log()),
            ("inbox.json", inbox::listing(&self.deck, &self.pane).to_string()),
            ("capture.json", json!({"at": chrono::Utc::now().to_rfc3339(), "panicking": std::thread::panicking(), "record": self.record()}).to_string()),
        ] {
            let _ = std::fs::write(snapshot.join(name), common::redact_credentials_for_output(&text));
        }
    }
    pub fn input(&self, text: &str) -> common::AttachInput {
        let mut input =
            common::AttachInput::attach(self.deck.attach_socket_path(), &self.pane.agent);
        input.write(text);
        input
    }
    pub fn prompt(&self, text: &str) {
        let mut input = self.input(text);
        self.wait_screen(text);
        input.write("\r");
    }
    pub fn prior(&self) {
        self.prompt("Reply with only the number equal to 900 plus 27. Do not use tools.");
        self.wait_answered("900 plus 27", "927");
        self.wait_screen("927");
        self.idle();
        assert!(
            common::wait_until(BUDGET, || self.route()["route"]["params"]["sessionID"]
                .is_string()),
            "OpenCode must show the prompt's session; {}",
            self.evidence()
        );
    }
    pub fn interrupted_cancellable_wait(&self) -> Value {
        let before = self.log().len();
        let prompt = "Run sleep 30 once in the shell as a disposable, cancellable wait while I prepare your next task. This request lasts only until I interrupt you. An interrupt cancels the wait and completes this request: do not restart it, send a follow-up answer, or carry it into the next task. After interruption, handle the next instruction normally.";
        let mut priming_input = self.input(prompt);
        assert!(
            common::wait_until(BUDGET, || composer::composer_has_prompt(
                &self.screen(),
                prompt
            )),
            "complete cancellable prompt must be in the composer before Enter; {}",
            self.evidence()
        );
        std::fs::write(
            self.diagnostics.join("composer-ready.json"),
            common::redact_credentials_for_output(
                &json!({"at":chrono::Utc::now().to_rfc3339(),"pane":self.screen()}).to_string(),
            ),
        )
        .unwrap();
        priming_input.write("\r");
        assert!(
            common::wait_until(BUDGET, || {
                self.record()["live"]["status"] == "Thinking"
                    && self
                        .log()
                        .get(before..)
                        .is_some_and(|log| log.contains("event_type=Thinking"))
                    && self.route()["route"]["params"]["sessionID"].is_string()
            }),
            "prior prompt must genuinely start Thinking; {}",
            self.evidence()
        );
        let running_call = std::cell::RefCell::new(Value::Null);
        assert!(
            common::wait_until(BUDGET, || {
                let transcript = self.transcript();
                let call = transcript["part"].as_array().unwrap().iter().find(|p| {
                    p["data"]["type"] == "tool"
                        && p["data"]["tool"] == "bash"
                        && p["data"]["state"]["status"] == "running"
                        && p["data"]["state"]["input"]["command"] == "sleep 30"
                });
                let Some(call) = call else { return false };
                if !composer::sleep_tool_visible(&self.screen())
                    || !composer::sleep_tool_visible(&self.deck.snapshot_grid())
                {
                    return false;
                }
                *running_call.borrow_mut() = call.clone();
                true
            }),
            "cancellable bash sleep 30 must be running and visible in both views before interrupt; {}",
            self.evidence()
        );
        let running_call = running_call.into_inner();
        std::fs::write(
            self.diagnostics.join("priming.json"),
            common::redact_credentials_for_output(
                &json!({"at":chrono::Utc::now().to_rfc3339(),"call":running_call,
                    "pane":self.screen(),"deck":self.deck.snapshot_grid()})
                .to_string(),
            ),
        )
        .unwrap();
        // One logical interrupt using this agent's discovered sequence (OpenCode
        // currently requires two Esc presses). Never repeat the sequence until Idle.
        let record = self.record();
        let steps = record["prompt_keys"]["interrupt"]
            .as_array()
            .expect("agent interrupt sequence");
        assert!(!steps.is_empty());
        let mut input = self.input("");
        let mut sent = Vec::new();
        for step in steps {
            sent.push(chrono::Utc::now().to_rfc3339());
            input.write(step["bytes"].as_str().unwrap());
            std::thread::sleep(Duration::from_millis(
                step["pause_after_ms"].as_u64().unwrap_or(0),
            ));
        }
        std::fs::write(
            self.diagnostics.join("interrupt.json"),
            json!({"logical_interrupts":1,"key_sent_at":sent}).to_string(),
        )
        .unwrap();
        self.idle();
        assert!(
            self.log()
                .get(before..)
                .is_some_and(|log| log.contains("event_type=Idle")),
            "interrupted prompt must produce a real Idle hook; {}",
            self.evidence()
        );
        let empty_composer = || {
            let shown = self.screen();
            let lines = shown.lines().collect::<Vec<_>>();
            let Some(mode) = lines.iter().rposition(|line| line.contains("Build auto")) else {
                return false;
            };
            mode >= 2
                && lines[mode - 2..mode].iter().all(|line| {
                    line.trim_matches(|c: char| c.is_whitespace() || c == '┃')
                        .is_empty()
                })
                && !shown.contains("esc interrupt")
        };
        // The attached deck redraws its status even when the composer is empty;
        // settle the observable composer instead of requiring all bytes to stop.
        let empty_since = std::cell::Cell::new(None::<std::time::Instant>);
        assert!(
            common::wait_until(BUDGET, || {
                if !empty_composer() || self.record()["live"]["status"] != "Idle" {
                    empty_since.set(None);
                    return false;
                }
                let since = empty_since.get().unwrap_or_else(|| {
                    let now = std::time::Instant::now();
                    empty_since.set(Some(now));
                    now
                });
                since.elapsed() >= Duration::from_millis(250)
            }),
            "interrupted composer must stay settled and empty for 250ms after genuine Idle; {}",
            self.evidence()
        );
        std::fs::write(
            self.diagnostics.join("interrupted.json"),
            common::redact_credentials_for_output(
                &json!({"at":chrono::Utc::now().to_rfc3339(),"transcript":self.transcript(),
                    "pane":self.screen(),"deck":self.deck.snapshot_grid()})
                .to_string(),
            ),
        )
        .unwrap();
        running_call["id"].clone()
    }
    /// Wait until [`Self::answered`].
    pub fn wait_answered(&self, asked: &str, answer: &str) {
        assert!(
            common::wait_until(BUDGET, || self.answered(asked, answer)),
            "OpenCode must answer {answer:?} to the prompt asking {asked:?}; {}",
            self.evidence()
        );
    }
    /// Whether OpenCode's own transcript holds a completed reply containing
    /// `answer` to a user prompt containing `asked`. A short answer on the
    /// screen proves nothing on its own: OpenCode's footer shows the work
    /// directory, whose name carries the test process's pid, and inbox/opencode/015
    /// once found `927` there (`dad-tests-892758-…`) before its prompt had run.
    pub fn answered(&self, asked: &str, answer: &str) -> bool {
        let transcript = self.transcript();
        let (Some(messages), Some(parts)) = (
            transcript["message"].as_array(),
            transcript["part"].as_array(),
        ) else {
            return false;
        };
        let text_of = |id: &Value| {
            parts
                .iter()
                .filter(|p| p["message_id"] == *id && p["data"]["type"] == "text")
                .filter_map(|p| p["data"]["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        };
        messages
            .iter()
            .filter(|m| m["data"]["role"] == "user" && text_of(&m["id"]).contains(asked))
            .any(|user| {
                messages.iter().any(|m| {
                    m["data"]["role"] == "assistant"
                        && m["data"]["parentID"] == user["id"]
                        && m["data"]["finish"] == "stop"
                        && text_of(&m["id"]).contains(answer)
                })
            })
    }
    pub fn idle(&self) {
        assert!(
            common::wait_until(BUDGET, || self.record()["live"]["status"] == "Idle"),
            "agent must finish: {}",
            self.evidence()
        );
    }
    pub fn delegate(&self, name: &str) -> String {
        self.delegate_body(&format!("Use the shell to run exactly: printf '{CONTENT}' > '{}'. Then give a brief final reply and end your turn. Do this once for this delivery id.",self.work.join(name).display()))
    }
    pub fn delegate_execution(&self, name: &str, token_suffix: &str) -> String {
        self.delegate_body(&format!("Use the shell to run exactly: printf '{CONTENT}' > '{}'; printf '%s%s\\n' 'NATIVE-EXEC-' '{token_suffix}'. Then give a brief final reply and end your turn. Do this once for this delivery id.",self.work.join(name).display()))
    }
    pub fn visible_execution(&self, id: &str, name: &str, token: &str) {
        let frame = format!("[end of dot-agent-deck message {id}]");
        self.wait_screen(&frame);
        assert!(
            self.deck.wait_for_grid_string_within(&frame, BUDGET),
            "ID-correlated frame must be visible in attached deck; {}",
            self.evidence()
        );
        common::wait_for_file_trimmed_eq(&self.work.join(name), CONTENT, BUDGET)
            .unwrap_or_else(|why| panic!("real OpenCode must act: {why}; {}", self.evidence()));
        assert!(
            common::wait_until(BUDGET, || {
                let transcript = self.transcript();
                let messages = transcript["message"].as_array().unwrap();
                let parts = transcript["part"].as_array().unwrap();
                let user = messages.iter().find(|m| {
                    m["data"]["role"] == "user"
                        && parts.iter().any(|p| {
                            p["message_id"] == m["id"]
                                && p["data"]["text"]
                                    .as_str()
                                    .is_some_and(|text| text.contains(&frame))
                        })
                });
                let Some(user) = user else { return false };
                parts.iter().any(|p| {
                    p["data"]["type"] == "tool"
                        && p["data"]["state"]["status"] == "completed"
                        && p["data"]["state"]["output"]
                            .as_str()
                            .is_some_and(|output| output.contains(token))
                        && messages.iter().any(|m| {
                            m["id"] == p["message_id"] && m["data"]["parentID"] == user["id"]
                        })
                })
            }),
            "this delivery must assemble its token in a completed tool result; {}",
            self.evidence()
        );
        self.wait_screen(token);
        assert!(
            self.deck.wait_for_grid_string_within(token, BUDGET),
            "execution token must be visible in attached deck; {}",
            self.evidence()
        );
        inbox::state(&self.deck, &self.pane, id, "acknowledged");
        let listing = inbox::listing(&self.deck, &self.pane);
        let message = listing["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap();
        assert_eq!(message["ack_how"], "plugin_confirmed", "{message}");
        assert_eq!(message["deliveries"], 1, "{message}");
        assert!(
            self.deck.wait_for_grid_string_within("✉ delivered", BUDGET),
            "worker card must show delivered; {}",
            self.evidence()
        );
    }
    pub fn mini_not_busy_after_idle(&self) {
        self.idle();
        assert!(
            self.log().contains("event_type=Idle"),
            "mini must have a genuine Idle hook; {}",
            self.evidence()
        );
        assert!(
            common::wait_until(Duration::from_secs(10), || {
                !self.screen().contains("esc interrupt")
                    && !self.deck.snapshot_grid().contains("esc interrupt")
            }),
            "mini must stop showing its busy footer within 10s of genuine Idle; {}",
            self.evidence()
        );
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
        self.delegate_succeeded(n);
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
            "delegate must enqueue: {}",
            self.evidence()
        );
        id.into_inner().unwrap()
    }
    fn delegate_succeeded(&self, n: usize) {
        let path = self.work.join(format!("delegate-{n}.exit"));
        assert!(
            common::wait_until(BUDGET, || std::fs::read_to_string(&path)
                .is_ok_and(|s| !s.is_empty())),
            "delegate CLI must finish; {}",
            self.evidence()
        );
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "0",
            "delegate CLI failed: {:?}",
            std::fs::read_to_string(self.work.join(format!("delegate-{n}.log")))
        );
    }
    pub fn clear_delegate(&mut self, body: &str) -> String {
        let n = self.next.get();
        self.next.set(n + 1);
        std::fs::write(self.work.join(format!("delegate-{n}.md")), body).unwrap();
        let previous = self.pane.agent.clone();
        self.deck.send_bytes(b"\x04");
        self.deck.wait_for_string("[New Agent n/Ctrl+N]");
        std::fs::write(self.work.join(format!("delegate-{n}.go")), "").unwrap();
        self.delegate_succeeded(n);
        let records = inbox::attach(&self.deck, json!({"op":"list-agents"}));
        let record = records["agent_records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["pane_id_env"] == self.pane.id && r["id"] != previous)
            .unwrap_or_else(|| panic!("clear must replace worker: {records}"));
        self.pane.agent = record["id"].as_str().unwrap().to_owned();
        self.pane.token = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(common::recorded_hook_capability(
                &self.work,
                &self.pane.agent,
            ))
            .unwrap();
        let listing = inbox::listing(&self.deck, &self.pane);
        let id = listing["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["bind_agent"] == self.pane.agent)
            .unwrap_or_else(|| panic!("replacement inbox task: {listing}"))["id"]
            .as_str()
            .unwrap()
            .to_owned();
        self.deck.send_bytes(b"2");
        id
    }
    pub fn done(&self, id: &str, name: &str) {
        self.done_with_ack(id, name, "plugin_confirmed");
    }
    pub fn done_with_ack(&self, id: &str, name: &str, ack_how: &str) {
        common::wait_for_file_trimmed_eq(&self.work.join(name), CONTENT, BUDGET)
            .unwrap_or_else(|why| panic!("real OpenCode must act: {why}; {}", self.evidence()));
        self.wait_reply(id);
        inbox::state(&self.deck, &self.pane, id, "acknowledged");
        let listing = inbox::listing(&self.deck, &self.pane);
        let message = listing["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap();
        assert_eq!(message["ack_how"], ack_how, "{message}");
    }
    fn final_reply(&self, id: &str) -> Option<String> {
        let transcript = self.transcript();
        let messages = transcript["message"].as_array().unwrap();
        let parts = transcript["part"].as_array().unwrap();
        let opener = format!("[dot-agent-deck message {id}");
        let user = messages.iter().find(|m| {
            m["data"]["role"] == "user"
                && parts.iter().any(|p| {
                    p["message_id"] == m["id"]
                        && p["data"]["text"]
                            .as_str()
                            .is_some_and(|s| s.contains(&opener))
                })
        })?;
        let assistant = messages
            .iter()
            .filter(|m| {
                m["data"]["role"] == "assistant"
                    && m["data"]["parentID"] == user["id"]
                    && m["data"]["finish"] == "stop"
            })
            .max_by_key(|m| m["time_created"].as_u64())?;
        let text = parts
            .iter()
            .filter(|p| p["message_id"] == assistant["id"] && p["data"]["type"] == "text")
            .filter_map(|p| p["data"]["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        (!text.trim().is_empty()).then_some(text)
    }
    pub fn wait_reply(&self, id: &str) {
        // Derive the expected text from this turn's completed assistant, then
        // assert its rendered tail in both vt100 views. A model need not obey
        // a cosmetic DONE spelling to prove native delivery and a visible reply.
        let reply = std::cell::RefCell::new(None::<String>);
        let visible = std::cell::RefCell::new(String::new());
        assert!(
            common::wait_until(BUDGET, || {
                if reply.borrow().is_none() {
                    *reply.borrow_mut() = self.final_reply(id).map(|s| normalize(&s));
                }
                let expected = reply.borrow();
                let Some(expected) = expected.as_ref() else {
                    return false;
                };
                let tail = expected
                    .chars()
                    .rev()
                    .take(80)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<String>();
                let shown = self.screen();
                if !normalize(&shown).contains(&tail) {
                    return false;
                }
                *visible.borrow_mut() = shown
                    .lines()
                    .map(normalize)
                    .rfind(|line| line.len() >= 4 && expected.ends_with(line))
                    .unwrap_or_default();
                !visible.borrow().is_empty()
            }),
            "this delivery's completed assistant reply must be visible; {}",
            self.evidence()
        );
        assert!(
            self.deck
                .wait_for_grid_predicate_within(BUDGET, |grid| normalize(grid)
                    .contains(visible.borrow().as_str())),
            "reply must be visible in attached deck; {}",
            self.evidence()
        );
    }
    pub fn route(&self) -> Value {
        std::fs::read(self.deck.workdir().join("opencode-route.json"))
            .ok()
            .and_then(|s| serde_json::from_slice(&s).ok())
            .unwrap_or(Value::Null)
    }
    pub fn transcript(&self) -> Value {
        let script = r#"import json,sqlite3,sys
c=sqlite3.connect(sys.argv[1]);c.row_factory=sqlite3.Row
out={}
for table in ('session','message','part'):
 rows=[]
 for row in c.execute('select * from '+table):
  r=dict(row)
  if 'data' in r:r['data']=json.loads(r['data'])
  rows.append(r)
 out[table]=rows
print(json.dumps(out))
"#;
        let output = Command::new("python3")
            .args(["-c", script])
            .arg(
                self.deck
                    .home_dir()
                    .join(".local/share/opencode/opencode.db"),
            )
            .output()
            .unwrap();
        assert!(output.status.success(), "transcript: {output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }
    pub fn roots(&self) -> Vec<Value> {
        self.transcript()["session"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["parent_id"].is_null())
            .cloned()
            .collect()
    }
    pub fn turns(&self, sessions: &[Value]) -> Vec<Value> {
        let transcript = self.transcript();
        let mut turns = transcript["message"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| sessions.contains(&m["session_id"]))
            .map(|m| {
                json!({"id":m["id"], "session":m["session_id"], "role":m["data"]["role"],
                "text":transcript["part"].as_array().unwrap().iter()
                    .filter(|p|p["message_id"]==m["id"] && p["data"]["type"]=="text")
                    .map(|p|json!({"id":p["id"],"text":p["data"]["text"]})).collect::<Vec<_>>() })
            })
            .collect::<Vec<_>>();
        turns.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        turns
    }
    pub fn navigate(&self, id: &str) {
        std::fs::write(self.deck.workdir().join("opencode-route.json.navigate"), id).unwrap();
        assert!(
            common::wait_until(BUDGET, || self.route()["route"]["params"]["sessionID"]
                == id),
            "actual OpenCode route must navigate; {}",
            self.evidence()
        );
    }
    pub fn watcher_pids(&self) -> Vec<i32> {
        let Some(pid) = self.route()["pid"].as_u64() else {
            return vec![];
        };
        let mut found = Vec::new();
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            return found;
        };
        for task in tasks.flatten() {
            for child in std::fs::read_to_string(task.path().join("children"))
                .unwrap_or_default()
                .split_whitespace()
            {
                let cmd = std::fs::read(format!("/proc/{child}/cmdline")).unwrap_or_default();
                // PRD #1666 §4: the server context's `--doorbell-only`
                // receiver is not the pane's delivery watcher.
                if cmd.split(|b| *b == 0).any(|arg| arg == b"inbox")
                    && cmd.split(|b| *b == 0).any(|arg| arg == b"wait")
                    && !cmd.split(|b| *b == 0).any(|arg| arg == b"--doorbell-only")
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

impl Drop for OpenCode {
    fn drop(&mut self) {
        self.observer_stop.store(true, Ordering::Relaxed);
        if let Some(observer) = self.observer.take() {
            let _ = observer.join();
        }
        // Do not let a diagnostic failure cause a second panic during unwinding.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.save_diagnostics()));
        eprintln!("OpenCode run diagnostics: {}", self.diagnostics.display());
    }
}

fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}
