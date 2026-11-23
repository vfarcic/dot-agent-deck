#![cfg(all(feature = "e2e", feature = "e2e-live"))]
#![cfg(unix)]

//! Real-agent conversation restore coverage. Lane 2 only, never CI/RWX.
//! Real interactive agents, no print/exec mode and no prompt in saved argv.
//! Both daemon lifetimes share a scratch HOME as well as a journal state dir:
//! otherwise the resumed agent cannot find its persisted conversation.

dot_agent_deck_test_harness::link!();
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/work_journal.rs"]
mod journal;

use serde_json::{Value, json};
use spec::spec;
use std::cell::RefCell;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";
// An independent literal: a wording regression must not change the oracle.
const CONTINUE_NOTE: &str = "The deck restarted and resumed this session. If your previous turn was cut off before it finished, first check the current state — `git status`, `git log`, whether anything was already pushed, opened or sent — then continue from where you stopped without repeating what already happened. If your previous turn had finished, do nothing and wait for the next message.";
// One budget for the whole test, and every per-step wait below is the
// smaller of its own bound and what is left of it, so a slow run that is
// still inside each step's bound fails with that step's diagnostics rather
// than being killed by nextest (PR #2497 review). The arithmetic:
// `.config/nextest.toml` ends `journal_restore_01[67]_` at 14 x 60 s = 840 s;
// each fixture daemon's lifetime cap is 900 s
// (`DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS`, below CLAUDE.md rule 14's
// `DEAD_PID_MIN_AGE`), and the first daemon starts with the test. The budget
// is 780 s, which leaves 60 s under nextest's limit for the waits that do not
// go through it (the harness's own boot and prompt-submission bounds, the
// scoped teardown). Without the budget the steps alone could take five turns
// of 240 s, a session flush of 75 s and several controls of 90 s each.
const TEST_BUDGET: Duration = Duration::from_secs(780);
const TURN: Duration = Duration::from_secs(240);
const SESSION_FLUSH: Duration = Duration::from_secs(75);
const CONTROL: Duration = Duration::from_secs(90);

thread_local! {
    /// When this thread's test runs out of [`TEST_BUDGET`].
    static DEADLINE: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// `step`, or what is left of the test's budget when that is less (never
/// zero, so a wait still looks once).
fn within(step: Duration) -> Duration {
    DEADLINE.with(|deadline| match deadline.get() {
        Some(deadline) => step
            .min(deadline.saturating_duration_since(Instant::now()))
            .max(Duration::from_millis(1)),
        None => step,
    })
}

/// A real agent's turn.
fn turn() -> Duration {
    within(TURN)
}

/// The journal's flush of a session report.
fn session_flush() -> Duration {
    within(SESSION_FLUSH)
}

/// A control-plane step: the CLI, the TUI's grid, the daemon's records.
fn control() -> Duration {
    within(CONTROL)
}

#[derive(Clone, Copy)]
enum Agent {
    Claude,
    Codex,
}

impl Agent {
    fn wire(self) -> &'static str {
        match self {
            Self::Claude => "claude_code",
            Self::Codex => "codex",
        }
    }

    fn command(self) -> String {
        // Keep the executable reachable through PATH so the restore shim sees
        // the invocation. In particular, never replace it with an absolute path.
        match self {
            Self::Claude => format!(
                "exec claude --ax-screen-reader --model {HAIKU_MODEL} --allowedTools Bash Read Write"
            ),
            Self::Codex => format!(
                "exec codex --model {} --sandbox workspace-write --ask-for-approval never -c 'model_reasoning_effort=\"low\"'",
                quote(common::codex_test_model())
            ),
        }
    }
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn string(value: &Value, field: &str) -> String {
    value[field]
        .as_str()
        .unwrap_or_else(|| panic!("missing {field}: {value}"))
        .to_string()
}

/// Bound hook installation and the human CLI too, not just observation loops.
/// Output goes to files so a full pipe cannot deadlock the child.
fn bounded(mut command: Command, budget: Duration) -> Output {
    let stdout = common::harness_tempfile().unwrap();
    let stderr = common::harness_tempfile().unwrap();
    command
        .stdin(Stdio::null())
        .stdout(stdout.reopen().unwrap())
        .stderr(stderr.reopen().unwrap());
    let mut child = command.spawn().expect("spawn fixture CLI");
    let deadline = Instant::now() + budget;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            // Signal only the exact child this call owns, then reap it.
            let _ = child.kill();
            let _ = child.wait();
            panic!("fixture CLI exceeded {budget:?}");
        }
        // Bounded polling via the shared harness (no sleep in test bodies).
        common::wait_until(Duration::from_millis(25), || false);
    };
    Output {
        status,
        stdout: std::fs::read(stdout.path()).unwrap(),
        stderr: std::fs::read(stderr.path()).unwrap(),
    }
}

fn succeeded(output: Output) -> Output {
    assert!(
        output.status.success(),
        "CLI failed ({:?}): stdout={} stderr={}",
        output.status.code(),
        common::redact_credentials_for_output(&String::from_utf8_lossy(&output.stdout)),
        common::redact_credentials_for_output(&String::from_utf8_lossy(&output.stderr))
    );
    output
}

struct LiveRestore {
    live: common::DaemonProc,
    gone: Vec<common::DaemonProc>,
    home: PathBuf,
    state: PathBuf,
    repo: PathBuf,
    path: String,
    name: String,
    /// Each daemon lifetime keeps its own log here (`deck-<n>.log`), so a
    /// failed restore can say why the daemon refused it.
    logs: Vec<PathBuf>,
    _scratch: tempfile::TempDir,
}

impl LiveRestore {
    fn new(agent: Agent, name: &str) -> Self {
        let scratch = common::harness_tempdir().unwrap();
        let home = scratch.path().join("home");
        let repo = scratch.path().join("repo");
        let state = scratch.path().join("state");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        journal::init_repo(&repo);
        std::fs::write(repo.join(".git/info/exclude"), "ack-*.txt\nrecall-*.txt\n").unwrap();
        match agent {
            Agent::Claude => {
                let mut trust = vec![repo.to_string_lossy().into_owned()];
                let canonical = repo.canonicalize().unwrap().to_string_lossy().into_owned();
                if !trust.contains(&canonical) {
                    trust.push(canonical);
                }
                // Imports credentials, seeds onboarding, per-folder trust and
                // API-key approval together (same helper as interrupted/028).
                common::seed_claude_worker_home(&home, &trust).unwrap();
            }
            Agent::Codex => {
                common::import_codex_credentials(&home).unwrap();
                let config = home.join(".codex/config.toml");
                let mut parsed: toml::Value =
                    toml::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
                for path in [&repo, &repo.canonicalize().unwrap()] {
                    parsed["projects"]
                        .as_table_mut()
                        .unwrap()
                        .insert(path.to_string_lossy().into_owned(), project_trust());
                }
                std::fs::write(config, toml::to_string(&parsed).unwrap()).unwrap();
            }
        }
        // DaemonProc seeds its own private HOME, so explicitly seed the stable
        // override too. Codex's hook commands need the durable contained binary.
        let installed = home.join(".local/bin/dot-agent-deck");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        if std::fs::hard_link(common::contained_deck_binary(), &installed).is_err() {
            std::fs::copy(common::contained_deck_binary(), &installed).unwrap();
        }
        let path = format!(
            "{}:{}",
            installed.parent().unwrap().display(),
            common::path_with_binary_dir()
        );
        let first_log = scratch.path().join("deck-1.log");
        let live = spawn(&home, &state, &path, &first_log);
        let fixture = Self {
            live,
            gone: Vec::new(),
            home,
            state,
            repo,
            path,
            name: name.to_string(),
            logs: vec![first_log],
            _scratch: scratch,
        };
        // This starts a standalone agent; there is no project orchestration
        // for the deck's `trust approve` command to approve.
        let hook_agent = match agent {
            Agent::Claude => "claude-code",
            Agent::Codex => "codex",
        };
        // The real install command completes Codex's app-server migration and
        // native-hook trust probe before an interactive Codex uses this HOME.
        // Modelled on support/codex_inbox_live::prepare, with no TuiDeck HOME.
        let deadline = Instant::now() + control();
        loop {
            let mut install = fixture.command();
            install.args(["hooks", "install", "--agent", hook_agent]);
            let output = bounded(install, deadline.saturating_duration_since(Instant::now()));
            if let Agent::Claude = agent {
                succeeded(output);
                break;
            }
            let config = std::fs::read_to_string(fixture.home.join(".codex/config.toml"))
                .ok()
                .and_then(|text| toml::from_str::<toml::Value>(&text).ok());
            let trusted = config
                .as_ref()
                .and_then(|config| config.get("hooks"))
                .and_then(|hooks| hooks.get("state"))
                .and_then(toml::Value::as_table)
                .is_some_and(|hooks| {
                    ["session_start", "user_prompt_submit", "stop"]
                        .iter()
                        .all(|event| {
                            hooks.iter().any(|(key, value)| {
                                key.contains(&format!(":{event}:"))
                                    && value.get("trusted_hash").is_some()
                            })
                        })
                });
            if output.status.success() && trusted {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Codex hook migration/trust never finished: {}",
                common::redact_credentials_for_output(&String::from_utf8_lossy(&output.stderr))
            );
            common::wait_until(Duration::from_millis(100), || false);
        }
        fixture
    }

    fn command(&self) -> Command {
        let mut command = Command::new(common::contained_deck_binary());
        command
            .env_clear()
            .envs(common::config_containment_env())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("PATH", &self.path)
            .env("DOT_AGENT_DECK_STATE_DIR", &self.state)
            .env("DOT_AGENT_DECK_SOCKET", &self.live.hook_socket)
            .env("DOT_AGENT_DECK_ATTACH_SOCKET", &self.live.attach_socket)
            .env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
            .current_dir(&self.repo);
        command
    }

    fn tui(&self, lifetime: &str) -> common::TuiDeck {
        // This client does not spawn the agent; the stable daemon HOME owns
        // credentials/session files. Separate absent saved-layout paths keep
        // workspace replay (018) out of this conversation-restore test.
        common::TuiDeck::builder()
            .with_pty_size(180, 50)
            .with_env(
                "DOT_AGENT_DECK_ATTACH_SOCKET",
                self.live.attach_socket.to_string_lossy(),
            )
            .with_env(
                "DOT_AGENT_DECK_SOCKET",
                self.live.hook_socket.to_string_lossy(),
            )
            .with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
            .with_env(
                "DOT_AGENT_DECK_SESSION",
                self.state.join(lifetime).to_string_lossy(),
            )
            .without_agent_credentials()
            .launch_with_fixture("minimal")
    }

    fn start(&self, agent: Agent) -> String {
        eprintln!("restore fixture launch argv: {}", agent.command());
        let reply = inbox::attach(
            &self.live,
            json!({
                "op":"start-agent", "command":agent.command(), "cwd":self.repo,
                "display_name":self.name, "agent_type":agent.wire(), "rows":45,"cols":175,
                "env":[["DOT_AGENT_DECK_PANE_ID", self.name]]
            }),
        );
        assert_eq!(reply["ok"], true, "start: {reply}");
        string(&reply, "id")
    }

    fn restart(&mut self, attached: common::TuiDeck) {
        // SAFETY: only the daemon pid spawned and still owned by this fixture.
        assert_eq!(
            unsafe { libc::kill(self.live.pid() as libc::pid_t, libc::SIGTERM) },
            0
        );
        assert!(
            self.live.wait_for_exit(Duration::from_secs(30)),
            "daemon must stop"
        );
        // Close the old client before replacement so its reconnect machinery
        // cannot start another daemon on the now-dead fixture endpoints.
        drop(attached);
        let log = self.logs[0].with_file_name(format!("deck-{}.log", self.logs.len() + 1));
        let next = spawn(&self.home, &self.state, &self.path, &log);
        self.logs.push(log);
        self.gone.push(std::mem::replace(&mut self.live, next));
    }

    fn attempt(&self, agent: &str, except: Option<&str>) -> Value {
        journal::wait_for(&self.state, "this generation's attempt", |lines| {
            lines
                .iter()
                .find(|line| {
                    line["kind"] == "attempt_started"
                        && line["agent"].as_u64().map(|id| id.to_string()).as_deref() == Some(agent)
                        && except.is_none_or(|old| line["attempt"] != old)
                })
                .cloned()
        })
    }

    fn session(&self, attempt: &Value) -> String {
        let found = RefCell::new(None);
        assert!(
            common::wait_until(session_flush(), || {
                *found.borrow_mut() = journal::lines(&self.state)
                    .iter()
                    .filter(|line| {
                        line["kind"] == "attempt_session"
                            && line["attempt"] == attempt["attempt"]
                            && line["run"] == attempt["run"]
                    })
                    .max_by_key(|line| line["seq"].as_u64().unwrap_or(0))
                    .and_then(|line| line["session"].as_str().map(str::to_string));
                found.borrow().is_some()
            }),
            "attested session never flushed for {attempt}: {}",
            journal::dump(&self.state)
        );
        found.into_inner().unwrap()
    }

    fn records(&self) -> Vec<Value> {
        let reply = inbox::attach(&self.live, json!({"op":"list-agents"}));
        assert_eq!(reply["ok"], true, "list-agents: {reply}");
        reply["agent_records"]
            .as_array()
            .expect("agent records")
            .clone()
    }

    fn item(&self, attempt: &str) -> Value {
        let mut cli = self.command();
        cli.args(["interrupted", "list", "--json", "--no-git"]);
        let output = succeeded(bounded(cli, control()));
        let list: Value = serde_json::from_slice(&output.stdout).unwrap();
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| {
                item["agents"]
                    .as_array()
                    .is_some_and(|agents| agents.iter().any(|a| a["attempt"] == attempt))
            })
            .unwrap_or_else(|| panic!("attempt {attempt} missing: {list}"))
            .clone()
    }

    fn restored(&self) -> Value {
        let found = RefCell::new(None);
        assert!(
            common::wait_until(control(), || {
                let records = self.records();
                assert!(
                    records.len() <= 1,
                    "restore must not start duplicates: {records:?}"
                );
                *found.borrow_mut() = records
                    .into_iter()
                    .find(|record| record["display_name"] == self.name);
                found.borrow().is_some()
            }),
            "restored named agent never appeared\n{}",
            self.restore_diagnostics()
        );
        found.into_inner().unwrap()
    }

    fn wait_idle(&self, id: &str, attached: &common::TuiDeck) {
        assert!(
            common::wait_until(turn(), || self
                .records()
                .iter()
                .any(|record| { record["id"] == id && record["live"]["status"] == "Idle" })),
            "agent {id} did not finish its turn\nrecords: {:?}\njournal: {}\npane:\n{}",
            self.records(),
            journal::dump(&self.state),
            attached.snapshot_grid()
        );
    }

    fn first_restart_note_received(&self, restored: &Value, attached: &common::TuiDeck) {
        let id = string(restored, "id");
        let pane = string(restored, "pane_id_env");
        assert!(
            common::wait_until(turn(), || {
                let reply = inbox::attach(
                    &self.live,
                    json!({"op":"list-inbox", "pane_id":pane, "bodies":true}),
                );
                assert_eq!(reply["ok"], true, "list-inbox: {reply}");
                let messages: Vec<_> = reply["inbox_messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|message| message["bind_agent"] == id)
                    .collect();
                if messages.is_empty() {
                    return false;
                }
                assert_eq!(
                    messages.len(),
                    1,
                    "restart note must be first and sole mail before recall: {messages:?}"
                );
                let first = messages[0];
                assert_eq!(
                    first["from"],
                    json!({"type":"deck", "reason":"restart note"})
                );
                assert!(
                    first["text"]
                        .as_str()
                        .is_some_and(|body| body.contains(&format!("\n{CONTINUE_NOTE}\n"))),
                    "wrong first prompt: {first}"
                );
                first["state"] == "acknowledged"
            }),
            "restored interactive agent never received its first restart note\njournal: {}\npane:\n{}",
            journal::dump(&self.state),
            attached.snapshot_grid()
        );
        // Receipt alone proves delivery; wait for the genuine agent's turn to
        // settle too, before the human sends the recall question.
        self.wait_idle(&id, attached);
    }
}

impl LiveRestore {
    /// Why a restore was refused, redacted for test output: every daemon
    /// lifetime's `restore:` log lines and the journal's `restore_failed`
    /// lines, whose `failure` and `shim` name the cause.
    fn restore_diagnostics(&self) -> String {
        let mut out = String::new();
        for log in &self.logs {
            let text = std::fs::read_to_string(log)
                .unwrap_or_else(|error| format!("<unreadable: {error}>"));
            out.push_str(&format!("daemon log {}:\n", log.display()));
            for line in text.lines().filter(|line| line.contains("restore:")) {
                out.push_str(&format!("  {line}\n"));
            }
        }
        out.push_str("journal restore_failed lines:\n");
        for line in journal::lines(&self.state)
            .iter()
            .filter(|line| line["kind"] == "restore_failed")
        {
            out.push_str(&format!(
                "  failure={} shim={} line={line}\n",
                line["failure"], line["shim"]
            ));
        }
        common::redact_credentials_for_output(&out)
    }
}

fn project_trust() -> toml::Value {
    let mut table = toml::map::Map::new();
    table.insert("trust_level".into(), toml::Value::String("trusted".into()));
    toml::Value::Table(table)
}

fn spawn(home: &Path, state: &Path, path: &str, log: &Path) -> common::DaemonProc {
    let codex = home.join(".codex");
    let config = home.join(".config");
    let mut env = vec![
        ("HOME", home.to_str().unwrap()),
        ("XDG_CONFIG_HOME", config.to_str().unwrap()),
        // Use Claude's HOME defaults, matching seed_claude_worker_home's
        // .claude.json onboarding/trust and .claude credential locations.
        ("CODEX_HOME", codex.to_str().unwrap()),
        ("DOT_AGENT_DECK_STATE_DIR", state.to_str().unwrap()),
        ("DOT_AGENT_DECK_EXPERIMENTAL", "1"),
        ("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "900"),
        ("DOT_AGENT_DECK_LOG", log.to_str().unwrap()),
        ("PATH", path),
    ];
    let key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty());
    if let Some(key) = key.as_deref() {
        env.push(("ANTHROPIC_API_KEY", key));
    }
    inbox::daemon(&env) // Bind inbox normally: never daemon_unbound for a real agent.
}

fn grid(deck: &common::TuiDeck, what: &str, predicate: impl Fn(&str) -> bool) {
    assert!(
        deck.wait_for_grid_predicate_within(control(), |frame| predicate(frame)),
        "{what}:\n{}",
        deck.snapshot_grid()
    );
}

fn command_mode(deck: &common::TuiDeck) {
    if deck.snapshot_grid().contains("[Command Mode Ctrl+D]") {
        deck.send_keys(b"\x04");
    }
    grid(deck, "command mode", |frame| {
        !frame.contains("[Command Mode Ctrl+D]")
    });
}

fn focus(deck: &common::TuiDeck, agent: Agent) {
    command_mode(deck);
    deck.send_keys(b"1"); // The fixture owns exactly one dashboard pane.
    grid(deck, "focus the restored interactive agent", |frame| {
        frame.contains("[Command Mode Ctrl+D]")
    });
    grid(deck, "interactive composer", |frame| match agent {
        // Screen-reader Claude renders its composer as a pane line that is
        // exactly `$`. The startup banner is not required: a resumed
        // conversation replays its transcript and the banner is not on
        // screen, while the bare `$` composer is.
        Agent::Claude => {
            frame.contains("? for shortcuts")
                || frame.lines().any(|line| {
                    line.split('│')
                        .nth(1)
                        .is_some_and(|pane| pane.trim() == "$")
                })
        }
        Agent::Codex => frame.contains("Ask Codex to do anything"),
    });
}

fn prompt(
    fixture: &LiveRestore,
    deck: &common::TuiDeck,
    agent: Agent,
    id: &str,
    body: &str,
    probe: &str,
) {
    // TuiDeck's default accessor names its own local socket, whereas this
    // client attaches to the separately owned daemon.
    let events = fixture.live.subscribe_events();
    if let Agent::Claude = agent {
        deck.submit_claude_prompt(&events, id, body, probe);
        return;
    }
    // Same outcome-based submission as e2e_shell_activity_real_agent's Codex
    // scenario. A separate Enter avoids an inferred-paste newline.
    deck.send_keys(body.as_bytes());
    grid(deck, "Codex rendered the human prompt", |frame| {
        frame.contains(probe)
    });
    let deadline = Instant::now() + control();
    loop {
        if !deck.snapshot_grid().contains("Waiting for startup") {
            deck.send_keys(b"\r");
        }
        if events
            .try_wait_for(
                |event| {
                    event.agent_id.as_deref() == Some(id)
                        && event.event_type == dot_agent_deck::event::EventType::Thinking
                        && event.user_prompt.as_deref().is_some_and(|text| {
                            dot_agent_deck::submitted_prompt::prompt_submission_matches(body, text)
                        })
                },
                Duration::from_secs(2),
            )
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Codex never submitted the human prompt: {}",
            deck.snapshot_grid()
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Ack {
    bytes: Vec<u8>,
    inode: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn ack(path: &Path) -> Ack {
    let metadata = std::fs::metadata(path).unwrap();
    Ack {
        bytes: std::fs::read(path).unwrap(),
        inode: metadata.ino(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    }
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Every regular file under `dir`, at any depth and with every directory
/// scanned (`.claude` and `.codex` included: the agents' own native session
/// storage lives in the fixture's separate home, not here), links not
/// followed, whose bytes hold `needle`. It fails closed: a directory, a
/// directory entry or a file that cannot be read is listed too, so it is
/// not taken to hold nothing.
fn files_holding(dir: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            found.push(dir);
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                found.push(dir.clone());
                continue;
            };
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                found.push(path);
                continue;
            };
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                match std::fs::read(&path) {
                    Ok(bytes) if bytes.windows(needle.len()).any(|w| w == needle) => {
                        found.push(path)
                    }
                    Ok(_) => {}
                    Err(_) => found.push(path),
                }
            }
        }
    }
    found
}

fn nonce() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS randomness for independent test sentinels");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn exercise(agent: Agent, name: &str, tui_restore: bool) {
    DEADLINE.with(|deadline| deadline.set(Some(Instant::now() + TEST_BUDGET)));
    let mut fixture = LiveRestore::new(agent, name);
    // Separate random draws: the filename cannot reveal the codename. No
    // prompt asks for the codename to be written anywhere: the
    // acknowledgement holds the fixed text `OK`. Before the recall turn the
    // test checks that no regular file anywhere under the agent's work
    // folder holds the codename (a note the agent kept on its own would;
    // [`files_holding`]), and the recall turn's tool
    // calls are checked for a visible look into the agent's stored
    // transcript. Together they narrow where the recalled codename can come
    // from; they are not a sandbox proof: a copy outside the work folder, or
    // a lookup whose tool event carries no detail, is not ruled out.
    let file_nonce = nonce();
    let word = format!("CODENAME{}", nonce());
    let ack_name = format!("ack-{file_nonce}.txt");
    let recall_name = format!("recall-{file_nonce}.txt");
    let ack_path = fixture.repo.join(&ack_name);
    let recall_path = fixture.repo.join(&recall_name);
    assert!(!ack_path.exists() && !recall_path.exists());
    let attached = fixture.tui("before.toml");
    let original_id = fixture.start(agent);
    grid(&attached, "original named card is attached", |frame| {
        common::label_in_box_top_border(frame, name)
    });
    focus(&attached, agent);
    let first_prompt = format!(
        "For this session, the project codename is {word}. Please acknowledge by creating the file {ack_name} containing the single line OK, using your shell tool. That is all for now; finish your turn."
    );
    prompt(
        &fixture,
        &attached,
        agent,
        &original_id,
        &first_prompt,
        &ack_name,
    );
    common::wait_for_file_trimmed_eq(&ack_path, "OK", turn()).unwrap_or_else(|observed| {
        panic!(
            "initial acknowledgement missing: {observed}\npane:\n{}",
            attached.snapshot_grid()
        )
    });
    fixture.wait_idle(&original_id, &attached);
    let original_ack = ack(&ack_path);
    assert_eq!(
        String::from_utf8_lossy(&original_ack.bytes).trim(),
        "OK",
        "the acknowledgement holds only the fixed text, never the codename"
    );
    let original_attempt = fixture.attempt(&original_id, None);
    let original_session = fixture.session(&original_attempt);
    eprintln!("restore original attempt: {original_attempt}; native session: {original_session}");
    assert!(!original_session.is_empty());
    let old_attempt = string(&original_attempt, "attempt");

    // DaemonProc gives each lifetime its own socket paths. Reattach a fresh
    // TUI after signalling only this test's daemon; do not simulate shutdown.
    fixture.restart(attached);
    let item = fixture.item(&old_attempt);
    let item_id = string(&item, "id");
    assert_eq!(
        item["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["attempt"] == old_attempt)
            .unwrap()["restore"]["kind"],
        "resume"
    );
    assert!(
        fixture.records().is_empty(),
        "no blank agent before the person's choice"
    );
    let attached = fixture.tui("after.toml");
    let mut cli_answered_ms = None;
    grid(&attached, "restart footer", |frame| {
        frame.contains("interrupted item")
    });
    if tui_restore {
        attached.send_keys(b"I");
        grid(&attached, "interrupted named agent", |frame| {
            frame.contains("Interrupted work") && frame.contains(name)
        });
        attached.send_keys(b"s");
        grid(&attached, "restore question", |frame| {
            frame.contains("Restore and continue") && frame.contains("Restore only")
        });
        assert!(fixture.records().is_empty(), "asking starts nothing");
        attached.send_keys(b"c");
    } else {
        let mut restore = fixture.command();
        restore.args([
            "interrupted",
            "restore",
            &item_id,
            "--agent",
            &old_attempt,
            "--continue",
            "--yes",
        ]);
        eprintln!(
            "restore human CLI: interrupted restore {item_id} --agent {old_attempt} --continue --yes"
        );
        let outcome = bounded(restore, control());
        if !outcome.status.success() {
            eprintln!(
                "restore failure journal: {}\nlive records: {:?}\npane:\n{}\nrestore diagnostics:\n{}",
                journal::dump(&fixture.state),
                fixture.records(),
                common::redact_credentials_for_output(&attached.snapshot_grid()),
                fixture.restore_diagnostics()
            );
        }
        let outcome = succeeded(outcome);
        cli_answered_ms = Some(epoch_ms());
        eprintln!(
            "restore human CLI outcome: {}",
            String::from_utf8_lossy(&outcome.stdout)
        );
    }
    let restored = fixture.restored();
    let restored_id = string(&restored, "id");
    let new_attempt = fixture.attempt(&restored_id, Some(&old_attempt));
    assert_ne!(new_attempt["attempt"], original_attempt["attempt"]);
    let resumed_session = fixture.session(&new_attempt);
    eprintln!(
        "restore new attempt: {new_attempt}; native session: {resumed_session}; live record: {restored}"
    );
    // The CLI answers only once the restored agent's first attested
    // SessionStart confirmed the conversation, so spawn (the new attempt's
    // journal stamp) to that answer bounds spawn to first SessionStart.
    if let (Some(answered), Some(spawned)) = (cli_answered_ms, new_attempt["ts_ms"].as_u64()) {
        eprintln!(
            "restore timing: spawn to confirmed first SessionStart <= {} ms (attempt_started ts_ms {spawned}, CLI answered {answered})",
            answered.saturating_sub(spawned)
        );
    }
    eprintln!(
        "restore diagnostics after success:\n{}",
        fixture.restore_diagnostics()
    );
    assert_eq!(
        resumed_session, original_session,
        "the new attempt must attest the same native conversation, not a blank one"
    );
    fixture.first_restart_note_received(&restored, &attached);
    assert_eq!(
        ack(&ack_path),
        original_ack,
        "restart must not repeat the first turn"
    );
    command_mode(&attached);
    grid(
        &attached,
        "restored named card has a live Idle status",
        |frame| common::label_in_box_top_border(frame, name) && frame.contains("Idle"),
    );
    focus(&attached, agent);
    let recall = format!(
        "What is the project codename I told you earlier in this conversation? Please write just the codename into the file {recall_name} using your shell tool, answering from our conversation without looking anything up. Then finish your turn."
    );
    // Round 5 (audit suggestion on 331efc9c), round 6: no regular file
    // anywhere under the work folder, `.claude` and `.codex` included, holds
    // the codename the recall turn could read instead of remembering; one
    // that cannot be read counts as holding it ([`files_holding`]).
    let copies = files_holding(&fixture.repo, word.as_bytes());
    assert!(
        copies.is_empty(),
        "the codename must be in no file of the work folder before the recall: {copies:?}"
    );
    let recall_events = fixture.live.subscribe_events();
    prompt(
        &fixture,
        &attached,
        agent,
        &restored_id,
        &recall,
        &recall_name,
    );
    common::wait_for_file_trimmed_eq(&recall_path, &word, turn()).unwrap_or_else(|observed| {
        panic!(
            "restored agent lost its conversation: {observed}\n{}",
            attached.snapshot_grid()
        )
    });
    fixture.wait_idle(&restored_id, &attached);
    // The codename reached the recall file from the conversation, not from
    // a look at the stored transcript: no tool call in the recall turn names
    // the agents' session storage. A tool event without a detail is not
    // evidence either way, so this rules out the visible route only.
    let lookups: Vec<String> = recall_events
        .snapshot()
        .into_iter()
        .filter(|event| {
            event.agent_id.as_deref() == Some(restored_id.as_str())
                && event.event_type == dot_agent_deck::event::EventType::ToolStart
        })
        .filter_map(|event| event.tool_detail)
        .filter(|detail| {
            [".claude", ".codex", ".jsonl"]
                .iter()
                .any(|storage| detail.contains(storage))
        })
        .collect();
    assert!(
        lookups.is_empty(),
        "the recall must come from the conversation, not from reading its stored transcript: {lookups:?}"
    );
    assert_eq!(
        ack(&ack_path),
        original_ack,
        "no second acknowledgement write during restart, note delivery or recall"
    );
    assert_eq!(fixture.session(&new_attempt), original_session);
    command_mode(&attached);
    grid(
        &attached,
        "the recalled session remains live on its named card",
        |frame| common::label_in_box_top_border(frame, name) && frame.contains("Idle"),
    );
    assert_eq!(
        fixture.records().len(),
        1,
        "exactly one restored interactive agent"
    );
    // TuiDeck/DaemonProc own their child pids and perform scoped teardown on
    // success or panic. No process-name kill, host session or credential edit.
}

/// Scenario: In an attached TUI, a real interactive Haiku is told a unique project codename and writes an acknowledgement, then the test stops its daemon by pid and starts a replacement sharing the same state directory and isolated HOME. The person presses I, s and c to restore and continue; the agent receives the restart note first and, with no copy of the codename in any regular file anywhere under its work folder and no visible look into its stored transcript, writes the codename into a new file without rewriting its acknowledgement, while its new journal attempt keeps the old session id and the named card shows a live status.
#[spec("journal/restore/016")]
#[test]
fn journal_restore_016_real_claude_recalls_its_conversation_after_tui_restore() {
    // Missing CLI/auth skips normally and fails with REQUIRE_REAL_E2E=1.
    skip_unless!(common::check_claude_available());
    exercise(Agent::Claude, "haiku-016", true);
}

/// Scenario: A real interactive Codex on the harness's cheap mini model is told a unique project codename under an attached TUI before its daemon is stopped by pid. After restart, a human CLI restore with --continue --yes brings back the same native session, delivers the restart note before the recall request, and, with no copy of the codename in any regular file anywhere under its work folder and no visible look into its stored transcript, writes the codename into a new file while leaving the first acknowledgement unchanged and showing a live named card.
#[spec("journal/restore/017")]
#[test]
// Quarantined (CLAUDE.md rule 6): Codex sends its first hook only with its
// first prompt, so a restore cannot confirm the conversation came back before
// the restart note, and every run timed out. Stage 3 ships Codex as a
// documented gap (its registry `resume` is `None`); #2492 is the design change
// that brings it back, and lifts this quarantine with the body unchanged.
#[ignore = "quarantined: vfarcic, #2492"]
fn journal_restore_017_real_codex_recalls_its_conversation_after_cli_restore() {
    // Existing helper checks persisted auth and a bounded live model probe.
    // skip_unless converts an unavailable login to failure under REQUIRE=1.
    skip_unless!(common::check_codex_available());
    exercise(Agent::Codex, "codex-017", false);
}
