//! PRD #1741: the deck's binary run as the telemetry `PATH` shim — the
//! hidden entrypoint the shim scripts `exec` — finds the real program past
//! the shim directory and replaces itself with it.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use dot_agent_deck::telemetry::shim;

#[path = "../src/test_temp.rs"]
mod test_temp;

/// A shim directory as the daemon installs it, and a directory holding a
/// real `codex` with `script` as its body.
fn fixture(root: &Path, script: &str) -> (PathBuf, PathBuf) {
    let deck = env!("CARGO_BIN_EXE_dot-agent-deck");
    let shim_dir = shim::install(&root.join("telemetry"), deck).unwrap();
    let real = root.join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("codex"), script).unwrap();
    std::fs::set_permissions(real.join("codex"), std::fs::Permissions::from_mode(0o755)).unwrap();
    (shim_dir, real)
}

fn path_of(dirs: &[&Path]) -> std::ffi::OsString {
    std::env::join_paths(dirs).unwrap()
}

/// Run the installed `codex` shim script with `path` as `PATH` and `pairs`
/// as the rest of its environment.
fn run_shim(
    shim_dir: &Path,
    path: &std::ffi::OsStr,
    pairs: &[(&str, &Path)],
) -> std::process::Output {
    let mut cmd = Command::new(shim_dir.join("codex"));
    cmd.args(["a b", "--continue"])
        .env_clear()
        .env("PATH", path);
    for (k, v) in pairs {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

/// The shim execs the real program in its own place: it gets Codex's
/// exporters in front of the user's arguments and the guard, and its exit
/// status is the shim's.
#[test]
fn the_shim_execs_the_real_program_with_its_additions() {
    let root = test_temp::tempdir().unwrap();
    let (shim_dir, real) = fixture(
        root.path(),
        "#!/bin/sh\nprintf '%s\\n' \"$0\" \"$@\" \"guard=$DOT_AGENT_DECK_TELEMETRY_SHIMMED\"\nexit 7\n",
    );
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut cmd = Command::new(shim_dir.join("codex"));
    cmd.args(["a b", "--continue"])
        .env_clear()
        .env("PATH", path_of(&[&shim_dir, &real]))
        .env("HOME", &home)
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318")
        .env("OTEL_METRICS_EXPORTER", "none");
    let out = cmd.output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(7),
        "the program's own status: {out:?}"
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 6, "{lines:?}");
    assert_eq!(
        lines[0],
        real.join("codex").to_str().unwrap(),
        "the real program, past the shim"
    );
    assert_eq!(lines[1], "-c");
    assert!(
        lines[2]
            .starts_with("otel.trace_exporter={otlp-http={endpoint=\"http://c:4318/v1/traces\""),
        "{lines:?}"
    );
    assert_eq!(lines[3..], ["a b", "--continue", "guard=codex"]);
}

/// With nothing to add, the program gets exactly its own arguments.
#[test]
fn a_declined_launch_reaches_the_program_unchanged() {
    let root = test_temp::tempdir().unwrap();
    let (shim_dir, real) = fixture(root.path(), "#!/bin/sh\nprintf '%s\\n' \"$@\"\n");
    let out = run_shim(
        &shim_dir,
        &path_of(&[&shim_dir, &real]),
        &[("HOME", root.path())],
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "a b\n--continue\n");
}

/// No real program on `PATH` past the shim: the shell's message and status.
#[test]
fn a_missing_program_is_command_not_found() {
    let root = test_temp::tempdir().unwrap();
    let (shim_dir, _real) = fixture(root.path(), "#!/bin/sh\n");
    let out = run_shim(&shim_dir, &path_of(&[&shim_dir]), &[]);
    assert_eq!(out.status.code(), Some(127), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "codex: command not found\n"
    );
}
