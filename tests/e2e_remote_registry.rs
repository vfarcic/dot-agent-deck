#![cfg(feature = "e2e")]

//! L2 lane-1 coverage for the shared CLI and desktop remote registry format.
//! Both commands run the real binary against a test-owned `remotes.toml`.

mod common;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use spec::spec;

const DESKTOP_ROW: &str = "[[remotes]]\n\
    name = \"deck-example-test\"\n\
    type = \"ssh\"\n\
    host = \"deck.example.test\"\n\
    port = 2222\n\
    key = \"/nonexistent/test-identity\"\n\
    version = \"0.1.0\"\n\
    added_at = \"2026-09-26T12:00:00Z\"\n\
    id = \"desktop-deck-7\"\n\
    user = \"alice\"\n\
    jump_host = \"bastion.example.test\"\n\
    socket = \"/run/user/1000/dot-agent-deck-attach.sock\"\n\
    some_future_field = \"x\"\n";

fn run_remote(args: &[&str], remotes_file: &Path, home: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    command.args(args).env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("HOME", home)
        .env("TERM", "xterm-256color")
        .env("DOT_AGENT_DECK_REMOTES", remotes_file)
        .env("DOT_AGENT_DECK_SOCKET", home.join("hook.sock"))
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", home.join("attach.sock"))
        .env("DOT_AGENT_DECK_STATE_DIR", home.join("state"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn dot-agent-deck remote command")
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Scenario: Write a desktop-shaped remote row, including its optional identity, SSH, and socket fields, to a sandboxed registry. The real CLI lists that deck without rejecting the file.
#[spec("remote/registry/001")]
#[test]
fn remote_registry_001_desktop_row_appears_in_cli_list() {
    let tempdir = common::race_safe_tempdir();
    let remotes_file = tempdir.path().join("remotes.toml");
    std::fs::write(&remotes_file, DESKTOP_ROW).expect("stage desktop-shaped remotes.toml");

    let output = run_remote(&["remote", "list"], &remotes_file, tempdir.path());
    assert!(
        output.status.success(),
        "remote list must accept the desktop-shaped row: {}",
        output_text(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("deck-example-test") && stdout.contains("deck.example.test:2222"),
        "remote list must show the staged deck: {}",
        output_text(&output)
    );
}

/// Scenario: Write a desktop-shaped deck and a second deck to a sandboxed registry, then remove the second deck through the real CLI. The first deck keeps its id, user, jump host, socket, and unknown future field after the CLI saves the file.
#[spec("remote/registry/002")]
#[test]
fn remote_registry_002_cli_save_preserves_desktop_and_future_fields() {
    let tempdir = common::race_safe_tempdir();
    let remotes_file = tempdir.path().join("remotes.toml");
    let second_row = "[[remotes]]\nname = \"remove-me\"\ntype = \"ssh\"\nhost = \"unused.example.test\"\nport = 22\nversion = \"0.1.0\"\nadded_at = \"2026-09-26T12:01:00Z\"\n";
    std::fs::write(&remotes_file, format!("{DESKTOP_ROW}\n{second_row}"))
        .expect("stage two remotes");

    // `remote remove` is registry-only: it re-saves without opening an SSH
    // connection, and removing the second row leaves the desktop row in place.
    let output = run_remote(
        &["remote", "remove", "remove-me"],
        &remotes_file,
        tempdir.path(),
    );
    assert!(
        output.status.success(),
        "remote remove must re-save the registry without SSH: {}",
        output_text(&output)
    );

    let saved = std::fs::read_to_string(&remotes_file).expect("read saved remotes.toml");
    let parsed: toml::Value = toml::from_str(&saved).expect("saved registry remains valid TOML");
    let rows = parsed["remotes"].as_array().expect("remotes array");
    assert_eq!(rows.len(), 1, "only the first deck should remain: {saved}");
    let first = &rows[0];
    assert_eq!(first["name"].as_str(), Some("deck-example-test"));
    for (field, expected) in [
        ("id", "desktop-deck-7"),
        ("user", "alice"),
        ("jump_host", "bastion.example.test"),
        ("socket", "/run/user/1000/dot-agent-deck-attach.sock"),
        ("some_future_field", "x"),
    ] {
        assert_eq!(
            first.get(field).and_then(toml::Value::as_str),
            Some(expected),
            "remote remove dropped {field} from the untouched deck: {saved}"
        );
    }
}
