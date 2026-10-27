#![cfg(feature = "e2e")]

//! Synthetic, credential-free orchestrator discovery and messaging through the
//! real CLI, daemon and attached TUI. The pane probes record their own minted identities outside
//! the checkout; tokens are never printed or included in assertion diagnostics.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::{AgentRecord, TabMembership};
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::AgentType;
use spec::spec;

const WAIT: Duration = Duration::from_secs(60);

struct AddressDeck {
    deck: TuiDeck,
    scratch: tempfile::TempDir,
    worktrees: Vec<PathBuf>,
}

impl Drop for AddressDeck {
    fn drop(&mut self) {
        // Dispatch creates siblings outside the fixture TempDir. Reclaim just
        // this test's paths, including when an expected RED assertion unwinds.
        for path in &self.worktrees {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

impl AddressDeck {
    fn new() -> Self {
        Self::with_sidebar(220, "orchestrator")
    }

    fn with_sidebar(cols: u16, leader_role: &str) -> Self {
        let scratch = common::race_safe_tempdir();
        let probe = scratch.path().join("address-probe.py");
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
    input=json.dumps({"hook_event_name": "SessionStart", "session_id": "address-probe-" + identity["DOT_AGENT_DECK_PANE_ID"]}),
    text=True, check=True, stdout=subprocess.DEVNULL)
if sys.argv[3] == "working":
    subprocess.run([sys.argv[2], "hook", "--agent", "claude-code"],
        input=json.dumps({"hook_event_name": "PreToolUse", "session_id": "address-probe-" + identity["DOT_AGENT_DECK_PANE_ID"],
            "tool_name": "Bash", "tool_input": {"command": "echo address-sidebar-probe"}}),
        text=True, check=True, stdout=subprocess.DEVNULL)
print("ADDRESS-PROBE-READY", flush=True)
os.execvp("cat", ["cat"])
"#,
        )
        .expect("write identity-recording stand-in");
        let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
        let command = format!(
            "python3 -u {} {} {} {}",
            quote(&probe),
            quote(scratch.path()),
            quote(Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"))),
            if leader_role == "orchestrator" {
                "idle"
            } else {
                "working"
            }
        );
        let encoded = toml::Value::String(command.clone()).to_string();
        let config = scratch.path().join("config.toml");
        std::fs::write(&config, format!("default_command = {encoded}\n"))
            .expect("write stand-in config");
        let bin_dir = Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .parent()
            .expect("binary parent");
        let deck = TuiDeck::builder()
            .with_pty_size(cols, 60)
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
            .with_env("RUST_LOG", "info")
            .launch_with_fixture("minimal");
        deck.wait_for_string("No active agents");
        std::fs::write(
            deck.workdir().join(".dot-agent-deck.toml"),
            format!(
                "[[orchestrations]]\nname = \"address-team\"\n\n\
                 [[orchestrations.roles]]\nname = \"{leader_role}\"\ncommand = {encoded}\nstart = true\n\n\
                 [[orchestrations.roles]]\nname = \"worker\"\ncommand = {encoded}\n"
            ),
        )
        .expect("write two-role stand-in orchestration");
        common::commit_fixture_repo(deck.workdir());
        Self {
            deck,
            scratch,
            worktrees: Vec::new(),
        }
    }

    fn records(&self) -> Vec<AgentRecord> {
        common::agent_records_on(self.deck.attach_socket_path())
    }

    fn identity_path(&self, record: &AgentRecord) -> PathBuf {
        self.scratch.path().join(format!(
            "{}.json",
            record.pane_id_env.as_deref().expect("pane id")
        ))
    }

    fn probe_ready(&self, record: &AgentRecord) -> bool {
        self.identity_path(record).is_file()
            && record
                .live
                .as_ref()
                .is_some_and(|live| live.agent_type == Some(AgentType::ClaudeCode))
    }

    fn open_caller(&self, name: &str) -> AgentRecord {
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
            common::wait_until(WAIT, || self.records().iter().any(|r| {
                r.display_name.as_deref() == Some(name) && self.probe_ready(r)
            })),
            "caller {name} did not publish its own identity\n{}",
            self.deck.snapshot_grid()
        );
        self.records()
            .into_iter()
            .find(|r| r.display_name.as_deref() == Some(name))
            .expect("ready caller")
    }

    fn cli(&self, caller: Option<&AgentRecord>, cwd: &Path, args: &[&str]) -> Output {
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
            .current_dir(cwd)
            .args(args);
        if let Some(record) = caller {
            let identity: std::collections::BTreeMap<String, String> = serde_json::from_slice(
                &std::fs::read(self.identity_path(record)).expect("read private stand-in identity"),
            )
            .expect("parse private stand-in identity");
            command.envs(identity);
        }
        command.output().expect("run real deck CLI")
    }

    fn dispatch(&mut self, caller: &AgentRecord, name: &str, team: bool) -> Vec<AgentRecord> {
        let worktree = self
            .deck
            .workdir()
            .parent()
            .expect("fixture parent")
            .join(format!(
                "{}-dispatch-{name}",
                self.deck
                    .workdir()
                    .file_name()
                    .expect("fixture name")
                    .to_string_lossy()
            ));
        self.worktrees.push(worktree.clone());
        let output = self.cli(
            Some(caller),
            self.deck.workdir(),
            &[
                "dispatch",
                name,
                "--task",
                "Wait for terminal completion from the test.",
                if team {
                    "--orchestration=address-team"
                } else {
                    "--single"
                },
            ],
        );
        assert_exit("dispatch setup", &output, 0);
        let members = || {
            self.records()
                .into_iter()
                .filter(|r| {
                    r.cwd
                        .as_deref()
                        .is_some_and(|cwd| Path::new(cwd) == worktree)
                })
                .collect::<Vec<_>>()
        };
        assert!(
            common::wait_until(WAIT, || {
                let roles = members();
                roles.len() == if team { 2 } else { 1 } && roles.iter().all(|r| self.probe_ready(r))
            }),
            "dispatched unit never became ready\n{}",
            self.deck.snapshot_grid()
        );
        let mut roles = members();
        roles.sort_by_key(|r| !is_orchestrator(r));
        roles
    }

    /// Every message in `pane`'s inbox, rendered whole, joined.
    fn inbox_text(&self, pane: &str) -> String {
        self.inbox_bodies(pane).join("\n")
    }

    fn inbox_bodies(&self, pane: &str) -> Vec<String> {
        common::attach_request_on(
            self.deck.attach_socket_path(),
            &AttachRequest::ListInbox {
                pane_id: Some(pane.to_string()),
                bodies: true,
            },
        )
        .ok()
        .and_then(|response| response.inbox_messages)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|message| message.text)
        .collect()
    }
}

fn is_orchestrator(record: &AgentRecord) -> bool {
    matches!(
        record.tab_membership,
        Some(TabMembership::Orchestration {
            is_start_role: true,
            ..
        })
    )
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_exit(label: &str, output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{label}: expected exit {code}\n{}",
        output_text(output)
    );
}

fn pane(record: &AgentRecord) -> &str {
    record.pane_id_env.as_deref().expect("pane identity")
}

fn folder(record: &AgentRecord) -> &Path {
    Path::new(record.cwd.as_deref().expect("agent folder"))
}

fn title(record: &AgentRecord) -> &str {
    match &record.tab_membership {
        Some(TabMembership::Orchestration {
            name,
            display_title,
            ..
        }) => display_title.as_deref().unwrap_or(name),
        _ => panic!("expected orchestration"),
    }
}

fn refused(output: &Output, reason: &str) {
    assert_exit("address refusal", output, 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("refused: ") && stderr.contains(reason),
        "expected refused: and {reason:?}, got {stderr}"
    );
}

fn teams() -> (AddressDeck, AgentRecord, Vec<AgentRecord>, Vec<AgentRecord>) {
    let mut fixture = AddressDeck::new();
    let caller = fixture.open_caller("outside-caller");
    let a = fixture.dispatch(&caller, "alpha", true);
    let b = fixture.dispatch(&caller, "beta", true);
    assert_ne!(folder(&a[0]), folder(&b[0]));
    (fixture, caller, a, b)
}

fn assert_delivered_only(fixture: &AddressDeck, target: &AgentRecord, sentinel: &str) -> String {
    assert!(
        common::wait_until(WAIT, || fixture.inbox_text(pane(target)).contains(sentinel)),
        "addressed sentinel never arrived in target inbox"
    );
    for record in fixture.records() {
        if pane(&record) != pane(target) {
            assert!(
                !fixture.inbox_text(pane(&record)).contains(sentinel),
                "addressed sentinel leaked into pane {}",
                pane(&record)
            );
        }
    }
    fixture
        .inbox_bodies(pane(target))
        .into_iter()
        .find(|body| body.contains(sentinel))
        .expect("delivered message body")
}

/// Scenario: Open a standalone caller and two same-named orchestrations in different folders, then run roles from the caller. Check the text and JSON rows identify each seat and the folder and orchestration filters intersect.
#[spec("orchestration/roles/001")]
#[test]
fn orchestration_roles_001_lists_seats_and_filters() {
    let (fixture, caller, a, b) = teams();
    let output = fixture.cli(Some(&caller), folder(&caller), &["roles"]);
    assert_exit("roles listing", &output, 0);
    let text = String::from_utf8_lossy(&output.stdout);
    let header: Vec<_> = text
        .lines()
        .next()
        .expect("header")
        .split_whitespace()
        .collect();
    assert_eq!(
        header,
        [
            "PANE",
            "NAME",
            "FOLDER",
            "ORCHESTRATION",
            "ROLE",
            "ORCHESTRATOR"
        ]
    );
    for record in a.iter().chain(&b).chain(std::iter::once(&caller)) {
        let row = text
            .lines()
            .find(|line| line.split_whitespace().next() == Some(pane(record)))
            .unwrap_or_else(|| panic!("missing pane {} in roles output: {text}", pane(record)));
        assert!(
            row.contains(folder(record).to_str().unwrap()),
            "wrong folder: {row}"
        );
        if record.tab_membership.is_some() {
            assert!(
                row.contains(title(record)),
                "missing orchestration title: {row}"
            );
            assert_eq!(
                row.split_whitespace().last() == Some("yes"),
                is_orchestrator(record),
                "{row}"
            );
        } else {
            assert!(
                row.split_whitespace().filter(|v| *v == "-").count() >= 2,
                "{row}"
            );
        }
    }
    let json = fixture.cli(Some(&caller), folder(&caller), &["roles", "--json"]);
    assert_exit("roles JSON", &json, 0);
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&json.stdout).expect("JSON array");
    assert_eq!(rows.len(), 5);
    let expected: std::collections::BTreeSet<_> = [
        "pane_id",
        "name",
        "folder",
        "orchestration",
        "orchestration_name",
        "orchestration_id",
        "role",
        "orchestrator",
    ]
    .into_iter()
    .collect();
    for record in a.iter().chain(&b).chain(std::iter::once(&caller)) {
        let row = rows
            .iter()
            .find(|row| row["pane_id"] == pane(record))
            .expect("JSON pane row");
        assert_eq!(
            row.as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            expected
        );
        assert!(row["name"].is_string());
        assert_eq!(row["folder"], folder(record).to_str().unwrap());
        assert_eq!(row["orchestrator"], is_orchestrator(record));
        if let Some(TabMembership::Orchestration {
            name,
            role_name,
            orchestration_id,
            ..
        }) = &record.tab_membership
        {
            assert_eq!(row["orchestration"], title(record));
            assert_eq!(row["orchestration_name"], name.as_str());
            assert_eq!(row["role"], role_name.as_str());
            assert_eq!(
                row["orchestration_id"],
                orchestration_id.as_deref().unwrap()
            );
        } else {
            for key in [
                "orchestration",
                "orchestration_name",
                "orchestration_id",
                "role",
            ] {
                assert!(row[key].is_null(), "standalone {key}: {row}");
            }
        }
    }
    for extra in [vec![], vec!["--orchestration", "address-team"]] {
        let mut args = vec![
            "roles",
            "--json",
            "--folder",
            folder(&a[0]).to_str().unwrap(),
        ];
        args.extend(extra);
        let filtered = fixture.cli(Some(&caller), folder(&caller), &args);
        assert_exit("filtered roles", &filtered, 0);
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&filtered.stdout).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|r| a.iter().any(|a| r["pane_id"] == pane(a)))
        );
    }
}

/// Scenario: Open a two-role orchestration with a long start-role name, Working status and a queued inbox at both 220 and 120 terminal columns using the default sidebar width. The orchestrator's live card shows the full ORCHESTRATOR word while the worker's card carries none.
#[spec("orchestration/roles/002")]
#[test]
fn orchestration_roles_002_sidebar_marks_only_orchestrator() {
    for cols in [120, 220] {
        let fixture = AddressDeck::with_sidebar(cols, "orchestrator-with-a-very-long-role-name");
        fixture.deck.send_keys(b"\x0e");
        fixture.deck.send_keys(b" ");
        fixture.deck.wait_for_string("No mode");
        fixture.deck.send_keys(b"\x1b[C");
        fixture.deck.send_keys(b"\r");
        fixture.deck.send_keys(b"\r");
        assert!(common::wait_until(WAIT, || fixture.records().len() == 2));
        let sidebar_width = usize::from(cols) * 34 / 100;
        fixture
            .deck
            .wait_until_grid("orchestrator card badge", |grid| {
                let sidebar: Vec<String> = grid
                    .lines()
                    .map(|line| line.chars().take(sidebar_width).collect())
                    .collect();
                let worker = sidebar
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| line.starts_with(['┏', '┌']))
                    .nth(1)
                    .map(|(index, _)| index);
                worker.is_some_and(|worker| {
                    let leader = sidebar[..worker].join("\n");
                    leader.contains("ORCHESTRATOR")
                        && leader.contains("Working")
                        && leader.contains("queued")
                        && sidebar.join("\n").matches("ORCHESTRATOR").count() == 1
                })
            });
        let grid = fixture.deck.snapshot_grid();
        let sidebar: Vec<String> = grid
            .lines()
            .map(|line| line.chars().take(sidebar_width).collect())
            .collect();
        let cards: Vec<_> = sidebar
            .iter()
            .enumerate()
            .filter(|(_, line)| line.starts_with(['┏', '┌']))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            cards.len(),
            2,
            "expected two sidebar cards at {cols} columns\n{grid}"
        );
        let (leader, worker) = (cards[0], cards[1]);
        let leader_card = sidebar[leader..worker].join("\n");
        assert!(
            leader_card.contains("Working") && leader_card.contains("queued"),
            "expected Working status and queued inbox at {cols} columns\n{grid}"
        );
        assert!(
            sidebar[leader..worker]
                .iter()
                .any(|l| l.contains("ORCHESTRATOR")),
            "badge belongs on the orchestrator card\n{grid}"
        );
        assert!(
            !sidebar[worker..].iter().any(|l| l.contains("ORCHESTRATOR")),
            "worker must not have the badge\n{grid}"
        );
    }
}

/// Scenario: A standalone caller sends a unique message to one of two orchestrations by folder. Only its orchestrator receives it, with the caller's pane in the trusted preface and the sender folder and reply command inside the untrusted frame before the message text.
#[spec("orchestration/address/001")]
#[test]
fn orchestration_address_001_folder_delivers_untrusted_only_to_orchestrator() {
    let (fixture, caller, a, _b) = teams();
    let sentinel = "ADDRESS_ALPHA_53c7e1";
    let output = fixture.cli(
        Some(&caller),
        folder(&caller),
        &[
            "inbox",
            "send",
            "--folder",
            folder(&a[0]).to_str().unwrap(),
            "--text",
            sentinel,
        ],
    );
    assert_exit("send to folder", &output, 0);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("Sent m-")
            && stdout.contains(&format!(
                "to the orchestrator of {} (pane {}) in {}.",
                title(&a[0]),
                pane(&a[0]),
                folder(&a[0]).display()
            )),
        "{stdout}"
    );
    let body = assert_delivered_only(&fixture, &a[0], sentinel);
    let start = body
        .find("[UNTRUSTED-WORKER-REPORT:")
        .expect("untrusted frame opens");
    let end = body
        .find(":END-UNTRUSTED-WORKER-REPORT]")
        .expect("untrusted frame closes");
    assert!(
        body[start..end].contains(sentinel),
        "sentinel must be inside untrusted frame: {body}"
    );
    let preface = &body[..start];
    assert!(
        preface.contains(&format!("pane {}", pane(&caller))),
        "caller pane absent from deck preface: {body}"
    );
    assert!(
        !preface.contains(folder(&caller).to_str().unwrap()),
        "caller folder must not appear outside the untrusted frame: {body}"
    );
    assert!(
        preface.contains("sender details and reply command are inside the untrusted text below"),
        "static attribution guidance absent: {body}"
    );
    let framed = &body[start..end];
    let sender = framed
        .find(&format!("sender folder: {}", folder(&caller).display()))
        .expect("sender folder inside untrusted frame");
    let reply = framed
        .find("reply with: dot-agent-deck inbox send --folder ")
        .expect("reply command inside untrusted frame");
    let message = framed.find(sentinel).expect("sentinel inside frame");
    assert!(
        sender < reply && reply < message,
        "attribution must precede caller text: {body}"
    );
    assert!(
        framed[reply..message].contains(folder(&caller).to_str().unwrap()),
        "reply command addresses caller folder: {body}"
    );
    assert!(
        !preface.contains("reply with:"),
        "reply command outside frame: {body}"
    );
    assert!(
        !body.contains("worker report"),
        "outside caller must not masquerade as a worker: {body}"
    );
}

/// Scenario: Address two same-named orchestrations by name and observe an ambiguous refusal listing both panes. Adding a folder selects one orchestrator, while unknown names and folders are refused.
#[spec("orchestration/address/002")]
#[test]
fn orchestration_address_002_ambiguity_intersection_and_unknown_address() {
    let (fixture, caller, a, b) = teams();
    let output = fixture.cli(
        Some(&caller),
        folder(&caller),
        &[
            "inbox",
            "send",
            "--orchestration",
            "address-team",
            "--text",
            "AMBIGUOUS_6d72",
        ],
    );
    refused(&output, "ambiguous");
    let stderr = String::from_utf8_lossy(&output.stderr);
    for record in [&a[0], &b[0]] {
        for needle in [
            pane(record),
            title(record),
            folder(record).to_str().unwrap(),
            "address-team",
        ] {
            assert!(
                stderr.contains(needle),
                "missing ambiguous candidate {needle}: {stderr}"
            );
        }
    }
    for record in fixture.records() {
        assert!(!fixture.inbox_text(pane(&record)).contains("AMBIGUOUS_6d72"));
    }
    let output = fixture.cli(
        Some(&caller),
        folder(&caller),
        &[
            "inbox",
            "send",
            "--orchestration",
            "address-team",
            "--folder",
            folder(&a[0]).to_str().unwrap(),
            "--text",
            "NARROWED_e91c",
        ],
    );
    assert_exit("name and folder intersect", &output, 0);
    assert_delivered_only(&fixture, &a[0], "NARROWED_e91c");
    let missing = fixture.scratch.path().join("unrelated-missing-folder");
    for (flag, value) in [
        ("--orchestration", "unknown-orchestration"),
        ("--folder", missing.to_str().unwrap()),
    ] {
        let output = fixture.cli(
            Some(&caller),
            folder(&caller),
            &["inbox", "send", flag, value, "--text", "UNKNOWN_d185"],
        );
        refused(&output, "refused:");
    }
}

/// Scenario: A worker attempts to send to its own orchestration and is directed to work-done, while its orchestrator is directed to delegate. A shell without pane identity is also refused, and none of the attempts delivers its text.
#[spec("orchestration/address/003")]
#[test]
fn orchestration_address_003_worker_and_unattested_shell_are_refused() {
    let (fixture, caller, a, _b) = teams();
    let args = [
        "inbox",
        "send",
        "--folder",
        folder(&a[0]).to_str().unwrap(),
        "--text",
        "REFUSED_b834",
    ];
    let output = fixture.cli(Some(&a[1]), folder(&a[1]), &args);
    refused(&output, "work-done");
    let output = fixture.cli(Some(&a[0]), folder(&a[0]), &args);
    refused(&output, "delegate");
    let output = fixture.cli(None, folder(&caller), &args);
    refused(&output, "refused:");
    for record in fixture.records() {
        assert!(!fixture.inbox_text(pane(&record)).contains("REFUSED_b834"));
    }
}

/// Scenario: A caller sends to a folder containing one standalone agent and that agent receives the message. Opening another agent in that folder makes the same address ambiguous and lists both candidates without delivering.
#[spec("orchestration/address/004")]
#[test]
fn orchestration_address_004_standalone_fallback_and_ambiguity() {
    let mut fixture = AddressDeck::new();
    let recipient = fixture.open_caller("reply-recipient");
    let caller = fixture.dispatch(&recipient, "outside", false).remove(0);
    let output = fixture.cli(
        Some(&caller),
        folder(&caller),
        &[
            "inbox",
            "send",
            "--folder",
            folder(&recipient).to_str().unwrap(),
            "--text",
            "STANDALONE_c7e3",
        ],
    );
    assert_exit("standalone fallback", &output, 0);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("Sent m-")
            && stdout.contains(&format!(
                "to reply-recipient (pane {}) in {}.",
                pane(&recipient),
                folder(&recipient).display()
            )),
        "{stdout}"
    );
    assert_delivered_only(&fixture, &recipient, "STANDALONE_c7e3");
    let other = fixture.open_caller("reply-other");
    assert_eq!(folder(&recipient), folder(&other));
    let output = fixture.cli(
        Some(&caller),
        folder(&caller),
        &[
            "inbox",
            "send",
            "--folder",
            folder(&recipient).to_str().unwrap(),
            "--text",
            "STANDALONE_AMBIGUOUS_01bd",
        ],
    );
    refused(&output, "ambiguous");
    for record in [&recipient, &other] {
        assert!(String::from_utf8_lossy(&output.stderr).contains(pane(record)));
        assert!(
            !fixture
                .inbox_text(pane(record))
                .contains("STANDALONE_AMBIGUOUS_01bd")
        );
    }
}
