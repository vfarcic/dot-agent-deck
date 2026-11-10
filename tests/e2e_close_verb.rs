#![cfg(all(feature = "e2e", unix))]

//! Synthetic, credential-free coverage of the close CLI through a real daemon
//! and attached TUI. The pane probes record their own minted identities outside
//! the checkout; tokens are never printed or included in assertion diagnostics.

dot_agent_deck_test_harness::link!();

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
        // PRD #1666: the completion is posted to the caller's inbox, not typed
        // into its pane.
        assert!(
            common::wait_until(WAIT, || self.records().iter().any(|r| {
                !self
                    .worktrees
                    .iter()
                    .any(|p| r.cwd.as_deref().is_some_and(|cwd| Path::new(cwd) == p))
                    && r.pane_id_env
                        .as_deref()
                        .is_some_and(|pane| self.inbox_text(pane).contains(&marker))
            })),
            "terminal completion never reached caller's inbox\nwork-done output: {}\nUnit PTY:\n{}\nDaemon log:\n{}\nGrid:\n{}",
            output_text(&output),
            common::strip_ansi(&common::pane_snapshot_on(
                self.deck.attach_socket_path(),
                &terminal.id
            )),
            std::fs::read_to_string(self.scratch.path().join("daemon.log")).unwrap_or_default(),
            self.deck.snapshot_grid()
        );
    }

    /// Every message in `pane`'s inbox, rendered whole, joined.
    fn inbox_text(&self, pane: &str) -> String {
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
        .collect::<Vec<_>>()
        .join("\n")
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
            keep_worktree: false,
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

/// Issue #1958: the rows of the close dialog's three choices — the selected
/// `Cancel`, the ordinary `Close` option row (a `Close` row with a ` — `
/// description that is not the keep choice) and `Close, keep worktree` — when
/// all three are drawn top to bottom in that order.
fn close_choice_rows(grid: &str) -> Option<(usize, usize, usize)> {
    let lines: Vec<&str> = grid.lines().collect();
    let row = |pred: &dyn Fn(&str) -> bool| lines.iter().position(|l| pred(l));
    let cancel = row(&|l| l.contains("> Cancel"))?;
    let close =
        row(&|l| l.contains("Close") && l.contains('\u{2014}') && !l.contains("keep worktree"))?;
    let keep = row(&|l| l.contains("Close, keep worktree"))?;
    (cancel < close && close < keep).then_some((cancel, close, keep))
}

/// `git status --porcelain` of a dispatched worktree, trimmed.
fn porcelain(dir: &Path) -> String {
    let out = common::fixture_git(dir, dir)
        .args(["status", "--porcelain"])
        .output()
        .expect("git available");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Issue #1958: put a file in the worktree's git-ignored `.dot-agent-deck/`,
/// the deck's own coordination directory, and prove the tree is still CLEAN —
/// so a close that keeps it is keeping it on request, not because it is dirty.
fn plant_ignored_sentinel(worktree: &Path, name: &str) -> PathBuf {
    // A real project ignores the directory (its `.gitignore`, or the clone-local
    // `info/exclude` rule the deck adds when it publishes orchestration context).
    // The `minimal` fixture has neither until a team is dispatched, so add the
    // clone-local rule here, as the deck would. `--git-path` resolves to the
    // shared exclude file that the linked worktree reads.
    let out = common::fixture_git(worktree, worktree)
        .args(["rev-parse", "--git-path", "info/exclude"])
        .output()
        .expect("git available");
    let exclude = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let exclude = if exclude.is_absolute() {
        exclude
    } else {
        worktree.join(exclude)
    };
    let mut rules = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !rules.lines().any(|l| l.trim() == "/.dot-agent-deck/") {
        rules.push_str("\n/.dot-agent-deck/\n");
        std::fs::create_dir_all(exclude.parent().expect("exclude has a parent"))
            .expect("create the info dir");
        std::fs::write(&exclude, rules).expect("ignore .dot-agent-deck/");
    }
    let dir = worktree.join(".dot-agent-deck");
    std::fs::create_dir_all(&dir).expect("create the worktree's .dot-agent-deck");
    let sentinel = dir.join(name);
    std::fs::write(&sentinel, "kept on request").expect("write the ignored sentinel");
    assert_eq!(
        porcelain(worktree),
        "",
        "premise: the dispatched worktree must be clean (its .dot-agent-deck/ \
         ignored) so that keeping it can only be the user's request"
    );
    sentinel
}

fn assert_kept_with_sentinel(worktree: &Path, sentinel: &Path, context: &str) {
    assert!(
        worktree.is_dir(),
        "a worktree closed with keep was removed: {}\n{context}",
        worktree.display()
    );
    assert_eq!(
        std::fs::read_to_string(sentinel).unwrap_or_default(),
        "kept on request",
        "the kept worktree lost its .dot-agent-deck/ contents\n{context}"
    );
}

/// Scenario: A clean single unit is dispatched and its card selected in the
/// attached TUI. Ctrl+W offers Cancel, Close and `Close, keep worktree`;
/// Down, Down, Enter closes the agent but leaves the worktree and its ignored
/// `.dot-agent-deck/` on disk, and the status line says it was kept on request
/// and where.
#[spec("prompt/close-confirm/012")]
#[test]
fn close_confirm_012_ctrl_w_keep_worktree_keeps_a_clean_dispatched_tree() {
    let mut test = CloseDeck::new();
    let caller = test.open_caller("caller");
    let members = test.dispatch(&caller, "kept-tui", false);
    let worktree = test.worktrees[0].clone();
    let sentinel = plant_ignored_sentinel(&worktree, "keep-sentinel-1958.txt");
    let card = members[0]
        .display_name
        .clone()
        .expect("the dispatched unit's card name");

    // Close the caller card first so the dispatched card is the selected one.
    test.deck.send_keys(b"\x04");
    test.deck.wait_for_string("COMMAND");
    test.deck.send_keys(b"\x17");
    test.deck.wait_for_string("Close selected agent?");
    test.deck.send_keys(b"\x1b[B");
    test.deck.send_keys(b"\r");
    test.assert_gone(std::slice::from_ref(&caller));
    test.deck
        .wait_until_grid("the dispatched card is the selected one", |grid| {
            grid.lines()
                .any(|l| l.contains('\u{25b8}') && l.contains(card.as_str()))
        });

    test.deck.send_keys(b"\x17");
    test.deck.wait_for_string("Close selected agent?");
    test.deck.wait_until_grid(
        "the dialog offers Cancel, Close and `Close, keep worktree`, in that order",
        |grid| close_choice_rows(grid).is_some(),
    );
    test.deck.send_keys(b"\x1b[B");
    test.deck.send_keys(b"\x1b[B");
    test.deck.send_keys(b"\r");

    let path = worktree.to_string_lossy().into_owned();
    test.deck.wait_until_grid(
        "the status line says the worktree was kept on request, with its path",
        |grid| grid.contains("Worktree kept on request:") && grid.contains(path.as_str()),
    );
    test.assert_gone(&members);
    assert_kept_with_sentinel(&worktree, &sentinel, &test.deck.snapshot_grid());
}

/// Scenario: A clean two-role unit is dispatched and its tab opened in the
/// attached TUI. Ctrl+W on the tab offers Cancel, Close and `Close, keep
/// worktree`; Down, Down, Enter stops both roles and removes the tab but leaves
/// the shared worktree and its ignored `.dot-agent-deck/` on disk, and the
/// status line says it was kept on request and where.
#[spec("prompt/close-confirm/013")]
#[test]
fn close_confirm_013_ctrl_w_tab_keep_worktree_keeps_a_teams_shared_tree() {
    let mut test = CloseDeck::new();
    let caller = test.open_caller("caller");
    let members = test.dispatch(&caller, "kept-tab", true);
    let worktree = test.worktrees[0].clone();
    let sentinel = plant_ignored_sentinel(&worktree, "keep-sentinel-tab.txt");
    let label = match &members[0].tab_membership {
        Some(TabMembership::Orchestration { name, .. }) => name.clone(),
        _ => panic!("dispatched team has no tab membership"),
    };
    test.deck.wait_until_grid("the unit's tab appears", |grid| {
        grid.lines().take(5).any(|l| l.contains(&label))
    });

    test.deck.send_keys(b"\x04");
    test.deck.wait_for_string("COMMAND");
    test.deck.send_keys(b"\x1b[C");
    // The tab shows its two roles of the three sessions (the caller's is the
    // third).
    test.deck.wait_for_string("2/3 agent(s)");
    test.deck.send_keys(b"\x17");
    test.deck
        .wait_for_string("Close this tab and all its agents?");
    test.deck.wait_until_grid(
        "the tab dialog offers Cancel, Close and `Close, keep worktree`, in that order",
        |grid| close_choice_rows(grid).is_some(),
    );
    test.deck.send_keys(b"\x1b[B");
    test.deck.send_keys(b"\x1b[B");
    test.deck.send_keys(b"\r");

    let path = worktree.to_string_lossy().into_owned();
    test.deck.wait_until_grid(
        "the status line says the shared worktree was kept on request, with its path",
        |grid| grid.contains("Worktree kept on request:") && grid.contains(path.as_str()),
    );
    test.assert_gone(&members);
    test.deck.wait_until_grid("the unit's tab is gone", |grid| {
        !grid.lines().take(5).any(|l| l.contains(&label))
    });
    assert_kept_with_sentinel(&worktree, &sentinel, &test.deck.snapshot_grid());
    test.assert_present(&[caller]);
}

/// Scenario: A completed clean single unit is closed by name with
/// `--keep-worktree`, then a second one with `--keep-worktree --json`. Both
/// agents go, both worktrees and their ignored `.dot-agent-deck/` stay, the
/// human report says `kept on request` with the path and how to reclaim it,
/// and the JSON verdict is `kept-on-request`.
#[spec("dispatch/close-verb/012")]
#[test]
fn close_verb_012_keep_worktree_keeps_a_clean_units_tree() {
    let mut test = CloseDeck::new();
    let caller = test.open_caller("caller");
    let human = test.dispatch(&caller, "kept-human", false);
    let json_unit = test.dispatch(&caller, "kept-json", false);
    test.report_done(&human[0]);
    test.report_done(&json_unit[0]);
    let human_tree = test.worktrees[0].clone();
    let json_tree = test.worktrees[1].clone();
    let human_sentinel = plant_ignored_sentinel(&human_tree, "keep-sentinel-human.txt");
    let json_sentinel = plant_ignored_sentinel(&json_tree, "keep-sentinel-json.txt");

    let output = test.close(Some(&caller), &["kept-human", "--keep-worktree"]);
    assert_exit("close --keep-worktree", &output, 0);
    let text = output_text(&output);
    assert!(
        text.contains(&format!("{}: kept on request", human_tree.display())),
        "the human report must say the worktree was kept on request, with its path\n{text}"
    );
    assert!(
        text.contains("dot-agent-deck worktree reclaim"),
        "the report must say how to find the kept worktree later\n{text}"
    );
    test.assert_gone(&human);
    assert_kept_with_sentinel(&human_tree, &human_sentinel, &text);

    let output = test.close(Some(&caller), &["kept-json", "--keep-worktree", "--json"]);
    assert_exit("close --keep-worktree --json", &output, 0);
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("keep-worktree close JSON");
    let verdicts = json["worktrees"].as_array().expect("worktree verdicts");
    assert_eq!(verdicts.len(), 1, "one worktree verdict: {json}");
    assert_eq!(verdicts[0]["verdict"], "kept-on-request", "{json}");
    assert_eq!(
        verdicts[0]["path"],
        json_tree.to_string_lossy().as_ref(),
        "{json}"
    );
    test.assert_gone(&json_unit);
    assert_kept_with_sentinel(&json_tree, &json_sentinel, &json.to_string());
    test.assert_present(&[caller]);
}

/// Scenario: A completed clean two-role unit is closed with
/// `close --orchestration-of <pane> --keep-worktree --json`. Both roles go,
/// the worktree they shared and its ignored `.dot-agent-deck/` stay, and the
/// JSON reports it `kept-on-request`.
#[spec("dispatch/close-verb/013")]
#[test]
fn close_verb_013_orchestration_of_keep_worktree_keeps_the_shared_tree() {
    let mut test = CloseDeck::new();
    let caller = test.open_caller("caller");
    let members = test.dispatch(&caller, "kept-team", true);
    test.report_done(&members[0]);
    let worktree = test.worktrees[0].clone();
    let sentinel = plant_ignored_sentinel(&worktree, "keep-sentinel-team.txt");

    let output = test.close(
        Some(&caller),
        &[
            "--orchestration-of",
            members[1].pane_id_env.as_deref().expect("worker pane id"),
            "--keep-worktree",
            "--json",
        ],
    );
    assert_exit("close --orchestration-of --keep-worktree", &output, 0);
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("orchestration keep close JSON");
    let verdicts = json["worktrees"].as_array().expect("worktree verdicts");
    assert_eq!(
        verdicts.len(),
        1,
        "the shared worktree is reported once: {json}"
    );
    assert_eq!(verdicts[0]["verdict"], "kept-on-request", "{json}");
    assert_eq!(
        verdicts[0]["path"],
        worktree.to_string_lossy().as_ref(),
        "{json}"
    );
    test.assert_gone(&members);
    assert_kept_with_sentinel(&worktree, &sentinel, &json.to_string());
    test.assert_present(&[caller]);
}
