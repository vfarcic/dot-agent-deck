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
    InboxListRequest, InboxWaitMode, InboxWaitReply, InboxWaitRequest, InboxWaitSession, InboxWake,
};
use crate::inbox::StateName;
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
        "opencode-mini" => InboxComponent::OpenCodeMini,
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

/// Exit status of `inbox wait --stream` run outside a deck pane or without a
/// capability token. The Pi extension stops respawning its watcher on it
/// (`WATCHER_NOT_IN_DECK_EXIT` in `pi-extension/src/orchestrator.ts`, pinned by
/// a test in `orchestrator_ext`), so a Pi started outside the deck does not
/// retry forever.
pub const STREAM_NOT_IN_DECK_EXIT: u8 = 3;

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
/// `{"nack":[…]}` lines from stdin to the deck, until either side closes. Run
/// outside a deck pane, or without a capability token, it exits
/// [`STREAM_NOT_IN_DECK_EXIT`] so the caller knows a respawn cannot succeed.
pub fn wait(agent: &str, stream: bool) -> ExitCode {
    let not_in_deck = if stream {
        ExitCode::from(STREAM_NOT_IN_DECK_EXIT)
    } else {
        ExitCode::SUCCESS
    };
    let Some(identity) = identity() else {
        return not_in_deck;
    };
    if identity.token.is_none() {
        return not_in_deck;
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

/// PRD #1666 S8: `dot-agent-deck inbox announce --agent <agent>`. Tells the
/// deck that `agent`'s native component is coming, so the doorbell gives it
/// the arm grace before ringing. Run by OpenCode's server plugin as it loads
/// in the full TUI, ahead of its first status report, because the TUI plugin
/// that arms the stream may load after that report. Silent and always
/// successful: outside a deck, or with no daemon, there is nobody to tell.
pub fn announce(agent: &str) -> ExitCode {
    if identity().is_some_and(|identity| identity.token.is_some()) {
        crate::hook::send_inbox_announce(component_for(agent), InboxWake::Native);
    }
    ExitCode::SUCCESS
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
    // Acks and nacks from the component, forwarded as they come. The
    // component closing our stdin is how it says it is done (PRD #1666 S8:
    // OpenCode's plugin on dispose), so the watcher then exits, and the deck
    // takes back whatever it handed over and was not acknowledged.
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
        std::process::exit(0);
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
    fetch_as(json, None)
}

/// PRD #1666 S8: `dot-agent-deck inbox fetch --json --doorbell <id>`, run by
/// an agent-side component that caught the doorbell naming message `id` on
/// its way to the model (OpenCode's server plugin). Prints the deck's JSON
/// answer and acknowledges nothing: the component acknowledges once the agent
/// has started on the content, so a turn aborted before then is handed over
/// again. When `id` is no longer waiting — a doorbell recalled from prompt
/// history — it fetches nothing, so a newer message is never pulled into an
/// old doorbell's turn.
pub fn fetch_for_doorbell(id: &str) -> ExitCode {
    if !crate::inbox::is_valid_message_id(id) {
        eprintln!("Could not fetch messages: {id:?} is not a deck message id.");
        return ExitCode::FAILURE;
    }
    fetch_as(true, Some(id))
}

fn fetch_as(json: bool, doorbell: Option<&str>) -> ExitCode {
    let Some(identity) = identity() else {
        eprintln!("{NOT_IN_PANE}");
        return ExitCode::FAILURE;
    };
    if let Some(id) = doorbell {
        let listing = DaemonMessage::InboxList(InboxListRequest {
            pane_id: identity.pane_id.clone(),
            agent_id: identity.agent_id.clone(),
            token: identity.token.clone(),
            all: false,
        });
        let waiting = request(&listing)
            .ok()
            .and_then(|line| serde_json::from_str::<InboxListReply>(&line).ok())
            .is_some_and(|reply| {
                reply
                    .messages
                    .iter()
                    .any(|m| m.id.as_str() == id && m.state == StateName::Queued)
            });
        if !waiting {
            println!("{}", serde_json::json!({ "messages": [] }));
            return ExitCode::SUCCESS;
        }
    }
    let message = DaemonMessage::InboxFetch(InboxFetchRequest {
        pane_id: identity.pane_id,
        agent_id: identity.agent_id,
        token: identity.token,
        consumer: if doorbell.is_some() {
            InboxFetchConsumer::DoorbellHook
        } else {
            InboxFetchConsumer::Cli
        },
        max_chars: None,
        max_bytes: None,
        ack: doorbell.is_none(),
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

/// The most characters of a pane id the plain listing shows.
const LIST_PANE_CHARS: usize = 64;

/// The most characters of a subject the plain listing shows.
const LIST_SUBJECT_CHARS: usize = 200;

/// The plain listing. Every value another process supplied goes through
/// [`crate::untrusted_text::label_line`] here, at the terminal (hardening L1);
/// `--json` prints the data as the deck answered it.
fn render_list(pane_id: &str, reply: &InboxListReply) -> String {
    let label = crate::untrusted_text::label_line;
    let mut out = format!(
        "Inbox of pane {} — {} armed waiter{}\n",
        label(pane_id, LIST_PANE_CHARS),
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
                    .map(|r| label(&r.pane_id, LIST_PANE_CHARS))
                    .unwrap_or_else(|| "-".to_string()),
                format!("{}s", m.age_ms / 1000),
                label(&m.subject, LIST_SUBJECT_CHARS),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// PRD #1666 S8: `inbox wait --agent opencode-mini` speaks for OpenCode's
    /// `--mini` server plugin, on the wire as `opencode_mini`; the full TUI's
    /// plugin keeps `opencode_plugin`.
    #[test]
    fn opencode_mini_agent_maps_to_its_own_component() {
        assert_eq!(component_for("opencode-mini"), InboxComponent::OpenCodeMini);
        assert_eq!(
            serde_json::to_value(component_for("opencode-mini")).unwrap(),
            "opencode_mini"
        );
        assert_eq!(component_for("opencode"), InboxComponent::OpenCodePlugin);
        assert_eq!(component_for("unheard-of"), InboxComponent::Other);
    }

    /// Hardening L1: the plain listing is drawn into a terminal, so a subject
    /// or pane id carrying controls, bidi or invisible marks is shown without
    /// them, on one line, bounded. `--json` stays the data as posted.
    #[test]
    fn the_plain_listing_strips_controls_bidi_and_invisibles_from_metadata() {
        let hub = crate::inbox_hub::InboxHub::new(
            crate::inbox::DeckId::parse("k3xzq2ab").unwrap(),
            crate::inbox::InboxLimits::default(),
            crate::inbox_hub::DEFAULT_KEEPALIVE,
        );
        let pane = "p\x1b]0;x\x07";
        let subject = format!(
            "fix \x1b[2J\u{202E}gpj.exe\u{200B}\nSTATE {}",
            "y".repeat(500)
        );
        hub.post_direct(
            crate::inbox::Recipient::bound(pane, "a"),
            crate::inbox::Content::from_deck("seed", subject, "body"),
        )
        .unwrap();
        let reply = InboxListReply {
            messages: hub.list(pane),
            armed: 0,
            wake: None,
            component: None,
        };
        let out = render_list(pane, &reply);
        assert!(
            !out.chars().any(|c| (c.is_control() && c != '\n')
                || crate::untrusted_text::is_bidi_format_char(c)
                || c == '\u{200B}'),
            "{out:?}"
        );
        assert_eq!(out.lines().count(), 3, "{out}");
        assert!(out.lines().all(|line| line.chars().count() <= 400), "{out}");
    }
}
