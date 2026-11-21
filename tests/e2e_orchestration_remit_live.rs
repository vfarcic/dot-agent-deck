#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! PRD #82 M2: a REAL interactive Claude Code orchestrator is compacted with
//! `/compact`, and the deck's re-assert of its role reaches the conversation
//! that follows the summary, not the one the summary replaced.

dot_agent_deck_test_harness::link!();

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::{AgentRecord, TabMembership};
use dot_agent_deck::event::EventType;
use spec::spec;

/// The file the fixture's role text appends one line to per read of the role.
const READS: &str = "remit_reads_82_5c1e.txt";

/// Each step's wait: one model round trip (a startup turn, a compaction's
/// summary call, the turn after it), measured at under 30 s on Haiku. Kept
/// well inside nextest's 180 s termination, so a step that never happens
/// fails with its evidence rather than being killed without any.
const BUDGET: Duration = Duration::from_secs(90);

fn reads(deck: &TuiDeck) -> usize {
    std::fs::read_to_string(deck.workdir().join(READS))
        .map(|text| text.lines().filter(|l| l.trim() == "REMIT-READ").count())
        .unwrap_or(0)
}

/// Every per-publish context file in the fixture's `.dot-agent-deck`.
fn context_files(deck: &TuiDeck) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(deck.workdir().join(".dot-agent-deck"))
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

/// Every Claude Code transcript under the test HOME.
fn transcripts(home: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(projects) = std::fs::read_dir(home.join(".claude").join("projects")) else {
        return found;
    };
    for project in projects.filter_map(Result::ok) {
        if let Ok(files) = std::fs::read_dir(project.path()) {
            found.extend(
                files
                    .filter_map(Result::ok)
                    .map(|f| f.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "jsonl")),
            );
        }
    }
    found
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
        common::wait_until(Duration::from_secs(90), || {
            common::agent_records_on(deck.attach_socket_path())
                .iter()
                .any(is_start)
        }),
        "the orchestrator did not spawn\n{}",
        deck.snapshot_grid()
    );
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(is_start)
        .expect("orchestrator record")
}

/// Scenario: Open a real interactive Haiku orchestrator from the TUI and wait until it has read its role file once. Type /compact into its pane. The deck sees the compaction, publishes a fresh role file and posts the re-assert; after the summary, the orchestrator reads that new file and records a second read, and its transcript shows the new file only after the compaction boundary.
#[spec("orchestration/remit/012")]
#[test]
fn orchestration_remit_012_real_claude_compaction_reassert_lands_after_the_summary() {
    skip_unless!(common::check_claude_available());
    let deck = TuiDeck::builder()
        .with_pty_size(200, 55)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_env("PATH", common::path_with_binary_dir())
        .launch_with_fixture("remit-compaction-live");
    deck.wait_for_string("No active agents");
    let events = deck.subscribe_events();
    deck.send_keys(b"\x0e");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.wait_for_absence("[Submit]");
    let coordinator = orchestrator(&deck);
    let pane = coordinator.pane_id_env.clone().expect("orchestrator pane");
    let session = events.wait_for_session_start_on_pane(&pane, &coordinator.id, BUDGET);

    // The startup read: the spawn-time pointer reached the agent and it
    // followed the role file once.
    assert!(
        common::wait_until(BUDGET, || reads(&deck) >= 1),
        "the orchestrator never recorded reading its role file\n{}",
        common::pane_search_key_on(deck.attach_socket_path(), &coordinator.id)
    );
    // Its startup turn has ended (Claude's own `Stop` hook), so the
    // compaction below summarises a finished conversation.
    events.wait_for(
        |event| {
            event.event_type == EventType::Idle
                && !event.is_from_subagent()
                && event.agent_id.as_deref() == Some(coordinator.id.as_str())
                && event.pane_id.as_deref() == Some(pane.as_str())
                && event.session_id == session
        },
        BUDGET,
    );
    let before = context_files(&deck);
    assert_eq!(reads(&deck), 1, "one read before the compaction");

    let mut input = common::AttachInput::attach(deck.attach_socket_path(), &coordinator.id);
    input.write("/compact\r");

    // Claude Code's `PreCompact` hook reaches the deck as `Compacting`.
    events.wait_for(
        |event| {
            event.event_type == EventType::Compacting
                && event.agent_id.as_deref() == Some(coordinator.id.as_str())
                && event.pane_id.as_deref() == Some(pane.as_str())
        },
        BUDGET,
    );
    // The re-assert published exactly one fresh role file.
    assert!(
        common::wait_until(BUDGET, || context_files(&deck).len() > before.len()),
        "no role file was published for the re-assert; before {before:?}"
    );
    let new: Vec<String> = context_files(&deck)
        .into_iter()
        .filter(|name| !before.contains(name))
        .collect();
    assert_eq!(new.len(), 1, "one re-assert, one new role file: {new:?}");
    let rearmed = &new[0];

    // After the summary the orchestrator followed the re-assert.
    assert!(
        common::wait_until(BUDGET, || reads(&deck) >= 2),
        "the orchestrator did not read its role file again after /compact\n{}",
        common::pane_search_key_on(deck.attach_socket_path(), &coordinator.id)
    );

    // Where it landed. In the transcript, the conversation after the
    // compaction boundary carries the re-assert as a user turn, and no user or
    // assistant turn before the boundary names it, so it is not something the
    // summary replaced. (Claude Code also records a message it queued while
    // compacting; that record is not a turn.) Whether the model then re-reads
    // the file or acts on what it remembers is its own choice: the second
    // sentinel line above is that it acted, and the read is only reported.
    let home = deck.home_dir().to_path_buf();
    let evidence = std::cell::RefCell::new(String::new());
    let landed_after = common::wait_until(Duration::from_secs(30), || {
        let mut evidence = evidence.borrow_mut();
        evidence.clear();
        transcripts(&home).iter().any(|path| {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            let entries: Vec<serde_json::Value> = text
                .lines()
                .map(|l| serde_json::from_str(l).unwrap_or(serde_json::Value::Null))
                .collect();
            let kind = |e: &serde_json::Value| {
                let field = |k: &str| e.get(k).and_then(|v| v.as_str()).unwrap_or("");
                format!(
                    "{}/{}{}",
                    field("type"),
                    field("subtype"),
                    field("operation")
                )
            };
            let boundary = entries.iter().position(|e| {
                e.get("subtype").and_then(|v| v.as_str()) == Some("compact_boundary")
            });
            let mentions: Vec<(usize, String)> = entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.to_string().contains(rearmed.as_str()))
                .map(|(i, e)| (i, kind(e)))
                .collect();
            let is_turn = |k: &str| k.starts_with("user/") || k.starts_with("assistant/");
            let turn_before = mentions
                .iter()
                .any(|(i, k)| boundary.is_none_or(|b| *i < b) && is_turn(k));
            let delivered = boundary.and_then(|b| {
                mentions
                    .iter()
                    .find(|(i, k)| *i > b && k.starts_with("user/"))
                    .map(|(i, _)| *i)
            });
            let read = delivered.and_then(|d| {
                entries
                    .iter()
                    .enumerate()
                    .skip(d)
                    .find(|(_, e)| {
                        let t = e.to_string();
                        t.contains("tool_use") && t.contains(rearmed.as_str())
                    })
                    .map(|(i, _)| i)
            });
            use std::fmt::Write;
            let _ = writeln!(
                *evidence,
                "{}: {} entries, boundary {boundary:?}, mentions {mentions:?}, delivered \
                 {delivered:?}, read {read:?}",
                path.display(),
                entries.len()
            );
            boundary.is_some() && !turn_before && delivered.is_some()
        })
    });
    assert!(
        landed_after,
        "the re-asserted role file {rearmed} must reach the conversation after the \
         compaction boundary as a user turn, and no turn before it\n{}",
        evidence.borrow()
    );
    eprintln!("re-assert landed after the summary:\n{}", evidence.borrow());
}

/// Scenario: Open a real interactive OpenCode orchestrator from the TUI and wait until it has read its role file once and gone idle. Type /compact into its pane. The deck sees the compaction start, publishes a fresh role file and posts the re-assert; once the compaction has ended, the orchestrator reads that new file and records a second read.
#[spec("orchestration/remit/014")]
#[test]
fn orchestration_remit_014_real_opencode_compaction_reassert_follows_the_summary() {
    skip_unless!(common::check_opencode_available());
    let deck = TuiDeck::builder()
        .with_pty_size(200, 55)
        .with_imported_opencode_credentials()
        .with_env(
            "PATH",
            common::path_with_opencode_test_bin(&common::path_with_binary_dir()),
        )
        .launch_with_fixture("remit-compaction-opencode-live");
    deck.wait_for_string("No active agents");
    let launcher = deck.workdir().join("run-orchestrator.sh");
    std::fs::write(
        &launcher,
        format!(
            "#!/bin/sh\nexec {}\n",
            common::opencode_interactive_command(false, true, "")
        ),
    )
    .expect("write the orchestrator launcher");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))
            .expect("make the launcher executable");
    }
    let events = deck.subscribe_events();
    deck.send_keys(b"\x0e");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.wait_for_absence("[Submit]");
    let coordinator = orchestrator(&deck);
    let pane = coordinator.pane_id_env.clone().expect("orchestrator pane");
    let evidence = || {
        format!(
            "{}\nevents: {:?}",
            common::pane_search_key_on(deck.attach_socket_path(), &coordinator.id),
            events
                .snapshot()
                .iter()
                .filter(|e| e.pane_id.as_deref() == Some(pane.as_str()))
                .map(|e| (e.event_type.clone(), e.tool_detail.clone()))
                .collect::<Vec<_>>()
        )
    };

    assert!(
        common::wait_until(BUDGET, || reads(&deck) >= 1),
        "the orchestrator never recorded reading its role file\n{}",
        evidence()
    );
    // Its startup turn has ended.
    assert!(
        common::wait_until(BUDGET, || events.snapshot().iter().rev().any(|e| {
            e.pane_id.as_deref() == Some(pane.as_str())
                && e.event_type == EventType::Idle
                && !e.is_from_subagent()
        })),
        "the orchestrator's startup turn never ended\n{}",
        evidence()
    );
    let before = context_files(&deck);
    assert_eq!(reads(&deck), 1, "one read before the compaction");
    let seen = events.snapshot().len();

    let mut input = common::AttachInput::attach(deck.attach_socket_path(), &coordinator.id);
    input.write_steps([
        ("/compact", Duration::from_millis(800)),
        ("\r", Duration::from_millis(200)),
    ]);

    // The plugin reports the compaction starting.
    assert!(
        common::wait_until(BUDGET, || events.snapshot().iter().skip(seen).any(|e| {
            e.pane_id.as_deref() == Some(pane.as_str()) && e.event_type == EventType::Compacting
        })),
        "OpenCode's compaction never reached the deck as Compacting\n{}",
        evidence()
    );
    assert!(
        common::wait_until(BUDGET, || context_files(&deck).len() > before.len()),
        "no role file was published for the re-assert; before {before:?}\n{}",
        evidence()
    );
    let new: Vec<String> = context_files(&deck)
        .into_iter()
        .filter(|name| !before.contains(name))
        .collect();
    assert_eq!(new.len(), 1, "one re-assert, one new role file: {new:?}");
    assert!(
        common::wait_until(BUDGET, || reads(&deck) >= 2),
        "the orchestrator did not read its role file again after /compact\n{}",
        evidence()
    );

    // Order: the orchestrator read the new role file after the compaction
    // started. That file is published only when the compaction starts, and
    // the model takes no turn while it writes the summary, so the read is a
    // turn after the summary. (OpenCode 2.x runs the queued re-assert in the
    // same execution as the compaction, with no idle between them; 1.x goes
    // idle first.)
    let after: Vec<_> = events
        .snapshot()
        .into_iter()
        .skip(seen)
        .filter(|e| e.pane_id.as_deref() == Some(pane.as_str()) && !e.is_from_subagent())
        .collect();
    let compacting = after
        .iter()
        .position(|e| e.event_type == EventType::Compacting);
    // A tool's detail is shortened on the event, which can cut the file name
    // off an absolute path (OpenCode 1.x), so the read is recognised by the
    // directory prefix and the re-assert by the inbox message naming the file.
    let read = after.iter().position(|e| {
        e.event_type == EventType::ToolStart
            && e.tool_detail.as_deref().is_some_and(|d| {
                d.contains(new[0].as_str()) || d.contains(".dot-agent-deck/orches")
            })
    });
    let pointer = format!("Read .dot-agent-deck/{}", new[0]);
    let posted = common::attach_request_on(
        deck.attach_socket_path(),
        &dot_agent_deck::daemon_protocol::AttachRequest::ListInbox {
            pane_id: Some(pane.clone()),
            bodies: true,
        },
    )
    .ok()
    .and_then(|r| r.inbox_messages)
    .unwrap_or_default()
    .iter()
    .any(|m| m.text.as_deref().is_some_and(|t| t.contains(&pointer)));
    assert!(posted, "the re-assert naming {} was never posted", new[0]);
    assert!(
        matches!((compacting, read), (Some(c), Some(r)) if c < r),
        "expected Compacting, then a read of a role file (the new one is {}); got compacting \
         {compacting:?}, read {read:?}\n{}",
        new[0],
        evidence()
    );
}
