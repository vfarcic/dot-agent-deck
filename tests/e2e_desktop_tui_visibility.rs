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

/// Send the same single-agent stop request as the desktop's
/// `stop_agent_action`, and require the daemon to confirm it.
fn stop_agent_from_desktop(daemon: &DaemonProc, agent_id: &str) {
    let response = daemon
        .send_attach_request(&AttachRequest::StopAgent {
            id: agent_id.into(),
        })
        .expect("desktop-shaped StopAgent over the attach socket");
    assert!(
        response.ok,
        "desktop-shaped StopAgent for {agent_id:?} must succeed: {:?}",
        response.error
    );
}

/// Mirror `stop_orchestration_action`: issue one independent `StopAgent` per
/// role concurrently, then require every stop to have been confirmed.
fn stop_orchestration_from_desktop(daemon: &DaemonProc, role_agents: &[(String, String)]) {
    let attach_socket = daemon.attach_socket.clone();
    let outcomes = std::thread::scope(|scope| {
        role_agents
            .iter()
            .map(|(role, agent_id)| {
                let role = role.clone();
                let agent_id = agent_id.clone();
                let attach_socket = attach_socket.clone();
                scope.spawn(move || {
                    let response = common::attach_request_on(
                        &attach_socket,
                        &AttachRequest::StopAgent {
                            id: agent_id.clone(),
                        },
                    );
                    (role, agent_id, response)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("desktop role-stop thread"))
            .collect::<Vec<_>>()
    });

    for (role, agent_id, response) in outcomes {
        let response = response.unwrap_or_else(|error| {
            panic!("desktop-shaped StopAgent for role {role:?} ({agent_id}) failed: {error}")
        });
        assert!(
            response.ok,
            "desktop-shaped StopAgent for role {role:?} ({agent_id}) must succeed: {:?}",
            response.error
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
/// desktop's empty-task prepared orchestration. The titled tab must appear
/// without a reconnect, and switching into it must show all three role-named
/// cards with their declared agent types.
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

    deck.wait_until_grid("desktop-started orchestration tab appears", |grid| {
        grid.lines()
            .next()
            .is_some_and(|tabs| tabs.contains("Dashboard") && tabs.contains(ORCHESTRATION_TITLE))
    });
    deck.send_bytes(b"\x1b[C"); // Right -> next tab -> Desktop prepared run

    assert!(
        common::wait_until(Duration::from_secs(10), || {
            let grid = deck.snapshot_grid();
            grid.lines().next().is_some_and(|tabs| {
                tabs.contains("Dashboard") && tabs.contains(ORCHESTRATION_TITLE)
            }) && grid.contains("3 session(s)")
                && grid.contains("ClaudeCode · coordinator")
                && grid.contains("OpenCode · builder")
                && grid.contains("Pi · reviewer")
        }),
        "the already-attached TUI created tab {ORCHESTRATION_TITLE:?}, but switching into it \
         did not show exactly three sessions with the role-named ClaudeCode coordinator, \
         OpenCode builder, and Pi reviewer cards. Role metadata: {records:#?}\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Start the same named plain agent through the desktop-shaped
/// request twice against already-attached TUIs. A confirmed TUI-native close is
/// the control; a desktop-shaped `StopAgent` must remove the second card from
/// the live dashboard just as completely, without any TUI input or reconnect.
#[spec("newagent/visibility/003")]
#[test]
fn visibility_003_desktop_stop_removes_plain_agent_from_attached_dashboard() {
    // Control: stop a desktop-started card through the TUI's own confirmed
    // Ctrl+W path. This proves the card and pane can be removed normally.
    let control_daemon = common::spawn_daemon_serve(None, "0");
    let control_deck = launch_tui_against(&control_daemon);
    control_deck.wait_for_string("No active sessions");
    let control_cwd = common::harness_tempdir().expect("create control cwd");
    start_plain_from_desktop(
        &control_daemon,
        canonical_string(control_cwd.path()),
        &desktop_pane_id(0),
    );
    control_deck.wait_for_string(PLAIN_LABEL);
    let (label_col, label_row) = control_deck.wait_for_in_grid(PLAIN_LABEL);
    control_deck.click(label_col, label_row);
    control_deck.send_keys(b"\x17"); // Ctrl+W -> close confirmation
    control_deck.wait_for_string("Close selected pane?");
    control_deck.send_keys(b"\x1b[B"); // Down -> Close
    control_deck.send_keys(b"\r");
    control_deck.wait_until_grid("TUI-native stop removes the desktop-started card", |grid| {
        grid.contains("No active sessions") && !grid.contains(PLAIN_LABEL)
    });
    assert!(
        common::wait_until(Duration::from_secs(10), || control_daemon
            .agent_records()
            .is_empty()),
        "control: the TUI-native close must empty the daemon registry"
    );
    drop(control_deck);
    drop(control_daemon);

    // Reproduction: the TUI stays untouched after the desktop-shaped stop.
    let daemon = common::spawn_daemon_serve(None, "0");
    let deck = launch_tui_against(&daemon);
    deck.wait_for_string("No active sessions");
    let cwd = common::harness_tempdir().expect("create desktop-selected cwd");
    start_plain_from_desktop(&daemon, canonical_string(cwd.path()), &desktop_pane_id(0));
    deck.wait_for_string(PLAIN_LABEL);
    let record = daemon
        .wait_for_agent_count(1, Duration::from_secs(10))
        .into_iter()
        .next()
        .expect("desktop-started plain agent in daemon registry");

    // Subscribe only after the start-side surface has landed, so this buffer is
    // diagnostic evidence about the stop rather than the synthetic start.
    let stop_events = daemon.subscribe_events();
    stop_agent_from_desktop(&daemon, &record.id);
    let registry_empty = common::wait_until(Duration::from_secs(10), || {
        daemon.agent_records().is_empty()
    });
    let card_disappeared = common::wait_until(Duration::from_secs(15), || {
        let grid = deck.snapshot_grid();
        grid.contains("No active sessions") && !grid.contains(PLAIN_LABEL)
    });
    let final_grid = deck.snapshot_grid();
    let published_events = stop_events.snapshot();

    assert!(
        registry_empty,
        "desktop-shaped StopAgent was accepted but the daemon registry did not empty. \
         Records: {:#?}",
        daemon.agent_records()
    );
    assert!(
        card_disappeared,
        "the desktop-shaped StopAgent emptied the daemon registry, but the already-attached \
         TUI did not remove {PLAIN_LABEL:?} WITHOUT a keypress, reconnect, or manual refresh. \
         AgentEvents published after stop: {published_events:#?}\nFinal grid:\n{final_grid}"
    );
}

/// Scenario: Start the same prepared three-role orchestration twice through
/// the desktop-shaped request sequence. A confirmed TUI-native tab close is the
/// control; concurrently stopping every role the desktop's way must remove the
/// second now-empty tab without TUI input or reconnect.
#[spec("newagent/visibility/004")]
#[test]
fn visibility_004_desktop_close_removes_orchestration_tab_from_attached_tui() {
    // Control: close the desktop-created orchestration through the TUI. The
    // native path stops all roles concurrently and removes a clean tab.
    let control_daemon = common::spawn_daemon_serve(None, "0");
    let control_deck = launch_tui_against(&control_daemon);
    control_deck.wait_for_string("No active sessions");
    let control_project = write_orchestration_project();
    let control_project_path = canonical_string(control_project.path());
    start_orchestration_from_desktop(&control_daemon, &control_project_path);
    control_deck.wait_until_grid("control orchestration tab appears", |grid| {
        grid.lines()
            .next()
            .is_some_and(|tabs| tabs.contains(ORCHESTRATION_TITLE))
    });
    control_deck.send_keys(b"\x1b[C"); // Right -> Desktop prepared run
    control_deck.wait_for_string("3 session(s)");
    control_deck.send_keys(b"\x17"); // Ctrl+W -> whole-tab confirmation
    control_deck.wait_for_string("Close this tab and all its panes?");
    control_deck.send_keys(b"\x1b[B"); // Down -> Close
    control_deck.send_keys(b"\r");
    control_deck.wait_until_grid("TUI-native close removes the orchestration tab", |grid| {
        grid.contains("No active sessions") && !grid.contains(ORCHESTRATION_TITLE)
    });
    assert!(
        common::wait_until(Duration::from_secs(10), || control_daemon
            .agent_records()
            .is_empty()),
        "control: the TUI-native whole-tab close must empty the daemon registry"
    );
    drop(control_deck);
    drop(control_daemon);

    // Reproduction: close every listed role concurrently, exactly as the
    // desktop action does, while leaving the attached TUI untouched.
    let daemon = common::spawn_daemon_serve(None, "0");
    let deck = launch_tui_against(&daemon);
    deck.wait_for_string("No active sessions");
    let project = write_orchestration_project();
    let project_path = canonical_string(project.path());
    start_orchestration_from_desktop(&daemon, &project_path);
    deck.wait_until_grid("desktop-started orchestration tab appears", |grid| {
        grid.lines()
            .next()
            .is_some_and(|tabs| tabs.contains(ORCHESTRATION_TITLE))
    });
    let records = daemon.wait_for_agent_count(ORCHESTRATION_ROLES.len(), Duration::from_secs(10));
    assert_eq!(
        records.len(),
        ORCHESTRATION_ROLES.len(),
        "precondition: every desktop-started role must be registered"
    );
    let role_agents: Vec<_> = records
        .iter()
        .map(|record| {
            (
                record
                    .display_name
                    .clone()
                    .unwrap_or_else(|| "<unnamed role>".into()),
                record.id.clone(),
            )
        })
        .collect();

    let stop_events = daemon.subscribe_events();
    stop_orchestration_from_desktop(&daemon, &role_agents);
    let registry_empty = common::wait_until(Duration::from_secs(10), || {
        daemon.agent_records().is_empty()
    });
    let tab_disappeared = common::wait_until(Duration::from_secs(15), || {
        let grid = deck.snapshot_grid();
        grid.contains("No active sessions") && !grid.contains(ORCHESTRATION_TITLE)
    });
    let final_grid = deck.snapshot_grid();
    let published_events = stop_events.snapshot();

    assert!(
        registry_empty,
        "desktop-shaped orchestration close returned but the daemon registry did not empty. \
         Role agents: {role_agents:#?}; records: {:#?}",
        daemon.agent_records()
    );
    assert!(
        tab_disappeared,
        "concurrent desktop-shaped StopAgent requests emptied every role from the daemon, but \
         the already-attached TUI did not remove the now-empty {ORCHESTRATION_TITLE:?} tab \
         WITHOUT a keypress, reconnect, or manual refresh. Role agents: {role_agents:#?}; \
         AgentEvents published after stops: {published_events:#?}\nFinal grid:\n{final_grid}"
    );
}
