#![cfg(all(feature = "e2e", target_os = "linux"))]

//! Installation contracts at the real spawn boundary. All agents are Python
//! stand-ins; no real editor, agent, user configuration or production endpoint.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

#[path = "support/editor_installation.rs"]
mod installation;

use installation::{installed, quote, read_json, spawn, tool};
use serde_json::{Value, json};
use spec::spec;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const ASSUME: &str = "DOT_AGENT_DECK_E2E_ASSUME_EDITOR_PROBE";
const ENTRIES: &str = "claude@2.1.310,codex@0.164.0";

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

/// Scenario: Start the real daemon with installation disabled, a runtime directory of its own, and stale helper variables in its own environment, then start synthetic Claude, Codex, OpenCode, Pi, wrapper, launcher and shell commands. No instance directory appears under the state or runtime directory at startup or after the spawns, no command inherits a variable under the helper's reserved prefix, and each keeps its own editor values and argv.
#[spec("inbox/probe/012")]
#[test]
fn probe_012_a_disabled_spawn_makes_no_files_and_inherits_no_helper_variable() {
    let runtime = common::race_safe_tempdir();
    let runtime_dir = runtime.path().to_str().unwrap().to_string();
    let daemon = common::spawn_daemon_serve_with_env(
        None,
        "0",
        &[
            ("XDG_RUNTIME_DIR", runtime_dir.as_str()),
            ("DOT_AGENT_DECK_EDITOR_GEN", "stale-generation"),
            ("DOT_AGENT_DECK_EDITOR_USER_VISUAL", "stale-visual"),
            ("DOT_AGENT_DECK_EDITOR_FUTURE", "stale"),
        ],
    );
    let state = daemon.home.parent().unwrap().join("state");
    let no_instance = |when: &str| {
        assert!(!state.join("editor").exists(), "{when}: no state root");
        assert!(
            !runtime.path().join("dot-agent-deck/editor").exists(),
            "{when}: no runtime root"
        );
    };
    no_instance("startup");
    let original = Some("user-editor --literal");
    for kind in [
        "claude", "codex", "opencode", "pi", "wrapper", "launcher", "shell",
    ] {
        let spawned = spawn(&daemon, kind, kind, "2.1.310", original, None, false);
        for name in ["VISUAL", "EDITOR"] {
            assert_eq!(
                spawned.report["env"][name],
                json!(original),
                "{kind}: {name}"
            );
        }
        assert_eq!(
            spawned.report["env"]["CLAUDE_ENV_FILE"],
            Value::Null,
            "{kind}"
        );
        assert_eq!(
            spawned.report["argv"], spawned.original_argv,
            "{kind}: argv"
        );
        let names = spawned.report["names"].as_array().unwrap();
        assert!(
            names.contains(&json!("DOT_AGENT_DECK_TEST_COMPOSER_RELEASE")),
            "{kind}: the report lists the environment's names"
        );
        let reserved: Vec<_> = names
            .iter()
            .filter(|n| n.as_str().unwrap().starts_with("DOT_AGENT_DECK_EDITOR_"))
            .collect();
        assert!(reserved.is_empty(), "{kind}: inherited {reserved:?}");
    }
    no_instance("after the spawns");
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

/// Scenario: Enable installation for Codex 0.164.0 and start a direct Codex stand-in with unset, exported-empty and nonempty editor settings. The argv it receives opens with the deck's embedded-mode pair, then carries its own arguments, then the installation's editor settings for the set values, so the empty table comes before the settings it would otherwise replace.
#[spec("inbox/probe/033")]
#[test]
fn probe_033_an_installed_codex_is_embedded_and_keeps_its_editor_settings() {
    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[(ASSUME, ENTRIES)]);
    for (state, original) in [
        ("unset", None),
        ("empty", Some("")),
        ("set", Some("user-editor --literal")),
    ] {
        let name = format!("codex-{state}");
        let spawned = spawn(&daemon, &name, "codex", "0.164.0", original, None, false);
        installed(&spawned);
        let mut expected = spawned.original_argv.as_array().unwrap().clone();
        if let Some(value) = original {
            for variable in ["VISUAL", "EDITOR"] {
                expected.push(json!("-c"));
                expected.push(json!(format!(
                    "shell_environment_policy.set.{variable}=\"{value}\""
                )));
            }
        }
        assert_eq!(
            spawned.report["argv"],
            Value::Array(expected),
            "{name}: the embedded pair first, the editor settings after it"
        );
    }
}

/// Scenario: Enable installation with the daemon's state directory below a directory its group can write, and a private runtime directory. The state directory's root is passed over, so a started Claude stand-in gets a private helper under the runtime directory instead, and nothing is made below the group-writable directory.
#[spec("inbox/probe/033")]
#[test]
fn probe_033_a_state_root_another_user_could_change_is_passed_over() {
    let sandbox = common::race_safe_tempdir();
    let shared = sandbox.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();
    let runtime = sandbox.path().join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let state = shared.join("state");
    let daemon = common::spawn_daemon_serve_with_env(
        None,
        "0",
        &[
            (ASSUME, ENTRIES),
            ("DOT_AGENT_DECK_STATE_DIR", state.to_str().unwrap()),
            ("XDG_RUNTIME_DIR", runtime.to_str().unwrap()),
        ],
    );
    let spawned = spawn(&daemon, "claude", "claude", "2.1.310", None, None, false);
    let helper = installed(&spawned);
    assert!(
        helper.starts_with(runtime.join("dot-agent-deck/editor")),
        "the helper is under the runtime root: {helper:?}"
    );
    assert!(
        !state.join("editor").exists(),
        "nothing under the state root"
    );
}

/// Scenario: Enable installation with the daemon's state directory below a directory its group can write, and no runtime directory installation may use: one set empty, one that is a symlink to a private directory, and one its group can write. In each, a started Claude stand-in keeps the user's editor value, uninstalled, and no helper root is made below the state root, the runtime path or the link's target. (Q35-13 review, auditor U3-03.)
#[spec("inbox/probe/033")]
#[test]
fn probe_033_a_refused_state_root_without_a_trusted_runtime_installs_nothing() {
    for case in ["empty", "symlink", "shared"] {
        let sandbox = common::race_safe_tempdir();
        let shared = sandbox.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();
        let state = shared.join("state");
        let real = sandbox.path().join("runtime-real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = sandbox.path().join("runtime");
        match case {
            "symlink" => std::os::unix::fs::symlink(&real, &runtime).unwrap(),
            "shared" => {
                std::fs::create_dir(&runtime).unwrap();
                std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o770)).unwrap();
            }
            _ => {}
        }
        let runtime_value = if case == "empty" {
            String::new()
        } else {
            runtime.to_str().unwrap().to_string()
        };
        let daemon = common::spawn_daemon_serve_with_env(
            None,
            "0",
            &[
                (ASSUME, ENTRIES),
                ("DOT_AGENT_DECK_STATE_DIR", state.to_str().unwrap()),
                ("XDG_RUNTIME_DIR", &runtime_value),
            ],
        );
        let spawned = spawn(
            &daemon,
            "claude",
            "claude",
            "2.1.310",
            Some("user-editor --literal"),
            None,
            false,
        );
        assert_eq!(
            spawned.report["env"]["VISUAL"],
            json!("user-editor --literal"),
            "{case}: an ordinary uninstalled spawn"
        );
        assert_eq!(
            spawned.report["env"]["CLAUDE_ENV_FILE"],
            Value::Null,
            "{case}: no environment file"
        );
        // The daemon keeps other state under `<runtime>/dot-agent-deck`; the
        // helper's root is `editor` below it.
        for refused in [
            state.join("editor"),
            real.join("dot-agent-deck/editor"),
            runtime.join("dot-agent-deck/editor"),
        ] {
            assert!(
                !refused.exists(),
                "{case}: nothing made under {}",
                refused.display()
            );
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
