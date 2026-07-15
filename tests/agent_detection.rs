//! Fast command-detection coverage for wrapper-strategy launch forms.

use dot_agent_deck::event::{AgentType, AgentType::Codex};
use spec::spec;

/// Scenario: Infer Codex from common shell command forms rather than only when
/// `codex` is the first whitespace token. Environment and sudo options that
/// consume arguments, quoted paths, and nested command-mode shells must resolve
/// correctly, while an unrelated shell option must not consume its argument as a script.
#[test]
fn codex_detection_matrix_handles_common_launchers() {
    for command in [
        "env FOO=1 codex",
        "sudo codex",
        "\"/opt/OpenAI Codex/codex\" --model mini",
        "sh -c 'codex'",
        "bash -lc \"codex --model mini\"",
        "sh -c \"sh -c 'codex'\"",
        "sudo -u root codex",
        "env -u FOO codex",
    ] {
        assert_eq!(
            AgentType::from_command(Some(command)),
            Some(Codex),
            "common launch form must resolve to Codex so the wrapper strategy is applied: {command:?}"
        );
    }

    assert_eq!(
        AgentType::from_command(Some("bash --rcfile codex")),
        None,
        "--rcfile consumes a startup file; it is not a shell command-mode flag"
    );
}

#[cfg(unix)]
fn write_executable(path: &std::path::Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, contents).expect("write respawn recorder");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod respawn recorder");
}

/// Scenario: Spawn an initial pane process, then respawn that same pane with a
/// bare `codex` command while PATH recorder stubs capture the actual argv. The
/// replacement child must launch through the Wrapper strategy exactly once.
#[spec("codex/spawn/005")]
#[test]
#[cfg(unix)]
fn spawn_005_respawn_wraps_codex() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build respawn runtime");
    runtime.block_on(async {
        use dot_agent_deck::agent_pty::{
            AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions,
        };

        let fixture = tempfile::tempdir().expect("respawn recorder fixture");
        let bin_dir = fixture.path().join("bin");
        let record = fixture.path().join("respawn.log");
        std::fs::create_dir(&bin_dir).expect("create respawn bin dir");
        write_executable(
            &bin_dir.join("dot-agent-deck"),
            "#!/bin/sh\nprintf 'WRAPPED %s\\n' \"$*\" >> \"$CODEX_RESPAWN_RECORD\"\nexec cat\n",
        );
        write_executable(
            &bin_dir.join("codex"),
            "#!/bin/sh\nprintf 'BARE codex %s\\n' \"$*\" >> \"$CODEX_RESPAWN_RECORD\"\nexec cat\n",
        );
        let path = format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").expect("test runner PATH")
        );
        let registry = AgentPtyRegistry::new();
        registry
            .spawn_agent(SpawnOptions {
                command: Some("cat"),
                env: vec![
                    (DOT_AGENT_DECK_PANE_ID.into(), "respawn-pane".into()),
                    ("PATH".into(), path),
                    (
                        "CODEX_RESPAWN_RECORD".into(),
                        record.to_string_lossy().into_owned(),
                    ),
                ],
                ..SpawnOptions::default()
            })
            .expect("spawn initial pane process");
        registry
            .respawn_agent_for_pane("respawn-pane", "codex")
            .await
            .expect("respawn pane as Codex");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !record.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let launched = std::fs::read_to_string(&record).unwrap_or_default();
        assert_eq!(
            launched.trim(),
            "WRAPPED wrap --agent codex -- codex",
            "respawning a pane with bare Codex must apply the Wrapper strategy; observed {launched:?}"
        );
        registry.shutdown_all();
    });
}

/// Scenario: Spawn a pane with an explicit Codex identity but a non-Codex
/// launcher basename, while PATH recorder shims capture the executed argv. The
/// command must still pass through the Codex wrapper and the live registry must
/// retain the same explicit Codex identity.
#[spec("codex/spawn/006")]
#[test]
#[cfg(unix)]
fn spawn_006_explicit_codex_identity_wraps_noninferable_launcher() {
    use dot_agent_deck::agent_pty::{AgentPtyRegistry, SpawnOptions};

    let fixture = tempfile::tempdir().expect("explicit Codex identity fixture");
    let bin_dir = fixture.path().join("bin");
    let record = fixture.path().join("spawn.log");
    std::fs::create_dir(&bin_dir).expect("create explicit identity bin dir");
    write_executable(
        &bin_dir.join("dot-agent-deck"),
        "#!/bin/sh\nprintf 'WRAPPED %s\\n' \"$*\" >> \"$CODEX_SPAWN_RECORD\"\nexec cat\n",
    );
    write_executable(
        &bin_dir.join("devbox"),
        "#!/bin/sh\nprintf 'BARE %s\\n' \"$*\" >> \"$CODEX_SPAWN_RECORD\"\nexec cat\n",
    );
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").expect("test runner PATH")
    );
    let registry = AgentPtyRegistry::new();
    registry
        .spawn_agent(SpawnOptions {
            command: Some("devbox run codex-big"),
            env: vec![
                ("PATH".into(), path),
                (
                    "CODEX_SPAWN_RECORD".into(),
                    record.to_string_lossy().into_owned(),
                ),
            ],
            agent_type: Some(AgentType::Codex),
            ..SpawnOptions::default()
        })
        .expect("spawn explicitly identified Codex launcher");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !record.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let launched = std::fs::read_to_string(&record).unwrap_or_default();
    let recorded_type = registry
        .agent_records()
        .first()
        .and_then(|entry| entry.agent_type.clone());
    registry.shutdown_all();

    assert_eq!(
        (launched.trim(), recorded_type),
        (
            "WRAPPED wrap --agent codex -- devbox run codex-big",
            Some(AgentType::Codex),
        ),
        "explicit Codex identity must drive both launch wrapping and pane metadata"
    );
}
