//! PRD #76 M2.3 — `remote list`, `remote remove`, `remote upgrade`.
//!
//! Drives the three new lifecycle subcommands through the same
//! `FakeSshExecutor` pattern as the M2.2 add tests. `list` and `remove` make
//! zero ssh calls (registry-only); `upgrade` reuses the install + verify
//! pipeline, so its tests look like trimmed-down `add` happy paths.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

use tempfile::TempDir;

use dot_agent_deck::remote::{
    RemoteAddError, RemoteEntry, RemoteRemoveError, RemoteUpgradeError, RemotesFile, SshError,
    SshExecutor, SshOutput, SshTarget, UpgradeOptions, list, remove, upgrade,
};

// ---------------------------------------------------------------------------
// FakeSshExecutor — pops responses in FIFO order, records every call.
// (Duplicated from `m2_2_remote_add.rs` — `tests/*.rs` are independent
// crates, and a tiny duplication beats a `mod common` ceremony for two files.)
// ---------------------------------------------------------------------------

struct FakeSshExecutor {
    responses: Mutex<VecDeque<Result<SshOutput, SshError>>>,
    calls: Mutex<Vec<(SshTarget, String)>>,
}

impl FakeSshExecutor {
    fn new(responses: Vec<Result<SshOutput, SshError>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn commands(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, c)| c.clone())
            .collect()
    }
}

impl SshExecutor for FakeSshExecutor {
    fn run(&self, target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
        self.calls
            .lock()
            .unwrap()
            .push((target.clone(), command.to_string()));
        match self.responses.lock().unwrap().pop_front() {
            Some(r) => r,
            None => panic!("FakeSshExecutor: no canned response left for `{command}`"),
        }
    }
}

fn ok(stdout: &str) -> Result<SshOutput, SshError> {
    Ok(SshOutput {
        status: 0,
        stdout: stdout.to_string(),
        stderr: String::new(),
    })
}

fn registry_path(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("remotes.toml")
}

fn load_registry(path: &Path) -> RemotesFile {
    RemotesFile::load(path).unwrap()
}

fn entry(name: &str, host: &str, port: u16, version: &str) -> RemoteEntry {
    RemoteEntry {
        name: name.to_string(),
        kind: "ssh".to_string(),
        host: host.to_string(),
        port,
        key: None,
        version: version.to_string(),
        added_at: "2026-05-09T01:00:00+00:00".to_string(),
        upgraded_at: None,
    }
}

fn upgrade_opts(name: &str, version: &str, no_install: bool) -> UpgradeOptions {
    UpgradeOptions {
        name: name.to_string(),
        version: version.to_string(),
        no_install,
        release_base: "https://example.test/releases/download".to_string(),
    }
}

// ---------------------------------------------------------------------------
// `remote list`
// ---------------------------------------------------------------------------

#[test]
fn remote_list_shows_configured_remotes() {
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);
    let pre = RemotesFile {
        remotes: vec![
            entry("hetzner-1", "viktor@hetzner-1.example.com", 22, "0.24.5"),
            entry("lab", "lab.local", 2222, "0.24.5"),
        ],
    };
    pre.save(&path).unwrap();

    let mut buf: Vec<u8> = Vec::new();
    list(&path, &mut buf).expect("list succeeds");
    let out = String::from_utf8(buf).unwrap();

    // Header is present.
    assert!(out.contains("NAME"), "missing header: {out}");
    assert!(out.contains("VERSION"), "missing header: {out}");

    // Both names appear.
    assert!(out.contains("hetzner-1"), "missing hetzner-1: {out}");
    assert!(out.contains("lab"), "missing lab: {out}");

    // Both versions appear (one row per entry).
    let version_count = out.matches("0.24.5").count();
    assert!(
        version_count >= 2,
        "expected version on both rows, got {version_count} occurrences in:\n{out}"
    );

    // Non-default port is rendered as `host:port`; default is bare.
    assert!(
        out.contains("lab.local:2222"),
        "non-default port should be in the host column: {out}"
    );
    assert!(
        out.contains("viktor@hetzner-1.example.com")
            && !out.contains("viktor@hetzner-1.example.com:22"),
        "default port (22) should be omitted from the host column: {out}"
    );
}

#[test]
fn remote_list_when_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir); // does not exist yet

    let mut buf: Vec<u8> = Vec::new();
    list(&path, &mut buf).expect("empty list succeeds");
    let out = String::from_utf8(buf).unwrap();

    assert!(
        out.contains("No remotes configured"),
        "empty-state message missing: {out}"
    );
    assert!(
        out.contains("dot-agent-deck remote add"),
        "empty-state should hint at `remote add`: {out}"
    );
}

// ---------------------------------------------------------------------------
// `remote remove`
// ---------------------------------------------------------------------------

#[test]
fn remote_remove_deletes_entry_atomically() {
    use std::os::unix::fs::MetadataExt;

    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);
    let pre = RemotesFile {
        remotes: vec![
            entry("hetzner-1", "viktor@hetzner-1.example.com", 22, "0.24.5"),
            entry("lab", "lab.local", 2222, "0.24.5"),
        ],
    };
    pre.save(&path).unwrap();

    let removed = remove("hetzner-1", &path).expect("remove succeeds");
    assert_eq!(removed.name, "hetzner-1");

    // File mode preserved at 0o600 across the rewrite.
    let mode = std::fs::metadata(&path).unwrap().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "after remove, mode is 0o{mode:o}, expected 0o600"
    );

    // The other entry survives untouched.
    let reg = load_registry(&path);
    assert_eq!(reg.remotes.len(), 1);
    assert_eq!(reg.remotes[0].name, "lab");
    assert_eq!(reg.remotes[0].port, 2222);
    assert_eq!(reg.remotes[0].version, "0.24.5");
}

#[test]
fn remote_remove_unknown_name_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir); // empty/missing registry

    let err = remove("foo", &path).expect_err("unknown name must error");
    match err {
        RemoteRemoveError::UnknownName { name } => assert_eq!(name, "foo"),
        other => panic!("unexpected error: {other:?}"),
    }
    let msg = format!("{}", remove("foo", &path).unwrap_err());
    assert!(
        msg.contains("foo"),
        "error message should name the remote: {msg}"
    );
    assert!(
        msg.contains("dot-agent-deck remote list"),
        "error should hint at `remote list`: {msg}"
    );
}

#[test]
fn remote_remove_last_entry_leaves_valid_toml() {
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);
    let pre = RemotesFile {
        remotes: vec![entry("only", "only.example.com", 22, "0.24.5")],
    };
    pre.save(&path).unwrap();

    remove("only", &path).expect("remove succeeds");

    // File still exists and parses; `remotes` is an empty array.
    assert!(
        path.exists(),
        "registry file should remain after last remove"
    );
    let contents = std::fs::read_to_string(&path).unwrap();
    let parsed: toml::Value = toml::from_str(&contents).expect("file must remain valid TOML");
    let remotes = parsed
        .get("remotes")
        .expect("`remotes` key present")
        .as_array()
        .expect("`remotes` is an array");
    assert!(
        remotes.is_empty(),
        "remotes array should be empty: {contents}"
    );

    // And the typed loader sees an empty registry.
    assert!(load_registry(&path).remotes.is_empty());
}

// ---------------------------------------------------------------------------
// `remote upgrade`
// ---------------------------------------------------------------------------

#[test]
fn remote_upgrade_runs_install_flow_with_new_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);
    let pre = RemotesFile {
        remotes: vec![entry(
            "hetzner-1",
            "viktor@hetzner-1.example.com",
            22,
            "0.24.4",
        )],
    };
    pre.save(&path).unwrap();
    let original_added_at = pre.remotes[0].added_at.clone();

    // 3 ssh calls in install mode: uname, install, version-check.
    let executor = FakeSshExecutor::new(vec![
        ok("Linux x86_64\n"),
        ok(""), // install: success
        ok("dot-agent-deck 0.24.5\n"),
    ]);

    let updated = upgrade(
        &upgrade_opts("hetzner-1", "0.24.5", false),
        &executor,
        &path,
    )
    .expect("upgrade succeeds");

    assert_eq!(updated.version, "0.24.5");
    assert!(
        updated.upgraded_at.is_some(),
        "upgraded_at must be populated after a successful upgrade"
    );
    assert_eq!(
        updated.added_at, original_added_at,
        "added_at must remain at the original registration timestamp"
    );

    let cmds = executor.commands();
    assert_eq!(cmds.len(), 3);
    assert_eq!(cmds[0], "uname -s -m");
    assert!(
        cmds[1].contains("/v0.24.5/dot-agent-deck-linux-amd64"),
        "install URL should target v0.24.5: {}",
        cmds[1]
    );
    assert_eq!(cmds[2], "~/.local/bin/dot-agent-deck --version");

    // Registry persisted: version updated, name unchanged, single row.
    let reg = load_registry(&path);
    assert_eq!(reg.remotes.len(), 1);
    assert_eq!(reg.remotes[0].name, "hetzner-1");
    assert_eq!(reg.remotes[0].version, "0.24.5");
    assert!(reg.remotes[0].upgraded_at.is_some());
    assert_eq!(reg.remotes[0].added_at, original_added_at);
}

#[test]
fn remote_upgrade_unknown_name_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);
    let pre = RemotesFile {
        remotes: vec![entry("hetzner-1", "h.example.com", 22, "0.24.5")],
    };
    pre.save(&path).unwrap();

    // Empty queue — any ssh call panics the fake.
    let executor = FakeSshExecutor::new(vec![]);

    let err = upgrade(
        &upgrade_opts("does-not-exist", "0.24.5", false),
        &executor,
        &path,
    )
    .expect_err("unknown name must error");
    match err {
        RemoteUpgradeError::UnknownName { name } => assert_eq!(name, "does-not-exist"),
        other => panic!("unexpected error: {other:?}"),
    }

    // Zero ssh calls — short-circuit before the network.
    assert!(
        executor.commands().is_empty(),
        "ssh was called for unknown name: {:?}",
        executor.commands()
    );

    // Existing entry is untouched.
    let reg = load_registry(&path);
    assert_eq!(reg.remotes.len(), 1);
    assert_eq!(reg.remotes[0].version, "0.24.5");
    assert!(reg.remotes[0].upgraded_at.is_none());
}

#[test]
fn remote_upgrade_invalid_version_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);
    let pre = RemotesFile {
        remotes: vec![entry("hetzner-1", "h.example.com", 22, "0.24.4")],
    };
    pre.save(&path).unwrap();

    let payloads = [
        "0.24.5; rm -rf ~",
        "0.24.5$(whoami)",
        "0.24.5`id`",
        "0.24.5 || true",
        "../../etc/passwd",
    ];

    for payload in payloads {
        // Empty queue — any ssh call panics the fake.
        let executor = FakeSshExecutor::new(vec![]);
        let err = upgrade(&upgrade_opts("hetzner-1", payload, false), &executor, &path)
            .expect_err("malicious version must be rejected");
        match err {
            RemoteUpgradeError::Inner(RemoteAddError::InvalidVersion { input }) => {
                assert_eq!(input, payload);
            }
            other => panic!("unexpected error for `{payload}`: {other:?}"),
        }
        assert!(
            executor.commands().is_empty(),
            "ssh was called for malicious payload `{payload}`: {:?}",
            executor.commands()
        );
    }

    // Registry unchanged (still v0.24.4, no upgraded_at).
    let reg = load_registry(&path);
    assert_eq!(reg.remotes[0].version, "0.24.4");
    assert!(reg.remotes[0].upgraded_at.is_none());
}

#[test]
fn remote_upgrade_no_install_requires_version_match() {
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);
    let pre = RemotesFile {
        remotes: vec![entry(
            "hetzner-1",
            "viktor@hetzner-1.example.com",
            22,
            "0.24.4",
        )],
    };
    pre.save(&path).unwrap();

    // no-install: uname, version-check (mismatched).
    let executor = FakeSshExecutor::new(vec![
        ok("Linux x86_64\n"),
        ok("dot-agent-deck 0.24.4\n"), // remote still on 0.24.4 — mismatch
    ]);

    let err = upgrade(&upgrade_opts("hetzner-1", "0.24.5", true), &executor, &path)
        .expect_err("must fail on version mismatch");
    match err {
        RemoteUpgradeError::Inner(RemoteAddError::VersionMismatch { actual, expected }) => {
            assert_eq!(actual, "0.24.4");
            assert_eq!(expected, "0.24.5");
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // Registry not updated.
    let reg = load_registry(&path);
    assert_eq!(reg.remotes[0].version, "0.24.4");
    assert!(reg.remotes[0].upgraded_at.is_none());
}

#[test]
fn remote_upgrade_normalizes_v_prefix() {
    // Sibling of the M2.2 J1 regression: `--version v0.24.5` must produce
    // exactly one `v` in the URL and store the unprefixed form in the
    // registry (otherwise the post-install version comparison fails).
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);
    let pre = RemotesFile {
        remotes: vec![entry(
            "hetzner-1",
            "viktor@hetzner-1.example.com",
            22,
            "0.24.4",
        )],
    };
    pre.save(&path).unwrap();

    let executor = FakeSshExecutor::new(vec![
        ok("Linux x86_64\n"),
        ok(""),
        ok("dot-agent-deck 0.24.5\n"),
    ]);

    let updated = upgrade(
        &upgrade_opts("hetzner-1", "v0.24.5", false),
        &executor,
        &path,
    )
    .expect("v-prefixed version must succeed");

    let cmds = executor.commands();
    assert!(
        cmds[1].contains("/v0.24.5/dot-agent-deck-linux-amd64"),
        "URL must have exactly one `v` before the version: {}",
        cmds[1]
    );
    assert!(
        !cmds[1].contains("vv0.24.5"),
        "URL must not contain `vv`: {}",
        cmds[1]
    );

    // Registry stores the canonical unprefixed form.
    assert_eq!(updated.version, "0.24.5");
    assert_eq!(load_registry(&path).remotes[0].version, "0.24.5");
}
