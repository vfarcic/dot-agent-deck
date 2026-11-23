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
    wrong = os.path.join(root, 'wrong-session')
    if os.path.exists(wrong):
        # The conversation the sentinel names, else the fixture's other one.
        session = open(wrong).read().strip() or __WRONG_SESSION__
elif '--session-id' in args:
    # A fresh start under the id the deck chose.
    session = args[args.index('--session-id') + 1]
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
if not os.path.exists(os.path.join(root, 'no-hook')):
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
    // Changed in round 3 (audit B3): a fresh start of Claude is started
    // under a new id the deck chose, which its first report names.
    let argv = fresh["argv"].as_array().unwrap();
    assert_eq!(argv.len(), 4, "{fresh}");
    assert_eq!(argv[0], "--session-id", "{fresh}");
    assert_eq!(fresh["session"], argv[1], "it reported the new id: {fresh}");
    assert_ne!(argv[1], SESSION, "not the old conversation: {fresh}");
    assert_eq!(
        &argv[2..],
        &[json!("--model"), json!("restore-synthetic")],
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
            Some(
                "already-restoring"
                    | "session-in-use"
                    | "directory-in-use"
                    | "not-listed"
                    | "restore-busy"
            )
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
    // Round 5: one restore acts at a time per state directory, so the
    // other is refused while the first acts, or finds its conversation
    // taken after.
    assert!(
        matches!(
            refused[0]["reason"].as_str(),
            Some("session-in-use" | "restore-busy")
        ),
        "{replies:?}"
    );
    assert_eq!(decks.starts().len(), 3, "{:?}", decks.starts());
    drop(other);
}

/// Scenario: Start scripted Claudes whose recorded commands continue or resume their conversation directly, and one through a wrapper that adds --continue. After a restart, Start over refuses the direct ones without starting anything; the wrapper's agent is started with --continue, so the deck refuses command-reopens, stops it, sends it nothing, and keeps the item listed.
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
    // Changed in round 3 (audit B3): the shim sees the wrapper's real
    // `--continue` and refuses it, rather than the deck noticing the old
    // conversation afterwards.
    assert_eq!(start_over_refusal(&reply), "command-reopens", "{reply}");
    // The refusal is settled when the shim runs, so the deck may stop the
    // wrapper's agent before it got as far as recording its start.
    let starts = decks.starts();
    assert!(
        (3..=4).contains(&starts.len()),
        "the wrapper started at most once: {starts:?}"
    );
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
            common::agent_records_on(&decks.live.attach_socket).is_empty()
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

/// Scenario: Restart a scripted Claude, then start a newer agent in the same folder on the new daemon, by its own path, through a symlink to it, and through a path with `..` in it. Each time, Start over and Restore of the stopped agent are both refused directory-in-use, nothing more starts, and the item stays listed.
#[spec("journal/restore/025")]
#[test]
fn journal_restore_025_newer_work_in_the_folder_refuses_start_over_and_restore() {
    for spelling in ["same", "symlink", "dotdot"] {
        let mut decks = Decks::start();
        let original = decks.start_pane("restore-folder-025");
        let attempt = string(&decks.attempt(&string(&original, "agent")), "attempt");
        decks.restart();
        let id = string(&decks.item_for(&attempt), "id");
        let folder = match spelling {
            "same" => decks.repo.clone(),
            "symlink" => {
                let link = decks.repo.parent().unwrap().join("restore-repo-link");
                std::os::unix::fs::symlink(&decks.repo, &link).unwrap();
                link
            }
            _ => {
                let sub = decks.repo.join("sub");
                std::fs::create_dir_all(&sub).unwrap();
                sub.join("..")
            }
        };
        decks.start_pane_with("restore-newer-025", &folder, &decks.command());
        assert_eq!(decks.starts().len(), 2, "{spelling}");
        let reply = final_reply(&decks, start_over_request(&id, &attempt));
        assert_eq!(
            start_over_refusal(&reply),
            "directory-in-use",
            "{spelling}: {reply}"
        );
        let reply = final_reply(&decks, restore_request(&id, &attempt));
        assert_refused(&reply, &attempt, "directory-in-use");
        assert_eq!(decks.starts().len(), 2, "{spelling}: nothing more started");
        assert_eq!(decks.item_for(&attempt)["id"], id);
    }
}

/// Two agents that were in one conversation, in folders of their own, on
/// the live daemon; answers their attempts after a restart into a daemon
/// started with `extra` in its environment.
fn two_in_one_conversation(decks: &mut Decks, tag: &str, extra: &[(&str, &str)]) -> [String; 2] {
    std::fs::write(decks.root.join("fixed-session"), "").unwrap();
    let first = decks.start_pane(&format!("restore-shared-a-{tag}"));
    let folder = decks.folder(&format!("restore-shared-b-{tag}"));
    let second = decks.start_pane_with(
        &format!("restore-shared-b-{tag}"),
        &folder,
        &decks.command(),
    );
    assert_eq!(
        first["session"], second["session"],
        "premise: one conversation"
    );
    let attempts = [
        string(&decks.attempt(&string(&first, "agent")), "attempt"),
        string(&decks.attempt(&string(&second, "agent")), "attempt"),
    ];
    decks.restart_with(extra);
    attempts
}

/// The number of restored agents in a restore reply.
fn restored_count(reply: &Value) -> usize {
    reply["interrupted_restore"]["restored"]
        .as_array()
        .map_or(0, Vec::len)
}

/// Scenario: Two scripted Claudes were in one conversation in folders of their own. After a restart into a daemon whose journal admits no session report, the first is restored and its request finishes; restoring the second afterwards, from that daemon and from a second daemon on the same state directory, is refused session-in-use and starts nothing. In a daemon whose successor record cannot be written, a restore that came back is stopped again and refused not-recorded, and the item stays listed.
#[spec("journal/restore/026")]
#[test]
fn journal_restore_026_a_finished_restore_keeps_its_conversation_from_a_second_writer() {
    let mut decks = Decks::start();
    let [first, second] = two_in_one_conversation(
        &mut decks,
        "026",
        &[("DOT_AGENT_DECK_TEST_WITHHOLD_SESSIONS", "1")],
    );
    let other = decks.another_daemon();
    let first_id = string(&decks.item_for(&first), "id");
    let second_id = string(&decks.item_for(&second), "id");
    let reply = final_reply(&decks, restore_request(&first_id, &first));
    assert_eq!(restored_count(&reply), 1, "the first comes back: {reply}");
    assert_eq!(
        reply["interrupted_restore"]["restored"][0]["recorded"], true,
        "{reply}"
    );
    let premise = journal::wait_for(&decks.state, "the restore's successor link", |lines| {
        journal::find(lines, "attempt_restored", |_| true).cloned()
    });
    let to = string(&premise, "to");
    assert!(
        journal::all(&journal::lines(&decks.state), "attempt_session", |line| {
            line["attempt"] == to.as_str()
        })
        .is_empty(),
        "premise: the restored agent's own session report was held back"
    );
    assert_eq!(premise["session"], SESSION, "{premise}");
    for daemon in [&decks.live, &other] {
        let reply = final_reply_on(daemon, restore_request(&second_id, &second));
        assert_refused(&reply, &second, "session-in-use");
    }
    assert_eq!(decks.starts().len(), 3, "one writer: {:?}", decks.starts());
    drop(other);

    let mut decks = Decks::start();
    let original = decks.start_pane("restore-edge-026");
    let attempt = string(&decks.attempt(&string(&original, "agent")), "attempt");
    decks.restart_with(&[("DOT_AGENT_DECK_TEST_RESTORE_REFUSE_EDGE", "1")]);
    let id = string(&decks.item_for(&attempt), "id");
    let reply = final_reply(&decks, restore_request(&id, &attempt));
    assert_refused(&reply, &attempt, "not-recorded");
    let resumed = decks
        .starts()
        .into_iter()
        .find(|s| s["pane"] != original["pane"])
        .expect("premise: it came back");
    let resumed_id = string(&resumed, "agent");
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
            !common::agent_records_on(&decks.live.attach_socket)
                .iter()
                .any(|r| r.id == resumed_id)
        }),
        "an agent its restore could not record is stopped again"
    );
    assert_eq!(decks.item_for(&attempt)["id"], id, "the item stays listed");
}

/// Scenario: Two scripted Claudes were in one conversation in folders of their own. After a restart into a daemon that cannot prove a stop, restoring the first comes back in another conversation and leaves a child running in its session, so it is refused not-stopped. Restoring the second, from that daemon and from a second daemon, is then refused session-in-use while the child runs, and restores once the child is gone.
#[cfg(target_os = "linux")]
#[spec("journal/restore/027")]
#[test]
fn journal_restore_027_an_unproven_stop_fences_its_conversation_from_every_item() {
    let mut decks = Decks::start();
    let [first, second] = two_in_one_conversation(
        &mut decks,
        "027",
        &[("DOT_AGENT_DECK_TEST_RESTORE_SKIP_SWEEP", "1")],
    );
    let other = decks.another_daemon();
    let first_id = string(&decks.item_for(&first), "id");
    let second_id = string(&decks.item_for(&second), "id");
    for sentinel in ["wrong-session", "survive-stop"] {
        std::fs::write(decks.root.join(sentinel), "").unwrap();
    }
    let reply = final_reply(&decks, restore_request(&first_id, &first));
    assert_refused(&reply, &first, "not-stopped");
    let child: Value =
        serde_json::from_slice(&std::fs::read(decks.root.join("survivor.json")).unwrap()).unwrap();
    let pid = child["pid"].as_u64().unwrap() as u32;
    let start = dot_agent_deck::platform::proc::process_start_of(pid).expect("premise: it runs");
    let survivor = Survivor { pid, start };
    for sentinel in ["wrong-session", "survive-stop"] {
        std::fs::remove_file(decks.root.join(sentinel)).unwrap();
    }
    for daemon in [&decks.live, &other] {
        let reply = final_reply_on(daemon, restore_request(&second_id, &second));
        assert_refused(&reply, &second, "session-in-use");
    }
    assert_eq!(decks.starts().len(), 3, "nothing more started");
    assert!(decks.inbox(None).is_empty(), "nothing was sent");
    drop(survivor);
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
            !running(pid, start)
        }),
        "the test's own child is gone"
    );
    let reply = final_reply_on(&other, restore_request(&second_id, &second));
    assert_eq!(
        restored_count(&reply),
        1,
        "the fence lifts once it is gone: {reply}"
    );
    drop(other);
}

/// Scenario: After a restart, Start over is asked of a scripted agent whose recorded command continues or opens an OpenCode conversation, of ones that reach Codex's resume through shell syntax, and of a Claude with no recorded conversation launched through wrappers. Commands the deck can read are refused command-reopens before anything starts; the launches only the real argument list shows are refused command-reopens once the agent is started with them; a wrapper that bypasses the check is refused not-fresh. Each started agent is stopped, nothing is sent, and every item stays listed.
#[spec("journal/restore/028")]
#[test]
fn journal_restore_028_start_over_checks_the_arguments_the_agent_really_gets() {
    let mut decks = Decks::start();
    for name in ["opencode", "codex"] {
        std::os::unix::fs::symlink(decks.bin.join("claude"), decks.bin.join(name)).unwrap();
    }
    for (name, body) in [
        (
            "opencode-session",
            "exec opencode --session ses_028 \"$@\"\n".to_string(),
        ),
        (
            "claude-continue",
            "exec claude --continue \"$@\"\n".to_string(),
        ),
        (
            "claude-direct",
            format!("exec {} \"$@\"\n", decks.bin.join("claude").display()),
        ),
    ] {
        let path = decks.bin.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let pane = |decks: &Decks, name: &str, agent: &str, command: &str| {
        let folder = decks.folder(name);
        let reply = inbox::attach(
            &decks.live,
            json!({
                "op":"start-agent", "command":common::capability_export_command(command),
                "cwd":folder, "display_name":name, "agent_type":agent, "rows":40, "cols":140,
                "env":[["DOT_AGENT_DECK_PANE_ID", name]]
            }),
        );
        assert_eq!(reply["ok"], true, "fixture launch: {reply}");
        let agent_id = string(&reply, "id");
        decks.wait_start(name);
        string(&decks.attempt(&agent_id), "attempt")
    };
    let early = [
        pane(
            &decks,
            "restore-oc-continue-028",
            "open_code",
            "exec opencode --continue",
        ),
        pane(
            &decks,
            "restore-oc-session-028",
            "open_code",
            "exec opencode --session ses_1",
        ),
        pane(
            &decks,
            "restore-codex-sub-028",
            "claude_code",
            "(codex resume --last)",
        ),
        pane(
            &decks,
            "restore-codex-seq-028",
            "claude_code",
            "true;codex resume --last",
        ),
    ];
    let at_exec = [
        pane(
            &decks,
            "restore-oc-wrapper-028",
            "open_code",
            "exec opencode-session",
        ),
        pane(
            &decks,
            "restore-codex-var-028",
            "claude_code",
            "c=codex; $c resume --last",
        ),
    ];
    std::fs::write(decks.root.join("no-hook"), "").unwrap();
    let unknown = [
        pane(
            &decks,
            "restore-claude-wrapper-028",
            "claude_code",
            "exec claude-continue",
        ),
        pane(
            &decks,
            "restore-claude-direct-028",
            "claude_code",
            "exec claude-direct",
        ),
    ];
    std::fs::remove_file(decks.root.join("no-hook")).unwrap();
    decks.restart();
    let before = decks.starts().len();
    for attempt in &early {
        let item = decks.item_for(attempt);
        let reply = final_reply(&decks, start_over_request(&string(&item, "id"), attempt));
        assert_eq!(
            start_over_refusal(&reply),
            "command-reopens",
            "{item}: {reply}"
        );
    }
    assert_eq!(
        decks.starts().len(),
        before,
        "a command it can read starts nothing"
    );
    for (attempt, reason) in [
        (&at_exec[0], "command-reopens"),
        (&at_exec[1], "command-reopens"),
        (&unknown[0], "command-reopens"),
        (&unknown[1], "not-fresh"),
    ] {
        let item = decks.item_for(attempt);
        if unknown.contains(attempt) {
            assert_eq!(
                item["agents"][0]["restore"]["reason"], "no-session-id",
                "premise: no recorded conversation: {item}"
            );
        }
        let reply = final_reply(&decks, start_over_request(&string(&item, "id"), attempt));
        assert_eq!(start_over_refusal(&reply), reason, "{item}: {reply}");
        assert_eq!(
            decks.item_for(attempt)["id"],
            item["id"],
            "the item stays listed"
        );
    }
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
            common::agent_records_on(&decks.live.attach_socket).is_empty()
        }),
        "every agent a refused Start over started is stopped"
    );
    assert!(decks.inbox(None).is_empty(), "nothing was sent");
}

/// Scenario: Restart a dispatched single-agent unit whose branch has no pull request, and hold a Start over after it has read the item. A pull request opens on the branch meanwhile; released, the Start over reads Git and GitHub again with its claims held, refuses pr-open, and starts nothing.
#[spec("journal/restore/029")]
#[test]
fn journal_restore_029_start_over_reads_github_again_under_its_claims() {
    let mut decks = Decks::start();
    let gh = decks.bin.join("gh");
    let flag = decks.root.join("pr-open");
    std::fs::write(
        &gh,
        format!(
            "#!/usr/bin/env python3\nimport json, os, sys\nargs = sys.argv[1:]\nbranch = args[args.index('--head') + 1] if '--head' in args else ''\nopen_pr = os.path.exists({flag:?})\nprint(json.dumps([{{'state':'OPEN','headRefName':branch,'isCrossRepository':False}}] if open_pr else []))\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let git = |args: &[&str]| {
        let out = common::fixture_git(&decks.repo, decks.repo.parent().unwrap())
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", text(&out));
    };
    git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/example/restore-029.git",
    ]);
    let caller = route::start(
        &decks.live,
        "restore-dispatcher-029",
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
            "restore-pr-029",
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
        common::wait_until(common::load_scaled(Duration::from_secs(20)), || {
            route::messages(&decks.live, &caller).len() > before
        }),
        "dispatch must finish before restart"
    );
    let unit = decks.wait_start(&string(&decks.starts()[0], "pane"));
    let attempt = string(&decks.attempt(&string(&unit, "agent")), "attempt");
    let barrier = decks.root.join("start-over-barrier");
    let reached = barrier.with_extension("reached");
    decks.restart_with(&[(
        "DOT_AGENT_DECK_TEST_RESTORE_BARRIER",
        barrier.to_str().unwrap(),
    )]);
    drop(caller);
    let item = decks.item_for(&attempt);
    assert_eq!(item["kind"], "unit", "{item}");
    let id = string(&item, "id");
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
    std::fs::write(&flag, "").unwrap();
    std::fs::remove_file(&barrier).unwrap();
    let late = held.join().unwrap();
    assert_eq!(start_over_refusal(&late), "pr-open", "{late}");
    assert_eq!(decks.starts().len(), 1, "nothing more started");
    assert!(decks.inbox(None).is_empty(), "nothing was sent");
}

/// The survivor child a restore left in its session, from the sentinel file
/// `file` it wrote.
#[cfg(target_os = "linux")]
fn survivor(decks: &Decks, file: &str) -> Survivor {
    let path = decks.root.join(file);
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(10)), || path
            .is_file()),
        "the survivor child never reported itself"
    );
    let child: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let pid = child["pid"].as_u64().unwrap() as u32;
    let start = dot_agent_deck::platform::proc::process_start_of(pid).expect("premise: it runs");
    Survivor { pid, start }
}

/// Scenario: Two scripted Claudes ran in conversations of their own, in folders of their own. After a restart into a daemon that can neither prove a stop nor write a failure record, restoring the first comes back in the second one's conversation and leaves a child running in its session, so it is refused not-stopped. Restoring the second, from that daemon and from a second daemon, is then refused session-in-use, and nothing more starts.
#[cfg(target_os = "linux")]
#[spec("journal/restore/030")]
#[test]
fn journal_restore_030_an_unrecorded_unproven_stop_fences_the_conversation_it_reported() {
    let mut decks = Decks::start();
    let first = decks.start_pane("restore-reported-a-030");
    let folder = decks.folder("restore-reported-b-030");
    let second = decks.start_pane_with(
        "restore-reported-b-030",
        &folder,
        &common::capability_export_command(&format!(
            "exec claude --model restore-synthetic --resume {WRONG_SESSION}"
        )),
    );
    assert_eq!(first["session"], SESSION, "premise: {first}");
    assert_eq!(second["session"], WRONG_SESSION, "premise: {second}");
    let first = string(&decks.attempt(&string(&first, "agent")), "attempt");
    let second = string(&decks.attempt(&string(&second, "agent")), "attempt");
    decks.restart_with(&[
        ("DOT_AGENT_DECK_TEST_RESTORE_SKIP_SWEEP", "1"),
        ("DOT_AGENT_DECK_TEST_RESTORE_REFUSE_FAILURE", "1"),
    ]);
    let other = decks.another_daemon();
    let first_id = string(&decks.item_for(&first), "id");
    let second_id = string(&decks.item_for(&second), "id");
    assert_eq!(
        decks.item_for(&second)["agents"][0]["restore"]["kind"],
        "resume",
        "premise: the second can be restored"
    );
    // The first comes back in the second's conversation and leaves a child.
    for sentinel in ["wrong-session", "survive-stop"] {
        std::fs::write(decks.root.join(sentinel), "").unwrap();
    }
    let reply = final_reply(&decks, restore_request(&first_id, &first));
    assert_refused(&reply, &first, "not-stopped");
    let survivor = survivor(&decks, "survivor.json");
    for sentinel in ["wrong-session", "survive-stop"] {
        std::fs::remove_file(decks.root.join(sentinel)).unwrap();
    }
    assert!(
        journal::all(&journal::lines(&decks.state), "restore_failed", |_| true).is_empty(),
        "premise: the failure could not be recorded"
    );
    for daemon in [&decks.live, &other] {
        let reply = final_reply_on(daemon, restore_request(&second_id, &second));
        assert_refused(&reply, &second, "session-in-use");
    }
    assert_eq!(decks.starts().len(), 3, "nothing more started");
    assert!(decks.inbox(None).is_empty(), "nothing was sent");
    drop(survivor);
    drop(other);
}

/// Scenario: A scripted Claude whose own command continues its conversation and a second one in another conversation ran in folders of their own. After a restart into a daemon that cannot prove a stop, restoring the first is refused by the launch shim, reports no conversation and leaves a child running, so it is refused not-stopped. Restoring the second, in a conversation the first never named, from that daemon and from a second daemon, is refused session-in-use while the child runs, and restores once it is gone.
#[cfg(target_os = "linux")]
#[spec("journal/restore/031")]
#[test]
fn journal_restore_031_a_survivor_in_an_unknown_conversation_fences_its_kind() {
    let mut decks = Decks::start();
    // With `survive-early`, a capped child in the launch's session but out
    // of its process group, started before the agent: the launch shim's
    // refusal is seen at once, before the agent itself could start one.
    let fork = decks.bin.join("fork-survivor");
    std::fs::write(
        &fork,
        format!(
            r#"#!/usr/bin/env python3
import json, os, signal, time
root = {root}
if os.path.exists(os.path.join(root, 'survive-early')) and os.fork() == 0:
    os.setpgid(0, 0)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    signal.signal(signal.SIGALRM, lambda *_: os._exit(0))
    signal.alarm(180)
    for fd in (0, 1, 2):
        os.close(fd)
    tmp = os.path.join(root, 'early-survivor.json.tmp')
    with open(tmp, 'w') as f:
        json.dump({{'pid': os.getpid(), 'sid': os.getsid(0)}}, f)
    os.replace(tmp, os.path.join(root, 'early-survivor.json'))
    while True:
        time.sleep(0.1)
"#,
            root = serde_json::to_string(&decks.root).unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fork, std::fs::Permissions::from_mode(0o700)).unwrap();
    let first = decks.start_pane_with(
        "restore-unknown-a-031",
        &decks.repo,
        &common::capability_export_command(
            "fork-survivor; exec claude --model restore-synthetic --continue",
        ),
    );
    // Every later fresh start is in a conversation of its own.
    std::fs::write(decks.root.join("session-taken"), "").unwrap();
    let folder = decks.folder("restore-unknown-b-031");
    let second = decks.start_pane_with("restore-unknown-b-031", &folder, &decks.command());
    assert_ne!(
        first["session"], second["session"],
        "premise: two conversations"
    );
    let first = string(&decks.attempt(&string(&first, "agent")), "attempt");
    let second = string(&decks.attempt(&string(&second, "agent")), "attempt");
    decks.restart_with(&[("DOT_AGENT_DECK_TEST_RESTORE_SKIP_SWEEP", "1")]);
    let other = decks.another_daemon();
    let first_id = string(&decks.item_for(&first), "id");
    let second_id = string(&decks.item_for(&second), "id");
    // The shim refuses the first's `--continue` beside the resume; the agent
    // runs on with its own arguments, reports nothing and leaves a child.
    for sentinel in ["no-hook", "survive-early"] {
        std::fs::write(decks.root.join(sentinel), "").unwrap();
    }
    let reply = final_reply(&decks, restore_request(&first_id, &first));
    assert_refused(&reply, &first, "not-stopped");
    let survivor = survivor(&decks, "early-survivor.json");
    for sentinel in ["no-hook", "survive-early"] {
        std::fs::remove_file(decks.root.join(sentinel)).unwrap();
    }
    let failed = journal::wait_for(&decks.state, "the failure record", |lines| {
        journal::find(lines, "restore_failed", |_| true).cloned()
    });
    assert_eq!(failed["failure"], "refused", "{failed}");
    assert_eq!(failed["unknown_session"], true, "{failed}");
    // The shim's refusal can stop the launch before the stand-in recorded
    // its start, so what counts is that nothing starts after it.
    let before = decks.starts().len();

    for daemon in [&decks.live, &other] {
        let reply = final_reply_on(daemon, restore_request(&second_id, &second));
        assert_refused(&reply, &second, "session-in-use");
    }
    assert_eq!(decks.starts().len(), before, "nothing more started");
    assert!(decks.inbox(None).is_empty(), "nothing was sent");
    let (pid, start) = (survivor.pid, survivor.start);
    drop(survivor);
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
            !running(pid, start)
        }),
        "the test's own child is gone"
    );
    let reply = final_reply_on(&other, restore_request(&second_id, &second));
    assert_eq!(
        restored_count(&reply),
        1,
        "the fence lifts once it is gone: {reply}"
    );
    drop(other);
}

/// Scenario: A scripted Claude runs through a script of the user's named `claude`, ahead of the agent on `PATH`, that starts the agent by its full path in an earlier conversation. After a restart, Start over reaches that script, which hands the deck's new conversation id on beside its own choice; the agent reports the earlier conversation, so Start over is refused not-fresh, the agent is stopped, nothing is sent and the item stays listed.
#[spec("journal/restore/032")]
#[test]
fn journal_restore_032_a_same_name_claude_wrapper_is_checked_by_its_conversation() {
    let mut decks = Decks::start();
    let wrap = decks.root.parent().unwrap().join("wrap");
    std::fs::create_dir_all(&wrap).unwrap();
    std::fs::set_permissions(&wrap, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        wrap.join("claude"),
        format!(
            "#!/bin/sh\nexec {} --resume {SESSION} \"$@\"\n",
            decks.bin.join("claude").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(wrap.join("claude"), std::fs::Permissions::from_mode(0o700)).unwrap();
    decks.path = format!("{}:{}", wrap.display(), decks.path);
    decks.restart();
    let original = decks.start_pane("restore-same-name-032");
    assert_eq!(original["session"], SESSION, "premise: {original}");
    let attempt = string(&decks.attempt(&string(&original, "agent")), "attempt");
    decks.restart();
    let item = decks.item_for(&attempt);
    let id = string(&item, "id");
    let reply = final_reply(&decks, start_over_request(&id, &attempt));
    assert_eq!(start_over_refusal(&reply), "not-fresh", "{reply}");
    let started = decks
        .starts()
        .into_iter()
        .find(|s| s["pane"] != original["pane"])
        .expect("premise: Start over started it");
    let argv: Vec<String> = started["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_string())
        .collect();
    assert!(
        argv.iter().any(|a| a == "--session-id") && argv.iter().any(|a| a == "--resume"),
        "premise: the shim reached the wrapper, which kept its own choice: {argv:?}"
    );
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
            common::agent_records_on(&decks.live.attach_socket).is_empty()
        }),
        "the agent is stopped"
    );
    assert!(decks.inbox(None).is_empty(), "nothing was sent");
    assert_eq!(decks.item_for(&attempt)["id"], id, "the item stays listed");
}

/// Send `request` to the attach socket `socket` on a thread of its own, and
/// answer its reply when joined: a request a test holds at a barrier.
fn send_held(socket: PathBuf, request: Value) -> std::thread::JoinHandle<Value> {
    std::thread::spawn(move || {
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
    })
}

/// Scenario: Two scripted Claudes ran in conversations and folders of their own, and two daemons share the state directory after a restart. While the first daemon's restore of the first agent is held just after it took the restore lock, the second daemon refuses a restore and a Start over of the second agent as a whole with restore-busy and starts nothing, while its dry run still says it would restore it. Released, the first restore brings its agent back, and the second daemon's restore then brings the second back.
#[spec("journal/restore/033")]
#[test]
fn journal_restore_033_one_restore_acts_at_a_time_across_daemons() {
    let mut decks = Decks::start();
    let first = decks.start_pane("restore-busy-a-033");
    // Every later fresh start is in a conversation of its own.
    std::fs::write(decks.root.join("session-taken"), "").unwrap();
    let folder = decks.folder("restore-busy-b-033");
    let second = decks.start_pane_with("restore-busy-b-033", &folder, &decks.command());
    assert_ne!(first["session"], second["session"], "premise");
    let first = string(&decks.attempt(&string(&first, "agent")), "attempt");
    let second = string(&decks.attempt(&string(&second, "agent")), "attempt");
    let barrier = decks.root.join("lock-barrier");
    let reached = barrier.with_extension("reached");
    decks.restart_with(&[(
        "DOT_AGENT_DECK_TEST_RESTORE_LOCK_BARRIER",
        barrier.to_str().unwrap(),
    )]);
    let other = decks.another_daemon();
    let first_id = string(&decks.item_for(&first), "id");
    let second_id = string(&decks.item_for(&second), "id");
    std::fs::write(&barrier, "").unwrap();
    let held = send_held(
        decks.live.attach_socket.clone(),
        restore_request(&first_id, &first),
    );
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(20)), || reached
            .is_file()),
        "the first restore must reach its barrier holding the restore lock"
    );
    let busy = final_reply_on(&other, restore_request(&second_id, &second));
    assert_whole_refusal(&busy, "restore-busy");
    assert!(
        busy["interrupted_restore"]["refused"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("in progress")),
        "{busy}"
    );
    let busy = final_reply_on(&other, start_over_request(&second_id, &second));
    assert_eq!(start_over_refusal(&busy), "restore-busy", "{busy}");
    let mut dry = restore_request(&second_id, &second);
    dry["dry_run"] = json!(true);
    let dry = final_reply_on(&other, dry);
    assert_eq!(
        dry["interrupted_restore"]["would_restore"],
        json!([second]),
        "a dry run takes no lock: {dry}"
    );
    assert_eq!(decks.starts().len(), 2, "nothing started meanwhile");
    std::fs::remove_file(&barrier).unwrap();
    let restored = held.join().unwrap();
    assert_eq!(restored_count(&restored), 1, "{restored}");
    let reply = final_reply_on(&other, restore_request(&second_id, &second));
    assert_eq!(restored_count(&reply), 1, "free again: {reply}");
    assert_eq!(decks.starts().len(), 4, "{:?}", decks.starts());
    drop(other);
}

/// Scenario: Two scripted Claudes ran in conversations of their own, in folders of their own. After a restart into a daemon that can neither prove a stop nor write a failure record, restoring the first comes back in the second one's conversation and leaves a child running in its session, so it is refused not-stopped. After that daemon restarts, restoring the second, from the new daemon and from a second one on the same state directory, is still refused session-in-use while the child runs, and restores once the child is gone.
#[cfg(target_os = "linux")]
#[spec("journal/restore/034")]
#[test]
fn journal_restore_034_an_unrecorded_fence_survives_its_daemon() {
    let mut decks = Decks::start();
    let first = decks.start_pane("restore-durable-a-034");
    // Every later fresh start is in a conversation of its own.
    std::fs::write(decks.root.join("session-taken"), "").unwrap();
    let folder = decks.folder("restore-durable-b-034");
    let second = decks.start_pane_with("restore-durable-b-034", &folder, &decks.command());
    assert_eq!(first["session"], SESSION, "premise: {first}");
    let reported = string(&second, "session");
    assert_ne!(reported, SESSION, "premise: {second}");
    let first = string(&decks.attempt(&string(&first, "agent")), "attempt");
    let second = string(&decks.attempt(&string(&second, "agent")), "attempt");
    decks.restart_with(&[
        ("DOT_AGENT_DECK_TEST_RESTORE_SKIP_SWEEP", "1"),
        ("DOT_AGENT_DECK_TEST_RESTORE_REFUSE_FAILURE", "1"),
    ]);
    let other = decks.another_daemon();
    let first_id = string(&decks.item_for(&first), "id");
    let second_id = string(&decks.item_for(&second), "id");
    // The first comes back in the second's conversation and leaves a child.
    std::fs::write(decks.root.join("wrong-session"), &reported).unwrap();
    std::fs::write(decks.root.join("survive-stop"), "").unwrap();
    let reply = final_reply(&decks, restore_request(&first_id, &first));
    assert_refused(&reply, &first, "not-stopped");
    let survivor = survivor(&decks, "survivor.json");
    for sentinel in ["wrong-session", "survive-stop"] {
        std::fs::remove_file(decks.root.join(sentinel)).unwrap();
    }
    assert!(
        journal::all(&journal::lines(&decks.state), "restore_failed", |_| true).is_empty(),
        "premise: the failure could not be recorded"
    );
    // The daemon that kept the fence goes away; the fence does not.
    decks.restart();
    assert!(
        running(survivor.pid, survivor.start),
        "premise: it outlives the daemon"
    );
    for daemon in [&decks.live, &other] {
        let reply = final_reply_on(daemon, restore_request(&second_id, &second));
        assert_refused(&reply, &second, "session-in-use");
    }
    assert_eq!(decks.starts().len(), 3, "nothing more started");
    assert!(decks.inbox(None).is_empty(), "nothing was sent");
    let (pid, start) = (survivor.pid, survivor.start);
    drop(survivor);
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
            !running(pid, start)
        }),
        "the test's own child is gone"
    );
    let reply = final_reply(&decks, restore_request(&second_id, &second));
    assert_eq!(
        restored_count(&reply),
        1,
        "the fence lifts once it is gone: {reply}"
    );
    drop(other);
}

/// The fence markers in the state directory's claims folder, as written.
fn fence_markers(state: &Path) -> Vec<Value> {
    let dir = state.join("restore-claims");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.expect("a claims folder entry"))
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with("fence-") && name.ends_with(".json")
        })
        .map(|entry| serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap())
        .collect()
}

/// Scenario: Two scripted Claudes ran in conversations of their own, in folders of their own. After a restart into a daemon that can neither prove a stop, record a failure, nor rewrite a fence it placed, restoring the first comes back in the second one's conversation and leaves a child running, so it is refused not-stopped, and only the fence written before the agent started is on disk. After that daemon restarts with the child still running, restoring the second from the new daemon and from a second one is refused session-in-use, and once the child is gone it restores.
#[cfg(target_os = "linux")]
#[spec("journal/restore/035")]
#[test]
fn journal_restore_035_a_fence_written_before_the_spawn_survives_every_later_write_failing() {
    let mut decks = Decks::start();
    let first = decks.start_pane("restore-prespawn-a-035");
    // Every later fresh start is in a conversation of its own.
    std::fs::write(decks.root.join("session-taken"), "").unwrap();
    let folder = decks.folder("restore-prespawn-b-035");
    let second = decks.start_pane_with("restore-prespawn-b-035", &folder, &decks.command());
    assert_eq!(first["session"], SESSION, "premise: {first}");
    let reported = string(&second, "session");
    assert_ne!(reported, SESSION, "premise: {second}");
    let first = string(&decks.attempt(&string(&first, "agent")), "attempt");
    let second = string(&decks.attempt(&string(&second, "agent")), "attempt");
    decks.restart_with(&[
        ("DOT_AGENT_DECK_TEST_RESTORE_SKIP_SWEEP", "1"),
        ("DOT_AGENT_DECK_TEST_RESTORE_REFUSE_FAILURE", "1"),
        ("DOT_AGENT_DECK_TEST_RESTORE_REFUSE_NARROW", "1"),
    ]);
    let other = decks.another_daemon();
    let first_id = string(&decks.item_for(&first), "id");
    let second_id = string(&decks.item_for(&second), "id");
    // The first comes back in the second's conversation and leaves a child.
    std::fs::write(decks.root.join("wrong-session"), &reported).unwrap();
    std::fs::write(decks.root.join("survive-stop"), "").unwrap();
    let reply = final_reply(&decks, restore_request(&first_id, &first));
    assert_refused(&reply, &first, "not-stopped");
    let survivor = survivor(&decks, "survivor.json");
    for sentinel in ["wrong-session", "survive-stop"] {
        std::fs::remove_file(decks.root.join(sentinel)).unwrap();
    }
    assert!(
        journal::all(&journal::lines(&decks.state), "restore_failed", |_| true).is_empty(),
        "premise: the failure could not be recorded"
    );
    let markers = fence_markers(&decks.state);
    assert_eq!(markers.len(), 1, "premise: one fence: {markers:?}");
    let marker = &markers[0];
    assert!(
        marker["pid"].is_null() && marker["to"].is_null(),
        "premise: never narrowed to the agent it started: {marker}"
    );
    assert!(marker["pane"].is_string(), "{marker}");
    assert_eq!(marker["unknown"], true, "{marker}");
    assert_eq!(marker["sessions"], json!([SESSION]), "{marker}");
    // The daemon that ran the restore goes away; the fence does not.
    decks.restart();
    assert!(
        running(survivor.pid, survivor.start),
        "premise: it outlives the daemon"
    );
    for daemon in [&decks.live, &other] {
        let reply = final_reply_on(daemon, restore_request(&second_id, &second));
        assert_refused(&reply, &second, "session-in-use");
    }
    assert_eq!(decks.starts().len(), 3, "nothing more started");
    assert!(decks.inbox(None).is_empty(), "nothing was sent");
    let (pid, start) = (survivor.pid, survivor.start);
    drop(survivor);
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
            !running(pid, start)
        }),
        "the test's own child is gone"
    );
    let reply = final_reply_on(&other, restore_request(&second_id, &second));
    assert_eq!(
        restored_count(&reply),
        1,
        "the fence lifts once its agent is gone: {reply}"
    );
    assert!(fence_markers(&decks.state).is_empty(), "and is removed");
    drop(other);
}
