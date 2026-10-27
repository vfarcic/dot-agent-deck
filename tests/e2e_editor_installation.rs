#![cfg(all(feature = "e2e", target_os = "linux"))]

//! Installation contracts at the real spawn boundary. All agents are Python
//! stand-ins; no real editor, agent, user configuration or production endpoint.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use inbox::attach;
use serde_json::{Value, json};
use spec::spec;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const ASSUME: &str = "DOT_AGENT_DECK_E2E_ASSUME_EDITOR_PROBE";
const ENTRIES: &str = "claude@2.1.310,codex@0.164.0";

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn read_json(path: &Path) -> Value {
    assert!(
        common::wait_until(common::child_boot_budget(), || {
            std::fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .is_some()
        }),
        "fixture must publish complete JSON at {}",
        path.display()
    );
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

struct Spawn {
    base: PathBuf,
    report: Value,
    original_argv: Value,
}

fn spawn(
    daemon: &common::DaemonProc,
    name: &str,
    kind: &str,
    release: &str,
    originals: Option<&str>,
    preset: Option<&Path>,
    declared: bool,
) -> Spawn {
    let bin = daemon.home.join(format!("bin-{name}"));
    std::fs::create_dir(&bin).unwrap();
    let executable = bin.join(kind);
    let script = if kind == "codex" {
        include_str!("support/codex_composer.py")
    } else {
        include_str!("support/claude_composer.py")
    };
    // A version query must not start a tty fixture or discover a real agent.
    let script = script.replacen(
        "tty.setraw(0, termios.TCSANOW)",
        &format!(
            "if sys.argv[1:] == ['--version']:\n    print('{release}')\n    sys.exit(0)\ntty.setraw(0, termios.TCSANOW)"
        ),
        1,
    );
    std::fs::write(&executable, script).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let base = daemon.home.join(format!("fixture-{name}"));
    let direct = format!(
        "{} {} {} {release}",
        quote(&executable),
        quote(&base),
        quote(common::contained_deck_binary())
    );
    let command = if kind == "shell" {
        format!(
            "/bin/bash --noprofile --norc -c {}",
            quote(Path::new(&format!("exec {direct}")))
        )
    } else {
        direct
    };
    let mut env = vec![
        json!(["DOT_AGENT_DECK_TEST_COMPOSER_REPORT", "1"]),
        json!(["DOT_AGENT_DECK_TEST_COMPOSER_BASH_TOOL", "1"]),
        json!(["DOT_AGENT_DECK_TEST_COMPOSER_RELEASE", release]),
        json!(["PATH", format!("{}:/usr/bin:/bin", bin.display())]),
    ];
    if let Some(value) = originals {
        env.push(json!(["VISUAL", value]));
        env.push(json!(["EDITOR", value]));
    }
    if let Some(path) = preset {
        env.push(json!(["CLAUDE_ENV_FILE", path]));
    }
    let mut request = json!({
        "op": "start-agent", "command": command, "cwd": daemon.home,
        "rows": 40, "cols": 140, "env": env, "display_name": name,
    });
    if declared {
        request["agent_type"] = json!(if kind == "codex" {
            "codex"
        } else {
            "claude_code"
        });
    }
    let reply = attach(daemon, request);
    assert_eq!(reply["ok"], true, "spawn {name}: {reply}");
    let report = read_json(&base.with_extension("spawn.json"));
    let original_argv = json!([executable, base, common::contained_deck_binary(), release]);
    Spawn {
        base,
        report,
        original_argv,
    }
}

fn installed(spawn: &Spawn) -> PathBuf {
    let visual = spawn.report["env"]["VISUAL"].as_str().unwrap_or("");
    assert_eq!(
        Path::new(visual).file_name().and_then(|v| v.to_str()),
        Some("dot-agent-deck-editor"),
        "installation must replace VISUAL before the stand-in starts: {}",
        spawn.report
    );
    let helper = PathBuf::from(visual);
    assert!(helper.is_file(), "the installed private helper exists");
    assert_eq!(
        helper.metadata().unwrap().permissions().mode() & 0o777,
        0o500,
        "the private helper is read/execute for its owner"
    );
    helper
}

fn tool(spawn: &Spawn, argv: &[String]) -> Value {
    let request = spawn.base.with_extension("bash-tool.json");
    let staging = spawn.base.with_extension("bash-tool.staging");
    std::fs::write(&staging, serde_json::to_vec(argv).unwrap()).unwrap();
    std::fs::rename(staging, request).unwrap();
    read_json(&spawn.base.with_extension("bash-tool-result.json"))
}

/// Scenario: Start synthetic Claude, Codex, OpenCode, Pi and wrapper commands with installation disabled, preserving unset, empty and nonempty editor values. Their startup reports must show the original environment and the original positional arguments, with no helper substituted.
#[spec("inbox/probe/012")]
#[test]
fn probe_012_disabled_installation_preserves_spawn_environment_and_arguments() {
    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[]);
    for kind in ["claude", "codex", "opencode", "pi", "wrapper"] {
        for (state, original) in [
            ("unset", None),
            ("empty", Some("")),
            ("set", Some("user-editor --literal")),
        ] {
            let name = format!("{kind}-{state}");
            let spawned = spawn(&daemon, &name, kind, "2.1.310", original, None, false);
            assert_eq!(
                spawned.report["env"]["VISUAL"],
                json!(original),
                "{name}: VISUAL"
            );
            assert_eq!(
                spawned.report["env"]["EDITOR"],
                json!(original),
                "{name}: EDITOR"
            );
            assert_eq!(
                spawned.report["env"]["CLAUDE_ENV_FILE"],
                Value::Null,
                "{name}: no injected environment file"
            );
            assert_eq!(
                spawned.report["argv"], spawned.original_argv,
                "{name}: complete original argv"
            );
        }
    }
}

/// Scenario: Enable installation for exact Claude and Codex releases, then start direct stand-ins whose headers report measured or unmeasured releases, plus declared and undeclared wrappers. Each command receives a private helper before producing its header, including the unmeasured release; the user's original editor state does not decide installation.
#[spec("inbox/probe/033")]
#[test]
fn probe_033_installation_precedes_the_header_for_direct_and_wrapper_commands() {
    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[(ASSUME, ENTRIES)]);
    for (class, kind, release, declared) in [
        ("claude", "claude", "2.1.310", false),
        ("unmeasured", "claude", "2.1.399", false),
        ("codex", "codex", "0.164.0", false),
        ("launcher", "launcher", "2.1.310", true),
        ("undeclared", "wrapper", "2.1.310", false),
    ] {
        for (state, original) in [
            ("unset", None),
            ("empty", Some("")),
            ("set", Some("user-editor --literal")),
        ] {
            let name = format!("{class}-{state}");
            let spawned = spawn(&daemon, &name, kind, release, original, None, declared);
            let helper = installed(&spawned);
            assert!(
                helper.starts_with(daemon.home.parent().unwrap()),
                "helper stays in the test sandbox: {helper:?}"
            );
            if kind != "codex" {
                let env_file = spawned.report["env"]["CLAUDE_ENV_FILE"]
                    .as_str()
                    .unwrap_or("");
                assert!(
                    Path::new(env_file).is_file(),
                    "Claude-style installation supplies its tool environment file: {}",
                    spawned.report
                );
            } else {
                let argv = spawned.report["argv"].as_array().unwrap();
                let settings: Vec<_> = argv
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|arg| arg.starts_with("shell_environment_policy."))
                    .collect();
                assert!(
                    !settings.iter().any(|arg| arg.contains("exclude")),
                    "never replace the user's exclude policy"
                );
                for variable in ["VISUAL", "EDITOR"] {
                    let prefix = format!("shell_environment_policy.set.{variable}=");
                    assert_eq!(
                        settings
                            .iter()
                            .filter(|arg| arg.starts_with(&prefix))
                            .count(),
                        usize::from(original.is_some()),
                        "set only exported values: {settings:?}"
                    );
                }
            }
        }
    }
}

/// Scenario: Enable the editor-probe assumption and start a shell command that subsequently execs the Claude screen fixture. Its child retains the user's editor values, because shell panes are excluded from installation before any agent is learned.
#[spec("inbox/probe/033")]
#[test]
fn probe_033_a_shell_that_execs_an_agent_is_not_installed() {
    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[(ASSUME, ENTRIES)]);
    for (state, original) in [
        ("unset", None),
        ("empty", Some("")),
        ("set", Some("user-editor --literal")),
    ] {
        let spawned = spawn(&daemon, state, "shell", "2.1.310", original, None, false);
        assert_eq!(spawned.report["env"]["VISUAL"], json!(original));
        assert_eq!(spawned.report["env"]["EDITOR"], json!(original));
        assert_eq!(spawned.report["env"]["CLAUDE_ENV_FILE"], Value::Null);
    }
}

/// Scenario: Start the real daemon with malformed, duplicate, unknown and metadata-suffixed editor-probe assumption lists in separate sandboxes. Each daemon must report refusal through its real assumption reader at startup, before serving an attach endpoint; the test permits either rejecting startup or continuing with no assumptions.
#[spec("inbox/probe/033")]
#[test]
fn probe_033_invalid_assumptions_are_rejected_at_startup() {
    use std::process::{Command, Stdio};
    for list in [
        "broken",
        "claude@2.1.310,claude@2.1.310",
        "claude@99.0.0",
        "claude@2.1.310+metadata",
    ] {
        let root = common::race_safe_tempdir();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let socket = root.path().join("attach.sock");
        let log = home.join("startup.log");
        let mut command = Command::new(common::contained_deck_binary());
        command
            .args(["daemon", "serve"])
            .env_clear()
            .current_dir(&home)
            .env("HOME", &home)
            .env("PATH", common::contain_path("/usr/bin:/bin"))
            .env("DOT_AGENT_DECK_ATTACH_SOCKET", &socket)
            .env("DOT_AGENT_DECK_SOCKET", root.path().join("hook.sock"))
            .env("DOT_AGENT_DECK_STATE_DIR", root.path().join("state"))
            .env("DOT_AGENT_DECK_EXIT_WHEN_ORPHANED", "1")
            .env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "30")
            .env("OTEL_SDK_DISABLED", "true")
            .env("DOT_AGENT_DECK_LOG", &log)
            .env(ASSUME, list)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (key, value) in common::config_containment_env() {
            command.env(key, value);
        }
        let child = std::cell::RefCell::new(command.spawn().unwrap());
        let exited = std::cell::Cell::new(None);
        common::wait_until(common::child_boot_budget(), || {
            exited.set(child.borrow_mut().try_wait().unwrap());
            exited.get().is_some() || socket.exists()
        });
        let exited = exited.get();
        let refused = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("editor probe assumption refused");
        let mut child = child.into_inner();
        // Reap our captured child before asserting, including on today's RED.
        if exited.is_none() {
            child.kill().unwrap();
        }
        child.wait().unwrap();
        assert!(
            refused,
            "invalid editor assumption must be rejected at daemon startup: {list:?}; exited={exited:?}, refusal_logged={refused}, attach_ready={}",
            socket.exists()
        );
    }
}

/// Scenario: Start Claude stand-ins with unset, exported-empty and exported-nonempty editor settings under installation, then run a Bash tool that reads its environment. Sourcing the installed file restores those exact settings while the parent composer keeps the helper.
#[spec("inbox/probe/034")]
#[test]
fn probe_034_bash_tools_restore_unset_empty_and_exported_editor_values() {
    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[(ASSUME, ENTRIES)]);
    for (state, original) in [
        ("unset", None),
        ("empty", Some("")),
        ("set", Some("user-editor --literal")),
    ] {
        let spawned = spawn(&daemon, state, "claude", "2.1.310", original, None, false);
        installed(&spawned);
        let result = spawned.base.with_extension("tool-env.json");
        let code = "import os,json,sys; json.dump({k:os.environ.get(k) for k in ('VISUAL','EDITOR')},open(sys.argv[1],'w'))";
        assert_eq!(
            tool(
                &spawned,
                &[
                    "python3".into(),
                    "-c".into(),
                    code.into(),
                    result.to_string_lossy().into_owned()
                ]
            )["exit"],
            0
        );
        assert_eq!(
            read_json(&result),
            json!({"VISUAL":original,"EDITOR":original}),
            "tool environment preserves {state}"
        );
    }
}

/// Scenario: Give Claude a user environment file whose path contains spaces and shell metacharacters, install the helper, and run a Bash tool. The user's file is sourced after restoration, its overrides reach the tool, and its name is handled literally without executing a command embedded in it.
#[spec("inbox/probe/034")]
#[test]
fn probe_034_a_user_environment_file_is_sourced_after_editor_restoration() {
    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[(ASSUME, ENTRIES)]);
    let preset = daemon.home.join("user env ' ; literal.sh");
    std::fs::write(&preset, "export VISUAL='user override'\nunset EDITOR\n").unwrap();
    let spawned = spawn(
        &daemon,
        "preset",
        "claude",
        "2.1.310",
        Some("before preset"),
        Some(&preset),
        false,
    );
    installed(&spawned);
    assert_ne!(
        spawned.report["env"]["CLAUDE_ENV_FILE"],
        json!(preset),
        "the installed file chains the user's file"
    );
    let result = spawned.base.with_extension("tool-env.json");
    let code = "import os,json,sys; json.dump({k:os.environ.get(k) for k in ('VISUAL','EDITOR')},open(sys.argv[1],'w'))";
    assert_eq!(
        tool(
            &spawned,
            &[
                "python3".into(),
                "-c".into(),
                code.into(),
                result.to_string_lossy().into_owned()
            ]
        )["exit"],
        0
    );
    assert_eq!(
        read_json(&result),
        json!({"VISUAL":"user override","EDITOR":null})
    );
}

/// Scenario: Start a Claude stand-in with the user's sentinel editor and helper installation enabled, then run a real git commit from its Bash tool with no probe armed. The tool must use the restored editor, create the sentinel once, and successfully commit the edited message.
#[spec("inbox/probe/034")]
#[test]
fn probe_034_a_tool_git_commit_opens_the_users_editor_without_an_arm() {
    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[(ASSUME, ENTRIES)]);
    let editor = daemon.home.join("sentinel-editor");
    let sentinel = daemon.home.join("editor-opened");
    std::fs::write(
        &editor,
        format!(
            "#!/bin/sh\nprintf 'fixture commit\\n' > \"$1\"\nprintf 'opened\\n' >> {}\n",
            quote(&sentinel)
        ),
    )
    .unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o700)).unwrap();
    let spawned = spawn(
        &daemon,
        "git",
        "claude",
        "2.1.310",
        Some(editor.to_str().unwrap()),
        None,
        false,
    );
    installed(&spawned);
    let repo = daemon.home.join("tool-repo");
    std::fs::create_dir(&repo).unwrap();
    let code = "import subprocess,sys; p=sys.argv[1]; subprocess.run(['git','init',p],check=True); subprocess.run(['git','-C',p,'-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','--allow-empty'],check=True)";
    assert_eq!(
        tool(
            &spawned,
            &[
                "python3".into(),
                "-c".into(),
                code.into(),
                repo.to_string_lossy().into_owned()
            ]
        )["exit"],
        0
    );
    assert_eq!(
        std::fs::read_to_string(sentinel).unwrap(),
        "opened\n",
        "the user's editor opened exactly once"
    );
}
