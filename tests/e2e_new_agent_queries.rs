#![cfg(all(feature = "e2e", unix))]

//! L2 lane-1 coverage for the daemon queries that back the desktop's new-agent
//! directory browser and form (PRD #1223 M1/M2).
//!
//! The request variants intentionally travel as raw JSON so these tests pin the
//! public wire names and response projection independently of the typed client
//! helpers that use them.

mod common;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::DaemonProc;
use dot_agent_deck::agent_registry;
use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP, PROTOCOL_VERSION};
use dot_agent_deck::directory_listing::MAX_DIRECTORY_ENTRIES;
use serde_json::{Value, json};
use spec::spec;

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|e| panic!("canonicalize {}: {e}", path.display()))
}

fn wire_path(path: &Path) -> String {
    path.to_str()
        .unwrap_or_else(|| panic!("harness paths are UTF-8: {}", path.display()))
        .to_string()
}

/// Send a JSON request through the attach protocol without constructing an
/// `AttachRequest`. This stays local because these scenarios deliberately test
/// the public JSON shape rather than the typed client wrapper.
fn send_json_request(daemon: &DaemonProc, request: &Value) -> Value {
    let mut stream = std::os::unix::net::UnixStream::connect(&daemon.attach_socket)
        .expect("connect to the daemon attach socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set attach read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .expect("set attach write timeout");

    let payload = serde_json::to_vec(request).expect("serialize raw attach request");
    let mut header = [0_u8; 5];
    header[0] = KIND_REQ;
    header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    stream.write_all(&header).expect("write request header");
    stream.write_all(&payload).expect("write request payload");
    stream.flush().expect("flush request");

    let mut response_header = [0_u8; 5];
    stream
        .read_exact(&mut response_header)
        .expect("read response header");
    assert_eq!(
        response_header[0], KIND_RESP,
        "raw attach request must receive a RESP frame"
    );
    let len = u32::from_be_bytes([
        response_header[1],
        response_header[2],
        response_header[3],
        response_header[4],
    ]) as usize;
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body).expect("read response payload");
    serde_json::from_slice(&body).expect("response payload is JSON")
}

fn successful_payload<'a>(response: &'a Value, key: &str, operation: &str) -> &'a Value {
    assert_eq!(
        response.get("ok").and_then(Value::as_bool),
        Some(true),
        "{operation} must succeed; response: {response}"
    );
    response
        .get(key)
        .unwrap_or_else(|| panic!("{operation} success must carry {key:?}; response: {response}"))
}

fn directory_listing(response: &Value) -> &Value {
    successful_payload(response, "directories", "ListDirectories")
}

fn assert_directory_refusal(response: &Value, case: &str) {
    assert_eq!(
        response.get("ok").and_then(Value::as_bool),
        Some(false),
        "{case} must use the daemon's ordinary error reply; response: {response}"
    );
    assert!(
        response
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| !error.is_empty()),
        "{case} refusal must carry a non-empty error string; response: {response}"
    );
    assert!(
        response.get("directories").is_none_or(Value::is_null),
        "{case} refusal must not carry a successful directory listing; response: {response}"
    );
}

fn assert_options(response: &Value, expected_experimental: bool, expected_command: &str) {
    let options = successful_payload(response, "new_agent_options", "NewAgentOptions");
    assert_eq!(
        options.get("default_command").and_then(Value::as_str),
        Some(expected_command),
        "the daemon must read DashboardConfig.default_command from its own config"
    );

    let agents = options
        .get("agents")
        .and_then(Value::as_array)
        .expect("NewAgentOptions agents must be an array");
    assert!(
        !agents.is_empty(),
        "the compiled agent registry is non-empty"
    );
    let expected_agents: Vec<Value> = agent_registry::ALL
        .iter()
        .map(|spec| {
            let id = spec
                .detect_basenames
                .first()
                .copied()
                .unwrap_or_else(|| panic!("shipped agent {} has no registry id", spec.label));
            json!({
                "id": id,
                "display_name": spec.label,
                "default_command": spec.default_command,
            })
        })
        .collect();
    assert_eq!(
        agents, &expected_agents,
        "NewAgentOptions must project agent_registry::ALL in registry order"
    );
    let claude = agents
        .iter()
        .find(|agent| agent.get("id").and_then(Value::as_str) == Some("claude"))
        .expect("the options must include the registry's claude entry");
    assert_eq!(
        claude.get("default_command").and_then(Value::as_str),
        Some("claude"),
        "the claude option must carry its registry default command"
    );
    assert_eq!(
        options.get("experimental").and_then(Value::as_bool),
        Some(expected_experimental),
        "the options must report the daemon process's experimental state"
    );
    let authoring_kinds = options
        .get("authoring_kinds")
        .and_then(Value::as_array)
        .expect("NewAgentOptions authoring_kinds must be present as an array");
    assert!(
        authoring_kinds.iter().all(Value::is_string),
        "NewAgentOptions authoring_kinds entries must be strings; got {authoring_kinds:?}"
    );
}

/// Scenario: Point a headless daemon's HOME at an isolated tree, list HOME and
/// a fixture directory over the attach socket, then list that directory through
/// a symlinked spelling. Replies must be canonical, sorted, one-level and free
/// of hidden, file and symlink entries while marking the configured child.
#[spec("newagent/browse/001")]
#[test]
fn newagent_browse_001_lists_one_canonical_visible_level_from_daemon_home() {
    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[]);
    let root = daemon.home.join("browse-root");
    let ordinary = root.join("alpha");
    let project = root.join("bravo-project");
    std::fs::create_dir_all(ordinary.join("grandchild")).expect("create ordinary grandchild");
    std::fs::create_dir_all(&project).expect("create project child");
    std::fs::write(project.join(".dot-agent-deck.toml"), "").expect("seed the project marker");
    std::fs::create_dir_all(root.join(".hidden")).expect("create hidden child");
    std::fs::write(root.join("plain-file.txt"), "not a directory").expect("create plain file");
    std::os::unix::fs::symlink(&ordinary, root.join("linked-child"))
        .expect("create child-directory symlink");
    let alias = daemon.home.join("browse-alias");
    std::os::unix::fs::symlink(&root, &alias).expect("create typed-path symlink spelling");

    let home_response = send_json_request(&daemon, &json!({"op": "list-directories"}));
    let home_listing = directory_listing(&home_response);
    assert_eq!(
        home_listing.get("path").and_then(Value::as_str),
        Some(wire_path(&canonical(&daemon.home)).as_str()),
        "an absent path must list the daemon user's canonical HOME"
    );

    let response = send_json_request(
        &daemon,
        &json!({"op": "list-directories", "path": wire_path(&root)}),
    );
    let listing = directory_listing(&response);
    assert_eq!(
        listing.get("path").and_then(Value::as_str),
        Some(wire_path(&canonical(&root)).as_str())
    );
    assert_eq!(
        listing.get("parent").and_then(Value::as_str),
        Some(wire_path(&canonical(&daemon.home)).as_str())
    );
    assert_eq!(
        listing.get("truncated").and_then(Value::as_bool),
        Some(false)
    );
    let expected_entries = vec![
        json!({
            "name": "alpha",
            "path": wire_path(&canonical(&ordinary)),
            "is_project": false,
        }),
        json!({
            "name": "bravo-project",
            "path": wire_path(&canonical(&project)),
            "is_project": true,
        }),
    ];
    assert_eq!(
        listing.get("entries").and_then(Value::as_array),
        Some(&expected_entries),
        "only immediate visible real directories may appear, sorted by name"
    );

    let alias_response = send_json_request(
        &daemon,
        &json!({"op": "list-directories", "path": wire_path(&alias)}),
    );
    let alias_listing = directory_listing(&alias_response);
    assert_eq!(
        alias_listing.get("path").and_then(Value::as_str),
        Some(wire_path(&canonical(&root)).as_str()),
        "a user-typed symlink spelling must resolve to the target's canonical path"
    );
}

/// Scenario: List a fixture containing more directories than the production
/// cap, then request two relative paths, a missing absolute path and a regular
/// file. The listing must stop exactly at the cap with `truncated` set, while
/// every invalid target returns the daemon's ordinary structured error reply.
#[spec("newagent/browse/002")]
#[test]
fn newagent_browse_002_caps_results_and_refuses_invalid_targets() {
    let fixture = common::harness_tempdir().expect("mint directory-listing fixture");
    let crowded = fixture.path().join("crowded");
    std::fs::create_dir_all(&crowded).expect("create crowded root");
    for index in 0..MAX_DIRECTORY_ENTRIES + 5 {
        std::fs::create_dir(crowded.join(format!("entry-{index:04}")))
            .unwrap_or_else(|e| panic!("create cap fixture directory {index}: {e}"));
    }
    let regular_file = fixture.path().join("regular-file.txt");
    std::fs::write(&regular_file, "not a directory").expect("create refusal fixture file");
    let missing = fixture.path().join("does-not-exist");
    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[]);

    let response = send_json_request(
        &daemon,
        &json!({"op": "list-directories", "path": wire_path(&crowded)}),
    );
    let listing = directory_listing(&response);
    let entries = listing
        .get("entries")
        .and_then(Value::as_array)
        .expect("a directory listing must carry entries");
    assert_eq!(
        entries.len(),
        MAX_DIRECTORY_ENTRIES,
        "a listing over the cap must return exactly the bounded number of entries"
    );
    assert_eq!(
        listing.get("truncated").and_then(Value::as_bool),
        Some(true),
        "the caller must be told that more entries existed"
    );

    for (case, path) in [
        ("plain relative path", "relative/path".to_string()),
        ("dot-relative path", "./x".to_string()),
        ("nonexistent absolute path", wire_path(&missing)),
        ("absolute regular file", wire_path(&regular_file)),
    ] {
        let refusal = send_json_request(&daemon, &json!({"op": "list-directories", "path": path}));
        assert_directory_refusal(&refusal, case);
    }
}

/// Scenario: Launch one daemon with the experimental flag absent and one with
/// it enabled, both pointed at a host-side DashboardConfig carrying a distinct
/// default command. Each options reply must mirror that daemon and the compiled
/// registry in order, carry an authoring-kinds string array, and the handshake
/// must advertise both new query capabilities.
#[spec("newagent/options/001")]
#[test]
fn newagent_options_001_reports_host_config_registry_features_and_capabilities() {
    const CONFIGURED_COMMAND: &str = "new-agent-options-configured-command";

    let config = common::harness_tempdir().expect("mint daemon config fixture");
    let config_path = config.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("default_command = {CONFIGURED_COMMAND:?}\n"),
    )
    .expect("write daemon DashboardConfig");
    let config_path = wire_path(&config_path);

    let ordinary = common::spawn_daemon_serve_with_env(
        None,
        "0",
        &[("DOT_AGENT_DECK_CONFIG", config_path.as_str())],
    );
    let ordinary_response = send_json_request(&ordinary, &json!({"op": "new-agent-options"}));
    assert_options(&ordinary_response, false, CONFIGURED_COMMAND);

    let experimental = common::spawn_daemon_serve_with_env(
        None,
        "0",
        &[
            ("DOT_AGENT_DECK_CONFIG", config_path.as_str()),
            ("DOT_AGENT_DECK_EXPERIMENTAL", "1"),
        ],
    );
    let experimental_response =
        send_json_request(&experimental, &json!({"op": "new-agent-options"}));
    assert_options(&experimental_response, true, CONFIGURED_COMMAND);

    let hello = send_json_request(
        &ordinary,
        &json!({
            "op": "hello",
            "client_version": PROTOCOL_VERSION,
        }),
    );
    let capabilities = successful_payload(&hello, "capabilities", "Hello")
        .as_array()
        .expect("Hello capabilities must be an array");
    for capability in ["list-directories", "new-agent-options"] {
        assert!(
            capabilities.iter().any(|value| value == capability),
            "the live daemon handshake must advertise {capability:?}; capabilities: {capabilities:?}"
        );
    }
}
