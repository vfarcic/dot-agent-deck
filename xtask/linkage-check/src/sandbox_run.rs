//! PR #1672: `scripts/sandbox-run.sh`, the script behind `task run`,
//! `task run-desktop`, `task run-all` and `task run-stop`
//! (`docs/develop/local-run.md`). Two defects a maintainer hit on macOS:
//!
//! - **One sandbox per checkout.** Every branch built from a checkout used
//!   `DIR/.dad-sandbox`, so `task run-all` of one branch met the sandbox daemon
//!   another branch had left running — with an agent under it — and offered to
//!   restart it. Each branch now gets `DIR/.dad-sandbox/branch-<slug>` (a
//!   detached HEAD `commit-<sha>`), bounded so its socket paths fit in a
//!   `sun_path`, and `run-stop` resolves the same one.
//! - **`run-all` suspended at the TUI's prompt** (`zsh: suspended (tty
//!   input)`). The desktop ran in the TUI's terminal session, so any process in
//!   its tree that made itself the terminal's foreground would leave the TUI in
//!   a background group, and the TUI's next read would stop its whole group,
//!   `task` included, with SIGTTIN. It did not reproduce on Linux, with the
//!   real desktop or under bash 3.2.57, so which process did it on the
//!   reporter's Mac is not established. The desktop now runs in a session of
//!   its own, which rules out every process in its tree. The terminal test
//!   below stands in for one with a process that ignores SIGTTOU and calls
//!   `tcsetpgrp` — the mechanism that leaves the TUI in the background.
//!
//! Every test drives the real script against a throwaway checkout whose
//! "binary" is a stand-in that reports the socket it was handed. The script
//! names the branch with `git`, so these tests shell out to `git` — the
//! property under test is which branch a real repository is on — with the
//! ambient git environment switched off the way `repo_state`'s `Sandbox::git()`
//! does, so no fixture can read or write the checkout the tests run in. No
//! external network (the `run_all_` tests pick a free loopback port); they
//! wait on their stand-ins by polling, bounded.
//!
//! Fixtures sit under `/tmp` rather than the system temp dir: macOS's
//! `/var/folders/…/T/` is long enough by itself to leave no room for a
//! per-branch sandbox under 103 bytes, which is the bound under test.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// The workspace root, from this crate's manifest dir.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

fn tool_present(tool: &str, arg: &str) -> bool {
    Command::new(tool)
        .arg(arg)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Say so loudly rather than fail an unrelated change on a missing tool.
fn skip_without(tools: &[(&str, &str)], test: &str) -> bool {
    for (tool, arg) in tools {
        if !tool_present(tool, arg) {
            eprintln!("SKIP: {test}: `{tool}` is not available");
            return true;
        }
    }
    false
}

/// The longest Unix socket path the script allows here, as it computes it.
fn socket_path_max() -> usize {
    if cfg!(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )) {
        103
    } else {
        107
    }
}

/// `repo_state`'s list: each one overrides `git -C`, so an ambient value would
/// point the script's branch lookup (and the fixtures' commits) at another
/// repository.
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

/// A throwaway checkout (`co/`) holding a copy of the real script and
/// stand-in builds, and a config dir (`cfg/`) the TUI would start in.
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
}

/// The stand-in build: as the TUI it prints the socket it was given, and as
/// `daemon stop` the socket that stop was aimed at.
const STAND_IN_BUILD: &str = r#"#!/usr/bin/env bash
case "$*" in
  "daemon stop"*) echo "STOP-SOCKET=$DOT_AGENT_DECK_ATTACH_SOCKET" ;;
  daemon*) exit 0 ;;
  *) echo "TUI-SOCKET=$DOT_AGENT_DECK_ATTACH_SOCKET" ;;
esac
"#;

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::Builder::new()
            .prefix("sr")
            .tempdir_in("/tmp")
            .expect("tempdir under /tmp");
        let root = dir.path().canonicalize().expect("canonicalize tempdir");
        let f = Fixture { _dir: dir, root };
        let co = f.checkout();
        fs::create_dir_all(co.join("scripts")).unwrap();
        fs::copy(
            repo_root().join("scripts/sandbox-run.sh"),
            co.join("scripts/sandbox-run.sh"),
        )
        .expect("copy scripts/sandbox-run.sh");
        for profile in ["release", "debug"] {
            f.write_exec(
                &co.join("target").join(profile).join("dot-agent-deck"),
                STAND_IN_BUILD,
            );
        }
        fs::create_dir_all(f.cfg()).unwrap();
        fs::create_dir_all(f.root.join("home")).unwrap();
        f.git(&["init", "-q", "-b", "main"]);
        f.commit("first");
        f
    }

    fn checkout(&self) -> PathBuf {
        self.root.join("co")
    }

    fn cfg(&self) -> PathBuf {
        self.root.join("cfg")
    }

    fn sandbox_root(&self) -> PathBuf {
        self.cfg().join(".dad-sandbox")
    }

    fn write_exec(&self, path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The ambient git environment switched off, for the fixtures' own git
    /// commands and for the script's.
    fn isolate(&self, cmd: &mut Command) {
        for var in AMBIENT_LOCATION_VARS {
            cmd.env_remove(var);
        }
        cmd.env("GIT_CEILING_DIRECTORIES", self.root.parent().unwrap())
            .env("GIT_CONFIG_GLOBAL", self.root.join("no-such-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT");
    }

    fn git(&self, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(self.checkout()).args(args);
        self.isolate(&mut cmd);
        cmd.env("HOME", self.root.join("home"))
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid");
        let out = cmd.output().expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", combined(&out));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(&self, msg: &str) {
        self.git(&["commit", "-q", "--allow-empty", "-m", msg]);
    }

    /// `sandbox-run.sh MODE CHECKOUT DIR SANDBOX PROFILE ARGS…`, the way the
    /// Taskfile calls it.
    fn run(&self, mode: &str, dir: &Path, sandbox: &str, profile: &str, args: &[&str]) -> Output {
        self.run_with_env(mode, dir, sandbox, profile, args, &[])
    }

    /// [`Fixture::run`] with extra environment variables.
    fn run_with_env(
        &self,
        mode: &str,
        dir: &Path,
        sandbox: &str,
        profile: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Output {
        let mut cmd = Command::new("bash");
        cmd.arg(self.checkout().join("scripts/sandbox-run.sh"))
            .arg(mode)
            .arg(self.checkout())
            .arg(dir)
            .arg(sandbox)
            .arg(profile)
            .args(args)
            .envs(env.iter().copied());
        self.isolate(&mut cmd);
        cmd.output().expect("run scripts/sandbox-run.sh")
    }

    /// The attach socket `task run` hands the TUI with SANDBOX unset.
    fn tui_socket(&self) -> String {
        let out = self.run("tui", &self.cfg(), "", "release", &[]);
        assert!(out.status.success(), "task run failed: {}", combined(&out));
        field(&out, "TUI-SOCKET=")
    }

    /// The attach socket `task run-stop` aims `daemon stop` at with SANDBOX
    /// unset, or `None` when it found no sandbox to stop.
    fn stop_socket(&self) -> Option<String> {
        let out = self.run("stop", &self.cfg(), "", "", &[]);
        assert!(
            out.status.success(),
            "task run-stop failed: {}",
            combined(&out)
        );
        let text = combined(&out);
        if text.contains("nothing to stop") {
            return None;
        }
        Some(field(&out, "STOP-SOCKET="))
    }
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn field(out: &Output, prefix: &str) -> String {
    let text = combined(out);
    text.lines()
        .find_map(|l| l.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no `{prefix}` line in:\n{text}"))
        .to_string()
}

/// The sandbox directory a socket path names.
fn sandbox_of(socket: &str) -> PathBuf {
    let path = Path::new(socket);
    assert_eq!(path.file_name().unwrap(), "attach.sock", "{socket}");
    path.parent().unwrap().to_path_buf()
}

/// Scenario: run `task run`'s script with no SANDBOX on branch `main` of a
/// throwaway checkout; the TUI is handed `DIR/.dad-sandbox/branch-main/attach.sock`,
/// a sandbox of that branch's own, not the shared `DIR/.dad-sandbox`.
#[test]
fn the_default_sandbox_is_the_branchs_own() {
    if skip_without(
        &[("bash", "--version"), ("git", "--version")],
        "the_default_sandbox_is_the_branchs_own",
    ) {
        return;
    }
    let f = Fixture::new();
    let socket = f.tui_socket();
    assert_eq!(
        Path::new(&socket),
        f.sandbox_root().join("branch-main/attach.sock")
    );
}

/// Scenario: run on `main`, then on `feature/x` and on `feature-x`, which
/// read the same once `/` is replaced; each gets its own sandbox. Then commit
/// on `feature/x` and run again; it keeps the sandbox it had, because a new
/// commit of the same branch is the build-stamp case whose recovery reuses it.
#[test]
fn each_branch_gets_its_own_sandbox_and_a_new_commit_keeps_it() {
    if skip_without(
        &[("bash", "--version"), ("git", "--version")],
        "each_branch_gets_its_own_sandbox_and_a_new_commit_keeps_it",
    ) {
        return;
    }
    let f = Fixture::new();
    let on_main = f.tui_socket();
    f.git(&["checkout", "-q", "-b", "feature/x"]);
    let on_slash = f.tui_socket();
    f.git(&["checkout", "-q", "-b", "feature-x"]);
    let on_dash = f.tui_socket();
    assert_ne!(on_main, on_slash);
    assert_ne!(
        on_slash, on_dash,
        "`feature/x` and `feature-x` must not share a sandbox"
    );
    assert_eq!(
        Path::new(&on_dash),
        f.sandbox_root().join("branch-feature-x/attach.sock")
    );
    let slash_dir = sandbox_of(&on_slash);
    let slash_name = slash_dir.file_name().unwrap().to_str().unwrap();
    assert!(
        slash_name.starts_with("branch-feature-x-")
            && slash_name.len() == "branch-feature-x-".len() + 8,
        "a rewritten name carries an eight-digit hash: {slash_name}"
    );

    f.git(&["checkout", "-q", "feature/x"]);
    f.commit("second");
    assert_eq!(
        f.tui_socket(),
        on_slash,
        "a new commit of the same branch moved its sandbox"
    );
}

/// Scenario: detach HEAD and run; the sandbox is named for the commit,
/// `commit-<12 hex>`.
#[test]
fn a_detached_head_gets_a_sandbox_named_for_its_commit() {
    if skip_without(
        &[("bash", "--version"), ("git", "--version")],
        "a_detached_head_gets_a_sandbox_named_for_its_commit",
    ) {
        return;
    }
    let f = Fixture::new();
    let sha = f.git(&["rev-parse", "--short=12", "HEAD"]);
    f.git(&["checkout", "-q", "--detach"]);
    assert_eq!(
        Path::new(&f.tui_socket()),
        f.sandbox_root().join(format!("commit-{sha}/attach.sock"))
    );
}

/// Scenario: run on two branches whose 150-byte names differ only at the end;
/// each is cut so its socket path is exactly the platform's `sun_path` bound,
/// a real Unix socket binds at that path, and the two cut names still get two
/// sandboxes.
#[test]
fn a_long_branch_name_is_cut_to_fit_the_socket_path_bound() {
    if skip_without(
        &[("bash", "--version"), ("git", "--version")],
        "a_long_branch_name_is_cut_to_fit_the_socket_path_bound",
    ) {
        return;
    }
    let f = Fixture::new();
    let stem = "very-long-branch-name-".repeat(6);
    f.git(&["checkout", "-q", "-b", &format!("{stem}one")]);
    let one = f.tui_socket();
    f.git(&["checkout", "-q", "-b", &format!("{stem}two")]);
    let two = f.tui_socket();
    for socket in [&one, &two] {
        // A cut slug fills its budget exactly, so this is the longest path the
        // script hands out. Binding it is what proves `socket_path_max` (and
        // the script's copy of it) is a length the kernel takes, rather than
        // one this file and the script merely agree on.
        assert_eq!(
            socket.len(),
            socket_path_max(),
            "{socket} is not cut to the bound"
        );
        assert!(sandbox_of(socket).starts_with(f.sandbox_root()));
        let listener = std::os::unix::net::UnixListener::bind(socket);
        assert!(
            listener.is_ok(),
            "a socket at the {}-byte path {socket} does not bind: {:?}",
            socket.len(),
            listener.err()
        );
    }
    assert_ne!(one, two);
}

/// Scenario: point DIR at a directory so deep no per-branch sandbox under it
/// could hold a socket, and run; it refuses before creating anything, naming
/// the bound and SANDBOX= as the way out. An explicit SANDBOX that is too long
/// is refused the same way.
#[test]
fn a_socket_path_past_the_bound_is_refused_with_the_reason() {
    if skip_without(
        &[("bash", "--version"), ("git", "--version")],
        "a_socket_path_past_the_bound_is_refused_with_the_reason",
    ) {
        return;
    }
    let f = Fixture::new();
    let deep = f.root.join("d".repeat(socket_path_max()));
    fs::create_dir_all(&deep).unwrap();
    let out = f.run("tui", &deep, "", "release", &[]);
    let text = combined(&out);
    assert!(!out.status.success(), "a too-long DIR was accepted: {text}");
    assert!(text.contains("pass SANDBOX="), "{text}");
    assert!(text.contains(&socket_path_max().to_string()), "{text}");
    assert!(
        !deep.join(".dad-sandbox").exists(),
        "it created a sandbox it refused"
    );

    let explicit = deep.join("sb");
    let out = f.run("tui", &f.cfg(), explicit.to_str().unwrap(), "release", &[]);
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a too-long SANDBOX was accepted: {text}"
    );
    assert!(text.contains("attach.sock is"), "{text}");
    assert!(!explicit.exists(), "it created a SANDBOX it refused");
}

/// Scenario: run on `a/b`, whose sandbox is `branch-a-b-<hash>`, then on a
/// branch literally named `a-b-<that hash>`; the second gets a sandbox of its
/// own, because a name that already ends like a hashed slug is hashed too.
#[test]
fn a_name_shaped_like_a_hashed_slug_cannot_take_its_sandbox() {
    if skip_without(
        &[("bash", "--version"), ("git", "--version")],
        "a_name_shaped_like_a_hashed_slug_cannot_take_its_sandbox",
    ) {
        return;
    }
    let f = Fixture::new();
    f.git(&["checkout", "-q", "-b", "a/b"]);
    let rewritten = f.tui_socket();
    let rewritten_name = sandbox_of(&rewritten)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let literal = rewritten_name
        .strip_prefix("branch-")
        .expect("a branch's slug starts with `branch-`")
        .to_string();
    assert!(
        literal.starts_with("a-b-") && literal.len() == "a-b-".len() + 8,
        "`a/b` carries an eight-digit hash: {rewritten_name}"
    );
    f.git(&["checkout", "-q", "-b", &literal]);
    let lookalike = f.tui_socket();
    assert_ne!(
        lookalike, rewritten,
        "branch `{literal}` shares `a/b`'s sandbox"
    );
    let lookalike_name = sandbox_of(&lookalike)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        lookalike_name.starts_with(&format!("branch-{literal}-"))
            && lookalike_name.len() == format!("branch-{literal}-").len() + 8,
        "a name ending like a hashed slug carries a hash of its own: {lookalike_name}"
    );
}

/// Scenario: point DIR at a directory deep enough to leave 12 bytes of slug,
/// room for `branch-main` (11) but not for a detached HEAD's `commit-<sha>`
/// (19) nor any hashed slug (17 at the least); `main` runs there, and a
/// detached HEAD is refused naming the bound and SANDBOX= as the way out.
#[test]
fn a_short_branch_runs_where_only_its_own_slug_fits() {
    if skip_without(
        &[("bash", "--version"), ("git", "--version")],
        "a_short_branch_runs_where_only_its_own_slug_fits",
    ) {
        return;
    }
    let f = Fixture::new();
    // The script's budget is max - len(DIR/.dad-sandbox) - 13, and
    // `/.dad-sandbox` is 13 bytes, so 12 bytes of slug need a DIR of
    // max - 38 bytes.
    let dir_len = socket_path_max() - 38;
    let parent_len = f.root.to_str().unwrap().len() + 1;
    let deep = f.root.join("d".repeat(dir_len - parent_len));
    assert_eq!(deep.to_str().unwrap().len(), dir_len);
    fs::create_dir_all(&deep).unwrap();

    let out = f.run("tui", &deep, "", "release", &[]);
    let text = combined(&out);
    assert!(
        out.status.success(),
        "a branch whose slug fits was refused: {text}"
    );
    let socket = field(&out, "TUI-SOCKET=");
    assert_eq!(
        Path::new(&socket),
        deep.join(".dad-sandbox/branch-main/attach.sock")
    );
    assert!(socket.len() < socket_path_max(), "{socket}");

    f.git(&["checkout", "-q", "--detach"]);
    let out = f.run("tui", &deep, "", "release", &[]);
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a detached HEAD's slug cannot fit, yet it ran: {text}"
    );
    assert!(text.contains("pass SANDBOX="), "{text}");
    assert!(text.contains(&socket_path_max().to_string()), "{text}");
    let entries: Vec<_> = fs::read_dir(deep.join(".dad-sandbox"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        entries,
        vec![std::ffi::OsString::from("branch-main")],
        "it created a sandbox it refused"
    );
}

/// A UTF-8 locale bash honours here, or `None` when there is none. Under one,
/// `${#var}` counts characters rather than bytes, which is the condition the
/// multibyte test needs; without one that test would pass vacuously.
fn utf8_locale() -> Option<&'static str> {
    ["C.UTF-8", "en_US.UTF-8"].into_iter().find(|locale| {
        Command::new("bash")
            .args(["-c", "x=\u{e9}; printf %s \"${#x}\""])
            .env("LC_ALL", locale)
            .output()
            .map(|o| o.stdout == b"1")
            .unwrap_or(false)
    })
}

/// Scenario: under a UTF-8 locale, point DIR at a directory named with
/// two-byte characters, short enough in characters for a per-branch sandbox
/// but too long in bytes, and run; it refuses with the bound, because a socket
/// path is bounded in bytes. An explicit SANDBOX that fits in characters but
/// not in bytes is refused the same way, and not created.
#[test]
fn a_multibyte_path_is_bounded_in_bytes_not_characters() {
    let test = "a_multibyte_path_is_bounded_in_bytes_not_characters";
    if skip_without(&[("bash", "--version"), ("git", "--version")], test) {
        return;
    }
    let Some(locale) = utf8_locale() else {
        eprintln!("SKIP: {test}: no UTF-8 locale for bash here");
        return;
    };
    let f = Fixture::new();
    let max = socket_path_max();
    let root_len = f.root.as_os_str().len();

    // DIR = ROOT/<n x e-acute>. Its sandbox root, DIR/.dad-sandbox, is
    // root_len + 1 + n + 13 characters, which leaves the shortest slug (19)
    // room exactly when n <= max - 46 - root_len. Every e-acute is two bytes,
    // so in bytes it does not.
    let n = max - 46 - root_len;
    let deep = f.root.join("\u{e9}".repeat(n));
    fs::create_dir_all(&deep).unwrap();
    let out = f.run_with_env("tui", &deep, "", "release", &[], &[("LC_ALL", locale)]);
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a DIR too long in bytes was accepted: {text}"
    );
    assert!(text.contains(&max.to_string()), "{text}");
    assert!(!deep.join(".dad-sandbox").exists(), "{text}");

    // SANDBOX = ROOT/<m x e-acute>/sb, whose attach.sock is exactly `max`
    // characters long and `max + m` bytes.
    let m = max - root_len - 16;
    let explicit = f.root.join("\u{e9}".repeat(m)).join("sb");
    let out = f.run_with_env(
        "tui",
        &f.cfg(),
        explicit.to_str().unwrap(),
        "release",
        &[],
        &[("LC_ALL", locale)],
    );
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a SANDBOX too long in bytes was accepted: {text}"
    );
    assert!(text.contains("attach.sock is"), "{text}");
    assert!(!explicit.exists(), "it created a SANDBOX it refused");
}

/// Scenario: create a tag with the same name as the branch the checkout is on,
/// and run; the sandbox is still the branch's own, `branch-main`.
#[test]
fn a_tag_named_like_the_branch_does_not_move_its_sandbox() {
    if skip_without(
        &[("bash", "--version"), ("git", "--version")],
        "a_tag_named_like_the_branch_does_not_move_its_sandbox",
    ) {
        return;
    }
    let f = Fixture::new();
    f.git(&["tag", "main"]);
    assert_eq!(
        Path::new(&f.tui_socket()),
        f.sandbox_root().join("branch-main/attach.sock")
    );
}

/// Scenario: run on a branch whose name holds an uppercase letter; its sandbox
/// carries the hash, so on a case-insensitive filesystem (macOS's default
/// APFS) it cannot land on the sandbox of the same name in lowercase.
#[test]
fn a_branch_name_with_uppercase_carries_the_hash() {
    if skip_without(
        &[("bash", "--version"), ("git", "--version")],
        "a_branch_name_with_uppercase_carries_the_hash",
    ) {
        return;
    }
    let f = Fixture::new();
    f.git(&["checkout", "-q", "-b", "Topic"]);
    let dir = sandbox_of(&f.tui_socket());
    let name = dir.file_name().unwrap().to_str().unwrap().to_string();
    assert!(
        name.starts_with("branch-Topic-") && name.len() == "branch-Topic-".len() + 8,
        "an uppercase name carries an eight-digit hash: {name}"
    );
    assert_ne!(
        name.to_lowercase(),
        "branch-topic",
        "`Topic` would share `topic`'s sandbox on a case-insensitive filesystem"
    );
}

/// Scenario: run with SANDBOX set; the TUI is handed exactly that sandbox's
/// socket, as before per-branch sandboxes.
#[test]
fn an_explicit_sandbox_is_used_as_given() {
    if skip_without(
        &[("bash", "--version"), ("git", "--version")],
        "an_explicit_sandbox_is_used_as_given",
    ) {
        return;
    }
    let f = Fixture::new();
    let explicit = f.root.join("mine");
    let out = f.run("tui", &f.cfg(), explicit.to_str().unwrap(), "release", &[]);
    assert!(out.status.success(), "{}", combined(&out));
    assert_eq!(
        Path::new(&field(&out, "TUI-SOCKET=")),
        explicit.join("attach.sock")
    );
}

/// Scenario: `task run` on `main`, then `task run-stop` with no SANDBOX; the
/// stop is aimed at the socket the run used. On another branch that never ran,
/// `run-stop` finds nothing to stop rather than reaching `main`'s daemon.
#[test]
fn run_stop_reaches_the_sandbox_the_run_used() {
    if skip_without(
        &[("bash", "--version"), ("git", "--version")],
        "run_stop_reaches_the_sandbox_the_run_used",
    ) {
        return;
    }
    let f = Fixture::new();
    let ran = f.tui_socket();
    assert_eq!(f.stop_socket().as_deref(), Some(ran.as_str()));
    f.git(&["checkout", "-q", "-b", "other"]);
    assert_eq!(
        f.stop_socket(),
        None,
        "run-stop on another branch reached main's sandbox"
    );
}

/// Scenario: leave a live socket at `DIR/.dad-sandbox/attach.sock`, the layout
/// from before per-branch sandboxes; `run-stop` and the run both say how to
/// stop it, the run uses its own sandbox, and `run-stop SANDBOX=DIR/.dad-sandbox`
/// still aims at the old daemon's socket.
#[test]
fn the_old_shared_sandbox_is_left_alone_and_still_reachable() {
    if skip_without(
        &[("bash", "--version"), ("git", "--version")],
        "the_old_shared_sandbox_is_left_alone_and_still_reachable",
    ) {
        return;
    }
    let f = Fixture::new();
    let old = f.sandbox_root();
    fs::create_dir_all(&old).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(old.join("attach.sock"))
        .expect("bind the old socket");
    let how_to_stop = format!("task run-stop SANDBOX={}", old.display());

    // Before this branch has run, `run-stop` has nothing of its own to stop,
    // and says where the old daemon is.
    let out = f.run("stop", &f.cfg(), "", "", &[]);
    let text = combined(&out);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("nothing to stop") && text.contains(&how_to_stop),
        "{text}"
    );

    let out = f.run("tui", &f.cfg(), "", "release", &[]);
    let text = combined(&out);
    assert!(out.status.success(), "{text}");
    assert_eq!(
        Path::new(&field(&out, "TUI-SOCKET=")),
        old.join("branch-main/attach.sock")
    );
    assert!(
        text.contains(&how_to_stop),
        "the run did not say how to stop the old sandbox: {text}"
    );

    let out = f.run("stop", &f.cfg(), old.to_str().unwrap(), "", &[]);
    assert!(out.status.success(), "{}", combined(&out));
    assert_eq!(
        Path::new(&field(&out, "STOP-SOCKET=")),
        old.join("attach.sock")
    );
}

/// The TUI stand-in for the terminal test: once the desktop stand-in has made
/// its attempt on the terminal, report whether this process's group is the
/// terminal's foreground, then read one line from it.
const TERMINAL_TUI: &str = r#"#!/usr/bin/env bash
case "${1:-}" in daemon) exit 0 ;; esac
i=0
while [ ! -s "$DAD_TEST_MARK" ] && [ "$i" -lt 200 ]; do sleep 0.05; i=$((i + 1)); done
echo "TUI pgid=$(ps -o pgid= -p $$ | tr -d ' ') tpgid=$(ps -o tpgid= -p $$ | tr -d ' ')"
line=""
read -r line
echo "GOT:$line"
"#;

/// The desktop stand-in (`pnpm`, as `pnpm tauri dev`): ignore SIGTTOU, the way
/// a job-control shell does, and make its own group the terminal's foreground
/// if it can reach the terminal at all. Records which happened, and whether it
/// started with SIGINT ignored (perl reports an inherited SIG_IGN as `IGNORE`).
const TERMINAL_TAKING_DESKTOP: &str = r#"#!/usr/bin/env perl
use POSIX;
my $int = ($SIG{INT} // '') eq 'IGNORE' ? 'int-ignored' : 'int-default';
$SIG{TTOU} = 'IGNORE';
my $result = 'no-tty';
if (open(my $tty, '<', '/dev/tty')) {
    $result = POSIX::tcsetpgrp(fileno($tty), getpgrp()) ? 'took-foreground' : 'tcsetpgrp-refused';
}
open(my $mark, '>', $ENV{DAD_TEST_MARK}) or die "mark: $!";
print $mark "$result $int";
close $mark;
sleep 600;
"#;

/// A loopback port nothing listens on, to stand in for vite's 1420 through
/// `DAD_SANDBOX_RUN_VITE_PORT`: 1420 is in use whenever a `tauri dev` runs on
/// the machine, which is the ordinary state of a desktop developer's box, and
/// the script's preflight would refuse.
///
/// **Held, not just picked** (PR #1672 review). A port read off a listener
/// that is then dropped is free only until another process binds it, and the
/// preflight would then refuse it and fail the test. So the port stays bound,
/// by a socket that never listens, for as long as the [`HeldPort`] lives: no
/// other process can bind the same address, and the script's probe — a
/// connect — finds no listener, which is the answer "free" it needs. On Linux
/// that connect is refused at once; macOS drops the SYN, and the probe gives up
/// after a second (`port_answers` in the script). The probe tries `::1` as
/// well, so that address is held too wherever this host has IPv6.
struct HeldPort {
    port: u16,
    _sockets: Vec<std::os::fd::OwnedFd>,
}

/// A TCP socket bound to `addr` that never listens, or the bind's error.
fn bound_not_listening(addr: std::net::IpAddr, port: u16) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let family = match addr {
        std::net::IpAddr::V4(_) => libc::AF_INET,
        std::net::IpAddr::V6(_) => libc::AF_INET6,
    };
    // SAFETY: `socket` takes no pointers; a non-negative return is a new fd
    // this function owns, which `OwnedFd` closes on every path below.
    let fd = unsafe { libc::socket(family, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by `socket` and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let rc = match addr {
        std::net::IpAddr::V4(v4) => {
            // SAFETY: an all-zero `sockaddr_in` is a valid value of a
            // plain-integer struct; the fields `bind` reads are set below.
            let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            sa.sin_family = libc::AF_INET as libc::sa_family_t;
            sa.sin_port = port.to_be();
            sa.sin_addr.s_addr = u32::from(v4).to_be();
            // SAFETY: `sa` is a live `sockaddr_in` and the length passed is
            // its own size.
            unsafe {
                libc::bind(
                    std::os::fd::AsRawFd::as_raw_fd(&fd),
                    (&sa as *const libc::sockaddr_in).cast(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }
        std::net::IpAddr::V6(v6) => {
            // `::1` only, so this bind cannot also claim the IPv4 port.
            let on: libc::c_int = 1;
            // SAFETY: `on` is a live `c_int` and the length is its size.
            unsafe {
                libc::setsockopt(
                    std::os::fd::AsRawFd::as_raw_fd(&fd),
                    libc::IPPROTO_IPV6,
                    libc::IPV6_V6ONLY,
                    (&on as *const libc::c_int).cast(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            // SAFETY: as for `sockaddr_in` above.
            let mut sa: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            sa.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sa.sin6_port = port.to_be();
            sa.sin6_addr.s6_addr = v6.octets();
            // SAFETY: `sa` is a live `sockaddr_in6` and the length passed is
            // its own size.
            unsafe {
                libc::bind(
                    std::os::fd::AsRawFd::as_raw_fd(&fd),
                    (&sa as *const libc::sockaddr_in6).cast(),
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

/// The port the kernel gave a socket bound to port 0.
fn bound_port(fd: &std::os::fd::OwnedFd) -> u16 {
    // SAFETY: an all-zero `sockaddr_in` is a valid value; `getsockname`
    // writes at most `len` bytes into it and updates `len`.
    let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockname(
            std::os::fd::AsRawFd::as_raw_fd(fd),
            (&mut sa as *mut libc::sockaddr_in).cast(),
            &mut len,
        )
    };
    assert_eq!(rc, 0, "getsockname: {}", std::io::Error::last_os_error());
    u16::from_be(sa.sin_port)
}

fn free_port() -> HeldPort {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    // Bounded: each attempt fails only when another process holds `::1` on
    // the port the kernel just gave `127.0.0.1`.
    for _ in 0..20 {
        let v4 =
            bound_not_listening(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).expect("bind a loopback port");
        let port = bound_port(&v4);
        match bound_not_listening(IpAddr::V6(Ipv6Addr::LOCALHOST), port) {
            Ok(v6) => {
                return HeldPort {
                    port,
                    _sockets: vec![v4, v6],
                };
            }
            // No IPv6 loopback here, so nothing can listen on `::1` either.
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::EADDRNOTAVAIL | libc::EAFNOSUPPORT)
                ) =>
            {
                return HeldPort {
                    port,
                    _sockets: vec![v4],
                };
            }
            Err(e) if e.raw_os_error() == Some(libc::EADDRINUSE) => continue,
            Err(e) => panic!("bind [::1]:{port}: {e}"),
        }
    }
    panic!("no loopback port free on both 127.0.0.1 and ::1 in 20 attempts");
}

/// What a `run_all_` test prints when it fails, so a red run on a platform
/// nobody here can reproduce on says why (PR #1672: build-macos failed with an
/// empty terminal and nothing else to go on).
struct Diagnostics {
    /// The fixture root; every process of the run carries a path under it.
    root: PathBuf,
    sandbox: PathBuf,
    /// The script's pid, whose descendants are listed whatever their argv.
    script_pid: Option<u32>,
    port: u16,
    started: Instant,
    phases: Vec<(String, Duration)>,
}

impl Diagnostics {
    fn new(root: &Path, sandbox: &Path, port: u16) -> Diagnostics {
        Diagnostics {
            root: root.to_path_buf(),
            sandbox: sandbox.to_path_buf(),
            script_pid: None,
            port,
            started: Instant::now(),
            phases: Vec::new(),
        }
    }

    /// Mark the end of a phase, timed from the end of the one before.
    fn phase(&mut self, name: &str) {
        let since = self.phases.iter().map(|(_, d)| *d).sum::<Duration>();
        self.phases.push((
            name.to_string(),
            self.started.elapsed().saturating_sub(since),
        ));
    }

    fn timings(&self) -> String {
        let mut out = String::new();
        for (name, d) in &self.phases {
            out.push_str(&format!("  {name}: {:.2}s\n", d.as_secs_f64()));
        }
        out.push_str(&format!(
            "  total so far: {:.2}s\n",
            self.started.elapsed().as_secs_f64()
        ));
        out
    }

    /// Everything known about the run, for a failure message.
    fn report(&self) -> String {
        format!(
            "\n--- phases ---\n{}--- tools ---\n{}--- vite-port probe ---\n{}--- processes ---\n{}--- sandbox {} ---\n{}",
            self.timings(),
            tools_report(),
            probe_report(self.port),
            self.processes(),
            self.sandbox.display(),
            files_report(&self.sandbox),
        )
    }

    /// `ps` for the script's descendants and anything naming the fixture,
    /// with the fields that say who holds the terminal. The flags are ones
    /// procps and BSD `ps` both take.
    fn processes(&self) -> String {
        let out = match Command::new("ps")
            .args(["-A", "-ww", "-o", "pid,ppid,pgid,sess,tpgid,stat,tty,args"])
            .output()
        {
            Ok(out) => out,
            Err(e) => return format!("ps: {e}\n"),
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        let header = lines.next().unwrap_or_default();
        let rows: Vec<(u32, u32, &str)> = lines
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                let pid = f.next()?.parse().ok()?;
                let ppid = f.next()?.parse().ok()?;
                Some((pid, ppid, l))
            })
            .collect();
        let mut ours: Vec<u32> = self.script_pid.into_iter().collect();
        let mut grew = true;
        while grew {
            grew = false;
            for (pid, ppid, _) in &rows {
                if ours.contains(ppid) && !ours.contains(pid) {
                    ours.push(*pid);
                    grew = true;
                }
            }
        }
        let root = self.root.to_string_lossy();
        let mut report = format!("{header}\n");
        for (pid, _, line) in &rows {
            if ours.contains(pid) || line.contains(root.as_ref()) {
                report.push_str(line);
                report.push('\n');
            }
        }
        report.push_str(&String::from_utf8_lossy(&out.stderr));
        report
    }
}

/// Which bash and perl a run gets, and on what.
fn tools_report() -> String {
    let probe = r#"
        echo "uname: $(uname -sr)"
        echo "bash: $(command -v bash) $(bash -c 'echo $BASH_VERSION')"
        echo "perl: $(command -v perl) $(perl -e 'print $]')"
        echo "sleep 0.1: $(sleep 0.1 && echo ok)"
    "#;
    match Command::new("sh").args(["-c", probe]).output() {
        Ok(out) => combined(&out),
        Err(e) => format!("sh: {e}\n"),
    }
}

/// How a connect to the held port goes, from Rust and bounded: the script's
/// preflight makes the same connect from bash.
fn probe_report(port: u16) -> String {
    let mut out = String::new();
    for addr in ["127.0.0.1", "[::1]"] {
        let Ok(sa) = format!("{addr}:{port}").parse::<std::net::SocketAddr>() else {
            continue;
        };
        let t = Instant::now();
        let result = std::net::TcpStream::connect_timeout(&sa, Duration::from_secs(2));
        let what = match result {
            Ok(_) => "connected".to_string(),
            Err(e) => format!("{:?}: {e}", e.kind()),
        };
        out.push_str(&format!(
            "  {sa}: {what} after {}ms\n",
            t.elapsed().as_millis()
        ));
    }
    out
}

/// Every file in the sandbox, with the contents of the small ones.
fn files_report(dir: &Path) -> String {
    let Ok(entries) = fs::read_dir(dir) else {
        return "(no sandbox dir)\n".to_string();
    };
    let mut out = String::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
        out.push_str(&format!("{} ({len} bytes)\n", path.display()));
        if path.is_file() && len <= 16 * 1024 {
            for line in fs::read_to_string(&path).unwrap_or_default().lines() {
                out.push_str(&format!("  | {line}\n"));
            }
        }
    }
    out
}

/// Kills a leftover desktop group if the test fails before the script's own
/// teardown runs.
struct GroupGuard(Option<String>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = &self.0 {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{pgid}")])
                .status();
        }
    }
}

/// Scenario: start `task run-all`'s script on a terminal with a desktop
/// stand-in that tries to take the terminal's foreground; the TUI stand-in is
/// still in the foreground group, reads the line typed at it, the desktop
/// started with SIGINT at its default, and the desktop's group — the one
/// `desktop.pgid` names — is gone once the TUI exits.
#[test]
fn run_all_keeps_the_tui_in_the_terminals_foreground_group() {
    let test = "run_all_keeps_the_tui_in_the_terminals_foreground_group";
    if skip_without(
        &[("bash", "--version"), ("git", "--version"), ("perl", "-v")],
        test,
    ) {
        return;
    }
    let f = Fixture::new();
    let co = f.checkout();
    f.write_exec(&co.join("target/debug/dot-agent-deck"), TERMINAL_TUI);
    fs::create_dir_all(co.join("desktop/node_modules")).unwrap();
    let stub_bin = f.root.join("stub-bin");
    f.write_exec(&stub_bin.join("pnpm"), TERMINAL_TAKING_DESKTOP);
    let sandbox = f.root.join("sb");
    let mark = f.root.join("desktop-attempt");

    let pty = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open a pty");
    // The script leads the terminal's session here, so its process group is
    // the foreground one, as `task`'s is when a shell runs `task run-all`.
    let mut cmd = portable_pty::CommandBuilder::new("bash");
    cmd.arg(co.join("scripts/sandbox-run.sh"));
    cmd.arg("all");
    cmd.arg(&co);
    cmd.arg(f.cfg());
    cmd.arg(&sandbox);
    cmd.arg("debug");
    for var in AMBIENT_LOCATION_VARS {
        cmd.env_remove(var);
    }
    let path = format!(
        "{}:{}",
        stub_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    cmd.env("PATH", path);
    cmd.env("DAD_TEST_MARK", &mark);
    let port = free_port();
    cmd.env("DAD_SANDBOX_RUN_VITE_PORT", port.port.to_string());
    cmd.cwd(&f.root);
    let mut diag = Diagnostics::new(&f.root, &sandbox, port.port);
    let mut child = pty
        .slave
        .spawn_command(cmd)
        .expect("spawn the script on the pty");
    drop(pty.slave);
    diag.script_pid = child.process_id();
    diag.phase("spawn the script on a pty");

    let mut reader = pty.master.try_clone_reader().expect("pty reader");
    let mut writer = pty.master.take_writer().expect("pty writer");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut seen = String::new();
    let wait_for = |needle: &str, seen: &mut String| -> bool {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !seen.contains(needle) {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left.min(Duration::from_millis(200))) {
                Ok(chunk) => seen.push_str(&String::from_utf8_lossy(&chunk)),
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                Err(_) => return false,
            }
        }
        true
    };

    if !wait_for("TUI pgid=", &mut seen) {
        diag.phase("wait for the TUI stand-in's report (timed out)");
        // Gathered before the kill, so the stuck processes are in it.
        let report = diag.report();
        let _ = child.kill();
        panic!("the TUI stand-in never reported; terminal output:\n{seen}{report}");
    }
    diag.phase("wait for the TUI stand-in's report");
    let mut guard = GroupGuard(
        fs::read_to_string(sandbox.join("desktop.pgid"))
            .ok()
            .map(|s| s.trim().to_string()),
    );
    let report = seen
        .lines()
        .find(|l| l.contains("TUI pgid="))
        .unwrap()
        .trim()
        .to_string();
    let attempt = fs::read_to_string(&mark).unwrap_or_default();
    // Without this the test passes vacuously: a desktop that never started
    // never tried to take the terminal.
    assert!(
        !attempt.is_empty(),
        "the desktop stand-in never ran, so nothing tried to take the terminal; output:\n{seen}{}",
        diag.report()
    );
    let mut fields = report.split_whitespace().filter_map(|w| w.split_once('='));
    let pgid = fields
        .next()
        .map(|(_, v)| v.to_string())
        .unwrap_or_default();
    let tpgid = fields
        .next()
        .map(|(_, v)| v.to_string())
        .unwrap_or_default();
    assert_eq!(
        pgid,
        tpgid,
        "the TUI is not in the terminal's foreground group (desktop stand-in: {attempt}); output:\n{seen}{}",
        diag.report()
    );
    // The keeper is a shell without job control, which starts a background
    // command with SIGINT and SIGQUIT ignored; the desktop's tree must not
    // inherit that.
    assert!(
        attempt.contains("int-default"),
        "the desktop started with SIGINT ignored: {attempt}{}",
        diag.report()
    );

    writer.write_all(b"hello\r").expect("type at the TUI");
    writer.flush().unwrap();
    assert!(
        wait_for("GOT:hello", &mut seen),
        "the TUI did not read the line typed at it (desktop stand-in: {attempt}); output:\n{seen}{}",
        diag.report()
    );
    diag.phase("type a line and read it back");

    // The TUI exits after one line, and the script's teardown stops the
    // desktop's group.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().expect("wait for the script") {
            assert!(
                status.success(),
                "the script failed: {status:?}; output:\n{seen}{}",
                diag.report()
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the script did not exit; output:\n{seen}{}",
            diag.report()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    diag.phase("wait for the script's teardown");
    let pgid = guard
        .0
        .clone()
        .expect("run-all recorded the desktop's group");
    let members = Command::new("ps")
        .args(["-A", "-o", "pgid="])
        .output()
        .expect("ps");
    let alive = String::from_utf8_lossy(&members.stdout)
        .lines()
        .any(|l| l.trim() == pgid);
    assert!(
        !alive,
        "the desktop's group {pgid} outlived the TUI{}",
        diag.report()
    );
    assert!(
        !sandbox.join("desktop.pgid").exists(),
        "desktop.pgid outlived the teardown{}",
        diag.report()
    );
    guard.0 = None;
    eprintln!("{test} phases:\n{}", diag.timings());
}

/// The TUI stand-in for the teardown-race test: exits the moment it starts.
const INSTANT_TUI: &str = "#!/usr/bin/env bash\nexit 0\n";

/// The desktop stand-in for the teardown-race test: records its pid and
/// whether the script had already exited when it started, then holds on.
const RECORDING_DESKTOP: &str = r#"#!/usr/bin/env bash
if [ -e "$DAD_TEST_EXITED" ]; then when=after-exit; else when=before-exit; fi
echo "$$ $when" >"$DAD_TEST_MARK"
exec sleep 600
"#;

/// Scenario: run `task run-all`'s script with a TUI that exits at once and a
/// `perl` that takes half a second to start the keeper's session, so the
/// teardown runs while the keeper is not yet a process-group leader; no
/// desktop starts after the script has exited, and nothing of the desktop's
/// is left running.
#[test]
fn run_all_stops_a_desktop_whose_session_is_not_made_yet() {
    let test = "run_all_stops_a_desktop_whose_session_is_not_made_yet";
    if skip_without(
        &[("bash", "--version"), ("git", "--version"), ("perl", "-v")],
        test,
    ) {
        return;
    }
    let real_perl = Command::new("sh")
        .args(["-c", "command -v perl"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    assert!(
        real_perl.starts_with('/'),
        "no absolute perl: {real_perl:?}"
    );

    let f = Fixture::new();
    let co = f.checkout();
    f.write_exec(&co.join("target/debug/dot-agent-deck"), INSTANT_TUI);
    fs::create_dir_all(co.join("desktop/node_modules")).unwrap();
    let stub_bin = f.root.join("stub-bin");
    f.write_exec(&stub_bin.join("pnpm"), RECORDING_DESKTOP);
    f.write_exec(
        &stub_bin.join("perl"),
        &format!("#!/usr/bin/env bash\nsleep 0.5\nexec {real_perl} \"$@\"\n"),
    );
    let sandbox = f.root.join("sb");
    let mark = f.root.join("desktop-started");
    let exited = f.root.join("script-exited");
    let path = format!(
        "{}:{}",
        stub_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let port = free_port();
    let port_arg = port.port.to_string();
    let mut diag = Diagnostics::new(&f.root, &sandbox, port.port);
    let out = f.run_with_env(
        "all",
        &f.cfg(),
        sandbox.to_str().unwrap(),
        "debug",
        &[],
        &[
            ("PATH", &path),
            ("DAD_TEST_MARK", mark.to_str().unwrap()),
            ("DAD_TEST_EXITED", exited.to_str().unwrap()),
            ("DAD_SANDBOX_RUN_VITE_PORT", &port_arg),
        ],
    );
    fs::write(&exited, "").unwrap();
    diag.phase("run the script to its exit");
    assert!(out.status.success(), "{}{}", combined(&out), diag.report());
    // The script's own waits add up to about eight seconds at most (the
    // vite-port probes, the keeper's two seconds, the group's five), so a run
    // past a minute is a wait that never saw what it waited for — on macOS,
    // before the probe was bounded, it took 151 seconds.
    let ran = diag.phases.last().map(|(_, d)| *d).unwrap_or_default();
    assert!(
        ran < Duration::from_secs(60),
        "the script took {:.1}s to exit; output:\n{}{}",
        ran.as_secs_f64(),
        combined(&out),
        diag.report()
    );

    // Wait, bounded, until nothing of the desktop's is alive: a keeper that
    // outlived the script carries the sandbox path in its argv from perl
    // onwards, and the desktop stand-in is named by the pid it recorded. With
    // the defect that keeper starts the desktop after the script has gone and
    // stops it a second later, so the record outlasts it either way.
    let sandbox_arg = sandbox.to_str().unwrap().to_string();
    let still_running = || -> Vec<String> {
        let desktop_pid = fs::read_to_string(&mark)
            .ok()
            .and_then(|m| m.split_whitespace().next().map(str::to_string));
        let ps = Command::new("ps")
            .args(["-A", "-ww", "-o", "pid=", "-o", "stat=", "-o", "args="])
            .output()
            .expect("ps");
        String::from_utf8_lossy(&ps.stdout)
            .lines()
            .filter(|l| {
                let mut fields = l.split_whitespace();
                let pid = fields.next();
                let zombie = fields.next().is_some_and(|stat| stat.starts_with('Z'));
                !zombie
                    && (l.contains(&sandbox_arg)
                        || (desktop_pid.is_some() && pid == desktop_pid.as_deref()))
            })
            .map(str::to_string)
            .collect()
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut left = still_running();
    while !left.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        left = still_running();
    }
    diag.phase("wait for the desktop's processes to go");
    // Gathered before the kill, so what outlived the script is in it.
    let report = if left.is_empty() {
        String::new()
    } else {
        diag.report()
    };
    for line in &left {
        if let Some(pid) = line.split_whitespace().next() {
            let _ = Command::new("kill").args(["-KILL", pid]).status();
        }
    }
    let started = fs::read_to_string(&mark).unwrap_or_default();
    assert!(
        !started.contains("after-exit"),
        "a desktop started after run-all had exited: {started}; output:\n{}{}",
        combined(&out),
        diag.report()
    );
    assert!(
        left.is_empty(),
        "the desktop outlived run-all: {left:?}{report}"
    );
    eprintln!("{test} phases:\n{}", diag.timings());
}
