//! PRD #76 M2.2 — `dot-agent-deck remote add --type=ssh ...`.
//!
//! Drives `remote::add` end-to-end through a `FakeSshExecutor` that records
//! the exact ssh commands the flow issued and returns canned outputs. The
//! production ssh impl is exercised via `Command::get_args` assertions in
//! `remote::tests` (unit) — these tests focus on the orchestration: which
//! ssh calls happen, in what order, with which short-circuits.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

use tempfile::TempDir;

use dot_agent_deck::remote::{
    AddOptions, RemoteAddError, RemoteEntry, RemotesFile, SshError, SshExecutor, SshOutput,
    SshTarget, add,
};

// ---------------------------------------------------------------------------
// FakeSshExecutor — pops responses in FIFO order, records every call.
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

    fn calls(&self) -> Vec<(SshTarget, String)> {
        self.calls.lock().unwrap().clone()
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

fn opts(name: &str, target: &str, no_install: bool) -> AddOptions {
    AddOptions {
        name: name.to_string(),
        remote_type: "ssh".to_string(),
        target: target.to_string(),
        port: 22,
        key: None,
        version: "0.24.5".to_string(),
        no_install,
        release_base: "https://example.test/releases/download".to_string(),
    }
}

fn registry_path(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("remotes.toml")
}

fn load_registry(path: &Path) -> RemotesFile {
    RemotesFile::load(path).unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn remote_add_happy_path_writes_registry_and_invokes_hooks_install() {
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);

    // 4 ssh calls in install mode: uname, install, version-check, hooks
    let executor = FakeSshExecutor::new(vec![
        ok("Linux x86_64\n"),
        ok(""), // install: mkdir/curl/chmod success
        ok("dot-agent-deck 0.24.5\n"),
        ok("Installed hooks: SessionStart, ...\n"),
    ]);

    let entry = add(
        &opts("hetzner-1", "viktor@hetzner-1.example.com", false),
        &executor,
        &path,
    )
    .expect("add succeeds");

    let cmds = executor.commands();
    assert_eq!(cmds.len(), 4);
    assert_eq!(cmds[0], "uname -s -m");
    assert!(
        cmds[1].starts_with("mkdir -p ~/.local/bin && curl -fsSL "),
        "install command shape: {}",
        cmds[1]
    );
    assert!(
        cmds[1].contains("/v0.24.5/dot-agent-deck-linux-amd64 -o ~/.local/bin/dot-agent-deck"),
        "install URL/dest: {}",
        cmds[1]
    );
    assert!(
        cmds[1].ends_with("&& chmod 0755 ~/.local/bin/dot-agent-deck"),
        "install chmod tail: {}",
        cmds[1]
    );
    assert_eq!(cmds[2], "~/.local/bin/dot-agent-deck --version");
    assert_eq!(cmds[3], "~/.local/bin/dot-agent-deck hooks install");

    // Target was parsed correctly on every call.
    for (t, _) in executor.calls() {
        assert_eq!(t.user.as_deref(), Some("viktor"));
        assert_eq!(t.host, "hetzner-1.example.com");
        assert_eq!(t.port, 22);
    }

    // Registry has exactly one entry, with the expected fields.
    let reg = load_registry(&path);
    assert_eq!(reg.remotes.len(), 1);
    let row = &reg.remotes[0];
    assert_eq!(row.name, "hetzner-1");
    assert_eq!(row.kind, "ssh");
    assert_eq!(row.host, "viktor@hetzner-1.example.com");
    assert_eq!(row.port, 22);
    assert!(row.key.is_none());
    assert_eq!(row.version, "0.24.5");
    assert!(!row.added_at.is_empty(), "added_at populated");

    // Same fields surfaced in the returned entry.
    assert_eq!(entry, *row);
}

#[test]
fn remote_add_no_install_requires_version_match() {
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);

    // no-install path: uname, version-check (mismatched).
    let executor = FakeSshExecutor::new(vec![
        ok("Linux x86_64\n"),
        ok("dot-agent-deck 0.24.4\n"), // mismatch
    ]);

    let err = add(&opts("hetzner-1", "viktor@host", true), &executor, &path)
        .expect_err("must fail on version mismatch");

    match err {
        RemoteAddError::VersionMismatch { actual, expected } => {
            assert_eq!(actual, "0.24.4");
            assert_eq!(expected, "0.24.5");
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // Registry was not updated.
    let reg = load_registry(&path);
    assert!(reg.remotes.is_empty());
}

#[test]
fn remote_add_unknown_arch_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);

    let executor = FakeSshExecutor::new(vec![ok("Linux riscv64\n")]);

    let err = add(
        &opts("rv-host", "user@rv.example.com", false),
        &executor,
        &path,
    )
    .expect_err("must fail on unsupported arch");

    let msg = err.to_string();
    assert!(msg.contains("Linux riscv64"), "msg should name arch: {msg}");
    assert!(
        msg.contains("linux-{amd64,arm64}") && msg.contains("darwin-{amd64,arm64}"),
        "msg should list supported arches: {msg}"
    );

    assert!(load_registry(&path).remotes.is_empty());
    // No second ssh call should have been attempted.
    assert_eq!(executor.commands().len(), 1);
}

#[test]
fn remote_add_duplicate_name_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);

    // Pre-populate registry with hetzner-1.
    let pre = RemotesFile {
        remotes: vec![RemoteEntry {
            name: "hetzner-1".to_string(),
            kind: "ssh".to_string(),
            host: "viktor@hetzner-1.example.com".to_string(),
            port: 22,
            key: None,
            version: "0.24.5".to_string(),
            added_at: "2026-05-09T01:00:00+00:00".to_string(),
            upgraded_at: None,
        }],
    };
    pre.save(&path).unwrap();

    // Empty queue — if any ssh call is made the fake will panic.
    let executor = FakeSshExecutor::new(vec![]);

    let err = add(
        &opts("hetzner-1", "viktor@hetzner-1.example.com", false),
        &executor,
        &path,
    )
    .expect_err("duplicate must be rejected");

    match err {
        RemoteAddError::DuplicateName { name } => assert_eq!(name, "hetzner-1"),
        other => panic!("unexpected error: {other:?}"),
    }

    // Crucial: zero ssh calls — short-circuit before the network.
    assert!(executor.commands().is_empty());

    // Registry unchanged (still one row).
    assert_eq!(load_registry(&path).remotes.len(), 1);
}

#[test]
fn remote_add_kubernetes_type_not_yet_implemented() {
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);

    let executor = FakeSshExecutor::new(vec![]);

    let mut o = opts("k8s-1", "ignored", false);
    o.remote_type = "kubernetes".to_string();
    let err = add(&o, &executor, &path).expect_err("kubernetes must not be implemented");

    let msg = err.to_string();
    assert!(
        msg.contains("PRD #80"),
        "msg should mention PRD #80 timeline: {msg}"
    );
    assert!(executor.commands().is_empty());
    assert!(load_registry(&path).remotes.is_empty());
}

#[test]
fn remote_add_ssh_unreachable_aborts_before_install() {
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);

    let executor = FakeSshExecutor::new(vec![Err(SshError::ConnectionRefused {
        host: "hetzner-1.example.com".to_string(),
        port: 22,
        detail: "ssh: connect to host hetzner-1.example.com port 22: Connection refused"
            .to_string(),
    })]);

    let err = add(
        &opts("hetzner-1", "viktor@hetzner-1.example.com", false),
        &executor,
        &path,
    )
    .expect_err("connection-refused must propagate");

    match err {
        RemoteAddError::Ssh(SshError::ConnectionRefused { host, port, .. }) => {
            assert_eq!(host, "hetzner-1.example.com");
            assert_eq!(port, 22);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // Only the first (uname) call was attempted; nothing else.
    let cmds = executor.commands();
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0], "uname -s -m");

    // Registry untouched.
    assert!(load_registry(&path).remotes.is_empty());
}

#[test]
fn version_string_with_shell_metacharacters_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);

    let payloads = [
        "0.24.5; rm -rf ~",
        "0.24.5$(whoami)",
        "0.24.5`id`",
        "0.24.5 || true",
        "../../etc/passwd",
    ];

    for payload in payloads {
        // Empty queue — if any ssh call is made the fake will panic.
        let executor = FakeSshExecutor::new(vec![]);
        let mut o = opts("hetzner-1", "viktor@hetzner-1.example.com", false);
        o.version = payload.to_string();
        let err = add(&o, &executor, &path).expect_err("malicious version must be rejected");
        match err {
            RemoteAddError::InvalidVersion { input } => assert_eq!(input, payload),
            other => panic!("unexpected error for `{payload}`: {other:?}"),
        }
        // Crucial: zero ssh calls — short-circuit before the network.
        assert!(
            executor.commands().is_empty(),
            "ssh was called for malicious payload `{payload}`: {:?}",
            executor.commands()
        );
    }

    // Registry untouched.
    assert!(load_registry(&path).remotes.is_empty());
}

#[test]
fn remote_add_normalizes_v_prefixed_version_end_to_end() {
    // Regression: `--version v0.24.5` previously built `/vv0.24.5/` URLs that
    // 404'd on GitHub, and the post-install version comparison ("0.24.5" vs
    // "v0.24.5") failed. Both must succeed once the input is normalized.
    let dir = tempfile::tempdir().unwrap();
    let path = registry_path(&dir);

    let executor = FakeSshExecutor::new(vec![
        ok("Linux x86_64\n"),
        ok(""),
        ok("dot-agent-deck 0.24.5\n"),
        ok("Installed hooks\n"),
    ]);

    let mut o = opts("hetzner-1", "viktor@hetzner-1.example.com", false);
    o.version = "v0.24.5".to_string();
    let entry = add(&o, &executor, &path).expect("v-prefixed version must succeed");

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
    assert_eq!(entry.version, "0.24.5");
    assert_eq!(load_registry(&path).remotes[0].version, "0.24.5");
}

#[test]
fn remotes_toml_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remotes.toml");
    let file = RemotesFile {
        remotes: vec![
            RemoteEntry {
                name: "a".to_string(),
                kind: "ssh".to_string(),
                host: "viktor@a.example.com".to_string(),
                port: 22,
                key: Some("~/.ssh/id_ed25519".to_string()),
                version: "0.24.5".to_string(),
                added_at: "2026-05-09T01:23:45+00:00".to_string(),
                upgraded_at: None,
            },
            RemoteEntry {
                name: "b".to_string(),
                kind: "ssh".to_string(),
                host: "b.local".to_string(),
                port: 2222,
                key: None,
                version: "0.24.5".to_string(),
                added_at: "2026-05-09T02:00:00+00:00".to_string(),
                upgraded_at: None,
            },
        ],
    };
    file.save(&path).unwrap();
    let loaded = RemotesFile::load(&path).unwrap();
    assert_eq!(loaded, file);
}
