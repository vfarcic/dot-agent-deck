//! The `/code-cleanup` scripts under `.claude/skills/code-cleanup/`.
//!
//! PRD #1666: the instructions mode owns the `dot-ai-*` skills. Syncing from
//! `dot-ai` stopped and CLAUDE.md rule 13 made those skills project-owned, so
//! they are instructions like any other skill. Which files a mode owns is
//! decided at runtime by `pick-area.sh`, both for the draw and for `--check`, so
//! it is guarded by running the script rather than by reading the skill's prose.
//!
//! Issue #1819: units start from static-tool findings (`find-targets.sh`), the
//! runner can put paths off limits (`pick-area.sh --off-limits`), a mode whose
//! last cleanup merged recently is skipped (`due-modes.sh`), and the tools are
//! pinned where Renovate reads them. Each of those is a runtime decision in a
//! shell script, which no compile step sees.
//!
//! The scripts run against offline `git`, `gh`, `cargo` and `pnpm` stand-ins, so
//! no test touches the network, a real repository or a real tool. Unix-only: the
//! scripts need `bash`, and also `jq`; where either cannot be spawned the tests
//! print `SKIP:` and return.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use regex::Regex;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

fn script(name: &str) -> PathBuf {
    repo_root().join(".claude/skills/code-cleanup").join(name)
}

fn tools_present() -> bool {
    let ok = |tool: &str| {
        Command::new(tool)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if ok("bash") && ok("jq") {
        return true;
    }
    eprintln!("SKIP: bash or jq cannot be spawned; the code-cleanup script tests need both");
    false
}

/// An offline `git` covering what the scripts ask of it: the tracked files are
/// `STUB_TRACKED`, the changed files `STUB_CHANGED`, and there are no untracked
/// files and no other worktrees.
const GIT_STUB: &str = r#"#!/usr/bin/env bash
case "$*" in
  "rev-parse --show-toplevel") pwd ;;
  "merge-base origin/main HEAD") echo base ;;
  "diff --no-renames --name-only base") cat "$STUB_CHANGED" ;;
  "ls-files --others --exclude-standard") ;;
  "ls-files -- "*) cat "$STUB_TRACKED" ;;
  "worktree list --porcelain") ;;
  *) echo "git stand-in: unhandled invocation: $*" >&2; exit 1 ;;
esac
"#;

/// An offline `gh`. Open PRs are the numbers in `STUB_OPEN_PRS` (none when it
/// is unset), every PR's files are `STUB_PR_FILES`, merged cleanup PRs are the
/// JSON list in `STUB_MERGED` (empty when unset), and there are no recent
/// cleanup PRs. It prints what the scripts' `--jq` would have extracted.
const GH_STUB: &str = r#"#!/usr/bin/env bash
case "$*" in
  "repo view "*) echo owner/repo ;;
  "pr list "*"--state open"*) if [ -n "${STUB_OPEN_PRS:-}" ]; then cat "$STUB_OPEN_PRS"; fi ;;
  "pr list "*"--state merged"*) if [ -n "${STUB_MERGED:-}" ]; then cat "$STUB_MERGED"; else echo '[]'; fi ;;
  "pr list "*) ;;
  "api repos/owner/repo/pulls/"*) cat "$STUB_PR_FILES" ;;
  *) echo "gh stand-in: unhandled invocation: $*" >&2; exit 1 ;;
esac
"#;

/// A throwaway checkout: `tracked` files exist on disk with some content and
/// are what `git ls-files` lists, `changed` is what `git diff` lists, and `bin`
/// holds the stand-ins, ahead of the real tools on `PATH`.
struct Sandbox {
    dir: tempfile::TempDir,
    env: Vec<(String, String)>,
}

impl Sandbox {
    fn new(tracked: &[&str], changed: &[&str]) -> Self {
        let mut sb = Sandbox {
            dir: tempfile::tempdir().expect("tempdir"),
            env: Vec::new(),
        };
        fs::create_dir_all(sb.repo()).unwrap();
        for file in tracked.iter().chain(changed) {
            let path = sb.repo().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "line one\nline two\nline three\n").unwrap();
        }
        sb.stub("git", GIT_STUB);
        sb.stub("gh", GH_STUB);
        sb.file("tracked", &lines(tracked));
        sb.file("changed", &lines(changed));
        sb.env("STUB_TRACKED", "tracked");
        sb.env("STUB_CHANGED", "changed");
        sb.file("pr-files", "");
        sb.env("STUB_PR_FILES", "pr-files");
        sb
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn stub(&self, name: &str, body: &str) {
        let bin = self.dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let p = bin.join(name);
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Writes `body` to a file beside the checkout and returns its path.
    fn file(&self, name: &str, body: &str) -> PathBuf {
        let p = self.dir.path().join(name);
        fs::write(&p, body).unwrap();
        p
    }

    /// Points `var` at the file `name` beside the checkout.
    fn env(&mut self, var: &str, name: &str) {
        let path = self.dir.path().join(name).display().to_string();
        self.set(var, &path);
    }

    fn set(&mut self, var: &str, value: &str) {
        self.env.push((var.to_string(), value.to_string()));
    }

    fn run(&self, name: &str, args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.dir.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new("bash");
        cmd.arg(script(name))
            .args(args)
            .current_dir(self.repo())
            .env("PATH", path);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.output().expect("run the script")
    }
}

fn lines(items: &[&str]) -> String {
    items.iter().map(|i| format!("{i}\n")).collect()
}

fn report(out: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

const DOT_AI_SKILL: &str = ".claude/skills/dot-ai-changelog-fragment/SKILL.md";

#[test]
fn code_cleanup_001_the_instructions_check_accepts_a_dot_ai_skill() {
    if !tools_present() {
        return;
    }
    let out = Sandbox::new(&[DOT_AI_SKILL], &[DOT_AI_SKILL])
        .run("pick-area.sh", &["instructions", "--check"]);
    assert!(
        out.status.success() && !stdout(&out).contains("OUTSIDE="),
        "a change to a dot-ai-* skill must be inside the instructions mode\n{}",
        report(&out)
    );
}

#[test]
fn code_cleanup_002_the_instructions_draw_can_pick_a_dot_ai_skill() {
    if !tools_present() {
        return;
    }
    let out = Sandbox::new(&[DOT_AI_SKILL], &[DOT_AI_SKILL]).run("pick-area.sh", &["instructions"]);
    assert!(
        out.status.success() && stdout(&out).contains(&format!("FILE={DOT_AI_SKILL}\n")),
        "a dot-ai-* skill must be in the instructions mode's owned set\n{}",
        report(&out)
    );
}

#[test]
fn code_cleanup_003_a_skill_script_stays_outside_the_instructions_mode() {
    if !tools_present() {
        return;
    }
    let script = ".claude/skills/dot-ai-changelog-fragment/helper.sh";
    let out = Sandbox::new(&[script], &[script]).run("pick-area.sh", &["instructions", "--check"]);
    assert!(
        !out.status.success() && stdout(&out).contains(&format!("OUTSIDE={script}\n")),
        "a script inside a skill directory is code, not an instruction\n{}",
        report(&out)
    );
}

/// The manifests a cargo-shear or knip finding is fixed in, and where a false
/// positive is recorded.
const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "xtask/site/Cargo.toml",
    "desktop/src-tauri/Cargo.toml",
    "desktop/package.json",
    "desktop/pnpm-lock.yaml",
    "desktop/knip.jsonc",
];

#[test]
fn code_cleanup_004_the_code_mode_owns_the_manifests_a_dependency_finding_changes() {
    if !tools_present() {
        return;
    }
    let out = Sandbox::new(&[], MANIFESTS).run("pick-area.sh", &["code", "--check"]);
    assert!(
        out.status.success() && !stdout(&out).contains("OUTSIDE="),
        "the code mode must be able to fix a dependency finding and record a false positive\n{}",
        report(&out)
    );
    let out = Sandbox::new(&[], &["Cargo.toml"]).run("pick-area.sh", &["tests", "--check"]);
    assert!(
        !out.status.success() && stdout(&out).contains("OUTSIDE=Cargo.toml\n"),
        "the manifests stay outside the tests mode, so the modes stay disjoint\n{}",
        report(&out)
    );
}

#[test]
fn code_cleanup_005_an_off_limits_change_fails_the_check() {
    if !tools_present() {
        return;
    }
    let changed = [
        "src/inbox_cli.rs",
        "src/platform/proc/linux.rs",
        "src/ok.rs",
    ];
    let sb = Sandbox::new(&[], &changed);
    let off = sb.file(
        "off-limits",
        "# reserved by another effort\nsrc/inbox*.rs\n  src/platform/proc/  \n\n",
    );
    let out = sb.run(
        "pick-area.sh",
        &["code", "--check", "--off-limits", off.to_str().unwrap()],
    );
    let text = stdout(&out);
    assert!(
        !out.status.success()
            && text.contains("OFF_LIMITS=src/inbox_cli.rs\n")
            && text.contains("OFF_LIMITS=src/platform/proc/linux.rs\n")
            && !text.contains("OFF_LIMITS=src/ok.rs")
            && !text.contains("OUTSIDE="),
        "a glob and a directory must both put their files off limits, and nothing else\n{}",
        report(&out)
    );
    // Without --off-limits the check is what it was before issue #1819.
    let out = sb.run("pick-area.sh", &["code", "--check"]);
    assert!(
        out.status.success() && !stdout(&out).contains("OFF_LIMITS="),
        "--check without --off-limits must not change\n{}",
        report(&out)
    );
}

#[test]
fn code_cleanup_006_the_draw_never_picks_an_off_limits_file() {
    if !tools_present() {
        return;
    }
    let tracked = ["src/a.rs", "src/platform/proc/b.rs", "desktop/src/App.tsx"];
    let sb = Sandbox::new(&tracked, &[]);
    let off = sb.file("off-limits", "src/platform/proc/\ndesktop/src/App.tsx\n");
    let out = sb.run(
        "pick-area.sh",
        &["code", "--off-limits", off.to_str().unwrap()],
    );
    let text = stdout(&out);
    assert!(
        out.status.success()
            && text.contains("SOURCE=draw\n")
            && text.contains("SKIPPED_OFF_LIMITS=2\n")
            && text.contains("FILE=src/a.rs\n"),
        "the only file not off limits must be the one drawn\n{}",
        report(&out)
    );
    let off = sb.file("all-off", "*\n");
    let out = sb.run(
        "pick-area.sh",
        &["code", "--off-limits", off.to_str().unwrap()],
    );
    assert!(
        !out.status.success() && stdout(&out).starts_with("ERROR="),
        "a draw with every file off limits must stop, not pick one anyway\n{}",
        report(&out)
    );
}

#[test]
fn code_cleanup_007_a_finding_the_mode_owns_is_handed_out_before_any_draw() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new(&["src/a.rs"], &[]);
    let findings = sb.file(
        "findings",
        "TOOL=cargo-shear 1.14.0\n\
         TARGET=cargo-shear\tCargo.toml\t128\tunused dependency `tokio-test`; remove this dependency\n\
         TARGET=knip\tdesktop/e2e/support/overview.ts\t214\texports: deckSections\n\
         FINDINGS=2\n",
    );
    let out = sb.run(
        "pick-area.sh",
        &["code", "--findings", findings.to_str().unwrap()],
    );
    let text = stdout(&out);
    assert!(
        out.status.success()
            && text.contains("SOURCE=findings\n")
            && text.contains("FINDINGS=2\n")
            && text.contains("SKIPPED_NOT_OWNED=1\n")
            && text.contains("CANDIDATES=1\n")
            && text.contains("TARGET=cargo-shear\tCargo.toml\t128\t")
            && !text.contains("TARGET=knip")
            && text.contains("FILE=Cargo.toml\nLINE=128\n")
            && text.contains("FINDING=cargo-shear\tunused dependency `tokio-test`"),
        "the code mode's finding must be handed out and the tests-mode file dropped\n{}",
        report(&out)
    );
}

#[test]
fn code_cleanup_008_findings_that_are_busy_or_off_limits_fall_back_to_the_draw() {
    if !tools_present() {
        return;
    }
    let mut sb = Sandbox::new(&["src/a.rs"], &[]);
    sb.file("open-prs", "7\n");
    sb.env("STUB_OPEN_PRS", "open-prs");
    sb.file("pr-files", "Cargo.toml\n");
    let findings = sb.file(
        "findings",
        "TARGET=cargo-shear\tCargo.toml\t93\tmisplaced dependency `futures-util`\n\
         TARGET=knip\tdesktop/src/App.tsx\t40\texports: PULL_REQUEST_AGENT_GONE\n",
    );
    let off = sb.file("off-limits", "desktop/src/App.tsx\n");
    let out = sb.run(
        "pick-area.sh",
        &[
            "code",
            "--findings",
            findings.to_str().unwrap(),
            "--off-limits",
            off.to_str().unwrap(),
        ],
    );
    let text = stdout(&out);
    assert!(
        out.status.success()
            && text.contains("FINDINGS=2\nFINDINGS_KEPT=0\n")
            && text.contains("SOURCE=draw\n")
            && text.contains("FILE=src/a.rs\n")
            && !text.contains("TARGET="),
        "a finding in a busy or off-limits file must not be handed out\n{}",
        report(&out)
    );
}

/// `NAME_VERSION=X.Y.Z` pins in `find-targets.sh`, as (variable, version).
fn script_pins(text: &str) -> Vec<(String, String)> {
    let pin = Regex::new(r"(?m)^([A-Z_]+_VERSION)=(\S+)$").expect("static regex");
    pin.captures_iter(text)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect()
}

#[test]
fn code_cleanup_009_renovate_reads_every_tool_pin() {
    let path = ".claude/skills/code-cleanup/find-targets.sh";
    let text = fs::read_to_string(repo_root().join(path)).expect("read find-targets.sh");
    let pins = script_pins(&text);
    assert_eq!(
        pins.iter().map(|(v, _)| v.as_str()).collect::<Vec<_>>(),
        [
            "CARGO_SHEAR_VERSION",
            "CARGO_MUTANTS_VERSION",
            "KNIP_VERSION"
        ],
        "find-targets.sh must pin each tool it runs, once"
    );

    let config: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo_root().join("renovate.json")).expect("read renovate.json"),
    )
    .expect("renovate.json is not valid JSON");
    let managers: Vec<&serde_json::Value> = config["customManagers"]
        .as_array()
        .expect("renovate.json has a customManagers array")
        .iter()
        .filter(|m| {
            m["managerFilePatterns"].as_array().is_some_and(|ps| {
                ps.iter().filter_map(|p| p.as_str()).any(|p| {
                    let p = p.trim_start_matches('/').trim_end_matches('/');
                    Regex::new(p).is_ok_and(|r| r.is_match(path))
                })
            })
        })
        .collect();
    assert_eq!(
        managers.len(),
        1,
        "exactly one renovate.json customManager must read {path}"
    );
    let mut found = Vec::new();
    for m in managers[0]["matchStrings"]
        .as_array()
        .expect("matchStrings")
    {
        let re = Regex::new(m.as_str().expect("a string matchString")).expect("matchString");
        for c in re.captures_iter(&text) {
            found.push((
                c["datasource"].to_string(),
                c["depName"].to_string(),
                c["currentValue"].to_string(),
            ));
        }
    }
    let want: Vec<(String, String, String)> = [
        ("crate", "cargo-shear"),
        ("crate", "cargo-mutants"),
        ("npm", "knip"),
    ]
    .iter()
    .zip(&pins)
    .map(|((ds, dep), (_, v))| (ds.to_string(), dep.to_string(), v.clone()))
    .collect();
    assert_eq!(
        found, want,
        "Renovate must extract every pin in {path}, with the datasource and package it names"
    );
}

/// The pin `find-targets.sh` holds for `var`.
fn pin(var: &str) -> String {
    let text = fs::read_to_string(script("find-targets.sh")).expect("read find-targets.sh");
    script_pins(&text)
        .into_iter()
        .find(|(v, _)| v == var)
        .map(|(_, version)| version)
        .unwrap_or_else(|| panic!("find-targets.sh pins no {var}"))
}

/// A `cargo` whose `shear` subcommand reports `$STUB_SHEAR_VERSION` and prints
/// `$STUB_SHEAR_JSON`, exiting 1 as the real one does when it finds something.
const CARGO_STUB: &str = r#"#!/usr/bin/env bash
case "$*" in
  "shear --version") [ -n "${STUB_SHEAR_VERSION:-}" ] || exit 101; echo "Version: $STUB_SHEAR_VERSION" ;;
  "shear --format json") cat "$STUB_SHEAR_JSON"; exit 1 ;;
  *) echo "cargo stand-in: unhandled invocation: $*" >&2; exit 101 ;;
esac
"#;

/// A `pnpm` whose `dlx knip` prints `$STUB_KNIP_JSON`, exiting 1 as knip does.
const PNPM_STUB: &str = r#"#!/usr/bin/env bash
case "$*" in
  "dlx knip@"*" --reporter json") cat "$STUB_KNIP_JSON"; exit 1 ;;
  *) echo "pnpm stand-in: unhandled invocation: $*" >&2; exit 1 ;;
esac
"#;

fn find_targets_sandbox(shear_version: Option<&str>) -> Sandbox {
    let mut sb = Sandbox::new(&[], &[]);
    sb.stub("cargo", CARGO_STUB);
    sb.stub("pnpm", PNPM_STUB);
    fs::create_dir_all(sb.repo().join("desktop/node_modules")).unwrap();
    // `tokio-test` starts at byte 22, on line 3.
    fs::write(
        sb.repo().join("Cargo.toml"),
        "[package]\nname = \"x\"\ntokio-test = \"0.4\"\n",
    )
    .unwrap();
    sb.file(
        "shear.json",
        r#"{"summary":{"errors":1},"findings":[{"code":"shear/unused_dependency","message":"unused dependency `tokio-test`","file":"Cargo.toml","location":{"offset":22,"length":10},"help":"remove this dependency"}]}"#,
    );
    sb.env("STUB_SHEAR_JSON", "shear.json");
    sb.file(
        "knip.json",
        r#"{"issues":[{"file":"src/App.tsx","files":[],"exports":[{"name":"PULL_REQUEST_AGENT_GONE","line":40,"col":14}],"types":[{"name":"Row","line":7}],"dependencies":[]},{"file":"src/old.ts","files":["src/old.ts"],"exports":[],"types":[]}]}"#,
    );
    sb.env("STUB_KNIP_JSON", "knip.json");
    if let Some(v) = shear_version {
        sb.set("STUB_SHEAR_VERSION", v);
    }
    sb
}

#[test]
fn code_cleanup_010_find_targets_prints_each_finding_with_its_line() {
    if !tools_present() {
        return;
    }
    let out =
        find_targets_sandbox(Some(&pin("CARGO_SHEAR_VERSION"))).run("find-targets.sh", &["code"]);
    let text = stdout(&out);
    for want in [
        "TARGET=cargo-shear\tCargo.toml\t3\tunused dependency `tokio-test`; remove this dependency\n",
        "TARGET=knip\tdesktop/src/App.tsx\t40\texports: PULL_REQUEST_AGENT_GONE\n",
        "TARGET=knip\tdesktop/src/App.tsx\t7\ttypes: Row\n",
        "TARGET=knip\tdesktop/src/old.ts\t1\tfiles: unused file\n",
        "FINDINGS=4\n",
    ] {
        assert!(
            out.status.success() && text.contains(want),
            "find-targets.sh code must print `{}`\n{}",
            want.trim_end(),
            report(&out)
        );
    }
}

#[test]
fn code_cleanup_011_a_missing_or_unpinned_tool_fails_instead_of_falling_back() {
    if !tools_present() {
        return;
    }
    let shear = pin("CARGO_SHEAR_VERSION");
    for (version, expect) in [
        (None, "is not installed".to_string()),
        (Some("0.0.1"), format!("the pin is {shear}")),
    ] {
        let out = find_targets_sandbox(version).run("find-targets.sh", &["code"]);
        let text = stdout(&out);
        assert!(
            !out.status.success()
                && text.starts_with("ERROR=")
                && text.contains(&expect)
                && text.contains(&format!(
                    "cargo install cargo-shear --version {shear} --locked"
                ))
                && !text.contains("TARGET="),
            "cargo-shear at {version:?} must stop the run and name the pinned install\n{}",
            report(&out)
        );
    }
}

#[test]
fn code_cleanup_012_a_mode_whose_last_cleanup_merged_within_the_window_is_skipped() {
    if !tools_present() {
        return;
    }
    let mut sb = Sandbox::new(&[], &[]);
    // A merge time in the future is always inside the window, and one in 2000
    // always outside it, so the test does not depend on the clock.
    sb.file(
        "merged",
        r#"[
          {"number": 11, "headRefName": "agent/dispatch-cleanup-code-1022", "mergedAt": "2999-01-01T00:00:00Z"},
          {"number": 10, "headRefName": "agent/dispatch-cleanup-code-1021", "mergedAt": "2000-01-01T00:00:00Z"},
          {"number": 9, "headRefName": "agent/dispatch-cleanup-tests-1004", "mergedAt": "2000-01-01T00:00:00Z"},
          {"number": 8, "headRefName": "some-hand-made-cleanup", "mergedAt": "2999-01-01T00:00:00Z"}
        ]"#,
    );
    sb.env("STUB_MERGED", "merged");
    let out = sb.run("due-modes.sh", &[]);
    assert_eq!(
        stdout(&out),
        "SKIP=code\t#11\t2999-01-01T00:00:00Z\nDUE=tests\nDUE=instructions\n",
        "{}",
        report(&out)
    );
    let out = sb.run("due-modes.sh", &["--window-hours", "1", "tests"]);
    assert_eq!(stdout(&out), "DUE=tests\n", "{}", report(&out));
}
