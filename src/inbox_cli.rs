//! PRD #1666: the `dot-agent-deck inbox …` commands an agent (or its hook,
//! plugin or extension) runs inside its pane to reach its inbox over the hook
//! socket.
//!
//! Every command identifies the caller by the pane id, agent id and hook
//! capability token the deck put in the pane's environment; the daemon acts
//! only for the agent the token attests.

use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use crate::agent_pty::{DOT_AGENT_DECK_AGENT_ID, DOT_AGENT_DECK_PANE_ID};
use crate::event::{
    DaemonMessage, InboxClaimReply, InboxClaimRequest, InboxComponent, InboxFetchConsumer,
    InboxFetchRequest, InboxListReply, InboxListRequest, InboxOfferRequest, InboxOneShotSender,
    InboxPostReply, InboxPublishRequest, InboxWaitMode, InboxWaitReply, InboxWaitRequest,
    InboxWaitSession, InboxWake,
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

/// Exit status of `inbox wait --stream` run outside a deck pane, without a
/// capability token, or without `--adapter-integration` — a stream this deck
/// does not serve. The Pi extension stops respawning its watcher on it
/// (`WATCHER_NOT_IN_DECK_EXIT` in `pi-extension/src/orchestrator.ts`, pinned by
/// a test in `orchestrator_ext`), so a Pi started outside the deck does not
/// retry forever.
pub const STREAM_NOT_IN_DECK_EXIT: u8 = 3;

/// `dot-agent-deck inbox wait --agent <agent> [--stream]`.
///
/// **Once** (Claude Code's `asyncRewake` hook): blocks until the deck hands
/// over messages, then writes their blocks to stderr and exits 2, which is what
/// wakes Claude. A daemon that does not confirm it serves one-shot receipts —
/// one older than this binary, which answers nothing — is handed nothing: the
/// waiter says so on stderr and exits 1, with
/// [`crate::inbox_oneshot::RECEIPTS_TIMED_OUT`] when no answer came in time,
/// [`crate::inbox_oneshot::RECEIPTS_UNSUPPORTED`] when one came without
/// the capability and [`crate::inbox_oneshot::RECEIPTS_CONNECTION_LOST`] when
/// the connection was lost before any answer.
/// Everything else — run outside a deck, no daemon, superseded by a newer
/// waiter, refused — exits 0 with no output, so a global hook never disturbs
/// a session the deck does not manage.
///
/// **Stream** (the OpenCode plugin, the Pi extension): prints each handed-over
/// message as one JSON line on stdout and forwards the component's receipt
/// controls from stdin to the deck, until either side closes
/// ([`crate::inbox_watcher::wait_receipt_stream`]). The deck serves a stream
/// only with `--adapter-integration` at its floor. Run outside a deck pane,
/// without a capability token, or without `--adapter-integration`, it exits
/// [`STREAM_NOT_IN_DECK_EXIT`] so the caller knows a respawn cannot succeed.
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
    if unserved_stream(stream, receipt) {
        eprintln!("inbox wait --stream needs --adapter-integration.");
        return ExitCode::from(STREAM_NOT_IN_DECK_EXIT);
    }
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
    // PRD #1666 part 2: the once-waiter asks the deck whether it serves
    // receipts, on the connection its wait then takes.
    let question = if stream {
        None
    } else {
        identity.token.as_deref().and_then(|token| {
            crate::inbox_oneshot::receipts_question(
                &identity.pane_id,
                identity.agent_id.as_deref(),
                token,
            )
        })
    };
    let wait_request = InboxWaitRequest {
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
        hook_integration: if stream {
            None
        } else {
            crate::inbox_oneshot::declared_hook_integration()
        },
    };
    // A waiter that declares no integration stands in for an older
    // dot-agent-deck, and speaks its legacy `inbox_wait` unasked.
    let receipts = !stream && wait_request.hook_integration.is_some();
    let request = if receipts {
        DaemonMessage::InboxReceiptWait(wait_request)
    } else {
        DaemonMessage::InboxWait(wait_request)
    };
    let Ok(json) = serde_json::to_string(&request) else {
        return ExitCode::SUCCESS;
    };
    if stream {
        crate::inbox_watcher::wait_receipt_stream(&json)
    } else {
        wait_once(&json, question.as_deref().filter(|_| receipts))
    }
}

/// PRD #1666 C5: whether `inbox wait` asks for a stream without receipts, one
/// the deck refuses, so there is nothing to ask it.
fn unserved_stream(stream: bool, receipt: Receipt) -> bool {
    stream && receipt.adapter_integration.is_none()
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

/// Claude's once-waiter on receipts (part 2 §1.1): register, take one
/// batch, and hand it to Claude through stderr — asking `may` for every
/// attempt and fencing each block right before its first byte — then publish
/// a `handed` receipt for every block any byte of which went, on the
/// attempt's own handed cap from its caps file, and report every attempt.
/// The commit comes later, from the prompt hook that sees the block in
/// Claude's notification. Exits 2 when anything reached stderr, 1 when
/// nothing of the batch did, and 0, silent, when no batch came.
///
/// With `question`, it first asks the deck whether it serves receipts, and
/// sends `json` only on a deck that says so; one that does not is sent
/// nothing that could take mail, and the waiter says so and exits 1
/// ([`crate::inbox_oneshot::Connection::open_receipts`]). A socket this user
/// may not use is asked nothing, and the waiter says that and exits 1
/// ([`crate::inbox_oneshot::RECEIPTS_FOREIGN_SOCKET`]). Without it, it is
/// a stand-in for an older waiter and sends `json` unasked.
#[cfg(unix)]
fn wait_once(json: &str, question: Option<&str>) -> ExitCode {
    use crate::inbox_oneshot::{Connection, OneShotBatch, Opened, Piece, Pinned, Receiver};
    exit_when_orphaned();
    let path = crate::endpoint_resolve::client_socket_path();
    let mut connection = match question {
        Some(question) => {
            match Connection::open_receipts(&path, question, InboxOneShotSender::ClaudeWaiter) {
                // The request is written within a budget of its own from the
                // answer; only the wait for a batch after it is unbounded.
                Opened::Ready(mut connection) => {
                    let by =
                        std::time::Instant::now() + crate::inbox_oneshot::receipts_answer_wait();
                    if connection.send(json, by).is_err() {
                        return ExitCode::SUCCESS;
                    }
                    connection
                }
                Opened::Unsupported => {
                    crate::inbox_oneshot::report_unsupported(InboxOneShotSender::ClaudeWaiter);
                    return ExitCode::FAILURE;
                }
                Opened::Lost => {
                    crate::inbox_oneshot::report_connection_lost();
                    return ExitCode::FAILURE;
                }
                Opened::TimedOut => {
                    crate::inbox_oneshot::report_timed_out(InboxOneShotSender::ClaudeWaiter);
                    return ExitCode::FAILURE;
                }
                Opened::Foreign => {
                    crate::inbox_oneshot::report_foreign_socket();
                    return ExitCode::FAILURE;
                }
                Opened::Refused(_) | Opened::Unreachable => return ExitCode::SUCCESS,
            }
        }
        None => match Connection::open(&path, json) {
            Ok(connection) => connection,
            Err(_) => return ExitCode::SUCCESS,
        },
    };
    let mut grant_ttl = crate::inbox::DEFAULT_ACK_WAIT;
    let messages = loop {
        let Some(reply) = connection.next(None) else {
            return ExitCode::SUCCESS;
        };
        if reply.keepalive {
            continue;
        }
        if let Some(registered) = &reply.registered {
            if let Some(ms) = registered.grant_ttl_ms {
                grant_ttl = Duration::from_millis(ms);
            }
            if reply.messages.is_none() {
                continue;
            }
        }
        if reply.messages.is_none() && !reply.revoke.is_empty() {
            connection.hold(reply);
            continue;
        }
        // A refusal, a supersession, an empty batch: nothing to wake for.
        match reply.messages {
            Some(messages) if !messages.is_empty() => break messages,
            _ => return ExitCode::SUCCESS,
        }
    };
    let mut batch = OneShotBatch::new(messages);
    if batch.is_empty() {
        return ExitCode::SUCCESS;
    }
    let (pane, agent) = (
        std::env::var(DOT_AGENT_DECK_PANE_ID).unwrap_or_default(),
        std::env::var(DOT_AGENT_DECK_AGENT_ID).unwrap_or_default(),
    );
    let pinned = Pinned::from_env(&pane, &agent);
    let degraded = match &pinned {
        Pinned::Open(spool) => batch.caps_from(spool),
        _ => None,
    };
    let Ok(mut stderr) = raw_stderr() else {
        // Nothing reached Claude: the deck is told so, and requeues the batch.
        batch.decline(&mut connection, Receiver::Waiter);
        return ExitCode::FAILURE;
    };
    let mut pieces = batch.plain_pieces("", crate::inbox_hub::BLOCK_SEPARATOR);
    pieces.push(Piece::Frame(b"\n"));
    let ran = batch.run(
        &mut connection,
        &pinned,
        grant_ttl,
        &mut stderr,
        &pieces,
        Receiver::Waiter,
        degraded,
    );
    if ran.handed_any() {
        ExitCode::from(2)
    } else {
        // Nothing reached Claude, and the deck was told so.
        ExitCode::FAILURE
    }
}

/// This process's stderr as the descriptor itself, with no buffer between a
/// write and the pipe, so every byte a write accepted is one Claude can read.
#[cfg(unix)]
fn raw_stderr() -> std::io::Result<std::fs::File> {
    use std::os::fd::AsFd;
    Ok(std::fs::File::from(
        std::io::stderr().as_fd().try_clone_to_owned()?,
    ))
}

/// The one-shot receipt protocol runs on Unix only, as the receipt stream
/// does: elsewhere the waiter exits quietly. The deck may still type its
/// doorbell line, but Claude's prompt hook injects nothing there either
/// (`hook::deck_prompt_submit`), so nothing here shows a message body
/// reaching Claude on Windows; it is untested until Windows ships.
#[cfg(not(unix))]
fn wait_once(_json: &str, _question: Option<&str>) -> ExitCode {
    ExitCode::SUCCESS
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
/// waiting for this agent, and settle each one the output carried whole.
///
/// PRD #1666 part 3 (H4): it is a one-shot receipt receiver, as an agent's
/// hook is ([`fetch_one_shot`]): it asks the deck whether it serves this
/// command, fetches on that connection, and commits what it wrote and flushed
/// to stdout. A deck that does not confirm it serves it, an older one
/// included, is sent nothing that could take mail. The one-shot is Unix only:
/// elsewhere, and from a process standing in for an older dot-agent-deck
/// ([`crate::inbox_oneshot::declared_hook_integration`]), the fetch is the
/// legacy one, acknowledged once printed.
pub fn fetch(json: bool) -> ExitCode {
    #[cfg(unix)]
    if crate::inbox_oneshot::declared_hook_integration().is_some() {
        let Some(identity) = identity() else {
            eprintln!("{NOT_IN_PANE}");
            return ExitCode::FAILURE;
        };
        let (pane, agent) = (
            identity.pane_id.clone(),
            identity.agent_id.clone().unwrap_or_default(),
        );
        return fetch_one_shot(
            json,
            identity,
            &crate::endpoint_resolve::client_socket_path(),
            crate::inbox_oneshot::receipts_answer_wait(),
            raw_stdout(),
            &|| crate::inbox_oneshot::Pinned::from_env(&pane, &agent),
            &mut std::io::stderr(),
        );
    }
    fetch_as(json, None, None, None)
}

/// PRD #1666 part 3 (H4): what `inbox fetch` says when the deck did not
/// confirm that it serves this command on receipts
/// ([`crate::inbox_oneshot::Opened::Unsupported`]): nothing was taken.
pub const CLI_RECEIPTS_UNSUPPORTED: &str = "Could not fetch messages: the deck daemon did not confirm that it can hand them to this `dot-agent-deck inbox fetch`, so no message was taken and they stay waiting. The daemon most likely runs an older dot-agent-deck than this command. Restart the deck daemon on this release (see \"Recycling the daemon\" in `dot-agent-deck docs troubleshooting`), or run `inbox fetch` with the dot-agent-deck the daemon runs.";

/// What `inbox fetch` says when the deck did not answer in time whether it
/// serves this command, or did not take the fetch in time: the fetch was not
/// sent, so nothing was taken. An older daemon answers nothing at all.
pub const CLI_RECEIPTS_TIMED_OUT: &str = "Could not fetch messages: the deck daemon did not answer in time whether it can hand them to this `dot-agent-deck inbox fetch`, so no message was taken and they stay waiting. A busy daemon can answer late; one older than this command does not answer at all. Run the command again; if this keeps happening, restart the deck daemon on this release (see \"Recycling the daemon\" in `dot-agent-deck docs troubleshooting`).";

/// What `inbox fetch` says when the connection was lost before the deck
/// answered whether it serves this command: nothing was asked that could take
/// mail.
pub const CLI_RECEIPTS_CONNECTION_LOST: &str = "Could not fetch messages: the connection to the deck daemon was lost before it answered, so no message was taken. Check that it is running with `dot-agent-deck daemon status`.";

/// What `inbox fetch` says when the fetch was sent and its answer did not
/// come in time. Unlike [`CLI_RECEIPTS_TIMED_OUT`], it does not say nothing
/// was taken: the deck may have set messages aside for this fetch.
pub const CLI_FETCH_TIMED_OUT: &str = "Could not fetch messages: the deck daemon did not answer this fetch in time. It may have set messages aside for it; the deck offers any it did again later, marked as a possible repeat.";

/// What `inbox fetch` says when the connection was lost after the fetch was
/// sent and before its answer.
pub const CLI_FETCH_CONNECTION_LOST: &str = "Could not fetch messages: the connection to the deck daemon was lost after this fetch was sent. It may have set messages aside for it; the deck offers any it did again later, marked as a possible repeat.";

/// What `inbox fetch` says when the deck socket refused it access
/// ([`crate::inbox_oneshot::Opened::Foreign`]): nothing was asked.
pub const CLI_FOREIGN_SOCKET: &str = "Could not fetch messages: the deck socket refused this command access, either because the deck listening on it runs as another user or because of the permissions of the socket or a directory above it, so it asked nothing and no message was taken. If you set DOT_AGENT_DECK_SOCKET or DOT_AGENT_DECK_ATTACH_SOCKET, point them at your own deck daemon's sockets, or unset them to use the default; otherwise fix the permissions of that socket and the directories above it.";

/// What `inbox fetch` says when it was handed messages and printed none of
/// them: the deck took the hand-over back (a revoke, a closed connection, an
/// approval that ran out), or the receipt spool could not be read.
pub const CLI_NOTHING_PRINTED: &str = "Could not fetch messages: none of them was printed, because the deck did not let this fetch hand them over (it may have taken them back, or this process could not read the deck's receipt records). The deck still has any that are waiting.";

/// What `inbox fetch` says when its output was cut short or not confirmed
/// written: what it printed may have reached the agent.
pub const CLI_CUT_SHORT: &str = "Could not print every message: what was printed may have reached the agent, and the deck may hand some of them over again, marked as a possible repeat.";

/// PRD #1666 part 3 (H4): plain `inbox fetch` as a one-shot receipt receiver
/// at the deck at `path` (part 2 §1.5): ask whether the deck serves this
/// command (an `inbox_receipts` question whose answer names
/// [`InboxOneShotSender::Cli`]), and only then send `inbox_receipt_fetch` on
/// that connection; print what it was issued to `stdout` — the framing and
/// each block, or with `json` a public projection of each message, never its
/// attempt or caps — asking `may` for every attempt and fencing each block
/// right before its first byte; publish a `committed` receipt into the spool
/// `pinned` opens once the deck has answered (it makes the generation's
/// directory as it registers the fetch) for every block written and flushed
/// whole (`handed` for one cut short);
/// and report every attempt on the connection. A block none of whose bytes
/// went is reported not handed, and the deck requeues it at once. What it
/// says goes to `err`, never anything of the batch but its text on stdout.
///
/// `wait` bounds the question and, from its answer, the fetch's request and
/// reply ([`crate::inbox_oneshot::RECEIPTS_ANSWER_WAIT`]).
#[cfg(unix)]
fn fetch_one_shot<W: Write>(
    json: bool,
    identity: Identity,
    path: &std::path::Path,
    wait: Duration,
    stdout: std::io::Result<W>,
    pinned: &dyn Fn() -> crate::inbox_oneshot::Pinned,
    err: &mut dyn Write,
) -> ExitCode {
    use crate::inbox_oneshot::{Connection, OneShotBatch, Opened, Piece, Receiver};
    let say = |err: &mut dyn Write, text: &str| {
        let _ = writeln!(err, "{text}");
        ExitCode::FAILURE
    };
    let Some(token) = identity.token.clone() else {
        return say(
            err,
            "Could not fetch messages: this process has no capability token from the deck, so the deck cannot tell it is this pane's agent. Run it inside the agent's own pane.",
        );
    };
    let Some(question) = crate::inbox_oneshot::receipts_question(
        &identity.pane_id,
        identity.agent_id.as_deref(),
        &token,
    ) else {
        return ExitCode::FAILURE;
    };
    let mut connection = match Connection::open_receipts_within(
        path,
        &question,
        InboxOneShotSender::Cli,
        wait,
    ) {
        Opened::Ready(connection) => connection,
        Opened::Unsupported => return say(err, CLI_RECEIPTS_UNSUPPORTED),
        Opened::TimedOut => return say(err, CLI_RECEIPTS_TIMED_OUT),
        Opened::Lost => return say(err, CLI_RECEIPTS_CONNECTION_LOST),
        Opened::Foreign => return say(err, CLI_FOREIGN_SOCKET),
        Opened::Refused(reply) => {
            let why = match (reply.refused, reply.error) {
                (Some(code), Some(error)) => format!("{error} ({code})"),
                (Some(code), None) => code,
                _ => "refused".to_string(),
            };
            return say(err, &format!("The deck refused to fetch messages: {why}."));
        }
        Opened::Unreachable => {
            return say(
                err,
                "Could not fetch messages: the deck did not answer; it may not be running, or may predate the inbox.",
            );
        }
    };
    let request = DaemonMessage::InboxReceiptFetch(InboxFetchRequest {
        pane_id: identity.pane_id,
        agent_id: identity.agent_id,
        token: Some(token),
        consumer: InboxFetchConsumer::Cli,
        max_chars: None,
        max_bytes: None,
        ack: false,
        doorbell: None,
        adapter_integration: None,
        rx: None,
        hook_integration: crate::inbox_oneshot::declared_hook_integration(),
        component: None,
        max_json_bytes: None,
    });
    let Ok(request) = serde_json::to_string(&request) else {
        return ExitCode::FAILURE;
    };
    // The request gets a budget of its own from the answer, as the waiter's
    // does. A line the deck never took whole is one it never read.
    let by = std::time::Instant::now() + wait;
    if let Err(error) = connection.send(&request, by) {
        return say(
            err,
            if error.kind() == std::io::ErrorKind::TimedOut {
                CLI_RECEIPTS_TIMED_OUT
            } else {
                CLI_RECEIPTS_CONNECTION_LOST
            },
        );
    }
    let reply = loop {
        match connection.next(Some(by)) {
            Some(reply) if reply.keepalive => continue,
            Some(reply) => break reply,
            None if connection.ended() => return say(err, CLI_FETCH_CONNECTION_LOST),
            None => return say(err, CLI_FETCH_TIMED_OUT),
        }
    };
    if reply.refused.is_some() || reply.not_in_deck {
        let why = match (reply.refused, reply.error) {
            (Some(code), Some(error)) => format!("{error} ({code})"),
            (Some(code), None) => code,
            (None, _) => {
                "this process is not the live agent of a pane the deck manages (not_in_deck)"
                    .to_string()
            }
        };
        return say(err, &format!("The deck refused to fetch messages: {why}."));
    }
    let Some(messages) = reply.messages else {
        return say(
            err,
            "Could not fetch messages: the deck's answer was not one this build understands.",
        );
    };
    let grant_ttl = reply
        .registered
        .as_ref()
        .and_then(|registered| registered.grant_ttl_ms)
        .map_or(crate::inbox::DEFAULT_ACK_WAIT, Duration::from_millis);
    // Only a hand-out naming its attempt is the receipt protocol's.
    let messages: Vec<crate::event::InboxDelivery> = messages
        .into_iter()
        .filter(|message| message.attempt.is_some())
        .collect();
    let public: Vec<String> = messages
        .iter()
        .map(|message| {
            serde_json::json!({
                "id": message.id,
                "subject": message.subject,
                "from": message.from,
                "text": message.text,
            })
            .to_string()
        })
        .collect();
    let batch = OneShotBatch::new(messages);
    let Ok(mut stdout) = stdout else {
        batch.decline(&mut connection, Receiver::Hook);
        return say(
            err,
            "Could not fetch messages: this process's output could not be opened, so none was printed. The deck still has them.",
        );
    };
    let mut pieces = Vec::new();
    if json {
        pieces.push(Piece::Frame(b"{\"messages\":["));
        for (i, message) in public.iter().enumerate() {
            if i > 0 {
                pieces.push(Piece::Joint(b","));
            }
            pieces.push(Piece::Block(i, message.as_bytes()));
        }
        pieces.push(Piece::Frame(b"]}\n"));
    } else if batch.is_empty() {
        pieces.push(Piece::Frame(b"No dot-agent-deck messages are waiting.\n"));
    } else {
        pieces.push(Piece::Frame(AGENT_FRAMING.as_bytes()));
        pieces.push(Piece::Frame(crate::inbox_hub::BLOCK_SEPARATOR.as_bytes()));
        for i in 0..batch.len() {
            if i > 0 {
                pieces.push(Piece::Joint(crate::inbox_hub::BLOCK_SEPARATOR.as_bytes()));
            }
            pieces.push(Piece::Block(
                i,
                batch.text(i).unwrap_or_default().as_bytes(),
            ));
        }
        pieces.push(Piece::Frame(b"\n"));
    }
    if batch.is_empty() {
        // Nothing was issued, so there is nothing to approve or report: the
        // deck ended this fetch already.
        connection.finish();
        let written = pieces.iter().try_for_each(|piece| match piece {
            Piece::Frame(bytes) => stdout.write_all(bytes),
            Piece::Block(..) | Piece::Joint(_) => Ok(()),
        });
        return if written.and_then(|()| stdout.flush()).is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    let ran = batch.run(
        &mut connection,
        &pinned(),
        grant_ttl,
        &mut stdout,
        &pieces,
        Receiver::Hook,
        None,
    );
    let written =
        |output: &crate::inbox_oneshot::Output| *output == crate::inbox_oneshot::Output::Written;
    if ran.error.is_none() && ran.outputs.iter().all(written) {
        return ExitCode::SUCCESS;
    }
    if !ran.handed_any() {
        return say(err, CLI_NOTHING_PRINTED);
    }
    // The error's kind only: fixed words, nothing of the batch.
    let _ = writeln!(
        err,
        "{CLI_CUT_SHORT}{}",
        ran.error
            .map(|error| format!(" ({})", error.kind()))
            .unwrap_or_default()
    );
    ExitCode::FAILURE
}

/// This process's stdout as the descriptor itself, with no buffer between a
/// write and the pipe, so every byte a write accepted is one the agent can
/// read.
#[cfg(unix)]
fn raw_stdout() -> std::io::Result<std::fs::File> {
    use std::os::fd::AsFd;
    Ok(std::fs::File::from(
        std::io::stdout().as_fd().try_clone_to_owned()?,
    ))
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
        hook_integration: None,
        component: None,
        max_json_bytes: None,
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
    use spec::spec;

    /// PRD #1666 C5: a stream without `--adapter-integration` is one the deck
    /// no longer serves, so `inbox wait` asks it nothing; a receipt stream and
    /// a once waiter are asked as before.
    #[test]
    fn a_stream_without_receipts_is_not_asked_for() {
        let receipts = Receipt {
            adapter_integration: Some(2),
            ..Receipt::default()
        };
        assert!(unserved_stream(true, Receipt::default()));
        assert!(!unserved_stream(true, receipts));
        assert!(!unserved_stream(false, Receipt::default()));
    }

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

    /// The two caps every stand-in hand-out below carries, which no output
    /// of `inbox fetch` may show.
    #[cfg(unix)]
    const HANDED_CAP: &str = "0123456789abcdef0123456789abcdef";
    #[cfg(unix)]
    const COMMIT_CAP: &str = "fedcba9876543210fedcba9876543210";

    /// The deck's answers to a one-shot whose deck serves `inbox fetch`:
    /// the question's, then its batch of `ids` (each with the known caps),
    /// then a grant for each `may`, then `extra`.
    #[cfg(unix)]
    fn serving(ids: &[&str], extra: &[String]) -> Vec<String> {
        let handouts: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| {
                serde_json::json!({"id": id, "subject": format!("about {id}"),
                    "from": "the orchestrator",
                    "text": format!("[dot-agent-deck message {id} delivery 1.1 from the orchestrator]\nbody of {id}\n[end of dot-agent-deck message {id}]"),
                    "a": 1, "e": 1, "d": 1,
                    "caps": {"handed": HANDED_CAP, "commit": COMMIT_CAP}})
            })
            .collect();
        let mut lines = vec![
            r#"{"one_shot_receipts":1,"one_shot_senders":["claude_waiter","claude_hooks","codex_hooks","cli"]}"#.to_string(),
            serde_json::json!({"registered": {"rx": 3, "slot": "hook", "keepalive_ms": 1000,
                "grant_ttl_ms": 5000}, "messages": handouts})
            .to_string(),
        ];
        for q in 1..=ids.len() {
            lines.push(format!(r#"{{"grant":{{"q":{q},"ok":true}}}}"#));
        }
        lines.extend(extra.iter().cloned());
        lines
    }

    /// A writer that takes `0` bytes, or that many and then refuses, or
    /// everything and then fails its flush.
    #[cfg(unix)]
    struct Out {
        took: Vec<u8>,
        room: Option<usize>,
        flush_fails: bool,
    }

    #[cfg(unix)]
    impl Write for Out {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let n = match self.room {
                Some(0) => return Err(std::io::ErrorKind::BrokenPipe.into()),
                Some(room) => buf.len().min(room),
                None => buf.len(),
            };
            if let Some(room) = &mut self.room {
                *room -= n;
            }
            self.took.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if self.flush_fails {
                Err(std::io::ErrorKind::BrokenPipe.into())
            } else {
                Ok(())
            }
        }
    }

    /// Scenario: Run plain and `--json` `inbox fetch` against stand-in decks: one that serves it, handing two messages with known caps; one whose stdout takes nothing, one that takes a prefix, one whose flush fails; one that revokes the attempt between its grant and the output; a silent deck, part 2's deck (receipts 1, no CLI) and a deck that never answers the fetch itself. Whole output commits both attempts and neither cap is in stdout, the JSON or stderr; zero bytes reports both not handed, a prefix or a failed flush possibly handed, and a revoke not handed with nothing printed; the gates send only the question, and a reply that never came after the fetch was sent says so in other words than one never asked for.
    #[cfg(unix)]
    #[spec("inbox/cli/006")]
    #[test]
    fn inbox_cli_006_a_model_run_fetch_is_a_one_shot_that_never_shows_a_cap() {
        use crate::event::InboxStreamControl;
        let (one, two) = ("m-k3xzq2ab-0f3c9a71d2e4b850", "m-k3xzq2ab-1a2b3c4d5e6f7081");
        let identity = || Identity {
            pane_id: "p".to_string(),
            agent_id: Some("a".to_string()),
            token: Some("t".to_string()),
        };
        let wait = Duration::from_millis(400);
        let run = |json: bool, answers: Option<Vec<String>>, out: Out| {
            let dir = crate::test_temp::tempdir().unwrap();
            let (path, deck) =
                crate::inbox_oneshot::tests::stand_in_deck(dir.path(), vec![answers]);
            let mut err = Vec::new();
            let mut out = out;
            let code = fetch_one_shot(
                json,
                identity(),
                &path,
                wait,
                Ok(&mut out),
                &|| crate::inbox_oneshot::Pinned::Unpinned,
                &mut err,
            );
            let lines = deck
                .join()
                .unwrap()
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            (
                code,
                String::from_utf8(out.took).unwrap(),
                String::from_utf8(err).unwrap(),
                lines,
            )
        };
        let controls = |lines: &[String]| -> Vec<InboxStreamControl> {
            lines
                .iter()
                .filter(|line| !line.contains("message_type") && !line.contains("\"may\""))
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        };
        let ats = |ids: &[&str]| -> Vec<crate::event::InboxAttemptRef> {
            ids.iter()
                .map(|id| crate::event::InboxAttemptRef {
                    id: id.to_string(),
                    a: 1,
                    e: 1,
                })
                .collect()
        };
        let whole = || Out {
            took: Vec::new(),
            room: None,
            flush_fails: false,
        };
        let no_cap = |what: &str, text: &str| {
            for cap in [HANDED_CAP, COMMIT_CAP] {
                assert!(!text.contains(cap), "{what} shows a cap: {text}");
            }
        };

        for json in [false, true] {
            let (code, stdout, stderr, lines) = run(json, Some(serving(&[one, two], &[])), whole());
            assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
            no_cap("stdout", &stdout);
            no_cap("stderr", &stderr);
            no_cap("the lines it sent", &lines.join("\n"));
            assert!(lines[0].contains("inbox_receipts"), "{lines:?}");
            assert!(lines[1].contains("inbox_receipt_fetch") && lines[1].contains("\"cli\""));
            if json {
                let printed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
                for message in printed["messages"].as_array().unwrap() {
                    let mut keys: Vec<&str> = message
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(String::as_str)
                        .collect();
                    keys.sort();
                    assert_eq!(keys, ["from", "id", "subject", "text"], "{message}");
                }
                assert_eq!(printed["messages"].as_array().unwrap().len(), 2);
            } else {
                assert!(stdout.starts_with(AGENT_FRAMING), "{stdout}");
                assert!(stdout.contains("body of m-k3xzq2ab-1a2b3c4d5e6f7081"));
            }
            let reported = controls(&lines);
            assert_eq!(reported.len(), 1, "{lines:?}");
            assert_eq!(reported[0].committed, ats(&[one, two]));
        }

        let (code, stdout, stderr, lines) = run(
            false,
            Some(serving(&[one, two], &[])),
            Out {
                took: Vec::new(),
                room: Some(0),
                flush_fails: false,
            },
        );
        assert_eq!(code, ExitCode::FAILURE);
        assert!(stdout.is_empty());
        assert!(stderr.contains(CLI_NOTHING_PRINTED), "{stderr}");
        assert_eq!(
            controls(&lines)[0].not_handed,
            ats(&[one, two]),
            "zero bytes"
        );

        for (out, what) in [
            (
                Out {
                    took: Vec::new(),
                    room: Some(AGENT_FRAMING.len() + 20),
                    flush_fails: false,
                },
                "a prefix",
            ),
            (
                Out {
                    took: Vec::new(),
                    room: None,
                    flush_fails: true,
                },
                "a failed flush",
            ),
        ] {
            let (code, _, stderr, lines) = run(false, Some(serving(&[one], &[])), out);
            assert_eq!(code, ExitCode::FAILURE, "{what}");
            assert!(stderr.contains(CLI_CUT_SHORT), "{what}: {stderr}");
            no_cap(what, &stderr);
            let reported = controls(&lines);
            assert_eq!(reported[0].handed, ats(&[one]), "{what}: possibly handed");
            assert!(reported[0].committed.is_empty(), "{what}");
        }

        let revoke = format!(r#"{{"revoke":[{{"id":"{one}","a":1,"e":1}}]}}"#);
        let (code, stdout, stderr, lines) = run(false, Some(serving(&[one], &[revoke])), whole());
        assert_eq!(code, ExitCode::FAILURE);
        assert!(!stdout.contains("body of"), "{stdout}");
        assert!(stderr.contains(CLI_NOTHING_PRINTED), "{stderr}");
        assert_eq!(controls(&lines)[0].not_handed, ats(&[one]), "revoked first");

        let only_asked = |lines: &[String]| {
            assert_eq!(lines.len(), 1, "only the question: {lines:?}");
            assert!(lines[0].contains("inbox_receipts"));
        };
        let (code, stdout, stderr, lines) = run(false, Some(Vec::new()), whole());
        assert_eq!(code, ExitCode::FAILURE);
        assert!(stdout.is_empty());
        assert!(stderr.contains(CLI_RECEIPTS_TIMED_OUT), "silent: {stderr}");
        only_asked(&lines);
        let (code, _, stderr, lines) = run(
            false,
            Some(vec![r#"{"one_shot_receipts":1}"#.to_string()]),
            whole(),
        );
        assert_eq!(code, ExitCode::FAILURE);
        assert!(
            stderr.contains(CLI_RECEIPTS_UNSUPPORTED),
            "part 2's deck: {stderr}"
        );
        only_asked(&lines);
        let mut asked_only = serving(&[], &[]);
        asked_only.truncate(1);
        let (code, _, stderr, lines) = run(false, Some(asked_only), whole());
        assert_eq!(code, ExitCode::FAILURE);
        assert!(
            stderr.contains(CLI_FETCH_TIMED_OUT),
            "after the fetch: {stderr}"
        );
        assert_ne!(CLI_FETCH_TIMED_OUT, CLI_RECEIPTS_TIMED_OUT);
        assert!(!CLI_FETCH_TIMED_OUT.contains("no message was taken"));
        assert!(lines[1].contains("inbox_receipt_fetch"), "{lines:?}");
    }

    /// Scenario: Run `inbox fetch --json` against a stand-in deck that hands it three messages and refuses the grant of the first, the middle or the last. Each time stdout parses as JSON listing exactly the other two, each with only its public keys, and the deck is told those two committed and the refused one not handed. With every grant refused, stdout is an empty list and nothing commits.
    #[cfg(unix)]
    #[spec("inbox/cli/007")]
    #[test]
    fn inbox_cli_007_a_fenced_out_message_leaves_the_json_whole() {
        use crate::event::InboxStreamControl;
        let ids = [
            "m-k3xzq2ab-0f3c9a71d2e4b850",
            "m-k3xzq2ab-1a2b3c4d5e6f7081",
            "m-k3xzq2ab-2b3c4d5e6f708192",
        ];
        let at = |id: &str| crate::event::InboxAttemptRef {
            id: id.to_string(),
            a: 1,
            e: 1,
        };
        let mut refusals: Vec<Vec<usize>> = (0..ids.len()).map(|i| vec![i]).collect();
        refusals.push((0..ids.len()).collect());
        for refused in refusals {
            let mut answers = serving(&ids, &[]);
            for &i in &refused {
                answers[2 + i] = format!(r#"{{"grant":{{"q":{},"ok":false}}}}"#, i + 1);
            }
            let dir = crate::test_temp::tempdir().unwrap();
            let (path, deck) =
                crate::inbox_oneshot::tests::stand_in_deck(dir.path(), vec![Some(answers)]);
            let mut out = Out {
                took: Vec::new(),
                room: None,
                flush_fails: false,
            };
            let mut err = Vec::new();
            fetch_one_shot(
                true,
                Identity {
                    pane_id: "p".to_string(),
                    agent_id: Some("a".to_string()),
                    token: Some("t".to_string()),
                },
                &path,
                Duration::from_millis(400),
                Ok(&mut out),
                &|| crate::inbox_oneshot::Pinned::Unpinned,
                &mut err,
            );
            let lines: Vec<String> = deck.join().unwrap().into_iter().flatten().collect();
            let stdout = String::from_utf8(out.took).unwrap();
            let printed: serde_json::Value = serde_json::from_str(&stdout)
                .unwrap_or_else(|error| panic!("refused {refused:?}: {error}: {stdout}"));
            let kept: Vec<&str> = (0..ids.len())
                .filter(|i| !refused.contains(i))
                .map(|i| ids[i])
                .collect();
            let listed: Vec<&str> = printed["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|message| {
                    let mut keys: Vec<&str> = message
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(String::as_str)
                        .collect();
                    keys.sort();
                    assert_eq!(keys, ["from", "id", "subject", "text"], "{message}");
                    message["id"].as_str().unwrap()
                })
                .collect();
            assert_eq!(listed, kept, "refused {refused:?}");
            let reported: Vec<InboxStreamControl> = lines
                .iter()
                .filter(|line| !line.contains("message_type") && !line.contains("\"may\""))
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            assert_eq!(reported.len(), 1, "{lines:?}");
            assert_eq!(
                reported[0].committed,
                kept.iter().map(|id| at(id)).collect::<Vec<_>>(),
                "refused {refused:?}"
            );
            assert_eq!(
                reported[0].not_handed,
                refused.iter().map(|&i| at(ids[i])).collect::<Vec<_>>(),
                "refused {refused:?}"
            );
        }
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
