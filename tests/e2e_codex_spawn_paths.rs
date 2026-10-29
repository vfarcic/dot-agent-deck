#![cfg(feature = "e2e")]

//! Synthetic coverage through the real wrapper; the real-agent case is lane 2.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::TuiDeck;
use spec::spec;

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, contents).expect("write recorder executable");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod recorder executable");
}

#[cfg(unix)]
struct EmbeddedFixture {
    root: tempfile::TempDir,
    config: String,
}

#[cfg(unix)]
impl EmbeddedFixture {
    fn new(user_exporter: bool) -> Self {
        let root = common::harness_tempdir().expect("embedded Codex fixture");
        let npm_bin = root.path().join("node_modules/@openai/codex/bin");
        std::fs::create_dir_all(&npm_bin).unwrap();
        // The npm-shaped path lets the real telemetry shim recognize this
        // synthetic executable and exercise its exporter-addition path.
        write_executable(
            &npm_bin.join("codex.js"),
            include_str!("fixtures/codex-embedded.py"),
        );
        std::os::unix::fs::symlink(npm_bin.join("codex.js"), root.path().join("codex")).unwrap();
        write_executable(
            &root.path().join("dot-agent-deck"),
            "#!/bin/sh\nprintf 'WRAPPED %s\\n' \"$*\" >> \"$CODEX_PATH_RECORD\"\nexec \"$EMBEDDED_DECK_BIN\" \"$@\"\n",
        );
        write_executable(
            &root.path().join("devbox"),
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$LAUNCHER_ARGS_RECORD\"\nprintf 'LAUNCHER_SUCCESS\\n'\nexec \"$SYNTHETIC_CODEX_BIN\" -c 'shell_environment_policy.set={}'\n",
        );
        let home = root.path().join("codex-home");
        std::fs::create_dir(&home).unwrap();
        let mut config = "model = \"fixture-model\"\n[shell_environment_policy]\nset = { FIXTURE_KEEP = \"kept\", EDITOR = \"fixture-editor\" }\n".to_string();
        if user_exporter {
            config.push_str("[otel]\ntrace_exporter = \"none\"\nmetrics_exporter = \"none\"\n");
        }
        std::fs::write(home.join("config.toml"), &config).unwrap();
        Self { root, config }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn builder(&self, telemetry: bool) -> common::TuiDeckBuilder {
        let mut builder = TuiDeck::builder()
            .without_agent_credentials()
            .with_pty_size(180, 45)
            // Do not inherit host agent binaries or telemetry shims.
            .with_env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.path().display()),
            )
            .with_env("SHELL", "/bin/sh")
            .with_env("CODEX_HOME", self.path("codex-home").to_string_lossy())
            .with_env("CODEX_PATH_RECORD", self.path("wrap.log").to_string_lossy())
            .with_env(
                "CODEX_ARGS_RECORD",
                self.path("args.jsonl").to_string_lossy(),
            )
            .with_env(
                "LAUNCHER_ARGS_RECORD",
                self.path("launcher.log").to_string_lossy(),
            )
            .with_env("SYNTHETIC_CODEX_BIN", self.path("codex").to_string_lossy())
            .with_env(
                "EMBEDDED_DECK_BIN",
                common::contained_deck_binary().to_string_lossy(),
            )
            .with_env(
                "DOT_AGENT_DECK_WRAP_BIN",
                self.path("dot-agent-deck").to_string_lossy(),
            );
        if telemetry {
            builder = builder
                .with_env("OTEL_SDK_DISABLED", "false")
                .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9")
                .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json");
        } else {
            builder = builder.with_env("OTEL_SDK_DISABLED", "true");
        }
        builder
    }

    fn observe(&self, deck: &TuiDeck, expected_command: &str) -> (Vec<String>, String) {
        common::wait_for_file_lines(&self.path("args.jsonl"), 1, Duration::from_secs(15))
            .unwrap_or_else(|state| {
                panic!(
                    "synthetic Codex never started: {state}\n{}",
                    deck.snapshot_grid()
                )
            });
        let records = std::fs::read_to_string(self.path("args.jsonl")).unwrap();
        let args: Vec<Vec<String>> = records
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            args.len(),
            1,
            "one synthetic Codex launch, no double wrapper"
        );
        let wrapped = std::fs::read_to_string(self.path("wrap.log")).unwrap();
        assert_eq!(
            wrapped.lines().collect::<Vec<_>>(),
            [format!("WRAPPED wrap --agent codex -- {expected_command}")]
        );
        assert_eq!(
            std::fs::read(self.path("codex-home/config.toml")).unwrap(),
            self.config.as_bytes(),
            "launch must preserve fixture config.toml byte for byte"
        );
        // Both modes remain interactive, so RED reflects the missing prefix
        // rather than a fixture that exited before the pane could be attached.
        assert!(
            deck.wait_for_grid_predicate_within(Duration::from_secs(10), |grid| grid
                .contains("EMBEDDED_SUCCESS")
                || grid.contains("SHARED_SELECTION")),
            "synthetic mode sentinel never became visible\n{}",
            deck.snapshot_grid()
        );
        deck.send_bytes(b"probe-2002\r");
        assert!(
            deck.wait_for_grid_string_within("INPUT_SUCCESS:probe-2002", Duration::from_secs(10)),
            "pane must remain usable\n{}",
            deck.snapshot_grid()
        );
        let grid = deck.snapshot_grid();
        (args.into_iter().next().unwrap(), grid)
    }
}

#[cfg(unix)]
fn embedded_failure(label: &str, args: &[String], grid: &str) -> Option<String> {
    let pair = ["-c", "shell_environment_policy.set={}"];
    let count = args
        .windows(2)
        .filter(|window| window[0] == pair[0] && window[1] == pair[1])
        .count();
    assert!(
        !args.iter().any(|arg| arg == "--no-daemon"),
        "old-release compatibility: {args:?}"
    );
    if args.starts_with(&pair.map(String::from))
        && count == 1
        && grid.contains("EMBEDDED_SUCCESS")
        && !grid.contains("SHARED_SELECTION")
    {
        None
    } else {
        let grid = grid
            .lines()
            .filter(|line| {
                ["SUCCESS", "SELECTION", "Codex", "STDERR_SENTINEL"]
                    .iter()
                    .any(|needle| line.contains(needle))
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some(format!(
            "{label}: missing prepended embedded pair or visible embedded success; argv={args:?}, pair_count={count}\n{grid}"
        ))
    }
}

fn open_form(deck: &TuiDeck) {
    deck.wait_for_string("No active agents");
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
}

/// Scenario: Restore bare and already-prefixed synthetic Codex panes through the real wrapper. Each pane must visibly select embedded mode, remain usable, preserve its config, and receive the embedded pair exactly once.
#[spec("codex/spawn/001")]
#[test]
#[cfg(unix)]
fn spawn_001_plain_restore_wraps_codex() {
    let mut failures = Vec::new();
    for command in ["codex -c 'shell_environment_policy.set={}'", "codex"] {
        let fixture = EmbeddedFixture::new(false);
        let deck = fixture
            .builder(false)
            .with_continue_session("restored-codex", command)
            .launch_with_fixture("minimal");
        let (args, grid) = fixture.observe(&deck, &command.replace('\'', ""));
        if let Some(failure) = embedded_failure(command, &args, &grid) {
            failures.push(failure);
        } else {
            eprintln!("PASS restore control: {command}");
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Scenario: Select an orchestration whose start role runs synthetic Codex through the real wrapper. The role pane must visibly select embedded mode, accept input, and retain the fixture configuration.
#[spec("codex/spawn/003")]
#[test]
#[cfg(unix)]
fn spawn_003_orchestration_role_wraps_codex() {
    let fixture = EmbeddedFixture::new(false);
    let deck = fixture
        .builder(false)
        .launch_with_fixture("codex-spawn-paths");
    open_form(&deck);
    deck.send_keys(b"\x1b[C");
    deck.wait_for_absence("Command:");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    let (args, grid) = fixture.observe(&deck, "codex");
    assert!(
        embedded_failure("orchestration", &args, &grid).is_none(),
        "{}",
        embedded_failure("orchestration", &args, &grid).unwrap()
    );
}

/// Scenario: Open a role declaring Codex through a synthetic devbox launcher that chooses embedded mode for its inner invocation. The launcher must keep its original arguments, its pane must accept input, and its card must show Codex before a task or hook.
#[spec("codex/spawn/009")]
#[test]
#[cfg(unix)]
fn spawn_009_declared_orchestration_launcher_wraps_and_badges_codex() {
    let fixture = EmbeddedFixture::new(false);
    let deck = fixture.builder(false).launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    std::fs::write(
        deck.workdir().join(".dot-agent-deck.toml"),
        "[[orchestrations]]\n\
         name = \"declared-codex\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"recorder\"\n\
         command = \"devbox run codex-big\"\n\
         agent = \"codex\"\n\
         start = true\n\
         clear = false\n",
    )
    .expect("write declared orchestration config");

    open_form(&deck);
    deck.send_keys(b"\x1b[C");
    deck.wait_for_absence("Command:");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");

    let (args, pane_grid) = fixture.observe(&deck, "devbox run codex-big");
    assert_eq!(
        std::fs::read_to_string(fixture.path("launcher.log")).unwrap(),
        "run\ncodex-big\n",
        "declared launcher must receive its original argv without Codex options"
    );
    assert!(pane_grid.contains("LAUNCHER_SUCCESS"), "{pane_grid}");
    assert!(embedded_failure("launcher-selected inner mode", &args, &pane_grid).is_none());
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Agent n/Ctrl+N]");
    let codex_badge = deck.wait_for_grid_string_within("Codex ·", Duration::from_secs(5));
    let grid = deck.snapshot_grid();

    assert!(
        codex_badge,
        "a declared launcher must render a Codex badge before any task or hook; codex_badge={codex_badge}\nFinal grid:\n{grid}"
    );
}

/// Scenario: Start a real cheap-model Codex through a bespoke launcher script
/// used by an orchestration role that declares Codex, with the role command
/// opening on an environment assignment the launcher must see (issue #1685).
/// Before any prompt is submitted, the role card must already read Codex and the
/// real CLI must be live.
#[spec("codex/spawn/012")]
#[test]
#[cfg(all(unix, feature = "e2e-live"))]
fn spawn_012_real_script_launched_codex_badges_before_first_prompt() {
    skip_unless!(common::check_codex_available());

    let real_codex = std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("codex"))
                // Not the deck's telemetry shim, which a deck pane's `PATH`
                // can carry ahead of the real Codex: this test launches the
                // real program by absolute path, not the developer's shim.
                .find(|candidate| {
                    candidate.is_file() && !dot_agent_deck::telemetry::shim::is_shim_file(candidate)
                })
        })
        .expect("available Codex binary resolves on PATH");
    let deck = TuiDeck::builder()
        .with_pty_size(160, 42)
        .with_env("PATH", common::path_with_binary_dir())
        .with_env("REAL_CODEX_BIN", real_codex.to_string_lossy())
        .with_imported_codex_credentials()
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");

    let work = deck.workdir().to_path_buf();
    let launch_record = work.join("real-script-codex.log");
    write_executable(
        &work.join("run-codex.sh"),
        "#!/bin/sh\nprintf 'REAL_CODEX probe=%s %s\\n' \"${CODEX_LAUNCH_PROBE:-unset}\" \"$*\" >> real-script-codex.log\nexec \"$REAL_CODEX_BIN\" \"$@\"\n",
    );
    let command = format!(
        "CODEX_LAUNCH_PROBE=1685 ./run-codex.sh --model {} --sandbox workspace-write --ask-for-approval never -c 'sandbox_workspace_write.network_access=true' -c 'model_reasoning_effort=\"low\"'",
        common::codex_test_model(),
    );
    std::fs::write(
        work.join(".dot-agent-deck.toml"),
        format!(
            "[[orchestrations]]\n\
             name = \"real-declared-codex\"\n\n\
             [[orchestrations.roles]]\n\
             name = \"real-codex\"\n\
             command = {command:?}\n\
             agent = \"codex\"\n\
             start = true\n\
             clear = false\n"
        ),
    )
    .expect("write real declared Codex orchestration config");

    let events = deck.subscribe_events();
    open_form(&deck);
    deck.send_keys(b"\x1b[C");
    deck.wait_for_absence("Command:");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    common::wait_for_file_containing(&launch_record, "REAL_CODEX", Duration::from_secs(20))
        .unwrap_or_else(|state| panic!("the bespoke real-Codex launcher never executed: {state}"));

    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Agent n/Ctrl+N]");
    let codex_badge = deck.wait_for_grid_string_within("Codex ·", Duration::from_secs(5));
    let grid = deck.snapshot_grid();
    let real_launch =
        std::fs::read_to_string(&launch_record).expect("read bespoke real-Codex launch record");
    let no_prompt_event = !events.snapshot().iter().any(|event| {
        event
            .user_prompt
            .as_deref()
            .is_some_and(|prompt| !prompt.is_empty())
    });

    // Issue #1685: a command opening on an assignment used to hand the wrapper
    // the assignment as its program, and the launcher never ran.
    let saw_assignment = real_launch.contains("REAL_CODEX probe=1685 ");

    assert!(
        saw_assignment && codex_badge && no_prompt_event && grid.contains("Idle"),
        "the real script-launched Codex role, started through a leading assignment, must render a Codex Idle card before any prompt is submitted; real_launch={real_launch:?}, saw_assignment={saw_assignment}, codex_badge={codex_badge}, no_prompt_event={no_prompt_event}\nFinal grid:\n{grid}"
    );
}

/// Scenario: Submit direct synthetic Codex commands through Ctrl+N with telemetry off, exporter additions, a user exporter, and an absolute executable path. The pane must visibly select embedded mode and accept input while user config overrides, model, prompt, and config-file bytes survive unchanged.
#[spec("codex/spawn/013")]
#[test]
#[cfg(unix)]
fn spawn_013_plain_new_pane_wraps_codex_once() {
    let mut failures = Vec::new();
    for (label, telemetry, exporter, absolute, user_config) in [
        ("telemetry off", false, false, false, None),
        ("exporter additions", true, false, false, None),
        ("user exporter", true, true, false, None),
        ("absolute Codex", false, false, true, None),
        (
            "whole-table user override",
            false,
            false,
            false,
            Some("shell_environment_policy.set={USER_KEEP=\"whole\"}"),
        ),
        (
            "nested user override",
            false,
            false,
            false,
            Some("shell_environment_policy.set.USER_KEEP=\"nested\""),
        ),
    ] {
        let fixture = EmbeddedFixture::new(exporter);
        let program = if absolute {
            fixture.path("codex").to_string_lossy().into_owned()
        } else {
            "codex".into()
        };
        let mut user_args = Vec::new();
        if let Some(config) = user_config {
            user_args.extend(["-c".to_string(), config.to_string()]);
        }
        user_args.extend([
            "--model".into(),
            "mini-fixture".into(),
            "prompt with spaces".into(),
        ]);
        let suffix = user_args
            .iter()
            .map(|arg| format!("'{arg}'"))
            .collect::<Vec<_>>()
            .join(" ");
        let command = format!("{program} {suffix}");
        let deck = fixture.builder(telemetry).launch_with_fixture("minimal");
        open_form(&deck);
        deck.send_keys(b"\r"); // Mode → Name
        deck.send_keys(b"\r"); // Name → Command
        deck.send_keys(command.as_bytes());
        deck.send_keys(b"\r");
        let (args, grid) = fixture.observe(&deck, &format!("{program} {}", user_args.join(" ")));
        assert!(
            args.ends_with(&user_args),
            "{label}: user arguments changed: {args:?}"
        );
        let exporters = args.iter().filter(|arg| arg.starts_with("otel.")).count();
        assert_eq!(
            exporters,
            if label == "exporter additions" { 2 } else { 0 },
            "{label}: exercise the intended telemetry branch: {args:?}"
        );
        eprintln!(
            "PASS {label}: original argv suffix, exporter branch, unchanged config, wrapper once, usable pane, no --no-daemon"
        );
        if let Some(failure) = embedded_failure(label, &args, &grid) {
            failures.push(failure);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
