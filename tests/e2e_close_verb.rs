#![cfg(all(feature = "e2e", unix))]

//! Synthetic, credential-free coverage of the close CLI through a real daemon
//! and attached TUI. The pane probes record their own minted identities outside
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

struct CloseDeck {
    deck: TuiDeck,
    scratch: tempfile::TempDir,
    worktrees: Vec<PathBuf>,
}

impl Drop for CloseDeck {
    fn drop(&mut self) {
        // Dispatch creates siblings outside the fixture TempDir. Reclaim just
        // this test's paths, including when an expected RED assertion unwinds.
        for path in &self.worktrees {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

impl CloseDeck {
    fn new() -> Self {
        let scratch = common::race_safe_tempdir();
        let probe = scratch.path().join("close-probe.py");
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
    input=json.dumps({"hook_event_name": "SessionStart", "session_id": "close-probe-" + identity["DOT_AGENT_DECK_PANE_ID"]}),
    text=True, check=True, stdout=subprocess.DEVNULL)
print("CLOSE-PROBE-READY", flush=True)
os.execvp("cat", ["cat"])
"#,
        )
        .expect("write identity-recording stand-in");
        let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
        let command = format!(
            "python3 -u {} {} {}",
            quote(&probe),
            quote(scratch.path()),
            quote(Path::new(env!("CARGO_BIN_EXE_dot-agent-deck")))
        );
        let encoded = toml::Value::String(command.clone()).to_string();
        let config = scratch.path().join("config.toml");
        std::fs::write(&config, format!("default_command = {encoded}\n"))
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
            .with_env("RUST_LOG", "info")
            .launch_with_fixture("minimal");
        deck.wait_for_string("No active agents");
        std::fs::write(
            deck.workdir().join(".dot-agent-deck.toml"),
            format!(
                "[[orchestrations]]\nname = \"close-team\"\n\n\
                 [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = {encoded}\nstart = true\n\n\
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
                    "--orchestration=close-team"
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

    fn report_done(&self, terminal: &AgentRecord) {
        let marker = format!("close-probe-completed-1589-{}", terminal.id);
        let output = self.cli(
            Some(terminal),
            Path::new(terminal.cwd.as_deref().expect("cwd")),
            &["work-done", "--done", "--task", &marker],
        );
        assert_exit("attested work-done --done setup", &output, 0);
        assert!(
            common::wait_until(WAIT, || self.records().iter().any(|r| {
                !self
                    .worktrees
                    .iter()
                    .any(|p| r.cwd.as_deref().is_some_and(|cwd| Path::new(cwd) == p))
                    && common::pane_search_key_on(self.deck.attach_socket_path(), &r.id)
                        .contains(&marker)
            })),
            "terminal completion never reached caller\nwork-done output: {}\nUnit PTY:\n{}\nDaemon log:\n{}\nGrid:\n{}",
            output_text(&output),
            common::strip_ansi(&common::pane_snapshot_on(
                self.deck.attach_socket_path(),
                &terminal.id
            )),
            std::fs::read_to_string(self.scratch.path().join("daemon.log")).unwrap_or_default(),
            self.deck.snapshot_grid()
        );
    }

    fn close(&self, caller: Option<&AgentRecord>, args: &[&str]) -> Output {
        let mut full = vec!["close"];
        full.extend_from_slice(args);
        self.cli(caller, self.deck.workdir(), &full)
    }

    fn assert_gone(&self, members: &[AgentRecord]) {
        assert!(
            common::wait_until(WAIT, || self
                .records()
                .iter()
                .all(|r| { members.iter().all(|member| member.id != r.id) })),
            "closed agents remain in ListAgents\n{}",
            self.deck.snapshot_grid()
        );
    }

    fn assert_present(&self, members: &[AgentRecord]) {
        let records = self.records();
        assert!(
            members
                .iter()
                .all(|member| records.iter().any(|r| r.id == member.id)),
            "a refusal stopped one of its targets"
        );
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

// Inspect serialized values, without coupling these end-to-end tests to the
// report's unsettled field names or externally/internally tagged enum choice.
fn json_has(value: &serde_json::Value, expected: &str) -> bool {
    match value {
        serde_json::Value::String(s) => s == expected,
        serde_json::Value::Array(items) => items.iter().any(|v| json_has(v, expected)),
        serde_json::Value::Object(fields) => fields
            .iter()
            .any(|(k, v)| k == expected || json_has(v, expected)),
        _ => false,
    }
}

fn assert_refused(output: &Output, reason: &str) {
    assert_exit("close refusal", output, 1);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "close --json did not emit a JSON report\n{}",
            output_text(output)
        )
    });
    assert!(
        json_has(&json, "refused") && json_has(&json, reason),
        "expected refused/{reason}: {json}"
    );
}

/// Scenario: A single stand-in reports completion, then its dispatcher closes
/// it by name. Its card and clean worktree disappear, but its branch remains.
#[spec("dispatch/close-verb/001")]
#[test]
fn close_verb_001_reported_single_closes_and_keeps_branch() {
    let mut test = CloseDeck::new();
    let caller = test.open_caller("caller");
    let members = test.dispatch(&caller, "closed-single", false);
    test.report_done(&members[0]);
    let output = test.close(Some(&caller), &["closed-single"]);
    assert_exit("close reported single", &output, 0);
    assert!(
        output_text(&output).contains("closed"),
        "{}",
        output_text(&output)
    );
    test.assert_gone(&members);
    assert!(
        !test.worktrees[0].exists(),
        "clean dispatched worktree was not removed before close replied"
    );
    let branch = common::fixture_git(test.deck.workdir(), test.deck.workdir())
        .args([
            "show-ref",
            "--verify",
            "refs/heads/agent/dispatch-closed-single",
        ])
        .output()
        .expect("check retained branch");
    assert_exit("retained dispatch branch", &branch, 0);
    test.assert_present(&[caller]);
}

/// Scenario: Close a completed two-role unit by its dispatch name. The reply
/// names every closed pane, with the orchestrator before its worker.
#[spec("dispatch/close-verb/002")]
#[test]
fn close_verb_002_team_closes_every_role_orchestrator_first() {
    let mut test = CloseDeck::new();
    let caller = test.open_caller("caller");
    let members = test.dispatch(&caller, "closed-team", true);
    assert!(is_orchestrator(&members[0]), "missing start role");
    test.report_done(&members[0]);
    let output = test.close(Some(&caller), &["closed-team"]);
    assert_exit("close reported team", &output, 0);
    let text = output_text(&output);
    assert!(text.contains("closed"), "{text}");
    let positions: Vec<_> = members
        .iter()
        .map(|r| {
            let pane = r.pane_id_env.as_deref().expect("pane id");
            text.find(pane)
                .unwrap_or_else(|| panic!("close reply omitted pane {pane}\n{text}"))
        })
        .collect();
    assert!(
        positions[0] < positions[1],
        "orchestrator must be named first\n{text}"
    );
    test.assert_gone(&members);
    test.assert_present(&[caller]);
}

/// Scenario: An unreported unit refuses both name and pane selectors without
/// stopping anything. An explicit force closes it and discloses the override.
#[spec("dispatch/close-verb/003")]
#[test]
fn close_verb_003_unreported_refuses_name_and_pane_then_force_closes() {
    let mut test = CloseDeck::new();
    let caller = test.open_caller("caller");
    let members = test.dispatch(&caller, "unreported", false);
    let output = test.close(Some(&caller), &["unreported", "--json"]);
    assert_refused(&output, "not-reported");
    test.assert_present(&members);
    let output = test.close(
        Some(&caller),
        &[
            "--pane",
            members[0].pane_id_env.as_deref().expect("pane id"),
            "--json",
        ],
    );
    assert_refused(&output, "not-reported");
    test.assert_present(&members);
    let output = test.close(Some(&caller), &["unreported", "--force"]);
    assert_exit("forced close", &output, 0);
    let text = output_text(&output);
    assert!(
        text.contains("closed") && text.contains("forced"),
        "forced close was not disclosed\n{text}"
    );
    test.assert_gone(&members);
}

/// Scenario: A second attested dispatcher tries to close the first one's
/// completed unit, including with force. Both requests refuse and keep it alive.
#[spec("dispatch/close-verb/004")]
#[test]
fn close_verb_004_sibling_dispatcher_cannot_close_anothers_unit() {
    let mut test = CloseDeck::new();
    let caller = test.open_caller("caller");
    let members = test.dispatch(&caller, "owned-unit", false);
    test.report_done(&members[0]);
    test.deck.send_keys(b"\x04");
    test.deck.wait_for_string("COMMAND");
    let sibling = test.open_caller("sibling");
    for args in [
        vec!["owned-unit", "--json"],
        vec!["owned-unit", "--force", "--json"],
    ] {
        let output = test.close(Some(&sibling), &args);
        assert_refused(&output, "not-your-unit");
        test.assert_present(&members);
    }
    test.assert_present(&[caller, sibling]);
}

/// Scenario: With the dispatched orchestration tab visible in an attached TUI,
/// a person shell closes the completed unit. Its tab disappears from that TUI.
#[spec("dispatch/close-verb/005")]
#[test]
fn close_verb_005_person_close_removes_the_attached_tui_tab() {
    let mut test = CloseDeck::new();
    let caller = test.open_caller("caller");
    let members = test.dispatch(&caller, "person-team", true);
    test.report_done(&members[0]);
    let label = match &members[0].tab_membership {
        Some(TabMembership::Orchestration { name, .. }) => name.clone(),
        _ => panic!("dispatched team has no tab membership"),
    };
    assert!(
        common::wait_until(WAIT, || test
            .deck
            .snapshot_grid()
            .lines()
            .take(5)
            .any(|l| l.contains(&label))),
        "the unit tab never appeared\n{}",
        test.deck.snapshot_grid()
    );
    // cli(None) clears all three ambient identity variables: this is a person.
    let output = test.close(None, &["person-team"]);
    assert_exit("person close", &output, 0);
    assert!(
        common::wait_until(WAIT, || !test
            .deck
            .snapshot_grid()
            .lines()
            .take(5)
            .any(|l| l.contains(&label))),
        "CLI-closed tab remains on attached TUI\n{}",
        test.deck.snapshot_grid()
    );
    test.assert_gone(&members);
    test.assert_present(&[caller]);
}

/// Scenario: A completed unit has an uncommitted file in its worktree. Closing
/// stops the agent and reports the worktree kept because of uncommitted changes.
#[spec("dispatch/close-verb/006")]
#[test]
fn close_verb_006_dirty_worktree_is_kept_and_explained() {
    let mut test = CloseDeck::new();
    let caller = test.open_caller("caller");
    let members = test.dispatch(&caller, "dirty-unit", false);
    test.report_done(&members[0]);
    let dirty = test.worktrees[0].join("uncommitted-close-sentinel.txt");
    std::fs::write(&dirty, "uncommitted work survives close").expect("dirty worktree");
    let output = test.close(Some(&caller), &["dirty-unit"]);
    assert_exit("close dirty unit", &output, 0);
    let text = output_text(&output).to_lowercase();
    assert!(
        text.contains("closed") && text.contains("kept") && text.contains("uncommitted"),
        "{text}"
    );
    test.assert_gone(&members);
    assert_eq!(
        std::fs::read_to_string(dirty).expect("kept dirty file"),
        "uncommitted work survives close"
    );
}

/// Scenario: Stop a completed team's orchestrator through the existing stop
/// path, then close the dispatch by name. Its remaining worker still resolves
/// and closes, and the caller stays alive.
#[spec("dispatch/close-verb/007")]
#[test]
fn close_verb_007_team_still_resolves_after_orchestrator_stops() {
    let mut test = CloseDeck::new();
    let caller = test.open_caller("caller");
    let members = test.dispatch(&caller, "surviving-team", true);
    test.report_done(&members[0]);
    let stopped = common::attach_request_on(
        test.deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: members[0].id.clone(),
        },
    )
    .expect("existing StopAgent reply");
    assert!(stopped.ok, "StopAgent setup refused: {:?}", stopped.error);
    test.assert_gone(&members[..1]);
    test.assert_present(&members[1..]);
    let output = test.close(Some(&caller), &["surviving-team"]);
    assert_exit("close surviving workers by unit name", &output, 0);
    assert!(
        output_text(&output).contains("closed"),
        "{}",
        output_text(&output)
    );
    test.assert_gone(&members);
    test.assert_present(&[caller]);
}

/// Scenario: A dispatcher previews two completed single units with close --all,
/// leaving both running. Confirming with --all --yes closes both and removes
/// their clean worktrees while keeping the dispatcher alive.
#[spec("dispatch/close-verb/009")]
#[test]
fn close_verb_009_all_yes_closes_the_previewed_units() {
    let mut test = CloseDeck::new();
    let caller = test.open_caller("caller");
    let first = test.dispatch(&caller, "bulk-first", false);
    let second = test.dispatch(&caller, "bulk-second", false);
    test.report_done(&first[0]);
    test.report_done(&second[0]);

    let preview = test.close(Some(&caller), &["--all", "--json"]);
    assert_exit("preview all reported units", &preview, 0);
    let json: serde_json::Value = serde_json::from_slice(&preview.stdout).expect("preview JSON");
    let listed = json["listed"].as_array().expect("listed units");
    assert_eq!(listed.len(), 2, "preview must list both units: {json}");
    for name in ["bulk-first", "bulk-second"] {
        assert!(
            listed.iter().any(|unit| unit["name"] == name),
            "preview omitted {name}: {json}"
        );
    }
    assert_eq!(json["closed"], serde_json::json!([]), "{json}");
    test.assert_present(&first);
    test.assert_present(&second);
    assert!(test.worktrees.iter().all(|path| path.exists()));

    let output = test.close(Some(&caller), &["--all", "--yes", "--json"]);
    assert_exit("confirmed bulk close", &output, 0);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("bulk close JSON");
    let closed = json["closed"].as_array().expect("closed units");
    assert_eq!(closed.len(), 2, "bulk close must close both units: {json}");
    for unit in listed {
        assert!(
            closed
                .iter()
                .any(|entry| entry["unit_id"] == unit["unit_id"]),
            "bulk close omitted a previewed unit: {json}"
        );
    }
    test.assert_gone(&first);
    test.assert_gone(&second);
    assert!(
        test.worktrees.iter().all(|path| !path.exists()),
        "confirmed bulk close left a clean worktree behind: {json}"
    );
    test.assert_present(&[caller]);
}

/// Scenario: A completed single unit is closed by its pane id. Its clean
/// worktree disappears, the JSON reply reports removed, and its branch remains.
#[spec("dispatch/close-verb/010")]
#[test]
fn close_verb_010_pane_close_removes_the_single_units_worktree() {
    let mut test = CloseDeck::new();
    let caller = test.open_caller("caller");
    let members = test.dispatch(&caller, "pane-cleanup", false);
    test.report_done(&members[0]);
    let output = test.close(
        Some(&caller),
        &[
            "--pane",
            members[0].pane_id_env.as_deref().expect("pane id"),
            "--json",
        ],
    );
    assert_exit("close reported single by pane", &output, 0);
    test.assert_gone(&members);
    assert!(
        !test.worktrees[0].exists(),
        "pane close left the clean single-unit worktree behind\n{}",
        output_text(&output)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("pane close JSON");
    let verdicts = json["worktrees"].as_array().expect("worktree verdicts");
    assert_eq!(verdicts.len(), 1, "pane close must report cleanup: {json}");
    assert_eq!(verdicts[0]["verdict"], "removed", "{json}");
    assert_eq!(
        verdicts[0]["path"],
        test.worktrees[0].to_string_lossy().as_ref(),
        "{json}"
    );
    let branch = common::fixture_git(test.deck.workdir(), test.deck.workdir())
        .args([
            "show-ref",
            "--verify",
            "refs/heads/agent/dispatch-pane-cleanup",
        ])
        .output()
        .expect("check retained branch");
    assert_exit("retained pane-closed dispatch branch", &branch, 0);
    test.assert_present(&[caller]);
}

/// Scenario: A dispatcher lists two completed single units, then closes one
/// using the stable unit id from that JSON listing. Only the selected unit and
/// its worktree disappear; the other unit stays open.
#[spec("dispatch/close-verb/011")]
#[test]
fn close_verb_011_unit_id_closes_only_the_selected_unit() {
    let mut test = CloseDeck::new();
    let caller = test.open_caller("caller");
    let selected = test.dispatch(&caller, "id-selected", false);
    let other = test.dispatch(&caller, "id-other", false);
    test.report_done(&selected[0]);
    test.report_done(&other[0]);
    let preview = test.close(Some(&caller), &["--all", "--json"]);
    assert_exit("list unit ids", &preview, 0);
    let json: serde_json::Value = serde_json::from_slice(&preview.stdout).expect("listing JSON");
    let listed = json["listed"].as_array().expect("listed units");
    assert_eq!(listed.len(), 2, "listing must contain both units: {json}");
    let id = listed
        .iter()
        .find(|unit| unit["name"] == "id-selected")
        .and_then(|unit| unit["unit_id"].as_str())
        .expect("selected unit's stable id");
    let output = test.close(Some(&caller), &["--unit-id", id, "--json"]);
    assert_exit("close selected stable unit id", &output, 0);
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("unit-id close JSON");
    let closed = json["closed"].as_array().expect("closed units");
    assert_eq!(
        closed.len(),
        1,
        "unit-id close must select one unit: {json}"
    );
    assert_eq!(closed[0]["unit_id"], id, "{json}");
    test.assert_gone(&selected);
    test.assert_present(&other);
    assert!(!test.worktrees[0].exists(), "{json}");
    assert!(test.worktrees[1].exists(), "{json}");
    test.assert_present(&[caller]);
}
