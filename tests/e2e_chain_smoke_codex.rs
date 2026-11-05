#![cfg(all(feature = "e2e", feature = "e2e-live", target_os = "linux"))]

//! Real interactive Codex chain smoke: a delegate reaches a Codex worker
//! whose input box the user touched only after the deck's check reads it
//! empty (PRD #1666 part 3, `chain-smoke/codex/001`, the Codex counterpart
//! of `chain-smoke/claude/002`).
//!
//! Until the Codex activation this runs with the e2e editor-probe
//! assumption, the seam (`support/codex_installed.rs`), which drops itself
//! once the build activates Codex.
//!
//! **Status (cxl2, 2026-11-05): written, not yet run green**: blocked by the
//! same `Keymap(Changed)` defect as `inbox/probe/043`
//! (`tests/e2e_composer_check_codex_live.rs`).

#[path = "support/codex_installed.rs"]
mod codex_installed;
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_view.rs"]
mod inbox_view;
#[path = "support/claude_installed.rs"]
mod installed;
#[path = "support/codex_inbox_live.rs"]
mod live;

use codex_installed::CprAnswerer;
use serde_json::{Value, json};
use spec::spec;
use std::time::Duration;

const CHECK_BUDGET: Duration = Duration::from_secs(180);
const LOG: &str = "chain-smoke-codex-001.log";
/// The file the test creates with `O_EXCL` and the agent appends to.
const SENTINEL: &str = "chain-smoke-codex-001-7b2f50.txt";
/// What the user types into the worker's input box and then deletes.
const DRAFT: &str = "half-typed-draft-7b2f50";
const ORCHESTRATION: &str = "chain-smoke-codex-001";

/// The orchestration: a scripted orchestrator that runs the real
/// `dot-agent-deck delegate` once `delegate-now` exists, and the worker, a
/// plain `codex …` argv (the `Direct(Codex)` class).
fn write_orchestration(deck: &common::TuiDeck, line: &str) {
    use std::os::unix::fs::PermissionsExt;
    let dir = deck.workdir();
    std::fs::write(
        dir.join(".dot-agent-deck.toml"),
        format!(
            "[[orchestrations]]\nname = \"{ORCHESTRATION}\"\n\n[[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"./delegate.sh\"\nstart = true\n\n[[orchestrations.roles]]\nname = \"coder\"\ncommand = {:?}\nclear = false\n",
            codex_installed::command()
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("delegate-task.md"),
        format!(
            "Run exactly this shell command once: printf '%s\\n' '{line}' >> '{}'. \
             Do not modify any other file. Then reply CHAIN-CODEX-001-DONE and end your turn.",
            dir.join(SENTINEL).display()
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

fn coder(deck: &common::TuiDeck) -> Value {
    inbox::attach(deck, json!({"op":"list-agents"}))["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tab_membership"]["role_name"] == "coder")
        .cloned()
        .unwrap_or(Value::Null)
}

/// The worker's own screen at its PTY's geometry.
fn pane_screen(deck: &common::TuiDeck, agent: &str) -> vt100::Parser {
    let record = coder(deck);
    let raw = common::pane_snapshot_on(deck.attach_socket_path(), agent);
    let mut parser = dot_agent_deck::vt_grid::parser(
        record["rows"].as_u64().unwrap_or(50) as u16,
        record["cols"].as_u64().unwrap_or(180) as u16,
        0,
    );
    parser.process(&raw);
    parser
}

fn composer(deck: &common::TuiDeck, agent: &str) -> Option<String> {
    codex_installed::composer_of(pane_screen(deck, agent).screen())
}

fn log(deck: &common::TuiDeck) -> String {
    std::fs::read_to_string(deck.workdir().join(LOG)).unwrap_or_default()
}

fn messages(deck: &common::TuiDeck, pane: &str) -> Vec<dot_agent_deck::inbox::MessageSummary> {
    inbox_view::messages(deck.attach_socket_path(), pane)
}

fn evidence(deck: &common::TuiDeck, agent: &str) -> String {
    common::redact_credentials_for_output(&format!(
        "record={}\nmessages={:?}\nworker pane:\n{}\nTUI grid:\n{}\ndelegate CLI={:?}\nLog:\n{}",
        coder(deck),
        messages(
            deck,
            coder(deck)["pane_id_env"].as_str().unwrap_or_default()
        ),
        pane_screen(deck, agent).screen().contents(),
        deck.snapshot_grid(),
        std::fs::read_to_string(deck.workdir().join("delegate-cli.log")),
        installed::tail(
            &log(deck)
                .lines()
                .filter(|l| !l.contains("doorbell ring not admitted"))
                .collect::<Vec<_>>()
                .join("\n"),
            300
        )
    ))
}

/// Scenario: Open an orchestration in the attached TUI whose worker is interactive Codex (the cheap test model), started directly in its pane; a test-owned client attached to the worker answers Codex's cursor position queries as the desktop app's terminal would (a disclosed stand-in for a desktop, not coverage of the desktop's own answers). The user focuses the worker, types a draft into its input box and deletes it. The scripted orchestrator then runs the real `dot-agent-deck delegate`: the deck checks the touched box, finds it empty, rings, and Codex appends one uniquely named line to a sentinel file the test created with `O_EXCL`. The file must hold exactly that one line and the inbox must report the task acknowledged with one delivery, with the check's empty read logged before the ring.
#[spec("chain-smoke/codex/001")]
#[test]
fn codex_001_touched_box_checked_then_delegate_delivered_once() {
    use std::io::Write as _;
    skip_unless!(codex_installed::checkable());
    let release = codex_installed::installed_release().unwrap();
    let seam = codex_installed::seam();
    eprintln!("chain-smoke/codex/001: Codex {release}, assumption {seam:?}");
    let mut builder = common::TuiDeck::builder();
    if let Some(assumed) = &seam {
        builder = builder.with_env("DOT_AGENT_DECK_E2E_ASSUME_EDITOR_PROBE", assumed);
    }
    let deck = builder
        .with_pty_size(180, 50)
        .with_imported_codex_credentials()
        .with_env("PATH", common::path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_LOG", LOG)
        .with_env(
            "RUST_LOG",
            "info,composer_probe=debug,dot_agent_deck::doorbell=debug",
        )
        .launch_with_fixture("minimal");
    assert!(deck.wait_for_grid_string_within("No active agents", CHECK_BUDGET));
    // The deck's Codex hooks trusted in the sandbox's `CODEX_HOME`, as an
    // install the user ran earlier would have left them.
    live::prepare(&deck);
    // A returning user's Codex home, which Codex itself no longer rewrites
    // on start (`codex_installed::steady_codex_home`).
    codex_installed::steady_codex_home(&deck);

    // A per-run line, so a stale file or a second append is visible.
    let nonce = format!(
        "{:x}{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );
    let line = format!("chain-smoke-codex-001-{nonce}");
    let sentinel = deck.workdir().join(SENTINEL);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&sentinel)
        .expect("create the sentinel with O_EXCL")
        .flush()
        .unwrap();
    write_orchestration(&deck, &line);

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
    let record = std::cell::RefCell::new(Value::Null);
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
    let answerer = CprAnswerer::attach(
        deck.attach_socket_path(),
        &agent,
        record["rows"].as_u64().unwrap_or(50) as u16,
        record["cols"].as_u64().unwrap_or(180) as u16,
    );
    let latched = format!("release={release}");
    assert!(
        common::wait_until(CHECK_BUDGET, || {
            log(&deck).lines().any(|l| {
                l.contains("pane release")
                    && l.contains(&agent)
                    && l.split_whitespace().any(|w| w == latched)
            }) && pane_screen(&deck, &agent)
                .screen()
                .contents()
                .contains("? for shortcuts")
        }),
        "Codex {release} must latch and reach its input box; {}",
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
        "the draft must reach Codex's input box; {}",
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
    // five seconds to show.
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
        .find(|m| m.id == id)
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
        "chain-smoke/codex/001 PASS: release={release} assumed={seam:?} task={} deliveries={} \
         cpr_answers={}\n{}{}",
        task.id,
        task.deliveries,
        answerer.answered(),
        [settled, applied, rang]
            .iter()
            .flatten()
            .filter_map(|i| after.lines().nth(*i))
            .collect::<Vec<_>>()
            .join("\n"),
        common::load_context::failure_report()
    );
}
