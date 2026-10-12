//! PRD #1666: the `dot-agent-deck inbox …` commands an agent (or its hook,
//! plugin or extension) runs inside its pane to reach its inbox over the hook
//! socket.
//!
//! Every command identifies the caller by the pane id, agent id and hook
//! capability token the deck put in the pane's environment; the daemon acts
//! only for the agent the token attests.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use crate::agent_pty::{DOT_AGENT_DECK_AGENT_ID, DOT_AGENT_DECK_PANE_ID};
use crate::event::{
    DaemonMessage, InboxComponent, InboxFetchConsumer, InboxFetchRequest, InboxListReply,
    InboxListRequest, InboxWaitMode, InboxWaitReply, InboxWaitRequest, InboxWaitSession,
};
use crate::inbox_hub::AGENT_FRAMING;

/// The most of a hook's stdin `inbox wait` reads.
const MAX_HOOK_STDIN_BYTES: u64 = 1024 * 1024;

/// How long `inbox wait` waits for its hook's stdin to close. Hook runners
/// write the JSON and close; one that does not must not stop the waiter arming.
const HOOK_STDIN_WAIT: Duration = Duration::from_secs(2);

/// The pane, agent and token this process was started with; `None` outside a
/// deck pane.
struct Identity {
    pane_id: String,
    agent_id: Option<String>,
    token: Option<String>,
}

fn identity() -> Option<Identity> {
    let pane_id = std::env::var(DOT_AGENT_DECK_PANE_ID)
        .ok()
        .filter(|v| !v.is_empty())?;
    Some(Identity {
        pane_id,
        agent_id: std::env::var(DOT_AGENT_DECK_AGENT_ID)
            .ok()
            .filter(|v| !v.is_empty()),
        token: crate::hook_provenance::token_from_env(),
    })
}

const NOT_IN_PANE: &str = "Error: DOT_AGENT_DECK_PANE_ID environment variable not set.\nThis command should be run from within a dot-agent-deck managed pane.";

/// The component that speaks for `agent` (a `--agent` value).
fn component_for(agent: &str) -> InboxComponent {
    match agent {
        "claude-code" | "claude_code" | "claude" => InboxComponent::ClaudeWaiter,
        "opencode" => InboxComponent::OpenCodePlugin,
        "pi" => InboxComponent::PiExtension,
        "codex" => InboxComponent::CodexHooks,
        "devin" => InboxComponent::DevinHooks,
        _ => InboxComponent::Other,
    }
}

/// The hook JSON on stdin, read in the background and abandoned if the hook
/// runner never closes it.
fn read_hook_session() -> Option<InboxWaitSession> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::stdin()
            .take(MAX_HOOK_STDIN_BYTES)
            .read_to_string(&mut text);
        let _ = tx.send(text);
    });
    let text = rx.recv_timeout(HOOK_STDIN_WAIT).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let field = |name: &str| value.get(name).and_then(|v| v.as_str()).map(str::to_string);
    Some(InboxWaitSession {
        source: field("source"),
        session_id: field("session_id"),
    })
}

fn parent_pid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(std::os::unix::process::parent_id())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// `dot-agent-deck inbox wait --agent <agent> [--stream]`.
///
/// **Once** (Claude Code's `asyncRewake` hook): blocks until the deck hands
/// over messages, then writes their blocks to stderr and exits 2, which is what
/// wakes Claude. Everything else — run outside a deck, no daemon, an older
/// daemon, superseded by a newer waiter, refused — exits 0 with no output, so a
/// global hook never disturbs a session the deck does not manage.
///
/// **Stream** (the OpenCode plugin, the Pi extension): prints each handed-over
/// message as one JSON line on stdout and forwards `{"ack":[…]}` /
/// `{"nack":[…]}` lines from stdin to the deck, until either side closes.
pub fn wait(agent: &str, stream: bool) -> ExitCode {
    let Some(identity) = identity() else {
        return ExitCode::SUCCESS;
    };
    if identity.token.is_none() {
        return ExitCode::SUCCESS;
    }
    let session = if stream { None } else { read_hook_session() };
    let request = DaemonMessage::InboxWait(InboxWaitRequest {
        pane_id: identity.pane_id,
        agent_id: identity.agent_id,
        token: identity.token,
        component: component_for(agent),
        component_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        mode: if stream {
            InboxWaitMode::Stream
        } else {
            InboxWaitMode::Once
        },
        session,
        pid: parent_pid(),
    });
    let Ok(json) = serde_json::to_string(&request) else {
        return ExitCode::SUCCESS;
    };
    if stream {
        wait_stream(&json)
    } else {
        wait_once(&json)
    }
}

/// How often a Once waiter checks that the agent that ran it is still there.
#[cfg(unix)]
const ORPHAN_POLL: Duration = Duration::from_secs(1);

/// PRD #1666 S6 (design §6.5): a Once waiter exits when the agent that ran it
/// is gone. Its stdin is the hook JSON, closed as soon as it is read, so EOF
/// cannot say so; an orphaned waiter would otherwise keep the dead generation
/// "armed" and hold off the doorbell. The waiter watches its parent and, on
/// Linux, its parent's parent — Claude Code runs a hook through `sh -c`, which
/// may or may not `exec` it — and exits 0, unwoken, when either changes.
#[cfg(unix)]
fn exit_when_orphaned() {
    let ancestry = || {
        let parent = std::os::unix::process::parent_id();
        (parent, parent_of(parent))
    };
    let started = ancestry();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(ORPHAN_POLL);
            if ancestry() != started {
                std::process::exit(0);
            }
        }
    });
}

#[cfg(not(unix))]
fn exit_when_orphaned() {}

/// The parent of process `pid`, from `/proc/<pid>/stat`; `None` once it is
/// gone or where there is no `/proc`.
#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid …`; `comm` may itself hold spaces or parentheses.
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn parent_of(_pid: u32) -> Option<u32> {
    None
}

fn wait_once(json: &str) -> ExitCode {
    exit_when_orphaned();
    let path = crate::endpoint_resolve::client_socket_path();
    let Ok(mut socket) = crate::platform::ipc::IpcClient::connect(&path) else {
        return ExitCode::SUCCESS;
    };
    if socket
        .write_all(format!("{json}\n").as_bytes())
        .and_then(|()| socket.flush())
        .is_err()
    {
        return ExitCode::SUCCESS;
    }
    let mut reader = BufReader::new(socket);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return ExitCode::SUCCESS,
            Ok(_) => {}
        }
        let Ok(reply) = serde_json::from_str::<InboxWaitReply>(&line) else {
            return ExitCode::SUCCESS;
        };
        if reply.keepalive {
            continue;
        }
        let Some(messages) = reply.messages.filter(|m| !m.is_empty()) else {
            return ExitCode::SUCCESS;
        };
        let text = messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut stderr = std::io::stderr().lock();
        return if writeln!(stderr, "{text}")
            .and_then(|()| stderr.flush())
            .is_ok()
        {
            ExitCode::from(2)
        } else {
            // Nothing reached Claude; the deck's ack-wait hands the messages
            // over again.
            ExitCode::FAILURE
        };
    }
}

#[cfg(unix)]
fn wait_stream(json: &str) -> ExitCode {
    let path = crate::endpoint_resolve::client_socket_path();
    let Ok(socket) = std::os::unix::net::UnixStream::connect(&path) else {
        return ExitCode::SUCCESS;
    };
    let Ok(mut writer) = socket.try_clone() else {
        return ExitCode::SUCCESS;
    };
    if writer
        .write_all(format!("{json}\n").as_bytes())
        .and_then(|()| writer.flush())
        .is_err()
    {
        return ExitCode::SUCCESS;
    }
    // Acks and nacks from the component, forwarded as they come.
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            if writer
                .write_all(format!("{}\n", line.trim()).as_bytes())
                .and_then(|()| writer.flush())
                .is_err()
            {
                break;
            }
        }
    });
    let reader = BufReader::new(socket);
    let mut stdout = std::io::stdout().lock();
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        let Ok(reply) = serde_json::from_str::<InboxWaitReply>(&line) else {
            continue;
        };
        if reply.keepalive {
            continue;
        }
        let Some(messages) = reply.messages else {
            // Superseded, refused or not in a deck: nothing more will come.
            break;
        };
        for message in messages {
            let Ok(line) = serde_json::to_string(&message) else {
                continue;
            };
            if writeln!(stdout, "{line}")
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return ExitCode::SUCCESS;
            }
        }
    }
    ExitCode::SUCCESS
}

#[cfg(not(unix))]
fn wait_stream(_json: &str) -> ExitCode {
    eprintln!("inbox wait --stream is not supported on this platform.");
    ExitCode::FAILURE
}

/// Send one inbox request and read its one-line answer.
fn request(message: &DaemonMessage) -> Result<String, String> {
    let json = serde_json::to_string(message).map_err(|e| e.to_string())?;
    crate::hook::request_from_socket(&json).ok_or_else(|| {
        "the deck did not answer; it may not be running, or may predate the inbox".to_string()
    })
}

/// A refusal's sentence, when `line` is one.
fn refusal(line: &str) -> Option<String> {
    let reply = serde_json::from_str::<InboxWaitReply>(line).ok()?;
    let code = reply.refused?;
    Some(match reply.error {
        Some(error) => format!("{error} ({code})"),
        None => code,
    })
}

/// `dot-agent-deck inbox fetch`: print the framing sentence and every message
/// waiting for this agent, and acknowledge them.
pub fn fetch(json: bool) -> ExitCode {
    let Some(identity) = identity() else {
        eprintln!("{NOT_IN_PANE}");
        return ExitCode::FAILURE;
    };
    let message = DaemonMessage::InboxFetch(InboxFetchRequest {
        pane_id: identity.pane_id,
        agent_id: identity.agent_id,
        token: identity.token,
        consumer: InboxFetchConsumer::Cli,
        max_chars: None,
        max_bytes: None,
        ack: true,
    });
    let line = match request(&message) {
        Ok(line) => line,
        Err(why) => {
            eprintln!("Could not fetch messages: {why}.");
            return ExitCode::FAILURE;
        }
    };
    if let Some(why) = refusal(&line) {
        eprintln!("The deck refused to fetch messages: {why}.");
        return ExitCode::FAILURE;
    }
    let Some(messages) = serde_json::from_str::<InboxWaitReply>(&line)
        .ok()
        .and_then(|r| r.messages)
    else {
        eprintln!(
            "Could not fetch messages: the deck's answer was not one this build understands."
        );
        return ExitCode::FAILURE;
    };
    if json {
        println!("{}", line.trim_end());
        return ExitCode::SUCCESS;
    }
    if messages.is_empty() {
        println!("No dot-agent-deck messages are waiting.");
        return ExitCode::SUCCESS;
    }
    println!("{AGENT_FRAMING}");
    for message in &messages {
        println!();
        println!("{}", message.text);
    }
    ExitCode::SUCCESS
}

/// `dot-agent-deck inbox list`: this agent's messages and their states.
pub fn list(json: bool, all: bool) -> ExitCode {
    let Some(identity) = identity() else {
        eprintln!("{NOT_IN_PANE}");
        return ExitCode::FAILURE;
    };
    let pane_id = identity.pane_id.clone();
    let message = DaemonMessage::InboxList(InboxListRequest {
        pane_id: identity.pane_id,
        agent_id: identity.agent_id,
        token: identity.token,
        all,
    });
    let line = match request(&message) {
        Ok(line) => line,
        Err(why) => {
            eprintln!("Could not list messages: {why}.");
            return ExitCode::FAILURE;
        }
    };
    if let Some(why) = refusal(&line) {
        eprintln!("The deck refused to list messages: {why}.");
        return ExitCode::FAILURE;
    }
    let Ok(reply) = serde_json::from_str::<InboxListReply>(&line) else {
        eprintln!("Could not list messages: the deck's answer was not one this build understands.");
        return ExitCode::FAILURE;
    };
    if json {
        println!("{}", line.trim_end());
        return ExitCode::SUCCESS;
    }
    print!("{}", render_list(&pane_id, &reply));
    ExitCode::SUCCESS
}

fn render_list(pane_id: &str, reply: &InboxListReply) -> String {
    let mut out = format!(
        "Inbox of pane {pane_id} — {} armed waiter{}\n",
        reply.armed,
        if reply.armed == 1 { "" } else { "s" }
    );
    if reply.messages.is_empty() {
        out.push_str("No messages.\n");
        return out;
    }
    let rows: Vec<[String; 5]> = reply
        .messages
        .iter()
        .map(|m| {
            [
                m.id.to_string(),
                m.state.to_string(),
                m.recipient
                    .as_ref()
                    .map(|r| r.pane_id.clone())
                    .unwrap_or_else(|| "-".to_string()),
                format!("{}s", m.age_ms / 1000),
                m.subject.clone(),
            ]
        })
        .collect();
    let headers = ["ID", "STATE", "PANE", "AGE", "SUBJECT"];
    let mut widths = headers.map(str::len);
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let format_row = |cells: [&str; 5]| {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                line.push_str(cell);
            } else {
                line.push_str(&format!("{cell:<width$}  ", width = widths[i]));
            }
        }
        line.trim_end().to_string()
    };
    out.push_str(&format_row(headers));
    out.push('\n');
    for row in &rows {
        out.push_str(&format_row([
            row[0].as_str(),
            row[1].as_str(),
            row[2].as_str(),
            row[3].as_str(),
            row[4].as_str(),
        ]));
        out.push('\n');
    }
    out
}
