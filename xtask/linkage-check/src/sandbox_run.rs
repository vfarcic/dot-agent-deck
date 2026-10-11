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
//! network; the terminal test waits on its stand-ins, bounded.
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
        let mut cmd = Command::new("bash");
        cmd.arg(self.checkout().join("scripts/sandbox-run.sh"))
            .arg(mode)
            .arg(self.checkout())
            .arg(dir)
            .arg(sandbox)
            .arg(profile)
            .args(args);
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
/// each socket path stays within the platform's `sun_path` bound, and the two
/// cut names still get two sandboxes.
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
        assert!(
            socket.len() <= socket_path_max(),
            "{socket} is {} bytes, past {}",
            socket.len(),
            socket_path_max()
        );
        assert!(sandbox_of(socket).starts_with(f.sandbox_root()));
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
/// if it can reach the terminal at all. Records which happened.
const TERMINAL_TAKING_DESKTOP: &str = r#"#!/usr/bin/env perl
use POSIX;
$SIG{TTOU} = 'IGNORE';
my $result = 'no-tty';
if (open(my $tty, '<', '/dev/tty')) {
    $result = POSIX::tcsetpgrp(fileno($tty), getpgrp()) ? 'took-foreground' : 'tcsetpgrp-refused';
}
open(my $mark, '>', $ENV{DAD_TEST_MARK}) or die "mark: $!";
print $mark $result;
close $mark;
sleep 600;
"#;

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
/// still in the foreground group, reads the line typed at it, and the desktop's
/// group — the one `desktop.pgid` names — is gone once the TUI exits.
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
    cmd.cwd(&f.root);
    let mut child = pty
        .slave
        .spawn_command(cmd)
        .expect("spawn the script on the pty");
    drop(pty.slave);

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
        let _ = child.kill();
        if seen.contains("port 1420 is in use") {
            eprintln!("SKIP: {test}: port 1420 is in use on this machine");
            return;
        }
        panic!("the TUI stand-in never reported; terminal output:\n{seen}");
    }
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
        "the desktop stand-in never ran, so nothing tried to take the terminal; desktop.log:\n{}\noutput:\n{seen}",
        fs::read_to_string(sandbox.join("desktop.log")).unwrap_or_default()
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
        pgid, tpgid,
        "the TUI is not in the terminal's foreground group (desktop stand-in: {attempt}); output:\n{seen}"
    );

    writer.write_all(b"hello\r").expect("type at the TUI");
    writer.flush().unwrap();
    assert!(
        wait_for("GOT:hello", &mut seen),
        "the TUI did not read the line typed at it (desktop stand-in: {attempt}); output:\n{seen}"
    );

    // The TUI exits after one line, and the script's teardown stops the
    // desktop's group.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().expect("wait for the script") {
            assert!(
                status.success(),
                "the script failed: {status:?}; output:\n{seen}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the script did not exit; output:\n{seen}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
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
    assert!(!alive, "the desktop's group {pgid} outlived the TUI");
    assert!(!sandbox.join("desktop.pgid").exists());
    guard.0 = None;
}
