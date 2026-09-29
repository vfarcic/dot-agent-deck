//! Issue #324: `Taskfile.yml` used to splice task variables — `{{.VERSION}}`
//! and `{{.NAME}}` in the release tasks, `{{.DIR}}` and friends in the `run*`
//! tasks — straight into shell text. go-task substitutes a template before the
//! shell parses the command, so a crafted value ran as code, and two of those
//! tasks hold `HOMEBREW_TAP_TOKEN` / `SCOOP_BUCKET_TOKEN`. Measured before the
//! fix: `task scoop-manifest 'VERSION=v1.2.3$(touch PWNED)'` exited 0 having
//! created `PWNED`.
//!
//! The fix binds every such value through a task's `env:` and validates the
//! release pair in `scripts/release-channel-vars.sh`. These tests hold both
//! halves:
//!
//! - one reads the real `Taskfile.yml` and fails on any `{{…}}` inside a
//!   command that is not on a short allowlist of templates whose output is
//!   inert — so the next task added with a spliced variable goes red here
//!   rather than waiting for the next audit;
//! - three source the validator under `bash` with good and hostile values;
//! - the last runs the two generating tasks through go-task itself, whose
//!   built-in interpreter (mvdan/sh) is what executes the task bodies in a
//!   release. It skips where `task` is not on PATH, except where
//!   `DAD_REQUIRE_GO_TASK` is set: ci.yml's Linux `build` job installs go-task
//!   and sets it, and devbox installs go-task too. `build-macos` does not, so
//!   there the `bash` tests are what runs; the script keeps to constructs both
//!   shells implement.
//!
//! Unix-only, like `pin_lockstep`: the validator needs a POSIX shell, and its
//! one automation caller is release.yml's Ubuntu `finalize` job.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use yaml_rust2::{Yaml, YamlLoader};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

fn bash_present() -> bool {
    Command::new("bash")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Templates allowed inside a command, each because its output cannot carry
/// a caller-chosen byte into the shell text:
///
/// - the `--release` conditional emits either that constant or nothing;
/// - `CLI_ARGS` is assembled by go-task with each argument shell-quoted
///   (checked on 3.53.1: `task run-stop -- 'a b' '$(id)'` reaches the script
///   as the two literal arguments), and it is the invoker's own argv anyway.
const INERT_TEMPLATES: &[&str] = &[
    r#"{{if eq .PROFILE "release"}}--release{{end}}"#,
    "{{.CLI_ARGS}}",
];

/// Every command string in the Taskfile, as `(task, command)`. A `cmds:`
/// entry is either a bare string or a mapping (`task:`, `cmd:`, …); the
/// mapping's string values are all templated by go-task, so all are checked.
fn commands(taskfile: &Yaml) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let tasks = taskfile["tasks"]
        .as_hash()
        .expect("Taskfile.yml has a `tasks:` mapping");
    for (name, task) in tasks {
        let name = name.as_str().unwrap_or("?").to_string();
        let Some(cmds) = task["cmds"].as_vec() else {
            continue;
        };
        for cmd in cmds {
            match cmd {
                Yaml::String(s) => out.push((name.clone(), s.clone())),
                Yaml::Hash(h) => {
                    for v in h.values() {
                        if let Yaml::String(s) = v {
                            out.push((name.clone(), s.clone()));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

#[test]
fn release_channel_vars_001_taskfile_commands_splice_no_task_variable() {
    let text = fs::read_to_string(repo_root().join("Taskfile.yml")).expect("read Taskfile.yml");
    let docs = YamlLoader::load_from_str(&text).expect("Taskfile.yml is valid YAML");
    let cmds = commands(&docs[0]);
    assert!(
        cmds.iter().any(|(t, _)| t == "homebrew-publish"),
        "the walk found no `homebrew-publish` command, so it is not reading the Taskfile's shape"
    );

    let mut offenders = Vec::new();
    for (task, cmd) in &cmds {
        let mut rest = cmd.clone();
        for inert in INERT_TEMPLATES {
            rest = rest.replace(inert, "");
        }
        if rest.contains("{{") {
            offenders.push(format!("  {task}: {}", cmd.lines().next().unwrap_or("")));
        }
    }
    assert!(
        offenders.is_empty(),
        "Taskfile.yml splices a task variable into shell text, where a crafted value runs as \
         code (issue #324). Bind it through the task's `env:` and quote it as a shell variable \
         instead:\n{}",
        offenders.join("\n")
    );
}

/// Source the validator with `version` / `name` in the environment and print
/// what it derived, one per line.
fn source(version: Option<&str>, name: Option<&str>, cwd: &Path) -> Output {
    let script = repo_root().join("scripts/release-channel-vars.sh");
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(
            r#". "$1" && printf '%s\n' "$VERSION_NO_V" "$BASE_URL" "$CLASS_NAME" "$CONFLICTS_WITH""#,
        )
        .arg("release-channel-vars-test")
        .arg(&script)
        .current_dir(cwd)
        .env_remove("DAD_RELEASE_VERSION")
        .env_remove("DAD_CHANNEL_NAME");
    if let Some(v) = version {
        cmd.env("DAD_RELEASE_VERSION", v);
    }
    if let Some(n) = name {
        cmd.env("DAD_CHANNEL_NAME", n);
    }
    cmd.output().expect("run bash")
}

#[test]
fn release_channel_vars_002_accepts_release_shapes_and_derives_the_formula_fields() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let cases = [
        (
            "v0.41.2",
            "dot-agent-deck",
            "0.41.2",
            "DotAgentDeck",
            "dot-agent-deck-beta",
        ),
        (
            "v0.42.0-rc.1",
            "dot-agent-deck-beta",
            "0.42.0-rc.1",
            "DotAgentDeckBeta",
            "dot-agent-deck",
        ),
        (
            "v1.0.0+build.7",
            "dot-agent-deck",
            "1.0.0+build.7",
            "DotAgentDeck",
            "dot-agent-deck-beta",
        ),
    ];
    for (version, name, no_v, class, conflicts) in cases {
        let out = source(Some(version), Some(name), dir.path());
        assert!(
            out.status.success(),
            "{version} / {name} was rejected: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let expected = format!(
            "{no_v}\nhttps://github.com/vfarcic/dot-agent-deck/releases/download/{version}\n\
             {class}\n{conflicts}\n"
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    }
}

#[test]
fn release_channel_vars_003_rejects_hostile_and_malformed_values_without_running_them() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let good_v = Some("v1.2.3");
    let good_n = Some("dot-agent-deck");
    let cases: &[(Option<&str>, Option<&str>)] = &[
        (Some("v1.2.3$(touch PWNED)"), good_n),
        (Some("v1.2.3\"; touch PWNED; echo \""), good_n),
        (Some("v1.2.3`touch PWNED`"), good_n),
        (Some("v1.2.3\n"), good_n),
        (Some("v1.2.3\ntouch PWNED"), good_n),
        (Some("1.2.3"), good_n),
        (Some("v01.2.3"), good_n),
        (Some("v1.2"), good_n),
        (Some("v1.2.3-é"), good_n),
        (Some(""), good_n),
        (None, good_n),
        (good_v, Some("x$(touch PWNED)")),
        (good_v, Some("../evil")),
        (good_v, Some("dot-agent-deck\n")),
        (good_v, Some("")),
        (good_v, None),
    ];
    for (version, name) in cases {
        let out = source(*version, *name, dir.path());
        assert!(
            !out.status.success(),
            "accepted VERSION={version:?} NAME={name:?}; stdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            out.stdout.is_empty(),
            "VERSION={version:?} NAME={name:?} was rejected but still derived values"
        );
        // The payload cases are the ones worth checking: a benign value like
        // `1.2.3` is a substring of the message's own `v1.2.3` example.
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("PWNED"),
            "the rejection echoed the rejected value into the log: {stderr}"
        );
    }
    assert!(
        !dir.path().join("PWNED").exists(),
        "a rejected value was executed"
    );
}

/// Run `dad_checksum dot-agent-deck-linux-amd64 [mode]` against a
/// `dist/checksums.txt` holding `checksums`.
fn checksum(checksums: &str, mode: &str) -> Output {
    let dir = tempfile::tempdir().expect("temp dir");
    fs::create_dir(dir.path().join("dist")).expect("dist/");
    fs::write(dir.path().join("dist/checksums.txt"), checksums).expect("checksums.txt");
    let script = repo_root().join("scripts/release-channel-vars.sh");
    Command::new("bash")
        .arg("-c")
        .arg(r#". "$1" && x=$(dad_checksum dot-agent-deck-linux-amd64 $2) || exit 1; printf '%s' "$x""#)
        .arg("release-channel-vars-test")
        .arg(&script)
        .arg(mode)
        .current_dir(dir.path())
        .env("DAD_RELEASE_VERSION", "v1.2.3")
        .env("DAD_CHANNEL_NAME", "dot-agent-deck")
        .output()
        .expect("run bash")
}

#[test]
fn release_channel_vars_004_checksum_helper_takes_exactly_one_sha256() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let sha = "a".repeat(64);
    let other = "b".repeat(64);
    let good = format!("{sha}  dot-agent-deck-linux-amd64\n{other}  dot-agent-deck-linux-arm64\n");
    let malformed = [
        "deadbeef  dot-agent-deck-linux-amd64\n".to_string(),
        format!("{}  dot-agent-deck-linux-amd64\n", sha.to_uppercase()),
        format!("{sha}  dot-agent-deck-linux-amd64\n{other}  dot-agent-deck-linux-amd64\n"),
    ];
    let absent = format!("{other}  dot-agent-deck-linux-arm64\n");

    for mode in ["", "optional"] {
        let out = checksum(&good, mode);
        assert!(
            out.status.success(),
            "mode {mode:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), sha, "mode {mode:?}");

        // `optional` relaxes only absence: a line that is there is still
        // held to the shape.
        for bad in &malformed {
            let out = checksum(bad, mode);
            assert!(
                !out.status.success(),
                "mode {mode:?} accepted checksums.txt {bad:?} and printed {:?}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
    }

    for missing in [String::new(), absent] {
        let out = checksum(&missing, "");
        assert!(
            !out.status.success(),
            "a required asset absent from checksums.txt {missing:?} was accepted"
        );
        let out = checksum(&missing, "optional");
        assert!(
            out.status.success() && out.stdout.is_empty(),
            "an optional asset absent from checksums.txt {missing:?} should print nothing and \
             succeed; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn task_present() -> bool {
    Command::new("task")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Greptile's finding on PR #1283: the tests above drive the validator under
/// `bash`, but go-task runs the task bodies under its own interpreter, and the
/// first version of this fix aborted `scoop-manifest` on the release's real
/// `dist/` — which has no Windows binary. So this runs the two generating
/// tasks through `task` itself, from a copy of the real Taskfile and
/// validator, against a `dist/` shaped like the one release.yml's `finalize`
/// assembles. It needs go-task on PATH (devbox installs it) and skips without.
#[test]
fn release_channel_vars_005_generating_tasks_run_under_go_task_on_the_release_dist() {
    if !task_present() {
        // ci.yml's Linux `build` job installs go-task and sets this, so the
        // one job that can run this test cannot pass it by skipping.
        assert!(
            std::env::var_os("DAD_REQUIRE_GO_TASK").is_none(),
            "DAD_REQUIRE_GO_TASK is set but go-task (`task`) is not on PATH"
        );
        eprintln!("SKIP: needs go-task (`task`) on PATH");
        return;
    }
    let root = repo_root();
    let dir = tempfile::tempdir().expect("temp dir");
    let w = dir.path();
    fs::copy(root.join("Taskfile.yml"), w.join("Taskfile.yml")).expect("copy Taskfile.yml");
    fs::create_dir_all(w.join("scripts")).expect("scripts/");
    fs::copy(
        root.join("scripts/release-channel-vars.sh"),
        w.join("scripts/release-channel-vars.sh"),
    )
    .expect("copy the validator");
    fs::create_dir_all(w.join("dist")).expect("dist/");
    // The four assets release.yml's build matrix produces, and no Windows one.
    let assets = [
        ("darwin-arm64", "1"),
        ("darwin-amd64", "2"),
        ("linux-arm64", "3"),
        ("linux-amd64", "4"),
    ];
    let checksums: String = assets
        .iter()
        .map(|(a, c)| format!("{}  dot-agent-deck-{a}\n", c.repeat(64)))
        .collect();
    fs::write(w.join("dist/checksums.txt"), checksums).expect("checksums.txt");

    for (task, args) in [
        (
            "homebrew-formula",
            vec!["VERSION=v0.42.0-rc.1", "NAME=dot-agent-deck-beta"],
        ),
        (
            "scoop-manifest",
            vec!["VERSION=v0.42.0-rc.1", "NAME=dot-agent-deck-beta"],
        ),
    ] {
        let out = Command::new("task")
            .arg("--silent")
            .arg(task)
            .args(&args)
            .current_dir(w)
            .output()
            .expect("run task");
        assert!(
            out.status.success(),
            "`task {task}` failed on the release-shaped dist/: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let base = "https://github.com/vfarcic/dot-agent-deck/releases/download/v0.42.0-rc.1";
    let rb = fs::read_to_string(w.join("dist/dot-agent-deck-beta.rb")).expect("formula");
    for line in [
        "class DotAgentDeckBeta < Formula\n".to_string(),
        "  version \"0.42.0-rc.1\"\n".to_string(),
        "  conflicts_with \"dot-agent-deck\",\n".to_string(),
        "    because: \"both install a `dot-agent-deck` binary; only one channel can be active at a time\"\n".to_string(),
        format!("      url \"{base}/dot-agent-deck-linux-amd64\"\n"),
        format!("      sha256 \"{}\"\n", "4".repeat(64)),
        "    assert_match \"dot-agent-deck\", shell_output(\"#{bin}/dot-agent-deck --help\")\n".to_string(),
    ] {
        assert!(rb.contains(&line), "formula lacks {line:?}:\n{rb}");
    }
    let json = fs::read_to_string(w.join("dist/dot-agent-deck-beta.json")).expect("manifest");
    for line in [
        "    \"version\": \"0.42.0-rc.1\",\n".to_string(),
        format!("    \"url\": \"{base}/dot-agent-deck-windows-amd64.exe\",\n"),
        "    \"hash\": \"\"\n".to_string(),
        "releases/download/v$version/dot-agent-deck-windows-amd64.exe\"\n".to_string(),
    ] {
        assert!(json.contains(&line), "manifest lacks {line:?}:\n{json}");
    }
    serde_json::from_str::<serde_json::Value>(&json).expect("the Scoop manifest is valid JSON");
}
