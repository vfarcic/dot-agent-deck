//! PRD #1666: the `/code-cleanup` instructions mode owns the `dot-ai-*` skills.
//!
//! Syncing from `dot-ai` stopped and CLAUDE.md rule 13 made those skills
//! project-owned, so they are instructions like any other skill. Which files a
//! mode owns is decided at runtime by `.claude/skills/code-cleanup/pick-area.sh`,
//! both for the draw and for `--check`, so it is guarded by running the script
//! rather than by reading the skill's prose.
//!
//! The script runs against offline `git` and `gh` stand-ins, so no test touches
//! the network or a real repository. Unix-only: the script needs `bash`, and
//! also `jq`; where either cannot be spawned the tests print `SKIP:` and return.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn pick_area() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .join(".claude/skills/code-cleanup/pick-area.sh")
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
    eprintln!("SKIP: bash or jq cannot be spawned; the pick-area.sh tests need both");
    false
}

/// An offline `git` covering what `pick-area.sh` asks of it: the tracked files
/// are `STUB_TRACKED`, the changed files `STUB_CHANGED`, and there are no
/// untracked files and no other worktrees.
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

/// An offline `gh` with no open PRs and no recent cleanup PRs.
const GH_STUB: &str = r#"#!/usr/bin/env bash
case "${1:-} ${2:-}" in
  "repo view") echo owner/repo ;;
  "pr list") ;;
  *) echo "gh stand-in: unhandled invocation: $*" >&2; exit 1 ;;
esac
"#;

/// Runs `pick-area.sh instructions [extra]` in a checkout whose only tracked
/// and changed file is `file`.
fn run_instructions(file: &str, extra: &[&str]) -> Output {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    let bin = dir.path().join("bin");
    fs::create_dir_all(repo.join(Path::new(file).parent().unwrap())).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(repo.join(file), "# A skill\n\nSome instructions.\n").unwrap();
    for (name, body) in [("git", GIT_STUB), ("gh", GH_STUB)] {
        let p = bin.join(name);
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let list = dir.path().join("files");
    fs::write(&list, format!("{file}\n")).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new("bash")
        .arg(pick_area())
        .arg("instructions")
        .args(extra)
        .current_dir(&repo)
        .env("PATH", path)
        .env("STUB_TRACKED", &list)
        .env("STUB_CHANGED", &list)
        .output()
        .expect("run pick-area.sh")
}

const DOT_AI_SKILL: &str = ".claude/skills/dot-ai-changelog-fragment/SKILL.md";

#[test]
fn code_cleanup_001_the_instructions_check_accepts_a_dot_ai_skill() {
    if !tools_present() {
        return;
    }
    let out = run_instructions(DOT_AI_SKILL, &["--check"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && !stdout.contains("OUTSIDE="),
        "a change to a dot-ai-* skill must be inside the instructions mode\n\
         stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn code_cleanup_002_the_instructions_draw_can_pick_a_dot_ai_skill() {
    if !tools_present() {
        return;
    }
    let out = run_instructions(DOT_AI_SKILL, &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains(&format!("FILE={DOT_AI_SKILL}\n")),
        "a dot-ai-* skill must be in the instructions mode's owned set\n\
         stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn code_cleanup_003_a_skill_script_stays_outside_the_instructions_mode() {
    if !tools_present() {
        return;
    }
    let script = ".claude/skills/dot-ai-changelog-fragment/helper.sh";
    let out = run_instructions(script, &["--check"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success() && stdout.contains(&format!("OUTSIDE={script}\n")),
        "a script inside a skill directory is code, not an instruction\n\
         stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
