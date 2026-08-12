//! Fast command-detection coverage for wrapper-strategy launch forms.

use dot_agent_deck::event::{AgentType, AgentType::Codex};
// PRD #42 M8: the `#[spec]` tests below are Unix-gated (they drive the socket
// harness), so the macro import is too — otherwise it is unused on Windows.
#[cfg(unix)]
use spec::spec;

mod common;

/// Budget for a PATH recorder shim to append its launch line. The shims are one
/// `printf` away from `exec cat`, so the happy path lands in milliseconds — this
/// whole file passes in well under a second — and this ceiling is only ever paid
/// when something is genuinely broken. Deliberately far above any plausible
/// scheduling delay on a loaded CI runner executing a debug binary, so a timeout
/// here means "nothing was ever recorded", never "the runner was slow".
#[cfg(unix)]
const RECORD_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Serializes the launch-shape tests below and points the wrapper rewrite at a
/// per-test recorder for as long as the guard lives.
///
/// [`wrap::DOT_AGENT_DECK_WRAP_BIN`](dot_agent_deck::wrap::DOT_AGENT_DECK_WRAP_BIN)
/// is read from the *spawning* process's environment, so every test here mutates
/// one process-global. `cargo nextest run` gives each test its own process and
/// they cannot collide — but CI's Linux `build` job runs plain `cargo test`,
/// where all tests in this binary are THREADS OF ONE PROCESS. Unserialized, a
/// test's spawn then names a *sibling* test's recorder, whose shim appends to an
/// env var that is unset in this child; the write goes nowhere and the recorder
/// file stays empty forever. That is precisely how `codex/spawn/005` and
/// `codex/spawn/006` failed on CI while passing locally under nextest (PRD
/// #225). Holding the lock across the whole set-env → spawn → observe window
/// makes the override honest under both runners, and is also what makes the
/// `set_var` calls sound: no sibling test is running while it is held.
#[cfg(unix)]
static WRAP_BIN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(unix)]
struct WrapBinOverride {
    _exclusive: std::sync::MutexGuard<'static, ()>,
}

#[cfg(unix)]
impl WrapBinOverride {
    fn pointing_at(recorder: &std::path::Path) -> Self {
        // A test that panics while holding the lock poisons it; recover the
        // guard rather than cascading one real failure into three confusing
        // ones.
        let exclusive = WRAP_BIN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the guard above excludes every other test in this binary, so
        // nothing else reads or writes the environment while this runs.
        unsafe {
            std::env::set_var(dot_agent_deck::wrap::DOT_AGENT_DECK_WRAP_BIN, recorder);
        }
        Self {
            _exclusive: exclusive,
        }
    }
}

#[cfg(unix)]
impl Drop for WrapBinOverride {
    fn drop(&mut self) {
        // Leave no override pointing at a deleted fixture. SAFETY: as above —
        // the exclusion guard is still held.
        unsafe {
            std::env::remove_var(dot_agent_deck::wrap::DOT_AGENT_DECK_WRAP_BIN);
        }
    }
}

/// Bounded wait for `record` to hold `want` complete launch lines, panicking
/// with what the file actually holds when it does not.
///
/// The point is that the wait FAILS rather than falls through: asserting on a
/// value that a timed-out wait produced reports the *symptom* of the timeout
/// (`observed ""`) instead of the timeout itself, which is what made the CI
/// failure above look like a wrap-strategy regression. This keeps the two
/// diagnoses distinguishable — "no line was ever recorded" is this panic, "the
/// wrong line was recorded" is the caller's `assert_eq!`.
#[cfg(unix)]
fn expect_launch_lines(record: &std::path::Path, want: usize) {
    common::wait_for_file_lines(record, want, RECORD_WAIT).unwrap_or_else(|state| {
        panic!(
            "recorder never captured {want} complete launch line(s) within {RECORD_WAIT:?}: {state}"
        )
    });
}

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
    use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};

    let fixture = common::harness_tempdir().expect("respawn recorder fixture");
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
    // compiled rather than whatever is installed on $PATH), which means a fake
    // `dot-agent-deck` on the child's PATH is no longer what runs — this
    // override is the seam for observing it. Taken before the runtime is built
    // (not inside the async block) so the guard is never held across an await.
    let _wrap_bin = WrapBinOverride::pointing_at(&bin_dir.join("dot-agent-deck"));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build respawn runtime");
    runtime.block_on(async {
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

        // Wait for a COMPLETE recorded line, not merely for the file to exist:
        // the shim appends with `>>`, and the shell creates the file before
        // `printf` writes into it, so an existence-only wait can read an empty
        // string (PRD #225 — same defect as `orchestration/delegate/009`).
        expect_launch_lines(&record, 1);
        let launched = std::fs::read_to_string(&record).expect("read respawn recorder");
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

    let fixture = common::harness_tempdir().expect("explicit Codex identity fixture");
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
    // this override is the seam for observing it.
    let _wrap_bin = WrapBinOverride::pointing_at(&bin_dir.join("dot-agent-deck"));
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

    // Wait for a COMPLETE recorded line, not merely for the file to exist: the
    // shim appends with `>>`, and the shell creates the file before `printf`
    // writes into it, so an existence-only wait can read an empty string
    // (PRD #225 — same defect as `orchestration/delegate/009`).
    expect_launch_lines(&record, 1);
    let launched = std::fs::read_to_string(&record).expect("read spawn recorder");
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
    use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};

    let fixture = common::harness_tempdir().expect("stable respawn fixture");
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
    // spawn/006). Without it this assertion cannot fail for the regression it
    // guards: the rewrite now names the co-located build by absolute path, so an
    // unwanted wrapper would exec the REAL deck, which in turn execs `devbox run
    // codex-big` — and the devbox stub would record the very same `BARE devbox
    // run codex-big` line as the unwrapped launch. With the override, a wrapper
    // shows up as a `WRAPPED …` line instead. Taken before the runtime is built
    // (not inside the async block) so the guard is never held across an await.
    let _wrap_bin = WrapBinOverride::pointing_at(&bin_dir.join("dot-agent-deck"));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build stable respawn runtime");
    runtime.block_on(async {
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

        expect_launch_lines(&record, 1);
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
        expect_launch_lines(&record, 2);
        let launched = std::fs::read_to_string(&record).expect("read stable respawn recorder");
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
    use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};

    let fixture = common::harness_tempdir().expect("launch-shape coherence fixture");
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
    // spawn/006). The rewrite names the co-located build by absolute path, so a
    // fake `dot-agent-deck` on the child's PATH is no longer what runs — without
    // this override the wrapped launches would exec the REAL deck and the
    // recorder would only ever see the inner `devbox` line. Taken before the
    // runtime is built (not inside the async block) so the guard is never held
    // across an await.
    let _wrap_bin = WrapBinOverride::pointing_at(&bin_dir.join("dot-agent-deck"));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build launch-shape coherence runtime");
    runtime.block_on(async {
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
        // Each launch is observed before the next respawn replaces the pane, so
        // the recorded sequence stays attributable line by line.
        expect_launch_lines(&record, 1);

        // The role command is untouched: the frozen identity is the only thing
        // that knows this launcher is Codex, so the wrapper must come back.
        registry
            .respawn_agent_for_pane("shape-pane", "devbox run codex-big")
            .await
            .expect("respawn with the unchanged role command");
        expect_launch_lines(&record, 2);

        // The user edited the role command in `.dot-agent-deck.toml` to a
        // different agent. The wrap decision must follow the command actually
        // being launched — never `wrap --agent codex -- claude …`.
        registry
            .respawn_agent_for_pane("shape-pane", "claude --model haiku")
            .await
            .expect("respawn with the edited role command");
        expect_launch_lines(&record, 3);

        let launched = std::fs::read_to_string(&record).expect("read launch-shape recorder");
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
