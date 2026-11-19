#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! L2 lane-2 REAL-agent proof for Generate `.dot-agent-deck.toml` (issue
//! #2119): a user selects a live interactive Claude Code agent's card, presses
//! `g`, confirms Yes, approves the configuration the agent proposes, and ends up
//! with a `.dot-agent-deck.toml` the deck accepts.
//!
//! ## Why this test exists when lane 1 already covers the verb
//! `inbox/route/007` drives the same dialog against a stand-in agent and pins
//! the exact inbox message the daemon's `generate-config` verb posts, and the
//! `daemon/config-gen/*` tests pin the verb and its capability gate. All of that
//! is satisfied by bytes in an inbox. None of it shows that a real agent,
//! handed that request, analyses the project, presents a proposal, waits for the
//! user, and writes a file the deck can load, which is the feature. CLAUDE.md
//! rule 4 asks for one test that validates it as a user uses and sees it; this
//! is that test. It runs on a developer's machine and nowhere in CI (CLAUDE.md
//! rule 5).
//!
//! ## The shape
//! 1. The agent is started over the attach socket (`StartAgent`), as
//!    `project/launch/003` does, so it is a daemon-owned dashboard agent with a
//!    known working directory. That is the precondition of the feature, not its
//!    subject: `g` acts on any selected agent card.
//! 2. The user's half is typed into the real TUI: `1` to open the agent's
//!    pane, Ctrl+D back to command mode, `g`, then Enter on the preselected Yes. The TUI sends the request through the daemon's
//!    `generate-config` verb (it has no other route since slice A), focuses the
//!    agent's pane and enters pane input.
//! 3. The production prompt (`assets/config_gen_prompt.md`, step 4) tells the
//!    agent to present the full proposal and wait for confirmation before
//!    writing. The test waits for that turn to end, checks the file was NOT
//!    written early, and types the approval into the focused pane, as the user
//!    would.
//! 4. The written file must pass the real `dot-agent-deck validate`.
//!
//! ## What makes the assertions robust rather than lucky
//! The fixture is a two-file project whose only agent launcher is the
//! uniquely named Makefile target [`SENTINEL`]. The prompt tells the agent to
//! discover project-defined launchers and use them in every role's `command`,
//! so a run in which the agent genuinely read the project names that target in
//! its proposal or in the file. The hard assertions are on that literal, on the
//! file's existence and on `validate`'s exit status, never on the model's
//! prose. The approval is directive, so a proposal that ends in a question
//! still gets a definite answer.
//!
//! The update case uses the same flow in a fixture with a valid user-written
//! config. After the proposal, the user approves and requests one concrete
//! addition: a uniquely named role. The written config must contain that role
//! and preserve the existing orchestration, role commands and reviewer description.
//!
//! Cost per case: two Haiku turns (one analysis of a small project, one write), well
//! under a cent or two per run. Real-agent failures need their pane evidence
//! inspected rather than blind retries.

dot_agent_deck_test_harness::link!();
#[path = "support/inbox_view.rs"]
mod inbox_view;

use std::cell::Cell;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::TuiDeck;
use dot_agent_deck::agent_pty::DOT_AGENT_DECK_PANE_ID;
use dot_agent_deck::config_gen::{CONFIG_GEN_SUBJECT, config_gen_prompt};
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::AgentType;
use dot_agent_deck::inbox::{MessageSummary, StateName};
use dot_agent_deck::state::SessionStatus;
use spec::spec;

/// A real interactive Haiku Claude Code (no `-p`). The tools are the ones the
/// generation prompt makes the agent use: Read/Glob/Grep to discover the
/// project, Bash for `which` and `dot-agent-deck inbox`, Write/Edit for the
/// file. Any tool missing here would park the agent on a permission prompt.
const AGENT_COMMAND: &str =
    "claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write Edit Glob Grep";

/// `DOT_AGENT_DECK_PANE_ID` for the agent's pane.
const PANE_ID: &str = "config-gen-live-agent";

/// The fixture's only agent launcher, a Makefile target named nowhere else.
const SENTINEL: &str = "deck-agent-7f3a2c";

/// User-owned values in the existing-config fixture that an update must keep.
const USER_ORCHESTRATION: &str = "user-maintenance-91b7ef";
const USER_DESCRIPTION: &str = "Preserve the hand-written review checklist 6d4a89";

/// What the user types into the agent's pane once the proposal is on screen.
/// Directive, so a proposal that ends in a question still gets a definite yes.
const APPROVAL: &str = "Yes, that looks right. Write exactly the configuration you proposed to \
     .dot-agent-deck.toml in the project root now. Do not ask anything else.";

/// The start of [`APPROVAL`], short enough that claude's input box does not
/// wrap it, which the test waits to see on screen before pressing Enter.
const APPROVAL_START: &str = "Yes, that looks right.";

/// A concrete addition makes the existing-config approval require a write,
/// even when the agent's proposal says the current config needs no changes.
const UPDATE_ROLE: &str = "tester-5e1c9a";
const UPDATE_APPROVAL: &str = "Yes. Also add a role named tester-5e1c9a with command = \
    \"make deck-agent-7f3a2c\" to the user-maintenance-91b7ef orchestration, keep everything \
    else exactly as it is, and write .dot-agent-deck.toml now. Do not ask anything else.";
/// The short prefix to observe in the pane before submitting the update.
const UPDATE_APPROVAL_START: &str = "Yes. Also add a role";

/// The bottom bar's first button while typing into a pane.
const PANE_MODE_BUTTON: &str = "[Command Mode";
/// The same button in command mode.
const COMMAND_MODE_BUTTON: &str = "[Back to Pane";

// The wait budget. Every bound below runs in sequence, and all of them must
// expire before the deck's 840 s lifetime cap (set at launch) stops the daemon or
// nextest's window for this test (.config/nextest.toml, 15 x 60 s) kills it;
// either would end the run before a phase's own assertion names what stalled.
// Measured whole runs take about 70-90 s, the boot about 20 s.
//
//   screen waits   7 x 30 s (harness default) + 10 s + 20 s     =  240 s
//   BOOT_WAIT      session start                                =  120 s
//   SETTLE_WAIT    pane settled (a warning, not a failure)      =   30 s
//   PROPOSAL_WAIT  acknowledge + proposal turn, one deadline    =  180 s
//   WRITE_WAIT     the file appears after approval              =  150 s
//   WRITE_SETTLE   the write turn ends (not asserted)           =   30 s
//                                                         sum   =  750 s
//
// That leaves 90 s of the 840 s cap for the unbounded steps (StartAgent, inbox
// reads, `validate`), and the 900 s nextest window 60 s more for what runs
// before the daemon starts. 840 s stays under linkage-check rule 11's ceiling
// (`MAX_PINNED_ORPHAN_CAP_SECS`, 900 s). Raising a bound means lowering another.
/// The agent's boot, measured at about 20 s on an idle box.
const BOOT_WAIT: Duration = Duration::from_secs(120);
/// The pane settling after boot; past it the test asks anyway.
const SETTLE_WAIT: Duration = Duration::from_secs(30);
/// Acknowledging the request and ending the proposal turn, as one deadline.
const PROPOSAL_WAIT: Duration = Duration::from_secs(180);
/// The config file appearing after the approval is submitted.
const WRITE_WAIT: Duration = Duration::from_secs(150);
/// The write turn ending once the file is there; not asserted.
const WRITE_SETTLE: Duration = Duration::from_secs(30);
/// How long the agent must stay Idle before its turn counts as over: a Stop
/// hook is the end of a turn, and this rides out a status flicker around it.
const IDLE_HOLD: Duration = Duration::from_secs(6);

/// Scenario: Launch the deck in a two-file project without a config and start an interactive Haiku agent there. Open its pane with `1`, return to command mode with Ctrl+D, press `g` and confirm Yes; the agent receives the request and proposes without writing until approval is typed into its pane. Verify the written config passes `dot-agent-deck validate` and the agent names the project's uniquely named launcher in its proposal or file.
#[spec("dashboard/config-gen/003")]
#[test]
fn config_gen_003_a_real_agent_generates_a_valid_config_after_approval() {
    // First statement: a missing CLI or credential is an environmental skip,
    // and `launch_with_fixture` panics rather than skips on an import failure.
    skip_unless!(common::check_claude_available());

    run_config_request("config-gen-live", false);
}

/// Scenario: Start an interactive Haiku agent in a project with a valid user-written config, open its pane, press `g` in command mode and confirm Yes. Verify the received request names the existing file and the agent waits for approval before editing it; approve in the pane while requesting a uniquely named tester role. Check that the written config validates, adds that role to the existing orchestration, and preserves the original roles, their commands and the reviewer's description.
#[spec("dashboard/config-gen/004")]
#[test]
fn config_gen_004_a_real_agent_updates_a_config_preserving_user_content() {
    skip_unless!(common::check_claude_available());

    run_config_request("config-gen-existing-live", true);
}

fn run_config_request(fixture: &str, existing: bool) {
    let deck = TuiDeck::builder()
        .with_pty_size(140, 40)
        .with_imported_claude_credentials()
        // Trust the raw and canonical spellings of the work dir, so the agent
        // never parks on claude's per-folder trust prompt.
        .with_claude_trust_workdir()
        // The agent's `dot-agent-deck inbox` and `which` resolve this build
        // first.
        .with_env("PATH", common::path_with_binary_dir())
        // Outlast every wait below; the budget is above `BOOT_WAIT`. A literal,
        // so linkage-check rule 11 can read it.
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "840")
        .launch_with_fixture(fixture);
    deck.wait_for_string("No active agents");

    let socket = deck.attach_socket_path().to_path_buf();
    let workdir = deck.workdir().to_path_buf();
    let config_path = workdir.join(".dot-agent-deck.toml");
    let original = if existing {
        let config = std::fs::read_to_string(&config_path)
            .expect("the update fixture must already have .dot-agent-deck.toml");
        assert_user_content(&config, "initial fixture");
        validate_config(&deck, &workdir, &config);
        Some(config)
    } else {
        assert!(
            !config_path.exists(),
            "the fixture must start with no .dot-agent-deck.toml"
        );
        None
    };

    // ---- 1. A live interactive agent in the project directory.
    let events = deck.subscribe_events();
    let resp = common::attach_request_on(
        &socket,
        &AttachRequest::StartAgent {
            command: Some(AGENT_COMMAND.into()),
            cwd: Some(workdir.to_string_lossy().into_owned()),
            rows: 36,
            cols: 100,
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE_ID.to_string())],
            display_name: Some("config-agent".to_string()),
            tab_membership: None,
            agent_type: Some(AgentType::ClaudeCode),
            seed: None,
            authoring_kind: None,
            client_seeded_kind: None,
            remember_command: false,
        },
    )
    .expect("StartAgent over the attach socket");
    assert!(resp.ok, "the daemon must spawn the agent: {:?}", resp.error);
    let agent_id = resp.id.expect("StartAgent reports the agent id");

    events.wait_for_session_start_on_pane(PANE_ID, &agent_id, BOOT_WAIT);
    deck.wait_for_absence("No active agents");
    if !common::wait_until_panes_settled(
        &socket,
        std::slice::from_ref(&agent_id),
        Duration::from_millis(1500),
        Duration::from_secs(8),
        SETTLE_WAIT,
    ) {
        eprintln!("warning: the agent pane never settled; asking anyway");
    }

    // ---- 2. The user's half. Open the agent's pane (`1`: jump to card 1 and
    // focus its pane), so the agent is on screen as a user watches it; back to
    // command mode with Ctrl+D; then `g` on the selected card, and Yes.
    //
    // Opening the pane first is load-bearing, not decoration: the agent was
    // started over the socket, so this TUI has no pane open for it, and Yes on
    // a card with no open pane leaves the TUI in command mode (measured on the
    // first run of this test), where the approval typed below would be read as
    // deck commands (`n` opened a New Agent form) instead of reaching claude.
    deck.send_keys(b"1");
    assert!(
        deck.wait_for_grid_string_within(PANE_MODE_BUTTON, Duration::from_secs(30)),
        "`1` must open the agent's pane in pane input; grid:\n{}",
        deck.snapshot_grid()
    );
    deck.send_keys(b"\x04");
    deck.wait_for_string(COMMAND_MODE_BUTTON);
    deck.send_keys(b"g");
    deck.wait_for_string("[Yes]");
    deck.send_keys(b"\r");
    deck.wait_for_absence("[Yes]");

    // The request reached the agent's inbox, bound to this agent, carrying the
    // prompt the daemon composed for the agent's own directory.
    let cwd = common::agent_records_on(&socket)
        .into_iter()
        .find(|r| r.id == agent_id)
        .and_then(|r| r.cwd)
        .expect("the daemon knows the agent's cwd");
    let request = || -> Option<MessageSummary> {
        inbox_view::messages(&socket, PANE_ID)
            .into_iter()
            .find(|m| m.subject == CONFIG_GEN_SUBJECT)
    };
    assert!(
        common::wait_until(Duration::from_secs(30), || request().is_some()),
        "confirming Yes must post a {CONFIG_GEN_SUBJECT:?} message to the agent's inbox; \
         inbox={:#?}\n=== deck grid ===\n{}",
        inbox_view::messages(&socket, PANE_ID),
        deck.snapshot_grid()
    );
    let posted = request().unwrap();
    assert_eq!(posted.bind_agent.as_deref(), Some(agent_id.as_str()));
    let expected_prompt = config_gen_prompt(&cwd);
    assert!(
        posted
            .text
            .as_deref()
            .is_some_and(|t| t.contains(expected_prompt.trim())),
        "the inbox body must be the daemon-composed prompt for {cwd}"
    );
    let body = posted.text.as_deref().expect("configuration request body");
    if existing {
        let existing_file = format!("{cwd}/.dot-agent-deck.toml");
        assert!(
            body.contains(&format!("`{existing_file}` already exists")),
            "the request must name the existing file and say it already exists; \
             request:\n{body}\n{}",
            evidence(&deck, &socket, &agent_id)
        );
    } else {
        assert!(
            !body.contains("already exists"),
            "a fresh project must receive the create request; request:\n{body}"
        );
    }

    // ---- 3. The agent takes the request, analyses, proposes, and waits.
    // One deadline for the whole turn, so the two waits cannot sum past it.
    let proposal_deadline = Instant::now() + PROPOSAL_WAIT;
    assert!(
        common::wait_until(PROPOSAL_WAIT, || request()
            .is_some_and(|m| m.state == StateName::Acknowledged)),
        "the agent never acknowledged the configuration request within {}s; {}",
        PROPOSAL_WAIT.as_secs(),
        evidence(&deck, &socket, &agent_id)
    );
    assert!(
        wait_for_turn_end(
            &socket,
            &agent_id,
            proposal_deadline.saturating_duration_since(Instant::now())
        ),
        "the agent acknowledged the request but never finished its proposal turn within \
         {}s of the request; {}",
        PROPOSAL_WAIT.as_secs(),
        evidence(&deck, &socket, &agent_id)
    );
    assert_eq!(
        std::fs::read_to_string(&config_path).ok(),
        original,
        "the agent must wait for user approval before writing {}; {}",
        config_path.display(),
        evidence(&deck, &socket, &agent_id)
    );

    // ---- 4. Approve in the agent's pane. Yes left the TUI in pane input on
    // that pane, so these keys go to the agent; checked first, because in
    // command mode the same keys would drive the deck instead.
    assert!(
        deck.wait_for_grid_string_within(PANE_MODE_BUTTON, Duration::from_secs(10)),
        "Yes must leave the TUI typing into the agent's pane; grid:\n{}",
        deck.snapshot_grid()
    );
    let (approval, approval_start) = if existing {
        (UPDATE_APPROVAL, UPDATE_APPROVAL_START)
    } else {
        (APPROVAL, APPROVAL_START)
    };
    deck.send_keys(approval.as_bytes());
    // Enter only once the text is in claude's input box: an Enter that
    // overtakes a still-arriving paste can submit half of it.
    if !deck.wait_for_grid_string_within(approval_start, Duration::from_secs(20)) {
        eprintln!("warning: the approval never showed on the deck's grid; submitting anyway");
    }
    deck.send_keys(b"\r");

    let written = common::wait_until(WRITE_WAIT, || {
        std::fs::read_to_string(&config_path).is_ok_and(|c| {
            c.contains("[[orchestrations]]") && original.as_deref() != Some(c.as_str())
        })
    });
    assert!(
        written,
        "no new or updated .dot-agent-deck.toml with an [[orchestrations]] table appeared in {} within {}s of \
         the approval; {}",
        workdir.display(),
        WRITE_WAIT.as_secs(),
        evidence(&deck, &socket, &agent_id)
    );
    // The Write tool may still be finishing; let the turn end before reading.
    let _ = wait_for_turn_end(&socket, &agent_id, WRITE_SETTLE);
    let config = std::fs::read_to_string(&config_path).unwrap();

    // ---- 5. The deck accepts the file.
    validate_config(&deck, &workdir, &config);
    if existing {
        // Include the pane output if a real agent drops user-owned content:
        // that is evidence about the update request, not a reason to weaken it.
        assert_user_content(&config, &evidence(&deck, &socket, &agent_id));
        let parsed: toml::Value = toml::from_str(&config).expect("config must be valid TOML");
        let added = user_roles(&parsed).iter().any(|role| {
            role.get("name").and_then(toml::Value::as_str) == Some(UPDATE_ROLE)
                && role.get("command").and_then(toml::Value::as_str)
                    == Some(format!("make {SENTINEL}").as_str())
        });
        assert!(
            added,
            "the update must add role {UPDATE_ROLE:?} with command `make {SENTINEL}` to \
             orchestration {USER_ORCHESTRATION:?}; config:\n{config}\n{}",
            evidence(&deck, &socket, &agent_id)
        );
    }

    // ---- 6. The agent analysed THIS project.
    let pane = common::pane_search_key_on(&socket, &agent_id);
    assert!(
        config.contains(SENTINEL) || pane.contains(&common::search_key(SENTINEL)),
        "neither the agent's proposal nor the written config names the fixture's launcher \
         {SENTINEL:?}, so nothing shows it analysed the project.\n=== .dot-agent-deck.toml \
         ===\n{config}\n{}",
        evidence(&deck, &socket, &agent_id)
    );
    eprintln!(
        "narrative (soft): launcher {SENTINEL:?} in the written config = {}",
        config.contains(SENTINEL)
    );
}

fn validate_config(deck: &TuiDeck, workdir: &Path, config: &str) {
    let validate = Command::new(common::contained_deck_binary())
        .arg("validate")
        .arg("--path")
        .arg(workdir)
        .env("HOME", deck.home_dir())
        .output()
        .expect("run dot-agent-deck validate");
    assert!(
        validate.status.success(),
        "`dot-agent-deck validate` rejected the generated config:\nstdout: {}\nstderr: {}\n\
         === .dot-agent-deck.toml ===\n{config}",
        String::from_utf8_lossy(&validate.stdout),
        String::from_utf8_lossy(&validate.stderr)
    );
}

fn assert_user_content(config: &str, evidence: &str) {
    let parsed: toml::Value = toml::from_str(config).expect("config must be valid TOML");
    let roles = user_roles(&parsed);
    let command = format!("make {SENTINEL}");
    for name in ["orchestrator", "reviewer"] {
        assert!(
            roles.iter().any(|role| {
                role.get("name").and_then(toml::Value::as_str) == Some(name)
                    && role.get("command").and_then(toml::Value::as_str) == Some(command.as_str())
            }),
            "the update must preserve orchestration {USER_ORCHESTRATION:?} and role \
             {name:?} with command {command:?}; config:\n{config}\n{evidence}"
        );
    }
    assert!(
        roles.iter().any(|role| {
            role.get("name").and_then(toml::Value::as_str) == Some("reviewer")
                && role.get("description").and_then(toml::Value::as_str) == Some(USER_DESCRIPTION)
        }),
        "the update must preserve user orchestration {USER_ORCHESTRATION:?} and its \
         reviewer description {USER_DESCRIPTION:?}; config:\n{config}\n{evidence}"
    );
}

fn user_roles(parsed: &toml::Value) -> &[toml::Value] {
    parsed
        .get("orchestrations")
        .and_then(toml::Value::as_array)
        .and_then(|orchestrations| {
            orchestrations
                .iter()
                .find(|o| o.get("name").and_then(toml::Value::as_str) == Some(USER_ORCHESTRATION))
        })
        .and_then(|o| o.get("roles"))
        .and_then(toml::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// Wait until the agent is Idle and stays Idle for [`IDLE_HOLD`]: the end of a
/// turn, which for the proposal turn is the agent waiting on the user.
fn wait_for_turn_end(socket: &Path, agent_id: &str, timeout: Duration) -> bool {
    let idle_since: Cell<Option<Instant>> = Cell::new(None);
    common::wait_until(timeout, || {
        let idle = common::agent_records_on(socket)
            .into_iter()
            .find(|r| r.id == agent_id)
            .and_then(|r| r.live)
            .is_some_and(|l| l.status == SessionStatus::Idle);
        if !idle {
            idle_since.set(None);
            return false;
        }
        let since = idle_since.get().unwrap_or_else(Instant::now);
        idle_since.set(Some(since));
        since.elapsed() >= IDLE_HOLD
    })
}

/// The agent's status, its inbox, the tail of its pane and the deck's grid, for
/// a failure message.
fn evidence(deck: &TuiDeck, socket: &Path, agent_id: &str) -> String {
    let status = common::agent_records_on(socket)
        .into_iter()
        .find(|r| r.id == agent_id)
        .and_then(|r| r.live)
        .map(|l| format!("{:?}", l.status));
    format!(
        "status={status:?}\ninbox={:#?}\n=== agent pane (normalized, tail) ===\n{}\n\
         === deck grid ===\n{}",
        inbox_view::messages(socket, PANE_ID)
            .into_iter()
            .map(|m| (m.subject, m.state))
            .collect::<Vec<_>>(),
        tail(&common::pane_search_key_on(socket, agent_id)),
        deck.snapshot_grid()
    )
}

/// The last ~1500 characters of a normalized pane dump.
fn tail(text: &str) -> &str {
    const KEEP: usize = 1500;
    if text.len() <= KEEP {
        return text;
    }
    let mut start = text.len() - KEEP;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}
