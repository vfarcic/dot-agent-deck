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

/// The first `name` on this process's `PATH`.
fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// A stand-in for the real Codex that is not a shell script — the shim adds
/// nothing to a shell script, which may be a user's wrapper — printing its
/// `argv` one per line, then `guard=` and the shim's guard variable, and
/// exiting with `status`. Python, by absolute path, so it runs on the
/// cleared `PATH` these tests use.
fn recorder(status: u8) -> String {
    let python = on_path("python3").expect("python3 on PATH for the Codex stand-in");
    format!(
        "#!{}\nimport os, sys\nfor a in sys.argv: print(a)\nprint('guard=' + os.environ.get('DOT_AGENT_DECK_TELEMETRY_SHIMMED', ''))\nsys.exit({status})\n",
        python.display()
    )
}

/// `cmd`'s output, failing the test instead of hanging when it has not
/// finished within ten seconds: a shim that keeps running itself never would.
fn output_within(cmd: &mut Command) -> std::process::Output {
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            panic!("the shim did not reach the real program within 10 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
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
    let (shim_dir, real) = fixture(root.path(), &recorder(7));
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

/// The resolved `codex` is the user's own shell wrapper that points
/// `CODEX_HOME` at a configuration opting out of both exporters and then runs
/// the real Codex by its full path: the shim cannot have read the
/// configuration Codex will, so it adds nothing.
#[test]
fn a_users_shell_wrapper_named_codex_gets_nothing() {
    let root = test_temp::tempdir().unwrap();
    let (shim_dir, real) = fixture(root.path(), &recorder(0));
    let opted_out = root.path().join("opted-out");
    std::fs::create_dir(&opted_out).unwrap();
    std::fs::write(
        opted_out.join("config.toml"),
        "[otel]\ntrace_exporter = \"none\"\nmetrics_exporter = \"none\"\n",
    )
    .unwrap();
    let wrapper_dir = root.path().join("wrapper");
    std::fs::create_dir(&wrapper_dir).unwrap();
    std::fs::write(
        wrapper_dir.join("codex"),
        format!(
            "#!/bin/sh\nexport CODEX_HOME='{}'\nexec '{}' \"$@\"\n",
            opted_out.display(),
            real.join("codex").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        wrapper_dir.join("codex"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut cmd = Command::new(shim_dir.join("codex"));
    cmd.args(["a b", "--continue"])
        .env_clear()
        .env("PATH", path_of(&[&shim_dir, &wrapper_dir, &real]))
        .env("HOME", &home)
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318");
    let out = output_within(&mut cmd);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        [
            real.join("codex").to_str().unwrap(),
            "a b",
            "--continue",
            "guard=codex"
        ],
        "the wrapper ran, the real Codex got no overrides"
    );
}

/// Codex as npm installs it — a Node script — gets the additions.
#[test]
fn a_node_launcher_gets_the_additions() {
    let Some(node) = on_path("node") else {
        eprintln!("SKIP: node is not on PATH for the npm-style Codex launcher");
        return;
    };
    let root = test_temp::tempdir().unwrap();
    let (shim_dir, real) = fixture(
        root.path(),
        "#!/usr/bin/env node\nfor (const a of process.argv.slice(2)) console.log(a);\n",
    );
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut cmd = Command::new(shim_dir.join("codex"));
    cmd.args(["a b"])
        .env_clear()
        .env(
            "PATH",
            path_of(&[
                &shim_dir,
                &real,
                node.parent().unwrap(),
                Path::new("/usr/bin"),
            ]),
        )
        .env("HOME", &home)
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318");
    let out = output_within(&mut cmd);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 5, "{lines:?}");
    assert_eq!(lines[0], "-c");
    assert!(lines[1].starts_with("otel.trace_exporter="), "{lines:?}");
    assert_eq!(lines[2], "-c");
    assert!(lines[3].starts_with("otel.metrics_exporter="), "{lines:?}");
    assert_eq!(lines[4], "a b");
}

/// Two deck installations on one `PATH`, each before the real program, and
/// a shim script copied or hard-linked outside its directory: the real
/// program runs, instead of the shims running each other forever.
#[test]
fn other_shims_on_path_are_passed_over() {
    let root = test_temp::tempdir().unwrap();
    let (shim_a, real) = fixture(root.path(), &recorder(0));
    let deck = env!("CARGO_BIN_EXE_dot-agent-deck");
    let shim_b = shim::install(&root.path().join("other-telemetry"), deck).unwrap();
    let copied = root.path().join("copied");
    std::fs::create_dir(&copied).unwrap();
    std::fs::copy(shim_a.join("codex"), copied.join("codex")).unwrap();
    let linked = root.path().join("linked");
    std::fs::create_dir(&linked).unwrap();
    std::fs::hard_link(shim_b.join("codex"), linked.join("codex")).unwrap();
    for (first, path) in [
        (&shim_a, path_of(&[&shim_a, &shim_b, &real])),
        (&shim_b, path_of(&[&shim_b, &shim_a, &real])),
        (
            &shim_a,
            path_of(&[&shim_a, &copied, &linked, &shim_b, &real]),
        ),
        (&copied, path_of(&[&copied, &linked, &real])),
    ] {
        let mut cmd = Command::new(first.join("codex"));
        cmd.arg("x")
            .env_clear()
            .env("PATH", &path)
            .env("HOME", root.path());
        let out = output_within(&mut cmd);
        assert!(out.status.success(), "{path:?}: {out:?}");
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert_eq!(
            stdout.lines().next(),
            real.join("codex").to_str(),
            "{path:?}: the real program"
        );
    }
}

/// An empty `PATH` entry is the current directory, for the shim as for the
/// shell: `PATH=<shim>:`, as the deck sets for an agent whose `PATH` was
/// empty, finds `./codex`.
#[test]
fn an_empty_path_entry_is_the_current_directory() {
    let root = test_temp::tempdir().unwrap();
    let (shim_dir, real) = fixture(root.path(), &recorder(0));
    let mut path = shim_dir.clone().into_os_string();
    path.push(":");
    let mut cmd = Command::new(shim_dir.join("codex"));
    cmd.current_dir(&real)
        .env_clear()
        .env("PATH", &path)
        .env("HOME", root.path());
    let out = output_within(&mut cmd);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().lines().next(),
        Some("./codex")
    );
}

/// A configuration home that is not UTF-8 cannot be read exactly as Codex
/// will read it, so the launch gets nothing rather than a decision made from
/// another configuration.
#[test]
fn a_configuration_home_it_cannot_read_exactly_gets_nothing() {
    use std::os::unix::ffi::OsStrExt;
    let root = test_temp::tempdir().unwrap();
    let (shim_dir, real) = fixture(root.path(), &recorder(0));
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut cmd = Command::new(shim_dir.join("codex"));
    cmd.arg("x")
        .env_clear()
        .env("PATH", path_of(&[&shim_dir, &real]))
        .env("HOME", &home)
        .env("CODEX_HOME", std::ffi::OsStr::from_bytes(b"/opted/out\xff"))
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318");
    let out = output_within(&mut cmd);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        stdout.lines().skip(1).collect::<Vec<_>>(),
        ["x", "guard=codex"]
    );
}
