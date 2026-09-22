#![cfg(all(feature = "e2e", unix))]

//! Lane-1 reproduction coverage for PRD #1223's report that agents started
//! through the desktop's daemon verbs do not appear correctly in the TUI.
//! The desktop GUI itself has no driver tier, so these scenarios drive the
//! exact attach requests its actions issue and then observe the real TUI in a
//! PTY. Each starts with the equivalent TUI-native launch as a control.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{DaemonProc, TuiDeck};
use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_PANE_ID, TabMembership};
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::AgentType;
use spec::spec;

const PLAIN_LABEL: &str = "desktop-visible-agent";
const PLAIN_COMMAND: &str = "sleep 600";
const ORCHESTRATION_NAME: &str = "desktop-visibility-team";
const ORCHESTRATION_TITLE: &str = "Desktop prepared run";
const ORCHESTRATION_ID: &str = "desktop-visibility-orchestration";
const ORCHESTRATION_ROLES: [&str; 3] = ["coordinator", "builder", "reviewer"];
const ORCHESTRATION_CONFIG: &str = include_str!("fixtures/desktop-visibility/.dot-agent-deck.toml");

/// Launch the real TUI binary in a PTY against an already-running daemon.
/// No key is sent by this helper: callers can distinguish startup hydration
/// from a card that appears only after selection or another interaction.
fn launch_tui_against(daemon: &DaemonProc) -> TuiDeck {
    TuiDeck::builder()
        .with_pty_size(120, 40)
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            daemon.attach_socket.to_string_lossy().to_string(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            daemon.hook_socket.to_string_lossy().to_string(),
        )
        .launch_with_fixture("minimal")
}

/// Use the real TUI new-agent form to start a named plain pane. This is the
/// control for the desktop's direct `StartAgent` request.
fn start_plain_from_tui(deck: &TuiDeck) {
    deck.wait_for_string("No active sessions");
    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // confirm the fixture cwd
    deck.wait_for_string("No mode");
    deck.send_keys(b"\r"); // Mode -> Name
    deck.send_keys(PLAIN_LABEL.as_bytes());
    deck.send_keys(b"\r"); // Name -> Command
    deck.send_keys(PLAIN_COMMAND.as_bytes());
    deck.send_keys(b"\r"); // submit
}

/// Use the real TUI new-agent form to launch the fixture's orchestration. This
/// is the control for the desktop's PrepareWorkflow + StartPreparedAgent loop.
fn start_orchestration_from_tui(deck: &TuiDeck) {
    deck.wait_for_string("No active sessions");
    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // confirm the fixture cwd
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C"); // select the fixture's only orchestration
    deck.send_keys(b"\r"); // Mode -> Name
    deck.send_keys(b"\r"); // keep the default title and submit
}

fn canonical_string(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|error| panic!("canonicalize {}: {error}", path.display()))
        .to_string_lossy()
        .into_owned()
}

/// Deterministic representative of `mint_desktop_pane_id()`'s
/// `desktop-{nonce:016x}-{sequence}` wire format.
fn desktop_pane_id(sequence: usize) -> String {
    format!("desktop-0123456789abcdef-{sequence}")
}

fn missing_roles(grid: &str) -> Vec<&'static str> {
    ORCHESTRATION_ROLES
        .iter()
        .copied()
        .filter(|role| !grid.contains(role))
        .collect()
}

/// Send the plain `StartAgent` shape built by the desktop action. The explicit
/// type is inferred from the command exactly as `start_agent_action` does.
fn start_plain_from_desktop(daemon: &DaemonProc, cwd: String, pane_id: &str) {
    let response = daemon
        .send_attach_request(&AttachRequest::StartAgent {
            command: Some(PLAIN_COMMAND.into()),
            cwd: Some(cwd),
            rows: 24,
            cols: 80,
            env: vec![(DOT_AGENT_DECK_PANE_ID.into(), pane_id.into())],
            display_name: Some(PLAIN_LABEL.into()),
            tab_membership: None,
            agent_type: AgentType::from_command(Some(PLAIN_COMMAND)),
            seed: None,
            authoring_kind: None,
        })
        .expect("desktop-shaped StartAgent over the attach socket");
    assert!(
        response.ok,
        "desktop-shaped StartAgent must succeed before visibility can be observed: {:?}",
        response.error
    );
}

/// Drive the desktop's empty-task preparation and configured-role loop. Its
/// dimensions are the desktop orchestration defaults (32×120), distinct from
/// the plain action's 24×80 defaults.
fn start_orchestration_from_desktop(daemon: &DaemonProc, project_path: &str) {
    let response = daemon
        .send_attach_request(&AttachRequest::PrepareWorkflow {
            path: project_path.into(),
            orchestration: ORCHESTRATION_NAME.into(),
            task: String::new(),
            config_revision: None,
        })
        .expect("desktop-shaped PrepareWorkflow over the attach socket");
    assert!(
        response.ok,
        "desktop-shaped PrepareWorkflow must succeed before roles can start: {:?}",
        response.error
    );
    let prepared = response
        .workflow_prepared
        .expect("successful PrepareWorkflow must return its roles and token");
    assert_eq!(
        prepared.roles.len(),
        ORCHESTRATION_ROLES.len(),
        "the fixture must prepare the same three roles as the TUI control"
    );

    for (role_index, role) in prepared.roles.iter().enumerate() {
        let pane_id = desktop_pane_id(role_index);
        let response = daemon
            .send_attach_request(&AttachRequest::StartPreparedAgent {
                prep_token: prepared.token.clone(),
                command: None,
                cwd: Some(project_path.into()),
                rows: 32,
                cols: 120,
                env: vec![(DOT_AGENT_DECK_PANE_ID.into(), pane_id)],
                display_name: Some(role.name.clone()),
                tab_membership: Some(TabMembership::Orchestration {
                    name: ORCHESTRATION_NAME.into(),
                    role_index,
                    role_name: role.name.clone(),
                    is_start_role: role.start,
                    orchestration_cwd: Some(project_path.into()),
                    display_title: Some(ORCHESTRATION_TITLE.into()),
                    orchestration_id: Some(ORCHESTRATION_ID.into()),
                }),
                agent_type: None,
                seed: None,
                use_configured_command: true,
            })
            .expect("desktop-shaped StartPreparedAgent over the attach socket");
        assert!(
            response.ok,
            "desktop-shaped start for role {:?} must succeed: {:?}",
            role.name, response.error
        );
    }
}

fn write_orchestration_project() -> tempfile::TempDir {
    let project = common::harness_tempdir().expect("create desktop project");
    std::fs::write(
        project.path().join(".dot-agent-deck.toml"),
        ORCHESTRATION_CONFIG,
    )
    .expect("write the desktop orchestration fixture");
    project
}

/// Scenario: First start a named plain pane through the real TUI form and
/// confirm its dashboard card renders. Then send the desktop-shaped
/// `StartAgent` request to a daemon with no TUI attached, attach a fresh real
/// TUI to that daemon, and require the same named card to appear on the
/// dashboard without sending any key or selection input.
#[spec("newagent/visibility/001")]
#[test]
fn visibility_001_desktop_started_plain_agent_appears_without_selection() {
    // Control: the equivalent TUI-native launch is visibly represented.
    let control = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("minimal");
    start_plain_from_tui(&control);
    control.wait_until_grid("TUI-started plain agent is visible", |grid| {
        grid.contains(PLAIN_LABEL)
    });
    drop(control);

    // Reproduction: the desktop sends StartAgent before this TUI exists.
    let daemon = common::spawn_daemon_serve(None, "0");
    let cwd = common::harness_tempdir().expect("create desktop-selected cwd");
    let canonical_cwd = canonical_string(cwd.path());
    start_plain_from_desktop(&daemon, canonical_cwd, &desktop_pane_id(0));
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(10));
    assert_eq!(
        records[0].display_name.as_deref(),
        Some(PLAIN_LABEL),
        "precondition: the daemon registry must carry the desktop display name"
    );

    let deck = launch_tui_against(&daemon);
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            deck.snapshot_grid().contains(PLAIN_LABEL)
        }),
        "a fresh TUI attached after the desktop-shaped StartAgent must show the named agent on \
         its dashboard WITHOUT any keypress or selection, but {PLAIN_LABEL:?} never appeared.\n\
         Final grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Keep a real TUI attached to an empty daemon, then send the same
/// desktop-shaped plain `StartAgent` request used by the fresh-attach control.
/// Without any keypress, selection, or agent hook, the newly accepted agent
/// must surface as a named dashboard card just as a TUI-native start does.
#[spec("newagent/visibility/001")]
#[test]
fn visibility_001_desktop_started_plain_agent_surfaces_into_attached_dashboard() {
    // Control: a TUI-native start creates its card synchronously, even for the
    // same hookless stand-in used below.
    let control = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("minimal");
    start_plain_from_tui(&control);
    control.wait_until_grid("TUI-started plain agent is visible", |grid| {
        grid.contains(PLAIN_LABEL)
    });
    drop(control);

    let daemon = common::spawn_daemon_serve(None, "0");
    let deck = launch_tui_against(&daemon);
    deck.wait_for_string("No active sessions");
    let cwd = common::harness_tempdir().expect("create desktop-selected cwd");
    start_plain_from_desktop(&daemon, canonical_string(cwd.path()), &desktop_pane_id(0));
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(10));

    assert!(
        common::wait_until(Duration::from_secs(10), || {
            deck.snapshot_grid().contains(PLAIN_LABEL)
        }),
        "the already-attached TUI must show the desktop-started agent WITHOUT a keypress, \
         selection, reconnect, or agent hook, but {PLAIN_LABEL:?} never appeared even though \
         the daemon registered it. Records: {records:#?}\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: First launch a three-role orchestration through the real TUI form
/// and confirm it creates a separate tab with every role visible. Then drive
/// the desktop's empty-task `PrepareWorkflow` plus one configured
/// `StartPreparedAgent` per role against a headless daemon, attach a fresh real
/// TUI without sending input, and require the named orchestration tab to be
/// rebuilt with all three role cards.
#[spec("newagent/visibility/002")]
#[test]
fn visibility_002_desktop_prepared_orchestration_rebuilds_its_tab_with_every_role() {
    // Control: the TUI's own role loop creates one orchestration tab carrying
    // all three configured roles.
    let control = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("desktop-visibility");
    start_orchestration_from_tui(&control);
    control.wait_until_grid("TUI-started orchestration tab carries every role", |grid| {
        grid.lines()
            .next()
            .is_some_and(|tabs| tabs.contains("Dashboard") && tabs.contains('│'))
            && missing_roles(grid).is_empty()
    });
    drop(control);

    // Reproduction: prepare and start every role through the desktop's daemon
    // sequence while there is no TUI attached.
    let daemon = common::spawn_daemon_serve(None, "0");
    let project = write_orchestration_project();
    let project_path = canonical_string(project.path());
    start_orchestration_from_desktop(&daemon, &project_path);

    let records = daemon.wait_for_agent_count(ORCHESTRATION_ROLES.len(), Duration::from_secs(10));
    let registered_roles: Vec<_> = records
        .iter()
        .map(|record| {
            (
                record.display_name.clone(),
                record.agent_type.clone(),
                record.tab_membership.clone(),
                record.pane_id_env.clone(),
            )
        })
        .collect();

    let deck = launch_tui_against(&daemon);
    assert!(
        common::wait_until(Duration::from_secs(15), || {
            let grid = deck.snapshot_grid();
            grid.lines().next().is_some_and(|tabs| {
                tabs.contains("Dashboard") && tabs.contains(ORCHESTRATION_TITLE)
            }) && missing_roles(&grid).is_empty()
        }),
        "a fresh TUI attached after the desktop's prepared-role loop must rebuild a distinct \
         tab titled {ORCHESTRATION_TITLE:?} with EVERY role visible. Missing roles: {:?}.\n\
         Daemon role metadata (display name, agent type, membership, pane id): {registered_roles:#?}\n\
         Final grid:\n{}",
        missing_roles(&deck.snapshot_grid()),
        deck.snapshot_grid()
    );
}

/// Scenario: Keep a real TUI attached to an empty daemon, then launch the
/// desktop's empty-task prepared orchestration. Inject one ordinary
/// `SessionStart` per registered role so all three role agents are visibly
/// present without relying on the stand-ins to emit hooks; the TUI must group
/// those roles into the same separate, titled orchestration tab that its own
/// launch control creates, without a reconnect or user interaction.
#[spec("newagent/visibility/002")]
#[test]
fn visibility_002_desktop_prepared_orchestration_surfaces_into_attached_tui_as_own_tab() {
    // Control: the TUI's own StartAgent role loop builds the separate tab.
    let control = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("desktop-visibility");
    start_orchestration_from_tui(&control);
    control.wait_until_grid("TUI-started orchestration tab carries every role", |grid| {
        grid.lines()
            .next()
            .is_some_and(|tabs| tabs.contains("Dashboard") && tabs.contains('│'))
            && missing_roles(grid).is_empty()
    });
    drop(control);

    let daemon = common::spawn_daemon_serve(None, "0");
    let deck = launch_tui_against(&daemon);
    deck.wait_for_string("No active sessions");
    let project = write_orchestration_project();
    let project_path = canonical_string(project.path());
    start_orchestration_from_desktop(&daemon, &project_path);
    let records = daemon.wait_for_agent_count(ORCHESTRATION_ROLES.len(), Duration::from_secs(10));

    // A real agent's hook can surface a flat card, but it carries no structural
    // tab membership. Inject all three so a missing tab cannot be mistaken for
    // the hookless stand-ins merely not announcing themselves.
    for (index, record) in records.iter().enumerate() {
        let pane_id = record
            .pane_id_env
            .as_deref()
            .unwrap_or_else(|| panic!("role record must carry its desktop pane id: {record:?}"));
        let event = serde_json::json!({
            "session_id": format!("desktop-visibility-session-{index}"),
            "agent_type": record.agent_type.clone().unwrap_or(AgentType::None),
            "event_type": "session_start",
            "timestamp": "2026-09-22T12:00:00Z",
            "pane_id": pane_id,
            "agent_id": record.id,
        });
        common::write_hook_line(&daemon.hook_socket, &event.to_string())
            .expect("inject the role's SessionStart through the real hook socket");
    }
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            let grid = deck.snapshot_grid();
            grid.contains("3 session(s)")
                && grid.contains("ClaudeCode")
                && grid.contains("OpenCode")
                && grid.contains("Pi")
        }),
        "precondition: all three desktop-started agents must produce visible cards after their \
         ordinary SessionStart hooks, distinguished by ClaudeCode/OpenCode/Pi. \
         Records: {records:#?}\nGrid:\n{}",
        deck.snapshot_grid()
    );

    assert!(
        common::wait_until(Duration::from_secs(10), || {
            deck.snapshot_grid().lines().next().is_some_and(|tabs| {
                tabs.contains("Dashboard") && tabs.contains(ORCHESTRATION_TITLE)
            })
        }),
        "all desktop-started roles are visible, but the already-attached TUI never grouped them \
         into their own tab titled {ORCHESTRATION_TITLE:?}. Missing roles: {:?}.\n\
         Role distinctions (start flag, agent type and every membership field): {records:#?}\n\
         Final grid:\n{}",
        missing_roles(&deck.snapshot_grid()),
        deck.snapshot_grid()
    );
}
