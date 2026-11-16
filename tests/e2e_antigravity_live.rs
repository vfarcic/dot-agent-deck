#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Real interactive Antigravity, isolated credentials, attached dashboard.
dot_agent_deck_test_harness::link!();
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;

use common::TuiDeck;
use inbox::Pane;
use serde_json::{Value, json};
use spec::spec;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const BUDGET: Duration = Duration::from_secs(120);
const TOKEN: &str = ".gemini/antigravity-cli/antigravity-oauth-token";
const CONTENT: &str = "antigravity-real-work-91cf4a";
const REPORT: &str = "AGY-REAL-REPORT-2c6f19";

fn antigravity_test_model() -> String {
    std::env::var("DOT_AGENT_DECK_ANTIGRAVITY_TEST_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "gemini-3.8-flash-low".into())
}

fn host_token() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("host HOME")).join(TOKEN)
}

fn gemini_key() -> Option<String> {
    std::env::var("GEMINI_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn token_redactions(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) if s.len() >= 8 => out.push(s.clone()),
        Value::Object(map) => map.values().for_each(|v| token_redactions(v, out)),
        Value::Array(items) => items.iter().for_each(|v| token_redactions(v, out)),
        _ => {}
    }
}

fn seed_home(home: &Path, fixture: &Path) -> std::io::Result<()> {
    let source = host_token();
    let target = home.join(TOKEN);
    std::fs::create_dir_all(target.parent().unwrap())?;
    std::fs::set_permissions(
        target.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )?;
    if std::fs::symlink_metadata(&source).is_ok_and(|m| m.file_type().is_file() && m.len() > 0) {
        let bytes = std::fs::read(source)?;
        // Register before a copy or a diagnostic can expose a token value.
        let mut redactions = vec![String::from_utf8_lossy(&bytes).into_owned()];
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            token_redactions(&value, &mut redactions);
        }
        common::register_diagnostic_redactions(redactions);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)?
            .write_all(&bytes)?;
        assert_eq!(
            std::fs::metadata(&target)?.permissions().mode() & 0o777,
            0o600
        );
    } else if gemini_key().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Antigravity token unavailable; sign in or set GEMINI_API_KEY",
        ));
    }
    let cli = home.join(".gemini/antigravity-cli");
    std::fs::create_dir_all(cli.join("cache"))?;
    std::fs::write(
        cli.join("cache/onboarding.json"),
        br#"{"onboardingComplete":true,"consumerOnboardingComplete":true}"#,
    )?;
    std::fs::write(
        cli.join("settings.json"),
        json!({"trustedWorkspaces":[fixture, fixture.canonicalize()?]}).to_string(),
    )?;
    Ok(())
}

/// Probe the same model the interactive pane uses, without an inference turn.
fn check_antigravity_available() -> Result<(), String> {
    common::install_credential_redaction();
    if !Command::new("agy")
        .arg("--version")
        .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return Err("Antigravity CLI not available on PATH (agy --version)".into());
    }
    let root = common::race_safe_tempdir();
    let home = root.path().join("home");
    seed_home(&home, root.path()).map_err(|_| {
        "Antigravity needs a non-empty OAuth token file or GEMINI_API_KEY".to_string()
    })?;
    let mut command = Command::new("agy");
    command
        .arg("models")
        .env_clear()
        .env("HOME", &home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
        .current_dir(root.path())
        .stdin(Stdio::null());
    if let Some(key) = gemini_key() {
        command.env("GEMINI_API_KEY", key);
    }
    let child = std::cell::RefCell::new(
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "Antigravity model availability probe failed".to_string())?,
    );
    if !common::wait_until(Duration::from_secs(30), || {
        child.borrow_mut().try_wait().is_ok_and(|s| s.is_some())
    }) {
        let _ = child.borrow_mut().kill();
        let _ = child.borrow_mut().wait();
        return Err("Antigravity model availability probe timed out".into());
    }
    let output = child
        .into_inner()
        .wait_with_output()
        .map_err(|_| "Antigravity model availability probe failed".to_string())?;
    let model = antigravity_test_model();
    if !output.status.success()
        || !String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.split_whitespace().next() == Some(model.as_str()))
    {
        return Err(format!(
            "Antigravity model {model} unavailable; set DOT_AGENT_DECK_ANTIGRAVITY_TEST_MODEL to an accessible model"
        ));
    }
    eprintln!("Antigravity model under test: {model}");
    Ok(())
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

struct Agy {
    deck: TuiDeck,
    pane: Pane,
    orchestrator: Option<Pane>,
}

impl Agy {
    fn start(team: bool) -> Self {
        let deck = TuiDeck::builder()
            .with_pty_size(220, 60)
            .with_env("PATH", common::path_with_binary_dir())
            .with_env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
            .with_env("DOT_AGENT_DECK_LOG", "antigravity-live.log")
            .launch_with_fixture("minimal");
        deck.wait_for_string("No active agents");
        seed_home(deck.home_dir(), deck.workdir()).expect("seed isolated Antigravity HOME");
        let binary = common::contained_deck_binary();
        let installed = deck.home_dir().join(".local/bin/dot-agent-deck");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        if !installed.exists() && std::fs::hard_link(binary, &installed).is_err() {
            std::fs::copy(binary, &installed).unwrap();
        }
        // The live test drives the hook CLI and delivery independently of the
        // installer's RED tests (hooks/install/028–031). Use the measured file
        // shape here so a missing installer cannot prevent a real agent turn.
        let hook = json!({"type":"command", "command":format!("{} hook --agent antigravity", quote(installed.to_str().unwrap()))});
        let hooks = json!({"dot-agent-deck":{
            "SessionStart":[hook],"PreInvocation":[hook],"Stop":[hook],
            "PreToolUse":[{"matcher":"*","hooks":[hook]}],
            "PostToolUse":[{"matcher":"*","hooks":[hook]}]
        }});
        let config = deck.home_dir().join(".gemini/config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("hooks.json"), hooks.to_string()).unwrap();
        // Explicitly approve tools for this bounded scratch-file scenario:
        // real 1.5.9 still asked to create a file with --mode auto alone.
        let command = common::capability_export_command(&format!(
            "exec agy --mode auto --dangerously-skip-permissions --model {}",
            quote(&antigravity_test_model())
        ));
        let membership = |role: &str, index, start| {
            json!({
                "kind":"orchestration","name":"agy-real","role_name":role,
                "role_index":index,"is_start_role":start,"orchestration_id":"agy-real-instance",
                "orchestration_cwd":deck.workdir()
            })
        };
        let orchestrator = if team {
            let orch_command = route::command(deck.workdir());
            let config = format!(
                "[[orchestrations]]\nname = 'agy-real'\n[[orchestrations.roles]]\nname = 'orchestrator'\ncommand = {}\nstart = true\n[[orchestrations.roles]]\nname = 'coder'\nagent = 'antigravity'\ncommand = {}\nclear = false\n",
                serde_json::to_string(&orch_command).unwrap(),
                serde_json::to_string(&command).unwrap()
            );
            std::fs::write(deck.workdir().join(".dot-agent-deck.toml"), config).unwrap();
            deck.approve_project_trust(deck.workdir());
            Some(route::start(
                &deck,
                "agy-real-orchestrator",
                json!({"tab_membership":membership("orchestrator",0,true)}),
            ))
        } else {
            None
        };
        let mut request = json!({"op":"start-agent","command":command,"agent_type":"antigravity",
            "cwd":deck.workdir(),"rows":48,"cols":175,"display_name":"live worker",
            "env":[["DOT_AGENT_DECK_PANE_ID","agy-real-worker"]]});
        if team {
            request["tab_membership"] = membership("coder", 1, false);
        }
        let reply = inbox::attach(&deck, request);
        assert_eq!(reply["ok"], true, "start real Antigravity: {reply}");
        let agent = reply["id"].as_str().unwrap().to_owned();
        let token = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(common::recorded_hook_capability(deck.workdir(), &agent))
            .unwrap();
        let pane = Pane {
            id: "agy-real-worker".into(),
            agent,
            token,
            pid: 0,
            capture: PathBuf::new(),
            attach_socket: deck.attach_socket_path().into(),
        };
        let result = Self {
            deck,
            pane,
            orchestrator,
        };
        assert!(
            common::wait_until(BUDGET, || result.screen().contains("? for shortcuts")),
            "real agy must reach the seeded interactive composer: {}",
            result.evidence()
        );
        // agy 1.5.9 displays a first-run announcement even after onboarding.
        // Dismiss it before the user types. Leave the delegated worker's box
        // untouched so its first prompt is genuinely the deck's doorbell.
        if !team
            && result
                .screen()
                .contains("Auto is now the default agent mode")
        {
            let mut input =
                common::AttachInput::attach(result.deck.attach_socket_path(), &result.pane.agent);
            input.write("\x1b");
            assert!(
                common::wait_until(Duration::from_secs(10), || !result
                    .screen()
                    .contains("Auto is now the default agent mode")),
                "dismiss the Auto-mode announcement: {}",
                result.evidence()
            );
        }
        // Open the actual agent pane in the attached TUI (second card for a team).
        if team {
            let (col, row) = result.deck.wait_for_in_grid("agy-real");
            result.deck.click(col + 1, row);
            result.deck.wait_for_string("coder");
        }
        result.deck.send_bytes(if team { b"2" } else { b"1" });
        assert!(
            result
                .deck
                .wait_for_grid_string_within("? for shortcuts", BUDGET),
            "real agy must be visible in attached TUI: {}",
            result.evidence()
        );
        result
    }

    fn screen(&self) -> String {
        let records = common::agent_records_on(self.deck.attach_socket_path());
        let record = records
            .iter()
            .find(|record| record.id == self.pane.agent)
            .expect("real Antigravity record");
        let mut parser = dot_agent_deck::vt_grid::parser(record.rows, record.cols, 0);
        parser.process(&common::pane_snapshot_on(
            self.deck.attach_socket_path(),
            &self.pane.agent,
        ));
        parser.screen().contents()
    }
    fn evidence(&self) -> String {
        format!(
            "Pane:\n{}\nGrid:\n{}\nLog:\n{}",
            self.screen(),
            self.deck.snapshot_grid(),
            std::fs::read_to_string(self.deck.workdir().join("antigravity-live.log"))
                .unwrap_or_default()
        )
    }
    fn card(&self, status: &str) {
        assert!(
            self.deck
                .wait_for_grid_predicate_within(BUDGET, |grid| grid.lines().any(|line| {
                    let title = line.split(['┐', '┓']).next().unwrap_or(line);
                    (title.starts_with('┌') || title.starts_with('┏'))
                        && title.contains("Antigravity")
                        && title.contains(status)
                })),
            "Antigravity card must show {status}: {}",
            self.evidence()
        );
    }
}

/// Scenario: Type a directive into real interactive Antigravity through the attached TUI asking it to create a uniquely named sentinel. Its Antigravity card shows the submitted prompt, visibly goes Working and then Idle, and the sentinel contains the requested unique content.
#[spec("antigravity/live/001")]
#[test]
fn antigravity_live_001_attached_real_agent_creates_a_sentinel_with_visible_status() {
    skip_unless!(check_antigravity_available());
    let agy = Agy::start(false);
    let sentinel = agy.deck.workdir().join("agy-attached-sentinel-91cf4a.txt");
    assert!(!sentinel.exists());
    let prompt = format!(
        "AGY-LIVE-91cf4a: Create the file {} with exactly this content: {CONTENT}. Use your file or shell tool now. Then reply DONE and end your turn. This is the complete task; do not ask questions.",
        sentinel.display()
    );
    agy.deck.send_bytes(prompt.as_bytes());
    assert!(
        common::wait_until(Duration::from_secs(10), || agy
            .screen()
            .contains("agy-attached-sentinel-91cf4a.txt")),
        "prompt must be visible in real composer: {}",
        agy.evidence()
    );
    agy.deck.send_bytes(b"\r");
    assert!(
        agy.deck
            .wait_for_grid_predicate_within(Duration::from_secs(10), |grid| grid.lines().any(
                |line| (line.starts_with("│Prmt:") || line.starts_with("┃Prmt:"))
                    && line.contains("AGY-LIVE-91cf4a")
            )),
        "Antigravity card must show the submitted prompt from its native hook: {}",
        agy.evidence()
    );
    agy.card("Working");
    common::wait_for_file_trimmed_eq(&sentinel, CONTENT, BUDGET).unwrap_or_else(|why| {
        panic!(
            "real Antigravity must write sentinel: {why}; {}",
            agy.evidence()
        )
    });
    agy.card("Idle");
}

/// Scenario: Delegate a task from an orchestration's start role to a real interactive Antigravity worker before its first prompt. The worker creates the unique sentinel and reports its task id through the real work-done CLI, whose framed report is fetched by the orchestrator.
#[spec("antigravity/live/002")]
#[test]
fn antigravity_live_002_real_worker_receives_delegate_and_reports_work_done() {
    skip_unless!(check_antigravity_available());
    let agy = Agy::start(true);
    let orchestrator = agy.orchestrator.as_ref().unwrap();
    let sentinel = agy.deck.workdir().join("agy-delegate-sentinel-2c6f19.txt");
    assert!(!sentinel.exists());
    let task = format!(
        "Create {} containing exactly {CONTENT}. Then run this shell command, replacing TASK_ID with this framed message's id: {} work-done --task-id TASK_ID --task {REPORT}. This is the complete task; use tools now, do not ask questions, then end your turn.",
        sentinel.display(),
        quote(common::contained_deck_binary().to_str().unwrap())
    );
    let id = route::delegate(&agy.deck, orchestrator, &task, false);
    common::wait_for_file_trimmed_eq(&sentinel, CONTENT, BUDGET).unwrap_or_else(|why| {
        panic!(
            "real Antigravity must act on delegated {id}: {why}; {}",
            agy.evidence()
        )
    });
    let fetches = orchestrator.capture.with_extension("fetches");
    assert!(
        common::wait_until(BUDGET, || {
            std::fs::write(orchestrator.capture.with_extension("fetch-now"), "").unwrap();
            std::fs::read_to_string(&fetches).is_ok_and(|text| {
                text.contains(REPORT) && text.contains(&id) && text.contains("UNTRUSTED")
            })
        }),
        "orchestrator must receive task-scoped work-done report for {id}: {}",
        agy.evidence()
    );
    agy.card("Idle");
}
