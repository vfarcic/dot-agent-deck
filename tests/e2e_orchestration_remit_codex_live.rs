#![cfg(all(feature = "e2e", feature = "e2e-live", target_os = "linux"))]

//! PRD #82, issue #2435: a REAL interactive Codex orchestrator is compacted
//! with `/compact`, and the deck's re-assert of its role reaches the
//! conversation that follows the summary, not the one the summary replaced
//! (the Codex counterpart of `orchestration/remit/012` and `014`, in
//! `tests/e2e_orchestration_remit_live.rs`).
//!
//! It runs in its own file because a Codex orchestrator runs in
//! `codex_installed::project_dir`, a folder that does not hold the test HOME,
//! so the deck can check its input box and ring the doorbell that hands it
//! its role (`support/codex_installed.rs` says why).

#[path = "support/codex_installed.rs"]
mod codex_installed;
dot_agent_deck_test_harness::link!();
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/claude_installed.rs"]
mod installed;
#[path = "support/codex_inbox_live.rs"]
mod live;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::{AgentRecord, TabMembership};
use dot_agent_deck::event::EventType;
use spec::spec;

/// The file the role text appends one line to per read of the role.
const READS: &str = "remit_reads_2435_c0d3.txt";
const LOG: &str = "remit-codex-live.log";
const ORCHESTRATION: &str = "remit-compaction-codex";

/// Each step's wait: one model round trip (a startup turn, a compaction's
/// summary call, the turn after it) plus Codex's start-up. Kept inside the
/// test's nextest window (`.config/nextest.toml`), so a step that never
/// happens fails with its evidence rather than being killed without any.
const BUDGET: Duration = Duration::from_secs(120);

fn reads(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join(READS))
        .map(|text| text.lines().filter(|l| l.trim() == "REMIT-READ").count())
        .unwrap_or(0)
}

/// Every per-publish context file in the project's `.dot-agent-deck`.
fn context_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.join(".dot-agent-deck"))
        .map(|dir| {
            dir.filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with("orchestrator-context-") && n.ends_with(".md"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Every Codex rollout under the test HOME's `.codex/sessions`.
fn rollouts(home: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, found);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
            {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(&home.join(".codex").join("sessions"), &mut found);
    found.sort();
    found
}

/// A rollout line's kind, `type/payload.type`, with a message's role.
fn kind(entry: &serde_json::Value) -> String {
    let field = |v: &serde_json::Value, k: &str| {
        v.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string()
    };
    let payload = entry.get("payload").cloned().unwrap_or_default();
    let role = field(&payload, "role");
    format!(
        "{}/{}{}",
        field(entry, "type"),
        field(&payload, "type"),
        if role.is_empty() {
            String::new()
        } else {
            format!(":{role}")
        }
    )
}

fn orchestrator(deck: &TuiDeck) -> AgentRecord {
    let is_start = |record: &AgentRecord| {
        matches!(
            record.tab_membership,
            Some(TabMembership::Orchestration {
                is_start_role: true,
                ..
            })
        )
    };
    assert!(
        common::wait_until(BUDGET, || {
            common::agent_records_on(deck.attach_socket_path())
                .iter()
                .any(|r| is_start(r) && r.pane_id_env.is_some())
        }),
        "the orchestrator did not spawn\n{}",
        deck.snapshot_grid()
    );
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(is_start)
        .expect("orchestrator record")
}

/// The orchestration: a real interactive Codex orchestrator, started as a
/// plain `codex …` argv (the `Direct(Codex)` class) whose role text makes
/// every read of its role file one line in [`READS`], and a `cat` worker.
fn write_orchestration(deck: &TuiDeck) {
    let dir = codex_installed::project_dir(deck);
    let role = format!(
        "This orchestration keeps a read log for the deck while it checks that coordinators \
         keep their role, and that log is all the work there is, so there is nothing to \
         delegate and no work-done to send. Each time you read this role file, including when \
         the deck asks you to read it again later, record the read by running this command \
         once with the shell: echo REMIT-READ >> '{}' and then reply REMIT-ACK- followed \
         immediately by 2435 with no space, and end your turn. Create or edit no other file.",
        dir.join(READS).display()
    );
    std::fs::write(
        dir.join(".dot-agent-deck.toml"),
        format!(
            "[[orchestrations]]\nname = \"{ORCHESTRATION}\"\n\n[[orchestrations.roles]]\nname = \
             \"orchestrator\"\ncommand = {:?}\nstart = true\nprompt_template = {role:?}\n\n\
             [[orchestrations.roles]]\nname = \"worker\"\ncommand = \"cat\"\n",
            codex_installed::command()
        ),
    )
    .unwrap();
    deck.approve_project_trust(&dir);
}

/// Scenario: Open a real interactive Codex orchestrator (the cheap test model) from the TUI in a project folder, and wait until it has read its role file once and ended its turn. Type /compact into its pane. The deck sees the compaction, publishes a fresh role file and posts the re-assert; after the summary, the orchestrator reads that new file and records a second read, and its rollout names the new file only after the compaction record, first as a message and then in a tool call.
#[spec("orchestration/remit/015")]
#[test]
fn orchestration_remit_015_real_codex_compaction_reassert_lands_after_the_summary() {
    skip_unless!(codex_installed::checkable());
    let release = codex_installed::installed_release().unwrap();
    eprintln!("orchestration/remit/015: Codex {release}");
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_codex_credentials()
        .with_env("PATH", common::path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_LOG", LOG)
        .launch_with_fixture("minimal");
    assert!(deck.wait_for_grid_string_within("No active agents", BUDGET));
    // The deck's Codex hooks trusted in the sandbox's `CODEX_HOME`, as an
    // install the user ran earlier would have left them.
    live::prepare(&deck);
    let dir = codex_installed::project_dir(&deck);
    write_orchestration(&deck);

    // Open the orchestration from the TUI's new-pane form, in the project
    // folder, as `chain-smoke/codex/001` does.
    let events = deck.subscribe_events();
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b"/project");
    deck.wait_for_string("project/");
    deck.send_keys(b"\r");
    deck.send_keys(b"j");
    deck.send_keys(b"\r");
    deck.wait_for_absence("project/");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.wait_for_absence("Command:");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.wait_for_absence("[Submit]");
    let coordinator = orchestrator(&deck);
    let pane = coordinator.pane_id_env.clone().expect("orchestrator pane");
    let log = || std::fs::read_to_string(deck.workdir().join(LOG)).unwrap_or_default();
    let evidence = || {
        common::redact_credentials_for_output(&format!(
            "{}\nevents: {:?}\ncontext files: {:?}\nlog tail:\n{}",
            common::pane_search_key_on(deck.attach_socket_path(), &coordinator.id),
            events
                .snapshot()
                .iter()
                .filter(|e| e.pane_id.as_deref() == Some(pane.as_str()))
                .map(|e| (e.event_type.clone(), e.tool_detail.clone()))
                .collect::<Vec<_>>(),
            context_files(&dir),
            installed::tail(&log(), 80)
        ))
    };

    // The startup read: the spawn-time pointer reached Codex through its
    // inbox and it followed the role file once.
    assert!(
        common::wait_until(BUDGET, || reads(&dir) >= 1),
        "the orchestrator never recorded reading its role file\n{}",
        evidence()
    );
    // Its startup turn has ended (Codex's `Stop` hook).
    assert!(
        common::wait_until(BUDGET, || events.snapshot().iter().rev().any(|e| {
            e.pane_id.as_deref() == Some(pane.as_str())
                && e.event_type == EventType::Idle
                && !e.is_from_subagent()
        })),
        "the orchestrator's startup turn never ended\n{}",
        evidence()
    );
    let before = context_files(&dir);
    assert_eq!(reads(&dir), 1, "one read before the compaction");
    let seen = events.snapshot().len();

    let mut input = common::AttachInput::attach(deck.attach_socket_path(), &coordinator.id);
    input.write_steps([
        ("/compact", Duration::from_millis(800)),
        ("\r", Duration::from_millis(200)),
    ]);

    // Codex's `PreCompact` hook reaches the deck as `Compacting`.
    assert!(
        common::wait_until(BUDGET, || events.snapshot().iter().skip(seen).any(|e| {
            e.pane_id.as_deref() == Some(pane.as_str()) && e.event_type == EventType::Compacting
        })),
        "Codex's compaction never reached the deck as Compacting\n{}",
        evidence()
    );
    // The re-assert published exactly one fresh role file.
    assert!(
        common::wait_until(BUDGET, || context_files(&dir).len() > before.len()),
        "no role file was published for the re-assert; before {before:?}\n{}",
        evidence()
    );
    let new: Vec<String> = context_files(&dir)
        .into_iter()
        .filter(|name| !before.contains(name))
        .collect();
    assert_eq!(new.len(), 1, "one re-assert, one new role file: {new:?}");
    let rearmed = &new[0];

    // After the summary the orchestrator followed the re-assert.
    assert!(
        common::wait_until(BUDGET, || reads(&dir) >= 2),
        "the orchestrator did not read its role file again after /compact\n{}",
        evidence()
    );

    // Where it landed. Codex's rollout records the compaction as a
    // `compacted` line; the new role file's name must appear in no line up to
    // it (so it is not something the summary replaced), then reach the
    // conversation after it as a message, which a tool call naming the file
    // follows.
    let home = deck.home_dir().to_path_buf();
    let found = std::cell::RefCell::new(String::new());
    let landed_after = common::wait_until(Duration::from_secs(30), || {
        let mut found = found.borrow_mut();
        found.clear();
        rollouts(&home).iter().any(|path| {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            let entries: Vec<serde_json::Value> = text
                .lines()
                .map(|l| serde_json::from_str(l).unwrap_or(serde_json::Value::Null))
                .collect();
            let boundary = entries
                .iter()
                .position(|e| e.get("type").and_then(|v| v.as_str()) == Some("compacted"));
            let mentions: Vec<(usize, String)> = entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.to_string().contains(rearmed.as_str()))
                .map(|(i, e)| (i, kind(e)))
                .collect();
            let before_boundary = mentions
                .iter()
                .any(|(i, _)| boundary.is_none_or(|b| *i <= b));
            // Measured on Codex 0.164.0: the re-assert reaches the model as a
            // message (the inbox message the deck's hook hands Codex, a
            // `developer` one), and a tool call naming the file follows it.
            let delivered = boundary.and_then(|b| {
                mentions
                    .iter()
                    .find(|(i, k)| *i > b && k.starts_with("response_item/message"))
                    .map(|(i, _)| *i)
            });
            let after_boundary = delivered.is_some_and(|d| {
                mentions
                    .iter()
                    .any(|(i, k)| *i > d && k.starts_with("response_item/") && k.contains("call"))
            });
            use std::fmt::Write;
            let _ = writeln!(
                *found,
                "{}: {} entries, boundary {boundary:?}, mentions {mentions:?}",
                path.display(),
                entries.len()
            );
            boundary.is_some() && !before_boundary && after_boundary
        })
    });
    assert!(
        landed_after,
        "the re-asserted role file {rearmed} must reach the conversation after the \
         compaction record as a message a tool call then names, and no line up to it\n{}\n{}",
        found.borrow(),
        evidence()
    );
    eprintln!("re-assert landed after the summary:\n{}", found.borrow());
}
