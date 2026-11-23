#![cfg(all(feature = "e2e", unix))]

//! Lane-1 restore coverage: every agent here is a fixture executable named
//! `claude`, never a real agent. Move this draft to tests/e2e_agent_restore.rs
//! only after batch A has committed. The CLI is run without a pane identity;
//! attach JSON and inbox bodies are the same public seams the clients use.
//! Test 015 extends that fixture with a real PTY-attached TUI and vt100 grid.

dot_agent_deck_test_harness::link!();
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/work_journal.rs"]
mod journal;
#[path = "support/inbox_route.rs"]
mod route;

use serde_json::{Value, json};
use spec::spec;
use std::cell::RefCell;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const SESSION: &str = "01100000-89ab-4cde-8fab-0123456789ab";
const WRONG_SESSION: &str = "01200000-89ab-4cde-8fab-0123456789ab";
const TASK: &str = "RESTORE-SAVED-TASK-014-4f791: inspect this worktree before acting.\nKeep this second line verbatim.";
// Literal expected text, intentionally independent of production constants.
const CONTINUE_NOTE: &str = "The deck restarted and resumed this session. If your previous turn was cut off before it finished, first check the current state — `git status`, `git log`, whether anything was already pushed, opened or sent — then continue from where you stopped without repeating what already happened. If your previous turn had finished, do nothing and wait for the next message.";
const ONLY_NOTE: &str = "The deck restarted and resumed this session. Your previous turn may have been cut off before it finished. Do nothing until you are told to continue; when you are, first check the current state — `git status`, `git log`, whether anything was already pushed, opened or sent — and do not repeat what already happened.";

// Each Python process also enforces this cap itself with SIGALRM, including
// the child that escapes its parent's group. 180 < the 900-second rule-11
// maximum, and far below DEAD_PID_MIN_AGE. No dependency on a live daemon
// or on Rust Drop is needed for that backstop to fire.
const STAND_IN: &str = r#"#!/usr/bin/env python3
import json, os, select, signal, subprocess, sys, time, tty
root = __ROOT__
deck = __DECK__
signal.signal(signal.SIGALRM, lambda *_: os._exit(0))
signal.alarm(180)
agent = os.environ['DOT_AGENT_DECK_AGENT_ID']
pane = os.environ['DOT_AGENT_DECK_PANE_ID']
args = sys.argv[1:]
session = __SESSION__
source = 'startup'
if '--resume' in args:
    session = args[args.index('--resume') + 1]
    source = 'resume'
    if os.path.exists(os.path.join(root, 'wrong-session')):
        session = __WRONG_SESSION__
elif '--continue' in args:
    # Reopens the last conversation: the fixture's one.
    source = 'resume'
elif not os.path.exists(os.path.join(root, 'fixed-session')):
    # The first fresh start in this fixture is in SESSION; every later fresh
    # start, a start over among them, is in a conversation of its own.
    try:
        os.close(os.open(os.path.join(root, 'session-taken'),
                         os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600))
    except FileExistsError:
        import uuid
        session = str(uuid.uuid4())
record = {'agent':agent, 'pane':pane, 'pid':os.getpid(), 'cwd':os.getcwd(),
          'argv':args, 'session':session,
          'resume_env':[k for k in ['DOT_AGENT_DECK_RESUME_AGENT',
              'DOT_AGENT_DECK_RESUME_SESSION', 'DOT_AGENT_DECK_RESUME_TOKEN'] if k in os.environ]}
def write_json(path, value):
    tmp = path + '.tmp'
    with open(tmp, 'w') as f:
        json.dump(value, f)
    os.replace(tmp, path)
# Registry agent ids restart in each daemon; pane ids distinguish both launches.
write_json(os.path.join(root, 'restore-argv-' + pane + '.json'), record)
if os.path.exists(os.path.join(root, 'survive-stop')):
    child = os.fork()
    if child == 0:
        # Keep the old launch's session, but escape the group shutdown kills.
        os.setpgid(0, 0)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGHUP, signal.SIG_IGN)
        signal.signal(signal.SIGALRM, lambda *_: os._exit(0))
        signal.alarm(180)
        for fd in (0, 1, 2):
            os.close(fd)
        write_json(os.path.join(root, 'survivor.json'),
                   {'pid':os.getpid(), 'sid':os.getsid(0)})
        while True:
            time.sleep(0.1)
if os.path.exists(os.path.join(root, 'ignore-term-child')):
    child = os.fork()
    if child == 0:
        # Stays in the launch's process group, and ignores SIGTERM.
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGHUP, signal.SIG_IGN)
        signal.signal(signal.SIGALRM, lambda *_: os._exit(0))
        signal.alarm(180)
        for fd in (0, 1, 2):
            os.close(fd)
        write_json(os.path.join(root, 'term-survivor.json'),
                   {'pid':os.getpid(), 'sid':os.getsid(0)})
        while True:
            time.sleep(0.1)
slow = os.path.join(root, 'slow-start')
if os.path.exists(slow):
    # Hold the SessionStart report, so a restore waits on its confirmation.
    time.sleep(float(open(slow).read().strip() or '4'))
payload = {'hook_event_name':'SessionStart', 'session_id':session,
           'source':source, 'cwd':os.getcwd()}
hook = subprocess.run([deck, 'hook', '--agent', 'claude-code'],
                      input=json.dumps(payload).encode(), capture_output=True, timeout=8)
if hook.returncode:
    sys.stderr.buffer.write(hook.stderr)
    sys.exit(97)
open(os.path.join(root, 'ready-' + pane), 'w').close()
tty.setraw(0)
os.write(1, '────────────────────\r\n❯\r\n────────────────────\r\n'.encode())
while True:
    if select.select([0], [], [], 0.05)[0] and not os.read(0, 4096):
        break
"#;

struct Decks {
    live: common::DaemonProc,
    gone: Vec<common::DaemonProc>,
    state: PathBuf,
    repo: PathBuf,
    root: PathBuf,
    bin: PathBuf,
    path: String,
    _scratch: tempfile::TempDir,
}

fn daemon(state: &Path, path: &str) -> common::DaemonProc {
    daemon_with(state, path, &[])
}

fn daemon_with(state: &Path, path: &str, extra: &[(&str, &str)]) -> common::DaemonProc {
    let mut env = vec![
        ("DOT_AGENT_DECK_STATE_DIR", state.to_str().unwrap()),
        ("PATH", path),
        ("DOT_AGENT_DECK_EXPERIMENTAL", "1"),
    ];
    env.extend_from_slice(extra);
    inbox::daemon_unbound(&env)
}

impl Decks {
    fn start() -> Self {
        let scratch = common::harness_tempdir().unwrap();
        let root = scratch.path().join("restore-sentinels");
        let bin = scratch.path().join("bin");
        let repo = scratch.path().join("restore-repo");
        for dir in [&root, &bin, &repo] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let script = STAND_IN
            .replace("__ROOT__", &serde_json::to_string(&root).unwrap())
            .replace(
                "__DECK__",
                &serde_json::to_string(common::contained_deck_binary()).unwrap(),
            )
            .replace("__SESSION__", &serde_json::to_string(SESSION).unwrap())
            .replace(
                "__WRONG_SESSION__",
                &serde_json::to_string(WRONG_SESSION).unwrap(),
            );
        std::fs::write(bin.join("claude"), script).unwrap();
        std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        // Keep the harness's contained PATH (including Python on macOS), with
        // our executable first. No fixture ever falls through to real Claude.
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let state = scratch.path().join("state");
        let live = daemon(&state, &path);
        journal::init_repo(&repo);
        std::fs::write(
            repo.join(".git/info/exclude"),
            "/.dot-agent-deck/\n.test-capability-*\n.route-*\n",
        )
        .unwrap();
        Self {
            live,
            gone: Vec::new(),
            state,
            repo,
            root,
            bin,
            path,
            _scratch: scratch,
        }
    }

    fn command(&self) -> String {
        common::capability_export_command("exec claude --model restore-synthetic")
    }

    fn start_pane(&self, name: &str) -> Value {
        self.start_pane_with(name, &self.repo, &self.command())
    }

    /// A pane named `name` running `command` in `cwd`, on the live daemon.
    fn start_pane_with(&self, name: &str, cwd: &Path, command: &str) -> Value {
        let reply = inbox::attach(
            &self.live,
            json!({
                "op":"start-agent", "command":command, "cwd":cwd,
                "display_name":name, "agent_type":"claude_code", "rows":40, "cols":140,
                "env":[["DOT_AGENT_DECK_PANE_ID", name]]
            }),
        );
        assert_eq!(reply["ok"], true, "fixture launch: {reply}");
        let agent = string(&reply, "id");
        let started = self.wait_start(name);
        journal::wait_for(&self.state, "the fixture attempt", |lines| {
            journal::attempt_of(lines, &agent).cloned()
        });
        started
    }

    fn wait_start(&self, pane: &str) -> Value {
        let file = self.root.join(format!("restore-argv-{pane}.json"));
        assert!(
            common::wait_until(common::child_boot_budget(), || file.is_file()
                && self.root.join(format!("ready-{pane}")).is_file()),
            "stand-in in pane {pane} never reported SessionStart"
        );
        serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap()
    }

    fn starts(&self) -> Vec<Value> {
        let mut starts: Vec<Value> = std::fs::read_dir(&self.root)
            .unwrap()
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with("restore-argv-") && name.ends_with(".json")
            })
            .map(|e| serde_json::from_slice(&std::fs::read(e.path()).unwrap()).unwrap())
            .collect();
        starts.sort_by_key(|s| string(s, "pane"));
        starts
    }

    fn restart(&mut self) {
        self.restart_with(&[]);
    }

    /// [`Self::restart`], the new daemon started with `extra` in its
    /// environment.
    fn restart_with(&mut self, extra: &[(&str, &str)]) {
        // SAFETY: signal only the daemon pid this fixture owns, never a pattern.
        assert_eq!(
            unsafe { libc::kill(self.live.pid() as libc::pid_t, libc::SIGTERM) },
            0
        );
        assert!(
            self.live
                .wait_for_exit(common::load_scaled(Duration::from_secs(20))),
            "daemon did not stop"
        );
        let mut next = daemon_with(&self.state, &self.path, extra);
        assert!(next.is_alive_public());
        self.gone.push(std::mem::replace(&mut self.live, next));
    }

    /// Another daemon on the same state directory, with sockets of its own.
    fn another_daemon(&self) -> common::DaemonProc {
        let mut other = daemon(&self.state, &self.path);
        assert!(other.is_alive_public());
        other
    }

    /// A private directory beside the repository, for a second agent's
    /// folder.
    fn folder(&self, name: &str) -> PathBuf {
        let dir = self.repo.parent().unwrap().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn person(&self, args: &[&str]) -> Output {
        self.person_with_flag(args, "1")
    }

    fn person_with_flag(&self, args: &[&str], flag: &str) -> Output {
        Command::new(common::contained_deck_binary())
            .args(args)
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", &self.live.home)
            .env("DOT_AGENT_DECK_SOCKET", &self.live.hook_socket)
            .env("DOT_AGENT_DECK_ATTACH_SOCKET", &self.live.attach_socket)
            .env("DOT_AGENT_DECK_STATE_DIR", &self.state)
            .env("DOT_AGENT_DECK_EXPERIMENTAL", flag)
            .stdin(Stdio::null())
            .current_dir(&self.repo)
            .output()
            .unwrap()
    }

    fn list(&self) -> Value {
        let out = self.person(&["interrupted", "list", "--json", "--no-git"]);
        exit(&out, 0);
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn item_for(&self, attempt: &str) -> Value {
        let list = self.list();
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| {
                item["agents"]
                    .as_array()
                    .is_some_and(|agents| agents.iter().any(|agent| agent["attempt"] == attempt))
            })
            .cloned()
            .unwrap_or_else(|| panic!("attempt {attempt} is not listed: {list}"))
    }

    fn attempt(&self, agent: &str) -> Value {
        journal::wait_for(&self.state, "the original attempt", |lines| {
            journal::attempt_of(lines, agent).cloned()
        })
    }

    fn inbox(&self, pane: Option<&str>) -> Vec<Value> {
        let reply = inbox::attach(
            &self.live,
            json!({"op":"list-inbox", "pane_id":pane, "bodies":true}),
        );
        assert_eq!(reply["ok"], true, "list-inbox: {reply}");
        reply["inbox_messages"]
            .as_array()
            .expect("inbox history")
            .clone()
    }

    fn assert_first_message(&self, started: &Value, reason: &str, body: &str) {
        let messages = RefCell::new(Vec::new());
        let pane = string(started, "pane");
        let agent = string(started, "agent");
        assert!(
            common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
                *messages.borrow_mut() = self
                    .inbox(Some(&pane))
                    .into_iter()
                    .filter(|m| m["bind_agent"] == agent)
                    .collect();
                !messages.borrow().is_empty()
            }),
            "agent {agent} got no first inbox message"
        );
        let messages = messages.into_inner();
        assert_eq!(
            messages.len(),
            1,
            "exactly one first prompt, no duplicate or replayed task: {messages:?}"
        );
        assert_eq!(
            messages[0]["from"],
            json!({"type":"deck", "reason":reason}),
            "{messages:?}"
        );
        let rendered = messages[0]["text"]
            .as_str()
            .expect("the agent-visible block");
        // Full framing is tested by the inbox suite; pin the exact body here.
        assert!(
            rendered.contains(&format!("\n{body}\n")),
            "first inbox body differs: {rendered}"
        );
    }

    fn assert_gone_from_list(&self, id: &str) {
        let list = self.list();
        assert!(
            !list["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["id"] == id),
            "item must leave interrupted list: {list}"
        );
    }
}

fn string(value: &Value, field: &str) -> String {
    value[field]
        .as_str()
        .unwrap_or_else(|| panic!("missing string {field}: {value}"))
        .into()
}

fn text(out: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn exit(out: &Output, expected: i32) {
    assert_eq!(out.status.code(), Some(expected), "{}", text(out));
}

fn restore_request(id: &str, attempt: &str) -> Value {
    json!({"op":"restore-interrupted", "id":id, "agents":[attempt], "mode":"continue", "dry_run":false})
}

// Unlike inbox fixture operations (10 seconds), restore may wait for a
// newly spawned process to confirm its session. This is a bounded final
// response, not a pending reply or an arbitrary sleep.
fn final_reply(decks: &Decks, request: Value) -> Value {
    final_reply_on(&decks.live, request)
}

fn final_reply_on(daemon: &common::DaemonProc, request: Value) -> Value {
    use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP};
    let mut socket = UnixStream::connect(&daemon.attach_socket).unwrap();
    let budget = common::load_scaled(Duration::from_secs(60));
    socket.set_read_timeout(Some(budget)).unwrap();
    socket.set_write_timeout(Some(budget)).unwrap();
    let body = serde_json::to_vec(&request).unwrap();
    socket.write_all(&[KIND_REQ]).unwrap();
    socket
        .write_all(&(body.len() as u32).to_be_bytes())
        .unwrap();
    socket.write_all(&body).unwrap();
    let mut header = [0; 5];
    socket.read_exact(&mut header).unwrap();
    assert_eq!(header[0], KIND_RESP);
    let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    assert!(len <= 1024 * 1024, "bounded restore response");
    let mut body = vec![0; len];
    socket.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn assert_refused(reply: &Value, attempt: &str, reason: &str) {
    let outcome = &reply["interrupted_restore"];
    assert_eq!(
        outcome["restored"],
        json!([]),
        "no success on refusal: {reply}"
    );
    let refused = outcome["refused"]
        .as_array()
        .unwrap_or_else(|| panic!("final restore outcome missing: {reply}"));
    assert_eq!(refused.len(), 1, "{reply}");
    assert_eq!(refused[0]["attempt"], attempt, "{reply}");
    assert_eq!(refused[0]["reason"], reason, "{reply}");
    assert!(
        refused[0]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "a human-readable reason: {reply}"
    );
}

/// Scenario: Restart a daemon that ran a scripted Claude with a known conversation, inspect the CLI's per-agent verdict, and refuse an unconfirmed or disabled restore without starting anything. Restore and continue then starts the original command with literal resume arguments, removes the interrupted item, and delivers exactly the continue note as its first inbox message.
#[spec("journal/restore/011")]
#[test]
fn journal_restore_011_continue_resumes_the_session_before_its_first_message() {
    let mut decks = Decks::start();
    let original = decks.start_pane("restore-continue-011");
    let attempt = string(&decks.attempt(&string(&original, "agent")), "attempt");
    decks.restart();
    let item = decks.item_for(&attempt);
    let id = string(&item, "id");
    assert_eq!(
        item["agents"][0]["restore"],
        json!({"kind":"resume"}),
        "{item}"
    );
    let shown = decks.person(&["interrupted", "show", &id]);
    exit(&shown, 0);
    let shown = text(&shown);
    assert!(
        shown.contains(&attempt)
            && shown.contains("restore-continue-011")
            && shown.contains("Can be restored with its conversation."),
        "show must name each agent, attempt, and verdict: {shown}"
    );

    let unconfirmed = decks.person(&["interrupted", "restore", &id, "--continue"]);
    exit(&unconfirmed, 1);
    assert!(
        text(&unconfirmed).contains("--yes"),
        "no TTY needs --yes: {}",
        text(&unconfirmed)
    );
    for args in [
        vec!["interrupted", "restore", &id, "--continue", "--yes"],
        vec![
            "interrupted",
            "start-over",
            &id,
            "--agent",
            &attempt,
            "--yes",
        ],
    ] {
        let disabled = decks.person_with_flag(&args, "0");
        exit(&disabled, 1);
        assert!(
            text(&disabled).contains("experimental"),
            "name the disabled flag: {}",
            text(&disabled)
        );
    }
    assert_eq!(decks.starts().len(), 1, "refusals start nothing");
    assert!(decks.inbox(None).is_empty(), "refusals send nothing");
    let dry = decks.person(&["interrupted", "restore", &id, "--continue", "--dry-run"]);
    exit(&dry, 0);
    assert_eq!(decks.starts().len(), 1, "a dry run starts nothing");
    assert_eq!(decks.item_for(&attempt)["id"], id);

    let restored = decks.person(&["interrupted", "restore", &id, "--continue", "--yes"]);
    exit(&restored, 0);
    let starts = decks.starts();
    assert_eq!(
        starts.len(),
        2,
        "restore must launch exactly once: {starts:?}"
    );
    let resumed = starts
        .iter()
        .find(|s| s["pane"] != original["pane"])
        .unwrap();
    let resumed = decks.wait_start(&string(resumed, "pane"));
    assert_eq!(
        resumed["argv"],
        json!(["--resume", SESSION, "--model", "restore-synthetic"])
    );
    assert_eq!(
        resumed["resume_env"],
        json!([]),
        "one-shot resume request must not reach the child"
    );
    assert_eq!(resumed["cwd"], original["cwd"], "original working folder");
    let records = common::agent_records_on(&decks.live.attach_socket);
    let card = records
        .iter()
        .find(|r| r.id == string(&resumed, "agent"))
        .expect("restored agent is on the dashboard");
    assert_eq!(card.display_name.as_deref(), Some("restore-continue-011"));
    assert!(
        card.tab_membership.is_none(),
        "restored agent is an independent dashboard pane"
    );
    decks.assert_first_message(&resumed, "restart note", CONTINUE_NOTE);
    decks.assert_gone_from_list(&id);
}

// 015 uses a separate, initially absent snapshot path for each TUI. It exercises
// the restore question; 018 owns deduplication against a saved workspace.
fn restore_tui(decks: &Decks, snapshot: &str) -> common::TuiDeck {
    common::TuiDeck::builder()
        .with_pty_size(140, 40)
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            decks.live.attach_socket.to_string_lossy().to_string(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            decks.live.hook_socket.to_string_lossy().to_string(),
        )
        .with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            decks.root.join(snapshot).to_string_lossy().to_string(),
        )
        .without_agent_credentials()
        .launch_with_fixture("minimal")
}

/// Scenario: Observe a scripted Claude's named pane under an attached TUI, restart its daemon, and attach a fresh TUI to the replacement daemon. Open Interrupted work with I, press s to see the two restore choices without starting anything, then o to bring back the named pane with exactly the Restore-only note as its first inbox message.
#[spec("journal/restore/015")]
#[test]
fn journal_restore_015_restore_only_from_the_tui_shows_the_named_pane_and_first_note() {
    use dot_agent_deck::interrupted_work::{
        INTERRUPTED_WORK_TITLE, RESTORE_AND_CONTINUE, RESTORE_ONLY, restore_question,
    };

    const NAME: &str = "restore-tui-015";
    let mut decks = Decks::start();
    let original = decks.start_pane(NAME);
    let attempt = string(&decks.attempt(&string(&original, "agent")), "attempt");
    let before = restore_tui(&decks, "tui-before-015.toml");
    assert!(
        before.wait_for_grid_predicate_within(common::child_boot_budget(), |grid| {
            common::label_in_box_top_border(grid, NAME)
        }),
        "the original agent must be visibly attached before restart:\n{}",
        before.snapshot_grid()
    );
    // Signal only the daemon this fixture owns, while its TUI is attached.
    assert_eq!(
        unsafe { libc::kill(decks.live.pid() as libc::pid_t, libc::SIGTERM) },
        0
    );
    assert!(
        decks
            .live
            .wait_for_exit(common::load_scaled(Duration::from_secs(20))),
        "the attached daemon must stop"
    );
    // DaemonProc assigns fresh socket paths per lifetime, so reattach a new TUI
    // to the new endpoints; this is deliberately not a same-process reconnect test.
    drop(before);
    let mut next = daemon(&decks.state, &decks.path);
    assert!(next.is_alive_public());
    decks.gone.push(std::mem::replace(&mut decks.live, next));
    let id = string(&decks.item_for(&attempt), "id");
    let attached = restore_tui(&decks, "tui-after-015.toml");
    attached.wait_until_grid("the restart's interrupted item", |grid| {
        grid.contains("interrupted item")
    });
    attached.send_keys(b"I");
    attached.wait_until_grid("the stopped agent in Interrupted work", |grid| {
        grid.contains(INTERRUPTED_WORK_TITLE) && grid.contains(NAME)
    });
    attached.send_keys(b"s");
    attached.wait_until_grid("the restore question and both choices", |grid| {
        grid.contains(&restore_question(NAME))
            && grid.contains(RESTORE_AND_CONTINUE)
            && grid.contains(RESTORE_ONLY)
    });
    assert_eq!(decks.starts().len(), 1, "asking alone starts nothing");
    assert!(decks.inbox(None).is_empty(), "asking alone sends nothing");
    attached.send_keys(b"o");
    let budget = common::load_scaled(Duration::from_secs(60));
    assert!(
        attached.wait_for_grid_predicate_within(budget, |grid| {
            !grid.contains(&restore_question(NAME)) && common::label_in_box_top_border(grid, NAME)
        }),
        "Restore only must reveal the named dashboard pane:\n{}",
        attached.snapshot_grid()
    );
    let starts = decks.starts();
    assert_eq!(starts.len(), 2, "one resumed launch: {starts:?}");
    let resumed = starts
        .iter()
        .find(|s| s["pane"] != original["pane"])
        .unwrap();
    let resumed = decks.wait_start(&string(resumed, "pane"));
    assert_eq!(
        resumed["argv"],
        json!(["--resume", SESSION, "--model", "restore-synthetic"])
    );
    assert_eq!(resumed["cwd"], original["cwd"]);
    assert_eq!(resumed["resume_env"], json!([]));
    decks.assert_first_message(&resumed, "restart note", ONLY_NOTE);
    decks.assert_gone_from_list(&id);
}

/// Scenario: After a restart a scripted Claude acknowledges the resume arguments but reports a different conversation. Restore reports not-resumed, stops the blank agent before sending any inbox message, and leaves the old item visible with Start over offered.
#[spec("journal/restore/012")]
#[test]
fn journal_restore_012_a_different_session_is_stopped_without_a_first_message() {
    let mut decks = Decks::start();
    let original = decks.start_pane("restore-wrong-012");
    let attempt = string(&decks.attempt(&string(&original, "agent")), "attempt");
    decks.restart();
    std::fs::write(decks.root.join("wrong-session"), "").unwrap();
    let id = string(&decks.item_for(&attempt), "id");
    let reply = final_reply(&decks, restore_request(&id, &attempt));
    assert_refused(&reply, &attempt, "not-resumed");
    assert!(
        reply["interrupted_restore"]["refused"][0]["message"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("conversation"),
        "explain the lost conversation: {reply}"
    );
    let starts = decks.starts();
    assert_eq!(starts.len(), 2, "one attempted resume: {starts:?}");
    let blank = starts
        .iter()
        .find(|s| s["pane"] != original["pane"])
        .unwrap();
    assert_eq!(blank["session"], WRONG_SESSION);
    assert_eq!(blank["argv"][0], "--resume");
    assert_eq!(blank["argv"][1], SESSION);
    let blank_id = string(blank, "agent");
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
            !common::agent_records_on(&decks.live.attach_socket)
                .iter()
                .any(|r| r.id == blank_id)
        }),
        "failed resume must stop its blank agent"
    );
    let pid = blank["pid"].as_u64().unwrap() as libc::pid_t;
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
            // SAFETY: signal 0 only probes this fixture's recorded pid.
            (unsafe { libc::kill(pid, 0) }) == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        }),
        "failed resume removed its card but left the blank process running"
    );
    assert!(
        decks.inbox(None).is_empty(),
        "failed restore posts nothing, even in recent inbox history"
    );
    let kept = decks.item_for(&attempt);
    assert_eq!(
        kept["agents"][0]["restore"]["kind"], "start_over",
        "failed resume offers Start over: {kept}"
    );
    let show = decks.person(&["interrupted", "show", &id]);
    exit(&show, 0);
    assert!(
        text(&show).contains("Start over in this worktree"),
        "{}",
        text(&show)
    );
}

#[cfg(target_os = "linux")]
struct Survivor {
    pid: u32,
    start: u64,
}

#[cfg(target_os = "linux")]
impl Drop for Survivor {
    fn drop(&mut self) {
        if dot_agent_deck::platform::proc::process_start_of(self.pid) == Some(self.start) {
            // SAFETY: exact fixture child, still carrying its recorded identity.
            unsafe {
                libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

/// Scenario: A scripted agent leaves a capped child in its old session but outside the process group stopped with the daemon. After the restart Restore refuses still-running, starts no second agent, sends no message, and keeps the item available.
#[cfg(target_os = "linux")]
#[spec("journal/restore/013")]
#[test]
fn journal_restore_013_an_old_session_descendant_blocks_a_second_writer() {
    let mut decks = Decks::start();
    std::fs::write(decks.root.join("survive-stop"), "").unwrap();
    let original = decks.start_pane("restore-survivor-013");
    let path = decks.root.join("survivor.json");
    assert!(common::wait_until(common::child_boot_budget(), || path.is_file()));
    let child: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let pid = child["pid"].as_u64().unwrap() as u32;
    let survivor = Survivor {
        pid,
        start: dot_agent_deck::platform::proc::process_start_of(pid).expect("fixture child exists"),
    };
    let attempt = string(&decks.attempt(&string(&original, "agent")), "attempt");
    decks.restart();
    assert_eq!(
        dot_agent_deck::platform::proc::process_start_of(pid),
        Some(survivor.start),
        "premise: the old agent child survived daemon shutdown"
    );
    let id = string(&decks.item_for(&attempt), "id");
    let reply = final_reply(&decks, restore_request(&id, &attempt));
    assert_refused(&reply, &attempt, "still-running");
    assert_eq!(
        decks.starts().len(),
        1,
        "old process refusal starts nothing"
    );
    assert!(
        decks.inbox(None).is_empty(),
        "old process refusal posts nothing"
    );
    assert_eq!(decks.item_for(&attempt)["id"], id);
    drop(survivor);
}

/// Scenario: Restart a dispatched single-agent unit and replace its saved task with a symlink: Start over refuses task-unsafe and starts nothing. Restore the original owner-only task file, then Start over runs the saved command in the worktree and gives the fresh agent exactly that task as its first inbox message, without a restart note.
#[spec("journal/restore/014")]
#[test]
fn journal_restore_014_start_over_replays_only_a_safely_saved_dispatch_task() {
    let mut decks = Decks::start();
    let caller = route::start(
        &decks.live,
        "restore-dispatcher-014",
        json!({"cwd":decks.repo}),
    );
    inbox::status(&decks.live, &caller, "session_start", "codex");
    inbox::status(&decks.live, &caller, "thinking", "codex");
    let before = route::messages(&decks.live, &caller).len();
    route::cli(
        &decks.live,
        &caller,
        &decks.repo,
        &[
            "dispatch",
            "restore-start-over-014",
            "--single",
            "--command",
            &decks.command(),
            "--agent",
            "claude",
            "--task",
            TASK,
        ],
    );
    assert!(
        common::wait_until(
            common::load_scaled(Duration::from_secs(20)),
            || route::messages(&decks.live, &caller).len() > before
        ),
        "dispatch must finish before restart"
    );
    let starts = decks.starts();
    assert_eq!(starts.len(), 1, "single unit started once: {starts:?}");
    let unit = decks.wait_start(&string(&starts[0], "pane"));
    let old_attempt = decks.attempt(&string(&unit, "agent"));
    let attempt = string(&old_attempt, "attempt");
    let work = string(&old_attempt, "work");
    let worktree = PathBuf::from(string(&unit, "cwd"));
    let task_path = worktree
        .join(".dot-agent-deck")
        .join(format!("dispatch-task-{work}.md"));
    assert_eq!(
        std::fs::read_to_string(&task_path).unwrap(),
        TASK,
        "premise: original dispatch saved this task"
    );
    decks.restart();
    drop(caller);
    let item = decks.item_for(&attempt);
    assert_eq!(item["kind"], "unit", "real dispatch item: {item}");
    let id = string(&item, "id");
    let kept = task_path.with_extension("kept");
    std::fs::rename(&task_path, &kept).unwrap();
    std::os::unix::fs::symlink(&kept, &task_path).unwrap();
    let reply = final_reply(
        &decks,
        json!({
            "op":"start-over-interrupted", "id":id, "agent":attempt, "dry_run":false
        }),
    );
    assert_eq!(
        reply["interrupted_start_over"]["refused"]["reason"], "task-unsafe",
        "symlinked task must be refused: {reply}"
    );
    assert!(
        reply["interrupted_start_over"]["started"].is_null(),
        "{reply}"
    );
    assert_eq!(decks.starts().len(), 1, "unsafe task starts nothing");
    assert!(decks.inbox(None).is_empty(), "unsafe task posts nothing");
    assert_eq!(decks.item_for(&attempt)["id"], id);
    std::fs::remove_file(&task_path).unwrap();
    std::fs::rename(&kept, &task_path).unwrap();
    let started = decks.person(&[
        "interrupted",
        "start-over",
        &id,
        "--agent",
        &attempt,
        "--yes",
    ]);
    exit(&started, 0);
    // Start over returns after spawn; the stand-in writes its sentinel next.
    common::wait_until(common::child_boot_budget(), || decks.starts().len() >= 2);
    let starts = decks.starts();
    assert_eq!(starts.len(), 2, "Start over launches once: {starts:?}");
    let fresh = starts.iter().find(|s| s["pane"] != unit["pane"]).unwrap();
    let fresh = decks.wait_start(&string(fresh, "pane"));
    assert_eq!(
        fresh["argv"],
        json!(["--model", "restore-synthetic"]),
        "fresh launch gets no resume arguments"
    );
    assert_eq!(fresh["cwd"], unit["cwd"], "saved worktree");
    decks.assert_first_message(&fresh, "saved task", TASK);
    for message in decks.inbox(Some(&string(&fresh, "pane"))) {
        assert_ne!(message["from"]["reason"], "restart note");
        let text = message["text"].as_str().unwrap();
        assert!(
            !text.contains(CONTINUE_NOTE) && !text.contains(ONLY_NOTE),
            "a fresh agent gets no restart note: {text}"
        );
    }
    decks.assert_gone_from_list(&id);
}

fn start_over_request(id: &str, attempt: &str) -> Value {
    json!({"op":"start-over-interrupted", "id":id, "agent":attempt, "dry_run":false})
}

/// The single refusal of a whole restore request (no attempt named).
fn assert_whole_refusal(reply: &Value, reason: &str) {
    let outcome = &reply["interrupted_restore"];
    assert_eq!(outcome["restored"], json!([]), "{reply}");
    let refused = outcome["refused"].as_array().expect("an outcome");
    assert_eq!(
        refused.len(),
        1,
        "one bounded whole-request refusal: {reply}"
    );
    assert!(refused[0]["attempt"].is_null(), "{reply}");
    assert_eq!(refused[0]["reason"], reason, "{reply}");
}

/// The reason a start-over request was refused for.
fn start_over_refusal(reply: &Value) -> String {
    let outcome = &reply["interrupted_start_over"];
    assert!(outcome["started"].is_null(), "refused: {reply}");
    string(&outcome["refused"], "reason")
}

/// Scenario: Restart a scripted Claude and ask the CLI to restore it with the same --agent twice, then send restore requests naming 17 agents, an over-long id, and five thousand agents. Each is refused whole as bad-selection with one small answer, and nothing is started or sent.
#[spec("journal/restore/019")]
#[test]
fn journal_restore_019_a_repeated_or_oversized_selection_is_refused_whole() {
    let mut decks = Decks::start();
    let original = decks.start_pane("restore-select-019");
    let attempt = string(&decks.attempt(&string(&original, "agent")), "attempt");
    decks.restart();
    let id = string(&decks.item_for(&attempt), "id");

    let twice = decks.person(&[
        "interrupted",
        "restore",
        &id,
        "--continue",
        "--yes",
        "--agent",
        &attempt,
        "--agent",
        &attempt,
    ]);
    exit(&twice, 1);
    assert!(
        text(&twice).contains("named an agent twice"),
        "{}",
        text(&twice)
    );

    let long = "x".repeat(129);
    let many: Vec<String> = (0..17).map(|n| format!("{attempt}{n}")).collect();
    let flood: Vec<String> = (0..5000).map(|n| format!("x{n}")).collect();
    for agents in [
        json!([attempt.clone(), attempt.clone()]),
        json!(many),
        json!([long]),
        json!(flood),
    ] {
        let reply = final_reply(
            &decks,
            json!({"op":"restore-interrupted", "id":id, "agents":agents, "mode":"continue", "dry_run":false}),
        );
        assert_whole_refusal(&reply, "bad-selection");
        assert!(
            serde_json::to_vec(&reply).unwrap().len() < 4096,
            "a small answer, not one refusal per selector: {reply}"
        );
    }
    assert_eq!(
        decks.starts().len(),
        1,
        "a refused selection starts nothing"
    );
    assert!(decks.inbox(None).is_empty(), "and sends nothing");
    assert_eq!(decks.item_for(&attempt)["id"], id);
}

/// Scenario: Restart a scripted Claude, then hold a Start over request after it has read the item but before it starts anything. Restore and continue brings the agent back meanwhile; released, the held Start over reads the item again, finds it no longer interrupted, and starts nothing, so exactly one agent and one restart note exist.
#[spec("journal/restore/020")]
#[test]
fn journal_restore_020_a_request_prepared_before_a_restore_rechecks_before_it_starts() {
    let mut decks = Decks::start();
    let original = decks.start_pane("restore-stale-020");
    let attempt = string(&decks.attempt(&string(&original, "agent")), "attempt");
    let barrier = decks.root.join("start-over-barrier");
    let reached = barrier.with_extension("reached");
    decks.restart_with(&[(
        "DOT_AGENT_DECK_TEST_RESTORE_BARRIER",
        barrier.to_str().unwrap(),
    )]);
    let id = string(&decks.item_for(&attempt), "id");
    std::fs::write(&barrier, "").unwrap();
    let socket = decks.live.attach_socket.clone();
    let held = std::thread::spawn({
        let request = start_over_request(&id, &attempt);
        move || {
            let mut socket = UnixStream::connect(socket).unwrap();
            let budget = common::load_scaled(Duration::from_secs(120));
            socket.set_read_timeout(Some(budget)).unwrap();
            let body = serde_json::to_vec(&request).unwrap();
            socket
                .write_all(&[dot_agent_deck::daemon_protocol::KIND_REQ])
                .unwrap();
            socket
                .write_all(&(body.len() as u32).to_be_bytes())
                .unwrap();
            socket.write_all(&body).unwrap();
            let mut header = [0; 5];
            socket.read_exact(&mut header).unwrap();
            let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
            let mut body = vec![0; len];
            socket.read_exact(&mut body).unwrap();
            serde_json::from_slice::<Value>(&body).unwrap()
        }
    });
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(20)), || reached
            .is_file()),
        "the Start over request must reach its barrier after reading the item"
    );
    let restored = final_reply(&decks, restore_request(&id, &attempt));
    assert_eq!(
        restored["interrupted_restore"]["restored"]
            .as_array()
            .map(Vec::len),
        Some(1),
        "the restore goes ahead: {restored}"
    );
    std::fs::remove_file(&barrier).unwrap();
    let late = held.join().unwrap();
    assert_eq!(
        start_over_refusal(&late),
        "not-listed",
        "the held request reads again before starting: {late}"
    );
    assert_eq!(
        decks.starts().len(),
        2,
        "one restored agent, no second writer"
    );
    let resumed = decks
        .starts()
        .into_iter()
        .find(|s| s["pane"] != original["pane"])
        .unwrap();
    decks.assert_first_message(&resumed, "restart note", CONTINUE_NOTE);
}

/// Run `requests` at once, each on its own daemon, and answer every reply.
fn at_once(requests: Vec<(&common::DaemonProc, Value)>) -> Vec<Value> {
    let start = std::sync::Barrier::new(requests.len());
    std::thread::scope(|scope| {
        let handles: Vec<_> = requests
            .into_iter()
            .map(|(daemon, request)| {
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    final_reply_on(daemon, request)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

/// Scenario: Two daemons with sockets of their own share one state directory after a restart, and the scripted Claude reports its conversation only after a delay. Restoring one agent from both daemons at once, and restoring two agents that were in one conversation from the two daemons at once, each brings back exactly one agent and refuses the other request.
#[spec("journal/restore/021")]
#[test]
fn journal_restore_021_two_daemons_on_one_state_directory_start_one_writer() {
    // The same attempt, from both daemons.
    let mut decks = Decks::start();
    let original = decks.start_pane("restore-twice-021");
    let attempt = string(&decks.attempt(&string(&original, "agent")), "attempt");
    decks.restart();
    let other = decks.another_daemon();
    let id = string(&decks.item_for(&attempt), "id");
    std::fs::write(decks.root.join("slow-start"), "3").unwrap();
    let replies = at_once(vec![
        (&decks.live, restore_request(&id, &attempt)),
        (&other, restore_request(&id, &attempt)),
    ]);
    let restored: usize = replies
        .iter()
        .map(|r| {
            r["interrupted_restore"]["restored"]
                .as_array()
                .unwrap()
                .len()
        })
        .sum();
    assert_eq!(restored, 1, "exactly one restore: {replies:?}");
    let refused: Vec<&Value> = replies
        .iter()
        .flat_map(|r| r["interrupted_restore"]["refused"].as_array().unwrap())
        .collect();
    assert_eq!(refused.len(), 1, "{replies:?}");
    assert!(
        matches!(
            refused[0]["reason"].as_str(),
            Some("already-restoring" | "session-in-use" | "directory-in-use" | "not-listed")
        ),
        "{replies:?}"
    );
    assert_eq!(decks.starts().len(), 2, "one writer: {:?}", decks.starts());
    drop(other);

    // Two attempts that were in one conversation, one from each daemon.
    let mut decks = Decks::start();
    std::fs::write(decks.root.join("fixed-session"), "").unwrap();
    let first = decks.start_pane("restore-shared-a-021");
    let second_folder = decks.folder("restore-shared-b-021");
    let second = decks.start_pane_with("restore-shared-b-021", &second_folder, &decks.command());
    let first_attempt = string(&decks.attempt(&string(&first, "agent")), "attempt");
    let second_attempt = string(&decks.attempt(&string(&second, "agent")), "attempt");
    assert_eq!(
        first["session"], second["session"],
        "premise: one conversation"
    );
    decks.restart();
    let other = decks.another_daemon();
    let first_id = string(&decks.item_for(&first_attempt), "id");
    let second_id = string(&decks.item_for(&second_attempt), "id");
    assert_ne!(first_id, second_id);
    std::fs::write(decks.root.join("slow-start"), "3").unwrap();
    let replies = at_once(vec![
        (&decks.live, restore_request(&first_id, &first_attempt)),
        (&other, restore_request(&second_id, &second_attempt)),
    ]);
    let restored: usize = replies
        .iter()
        .map(|r| {
            r["interrupted_restore"]["restored"]
                .as_array()
                .unwrap()
                .len()
        })
        .sum();
    assert_eq!(restored, 1, "one writer into the conversation: {replies:?}");
    let refused: Vec<&Value> = replies
        .iter()
        .flat_map(|r| r["interrupted_restore"]["refused"].as_array().unwrap())
        .collect();
    assert_eq!(refused.len(), 1, "{replies:?}");
    assert_eq!(refused[0]["reason"], "session-in-use", "{replies:?}");
    assert_eq!(decks.starts().len(), 3, "{:?}", decks.starts());
    drop(other);
}

/// Scenario: Start scripted Claudes whose recorded commands continue or resume their conversation directly, and one through a wrapper that adds --continue. After a restart, Start over refuses the direct ones without starting anything; the wrapper's agent comes back in its old conversation, so the deck stops it, sends it nothing, and keeps the item listed.
#[spec("journal/restore/022")]
#[test]
fn journal_restore_022_start_over_refuses_a_launch_that_reopens_the_old_conversation() {
    let mut decks = Decks::start();
    let wrapper = decks.bin.join("claude-continue");
    std::fs::write(&wrapper, "#!/bin/sh\nexec claude --continue \"$@\"\n").unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut attempts = Vec::new();
    for (name, command) in [
        (
            "restore-continue-022",
            "exec claude --continue --model restore-synthetic".to_string(),
        ),
        (
            "restore-resume-022",
            format!("exec claude --resume {SESSION} --model restore-synthetic"),
        ),
        (
            "restore-wrapper-022",
            "exec claude-continue --model restore-synthetic".to_string(),
        ),
    ] {
        let started = decks.start_pane_with(
            name,
            &decks.repo,
            &common::capability_export_command(&command),
        );
        assert_eq!(started["session"], SESSION, "premise: {started}");
        attempts.push(string(
            &decks.attempt(&string(&started, "agent")),
            "attempt",
        ));
    }
    decks.restart();
    for attempt in &attempts[..2] {
        let id = string(&decks.item_for(attempt), "id");
        let cli = decks.person(&[
            "interrupted",
            "start-over",
            &id,
            "--agent",
            attempt,
            "--yes",
        ]);
        exit(&cli, 1);
        assert!(
            text(&cli).contains("reopens an earlier conversation"),
            "{}",
            text(&cli)
        );
        let reply = final_reply(&decks, start_over_request(&id, attempt));
        assert_eq!(start_over_refusal(&reply), "command-reopens");
    }
    assert_eq!(decks.starts().len(), 3, "a refused command starts nothing");

    let id = string(&decks.item_for(&attempts[2]), "id");
    let reply = final_reply(&decks, start_over_request(&id, &attempts[2]));
    assert_eq!(start_over_refusal(&reply), "not-fresh", "{reply}");
    let starts = decks.starts();
    assert_eq!(starts.len(), 4, "the wrapper started once: {starts:?}");
    let originals = [
        "restore-continue-022",
        "restore-resume-022",
        "restore-wrapper-022",
    ];
    let blank = starts
        .iter()
        .find(|s| !originals.contains(&s["pane"].as_str().unwrap()))
        .expect("the started-over pane");
    assert_eq!(
        blank["session"], SESSION,
        "premise: it reopened the old one"
    );
    let blank_id = string(blank, "agent");
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
            !common::agent_records_on(&decks.live.attach_socket)
                .iter()
                .any(|r| r.id == blank_id)
        }),
        "the agent that reopened its conversation is stopped"
    );
    assert!(decks.inbox(None).is_empty(), "nothing was sent to it");
    assert_eq!(
        decks.item_for(&attempts[2])["id"],
        id,
        "the item stays listed"
    );
}

/// Whether `pid` is still the process that started at `start`, and has not
/// merely exited waiting to be reaped.
#[cfg(target_os = "linux")]
fn running(pid: u32, start: u64) -> bool {
    dot_agent_deck::platform::proc::process_start_of(pid) == Some(start)
        && std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .and_then(|(_, rest)| rest.trim_start().chars().next())
            })
            .is_some_and(|state| !matches!(state, 'Z' | 'X'))
}

/// Scenario: After a restart, a scripted Claude restored with its conversation starts a child in another process group and a child in its own group that ignores SIGTERM, then reports a different conversation. The failed restore answers not-resumed only once both children are gone; Start over afterwards starts the agent again.
#[cfg(target_os = "linux")]
#[spec("journal/restore/023")]
#[test]
fn journal_restore_023_a_failed_restore_stops_every_process_it_started() {
    let mut decks = Decks::start();
    let original = decks.start_pane("restore-teardown-023");
    let attempt = string(&decks.attempt(&string(&original, "agent")), "attempt");
    decks.restart();
    for sentinel in ["wrong-session", "survive-stop", "ignore-term-child"] {
        std::fs::write(decks.root.join(sentinel), "").unwrap();
    }
    let id = string(&decks.item_for(&attempt), "id");
    let reply = final_reply(&decks, restore_request(&id, &attempt));
    assert_refused(&reply, &attempt, "not-resumed");
    let mut children = Vec::new();
    for file in ["survivor.json", "term-survivor.json"] {
        let path = decks.root.join(file);
        assert!(path.is_file(), "premise: the restored agent started {file}");
        let child: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        children.push(child["pid"].as_u64().unwrap() as u32);
    }
    // Cleanup for a failure: only the exact children, while they are theirs.
    let _guards: Vec<Survivor> = children
        .iter()
        .filter_map(|&pid| {
            dot_agent_deck::platform::proc::process_start_of(pid)
                .map(|start| Survivor { pid, start })
        })
        .collect();
    for guard in &_guards {
        assert!(
            !running(guard.pid, guard.start),
            "a child of the failed restore survived its stop: {}",
            guard.pid
        );
    }
    assert!(decks.inbox(None).is_empty(), "nothing was sent");
    for sentinel in ["wrong-session", "survive-stop", "ignore-term-child"] {
        std::fs::remove_file(decks.root.join(sentinel)).unwrap();
    }
    let started = decks.person(&[
        "interrupted",
        "start-over",
        &id,
        "--agent",
        &attempt,
        "--yes",
    ]);
    exit(&started, 0);
    common::wait_until(common::child_boot_budget(), || decks.starts().len() >= 3);
    assert_eq!(
        decks.starts().len(),
        3,
        "restore once, then start over once"
    );
}

/// Scenario: After a restart into a daemon whose inbox takes no message as long as the restart note, Restore is refused note-refused before anything starts. After a restart into a daemon whose restart-note post fails once the agent confirmed, the restored agent is stopped again, nothing is sent, and the item stays listed for a restore.
#[spec("journal/restore/024")]
#[test]
fn journal_restore_024_a_restart_note_that_cannot_be_delivered_restores_nothing() {
    let mut decks = Decks::start();
    let original = decks.start_pane("restore-note-024");
    let attempt = string(&decks.attempt(&string(&original, "agent")), "attempt");
    decks.restart_with(&[("DOT_AGENT_DECK_INBOX_MAX_BODY_BYTES", "200")]);
    let id = string(&decks.item_for(&attempt), "id");
    let reply = final_reply(&decks, restore_request(&id, &attempt));
    assert_refused(&reply, &attempt, "note-refused");
    assert_eq!(decks.starts().len(), 1, "admitted first: nothing started");

    let mut decks = Decks::start();
    let original = decks.start_pane("restore-note-024");
    let attempt = string(&decks.attempt(&string(&original, "agent")), "attempt");
    decks.restart_with(&[("DOT_AGENT_DECK_TEST_RESTORE_REFUSE_NOTE", "1")]);
    let id = string(&decks.item_for(&attempt), "id");
    let reply = final_reply(&decks, restore_request(&id, &attempt));
    assert_refused(&reply, &attempt, "note-not-delivered");
    let starts = decks.starts();
    assert_eq!(starts.len(), 2, "the agent came back once: {starts:?}");
    let resumed = starts
        .iter()
        .find(|s| s["pane"] != original["pane"])
        .unwrap();
    assert_eq!(resumed["session"], SESSION, "premise: it confirmed");
    let resumed_id = string(resumed, "agent");
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
            !common::agent_records_on(&decks.live.attach_socket)
                .iter()
                .any(|r| r.id == resumed_id)
        }),
        "an agent without its restart note is stopped again"
    );
    assert!(decks.inbox(None).is_empty(), "nothing was sent");
    let kept = decks.item_for(&attempt);
    assert_eq!(kept["id"], id);
    assert_eq!(
        kept["agents"][0]["restore"],
        json!({"kind":"resume"}),
        "the item stays as it was, for a restore: {kept}"
    );
}

/// Scenario: Restart a scripted Claude, then start a newer agent in the same folder on the new daemon. Start over and Restore of the stopped agent are both refused directory-in-use, nothing more starts, and the item stays listed.
#[spec("journal/restore/025")]
#[test]
fn journal_restore_025_newer_work_in_the_folder_refuses_start_over_and_restore() {
    let mut decks = Decks::start();
    let original = decks.start_pane("restore-folder-025");
    let attempt = string(&decks.attempt(&string(&original, "agent")), "attempt");
    decks.restart();
    let id = string(&decks.item_for(&attempt), "id");
    decks.start_pane("restore-newer-025");
    assert_eq!(decks.starts().len(), 2);
    let reply = final_reply(&decks, start_over_request(&id, &attempt));
    assert_eq!(start_over_refusal(&reply), "directory-in-use", "{reply}");
    let reply = final_reply(&decks, restore_request(&id, &attempt));
    assert_refused(&reply, &attempt, "directory-in-use");
    assert_eq!(decks.starts().len(), 2, "nothing more started");
    assert_eq!(decks.item_for(&attempt)["id"], id);
}
