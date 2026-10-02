//! `.claude/skills/tag-release/cleanup.sh`, the detector behind the
//! tag-release skill's cleanup step and behind any "clean up the box" request.
//!
//! The script deletes nothing, but the skill deletes what it lists once the
//! user confirms, so which list a directory lands in IS the safety property:
//! a directory a live process is working in must never be offered, a stray's
//! unpushed commits must be named, and a worktree whose branch is still open
//! must not appear at all. Every one of those is a runtime decision in a shell
//! script, which no compile step sees — the argument CLAUDE.md rule 5 records
//! for `clean_tmp.rs` and `reap_orphans.rs`.
//!
//! So these tests build a throwaway repository with sibling worktrees, tool
//! caches and stray directories in a `tempfile::tempdir()`, park short-lived
//! processes in some of them, and run the real script against it. `gh` is a
//! stub on `PATH` serving fixed PR lists, so nothing reaches the network, and
//! no test sleeps: a parked process is a `cat` blocked on a pipe the test holds,
//! killed when its guard drops.
//!
//! This module shells out to `git`, which rule 5 asks a fast-tier xtask test to
//! justify. The reason is the one `repo_state.rs`'s `mod real_git` gives: the
//! script under test is a collector whose whole job is reading worktrees,
//! branches and remote refs, so hand-written fixtures would test nothing. Every
//! fixture command AND the script itself run with the ambient git environment
//! switched off — configuration, and the location variables that outrank a
//! working directory, with `GIT_CEILING_DIRECTORIES` bounding discovery at the
//! sandbox — so neither can read or write the checkout these tests run in.
//!
//! Tests only. The rules live in the script; this is its gate.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use tempfile::TempDir;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .join(".claude/skills/tag-release/cleanup.sh")
}

fn tool_present(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Git's location-discovery variables, cleared for every command here. A copy
/// of `repo_state.rs`'s list, which is private to a test module of its own.
const AMBIENT_LOCATION_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

const GH_STUB: &str = r#"#!/usr/bin/env bash
# Offline `gh`: answers the two `gh pr list` calls cleanup.sh makes with the
# lines their `--jq` filters would have produced.
case " $* " in
  *" --state merged "*) cat "$CLEANUP_TEST_FIXTURES/merged.tsv" ;;
  *" --state open "*) cat "$CLEANUP_TEST_FIXTURES/open.txt" ;;
esac
exit 0
"#;

/// Stands in for a deck daemon: `bash <root>/bin/dot-agent-deck daemon serve`,
/// blocked on stdin.
const FAKE_DAEMON: &str = "#!/usr/bin/env bash\nread -r _\n";

struct Sandbox {
    _dir: TempDir,
    /// Canonical, because the script and `/proc` both report resolved paths.
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize tempdir");
        for d in ["home", "empty-template", "bin", "fixtures", "work"] {
            fs::create_dir_all(root.join(d)).expect("mkdir");
        }
        let gh = root.join("bin/gh");
        fs::write(&gh, GH_STUB).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        let daemon = root.join("bin/dot-agent-deck");
        fs::write(&daemon, FAKE_DAEMON).unwrap();
        fs::set_permissions(&daemon, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("fixtures/merged.tsv"), "").unwrap();
        fs::write(root.join("fixtures/open.txt"), "").unwrap();
        Sandbox { _dir: dir, root }
    }

    fn at(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// The neutralised environment shared by fixture commands and the script.
    fn env(&self, cmd: &mut Command) {
        let path = format!(
            "{}:{}",
            self.at("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        cmd.env("PATH", path)
            .env("HOME", self.at("home"))
            .env("XDG_CONFIG_HOME", self.at("home/.config"))
            .env("GIT_CONFIG_GLOBAL", self.at("no-such-gitconfig"))
            .env("GIT_CONFIG_SYSTEM", self.at("no-such-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TEMPLATE_DIR", self.at("empty-template"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "linkage-check tests")
            .env("GIT_AUTHOR_EMAIL", "tests@example.invalid")
            .env("GIT_COMMITTER_NAME", "linkage-check tests")
            .env("GIT_COMMITTER_EMAIL", "tests@example.invalid")
            .env("CLEANUP_TEST_FIXTURES", self.at("fixtures"))
            .env_remove("GIT_DEFAULT_HASH")
            .env_remove("GIT_DEFAULT_REF_FORMAT")
            .env_remove("CLEANUP_PROC_ROOT")
            // The parent, not the root: git never excludes its own cwd, so
            // only the parent bounds a walk that starts at the root.
            .env(
                "GIT_CEILING_DIRECTORIES",
                self.root.parent().unwrap_or(&self.root),
            );
        for var in AMBIENT_LOCATION_VARS {
            cmd.env_remove(var);
        }
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(cwd);
        self.env(&mut cmd);
        let out = cmd
            .output()
            .unwrap_or_else(|e| panic!("failed to invoke `git {}`: {e}", args.join(" ")));
        assert!(
            out.status.success(),
            "fixture command `git {}` failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&out.stderr).trim(),
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Builds the layout every test reads. Returns the main checkout.
    ///
    /// `work/` holds the main checkout `repo`, on a merged branch `parked`, and
    /// everything beside it:
    ///
    /// | sibling            | what it is                                    |
    /// | ------------------ | --------------------------------------------- |
    /// | `repo-done`        | worktree, branch an ancestor of `origin/main` |
    /// | `repo-squashed`    | worktree, tip = a merged PR's head            |
    /// | `repo-open`        | worktree, merged tip but an OPEN PR           |
    /// | `repo-active`      | worktree, unpushed work, no PR                |
    /// | `repo-busy`        | worktree, merged — a process may park here    |
    /// | `repo-pr-7-base`   | detached worktree (`/verify-pr`'s base)       |
    /// | `repo-land`        | detached worktree (`/land-prs`)               |
    /// | `repo-xver-target` | plain directory (`cargo xver`'s target)       |
    /// | `repo-scratch`     | plain directory, purpose unknown              |
    /// | `dad-3-target`     | plain directory, purpose unknown              |
    /// | `repo-clone`       | standalone clone with an unpushed commit      |
    /// | `repo-tagged`      | clone, an unpushed commit only a tag reaches, |
    /// |                    | and a local-only tag on a pushed commit       |
    /// | `repo-deep`        | plain directory, a dirty checkout 7 levels in |
    /// | `repo-many`        | plain directory, 21 nested checkouts          |
    /// | `repo-unborn`      | `git init`, no commit, one staged file        |
    /// | `repo-nest`        | plain directory, an unpushed clone inside it  |
    /// | `repo-held`        | plain directory — a process may park here     |
    /// | `repo-daemon`      | plain directory — a fake daemon may park here |
    /// | `other-project`    | unrelated, must never be listed               |
    fn build(&self) -> PathBuf {
        let seed = self.at("seed");
        fs::create_dir_all(&seed).unwrap();
        self.git(&seed, &["init", "-q"]);
        self.git(&seed, &["checkout", "-q", "-b", "main"]);
        self.git(&seed, &["commit", "-q", "--allow-empty", "-m", "first"]);
        for b in ["done", "open-pr", "busy"] {
            self.git(&seed, &["branch", b]);
        }
        self.git(&seed, &["commit", "-q", "--allow-empty", "-m", "second"]);
        let remote = self.at("remote.git");
        self.git(
            &self.root,
            &["clone", "-q", "--bare", "seed", remote.to_str().unwrap()],
        );

        let work = self.at("work");
        let repo = work.join("repo");
        self.git(&work, &["clone", "-q", remote.to_str().unwrap(), "repo"]);
        for (dir, branch) in [
            ("repo-done", "done"),
            ("repo-open", "open-pr"),
            ("repo-busy", "busy"),
        ] {
            self.git(
                &repo,
                &["worktree", "add", "-q", &format!("../{dir}"), branch],
            );
        }
        for (dir, branch, subject) in [
            ("repo-squashed", "squashed", "squash me"),
            ("repo-active", "active", "work in progress"),
        ] {
            self.git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    branch,
                    &format!("../{dir}"),
                    "origin/main",
                ],
            );
            self.git(
                &work.join(dir),
                &["commit", "-q", "--allow-empty", "-m", subject],
            );
        }
        for dir in ["repo-pr-7-base", "repo-land"] {
            self.git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "--detach",
                    &format!("../{dir}"),
                    "origin/main",
                ],
            );
        }
        let squashed_tip = self.git(&work.join("repo-squashed"), &["rev-parse", "HEAD"]);
        fs::write(
            self.at("fixtures/merged.tsv"),
            format!("squashed\t{squashed_tip}\n"),
        )
        .unwrap();
        fs::write(self.at("fixtures/open.txt"), "open-pr\n").unwrap();

        self.git(
            &work,
            &["clone", "-q", remote.to_str().unwrap(), "repo-clone"],
        );
        self.git(
            &work.join("repo-clone"),
            &["commit", "-q", "--allow-empty", "-m", "local only work"],
        );

        // A clone whose only unpushed commit is reachable from a local tag.
        let tagged = work.join("repo-tagged");
        self.git(
            &work,
            &["clone", "-q", remote.to_str().unwrap(), "repo-tagged"],
        );
        self.git(&tagged, &["checkout", "-q", "--detach"]);
        self.git(
            &tagged,
            &["commit", "-q", "--allow-empty", "-m", "tagged only"],
        );
        self.git(&tagged, &["tag", "-a", "-m", "keep", "keep-me"]);
        self.git(&tagged, &["checkout", "-q", "main"]);
        // A local-only tag on a commit that IS pushed: the commit survives the
        // clone's removal, the tag does not.
        self.git(&tagged, &["tag", "pushed-tag", "origin/main"]);

        // A checkout seven levels down, beyond any shallow search.
        let deep = work.join("repo-deep/a/b/c/d/e/f/inner");
        fs::create_dir_all(&deep).unwrap();
        self.git(&deep, &["init", "-q"]);
        fs::write(deep.join("deep.txt"), "deep").unwrap();

        // More nested checkouts than the script examines.
        for i in 0..21 {
            let d = work.join(format!("repo-many/n{i:02}"));
            fs::create_dir_all(&d).unwrap();
            self.git(&d, &["init", "-q"]);
        }

        // A repository with no commit yet still holds work: a staged file.
        let unborn = work.join("repo-unborn");
        fs::create_dir_all(&unborn).unwrap();
        self.git(&unborn, &["init", "-q"]);
        fs::write(unborn.join("notes.txt"), "draft").unwrap();
        self.git(&unborn, &["add", "notes.txt"]);

        // A plain directory with a clone nested inside it, holding a commit
        // that is nowhere else.
        let nest = work.join("repo-nest/sub");
        fs::create_dir_all(&nest).unwrap();
        self.git(&nest, &["clone", "-q", remote.to_str().unwrap(), "clone"]);
        self.git(
            &nest.join("clone"),
            &["commit", "-q", "--allow-empty", "-m", "nested work"],
        );

        // The main checkout parks on a merged FEATURE branch, which is what the
        // default-branch exclusion alone let through as a merged worktree.
        self.git(&repo, &["checkout", "-q", "-b", "parked", "origin/main"]);

        for d in [
            "repo-xver-target",
            "repo-scratch",
            "dad-3-target",
            "repo-held",
            "repo-daemon",
            "other-project",
        ] {
            fs::create_dir_all(work.join(d)).unwrap();
            fs::write(work.join(d).join("file"), "x").unwrap();
        }
        repo
    }

    fn run_raw(&self, cwd: &Path, args: &[&str], proc_root: Option<&Path>) -> Output {
        let mut cmd = Command::new("bash");
        cmd.arg(script()).args(args).current_dir(cwd);
        self.env(&mut cmd);
        if let Some(p) = proc_root {
            cmd.env("CLEANUP_PROC_ROOT", p);
        }
        cmd.output().expect("run cleanup.sh")
    }

    /// `cleanup.sh --holders <dir>`: its exit code and stdout.
    fn holders(&self, cwd: &Path, dir: &Path, proc_root: Option<&Path>) -> (i32, String) {
        let out = self.run_raw(cwd, &["--holders", dir.to_str().unwrap()], proc_root);
        (
            out.status
                .code()
                .expect("cleanup.sh --holders exited by signal"),
            String::from_utf8_lossy(&out.stdout).to_string(),
        )
    }

    fn run(&self, repo: &Path, proc_root: Option<&Path>) -> Report {
        let out: Output = self.run_raw(repo, &[], proc_root);
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(
            out.status.success(),
            "cleanup.sh failed: {}\nstdout:\n{stdout}",
            String::from_utf8_lossy(&out.stderr)
        );
        Report::parse(&stdout)
    }
}

/// A process parked in a directory: a `cat` (or the fake daemon) blocked on a
/// pipe this guard owns. Dropping the guard kills and reaps it, so a failing
/// assertion leaves nothing running.
struct Parked(Child);

impl Parked {
    fn cat_in(dir: &Path) -> Parked {
        Parked::spawn(Command::new("cat"), dir)
    }

    fn daemon_in(sandbox: &Sandbox, dir: &Path) -> Parked {
        let mut cmd = Command::new(sandbox.at("bin/dot-agent-deck"));
        cmd.args(["daemon", "serve"]);
        Parked::spawn(cmd, dir)
    }

    fn spawn(mut cmd: Command, dir: &Path) -> Parked {
        let child = cmd
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn parked process");
        Parked(child)
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Parked {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Report {
    stdout: String,
    keys: BTreeMap<String, String>,
    lists: BTreeMap<String, Vec<String>>,
}

impl Report {
    fn parse(stdout: &str) -> Report {
        let mut keys = BTreeMap::new();
        let mut lists: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut current: Option<String> = None;
        for line in stdout.lines() {
            if let Some(item) = line.strip_prefix("  ") {
                let list = current.as_ref().expect("an item outside any list");
                lists.get_mut(list).unwrap().push(item.to_string());
            } else if let Some((k, v)) = line.split_once('=') {
                keys.insert(k.to_string(), v.to_string());
                current = None;
            } else if let Some(h) = line.strip_suffix(':') {
                lists.insert(h.to_string(), Vec::new());
                current = Some(h.to_string());
            }
        }
        Report {
            stdout: stdout.to_string(),
            keys,
            lists,
        }
    }

    fn key(&self, k: &str) -> &str {
        self.keys
            .get(k)
            .map(String::as_str)
            .unwrap_or_else(|| panic!("no {k}= in:\n{}", self.stdout))
    }

    fn list(&self, name: &str) -> &[String] {
        self.lists
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or_else(|| panic!("no {name}: list in:\n{}", self.stdout))
    }

    /// The entry in `list` that names `path` as its first `|` field.
    fn entry(&self, list: &str, path: &Path) -> Option<&String> {
        let p = path.to_str().unwrap();
        self.list(list)
            .iter()
            .find(|l| l.split('|').next() == Some(p))
    }

    /// Every list whose entries name `path` in their first field.
    fn lists_naming(&self, path: &Path) -> Vec<&str> {
        let p = path.to_str().unwrap();
        self.lists
            .iter()
            .filter(|(_, items)| items.iter().any(|l| l.split('|').next() == Some(p)))
            .map(|(k, _)| k.as_str())
            .collect()
    }

    fn mentions(&self, needle: &str) -> bool {
        self.stdout.contains(needle)
    }
}

/// Scenario: build a repository with merged, squash-merged, open-PR and active
/// worktrees, detached worktrees, tool caches and stray directories beside it,
/// park processes in a merged worktree, a stray directory and a deleted
/// directory, run `cleanup.sh` from a linked worktree, and check each lands in
/// exactly the list the skill acts on — and that no directory with a live
/// process in it is offered.
#[test]
fn every_leftover_lands_in_its_list_and_a_busy_directory_is_never_offered() {
    if !tool_present("bash") || !tool_present("git") {
        eprintln!("SKIP: cleanup.sh's sandbox test needs bash and git on PATH");
        return;
    }
    let sb = Sandbox::new();
    sb.build();
    let work = sb.at("work");
    let w = |d: &str| work.join(d);

    let linux = cfg!(target_os = "linux");
    let _busy = Parked::cat_in(&w("repo-busy"));
    let held = Parked::cat_in(&w("repo-held"));
    let daemon = Parked::daemon_in(&sb, &w("repo-daemon"));
    let gone_dir = w("gone");
    fs::create_dir_all(&gone_dir).unwrap();
    let orphan = Parked::cat_in(&gone_dir);
    fs::remove_dir(&gone_dir).unwrap();

    // From a linked worktree, so the main checkout is not excluded merely as
    // the worktree the script stands in.
    let r = sb.run(&w("repo-active"), None);
    assert_eq!(r.key("NOTHING_TO_CLEAN"), "false", "{}", r.stdout);
    assert_eq!(r.key("SIBLING_ROOT"), work.to_str().unwrap());

    // Merged worktrees, by ancestry and by merged-PR head SHA.
    for d in ["repo-done", "repo-squashed"] {
        assert!(
            r.entry("WORKTREES", &w(d)).is_some(),
            "{d} should be offered as a merged worktree:\n{}",
            r.stdout
        );
    }
    // An open PR's worktree, unpushed work and the main checkout appear nowhere.
    for d in ["repo-open", "repo-active", "repo", "other-project"] {
        assert!(
            r.lists_naming(&w(d)).is_empty(),
            "{d} must not be listed, but is in {:?}:\n{}",
            r.lists_naming(&w(d)),
            r.stdout
        );
    }
    let locals = r.list("LOCAL_BRANCHES");
    assert!(
        locals.iter().any(|l| l.starts_with("done ")),
        "{}",
        r.stdout
    );
    assert!(
        locals.iter().any(|l| l.starts_with("squashed ")),
        "{}",
        r.stdout
    );
    assert!(
        !locals
            .iter()
            .any(|l| l.starts_with("open-pr ") || l.starts_with("active ")),
        "an open-PR or unmerged branch was offered:\n{}",
        r.stdout
    );
    assert!(
        r.list("REMOTE_BRANCHES")
            .iter()
            .any(|l| l.starts_with("done ")),
        "{}",
        r.stdout
    );

    // Detached worktrees: `/verify-pr`'s base is its own kind, `-land` a cache.
    let base = r
        .entry("DETACHED_WORKTREES", &w("repo-pr-7-base"))
        .unwrap_or_else(|| panic!("repo-pr-7-base not detached:\n{}", r.stdout));
    assert!(base.contains("every commit is on a remote"), "{base}");
    let land = r
        .entry("TOOL_CACHES", &w("repo-land"))
        .unwrap_or_else(|| panic!("repo-land not a cache:\n{}", r.stdout));
    assert!(land.contains("|land-worktree|size="), "{land}");
    let xver = r
        .entry("TOOL_CACHES", &w("repo-xver-target"))
        .unwrap_or_else(|| panic!("repo-xver-target not a cache:\n{}", r.stdout));
    assert!(xver.contains("|xver|size="), "{xver}");

    // Strays carry size and modification time; a clone names its unpushed commit.
    for d in ["repo-scratch", "dad-3-target"] {
        let e = r
            .entry("STRAY_DIRS", &w(d))
            .unwrap_or_else(|| panic!("{d} not a stray:\n{}", r.stdout));
        assert!(e.contains("|size=") && e.contains("|modified="), "{e}");
        assert!(e.contains("git: not a checkout"), "{e}");
    }
    let clone = r
        .entry("STRAY_DIRS", &w("repo-clone"))
        .unwrap_or_else(|| panic!("repo-clone not a stray:\n{}", r.stdout));
    assert!(
        clone.contains("UNPUSHED 1 commit(s)") && clone.contains("local only work"),
        "the clone's unpushed commit must be named: {clone}"
    );
    let tagged = r
        .entry("STRAY_DIRS", &w("repo-tagged"))
        .unwrap_or_else(|| panic!("repo-tagged not a stray:\n{}", r.stdout));
    assert!(
        tagged.contains("UNPUSHED 1 commit(s)") && tagged.contains("tagged only"),
        "a commit only a local tag reaches must be named: {tagged}"
    );
    assert!(
        tagged.contains("LOCAL TAGS not in this repository:")
            && tagged.contains("keep-me")
            && tagged.contains("pushed-tag"),
        "local-only tags must be named, even on a pushed commit: {tagged}"
    );
    let deep = r
        .entry("STRAY_DIRS", &w("repo-deep"))
        .unwrap_or_else(|| panic!("repo-deep not a stray:\n{}", r.stdout));
    assert!(
        deep.contains("nested a/b/c/d/e/f/inner: no commits; UNCOMMITTED CHANGES"),
        "a dirty checkout deep inside a stray must be named: {deep}"
    );
    let many = r
        .entry("STRAY_DIRS", &w("repo-many"))
        .unwrap_or_else(|| panic!("repo-many not a stray:\n{}", r.stdout));
    assert!(
        many.contains("COULD NOT VERIFY nested checkouts past the first 20 examined"),
        "a scan that stopped early must say so rather than vouch: {many}"
    );
    let unborn = r
        .entry("STRAY_DIRS", &w("repo-unborn"))
        .unwrap_or_else(|| panic!("repo-unborn not a stray:\n{}", r.stdout));
    assert!(
        unborn.contains("git: no commits; UNCOMMITTED CHANGES"),
        "a repository with no commit but a staged file must say so: {unborn}"
    );
    let nest = r
        .entry("STRAY_DIRS", &w("repo-nest"))
        .unwrap_or_else(|| panic!("repo-nest not a stray:\n{}", r.stdout));
    assert!(
        nest.contains("nested sub/clone:")
            && nest.contains("UNPUSHED 1 commit(s)")
            && nest.contains("nested work"),
        "a clone nested inside a stray must have its unpushed commit named: {nest}"
    );

    if linux {
        assert_eq!(r.key("PROC_CHECK"), "ok");
        // A directory with a live process in it is held, never offered.
        for (d, list) in [
            ("repo-busy", "WORKTREES"),
            ("repo-held", "STRAY_DIRS"),
            ("repo-daemon", "STRAY_DIRS"),
        ] {
            assert!(
                r.entry(list, &w(d)).is_none(),
                "{d} has a live process in it and must not be offered in {list}:\n{}",
                r.stdout
            );
            assert!(
                r.entry("HELD_DIRS", &w(d)).is_some(),
                "{d} should be HELD:\n{}",
                r.stdout
            );
        }
        let pids: Vec<&str> = r
            .list("PROCESSES")
            .iter()
            .filter_map(|l| l.split('|').next())
            .collect();
        assert!(
            pids.contains(&held.pid().to_string().as_str()),
            "{}",
            r.stdout
        );
        assert!(
            pids.contains(&orphan.pid().to_string().as_str()),
            "a process whose cwd was deleted should be listed:\n{}",
            r.stdout
        );
        assert!(
            !pids.contains(&daemon.pid().to_string().as_str()),
            "a dot-agent-deck daemon must never be offered for a kill:\n{}",
            r.stdout
        );
        assert!(r.mentions("never offered for a kill"), "{}", r.stdout);
        // Each process carries its start time, which the kill step compares to
        // tell it from a later process given the same pid.
        let entry = r
            .list("PROCESSES")
            .iter()
            .find(|l| l.starts_with(&format!("{}|", held.pid())))
            .unwrap();
        let start = entry.split('|').nth(1).unwrap();
        assert!(
            start
                .strip_prefix("start=")
                .is_some_and(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit())),
            "expected `start=<ticks>` as the second field: {entry}"
        );

        // `--holders`, the pre-removal re-check: 1 and the holder's pid for a
        // busy directory, 0 for an idle one.
        let (code, out) = sb.holders(&w("repo-active"), &w("repo-held"), None);
        assert_eq!(code, 1, "{out}");
        assert!(out.contains(&format!("  {}|start=", held.pid())), "{out}");
        let (code, out) = sb.holders(&w("repo-active"), &w("repo-scratch"), None);
        assert_eq!(code, 0, "{out}");
        // A process that arrives after the snapshot is still caught.
        let late = Parked::cat_in(&w("repo-scratch"));
        let (code, out) = sb.holders(&w("repo-active"), &w("repo-scratch"), None);
        assert_eq!(code, 1, "{out}");
        assert!(out.contains(&format!("  {}|", late.pid())), "{out}");
    } else {
        assert_eq!(r.key("PROC_CHECK"), "unavailable");
        let e = r.entry("STRAY_DIRS", &w("repo-held")).unwrap();
        assert!(e.contains("processes: could not check"), "{e}");
    }
}

/// Scenario: point the process scan at a directory that is not a `/proc`, run
/// `cleanup.sh`, and check it reports the check as unavailable and labels each
/// directory "could not check" instead of claiming nothing runs there.
#[test]
fn without_proc_every_directory_says_the_process_check_could_not_run() {
    if !tool_present("bash") || !tool_present("git") {
        eprintln!("SKIP: cleanup.sh's sandbox test needs bash and git on PATH");
        return;
    }
    let sb = Sandbox::new();
    let repo = sb.build();
    let work = sb.at("work");
    let r = sb.run(&repo, Some(&sb.at("no-proc-here")));

    assert_eq!(r.key("PROC_CHECK"), "unavailable", "{}", r.stdout);
    for (list, d) in [
        ("STRAY_DIRS", "repo-held"),
        ("STRAY_DIRS", "repo-scratch"),
        ("TOOL_CACHES", "repo-xver-target"),
        ("DETACHED_WORKTREES", "repo-pr-7-base"),
    ] {
        let e = r
            .entry(list, &work.join(d))
            .unwrap_or_else(|| panic!("{d} missing from {list}:\n{}", r.stdout));
        assert!(e.contains("processes: could not check"), "{e}");
        assert!(!e.contains("processes: none"), "{e}");
    }
    assert!(r.list("HELD_DIRS").is_empty(), "{}", r.stdout);
    assert!(r.list("PROCESSES").is_empty(), "{}", r.stdout);

    // The pre-removal re-check refuses to vouch for a directory it could not
    // check: exit 2, not the 0 that means "nothing is there".
    let (code, out) = sb.holders(
        &repo,
        &work.join("repo-scratch"),
        Some(&sb.at("no-proc-here")),
    );
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("PROC_CHECK=unavailable"), "{out}");
}
