//! The installation fixture shared by e2e and no-e2e production controls.
#![allow(dead_code)] // Each build exercises a different part of the fixture.

use crate::common;
use crate::inbox::attach;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub(crate) fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

pub(crate) fn read_json(path: &Path) -> Value {
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

pub(crate) struct Spawn {
    pub(crate) base: PathBuf,
    pub(crate) report: Value,
    pub(crate) original_argv: Value,
}

pub(crate) fn spawn(
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
    // A launcher runs Claude's executable below an assignment.
    let executable = bin.join(if kind == "launcher" { "claude" } else { kind });
    let script = if kind == "codex" {
        include_str!("codex_composer.py")
    } else {
        include_str!("claude_composer.py")
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
    let command = match kind {
        "shell" => format!(
            "/bin/bash --noprofile --norc -c {}",
            quote(Path::new(&format!("exec {direct}")))
        ),
        "launcher" => format!("DOT_AGENT_DECK_TEST_LAUNCHER=1 {direct}"),
        _ => direct,
    };
    let mut env = vec![
        json!(["DOT_AGENT_DECK_TEST_COMPOSER_REPORT", "1"]),
        json!(["DOT_AGENT_DECK_TEST_COMPOSER_REPORT_NAMES", "1"]),
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
    // A direct Codex is launched in embedded mode whether or not anything is
    // installed (#2002): the wrapper puts that pair in front of its arguments.
    let original_argv = if kind == "codex" {
        json!([
            executable,
            "-c",
            "shell_environment_policy.set={}",
            base,
            common::contained_deck_binary(),
            release
        ])
    } else {
        json!([executable, base, common::contained_deck_binary(), release])
    };
    Spawn {
        base,
        report,
        original_argv,
    }
}

pub(crate) fn installed(spawn: &Spawn) -> PathBuf {
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

pub(crate) fn tool(spawn: &Spawn, argv: &[String]) -> Value {
    let request = spawn.base.with_extension("bash-tool.json");
    let staging = tempfile::Builder::new()
        .prefix(&format!(
            "{}.tmp-",
            request.file_name().unwrap().to_str().unwrap()
        ))
        .tempfile_in(request.parent().unwrap())
        .unwrap()
        .into_temp_path();
    let id = staging.file_name().unwrap().to_str().unwrap();
    std::fs::write(
        &staging,
        serde_json::to_vec(&json!({"id": id, "argv": argv})).unwrap(),
    )
    .unwrap();
    std::fs::rename(&staging, request).unwrap();
    let result_path = spawn.base.with_extension("bash-tool-result.json");
    let result = read_json(&result_path);
    assert_eq!(result["id"], id, "the tool result echoes its request id");
    std::fs::remove_file(result_path).unwrap();
    result
}
