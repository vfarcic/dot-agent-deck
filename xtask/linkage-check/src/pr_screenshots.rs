//! PR #2491 audit K4: what `.claude/skills/pr-create/pr-screenshots.sh` lets
//! into the Markdown it prints for a pull request's body.
//!
//! The script prints one image line per file and a `| Light | Dark |` table
//! per pair, built from the alt text the caller passes and the repository the
//! URLs name. A carriage return or another control character in the alt text,
//! or a repository override that is not `<owner>/<name>`, can end the image's
//! line early and inject a heading, a link or a table row of its own. These
//! tests drive the real script against a local bare repository standing in for
//! the remote, so the refusals are proved to happen before anything is pushed,
//! and the accepted case is proved to print what it should.
//!
//! The script is a git caller (it builds its commit with plumbing and pushes
//! it), so the tests run git, isolated the way `sandbox_run.rs` does it: the
//! eight discovery variables cleared, `GIT_CEILING_DIRECTORIES` bounding the
//! upward walk at the fixture root's parent, no global or system config, and
//! `GIT_ALLOW_PROTOCOL=file`, so a refusal that regressed could not reach
//! GitHub. Unix-only; where `bash` or `git` cannot be spawned the tests print
//! `SKIP:` and return.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DISCOVERY: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

/// The eight bytes every PNG starts with, then a few more: what the script's
/// content check reads.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .join(".claude/skills/pr-create/pr-screenshots.sh")
}

fn tools_present() -> bool {
    let present = |tool: &str| {
        Command::new(tool)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if present("bash") && present("git") {
        return true;
    }
    eprintln!("SKIP: bash or git cannot be spawned; the pr-screenshots.sh tests need both");
    false
}

struct Sandbox {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Sandbox {
    /// A working repository to run the script in, a bare one standing in for
    /// the remote, and a light and a dark image.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize tempdir");
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("work")).unwrap();
        let sandbox = Sandbox { _dir: dir, root };
        sandbox.git(&sandbox.root.join("work"), &["init", "-q"]);
        sandbox.git(&sandbox.root, &["init", "-q", "--bare", "remote.git"]);
        for name in ["scene-desktop-light.png", "scene-desktop-dark.png"] {
            fs::write(sandbox.root.join("work").join(name), PNG).unwrap();
        }
        sandbox
    }

    fn isolate(&self, cmd: &mut Command) {
        for var in DISCOVERY {
            cmd.env_remove(var);
        }
        cmd.env("GIT_CEILING_DIRECTORIES", self.root.parent().unwrap())
            .env("GIT_CONFIG_GLOBAL", self.root.join("no-such-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("HOME", self.root.join("home"))
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("PR_SCREENSHOTS_REPO")
            .env_remove("PR_SCREENSHOTS_REMOTE");
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        cmd.current_dir(cwd).args(args);
        self.isolate(&mut cmd);
        let out = cmd.output().expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The script, with `remote` as `PR_SCREENSHOTS_REMOTE` and `repo` (when
    /// given) as `PR_SCREENSHOTS_REPO`.
    fn run(&self, remote: &str, repo: Option<&str>, args: &[&str]) -> Output {
        let mut cmd = Command::new("bash");
        cmd.current_dir(self.root.join("work"))
            .arg(script())
            .args(args);
        self.isolate(&mut cmd);
        cmd.env("PR_SCREENSHOTS_REMOTE", remote);
        if let Some(repo) = repo {
            cmd.env("PR_SCREENSHOTS_REPO", repo);
        }
        cmd.output().expect("spawn bash")
    }

    fn bare(&self) -> String {
        self.root.join("remote.git").display().to_string()
    }

    /// Whether anything reached the stand-in remote's `pr-screenshots` branch.
    fn pushed(&self) -> bool {
        let mut cmd = Command::new("git");
        cmd.current_dir(self.root.join("remote.git")).args([
            "rev-parse",
            "--verify",
            "-q",
            "refs/heads/pr-screenshots",
        ]);
        self.isolate(&mut cmd);
        cmd.output().expect("spawn git").status.success()
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Scenario: pass the light and dark images with alt text carrying a carriage
/// return, a tab, an escape, a DEL or a line feed. Each run is refused before
/// anything is pushed, naming the control character.
#[test]
fn pr_screenshots_001_alt_text_with_a_control_character_is_refused_before_the_push() {
    if !tools_present() {
        return;
    }
    let sandbox = Sandbox::new();
    for alt in [
        "Normal\r\r# INJECTED-HEADING\r\r<https://metadata-controlled.invalid/>\r\r",
        "Normal\textra",
        "Normal\x1b[31mred",
        "Normal\x7fdel",
        "Normal\nextra line",
    ] {
        let out = sandbox.run(
            &sandbox.bare(),
            Some("owner/repo"),
            &["2491", &format!("scene-desktop-light.png={alt}")],
        );
        assert_eq!(
            out.status.code(),
            Some(2),
            "{alt:?} was accepted: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("control character"),
            "{alt:?}: the refusal names it: {}",
            stderr(&out)
        );
        assert!(!sandbox.pushed(), "{alt:?}: nothing reaches the remote");
    }
}

/// Scenario: set `PR_SCREENSHOTS_REPO` to values that are not one GitHub
/// `<owner>/<name>` — a Markdown breakout, three components, a space, an owner
/// starting with a hyphen, a `..` name. Each is refused before anything is
/// pushed.
#[test]
fn pr_screenshots_002_a_repository_override_that_is_not_owner_and_name_is_refused() {
    if !tools_present() {
        return;
    }
    let sandbox = Sandbox::new();
    for repo in [
        "owner/repo)\n\n# ENV-INJECTED\n\n(",
        "owner/repo/extra",
        "own er/repo",
        "-owner/repo",
        "owner/..",
        "owner/",
        "owner_name/repo",
    ] {
        let out = sandbox.run(
            &sandbox.bare(),
            Some(repo),
            &["2491", "scene-desktop-light.png=Normal caption"],
        );
        assert_eq!(
            out.status.code(),
            Some(2),
            "{repo:?} was accepted: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("not an <owner>/<name>"),
            "{repo:?}: {}",
            stderr(&out)
        );
        assert!(!sandbox.pushed(), "{repo:?}: nothing reaches the remote");
    }
}

/// Scenario: name a GitHub remote URL whose path is not a valid
/// `<owner>/<name>` (a parenthesis in the name). The repository read from the
/// remote is held to the same rule as the override, and the run is refused
/// before any push.
#[test]
fn pr_screenshots_003_a_repository_read_from_the_remote_is_held_to_the_same_rule() {
    if !tools_present() {
        return;
    }
    let sandbox = Sandbox::new();
    let out = sandbox.run(
        "https://github.com/owner/re)po.git",
        None,
        &["2491", "scene-desktop-light.png=Normal caption"],
    );
    assert_eq!(out.status.code(), Some(2), "accepted: {}", stderr(&out));
    assert!(
        stderr(&out).contains("not an <owner>/<name>"),
        "{}",
        stderr(&out)
    );
}

/// Scenario: pass the light and dark images with plain alt text and a valid
/// override, pushing to a local bare repository. Both land on the
/// `pr-screenshots` branch, and the output is one image line each and the pair
/// as one `| Light | Dark |` table row.
#[test]
fn pr_screenshots_004_plain_alt_text_and_a_valid_repository_print_the_lines_and_the_pair() {
    if !tools_present() {
        return;
    }
    let sandbox = Sandbox::new();
    let out = sandbox.run(
        &sandbox.bare(),
        Some("my-org/my.repo_name"),
        &[
            "2491",
            "scene-desktop-light.png=The Card panel, light",
            "scene-desktop-dark.png=The Card panel, dark",
        ],
    );
    assert!(out.status.success(), "refused: {}", stderr(&out));
    assert!(sandbox.pushed());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let commit = sandbox
        .git(
            &sandbox.root.join("remote.git"),
            &["rev-parse", "refs/heads/pr-screenshots"],
        )
        .trim()
        .to_string();
    let light = format!(
        "![The Card panel, light](https://raw.githubusercontent.com/my-org/my.repo_name/{commit}/2491/scene-desktop-light.png)"
    );
    let dark = format!(
        "![The Card panel, dark](https://raw.githubusercontent.com/my-org/my.repo_name/{commit}/2491/scene-desktop-dark.png)"
    );
    assert_eq!(
        stdout,
        format!("{light}\n{dark}\n\n| Light | Dark |\n| --- | --- |\n| {light} | {dark} |\n")
    );
}
