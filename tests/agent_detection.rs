//! Fast command-detection coverage for wrapper-strategy launch forms.

use dot_agent_deck::event::{AgentType, AgentType::Codex};
// PRD #42 M8: the `#[spec]` tests below are Unix-gated (they drive the socket
// harness), so the macro import is too — otherwise it is unused on Windows.
#[cfg(unix)]
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
        // Point the wrapper rewrite at the recorder. The rewrite names the
        // co-located build by absolute path (so the suite tests what it just
        // compiled rather than whatever is installed on $PATH), which means a
        // fake `dot-agent-deck` on the child's PATH is no longer what runs —
        // this override is the seam for observing it. Set on THIS process
        // because the rewrite happens here, not in the child; nextest gives
        // each test its own process, so it cannot leak sideways.
        // SAFETY: single-threaded test process, set before the spawn below.
        unsafe {
            std::env::set_var(
                dot_agent_deck::wrap::DOT_AGENT_DECK_WRAP_BIN,
                bin_dir.join("dot-agent-deck"),
            );
        }
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
    // Point the wrapper rewrite at the recorder. The rewrite names the
    // co-located build by absolute path (so the suite tests what it just
    // compiled rather than whatever is installed on $PATH), which means a
    // fake `dot-agent-deck` on the child's PATH is no longer what runs —
    // this override is the seam for observing it. Set on THIS process
    // because the rewrite happens here, not in the child; nextest gives
    // each test its own process, so it cannot leak sideways.
    // SAFETY: single-threaded test process, set before the spawn below.
    unsafe {
        std::env::set_var(
            dot_agent_deck::wrap::DOT_AGENT_DECK_WRAP_BIN,
            bin_dir.join("dot-agent-deck"),
        );
    }
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

/// Scenario: Spawn a pane from the non-inferable `devbox run codex-big` command, then model a native Codex hook teaching the registry its display badge before respawning the pane. The captured launch lines before and after respawn must be byte-identical bare commands; learned display identity must never introduce a wrapper.
#[spec("codex/spawn/007")]
#[test]
#[cfg(unix)]
fn spawn_007_hook_learned_badge_does_not_change_respawn_launch() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build stable respawn runtime");
    runtime.block_on(async {
        use dot_agent_deck::agent_pty::{
            AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions,
        };

        let fixture = tempfile::tempdir().expect("stable respawn fixture");
        let bin_dir = fixture.path().join("bin");
        let record = fixture.path().join("launch.log");
        std::fs::create_dir_all(&bin_dir).expect("create stable respawn bin dir");
        write_executable(
            &bin_dir.join("devbox"),
            "#!/bin/sh\nprintf 'BARE devbox %s\\n' \"$*\" >> \"$STABLE_RESPAWN_RECORD\"\nexec cat\n",
        );
        write_executable(
            &bin_dir.join("dot-agent-deck"),
            "#!/bin/sh\nprintf 'WRAPPED %s\\n' \"$*\" >> \"$STABLE_RESPAWN_RECORD\"\nexec cat\n",
        );
        let path = format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").expect("test runner PATH")
        );
        // Point the wrapper rewrite at the recorder (same seam as spawn/005 and
        // spawn/006). Without it this assertion cannot fail for the regression
        // it guards: the rewrite now names the co-located build by absolute
        // path, so an unwanted wrapper would exec the REAL deck, which in turn
        // execs `devbox run codex-big` — and the devbox stub would record the
        // very same `BARE devbox run codex-big` line as the unwrapped launch.
        // With the override, a wrapper shows up as a `WRAPPED …` line instead.
        // Set on THIS process because the rewrite happens here, not in the
        // child; nextest gives each test its own process, so it cannot leak
        // sideways.
        // SAFETY: single-threaded test process, set before the spawn below.
        unsafe {
            std::env::set_var(
                dot_agent_deck::wrap::DOT_AGENT_DECK_WRAP_BIN,
                bin_dir.join("dot-agent-deck"),
            );
        }
        let registry = AgentPtyRegistry::new();
        registry
            .spawn_agent(SpawnOptions {
                command: Some("devbox run codex-big"),
                env: vec![
                    (DOT_AGENT_DECK_PANE_ID.into(), "stable-pane".into()),
                    ("PATH".into(), path),
                    (
                        "STABLE_RESPAWN_RECORD".into(),
                        record.to_string_lossy().into_owned(),
                    ),
                ],
                ..SpawnOptions::default()
            })
            .expect("spawn initially unwrapped launcher");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::fs::read_to_string(&record)
            .unwrap_or_default()
            .lines()
            .count()
            < 1
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        registry.set_agent_type("stable-pane", &AgentType::Codex);
        assert_eq!(
            registry.agent_records()[0].agent_type,
            Some(AgentType::Codex),
            "native-hook learning should upgrade the display badge"
        );

        registry
            .respawn_agent_for_pane("stable-pane", "devbox run codex-big")
            .await
            .expect("respawn non-inferable launcher");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::fs::read_to_string(&record)
            .unwrap_or_default()
            .lines()
            .count()
            < 2
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let launched = std::fs::read_to_string(&record).unwrap_or_default();
        registry.shutdown_all();

        assert_eq!(
            launched.lines().collect::<Vec<_>>(),
            vec![
                "BARE devbox run codex-big",
                "BARE devbox run codex-big"
            ],
            "the respawn launch line must remain byte-identical after a hook-learned Codex badge; observed {launched:?}"
        );
    });
}

/// Scenario: Spawn a pane with an explicit Codex identity on the non-inferable `devbox run codex-big` launcher, respawn it once with that same command, then respawn it again with the role command edited to a Claude one — with PATH recorder stubs capturing every exec line. The unchanged respawn must relaunch byte-identically through the Codex wrapper, while the edited command must launch as itself rather than being wrapped as Codex.
#[spec("codex/spawn/008")]
#[test]
#[cfg(unix)]
fn spawn_008_respawn_wrap_decision_follows_the_launched_command() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build launch-shape coherence runtime");
    runtime.block_on(async {
        use dot_agent_deck::agent_pty::{
            AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions,
        };

        let fixture = tempfile::tempdir().expect("launch-shape coherence fixture");
        let bin_dir = fixture.path().join("bin");
        let record = fixture.path().join("launch.log");
        std::fs::create_dir_all(&bin_dir).expect("create launch-shape bin dir");
        write_executable(
            &bin_dir.join("devbox"),
            "#!/bin/sh\nprintf 'BARE devbox %s\\n' \"$*\" >> \"$SHAPE_RECORD\"\nexec cat\n",
        );
        write_executable(
            &bin_dir.join("claude"),
            "#!/bin/sh\nprintf 'BARE claude %s\\n' \"$*\" >> \"$SHAPE_RECORD\"\nexec cat\n",
        );
        write_executable(
            &bin_dir.join("dot-agent-deck"),
            "#!/bin/sh\nprintf 'WRAPPED %s\\n' \"$*\" >> \"$SHAPE_RECORD\"\nexec cat\n",
        );
        let path = format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").expect("test runner PATH")
        );
        // Point the wrapper rewrite at the recorder (same seam as spawn/005 and
        // spawn/006). The rewrite names the co-located build by absolute path,
        // so a fake `dot-agent-deck` on the child's PATH is no longer what runs
        // — without this override the wrapped launches would exec the REAL deck
        // and the recorder would only ever see the inner `devbox` line. Set on
        // THIS process because the rewrite happens here, not in the child;
        // nextest gives each test its own process, so it cannot leak sideways.
        // SAFETY: single-threaded test process, set before the spawn below.
        unsafe {
            std::env::set_var(
                dot_agent_deck::wrap::DOT_AGENT_DECK_WRAP_BIN,
                bin_dir.join("dot-agent-deck"),
            );
        }
        // Wait until the recorder has appended `want` lines, so each launch is
        // observed before the next respawn overwrites the pane.
        let await_lines = |want: usize| {
            let record = record.clone();
            async move {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while std::fs::read_to_string(&record)
                    .unwrap_or_default()
                    .lines()
                    .count()
                    < want
                    && std::time::Instant::now() < deadline
                {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
        };

        let registry = AgentPtyRegistry::new();
        registry
            .spawn_agent(SpawnOptions {
                command: Some("devbox run codex-big"),
                env: vec![
                    (DOT_AGENT_DECK_PANE_ID.into(), "shape-pane".into()),
                    ("PATH".into(), path),
                    (
                        "SHAPE_RECORD".into(),
                        record.to_string_lossy().into_owned(),
                    ),
                ],
                agent_type: Some(AgentType::Codex),
                ..SpawnOptions::default()
            })
            .expect("spawn explicitly identified Codex launcher");
        await_lines(1).await;

        // The role command is untouched: the frozen identity is the only thing
        // that knows this launcher is Codex, so the wrapper must come back.
        registry
            .respawn_agent_for_pane("shape-pane", "devbox run codex-big")
            .await
            .expect("respawn with the unchanged role command");
        await_lines(2).await;

        // The user edited the role command in `.dot-agent-deck.toml` to a
        // different agent. The wrap decision must follow the command actually
        // being launched — never `wrap --agent codex -- claude …`.
        registry
            .respawn_agent_for_pane("shape-pane", "claude --model haiku")
            .await
            .expect("respawn with the edited role command");
        await_lines(3).await;

        let launched = std::fs::read_to_string(&record).unwrap_or_default();
        let badge = registry
            .agent_records()
            .first()
            .and_then(|entry| entry.agent_type.clone());
        registry.shutdown_all();

        assert_eq!(
            launched
                .lines()
                .map(str::trim_end)
                .collect::<Vec<_>>(),
            vec![
                "WRAPPED wrap --agent codex -- devbox run codex-big",
                "WRAPPED wrap --agent codex -- devbox run codex-big",
                "BARE claude --model haiku",
            ],
            "an unchanged role command must relaunch byte-identically while an edited one must launch as itself, never wrapped as the previous agent; observed {launched:?}"
        );
        assert_eq!(
            badge,
            Some(AgentType::ClaudeCode),
            "the badge must follow the newly launched command too, not keep advertising the replaced agent"
        );
    });
}
