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
    DaemonMessage, InboxClaimReply, InboxClaimRequest, InboxComponent, InboxFetchConsumer,
    InboxFetchRequest, InboxListReply, InboxListRequest, InboxOfferRequest, InboxPostReply,
    InboxPublishRequest, InboxWaitMode, InboxWaitReply, InboxWaitRequest, InboxWaitSession,
    InboxWake,
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
/// message as one JSON line on stdout and forwards the component's receipt
/// controls from stdin to the deck, until either side closes. The deck serves
/// a stream only with `--adapter-integration` at its floor. Run outside a deck pane, or without a capability token, it
/// exits [`STREAM_NOT_IN_DECK_EXIT`] so the caller knows a respawn cannot
/// succeed.
/// PRD #1666 §4, §11: what a watcher's component declared on its command
/// line beyond `--agent` and `--stream`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Receipt {
    /// `--adapter-integration`: with it, the stream carries receipts.
    pub adapter_integration: Option<u32>,
    pub doorbell_only: bool,
    pub resumed: bool,
}

pub fn wait(agent: &str, stream: bool, receipt: Receipt) -> ExitCode {
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
        adapter_integration: receipt.adapter_integration,
        doorbell_only: receipt.doorbell_only,
        resumed: receipt.resumed,
    });
    let Ok(json) = serde_json::to_string(&request) else {
        return ExitCode::SUCCESS;
    };
    if stream && receipt.adapter_integration.is_some() {
        crate::inbox_watcher::wait_receipt_stream(&json)
    } else if stream {
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
    // Control lines from the component, forwarded as they come; a daemon from
    // this build refuses a stream without `--adapter-integration` before it
    // reads any. The component closing our stdin is how it says it is done
    // (PRD #1666 S8: OpenCode's plugin on dispose), so the watcher then exits.
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
/// waiting for this agent, and acknowledge them once they are printed.
pub fn fetch(json: bool) -> ExitCode {
    fetch_as(json, None, None, None)
}

/// PRD #1666 S8: `dot-agent-deck inbox fetch --json --doorbell <id>`, run by
/// an agent-side component that caught the doorbell naming message `id` on
/// its way to the model (OpenCode's server plugin). Prints the deck's JSON
/// answer and acknowledges nothing: the component acknowledges once the agent
/// has started on the content, so a turn aborted before then is handed over
/// again. When `id` is no longer waiting — a doorbell recalled from prompt
/// history, or a message another consumer took meanwhile — it fetches
/// nothing, so a newer message is never pulled into an old doorbell's turn.
/// The daemon decides that with the fetch itself, under its inbox lock.
///
/// PRD #1666 §4, §11: with `adapter_integration` and `rx` (the OpenCode
/// plugin's receipt stream), the fetched attempts are bound to that stream,
/// which holds their caps and their possession; the JSON names each attempt
/// and carries no cap.
pub fn fetch_for_doorbell(id: &str, adapter_integration: Option<u32>, rx: Option<u64>) -> ExitCode {
    if !crate::inbox::is_valid_message_id(id) {
        eprintln!("Could not fetch messages: {id:?} is not a deck message id.");
        return ExitCode::FAILURE;
    }
    fetch_as(true, Some(id), adapter_integration, rx)
}

fn fetch_as(
    json: bool,
    doorbell: Option<&str>,
    adapter_integration: Option<u32>,
    rx: Option<u64>,
) -> ExitCode {
    let Some(identity) = identity() else {
        eprintln!("{NOT_IN_PANE}");
        return ExitCode::FAILURE;
    };
    fetch_with(
        json,
        doorbell,
        (adapter_integration, rx),
        identity,
        &mut request,
        &mut std::io::stdout().lock(),
        &mut |ids| crate::hook::acknowledge_handed_over(ids),
    )
}

/// [`fetch_as`] with its three effects passed in: `send` asks the deck,
/// `out` is where the content goes, and `settle` acknowledges what reached
/// `out`.
///
/// PR #1735 fifth review (N2): the fetch never acknowledges. A plain fetch
/// writes the content, flushes it, and only then acknowledges exactly what it
/// wrote, so a reply lost on the way, a request that timed out or a closed
/// stdout leaves the messages to be handed over again instead of shown as
/// delivered. A doorbell fetch acknowledges nothing here: its component does,
/// once its agent has the content. `get-seed`, the compatibility alias, keeps
/// its own acknowledge-on-fetch in the daemon and does not come through here.
fn fetch_with(
    json: bool,
    doorbell: Option<&str>,
    (adapter_integration, rx): (Option<u32>, Option<u64>),
    identity: Identity,
    send: &mut dyn FnMut(&DaemonMessage) -> Result<String, String>,
    out: &mut dyn Write,
    settle: &mut dyn FnMut(&[String]),
) -> ExitCode {
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
        ack: false,
        doorbell: doorbell.map(str::to_string),
        adapter_integration,
        rx,
    });
    let line = match send(&message) {
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
    let written = if json {
        writeln!(out, "{}", line.trim_end())
    } else if messages.is_empty() {
        writeln!(out, "No dot-agent-deck messages are waiting.")
    } else {
        let mut text = AGENT_FRAMING.to_string();
        for message in &messages {
            text.push_str("\n\n");
            text.push_str(&message.text);
        }
        writeln!(out, "{text}")
    };
    if written.and_then(|()| out.flush()).is_err() {
        return ExitCode::FAILURE;
    }
    if doorbell.is_none() {
        let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
        settle(&ids);
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

/// The body of an `inbox publish` or `inbox offer`: `--text` as given, or
/// `--file` read with the task cap (`-` is stdin).
fn body_from(text: Option<String>, file: Option<String>) -> Result<String, String> {
    match (text, file) {
        (Some(text), None) => Ok(text),
        (None, Some(path)) => crate::bounded_read::read_task_input(&path, std::io::stdin()),
        _ => Err("give the text with exactly one of --text or --file".to_string()),
    }
}

/// A post's refusal sentence, when `line` is one.
fn post_refusal(line: &str) -> Option<String> {
    let reply = serde_json::from_str::<InboxPostReply>(line).ok()?;
    let code = reply.refused?;
    Some(match reply.error {
        Some(error) => format!("{error} ({code})"),
        None => code,
    })
}

/// Send a publish or offer and print what became of it.
fn post(message: DaemonMessage, what: &str) -> ExitCode {
    let line = match request(&message) {
        Ok(line) => line,
        Err(why) => {
            eprintln!("Could not {what}: {why}.");
            return ExitCode::FAILURE;
        }
    };
    if let Some(why) = post_refusal(&line) {
        eprintln!("The deck refused to {what}: {why}.");
        return ExitCode::FAILURE;
    }
    let Some(reply) = serde_json::from_str::<InboxPostReply>(&line)
        .ok()
        .filter(|reply| reply.id.is_some())
    else {
        eprintln!("Could not {what}: the deck's answer was not one this build understands.");
        return ExitCode::FAILURE;
    };
    let id = reply.id.unwrap_or_default();
    let label = crate::untrusted_text::label_line;
    match (&message, reply.delivered_to, reply.offered_to) {
        (DaemonMessage::InboxPublish(_), delivered, _) => {
            let delivered = delivered.unwrap_or(0);
            println!(
                "Published {id} to {delivered} subscriber{}.",
                if delivered == 1 { "" } else { "s" }
            );
            for pane in &reply.refused_full {
                println!(
                    "Not delivered to pane {}: its inbox is full.",
                    label(pane, 64)
                );
            }
        }
        (_, _, Some(pane)) => println!("Offered {id}; handed to pane {}.", label(&pane, 64)),
        _ => println!("Offered {id}; it waits in its pool until a worker that takes it is free."),
    }
    ExitCode::SUCCESS
}

/// `dot-agent-deck inbox publish <topic>`.
pub fn publish(topic: &str, text: Option<String>, file: Option<String>) -> ExitCode {
    let Some(identity) = identity() else {
        eprintln!("{NOT_IN_PANE}");
        return ExitCode::FAILURE;
    };
    let body = match body_from(text, file) {
        Ok(body) => body,
        Err(why) => {
            eprintln!("Could not publish: {why}.");
            return ExitCode::FAILURE;
        }
    };
    post(
        DaemonMessage::InboxPublish(InboxPublishRequest {
            pane_id: identity.pane_id,
            agent_id: identity.agent_id,
            token: identity.token,
            topic: topic.to_string(),
            body,
        }),
        "publish",
    )
}

/// `dot-agent-deck inbox offer <pool>`.
pub fn offer(
    pool: &str,
    text: Option<String>,
    file: Option<String>,
    claim_ttl: Option<String>,
) -> ExitCode {
    let Some(identity) = identity() else {
        eprintln!("{NOT_IN_PANE}");
        return ExitCode::FAILURE;
    };
    let claim_ttl_ms = match claim_ttl.as_deref().map(parse_duration_ms).transpose() {
        Ok(ms) => ms,
        Err(why) => {
            eprintln!("Could not offer: --claim-ttl {why}.");
            return ExitCode::FAILURE;
        }
    };
    let body = match body_from(text, file) {
        Ok(body) => body,
        Err(why) => {
            eprintln!("Could not offer: {why}.");
            return ExitCode::FAILURE;
        }
    };
    post(
        DaemonMessage::InboxOffer(InboxOfferRequest {
            pane_id: identity.pane_id,
            agent_id: identity.agent_id,
            token: identity.token,
            pool: pool.to_string(),
            body,
            claim_ttl_ms,
        }),
        "offer the job",
    )
}

/// `dot-agent-deck inbox complete <id>` (`complete`) or `inbox release <id>`.
pub fn settle(id: &str, complete: bool) -> ExitCode {
    let Some(identity) = identity() else {
        eprintln!("{NOT_IN_PANE}");
        return ExitCode::FAILURE;
    };
    let request_body = InboxClaimRequest {
        pane_id: identity.pane_id,
        agent_id: identity.agent_id,
        token: identity.token,
        id: id.to_string(),
    };
    let (message, what) = if complete {
        (
            DaemonMessage::InboxComplete(request_body),
            "complete the job",
        )
    } else {
        (DaemonMessage::InboxRelease(request_body), "release the job")
    };
    let line = match request(&message) {
        Ok(line) => line,
        Err(why) => {
            eprintln!("Could not {what}: {why}.");
            return ExitCode::FAILURE;
        }
    };
    let Ok(reply) = serde_json::from_str::<InboxClaimReply>(&line) else {
        eprintln!("Could not {what}: the deck's answer was not one this build understands.");
        return ExitCode::FAILURE;
    };
    if let Some(code) = reply.refused {
        let why = reply.error.map(|e| format!("{e} ({code})")).unwrap_or(code);
        eprintln!("The deck refused to {what}: {why}.");
        return ExitCode::FAILURE;
    }
    let id = crate::untrusted_text::label_line(id, 64);
    match reply.outcome.as_deref() {
        Some("completed") => println!("Job {id} is complete; its offerer has been told."),
        Some("released") => println!("Job {id} is back in its pool for another worker."),
        _ => {
            eprintln!("Could not {what}: the deck's answer was not one this build understands.");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

/// A duration such as `30m`, `1h30m`, `90s` or `500ms`, in milliseconds.
/// Every number needs a unit: `ms`, `s`, `m` or `h`.
pub fn parse_duration_ms(text: &str) -> Result<u64, String> {
    let invalid = || {
        format!(
            "\"{}\" is not a duration such as 30m, 1h30m or 90s",
            crate::untrusted_text::label_line(text, 64)
        )
    };
    let mut rest = text.trim();
    if rest.is_empty() {
        return Err(invalid());
    }
    let mut total: u64 = 0;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(invalid());
        }
        let number: u64 = rest[..digits].parse().map_err(|_| invalid())?;
        rest = &rest[digits..];
        let (unit_ms, len) = if rest.starts_with("ms") {
            (1, 2)
        } else if rest.starts_with('s') {
            (1_000, 1)
        } else if rest.starts_with('m') {
            (60_000, 1)
        } else if rest.starts_with('h') {
            (3_600_000, 1)
        } else {
            return Err(invalid());
        };
        rest = &rest[len..];
        total = number
            .checked_mul(unit_ms)
            .and_then(|ms| total.checked_add(ms))
            .ok_or_else(invalid)?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that refuses everything, as a closed stdout does.
    struct Closed;

    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
    }

    /// PR #1735 fifth review (N2): plain `inbox fetch` asks the deck for the
    /// messages without acknowledging them, writes them, and acknowledges
    /// exactly what it wrote, only once the write succeeded. Output that
    /// never reaches the caller — a closed stdout — acknowledges nothing, so
    /// the deck hands the message over again rather than showing it
    /// delivered. A doorbell fetch, which the component acknowledges itself,
    /// settles nothing here.
    #[test]
    fn a_plain_fetch_acknowledges_only_what_it_wrote() {
        let delivery = crate::event::InboxDelivery {
            id: "m-k3xzq2ab-0123456789abcdef".to_string(),
            subject: "subject".to_string(),
            from: "deck".to_string(),
            text: "[dot-agent-deck message m-k3xzq2ab-0123456789abcdef from deck]".to_string(),
            attempt: None,
        };
        let answer =
            serde_json::to_string(&InboxWaitReply::messages(vec![delivery.clone()])).unwrap();
        let identity = || Identity {
            pane_id: "p".to_string(),
            agent_id: Some("a".to_string()),
            token: Some("t".to_string()),
        };
        let run = |json: bool, doorbell: Option<&str>, out: &mut dyn Write| {
            let mut asked = Vec::new();
            let mut settled = Vec::new();
            let code = fetch_with(
                json,
                doorbell,
                (None, None),
                identity(),
                &mut |message| {
                    asked.push(message.clone());
                    Ok(answer.clone())
                },
                out,
                &mut |ids| settled.extend_from_slice(ids),
            );
            (code, asked, settled)
        };
        let acked = |asked: &[DaemonMessage]| match asked {
            [DaemonMessage::InboxFetch(request)] => request.ack,
            other => panic!("one inbox_fetch, got {other:?}"),
        };

        let (code, asked, settled) = run(false, None, &mut Closed);
        assert_eq!(code, ExitCode::FAILURE);
        assert!(!acked(&asked), "the fetch itself must not acknowledge");
        assert!(settled.is_empty(), "nothing was written: {settled:?}");

        for json in [false, true] {
            let mut out = Vec::new();
            let (code, asked, settled) = run(json, None, &mut out);
            assert_eq!(code, ExitCode::SUCCESS);
            assert!(!acked(&asked));
            assert!(String::from_utf8(out).unwrap().contains(&delivery.text));
            assert_eq!(settled, vec![delivery.id.clone()], "json: {json}");
        }

        let mut out = Vec::new();
        let (_, asked, settled) = run(true, Some(delivery.id.as_str()), &mut out);
        assert!(!acked(&asked));
        assert!(
            settled.is_empty(),
            "a doorbell fetch's component acknowledges"
        );
    }

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

    /// PRD #1666 S11: `--claim-ttl` takes a duration with a unit on every
    /// number, and refuses anything else rather than guessing a unit.
    #[test]
    fn claim_ttl_durations_need_units() {
        assert_eq!(parse_duration_ms("30m"), Ok(1_800_000));
        assert_eq!(parse_duration_ms("1h30m"), Ok(5_400_000));
        assert_eq!(parse_duration_ms("90s"), Ok(90_000));
        assert_eq!(parse_duration_ms("500ms"), Ok(500));
        for bad in [
            "",
            "30",
            "m",
            "1x",
            "1h 30m",
            "-5m",
            "99999999999999999999h",
        ] {
            assert!(parse_duration_ms(bad).is_err(), "{bad:?} must be refused");
        }
    }
}
