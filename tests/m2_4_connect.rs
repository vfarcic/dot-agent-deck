//! PRD #76 M2.4 — `dot-agent-deck connect [name]`.
//!
//! These integration tests exercise the picker + lookup paths against a temp
//! registry. They deliberately stop short of spawning real ssh — the
//! M2.2/M2.3 tests already established the convention of not making real
//! transport calls in tests, and the byte-relay portion of the bridge is
//! covered by the in-process unit test in `src/connect.rs`.

use std::io::Cursor;
use std::path::PathBuf;

use dot_agent_deck::connect::{RemoteConnectError, lookup_remote, pick_remote};
use dot_agent_deck::remote::{RemoteEntry, RemotesFile};

fn entry(name: &str, host: &str) -> RemoteEntry {
    RemoteEntry {
        name: name.to_string(),
        kind: "ssh".to_string(),
        host: host.to_string(),
        port: 22,
        key: None,
        version: "0.24.5".to_string(),
        added_at: "2026-05-09T01:00:00+00:00".to_string(),
        upgraded_at: None,
        last_connected: None,
    }
}

fn write_registry(entries: Vec<RemoteEntry>) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remotes.toml");
    let file = RemotesFile { remotes: entries };
    file.save(&path).unwrap();
    (dir, path)
}

#[test]
fn connect_unknown_name_exit_nonzero() {
    // The CLI handler maps `lookup_remote` errors to nonzero exit + stderr.
    // Asserting on the typed error here is sufficient: the CLI's `eprintln!`
    // + `ExitCode::FAILURE` shape mirrors `remote remove` and is unit-tested
    // through `Display` + `match`.
    let (_dir, path) = write_registry(vec![
        entry("hetzner-1", "viktor@hetzner-1.example.com"),
        entry("lab", "lab.local"),
    ]);

    let err = lookup_remote("missing", &path).expect_err("unknown name must error");
    match &err {
        RemoteConnectError::UnknownName { name } => assert_eq!(name, "missing"),
        other => panic!("unexpected error: {other:?}"),
    }
    let rendered = err.to_string();
    assert!(
        rendered.contains("missing"),
        "stderr-bound message must name the missing remote: {rendered}"
    );
    assert!(
        rendered.contains("dot-agent-deck remote list"),
        "stderr-bound message should hint at `remote list`: {rendered}"
    );
}

#[test]
fn connect_picker_lists_remotes_in_stdout() {
    // Picker prints both entry names; feeding "1\n" on stdin selects the
    // first one. We can't easily assert the bridge actually came up without
    // spawning real ssh; the bridge half is covered by the unit test in
    // `src/connect.rs::tests::bridge_relays_bytes_both_directions`.
    let (_dir, path) = write_registry(vec![
        entry("hetzner-1", "viktor@hetzner-1.example.com"),
        entry("lab", "lab.local"),
    ]);

    let mut input = Cursor::new(b"1\n".to_vec());
    let mut output = Vec::<u8>::new();
    let chosen = pick_remote(&path, &mut input, &mut output).expect("picker selects #1");
    assert_eq!(chosen.name, "hetzner-1");

    let stdout = String::from_utf8(output).unwrap();
    assert!(
        stdout.contains("hetzner-1"),
        "picker stdout should list the first entry: {stdout}"
    );
    assert!(
        stdout.contains("lab"),
        "picker stdout should list the second entry: {stdout}"
    );
    assert!(
        stdout.contains("Select a remote"),
        "picker stdout should print the header: {stdout}"
    );
}
