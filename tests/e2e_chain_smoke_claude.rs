#![cfg(all(feature = "e2e", feature = "e2e-live"))]

//! Real interactive Claude chain smoke through the native inbox and live card.

dot_agent_deck_test_harness::link!();

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::AttachRequest;
use spec::spec;

const PINNED_MODEL: &str = "claude-haiku-4-5-20251001";
const SENTINEL: &str = "claude_chain_sentinel_8d24.txt";
const PROOF: &str = "claude_chain_proof_8d24.txt";
const BUDGET: Duration = Duration::from_secs(120);

/// Scenario: Restore an interactive Claude Haiku pane with isolated credentials
/// and pre-seeded project trust, then post one inbox task asking it to copy a
/// uniquely named sentinel through Bash. On the dashboard, require Thinking →
/// Working with Bash → Idle, the exact proof file, and a still-live Claude pane.
#[spec("chain-smoke/claude/001")]
#[test]
fn claude_001_thinking_working_idle() {
    skip_unless!(common::check_claude_available());
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_continue_session(
            "claude-smoke",
            format!("claude --model {PINNED_MODEL} --allowedTools Bash Read Write"),
        )
        .launch_with_fixture("chain-smoke-claude");
    std::fs::write(deck.workdir().join(SENTINEL), "CLAUDE_CHAIN_OK\n")
        .expect("write isolated sentinel");
    deck.wait_for_string("Claude Code v");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    deck.wait_for_string("claude-smoke");

    let agents = common::attach_request_on(deck.attach_socket_path(), &AttachRequest::ListAgents)
        .expect("list the sandbox's agents")
        .agent_records
        .expect("agent records");
    let agent = agents
        .iter()
        .find(|agent| agent.display_name.as_deref() == Some("claude-smoke"))
        .expect("restored interactive Claude");
    let pane = agent.pane_id_env.as_deref().expect("managed pane identity");
    let start = deck.stream_text().len();
    let directive = format!(
        "Use Bash exactly once to run: sleep 3; cat '{}/{SENTINEL}' > '{}/{PROOF}'. \
         Do not modify any other file. Then reply CLAUDE_CHAIN_DONE and end your turn.",
        deck.workdir().display(),
        deck.workdir().display(),
    );
    common::post_message_on(deck.attach_socket_path(), pane, &agent.id, &directive)
        .expect("post the sentinel task through Claude's inbox");

    // Inspect bytes painted after the post boundary, so an initial Idle card
    // cannot count as completion. Keeping Bash alive for three seconds makes
    // its Working frame visible without a second model turn.
    assert!(
        common::wait_until(BUDGET, || {
            let stream = deck.stream_text();
            let turn = stream.get(start..).unwrap_or_default();
            common::match_needles_in_order(
                turn.as_bytes(),
                &["Thinking", "Working", "Bash", "Idle"],
            ) == 4
        }),
        "the interactive Claude card did not complete Thinking → Working/Bash → Idle:\n{}",
        deck.snapshot_grid()
    );
    common::wait_for_file_trimmed_eq(
        &deck.workdir().join(PROOF),
        "CLAUDE_CHAIN_OK",
        Duration::from_secs(10),
    )
    .expect("real Claude must copy the fixture sentinel exactly");
    assert!(
        deck.snapshot_grid().contains("Idle"),
        "{}",
        deck.snapshot_grid()
    );
    let agents = common::attach_request_on(deck.attach_socket_path(), &AttachRequest::ListAgents)
        .expect("list surviving interactive agent")
        .agent_records
        .expect("agent records");
    assert!(agents.iter().any(|record| record.id == agent.id));
}

// ---------------------------------------------------------------------------
// chain-smoke/claude/002: a delegate reaches a touched Claude input box only
// after the deck's check of the box reads it empty (PRD #1666 part 3).
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[cfg(target_os = "linux")]
#[path = "support/inbox_view.rs"]
mod inbox_view;

#[cfg(target_os = "linux")]
mod checked_delegate {
    use super::{PINNED_MODEL, common, inbox, inbox_view};
    use dot_agent_deck::doorbell_activation::{
        AGENT_FACTS, Activation, CLAUDE_FACTS, LatchedRelease, Measurement,
    };
    use dot_agent_deck::event::AgentType;
    use serde_json::{Value, json};
    use std::time::Duration;

    pub const CHECK_BUDGET: Duration = Duration::from_secs(180);
    pub const LOG_002: &str = "chain-smoke-002.log";
    /// The file the test creates with `O_EXCL` and the agent appends to.
    pub const SENTINEL_002: &str = "chain-smoke-claude-002-c4e9a1.txt";
    /// What the user types into the worker's input box and then deletes.
    pub const DRAFT: &str = "half-typed-draft-c4e9a1";
    const ORCHESTRATION: &str = "chain-smoke-002";

    /// The worker's Claude Code: the installed `claude`, at any release (the
    /// deck checks every release on Claude Code's facts). Since the Claude activation (Q35-28) the production
    /// rollout installs `Direct(Claude)`, so no e2e assumption is set.
    pub struct Mode {
        release: String,
    }

    impl Mode {
        pub fn release(&self) -> &str {
            &self.release
        }
    }

    fn checkable(release: &str) -> bool {
        let Ok(version) = semver::Version::parse(release) else {
            return false;
        };
        let go = matches!(
            dot_agent_deck::doorbell_activation::activation(
                &AGENT_FACTS,
                &AgentType::ClaudeCode,
                &LatchedRelease::Known(version.clone()),
            ),
            Activation::Go { .. }
        );
        go && matches!(CLAUDE_FACTS.editor_probe, Measurement::Verified(_))
    }

    fn installed_release() -> Result<String, String> {
        let output = std::process::Command::new("claude")
            .arg("--version")
            .current_dir(std::env::temp_dir())
            .output()
            .map_err(|e| format!("`claude --version` did not run: {e}"))?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.split_whitespace()
            .next()
            .filter(|r| semver::Version::parse(r).is_ok())
            .map(str::to_string)
            .ok_or_else(|| format!("`claude --version` printed no release: {text:?}"))
    }

    /// The installed release this build checks, or why it can't run here (a
    /// SKIP, which `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` turns into a failure).
    pub fn mode() -> Result<Mode, String> {
        use dot_agent_deck::composer_probe::install::{INSTALL_ROLLOUT, PaneClass, ProbeAgent};
        if !INSTALL_ROLLOUT.contains(&PaneClass::Direct(ProbeAgent::Claude)) {
            return Err(format!(
                "this build installs no Direct(Claude) pane (INSTALL_ROLLOUT = {INSTALL_ROLLOUT:?})"
            ));
        }
        let release = installed_release()?;
        if checkable(&release) {
            Ok(Mode { release })
        } else {
            Err(format!(
                "Claude Code {release} is installed, and this build's Claude Code facts are \
                 not Go with a verified editor_probe"
            ))
        }
    }

    /// The worker's command: a plain argv whose first word is `claude`, so
    /// the pane's class is `Direct(Claude)`.
    pub fn worker_command() -> String {
        format!("claude --model {PINNED_MODEL} --allowedTools Bash Read Write")
    }

    /// The orchestration: a scripted orchestrator that runs the real
    /// `dot-agent-deck delegate` once `delegate-now` exists, and the worker.
    pub fn write_orchestration(deck: &common::TuiDeck, worker: &str, line: &str) {
        use std::os::unix::fs::PermissionsExt;
        let dir = deck.workdir();
        std::fs::write(
            dir.join(".dot-agent-deck.toml"),
            format!(
                "[[orchestrations]]\nname = \"{ORCHESTRATION}\"\n\n[[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"./delegate.sh\"\nstart = true\n\n[[orchestrations.roles]]\nname = \"coder\"\ncommand = {worker:?}\nclear = false\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("delegate-task.md"),
            format!(
                "Use the Bash tool exactly once to run: printf '%s\\n' '{line}' >> '{}'. \
                 Do not modify any other file. Then reply CHAIN-002-DONE and end your turn.",
                dir.join(SENTINEL_002).display()
            ),
        )
        .unwrap();
        let script = dir.join("delegate.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'orchestrator: waiting to delegate\\n'\nwhile [ ! -f delegate-now ]; do sleep 0.2; done\ndot-agent-deck delegate --to coder --task-file delegate-task.md > delegate-cli.log 2>&1\nprintf 'orchestrator: delegated (exit %s)\\n' \"$?\"\nexec cat\n",
        )
        .unwrap();
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub fn coder(deck: &common::TuiDeck) -> Value {
        inbox::attach(deck, json!({"op":"list-agents"}))["agent_records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["tab_membership"]["role_name"] == "coder")
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// The worker's own screen at its PTY's geometry.
    pub fn pane_grid(deck: &common::TuiDeck, agent: &str) -> String {
        let record = coder(deck);
        let raw = common::pane_snapshot_on(deck.attach_socket_path(), agent);
        let mut parser = dot_agent_deck::vt_grid::parser(
            record["rows"].as_u64().unwrap_or(50) as u16,
            record["cols"].as_u64().unwrap_or(180) as u16,
            0,
        );
        parser.process(&raw);
        parser.screen().contents()
    }

    /// The text on Claude Code's input row, if one is drawn. An emptied box
    /// can show Claude Code's `Try "…"` suggestion, which reads as empty here.
    pub fn composer(deck: &common::TuiDeck, agent: &str) -> Option<String> {
        pane_grid(deck, agent).lines().rev().find_map(|row| {
            let tail = row.trim_start().strip_prefix('❯')?;
            let text = tail.split("· ? for shortcuts").next().unwrap().trim();
            Some(if text.starts_with("Try \"") {
                String::new()
            } else {
                text.to_string()
            })
        })
    }

    pub fn log(deck: &common::TuiDeck) -> String {
        std::fs::read_to_string(deck.workdir().join(LOG_002)).unwrap_or_default()
    }

    /// Kill the worker's native waiter, the one process of this deck's HOME
    /// running `inbox wait`, so the doorbell is the wake path.
    pub fn kill_native_waiter(deck: &common::TuiDeck) -> i32 {
        let home = format!("HOME={}", deck.home_dir().display());
        let mut pids = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let cmd = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
            let env = std::fs::read(entry.path().join("environ")).unwrap_or_default();
            if cmd.split(|c| *c == 0).any(|a| a == b"wait")
                && cmd.split(|c| *c == 0).any(|a| a == b"inbox")
                && env.split(|c| *c == 0).any(|v| v == home.as_bytes())
            {
                pids.push(pid);
            }
        }
        assert_eq!(pids.len(), 1, "one captured scratch waiter: {pids:?}");
        assert_eq!(unsafe { libc::kill(pids[0], libc::SIGKILL) }, 0);
        pids[0]
    }

    pub fn messages(
        deck: &common::TuiDeck,
        pane: &str,
    ) -> Vec<dot_agent_deck::inbox::MessageSummary> {
        inbox_view::messages(deck.attach_socket_path(), pane)
    }

    pub fn evidence(deck: &common::TuiDeck, agent: &str) -> String {
        format!(
            "record={}\nmessages={:?}\nworker pane:\n{}\nTUI grid:\n{}\ndelegate CLI={:?}\nLog:\n{}",
            coder(deck),
            messages(
                deck,
                coder(deck)["pane_id_env"].as_str().unwrap_or_default()
            ),
            pane_grid(deck, agent),
            deck.snapshot_grid(),
            std::fs::read_to_string(deck.workdir().join("delegate-cli.log")),
            log(deck)
        )
    }
}

/// Scenario: Open an orchestration in the attached TUI whose worker is interactive Claude Code (Haiku), started directly in its pane, with its own wake-up process killed so the deck's doorbell is the way in; the user focuses the worker, types a draft into its input box and deletes it. The scripted orchestrator then runs the real `dot-agent-deck delegate`: the deck checks the touched box (Claude redraws for the check), finds it empty, rings, and the agent appends one uniquely named line to a sentinel file the test created with `O_EXCL`. The file must hold exactly that one line and the inbox must report the task acknowledged with one delivery, with the check's empty read logged before the ring.
#[cfg(target_os = "linux")]
#[spec("chain-smoke/claude/002")]
#[test]
fn claude_002_touched_box_checked_then_delegate_delivered_once() {
    use checked_delegate::*;
    use std::io::Write as _;
    skip_unless!(common::check_claude_available());
    let mode = match mode() {
        Ok(mode) => mode,
        Err(why) => {
            skip_unless!(Err(why));
            return;
        }
    };
    eprintln!(
        "chain-smoke/claude/002: Claude Code {} activated in production (no assumption)",
        mode.release()
    );
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_env("PATH", common::path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_LOG", LOG_002)
        .with_env(
            "RUST_LOG",
            "info,composer_probe=debug,dot_agent_deck::doorbell=debug",
        )
        .launch_with_fixture("minimal");
    assert!(deck.wait_for_grid_string_within("No active agents", CHECK_BUDGET));

    // A per-run line, so a stale file or a second append is visible.
    let nonce = format!(
        "{:x}{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );
    let line = format!("chain-smoke-claude-002-{nonce}");
    let sentinel = deck.workdir().join(SENTINEL_002);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&sentinel)
        .expect("create the sentinel with O_EXCL")
        .flush()
        .unwrap();
    let worker = worker_command();
    write_orchestration(&deck, &worker, &line);

    // Open the orchestration from the TUI's new-pane form, then focus the
    // worker (card 2) in pane-input mode.
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.wait_for_absence("Command:");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.wait_for_string("coder");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Agent n/Ctrl+N]");
    deck.send_bytes(b"2");
    let record = std::cell::RefCell::new(serde_json::Value::Null);
    assert!(
        common::wait_until(CHECK_BUDGET, || {
            *record.borrow_mut() = coder(&deck);
            record.borrow()["pane_id_env"].is_string()
        }),
        "the worker must register; {}",
        deck.snapshot_grid()
    );
    let record = record.into_inner();
    let agent = record["id"].as_str().unwrap().to_string();
    let pane = record["pane_id_env"].as_str().unwrap().to_string();
    assert!(
        common::wait_until(CHECK_BUDGET, || {
            let grid = pane_grid(&deck, &agent);
            grid.contains(&format!("Claude Code v{}", mode.release()))
                && grid.contains("? for shortcuts")
        }),
        "Claude Code {} must reach its input box; {}",
        mode.release(),
        evidence(&deck, &agent)
    );
    assert!(
        common::wait_until(CHECK_BUDGET, || coder(&deck)["inbox"]["wake"] == "native"),
        "Claude's own wake-up must arm first; {}",
        evidence(&deck, &agent)
    );
    let waiter = kill_native_waiter(&deck);
    assert!(
        common::wait_until(Duration::from_secs(15), || coder(&deck)["inbox"]["wake"]
            == "doorbell"),
        "with its waiter {waiter} gone the worker's wake path is the doorbell; {}",
        evidence(&deck, &agent)
    );
    assert!(
        common::wait_until(CHECK_BUDGET, || log(&deck)
            .lines()
            .any(|l| l.contains("editor probe permission settled")
                && l.contains(&agent)
                && l.contains("Armable"))),
        "the worker's box must be checkable; {}",
        evidence(&deck, &agent)
    );

    // The user types a draft into the worker's input box, then deletes it.
    deck.send_keys(DRAFT.as_bytes());
    assert!(
        common::wait_until(Duration::from_secs(15), || composer(&deck, &agent)
            .as_deref()
            == Some(DRAFT)),
        "the draft must reach Claude's input box; {}",
        evidence(&deck, &agent)
    );
    deck.send_keys(&vec![0x7f; DRAFT.len()]);
    assert!(
        common::wait_until(Duration::from_secs(15), || composer(&deck, &agent)
            .as_deref()
            == Some("")),
        "the deleted draft must leave the input box empty; {}",
        evidence(&deck, &agent)
    );
    let touched_at = log(&deck).len();

    // The orchestrator delegates.
    std::fs::write(deck.workdir().join("delegate-now"), "").unwrap();
    assert!(
        common::wait_until(CHECK_BUDGET, || {
            std::fs::read_to_string(&sentinel).is_ok_and(|text| text.contains(&line))
        }),
        "the delegated task must reach the worker and run; {}",
        evidence(&deck, &agent)
    );
    let task = std::cell::RefCell::new(None);
    assert!(
        common::wait_until(CHECK_BUDGET, || {
            // The orchestrator's delegate, the one message an agent sent.
            *task.borrow_mut() = messages(&deck, &pane)
                .into_iter()
                .find(|m| matches!(&m.from, dot_agent_deck::inbox::Sender::Agent { .. }));
            task.borrow()
                .as_ref()
                .is_some_and(|m| m.state == dot_agent_deck::inbox::StateName::Acknowledged)
        }),
        "the delegate must be acknowledged; {}",
        evidence(&deck, &agent)
    );
    // A bounded observation: a duplicate hand-over or a second action gets
    // five seconds to show. The closure never asks to stop, so it samples the
    // once-only oracle for the whole window.
    let id = task.borrow().as_ref().unwrap().id.clone();
    let repeated = std::cell::Cell::new(false);
    common::wait_until(Duration::from_secs(5), || {
        let twice = messages(&deck, &pane)
            .iter()
            .any(|m| m.id == id && m.deliveries > 1)
            || std::fs::read_to_string(&sentinel).is_ok_and(|text| text.lines().count() > 1);
        repeated.set(repeated.get() || twice);
        false
    });
    assert!(
        !repeated.get(),
        "a second hand-over or append was seen; {}",
        evidence(&deck, &agent)
    );
    let task = messages(&deck, &pane)
        .into_iter()
        .find(|m| m.id == task.borrow().as_ref().unwrap().id)
        .unwrap();
    assert_eq!(
        task.deliveries,
        1,
        "one hand-over: {task:?}; {}",
        evidence(&deck, &agent)
    );
    assert_eq!(
        std::fs::read_to_string(&sentinel).unwrap(),
        format!("{line}\n"),
        "exactly one append; {}",
        evidence(&deck, &agent)
    );

    // The ring came after a check of the touched box read it empty.
    let after = log(&deck)[touched_at..].to_string();
    // The worker is the deck's only probed and rung pane: the orchestrator
    // role is a shell script.
    let at = |needle: &str, also: &str| {
        after
            .lines()
            .position(|l| l.contains(needle) && l.contains(also))
    };
    let settled = at("probe settled", "read=Some(true)");
    let applied = at("empty read applied", "");
    let rang = at("doorbell rang", "");
    assert!(
        matches!((settled, applied, rang), (Some(s), Some(a), Some(r)) if s <= a && a < r),
        "the check must read the touched box empty before the ring \
         (settled={settled:?}, applied={applied:?}, rang={rang:?}); {}",
        evidence(&deck, &agent)
    );
    assert!(
        !coder(&deck)["live"]["last_user_prompt"]
            .as_str()
            .unwrap_or("")
            .contains(DRAFT),
        "the deleted draft was never submitted; {}",
        evidence(&deck, &agent)
    );
    eprintln!(
        "chain-smoke/claude/002 PASS: release={} assumed=None task={} deliveries={}\n{}{}",
        mode.release(),
        task.id,
        task.deliveries,
        [settled, applied, rang]
            .iter()
            .flatten()
            .filter_map(|i| after.lines().nth(*i))
            .collect::<Vec<_>>()
            .join("\n"),
        common::load_context::failure_report()
    );
}
