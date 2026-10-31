#![cfg(feature = "e2e")]

//! Issue #2053: `inbox send --name` / `--pane`, through the real CLI, daemon
//! and attached TUI, with credential-free stand-ins that record the identity
//! the deck minted for their pane. Tokens are never printed or included in an
//! assertion diagnostic.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::AgentRecord;
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::AgentType;
use spec::spec;

const WAIT: Duration = Duration::from_secs(60);

struct Deck {
    deck: TuiDeck,
    scratch: tempfile::TempDir,
}

impl Deck {
    fn new() -> Self {
        let scratch = common::race_safe_tempdir();
        let probe = scratch.path().join("identity-probe.py");
        std::fs::write(
            &probe,
            r#"import json, os, pathlib, subprocess, sys
identity = {key: os.environ[key] for key in (
    "DOT_AGENT_DECK_PANE_ID", "DOT_AGENT_DECK_AGENT_ID", "DOT_AGENT_DECK_PANE_CAPABILITY")}
path = pathlib.Path(sys.argv[1]) / (identity["DOT_AGENT_DECK_PANE_ID"] + ".json")
temporary = path.with_suffix(".pending")
temporary.write_text(json.dumps(identity))
temporary.chmod(0o600)
temporary.replace(path)
subprocess.run([sys.argv[2], "hook", "--agent", "claude-code"],
    input=json.dumps({"hook_event_name": "SessionStart", "session_id": "by-name-" + identity["DOT_AGENT_DECK_PANE_ID"]}),
    text=True, check=True, stdout=subprocess.DEVNULL)
print("BY-NAME-PROBE-READY", flush=True)
os.execvp("cat", ["cat"])
"#,
        )
        .expect("write identity-recording stand-in");
        let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
        let command = format!(
            "python3 -u {} {} {}",
            quote(&probe),
            quote(scratch.path()),
            quote(Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"))),
        );
        let config = scratch.path().join("config.toml");
        std::fs::write(
            &config,
            format!("default_command = {}\n", toml::Value::String(command)),
        )
        .expect("write stand-in config");
        let bin_dir = Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .parent()
            .expect("binary parent");
        let deck = TuiDeck::builder()
            .with_pty_size(220, 60)
            .with_env(
                "PATH",
                format!(
                    "{}:{}",
                    bin_dir.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .with_env("DOT_AGENT_DECK_CONFIG", config.to_string_lossy())
            .with_env(
                "DOT_AGENT_DECK_LOG",
                scratch.path().join("daemon.log").to_string_lossy(),
            )
            .launch_with_fixture("minimal");
        deck.wait_for_string("No active agents");
        Self { deck, scratch }
    }

    fn records(&self) -> Vec<AgentRecord> {
        common::agent_records_on(self.deck.attach_socket_path())
    }

    fn identity_path(&self, record: &AgentRecord) -> PathBuf {
        self.scratch.path().join(format!("{}.json", pane(record)))
    }

    fn ready(&self, record: &AgentRecord) -> bool {
        self.identity_path(record).is_file()
            && record
                .live
                .as_ref()
                .is_some_and(|live| live.agent_type == Some(AgentType::ClaudeCode))
    }

    /// Open a standalone agent named `name` in the fixture folder through the
    /// TUI's New Agent dialog.
    fn open(&self, name: &str) -> AgentRecord {
        self.deck.send_keys(b"\x0e");
        self.deck.send_keys(b" ");
        self.deck.wait_for_string("┌ New Agent");
        self.deck.send_keys(b"\t");
        self.deck.send_keys(&[0x7f; 96]);
        self.deck.send_keys(name.as_bytes());
        let (col, row) = self.deck.wait_for_in_grid("[Submit]");
        self.deck.click(col, row);
        self.deck.wait_for_absence("[Submit]");
        assert!(
            common::wait_until(WAIT, || self.records().iter().any(|r| r
                .display_name
                .as_deref()
                == Some(name)
                && self.ready(r))),
            "agent {name} did not publish its identity\n{}",
            self.deck.snapshot_grid()
        );
        self.records()
            .into_iter()
            .find(|r| r.display_name.as_deref() == Some(name))
            .expect("ready agent")
    }

    /// Run the real CLI as `caller`'s pane (or as a plain shell for `None`).
    fn cli(&self, caller: Option<&AgentRecord>, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
        command
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.deck.home_dir())
            .env("DOT_AGENT_DECK_SOCKET", self.deck.hook_socket_path())
            .env(
                "DOT_AGENT_DECK_ATTACH_SOCKET",
                self.deck.attach_socket_path(),
            )
            .current_dir(self.deck.workdir())
            .args(args);
        if let Some(record) = caller {
            let identity: std::collections::BTreeMap<String, String> = serde_json::from_slice(
                &std::fs::read(self.identity_path(record)).expect("read stand-in identity"),
            )
            .expect("parse stand-in identity");
            command.envs(identity);
        }
        command.output().expect("run real deck CLI")
    }

    fn inbox_text(&self, record: &AgentRecord) -> String {
        common::attach_request_on(
            self.deck.attach_socket_path(),
            &AttachRequest::ListInbox {
                pane_id: Some(pane(record).to_string()),
                bodies: true,
            },
        )
        .ok()
        .and_then(|response| response.inbox_messages)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|message| message.text)
        .collect::<Vec<_>>()
        .join("\n")
    }

    fn delivered(&self, record: &AgentRecord, sentinel: &str) -> bool {
        common::wait_until(WAIT, || self.inbox_text(record).contains(sentinel))
    }
}

fn pane(record: &AgentRecord) -> &str {
    record.pane_id_env.as_deref().expect("pane identity")
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn exit(label: &str, output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{label}: expected exit {code}\n{}",
        text(output)
    );
}

/// Scenario: Three standalone agents open in one folder: a caller, `dispatcher` and `observability`. The caller's `inbox send --folder` is refused as ambiguous, `--name dispatcher` and `--pane <observability's pane>` each deliver to that agent alone, and `roles --name dispatcher` lists only it. The dispatcher then replies with the command the deck wrote into the message, which names the caller with `--name`, and the reply reaches the caller.
#[spec("orchestration/address/006")]
#[test]
fn orchestration_address_006_name_and_pane_reach_one_of_two_standalone_agents() {
    let fixture = Deck::new();
    let caller = fixture.open("caller");
    let dispatcher = fixture.open("dispatcher");
    let observability = fixture.open("observability");
    let all = [&caller, &dispatcher, &observability];
    let folder = fixture.deck.workdir().to_str().expect("utf-8 fixture path");
    for record in all {
        assert_eq!(
            record.cwd.as_deref().map(Path::new),
            Some(Path::new(folder))
        );
    }

    // A folder alone is ambiguous, and the refusal points at --name / --pane.
    let output = fixture.cli(
        Some(&caller),
        &[
            "inbox",
            "send",
            "--folder",
            folder,
            "--text",
            "AMBIGUOUS_2053_a1",
        ],
    );
    exit("folder alone", &output, 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("refused: ambiguous")
            && stderr.contains("--name")
            && stderr.contains(pane(&dispatcher))
            && stderr.contains(pane(&observability)),
        "{stderr}"
    );

    // --name, with the folder, reaches the dispatcher alone.
    let output = fixture.cli(
        Some(&caller),
        &[
            "inbox",
            "send",
            "--folder",
            folder,
            "--name",
            "dispatcher",
            "--text",
            "BY_NAME_2053_b2",
        ],
    );
    exit("by name", &output, 0);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("Sent m-")
            && stdout.contains(&format!("to dispatcher (pane {})", pane(&dispatcher))),
        "{stdout}"
    );
    assert!(
        fixture.delivered(&dispatcher, "BY_NAME_2053_b2"),
        "the name-addressed message never reached the dispatcher"
    );

    // --pane reaches the observability agent alone.
    let output = fixture.cli(
        Some(&caller),
        &[
            "inbox",
            "send",
            "--pane",
            pane(&observability),
            "--text",
            "BY_PANE_2053_c3",
        ],
    );
    exit("by pane", &output, 0);
    assert!(
        fixture.delivered(&observability, "BY_PANE_2053_c3"),
        "the pane-addressed message never reached the observability agent"
    );

    // A name nobody has is refused, and nothing goes anywhere.
    let output = fixture.cli(
        Some(&caller),
        &[
            "inbox",
            "send",
            "--folder",
            folder,
            "--name",
            "nobody",
            "--text",
            "NOBODY_2053_d4",
        ],
    );
    exit("unknown name", &output, 1);
    assert!(
        text(&output).contains("refused: no live standalone agent"),
        "{}",
        text(&output)
    );

    // `roles --name` previews the same single recipient, from any shell.
    let output = fixture.cli(None, &["roles", "--json", "--name", "dispatcher"]);
    exit("roles --name", &output, 0);
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).expect("roles --json output");
    let listed: Vec<_> = rows
        .iter()
        .map(|row| row["pane_id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, [pane(&dispatcher)]);

    // The dispatcher answers with the command the deck wrote into the
    // message: from its pane the folder matches the caller and the
    // observability agent, so the command names the caller.
    let message = fixture.inbox_text(&dispatcher);
    let reply = message
        .lines()
        .find_map(|line| line.trim().strip_prefix("reply with: "))
        .unwrap_or_else(|| panic!("no reply command in the delivered message\n{message}"));
    assert!(reply.contains("--name=caller"), "{reply}");
    let words = shell_words(reply.replace("'<your reply>'", "REPLY_2053_e5").as_str());
    assert_eq!(words[0], "dot-agent-deck");
    let args: Vec<&str> = words[1..].iter().map(String::as_str).collect();
    let output = fixture.cli(Some(&dispatcher), &args);
    exit("reply by name", &output, 0);
    assert!(
        fixture.delivered(&caller, "REPLY_2053_e5"),
        "the reply never reached the caller"
    );

    for (record, sentinel) in [
        (&caller, "AMBIGUOUS_2053_a1"),
        (&dispatcher, "AMBIGUOUS_2053_a1"),
        (&observability, "AMBIGUOUS_2053_a1"),
        (&observability, "BY_NAME_2053_b2"),
        (&dispatcher, "BY_PANE_2053_c3"),
        (&observability, "REPLY_2053_e5"),
    ] {
        assert!(
            !fixture.inbox_text(record).contains(sentinel),
            "{sentinel} reached pane {}",
            pane(record)
        );
    }
    for record in all {
        assert!(!fixture.inbox_text(record).contains("NOBODY_2053_d4"));
    }
}

/// Split a command the deck printed: words separated by spaces, with
/// single-quoted words taken literally.
fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut started = false;
    for c in line.chars() {
        match c {
            '\'' => {
                quoted = !quoted;
                started = true;
            }
            ' ' if !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}
