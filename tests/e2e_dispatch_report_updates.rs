#![cfg(all(feature = "e2e", unix))]

//! Synthetic, credential-free coverage of a dispatched unit's updates to its
//! `work-done` report through the real binary: a real daemon with an attached
//! TUI, a dispatcher pane opened from the TUI, a real `dispatch --single`, and
//! the unit's own `work-done --done` calls. The panes are identity-recording
//! stand-ins that announce themselves as Claude Code and exec `cat`; their
//! tokens are never printed or included in assertion diagnostics.

dot_agent_deck_test_harness::link!();

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::AgentRecord;
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::dispatch_return::{LOOPING_NOTE, UPDATE_OPENING};
use dot_agent_deck::event::AgentType;
use dot_agent_deck::report_recovery::{MAX_REPORT_UPDATES, USUAL_REPORT_UPDATES};
use spec::spec;

const WAIT: Duration = Duration::from_secs(60);

struct UpdateDeck {
    deck: TuiDeck,
    scratch: tempfile::TempDir,
    worktrees: Vec<PathBuf>,
}

impl Drop for UpdateDeck {
    fn drop(&mut self) {
        // Dispatch creates siblings outside the fixture TempDir. Reclaim just
        // this test's paths, including when an assertion unwinds.
        for path in &self.worktrees {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

impl UpdateDeck {
    fn new() -> Self {
        let scratch = common::race_safe_tempdir();
        let probe = scratch.path().join("update-probe.py");
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
    input=json.dumps({"hook_event_name": "SessionStart", "session_id": "update-probe-" + identity["DOT_AGENT_DECK_PANE_ID"]}),
    text=True, check=True, stdout=subprocess.DEVNULL)
print("UPDATE-PROBE-READY", flush=True)
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
        let encoded = toml::Value::String(command).to_string();
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

    /// The dispatcher, opened the way a user opens a pane: from the TUI's
    /// New Agent dialog.
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

    fn cli(&self, as_pane: &AgentRecord, cwd: &Path, args: &[&str]) -> Output {
        let identity: std::collections::BTreeMap<String, String> = serde_json::from_slice(
            &std::fs::read(self.identity_path(as_pane)).expect("read private stand-in identity"),
        )
        .expect("parse private stand-in identity");
        Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.deck.home_dir())
            .env("DOT_AGENT_DECK_SOCKET", self.deck.hook_socket_path())
            .env(
                "DOT_AGENT_DECK_ATTACH_SOCKET",
                self.deck.attach_socket_path(),
            )
            .envs(identity)
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("run real deck CLI")
    }

    fn dispatch_single(&mut self, caller: &AgentRecord, name: &str) -> AgentRecord {
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
            caller,
            self.deck.workdir(),
            &[
                "dispatch",
                name,
                "--task",
                "Wait for the test to report.",
                "--single",
            ],
        );
        assert_exit("dispatch setup", &output, 0);
        let unit = || {
            self.records().into_iter().find(|r| {
                r.cwd
                    .as_deref()
                    .is_some_and(|cwd| Path::new(cwd) == worktree)
                    && self.probe_ready(r)
            })
        };
        assert!(
            common::wait_until(WAIT, || unit().is_some()),
            "dispatched unit never became ready\n{}",
            self.deck.snapshot_grid()
        );
        unit().expect("ready unit")
    }

    /// The unit's `work-done --done` with `report`, retried while the deck is
    /// still recording the dispatch (`report_pending`), as an agent retries.
    fn report(&self, unit: &AgentRecord, report: &str) -> Output {
        let cwd = PathBuf::from(unit.cwd.as_deref().expect("unit cwd"));
        let run = || self.cli(unit, &cwd, &["work-done", "--done", "--task", report]);
        let last = std::cell::RefCell::new(None);
        common::wait_until(WAIT, || {
            let output = run();
            let pending = output.status.code() == Some(1)
                && output_text(&output).contains("[report_pending]");
            *last.borrow_mut() = Some(output);
            !pending
        });
        last.into_inner().expect("the report ran")
    }

    /// Every message in `pane`'s inbox, rendered whole.
    fn inbox(&self, pane: &str) -> Vec<String> {
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

    /// The caller's inbox messages carrying `needle`, once at least `count`
    /// of them arrived (or the wait ran out).
    fn delivered(&self, caller: &AgentRecord, needle: &str, count: usize) -> Vec<String> {
        let pane = caller.pane_id_env.as_deref().expect("caller pane id");
        let carrying = || {
            self.inbox(pane)
                .into_iter()
                .filter(|text| text.contains(needle))
                .collect::<Vec<_>>()
        };
        common::wait_until(WAIT, || carrying().len() >= count);
        carrying()
    }

    fn daemon_log(&self) -> String {
        std::fs::read_to_string(self.scratch.path().join("daemon.log")).unwrap_or_default()
    }
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

/// Scenario: Start the deck, open a dispatcher pane from the New Agent dialog and dispatch a single stand-in unit from it. The unit reports, then sends four different updates with `work-done --done`; the dispatcher's inbox receives each update once as its own numbered update, the first three without a note and the fourth saying the unit has sent more updates than usual and may be looping. Sending the fourth update again delivers nothing new.
#[spec("dispatch/return/034")]
#[test]
fn return_034_an_update_past_the_usual_count_reaches_the_dispatcher_flagged() {
    let mut test = UpdateDeck::new();
    let caller = test.open_caller("dispatcher");
    let unit = test.dispatch_single(&caller, "updating-unit");

    let first = test.report(&unit, "R034-FIRST-REPORT");
    assert_exit("the first report", &first, 0);
    assert_eq!(
        test.delivered(&caller, "R034-FIRST-REPORT", 1).len(),
        1,
        "the first report reaches the dispatcher once\n{}",
        test.daemon_log()
    );

    let flagged = USUAL_REPORT_UPDATES + 1;
    assert!(flagged <= MAX_REPORT_UPDATES);
    for n in 1..=flagged {
        let body = format!("R034-UPDATE-{n}-OF-THE-REPORT");
        let output = test.report(&unit, &body);
        assert_exit(&format!("update {n}"), &output, 0);
        assert!(
            output_text(&output).contains(&format!("Sent update {n} to this unit's report")),
            "{}",
            output_text(&output)
        );
        let posts = test.delivered(&caller, &body, 1);
        assert_eq!(
            posts.len(),
            1,
            "update {n} reaches the dispatcher once: {posts:?}\n{}",
            test.daemon_log()
        );
        let post = &posts[0];
        assert!(
            post.contains(UPDATE_OPENING)
                && post.contains(&format!("update {n} of at most {MAX_REPORT_UPDATES}.")),
            "{post}"
        );
        assert_eq!(
            post.contains(LOOPING_NOTE),
            n > USUAL_REPORT_UPDATES,
            "update {n}: {post}"
        );
    }

    // An identical repeat of the flagged update is answered and delivers
    // nothing new.
    let body = format!("R034-UPDATE-{flagged}-OF-THE-REPORT");
    let again = test.report(&unit, &body);
    assert_exit("a repeat of the flagged update", &again, 0);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(test.delivered(&caller, &body, 1).len(), 1);
    assert_eq!(test.delivered(&caller, UPDATE_OPENING, 0).len(), flagged);
}
