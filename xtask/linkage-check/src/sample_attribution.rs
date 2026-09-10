//! Issue #906: the worktree-attribution rule inside `scripts/sample-attribution.sh`.
//!
//! The script samples toolchain RSS across concurrent worktrees and folds each
//! process into the worktree it is building. That fold is a bare string-prefix
//! test in awk, and it shipped wrong twice in one review round: `--root
//! /home/u/code` also matched `/home/u/code2/...`, and a `--root` with the
//! trailing slash tab-completion supplies ate the first character of the
//! worktree name. Both produce a *number*, not an error — which the doc this
//! script backs calls out as the worse failure, so the rule is worth a runtime
//! assertion rather than a reading.
//!
//! The awk program is extracted from the real script rather than copied here, so
//! this cannot drift into testing a stale duplicate. `/proc` is not faked: the
//! `ps`/`readlink` half that produces `<cwd>\t<rss>` lines is not what broke and
//! is not what this guards.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The attribution program as committed, lifted out of the pipeline it sits in.
///
/// Fails loudly rather than silently passing if the block moves: a test that
/// quietly stops covering anything is the thing this file exists to prevent.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

fn attribution_awk() -> String {
    let script = std::fs::read_to_string(repo_root().join("scripts/sample-attribution.sh"))
        .expect("scripts/sample-attribution.sh is readable");
    let start = script
        .find("| awk -v root=\"$root\" '")
        .expect("the attribution pipeline stage is still spelled `| awk -v root=\"$root\" '`");
    let body = &script[start..];
    let open = body.find('\'').expect("awk program opens with a quote");
    let close = body[open + 1..]
        .find('\'')
        .expect("awk program closes with a quote");
    body[open + 1..open + 1 + close].to_string()
}

fn tool_present(bin: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run the real awk program over synthetic `<cwd>\t<rss>` lines.
/// `None` means this host has no awk, not that the rule is satisfied.
fn attribute(root: &str, cwds: &[&str]) -> Option<Vec<(String, u64)>> {
    if !tool_present("awk") {
        eprintln!("SKIP: sample-attribution test needs `awk` on PATH");
        return None;
    }
    let input: String = cwds.iter().map(|c| format!("{c}\t100\n")).collect();
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "printf '%s' \"$INPUT\" | awk -v root=\"$ROOT\" '{}'",
            attribution_awk().replace('\'', "'\\''")
        ))
        .env("INPUT", &input)
        .env("ROOT", root)
        .output()
        .expect("run the attribution awk");
    assert!(
        out.status.success(),
        "awk failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut rows: Vec<(String, u64)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut f = l.split('\t');
            let name = f.next().unwrap_or_default().to_string();
            let procs: u64 = f.next().unwrap_or("0").parse().unwrap_or(0);
            (name, procs)
        })
        .collect();
    rows.sort();
    Some(rows)
}

/// A sibling directory that merely shares the root's spelling is a DIFFERENT
/// build, and folding it in overstates this run's footprint. The prefix test
/// must not straddle a path component.
#[test]
fn a_sibling_sharing_the_root_prefix_is_not_attributed_to_this_run() {
    let Some(rows) = attribute(
        "/home/u/code",
        &[
            "/home/u/code/deck-a/src",
            "/home/u/code2/deck-b/src",
            "/home/u/codex/x",
        ],
    ) else {
        return;
    };
    assert_eq!(
        rows,
        vec![("deck-a".to_string(), 1), ("other".to_string(), 2)],
        "code2/ and codex/ must land in `other`, not in this run's worktrees"
    );
}

/// `--root ~/code/` is what shell tab-completion produces. The separator must be
/// appended once however the caller spelled it, or the worktree name is reported
/// one character short — silently, under a name that matches no real worktree.
#[test]
fn a_trailing_slash_on_root_does_not_eat_the_worktree_name() {
    for root in ["/home/u/code", "/home/u/code/", "/home/u/code//"] {
        let Some(rows) = attribute(root, &["/home/u/code/deck-a/src"]) else {
            return;
        };
        assert_eq!(
            rows,
            vec![("deck-a".to_string(), 1)],
            "root {root:?} must still name the worktree `deck-a`"
        );
    }
}

/// A process sitting in the root itself names no worktree, and neither does a
/// stray double slash. Both must be `other` rather than an empty-string key that
/// silently accumulates unrelated processes together.
#[test]
fn paths_that_name_no_worktree_fall_into_other() {
    let Some(rows) = attribute("/home/u/code", &["/home/u/code", "/home/u/code//x"]) else {
        return;
    };
    assert_eq!(rows, vec![("other".to_string(), 2)]);
}
