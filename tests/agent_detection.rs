//! Fast command-detection coverage for wrapper-strategy launch forms.

use dot_agent_deck::event::{AgentType, AgentType::Codex};
use spec::spec;

/// Scenario: Infer Codex from common shell command forms rather than only when
/// `codex` is the first whitespace token. Environment prefixes, sudo, quoted
/// executable paths, and shell launchers must all select the Codex adapter.
#[test]
fn codex_detection_matrix_handles_common_launchers() {
    for command in [
        "env FOO=1 codex",
        "sudo codex",
        "\"/opt/OpenAI Codex/codex\" --model mini",
        "sh -c 'codex'",
        "bash -lc \"codex --model mini\"",
    ] {
        assert_eq!(
            AgentType::from_command(Some(command)),
            Some(Codex),
            "common launch form must resolve to Codex so the wrapper strategy is applied: {command:?}"
        );
    }
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
