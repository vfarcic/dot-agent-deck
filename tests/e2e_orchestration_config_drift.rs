#![cfg(all(feature = "e2e", unix))]

//! Issue #554: a TUI reattaching to a daemon whose orchestration no longer
//! matches `.dot-agent-deck.toml` rebuilds the tab from the daemon's role
//! metadata — and now says so, instead of logging one `info!` line.
//!
//! Each scenario seeds a warm daemon with a two-role orchestration exactly the
//! way a detached TUI leaves one behind (role agents carrying
//! `TabMembership::Orchestration`), edits the project config, then attaches the
//! real binary in a PTY and watches what it paints and what it prints on exit.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{DaemonProc, TuiDeck};
use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_PANE_ID, TabMembership};
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::AgentType;
use spec::spec;

const ORCHESTRATION: &str = "review-team";
const ROLES: [&str; 2] = ["lead", "coder"];
const MARKER: &str = "[config drift]";

fn canonical_string(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|error| panic!("canonicalize {}: {error}", path.display()))
        .to_string_lossy()
        .into_owned()
}

fn write_config(dir: &Path, orchestration: &str, roles: &[&str]) {
    let mut toml = format!("[[orchestrations]]\nname = \"{orchestration}\"\n");
    for (i, role) in roles.iter().enumerate() {
        toml.push_str(&format!(
            "\n[[orchestrations.roles]]\nname = \"{role}\"\ncommand = \"sleep 600\"\n{}",
            if i == 0 { "start = true\n" } else { "" }
        ));
    }
    std::fs::write(dir.join(".dot-agent-deck.toml"), toml).expect("write project config");
}

/// Start `ROLES` under `daemon` as one orchestration in `cwd`, stamped the way
/// the TUI's own Ctrl+n launch stamps them.
fn seed_orchestration(daemon: &DaemonProc, cwd: &str) {
    for (role_index, role) in ROLES.iter().enumerate() {
        let response = daemon
            .send_attach_request(&AttachRequest::StartAgent {
                command: Some("sleep 600".into()),
                cwd: Some(cwd.into()),
                rows: 24,
                cols: 80,
                env: vec![(
                    DOT_AGENT_DECK_PANE_ID.into(),
                    format!("drift-0123456789abcdef-{role_index}"),
                )],
                display_name: Some((*role).into()),
                tab_membership: Some(TabMembership::Orchestration {
                    name: ORCHESTRATION.into(),
                    role_index,
                    role_name: (*role).into(),
                    is_start_role: role_index == 0,
                    orchestration_cwd: Some(cwd.into()),
                    display_title: None,
                    orchestration_id: Some("drift-instance".into()),
                }),
                agent_type: AgentType::from_command(Some("sleep 600")),
                seed: None,
                authoring_kind: None,
            })
            .expect("StartAgent over the attach socket");
        assert!(
            response.ok,
            "seeding role {role:?} must succeed: {:?}",
            response.error
        );
    }
    daemon.wait_for_agent_count(ROLES.len(), Duration::from_secs(10));
}

fn launch_tui_against(daemon: &DaemonProc) -> TuiDeck {
    TuiDeck::builder()
        .with_pty_size(160, 40)
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

/// The first grid row — the tab strip.
fn tab_strip(deck: &TuiDeck) -> String {
    deck.snapshot_grid()
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Wait for the rebuilt orchestration tab to appear in the strip.
fn wait_for_rebuilt_tab(deck: &TuiDeck) {
    deck.wait_until_grid("the reattached orchestration tab is in the strip", |grid| {
        grid.lines()
            .next()
            .is_some_and(|tabs| tabs.contains("Dashboard") && tabs.contains(ORCHESTRATION))
    });
}

/// Detach-quit through the real quit dialog so `session_warnings` are flushed,
/// then wait for the process to exit and its bytes to drain.
fn detach_quit(deck: &mut TuiDeck) {
    if deck.snapshot_grid().contains("[Command Mode Ctrl+D]") {
        deck.send_keys(b"\x04"); // Ctrl+D → command mode, so Ctrl+C reaches the deck
        deck.wait_for_absence("[Command Mode Ctrl+D]");
    }
    deck.send_keys(b"\x03"); // Ctrl+C → quit-confirm modal
    deck.wait_for_string("Quit dot-agent-deck?");
    deck.send_keys(b"\r"); // Enter → Detach (default)
    assert_eq!(
        deck.wait_for_exit_within(Duration::from_secs(15)),
        Some(true),
        "the detach-quit must exit cleanly"
    );
}

/// Scenario: Seed a warm daemon with a two-role `review-team` orchestration,
/// then attach the real TUI three times. With a config that still lists it, the
/// tab reattaches with no drift marker and nothing about drift is printed on
/// exit (the control). After renaming the orchestration in
/// `.dot-agent-deck.toml`, the reattached tab is labelled
/// `review-team [config drift]`, the status line explains the marker, and the
/// detach-quit prints a warning naming the orchestration, its directory and the
/// roles it is running. After instead renaming a role (`coder` → `qa`), the tab
/// is marked again and the exit warning names both role lists. With the file
/// deleted — the branch a remote reconnect takes — the tab reattaches unmarked
/// and nothing about drift is printed.
#[spec("session/restore/020")]
#[test]
fn restore_020_reattach_surfaces_orchestration_config_drift() {
    let daemon = common::spawn_daemon_serve(None, "0");
    let project = common::harness_tempdir().expect("create project dir");
    let cwd = canonical_string(project.path());
    write_config(project.path(), ORCHESTRATION, &ROLES);
    seed_orchestration(&daemon, &cwd);

    // Control: the file matches what the daemon is running.
    let mut control = launch_tui_against(&daemon);
    wait_for_rebuilt_tab(&control);
    let strip = tab_strip(&control);
    assert!(
        !strip.contains(MARKER),
        "a config that matches the running orchestration must not mark the tab; strip = {strip:?}"
    );
    detach_quit(&mut control);
    let control_stream = control.stream_text();
    assert!(
        !control_stream.contains("config drift"),
        "a matching config must print no drift warning on exit.\nStream tail:\n{}",
        control_stream
            .chars()
            .rev()
            .take(2000)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    );
    drop(control);

    // Drift: the orchestration was renamed in the file.
    write_config(project.path(), "renamed-team", &ROLES);
    let mut renamed = launch_tui_against(&daemon);
    wait_for_rebuilt_tab(&renamed);
    renamed.wait_until_grid("the drifted tab carries the marker", |grid| {
        grid.lines()
            .next()
            .is_some_and(|tabs| tabs.contains(&format!("{ORCHESTRATION} {MARKER}")))
    });
    assert!(
        renamed.wait_for_grid_string_within("Config drift:", Duration::from_secs(5)),
        "the status line must explain the marker when the tab appears.\nGrid:\n{}",
        renamed.snapshot_grid()
    );
    detach_quit(&mut renamed);
    let stream = renamed.stream_text();
    for needle in [
        format!("orchestration '{ORCHESTRATION}' is not listed in {cwd}/.dot-agent-deck.toml"),
        "(lead, coder)".to_string(),
    ] {
        assert!(
            stream.contains(&needle),
            "the exit warning must contain {needle:?}"
        );
    }
    drop(renamed);

    // Drift: a role was renamed; the orchestration name still matches.
    write_config(project.path(), ORCHESTRATION, &["lead", "qa"]);
    let mut role_renamed = launch_tui_against(&daemon);
    wait_for_rebuilt_tab(&role_renamed);
    role_renamed.wait_until_grid("the role-drifted tab carries the marker", |grid| {
        grid.lines()
            .next()
            .is_some_and(|tabs| tabs.contains(&format!("{ORCHESTRATION} {MARKER}")))
    });
    detach_quit(&mut role_renamed);
    let stream = role_renamed.stream_text();
    assert!(
        stream.contains("was started with roles (lead, coder)")
            && stream.contains("no longer match .dot-agent-deck.toml (lead, qa)"),
        "the exit warning must name the running and the configured roles"
    );
    drop(role_renamed);

    // No file at all: the branch a remote reconnect takes (PRD #111), where the
    // synthesised tab is the right answer and a warning would be noise.
    std::fs::remove_file(project.path().join(".dot-agent-deck.toml")).expect("remove config");
    let mut absent = launch_tui_against(&daemon);
    wait_for_rebuilt_tab(&absent);
    let strip = tab_strip(&absent);
    assert!(
        !strip.contains(MARKER),
        "an absent config is the remote-reconnect case and must not mark the tab; strip = {strip:?}"
    );
    detach_quit(&mut absent);
    assert!(
        !absent.stream_text().contains("config drift"),
        "an absent config must print no drift warning on exit"
    );
}
