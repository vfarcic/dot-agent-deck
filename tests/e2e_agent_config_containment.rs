#![cfg(all(feature = "e2e", unix))]

//! Real wrapper startup against two fake homes; never the developer's home.
#[path = "../src/test_temp.rs"]
mod test_temp;

use spec::spec;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

type Fingerprint = (Vec<u8>, u64, i64, i64);

fn fingerprint(path: &Path) -> Fingerprint {
    let metadata = std::fs::metadata(path).unwrap();
    (
        std::fs::read(path).unwrap(),
        metadata.ino(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    )
}

fn seed_executable(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn fake_operator(home: &Path) -> Vec<(PathBuf, Fingerprint)> {
    [
        ".codex/hooks.json",
        ".codex/config.toml",
        ".claude/settings.json",
        ".config/devin/config.json",
        ".config/opencode/plugin/dot-agent-deck.js",
        ".opencode/plugin/dot-agent-deck.js",
        ".pi/agent/extensions/user.ts",
    ]
    .into_iter()
    .map(|name| {
        let path = home.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: &[u8] = if name.ends_with(".json") {
            b"{\"hooks\":{},\"operatorSentinel\":true}\n"
        } else {
            b"operator-owned sentinel\n"
        };
        std::fs::write(&path, body).unwrap();
        let before = fingerprint(&path);
        (path, before)
    })
    .collect()
}

fn wrap(root: &Path, home: &Path, codex_home: &Path, owned_root: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("CODEX_HOME", codex_home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("PI_CODING_AGENT_DIR", home.join(".pi/agent"))
        .env("DOT_AGENT_DECK_TEST_CONFIG_WRITE", "1")
        .env("DOT_AGENT_DECK_PANE_ID", "config-containment-worker")
        .env("DOT_AGENT_DECK_SOCKET", root.join("no-hook-listener.sock"))
        .env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            root.join("no-attach-listener.sock"),
        )
        .env("DOT_AGENT_DECK_STATE_DIR", home.join("state"))
        .env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "20")
        .current_dir(root)
        .args(["wrap", "--agent", "codex", "--", "/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(owned_root) = owned_root {
        command.env("DOT_AGENT_DECK_TEST_CONFIG_ROOT", owned_root);
    }
    command.output().unwrap()
}

/// Scenario: Start the real Codex wrapper twice with an owned sandbox home and a separate fake operator home. Hooks appear only in the sandbox, the second install does not rewrite them, and every operator config stays untouched.
#[spec("hooks/containment/001")]
#[test]
fn containment_001_wrapper_installs_only_in_owned_home_and_repeat_is_no_op() {
    let fixture = test_temp::tempdir().unwrap();
    let operator = fixture.path().join("fake-operator-home");
    let sandbox = fixture.path().join("sandbox-home");
    let originals = fake_operator(&operator);
    seed_executable(&sandbox.join(".local/bin/dot-agent-deck"));
    let codex = sandbox.join(".codex");
    let output = wrap(fixture.path(), &sandbox, &codex, Some(fixture.path()));
    assert!(
        output.status.success(),
        "owned wrapper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let hooks = codex.join("hooks.json");
    let document: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&hooks).expect("owned wrapper must actually install hooks"),
    )
    .unwrap();
    let command = document["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        command.contains(sandbox.to_str().unwrap()),
        "hook must pin the sandbox install: {command}"
    );
    let before = fingerprint(&hooks);
    assert!(
        wrap(fixture.path(), &sandbox, &codex, Some(fixture.path()))
            .status
            .success()
    );
    for (path, original) in originals {
        assert_eq!(
            fingerprint(&path),
            original,
            "fake operator config changed: {}",
            path.display()
        );
    }
    assert_eq!(
        fingerprint(&hooks),
        before,
        "automatic Codex reinstall must preserve bytes, inode and mtime"
    );
}

/// Scenario: Start the real Codex wrapper with the test marker and isolated homes but omit the owned root. It refuses automatic installation before creating a Codex directory, while fake operator configs remain untouched.
#[spec("hooks/containment/002")]
#[test]
fn containment_002_wrapper_without_owned_root_refuses_before_mkdir() {
    let fixture = test_temp::tempdir().unwrap();
    let originals = fake_operator(&fixture.path().join("fake-operator-home"));
    let sandbox = fixture.path().join("sandbox-home");
    seed_executable(&sandbox.join(".local/bin/dot-agent-deck"));
    let codex = sandbox.join("missing-codex-home");
    wrap(fixture.path(), &sandbox, &codex, None);
    for (path, original) in originals {
        assert_eq!(fingerprint(&path), original);
    }
    assert!(
        !codex.exists(),
        "test marker without owned root must refuse before mkdir or temp-file creation"
    );
    assert!(
        wrap(fixture.path(), &sandbox, &codex, Some(fixture.path()))
            .status
            .success()
    );
    assert!(
        codex.join("hooks.json").exists(),
        "supplying the owned root must permit the same install"
    );
}

/// Scenario: Give the real wrapper a sandbox HOME but override CODEX_HOME through a symlink to a separate fake operator home outside the allowed root. It refuses the escaped install without rewriting the operator's hooks or leaving temporary files.
#[spec("hooks/containment/003")]
#[test]
fn containment_003_wrapper_rejects_codex_home_symlink_escape() {
    let fixture = test_temp::tempdir().unwrap();
    let operator = fixture.path().join("fake-operator-home");
    let originals = fake_operator(&operator);
    let sandbox = fixture.path().join("sandbox-home");
    seed_executable(&sandbox.join(".local/bin/dot-agent-deck"));
    let escape = sandbox.join("codex-escape");
    std::os::unix::fs::symlink(operator.join(".codex"), &escape).unwrap();
    wrap(fixture.path(), &sandbox, &escape, Some(&sandbox));
    for (path, original) in originals {
        assert_eq!(
            fingerprint(&path),
            original,
            "symlink escaped into fake operator config: {}",
            path.display()
        );
    }
    assert_eq!(
        std::fs::read_dir(operator.join(".codex")).unwrap().count(),
        2,
        "refused writer must not leave a temporary file or backup"
    );
}

/// Scenario: Point the real wrapper's CODEX_HOME at a fake operator directory outside its owned root whose `hooks.json` is a symlink into the root. The install is refused: the symlink stays in place with its target unchanged, and no temporary file or backup appears beside it.
#[spec("hooks/containment/004")]
#[test]
fn containment_004_wrapper_refuses_an_outside_symlink_pointing_into_the_root() {
    let fixture = test_temp::tempdir().unwrap();
    let sandbox = fixture.path().join("sandbox-home");
    seed_executable(&sandbox.join(".local/bin/dot-agent-deck"));
    let target = sandbox.join("hooks-target.json");
    std::fs::write(&target, b"{\"hooks\":{},\"operatorSentinel\":true}\n").unwrap();
    let target_before = fingerprint(&target);
    let codex = fixture.path().join("fake-operator-home/.codex");
    std::fs::create_dir_all(&codex).unwrap();
    let link = codex.join("hooks.json");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let link_before = std::fs::symlink_metadata(&link).unwrap().ino();

    wrap(fixture.path(), &sandbox, &codex, Some(&sandbox));

    let link_after = std::fs::symlink_metadata(&link).unwrap();
    assert!(
        link_after.file_type().is_symlink() && link_after.ino() == link_before,
        "the publish replaced the outside-root symlink {}",
        link.display()
    );
    assert_eq!(std::fs::read_link(&link).unwrap(), target);
    assert_eq!(
        fingerprint(&target),
        target_before,
        "a refused install must not write through the symlink either"
    );
    let entries: Vec<_> = std::fs::read_dir(&codex)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        entries,
        [std::ffi::OsString::from("hooks.json")],
        "refused writer must not leave a temporary file, backup or lock beside the symlink"
    );
}
