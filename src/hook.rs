use std::collections::HashMap;
use std::io::Read as _;
use std::io::Write as _;
use std::process::ExitCode;

use chrono::Utc;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::agent_pty::{DOT_AGENT_DECK_AGENT_ID, DOT_AGENT_DECK_PANE_ID};
use crate::endpoint_resolve::client_socket_path;
use crate::event::{AgentEvent, AgentType, EventType};

#[derive(Debug, Default, Deserialize)]
struct ClaudeCodeHookInput {
    session_id: String,
    hook_event_name: String,
    cwd: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    tool_use_id: Option<String>,
    prompt: Option<String>,
    // Claude Code's native `SessionStart` hook carries a `source` field
    // (`"startup"`/`"resume"`/`"compact"`/`"clear"`) that today is silently
    // absorbed into `_extra` and never read. A NAMED field, not routed
    // through `_extra`/`metadata` — see `build_event_typed`'s narrow
    // forwarding of it below.
    //
    // `lenient_string` degrades a non-string shape (object, number, bool,
    // array) to `None` instead of failing the whole payload decode: `handle_hook`
    // swallows a decode error silently (`Err(_) => return ExitCode::SUCCESS`),
    // so a strict `Option<String>` would blackout the WHOLE event over an
    // unexpected `source` shape, not just lose the field.
    #[serde(default, deserialize_with = "lenient_string")]
    source: Option<String>,
    // Issue #714: named for the same reason as `source`, and lenient for the
    // same reason. `error` and `last_assistant_message` ride Claude Code's
    // `StopFailure`; `transcript_path` rides every Claude and Codex event (the
    // `StopFailure` classifier reads it for Claude, and the daemon's Codex
    // rollout tailer for Codex); `notification_type` rides Claude's
    // `Notification`; `turn_id` rides Codex's `UserPromptSubmit`.
    #[serde(default, deserialize_with = "lenient_string")]
    error: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    transcript_path: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    last_assistant_message: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    notification_type: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    turn_id: Option<String>,
    // Issue #1354: the payload's own `agent_id`, which Claude Code (and Codex)
    // set ONLY on a hook that fires inside a subagent. Renamed on the Rust side
    // because `AgentEvent::agent_id` is the deck's id for the spawned process,
    // an unrelated thing. Lenient for the same reason as `source`: a strange
    // shape must cost this field, not the whole event.
    #[serde(default, rename = "agent_id", deserialize_with = "lenient_string")]
    subagent_id: Option<String>,
    #[serde(flatten)]
    _extra: HashMap<String, Value>,
}

/// A non-string value (object, number, bool, array) degrades to `None` rather
/// than failing the whole payload decode. `null` and a missing key already
/// decode to `None` via `#[serde(default)]`; this only widens the tolerance
/// to non-string, non-null shapes. See [`ClaudeCodeHookInput::source`].
fn lenient_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.and_then(|v| v.as_str().map(str::to_owned)))
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct OpenCodeHookInput {
    session_id: String,
    event: String,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    status: Option<String>,
    cwd: Option<String>,
    prompt: Option<String>,
    // Issue #714: the structured fields of a `session.error`, as the deck's
    // plugin forwards them (`crate::opencode_manage`). All lenient: an
    // unexpected shape degrades to "no field", which classifies as `Error`,
    // today's behaviour, instead of dropping the whole event.
    #[serde(default, deserialize_with = "lenient_string")]
    error_name: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    error_message: Option<String>,
    // The response body's marker keys only, parsed from the WHOLE body by the
    // plugin (`crate::opencode_manage`), never a truncated copy of it.
    #[serde(default, deserialize_with = "lenient_object")]
    response_markers: Option<Value>,
    #[serde(default, deserialize_with = "lenient_string_map")]
    response_headers: HashMap<String, String>,
    // PRD #1542: the request id `permission.replied`, `question.replied` and
    // `question.rejected` name, so the deck clears exactly that question.
    #[serde(default, deserialize_with = "lenient_string")]
    request_id: Option<String>,
    #[serde(flatten)]
    _extra: HashMap<String, Value>,
}

/// [`lenient_string`] for a JSON object: anything else is `None`.
fn lenient_object<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.filter(Value::is_object))
}

/// [`lenient_string`] for a string map: anything but a JSON object is empty,
/// and a non-string value is dropped.
fn lenient_string_map<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<HashMap<String, String>, D::Error> {
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::Object(map)) => map
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|v| (k.to_ascii_lowercase(), v.to_owned())))
            .collect(),
        _ => HashMap::new(),
    })
}

pub fn handle_hook(agent: &str) -> ExitCode {
    let input = match read_stdin() {
        Some(s) if !s.is_empty() => s,
        _ => return ExitCode::SUCCESS,
    };

    let event = match agent {
        "opencode" => {
            let hook_input: OpenCodeHookInput = match serde_json::from_str(&input) {
                Ok(v) => v,
                Err(_) => return ExitCode::SUCCESS,
            };
            build_opencode_event(hook_input)
        }
        // PRD #20 W1: Codex ships a Claude-Code-compatible hooks engine, so its
        // command hooks POST the SAME stdin JSON shape as Claude
        // ([`ClaudeCodeHookInput`]). We reuse the whole ingestion path, only
        // stamping [`AgentType::Codex`] and letting the Codex-aware
        // `extract_tool_detail` arms (`shell`, `apply_patch`) sharpen the detail.
        "codex" => {
            let hook_input: ClaudeCodeHookInput = match serde_json::from_str(&input) {
                Ok(v) => v,
                Err(_) => return ExitCode::SUCCESS,
            };
            build_event_typed(hook_input, AgentType::Codex)
        }
        // Devin CLI ships a Claude-Code-compatible hooks engine too, so its
        // command hooks POST the SAME stdin JSON shape ([`ClaudeCodeHookInput`])
        // and reuse the whole ingestion path, only stamping
        // [`AgentType::Devin`]. Its `exec` tool carries a plain-string
        // `command`, which the `extract_tool_detail` `"exec"` arm sharpens.
        "devin" => {
            let hook_input: ClaudeCodeHookInput = match serde_json::from_str(&input) {
                Ok(v) => v,
                Err(_) => return ExitCode::SUCCESS,
            };
            build_event_typed(hook_input, AgentType::Devin)
        }
        _ => {
            let hook_input: ClaudeCodeHookInput = match serde_json::from_str(&input) {
                Ok(v) => v,
                Err(_) => return ExitCode::SUCCESS,
            };
            build_event_typed(hook_input, AgentType::ClaudeCode)
        }
    };

    let event = match event {
        Some(e) => e,
        None => return ExitCode::SUCCESS,
    };

    // PRD #1542: a Claude Code question the deck can answer through this hook
    // is HELD — the hook waits for the daemon's decision and prints it.
    if !matches!(agent, "opencode" | "codex" | "devin")
        && let Some(held) = claude_held_question(&event, &input)
    {
        let reply = hold_question(&event, CLAUDE_HOLD_DEADLINE);
        if let Some(decision) = reply.as_ref().and_then(|reply| {
            crate::question::claude_decision(
                &held.question,
                held.tool_input.as_ref(),
                held.permission_suggestions.as_ref(),
                reply,
            )
        }) {
            let mut out = std::io::stdout();
            let _ = writeln!(out, "{decision}");
            let _ = out.flush();
        }
        send_plain_if_refused(event, reply.as_ref());
        return ExitCode::SUCCESS;
    }

    // PRD #1542 (audit A9): a frame that raises or clears a question goes
    // through the daemon's provenance gate as an unheld `question` message —
    // a bare event carrying either is applied as a plain status.
    if carries_question_metadata(&event) {
        send_question_unheld_at(&client_socket_path(), event, UNHELD_QUESTION_WAIT);
        return ExitCode::SUCCESS;
    }

    let json = match serde_json::to_string(&event) {
        Ok(j) => j,
        Err(_) => return ExitCode::SUCCESS,
    };

    let _ = send_to_socket(&json);
    ExitCode::SUCCESS
}

/// How long a hook waits for the daemon to acknowledge an unheld question. A
/// daemon that knows the message answers as soon as it has applied it; this
/// bound is what an OLDER daemon, which logs the message malformed and never
/// answers, costs before the hook falls back to a plain event. Kept short
/// because Codex draws its approval prompt only after its `PermissionRequest`
/// hook exits [observed on 0.160.0].
const UNHELD_QUESTION_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether `event` raises a question or reports one answered — the metadata
/// the daemon accepts only from a message its provenance gate attested.
fn carries_question_metadata(event: &AgentEvent) -> bool {
    event
        .metadata
        .contains_key(crate::event::QUESTION_METADATA_KEY)
        || event
            .metadata
            .contains_key(crate::event::QUESTION_RESOLVED_METADATA_KEY)
}

/// What [`send_question_unheld_at`] did.
#[derive(Debug, PartialEq, Eq)]
enum UnheldSend {
    /// The daemon applied it through the provenance gate.
    Attested,
    /// Refused by the gate, or no daemon answered (an older daemon, or none):
    /// sent again as a plain event without its question metadata, so the
    /// status still lands exactly as it did before the deck knew questions.
    Plain,
}

/// PRD #1542 (audit A9): send `event` — a question the deck answers by keys
/// (Codex, Devin, Claude Code's plan approval) or an agent's own "answered"
/// report (OpenCode's `permission.replied` / `question.replied` /
/// `question.rejected`) — as an unheld `question` message carrying this pane's
/// hook token, and wait up to `wait` for the daemon's one reply line.
fn send_question_unheld_at(
    path: &std::path::Path,
    mut event: AgentEvent,
    wait: std::time::Duration,
) -> UnheldSend {
    if let Some(pane_id) = event.pane_id.clone() {
        let signal = crate::event::DaemonMessage::Question(crate::event::QuestionSignal {
            pane_id,
            token: crate::hook_provenance::token_from_env(),
            event: event.clone(),
            hold: false,
        });
        if let Ok(json) = serde_json::to_string(&signal)
            && let (SocketReply::Line(line), _) =
                request_from_socket_at_detailed_with(path, &json, Some(wait), true)
            && let Ok(reply) = serde_json::from_str::<crate::question::QuestionReply>(&line)
            && !reply.refused()
        {
            return UnheldSend::Attested;
        }
    }
    event.metadata.remove(crate::event::QUESTION_METADATA_KEY);
    event
        .metadata
        .remove(crate::event::QUESTION_RESOLVED_METADATA_KEY);
    if let Ok(json) = serde_json::to_string(&event) {
        let _ = send_to_socket_at(path, &json);
    }
    UnheldSend::Plain
}

/// PRD #1542: how long a held Claude Code `PermissionRequest` hook waits for
/// the daemon — 3570 s, ending 30 s BEFORE the 3600 s `timeout` the deck
/// installs the hook with
/// ([`crate::hooks_manage::PERMISSION_REQUEST_HOOK_TIMEOUT_SECS`]), so the hook
/// exits on its own before Claude Code cancels it. Either way Claude Code then
/// behaves as if there were no hook, and its dialog, on screen all along, is
/// still answerable by keyboard [observed on 2.1.289].
pub const CLAUDE_HOLD_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(crate::hooks_manage::PERMISSION_REQUEST_HOOK_TIMEOUT_SECS - 30);

/// How long an `await-answer` child waits. The plugin and the extension stop it
/// as soon as the agent reports the question answered another way, so this is
/// only a backstop.
pub const AWAIT_ANSWER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3600);

/// What a held Claude Code hook needs to turn the daemon's answer into its
/// decision: the question, and the payload fields the decision echoes back.
struct HeldClaudeQuestion {
    question: crate::question::PendingQuestion,
    tool_input: Option<Value>,
    permission_suggestions: Option<Value>,
}

/// The held question this Claude Code hook event carries, if any: a
/// `PermissionRequest` whose question is answered through this hook (not a
/// plan approval, which is answered by keys), from a deck pane.
fn claude_held_question(event: &AgentEvent, raw: &str) -> Option<HeldClaudeQuestion> {
    if event.event_type != EventType::PermissionRequest || event.pane_id.is_none() {
        return None;
    }
    let question = event.question()?;
    if question.channel != crate::question::AnswerChannel::Held {
        return None;
    }
    let payload: Value = serde_json::from_str(raw).ok()?;
    Some(HeldClaudeQuestion {
        question,
        tool_input: payload.get("tool_input").cloned(),
        permission_suggestions: payload.get("permission_suggestions").cloned(),
    })
}

/// PRD #1542: send `event` as a held [`crate::event::DaemonMessage::Question`]
/// and wait up to `deadline` for the daemon's one reply line. `None` on an
/// unreachable or older daemon (which answers nothing, and closes the
/// connection only once its own idle bound passes), a closed connection or an
/// unparseable line. A caller acts on the reply only through
/// [`crate::question::QuestionReply::answers_for`], which also refuses a reply
/// naming another question.
fn hold_question(
    event: &AgentEvent,
    deadline: std::time::Duration,
) -> Option<crate::question::QuestionReply> {
    hold_question_at(&client_socket_path(), event, deadline)
}

/// PRD #1542: when the daemon's provenance gate refused the held question, the
/// event it carried was not applied — send it again as a plain event, without
/// the question, so the card still reads Needs Input, exactly as it would have
/// before the deck could answer questions.
fn send_plain_if_refused(mut event: AgentEvent, reply: Option<&crate::question::QuestionReply>) {
    if reply.is_some_and(crate::question::QuestionReply::refused) {
        event.metadata.remove(crate::event::QUESTION_METADATA_KEY);
        if let Ok(json) = serde_json::to_string(&event) {
            let _ = send_to_socket(&json);
        }
    }
}

fn hold_question_at(
    path: &std::path::Path,
    event: &AgentEvent,
    deadline: std::time::Duration,
) -> Option<crate::question::QuestionReply> {
    let signal = crate::event::DaemonMessage::Question(crate::event::QuestionSignal {
        pane_id: event.pane_id.clone()?,
        token: crate::hook_provenance::token_from_env(),
        event: event.clone(),
        hold: true,
    });
    let json = serde_json::to_string(&signal).ok()?;
    let SocketReply::Line(line) = request_held_at(path, &json, deadline) else {
        return None;
    };
    serde_json::from_str(&line).ok()
}

/// PRD #1542: the `await-answer` verb, for producers that are not a hook
/// process of their own — the OpenCode plugin and the Pi extension. Builds the
/// question from what the producer passed (`stdin` for OpenCode, `--question`
/// for Pi, whose exec helper has no stdin), holds it with the daemon, and
/// prints ONE line: what the producer should do with the answer
/// ([`crate::question::opencode_reply`], [`crate::question::pi_value`]).
/// Prints nothing — the producer then leaves the agent's own prompt alone — on
/// every other outcome, and always exits 0.
pub fn handle_await_answer(agent: &str, question_arg: Option<&str>) -> ExitCode {
    let raw = match question_arg {
        Some(arg) => arg.to_string(),
        None => match read_stdin() {
            Some(s) if !s.is_empty() => s,
            _ => return ExitCode::SUCCESS,
        },
    };
    let line = match agent {
        "opencode" => opencode_await_answer(&raw),
        "pi" => pi_await_answer(&raw),
        _ => None,
    };
    if let Some(line) = line {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
    ExitCode::SUCCESS
}

/// The OpenCode plugin's description of a question: the event's type and its
/// raw `properties`, plus the session the plugin normalised.
#[derive(Debug, Deserialize)]
struct OpenCodeQuestionInput {
    session_id: String,
    event: String,
    #[serde(default)]
    properties: Value,
    #[serde(default)]
    cwd: Option<String>,
}

/// The OpenCode `await-answer` event and question for `raw`, or `None` when it
/// describes neither a permission nor a question.
fn opencode_question_event(
    raw: &str,
) -> Option<(AgentEvent, crate::question::PendingQuestion, Value)> {
    let input: OpenCodeQuestionInput = serde_json::from_str(raw).ok()?;
    let now = Utc::now().timestamp_millis();
    let (question, event_type) = match input.event.as_str() {
        "permission.asked" => (
            crate::question::opencode_permission_asked(&input.properties, now)?,
            EventType::PermissionRequest,
        ),
        "question.asked" => (
            crate::question::opencode_question_asked(&input.properties, now)?,
            EventType::WaitingForInput,
        ),
        _ => return None,
    };
    let mut event = producer_event(input.session_id, AgentType::OpenCode, event_type, input.cwd);
    if let Some(tool) = &question.tool {
        event.tool_name = Some(tool.name.clone());
        event.tool_detail = tool.detail.clone();
    }
    event.set_question(&question);
    Some((event, question, input.properties))
}

fn opencode_await_answer(raw: &str) -> Option<Value> {
    let (event, question, properties) = opencode_question_event(raw)?;
    let reply = hold_question(&event, AWAIT_ANSWER_DEADLINE);
    send_plain_if_refused(event, reply.as_ref());
    crate::question::opencode_reply(&question, &properties, &reply?)
}

fn pi_await_answer(raw: &str) -> Option<Value> {
    let dialog: crate::question::PiDialog = serde_json::from_str(raw).ok()?;
    let question = crate::question::pi_dialog(&dialog, Utc::now().timestamp_millis())?;
    let pane_id = std::env::var(DOT_AGENT_DECK_PANE_ID).ok()?;
    // The session the `agent-event` verb reports Pi's status under, so the
    // question lands on the same card.
    let mut event = producer_event(
        format!("{pane_id}-session"),
        AgentType::Pi,
        EventType::WaitingForInput,
        None,
    );
    event.set_question(&question);
    let reply = hold_question(&event, AWAIT_ANSWER_DEADLINE);
    send_plain_if_refused(event, reply.as_ref());
    crate::question::pi_value(&dialog, &question, &reply?)
}

/// A bare event from this pane, as every producer here stamps one.
fn producer_event(
    session_id: String,
    agent_type: AgentType,
    event_type: EventType,
    cwd: Option<String>,
) -> AgentEvent {
    AgentEvent {
        session_id,
        agent_type,
        event_type,
        tool_name: None,
        tool_detail: None,
        cwd,
        timestamp: Utc::now(),
        user_prompt: None,
        metadata: HashMap::new(),
        pane_id: std::env::var(DOT_AGENT_DECK_PANE_ID).ok(),
        agent_id: std::env::var(DOT_AGENT_DECK_AGENT_ID).ok(),
        agent_version: None,
        schema_version: None,
        live_target: None,
    }
}

fn read_stdin() -> Option<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).ok()?;
    Some(buf)
}

fn map_event_type(hook_event_name: &str) -> Option<EventType> {
    match hook_event_name {
        "SessionStart" => Some(EventType::SessionStart),
        "SessionEnd" => Some(EventType::SessionEnd),
        "UserPromptSubmit" => Some(EventType::Thinking),
        "PreToolUse" => Some(EventType::ToolStart),
        "PostToolUse" => Some(EventType::ToolEnd),
        "Notification" => Some(EventType::WaitingForInput),
        "PermissionRequest" => Some(EventType::PermissionRequest),
        "Stop" => Some(EventType::Idle),
        // Issue #714: Claude Code fires `StopFailure` *instead of* `Stop` when an
        // API error ends the turn (hook metadata in 2.1.283). It lands here as
        // `Error`, and [`build_event_typed`] upgrades a Claude quota failure to
        // [`EventType::QuotaBlocked`]. Codex has no failure hook: its errored
        // turn runs no `Stop` hook (`core/src/session/turn.rs`, `rust-v0.156.1`),
        // which is why Codex is read from its rollout
        // (`crate::codex_rollout_tail`). A failed TOOL call surfaces through a
        // FAILED `PostToolUse` (`tool_response` reports a non-zero exit / error),
        // which [`build_event_typed`] promotes to [`EventType::Error`] — see
        // [`tool_response_is_failure`].
        "StopFailure" => Some(EventType::Error),
        "PreCompact" => Some(EventType::Compacting),
        "PostCompact" => Some(EventType::Thinking),
        // Devin's spelling of the post-compaction event. Devin fires ONLY the
        // post event (it has no `PreCompact`), so this is the sole compaction
        // signal a Devin session produces.
        "PostCompaction" => Some(EventType::Thinking),
        "SubagentStart" => Some(EventType::SubagentStart),
        "SubagentStop" => Some(EventType::SubagentStop),
        // PRD #1542: Codex fires `Interrupt` when a turn is interrupted — after
        // a keyboard "No" on its approval prompt [observed on 0.160.0], which
        // otherwise leaves the card on Needs Input. An interrupted turn has
        // ended, so it reads as `Stop` does.
        "Interrupt" => Some(EventType::Idle),
        _ => None,
    }
}

/// PRD #20 W3-Pass-2 (finding #9): whether a `PostToolUse` `tool_response`
/// reports a FAILED tool call, so the native-hook path can surface a mid-session
/// [`EventType::Error`] instead of a plain `ToolEnd`. Uses Codex's REAL response
/// shapes: shell/`Bash` returns a STRING beginning with a `Exit code: <n>` line
/// (n != 0 is a failure; a success returns an empty string or `Exit code: 0`),
/// while structured tools return an OBJECT (a `completed`/`success` status is OK;
/// an explicit `failed`/`error` status or a non-zero `exit_code` is a failure).
/// Anything without a clear failure signal is treated as success, so an ordinary
/// completed tool never false-positives into Error.
fn tool_response_is_failure(response: &Value) -> bool {
    match response {
        Value::String(text) => exit_code_from_text(text).is_some_and(|code| code != 0),
        Value::Object(map) => {
            if let Some(status) = map.get("status").and_then(Value::as_str) {
                let status = status.to_ascii_lowercase();
                if status == "failed" || status == "error" {
                    return true;
                }
            }
            if let Some(code) = map.get("exit_code").and_then(Value::as_i64)
                && code != 0
            {
                return true;
            }
            false
        }
        _ => false,
    }
}

/// Parse the integer from a leading `Exit code: <n>` line in a Codex shell
/// `tool_response` string (case-insensitive on the label). Returns `None` when no
/// such line is present so a response without an exit marker is not treated as a
/// failure.
fn exit_code_from_text(text: &str) -> Option<i64> {
    for line in text.lines() {
        let line = line.trim();
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("exit code:")
            && let Ok(code) = rest.trim().parse::<i64>()
        {
            return Some(code);
        }
    }
    None
}

fn extract_tool_detail(tool_name: Option<&str>, tool_input: Option<&Value>) -> Option<String> {
    let input = tool_input?.as_object()?;
    let detail = match tool_name? {
        "Bash" => {
            let cmd = input.get("command")?.as_str()?;
            let first_line = cmd.lines().next().unwrap_or(cmd);
            truncate(first_line, 120)
        }
        "Read" | "Edit" | "Write" => input.get("file_path")?.as_str()?.to_string(),
        "Grep" | "Glob" => input.get("pattern")?.as_str()?.to_string(),
        "Agent" => input.get("description")?.as_str()?.to_string(),
        // PRD #20 W1 — Codex-specific tool shapes. Codex's `shell` tool carries
        // its `command` as an ARGV ARRAY (e.g. `["/bin/sh","-lc","touch x"]`),
        // not a Claude-style command STRING; join it so the human detail still
        // shows the real command. `apply_patch` carries a `*** … File: <path>`
        // patch envelope; surface the file path.
        "shell" => {
            let joined = codex_shell_command(input.get("command")?)?;
            let first_line = joined.lines().next().unwrap_or(&joined).to_string();
            truncate(&first_line, 120)
        }
        // PRD #20 W3-Pass-2 (finding #8): Codex's real `apply_patch` hook input
        // carries the patch envelope in `tool_input.command` (the string Codex
        // actually sends), NOT `patch`. Read `command` first and keep `patch` as
        // a defensive fallback so both the live shape and any older/synthetic
        // shape yield a non-empty detail (the target file path).
        "apply_patch" => {
            let patch = input
                .get("command")
                .and_then(|v| v.as_str())
                .or_else(|| input.get("patch").and_then(|v| v.as_str()))?;
            codex_patch_path(patch)
                .unwrap_or_else(|| truncate(patch.lines().next().unwrap_or(patch), 120))
        }
        // Devin's shell tool is named `exec` and — like Claude's `Bash` — carries
        // a plain-string `command`. Unlike the arms above this one FALLS THROUGH
        // to the generic first-string extraction when `command` is absent: only
        // the tool NAME is documented, so a shape we guessed wrong must still
        // yield a useful detail rather than none. Devin's other tools (`read`,
        // `edit`, `grep`, …) are left to the generic branch for the same reason.
        "exec" => match input.get("command").and_then(|v| v.as_str()) {
            Some(cmd) => truncate(cmd.lines().next().unwrap_or(cmd), 120),
            None => truncate(input.values().find_map(|v| v.as_str())?, 80),
        },
        _ => {
            // First string-valued key
            let val = input.values().find_map(|v| v.as_str())?;
            truncate(val, 80)
        }
    };
    Some(detail)
}

/// Issue #424: delegates to the shared, char-boundary-safe truncation. The
/// former `&s[..max]` PANICKED whenever the cut landed inside a multi-byte
/// character — in this binary that kills the hook process and the event is never
/// emitted at all, which for a `user_prompt` now also means a delivered prompt
/// can never be confirmed. Identical output for ASCII, so nothing else moves.
fn truncate(s: &str, max: usize) -> String {
    crate::prompt_delivery::truncate_on_char_boundary(s, max)
}

/// Record a reported prompt as the text the agent actually submitted, with the
/// producer's paste envelope taken off.
///
/// Issue #1182. A producer does not necessarily report a pasted payload
/// verbatim: Claude Code reports the turn with it wrapped in
/// `<pasted_content id="…">` … `</pasted_content id="…">`, and the deck writes
/// every MULTI-LINE payload as a bracketed paste, so that envelope is on the
/// ordinary path rather than an edge case. Unwrapping it HERE, once, at the
/// boundary where a producer's report enters the deck, is what makes the
/// difference visible where it matters:
///
/// * the TUI card's `Prmt:` line, the desktop overview and the prompt history
///   show the prompt instead of `<pasted_content id="239f">You a…`, which is
///   what the pane rendered for every seeded Claude agent (measured on
///   `prompt/new-pane/016`);
/// * the [`crate::prompt_delivery::USER_PROMPT_MAX_LEN`] budget below is spent
///   on the prompt rather than partly on a wrapper.
///
/// **This does not replace [`crate::prompt_delivery`]'s envelope shape and must
/// not be read as making it unreachable.** The `dot-agent-deck` invoked inside
/// a pane comes from the agent's own hook configuration and can be OLDER than
/// the daemon it reports to — `crate::hook_provenance`'s `missing_token`
/// refusal exists for exactly that population — so a daemon still receives
/// enveloped reports from binaries that predate this, and the matcher still has
/// to read them.
///
/// Unwrapping is refused for anything but a turn that is wholly one paste (see
/// [`crate::prompt_delivery::paste_envelope_payload`]): a turn where someone
/// typed prose AROUND a paste is reported as they composed it.
fn record_submitted_prompt(prompt: &str) -> String {
    // The closing delimiter is stripped ONLY when an opening one was found and
    // accepted. Applying it unconditionally would cut a trailing
    // `</pasted_content …>` off a turn this deliberately declined to unwrap —
    // pinned by `a_turn_that_merely_contains_a_paste_is_recorded_unchanged`,
    // which is what caught it.
    let recorded = match crate::prompt_delivery::paste_envelope_payload(prompt) {
        Some((payload, _)) => crate::prompt_delivery::strip_paste_envelope_close(payload),
        None => prompt,
    };
    truncate(recorded, crate::prompt_delivery::USER_PROMPT_MAX_LEN)
}

/// PRD #20 W1: normalize a Codex `shell` tool's `command` value into a single
/// human-readable command string. Codex passes an ARGV array
/// (`["/bin/sh","-lc","touch x"]`); we join with spaces. A plain string is used
/// verbatim (tolerant of a future shape change). Returns `None` for any other
/// JSON type so classification degrades gracefully.
fn codex_shell_command(command: &Value) -> Option<String> {
    match command {
        Value::Array(parts) => {
            let joined = parts
                .iter()
                .filter_map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            if joined.is_empty() {
                None
            } else {
                Some(joined)
            }
        }
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// PRD #20 W1: extract the target file path from a Codex `apply_patch` patch
/// envelope. Codex uses the `*** Add File: <path>` / `*** Update File: <path>` /
/// `*** Delete File: <path>` marker lines; return the first such path. `None`
/// when no marker is present (the caller then falls back to the first line).
fn codex_patch_path(patch: &str) -> Option<String> {
    for line in patch.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("***")
            && let Some((_, path)) = rest.split_once("File:")
        {
            let path = path.trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// Claude-Code hook builder — the [`AgentType::ClaudeCode`] specialization of
/// [`build_event_typed`]. Kept as a thin wrapper so the existing Claude unit
/// tests (and any Claude caller) stay unchanged.
#[cfg(test)]
fn build_event(input: ClaudeCodeHookInput) -> Option<AgentEvent> {
    build_event_typed(input, AgentType::ClaudeCode)
}

/// Build an [`AgentEvent`] from a Claude-compatible hook payload, stamping the
/// given `agent_type`. PRD #20 W1 parameterized this over the agent type so the
/// Codex hook path (which posts the SAME payload shape) reuses the whole
/// builder, differing only in the stamped identity and the Codex-aware
/// `extract_tool_detail` arms.
fn build_event_typed(input: ClaudeCodeHookInput, agent_type: AgentType) -> Option<AgentEvent> {
    let ClaudeCodeHookInput {
        session_id,
        hook_event_name,
        cwd,
        tool_name,
        tool_input,
        tool_use_id,
        prompt,
        source,
        error,
        transcript_path,
        last_assistant_message,
        notification_type,
        turn_id,
        subagent_id,
        _extra: extra,
    } = input;

    // Issue #1354: only the two agents whose `agent_id` is verified to mean
    // "this hook fired inside a subagent" — Claude Code (its hook-input schema
    // says so, and that it is absent on the main thread) and Codex
    // (`thread_spawn_subagent_hook_context` sets it for a thread-spawned
    // subagent and nothing else). Devin's hook input carries no such field.
    let subagent_id = subagent_id
        .filter(|id| !id.is_empty())
        .filter(|_| matches!(agent_type, AgentType::ClaudeCode | AgentType::Codex));
    let from_subagent = subagent_id.is_some();

    let mut event_type = map_event_type(&hook_event_name)?;
    // PRD #20 W3-Pass-2 (finding #9): a FAILED tool call arrives as an ordinary
    // `PostToolUse` (→ `ToolEnd`) whose `tool_response` reports the failure.
    // Promote it to a mid-session `Error` (emitted before the process exits) so
    // the card surfaces the failure with Working/Idle/Error parity, rather than
    // discarding `tool_response` and showing a benign `ToolEnd`. `tool_response`
    // rides the flattened `_extra` (it is not a first-class field), keeping the
    // existing input/struct shape unchanged.
    //
    // Not for a subagent's call (issue #1354): its failure is the subagent's to
    // handle and reaches the main thread as that subagent's result, and an
    // `Error` asserts the card's status unconditionally — so a background
    // agent's failed call after the turn ended would leave the card on Error
    // exactly the way its unfinished call left it on Working.
    if event_type == EventType::ToolEnd
        && subagent_id.is_none()
        && extra
            .get("tool_response")
            .is_some_and(tool_response_is_failure)
    {
        event_type = EventType::Error;
    }
    let tool_detail = extract_tool_detail(tool_name.as_deref(), tool_input.as_ref());

    let user_prompt = prompt.as_deref().map(record_submitted_prompt);
    let pane_id = std::env::var(DOT_AGENT_DECK_PANE_ID).ok();
    // PRD #92 F9 followup-7: the daemon injects DOT_AGENT_DECK_AGENT_ID
    // on spawn (same pattern as DOT_AGENT_DECK_PANE_ID). Forwarding it
    // here lets the post-respawn dispatch task scope its SessionStart
    // wait to the NEW agent and reject a late SessionStart from the
    // OLD agent that fires within the subscribe→kill window.
    let agent_id = std::env::var(DOT_AGENT_DECK_AGENT_ID).ok();

    let subagent_id_for_question = subagent_id.clone();
    let mut metadata = HashMap::new();
    if let Some(tool_use_id) = tool_use_id {
        metadata.insert("tool_use_id".to_string(), tool_use_id);
    }
    if let Some(subagent_id) = subagent_id {
        metadata.insert(
            crate::event::SUBAGENT_ID_METADATA_KEY.to_string(),
            subagent_id,
        );
    }

    // Issue #424 (reviewer option 3): forward EXPLICIT BOOT PROVENANCE.
    //
    // A launcher that posts a Claude-shaped `SessionStart` for its own bootstrap
    // (`devbox run claude …`, a wrapper script that `exec`s the real agent) can
    // say so with [`crate::event::SESSION_START_ORIGIN_METADATA_KEY`], exactly
    // as `dot-agent-deck wrap` does on its fork-time event (PRD #225 M3).
    // Without this, the whole `metadata` object of a Claude-compatible payload
    // was dropped on the floor here, so such a launcher was INDISTINGUISHABLE
    // from an initialized session — which is what made a boot-time generation
    // change and a `/clear` look identical to the delivery latch, and what was
    // used to argue for the (rejected) forward-tracking rule. See
    // [`crate::state::latch_generation`].
    //
    // Deliberately narrow: ONE key, only on `SessionStart`, only the one value
    // the repo defines. Everything else in an incoming `metadata` object is
    // still ignored, so this cannot become an arbitrary producer-controlled
    // channel into the daemon's event metadata.
    //
    // Issue #243 added the INTERFACE origin values
    // (`WRAPPER_INTERFACE_READY_SESSION_START_ORIGIN`,
    // `WRAPPER_INTERFACE_SETTLED_SESSION_START_ORIGIN`) and neither is forwardable
    // here. The asymmetry is deliberate: boot provenance is a producer CONFESSING
    // that its child is not up yet, which costs it privilege and is therefore safe
    // to believe from anyone; interface readiness is a producer CLAIMING that its
    // child is up, which BUYS privilege — it releases the readiness gate, and the
    // strong value additionally selects which post-readiness buffer is paid over
    // it. So this CLI does not carry it, and should not be taught to.
    //
    // **This narrowing is NOT the trust boundary, and the comment that used to
    // stand here claimed it was.** `build_event_typed` is one of several
    // `AgentEvent` builders, not a chokepoint: the daemon's hook socket also
    // accepts a RAW `AgentEvent` JSON line whose `metadata` map is free-form and
    // unvalidated (`crate::daemon`, and `crate::event::AgentEvent`'s own note that
    // the wrapper rides that same socket). Issue #243's audit reproduced a forged
    // `wrapper_interface_ready` `SessionStart` from a bare `python3` with no deck
    // environment variables at all. Keep the narrowing — it is correct and cheap,
    // and it keeps the Claude-shaped path honest — but do not build an argument on
    // it. The privilege is gated where it is USED, by asking whether this daemon
    // spawned the named agent as a wrapper: see
    // `crate::agent_pty::AgentPtyRegistry::agent_spawned_as_wrapper_host` and its
    // caller in `crate::state::dispatch_one_owned`.
    if event_type == EventType::SessionStart
        && extra
            .get("metadata")
            .and_then(|m| m.get(crate::event::SESSION_START_ORIGIN_METADATA_KEY))
            .and_then(|v| v.as_str())
            == Some(crate::event::WRAPPER_FORK_SESSION_START_ORIGIN)
    {
        metadata.insert(
            crate::event::SESSION_START_ORIGIN_METADATA_KEY.to_string(),
            crate::event::WRAPPER_FORK_SESSION_START_ORIGIN.to_string(),
        );
    }

    // Forward "this SessionStart came from `/clear`", same deliberately
    // narrow shape as the boot-provenance forwarding just above — one key,
    // one value, only on `SessionStart`. Distinct from
    // `SESSION_START_ORIGIN_METADATA_KEY`: that key is wrapper-fork boot
    // provenance, an unrelated concern; this one is Claude Code's own
    // `source` field on its native `SessionStart` hook
    // (`"startup"`/`"resume"`/`"compact"`/`"clear"`), and only the `"clear"`
    // value is ever forwarded — every other `source` value stays dropped.
    //
    // Also gated on `agent_type == AgentType::ClaudeCode`, since this
    // builder is shared by the Codex/Devin/default hook arms (all decode
    // the same `ClaudeCodeHookInput`) and this feature is Claude-Code only.
    // Defense-in-depth: the consumer-side check in
    // `orchestrator_remit_pane_latest_clear_session_start` (`src/ui.rs`) is
    // what actually enforces the scope for a raw `AgentEvent` injected
    // straight onto the hook socket, which bypasses this builder entirely.
    if event_type == EventType::SessionStart
        && agent_type == AgentType::ClaudeCode
        && source.as_deref() == Some(crate::event::CLEAR_SESSION_START_METADATA_VALUE)
    {
        metadata.insert(
            crate::event::CLEAR_SESSION_START_METADATA_KEY.to_string(),
            crate::event::CLEAR_SESSION_START_METADATA_VALUE.to_string(),
        );
    }

    // Store the full bash command (tool_detail truncates).
    if matches!(event_type, EventType::ToolStart)
        && tool_name.as_deref() == Some("Bash")
        && let Some(ref input) = tool_input
        && let Some(cmd) = input.get("command").and_then(|v| v.as_str())
    {
        metadata.insert("bash_command".to_string(), cmd.to_string());
    }

    // Issue #714: a Claude Code turn that an API error ended. Every `error`
    // kind maps to something — `StopFailure` replaces `Stop`, so an unmapped
    // kind would leave the card `Thinking` — and only a quota or credit refusal
    // becomes `QuotaBlocked`; see `crate::quota_signals`.
    //
    // Not for a subagent's (issue #1354): a `StopFailure` fired inside one ends
    // that subagent's run, whose failure reaches the main thread as its
    // result, and the main thread reports its own refusal if it meets one. A
    // `QuotaBlocked` or an `Error` would assert the parent card's status —
    // after the turn ended, for a background agent, with nothing to lift it —
    // so it arrives as the `SubagentStop` it stands in for.
    if hook_event_name == "StopFailure" && from_subagent {
        event_type = EventType::SubagentStop;
    } else if hook_event_name == "StopFailure" && agent_type == AgentType::ClaudeCode {
        let outcome = claude_stop_failure_outcome(error.as_deref(), transcript_path.as_deref());
        if let crate::quota_signals::FailureOutcome::Blocked { kind, resets_at_ms } = outcome {
            event_type = EventType::QuotaBlocked;
            insert_quota_blocked_metadata(
                &mut metadata,
                kind,
                resets_at_ms,
                last_assistant_message.as_deref(),
            );
        }
    }

    // Issue #714: which notification this is, so a Claude `idle_prompt` —
    // fired after every ended turn, a quota-blocked one included — is not read
    // as work evidence (`crate::quota_block::is_work_evidence`).
    if event_type == EventType::WaitingForInput
        && agent_type == AgentType::ClaudeCode
        && let Some(kind) = notification_type.filter(|k| k.len() <= 64)
    {
        metadata.insert(
            crate::quota_block::NOTIFICATION_TYPE_METADATA_KEY.to_string(),
            kind,
        );
    }

    // Issue #714: hand the daemon's Codex rollout tailer the session log to
    // read and the turn to watch (`crate::codex_rollout_tail`): `SessionStart`
    // names the log, `UserPromptSubmit` the log and the turn, and `Stop` the
    // turn that ended normally.
    if agent_type == AgentType::Codex
        && matches!(
            hook_event_name.as_str(),
            "SessionStart" | "UserPromptSubmit" | "Stop"
        )
    {
        if let Some(path) =
            transcript_path.filter(|p| crate::codex_rollout_tail::admissible_path(p))
        {
            metadata.insert(
                crate::codex_rollout_tail::CODEX_TRANSCRIPT_PATH_METADATA_KEY.to_string(),
                path,
            );
        }
        if let Some(turn) = turn_id.filter(|t| crate::codex_rollout_tail::admissible_turn_id(t)) {
            metadata.insert(
                crate::codex_rollout_tail::CODEX_TURN_ID_METADATA_KEY.to_string(),
                turn,
            );
        }
    }

    // PRD #1542: the question this event raises, built from the payload and
    // the agent's option table.
    let question = hook_question(
        &agent_type,
        &hook_event_name,
        tool_name.as_deref(),
        tool_input.as_ref(),
        tool_detail.clone(),
        metadata.get("tool_use_id").map(String::as_str),
        extra.get("permission_suggestions"),
    );

    let mut event = AgentEvent {
        session_id,
        agent_type,
        event_type,
        tool_name,
        tool_detail,
        cwd,
        timestamp: Utc::now(),
        user_prompt,
        metadata,
        pane_id,
        agent_id,
        agent_version: None,
        schema_version: None,
        live_target: None,
    };
    if let Some(mut question) = question {
        question.subagent_id = subagent_id_for_question;
        event.set_question(&question);
    }
    Some(event)
}

/// PRD #1542: the question a Claude-shaped hook payload raises, if any.
///
/// - Claude Code `PermissionRequest` → [`crate::question::claude_permission_request`].
/// - Codex `PermissionRequest` → [`crate::question::codex_permission_request`];
///   Codex `PreToolUse` of `request_user_input` →
///   [`crate::question::codex_request_user_input`].
/// - Devin `PermissionRequest` → [`crate::question::devin_permission_request`].
///
/// The agent's version is not in any of these payloads, so the tables are used
/// as verified (`None`).
fn hook_question(
    agent_type: &AgentType,
    hook_event_name: &str,
    tool_name: Option<&str>,
    tool_input: Option<&Value>,
    tool_detail: Option<String>,
    tool_use_id: Option<&str>,
    permission_suggestions: Option<&Value>,
) -> Option<crate::question::PendingQuestion> {
    let now = Utc::now().timestamp_millis();
    let id = crate::question::mint_question_id;
    match (agent_type, hook_event_name) {
        (AgentType::ClaudeCode, "PermissionRequest") => Some(
            crate::question::claude_permission_request(
                id(),
                tool_name?,
                tool_input,
                tool_detail,
                permission_suggestions,
                now,
                None,
            )
            .question,
        ),
        (AgentType::Codex, "PermissionRequest") => Some(crate::question::codex_permission_request(
            id(),
            tool_name?,
            tool_detail,
            now,
            None,
        )),
        (AgentType::Codex, "PreToolUse") if tool_name == Some("request_user_input") => {
            crate::question::codex_request_user_input(tool_use_id, tool_input, now, None)
        }
        (AgentType::Devin, "PermissionRequest") => Some(crate::question::devin_permission_request(
            id(),
            tool_name?,
            tool_detail,
            now,
            None,
        )),
        _ => None,
    }
}

/// Issue #714: how many times, after the first read, the Claude `StopFailure`
/// hook re-reads the transcript when its last assistant record is not yet the
/// API error, and how long it waits between reads. Whether Claude Code flushes
/// that record before running the hook is unconfirmed, so the hook allows for
/// it; giving up yields `Error`, a false negative, never a false `Blocked`.
const CLAUDE_TRANSCRIPT_RETRIES: u32 = 3;
const CLAUDE_TRANSCRIPT_RETRY_GAP: std::time::Duration = std::time::Duration::from_millis(100);

/// Issue #714: classify a Claude Code `StopFailure`, reading the transcript
/// only when the `error` kind needs it
/// ([`crate::quota_signals::claude_stop_failure_needs_transcript`]).
fn claude_stop_failure_outcome(
    error: Option<&str>,
    transcript_path: Option<&str>,
) -> crate::quota_signals::FailureOutcome {
    use crate::quota_signals::{
        classify_claude_stop_failure, claude_stop_failure_needs_transcript,
        last_claude_api_error_record, read_claude_transcript_tail,
    };
    if !claude_stop_failure_needs_transcript(error) {
        return classify_claude_stop_failure(error, None);
    }
    let Some(path) = transcript_path else {
        return classify_claude_stop_failure(error, None);
    };
    let mut record = None;
    for attempt in 0..=CLAUDE_TRANSCRIPT_RETRIES {
        if attempt > 0 {
            std::thread::sleep(CLAUDE_TRANSCRIPT_RETRY_GAP);
        }
        let Some(tail) = read_claude_transcript_tail(path) else {
            // Refused (not a regular `.jsonl` file): re-reading cannot help.
            break;
        };
        record = last_claude_api_error_record(&tail);
        if record.is_some() {
            break;
        }
    }
    classify_claude_stop_failure(error, record.as_ref())
}

/// Issue #714: the producer's half of the `quota_blocked` contract — the kind,
/// the reset when the provider gave one, and the agent's own message, scrubbed,
/// for display.
fn insert_quota_blocked_metadata(
    metadata: &mut HashMap<String, String>,
    kind: crate::quota_block::BlockedKind,
    resets_at_ms: Option<i64>,
    detail: Option<&str>,
) {
    use crate::quota_block::{
        QUOTA_BLOCKED_DETAIL_METADATA_KEY, QUOTA_BLOCKED_KIND_METADATA_KEY,
        QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY, scrub_detail,
    };
    metadata.insert(
        QUOTA_BLOCKED_KIND_METADATA_KEY.to_string(),
        kind.as_wire().to_string(),
    );
    if let Some(at) = resets_at_ms {
        metadata.insert(
            QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY.to_string(),
            at.to_string(),
        );
    }
    if let Some(detail) = detail.map(scrub_detail).filter(|d| !d.is_empty()) {
        metadata.insert(QUOTA_BLOCKED_DETAIL_METADATA_KEY.to_string(), detail);
    }
}

fn map_opencode_event_type(event: &str, status: Option<&str>) -> Option<EventType> {
    match event {
        "session.created" => Some(EventType::SessionStart),
        "session.deleted" => Some(EventType::SessionEnd),
        "session.idle" => Some(EventType::Idle),
        "session.error" => Some(EventType::Error),
        "session.prompt" => Some(EventType::Thinking),
        "session.status" | "session.status.updated" => {
            let norm = status.map(|s| s.to_ascii_lowercase());
            match norm.as_deref() {
                Some("idle") => Some(EventType::Idle),
                Some("error") => Some(EventType::Error),
                Some("waiting") => Some(EventType::WaitingForInput),
                _ => Some(EventType::Thinking),
            }
        }
        "tool.execute.before" => Some(EventType::ToolStart),
        "tool.execute.after" => Some(EventType::ToolEnd),
        "permission.asked" => Some(EventType::PermissionRequest),
        "permission.replied" => Some(EventType::Thinking),
        // PRD #1542: OpenCode's `question` tool. The plugin forwards the ask
        // through `await-answer`, which builds the event itself; this arm is
        // for a plugin that forwards it as a plain event.
        "question.asked" => Some(EventType::WaitingForInput),
        "question.replied" | "question.rejected" => Some(EventType::Thinking),
        _ => None,
    }
}

pub(crate) fn build_opencode_event(input: OpenCodeHookInput) -> Option<AgentEvent> {
    let mut event_type = map_opencode_event_type(&input.event, input.status.as_deref())?;
    let tool_detail = extract_tool_detail(input.tool_name.as_deref(), input.tool_input.as_ref());
    let user_prompt = input.prompt.as_deref().map(record_submitted_prompt);
    let pane_id = std::env::var(DOT_AGENT_DECK_PANE_ID).ok();
    let agent_id = std::env::var(DOT_AGENT_DECK_AGENT_ID).ok();

    let mut metadata = HashMap::new();
    if matches!(event_type, EventType::PermissionRequest) {
        metadata.insert("permission_state".to_string(), "pending".to_string());
        metadata.insert(
            "tool_use_id".to_string(),
            format!(
                "perm-{}-{}",
                input.session_id,
                Utc::now().timestamp_millis()
            ),
        );
    }

    // PRD #1542: OpenCode reports the question answered, by its request id.
    if matches!(
        input.event.as_str(),
        "permission.replied" | "question.replied" | "question.rejected"
    ) && let Some(request_id) = input
        .request_id
        .as_deref()
        .filter(|id| crate::question::is_valid_question_id(id))
    {
        metadata.insert(
            crate::event::QUESTION_RESOLVED_METADATA_KEY.to_string(),
            request_id.to_string(),
        );
    }

    // Store the full bash command (tool_detail truncates).
    if matches!(event_type, EventType::ToolStart)
        && input.tool_name.as_deref() == Some("Bash")
        && let Some(ref tool_input) = input.tool_input
        && let Some(cmd) = tool_input.get("command").and_then(|v| v.as_str())
    {
        metadata.insert("bash_command".to_string(), cmd.to_string());
    }

    // Issue #714: a `session.error` whose structured fields name a provider
    // quota or credit refusal is a block; every other one stays `Error`. See
    // `crate::quota_signals::classify_opencode_error`.
    if input.event == "session.error" {
        let fields = crate::quota_signals::OpenCodeErrorFields {
            error_name: input.error_name,
            response_markers: input.response_markers,
            response_headers: input.response_headers,
        };
        if let crate::quota_signals::FailureOutcome::Blocked { kind, resets_at_ms } =
            crate::quota_signals::classify_opencode_error(&fields, Utc::now().timestamp_millis())
        {
            event_type = EventType::QuotaBlocked;
            insert_quota_blocked_metadata(
                &mut metadata,
                kind,
                resets_at_ms,
                input.error_message.as_deref(),
            );
        }
    }

    Some(AgentEvent {
        session_id: input.session_id,
        agent_type: AgentType::OpenCode,
        event_type,
        tool_name: input.tool_name,
        tool_detail,
        cwd: input.cwd,
        timestamp: Utc::now(),
        user_prompt,
        metadata,
        pane_id,
        agent_id,
        agent_version: None,
        schema_version: None,
        live_target: None,
    })
}

/// The total-operation budget for a `delegate`'s reply — the same 5s
/// [`GET_SEED_REQUEST_TIMEOUT`] gives `get-seed`, and the value that comment
/// already names as this path's bound.
///
/// PR #466 review: `send_and_await_reply` originally connected, wrote and read
/// with **no** deadline of any kind, and the two platforms were not symmetric
/// about it. Windows' `IpcClient::connect` seeds a 5s default, so the
/// "delivered, unverifiable" story held there; Unix' is a bare
/// `UnixStream::connect` and nothing else, leaving the read unbounded. The
/// half-close covers only an OLD daemon (its line reader hits EOF, its task
/// ends, the write half drops). It does not cover a LIVE daemon that accepts
/// and then answers slowly — and `handle_delegate` runs under
/// `state.read().await` while tokio's `RwLock` is write-preferring, so a queued
/// `state.write().await` (including the one this change adds inside `spawn` for
/// a large orchestration) parks readers behind it. In that window `delegate`
/// blocked with no ceiling: an orchestrator whose `delegate` hangs is the same
/// hung orchestration this change set out to remove, reached through another
/// door.
pub const DELEGATE_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Send a line to the daemon hook socket and await ONE reply line, classifying
/// what came back rather than folding every failure into `None`.
///
/// [`request_from_socket`]'s `None` cannot distinguish "no daemon" from "old
/// daemon that does not answer this verb", which is fine for `get-seed` (both
/// degrade to "no seed") and wrong for `delegate`, where the first is a failure
/// the orchestrator must see and the second must stay a success or every
/// delegate against an older daemon starts reporting a phantom error. Only
/// [`SocketReply::Unreachable`] means the signal was not delivered — and
/// since issue #434 that is something the transport establishes by counting
/// the bytes it wrote, rather than something this sentence asserts on its
/// behalf.
///
/// Deliberately the same transport as `get-seed` — [`request_from_socket_at`],
/// bounded by [`DELEGATE_REPLY_TIMEOUT`] — rather than a second hand-rolled
/// connect/write/read. An earlier draft of this function was exactly that, and
/// it carried no deadline; sharing the one implementation gives `delegate` the
/// total-operation bound, the half-close, the read-exactly-one-line behaviour
/// PRD #163 M4 needed for Windows, and — since issue #435 — a connect step that
/// is inside the deadline rather than beside it, for free.
pub fn send_and_await_reply(json: &str) -> SocketReply {
    request_from_socket_inner(json, Some(DELEGATE_REPLY_TIMEOUT))
}

/// Issue #868: `pane restart`'s own CLI round-trip budget —
/// [`DELEGATE_REPLY_TIMEOUT`] (5s) is smaller than what the respawn spends
/// inside the daemon on a HEALTHY one (`AGENT_TERMINATE_GRACE` +
/// `PANE_CLOSE_SETTLE_TIMEOUT` = up to 9s), and at the time this constant was
/// written a timeout here
/// silently mapped to `ExitCode::SUCCESS` with no output — so a `--force`
/// restart of an agent that ignores SIGTERM, or any recreate-leg restart,
/// would have printed nothing and exited 0 while the outcome was genuinely
/// unknown. That "must stay a success" contract is `delegate`'s, not this
/// verb's: since `40178393` a `NoReply` here is a hard `ExitCode::FAILURE`
/// (see the `SocketReply::NoReply` arm behind `pane restart` in `main.rs`).
///
/// **This budget is DERIVED FROM THE RESPAWN ALONE, and is deliberately not
/// the end-to-end worst case** (issue #1095). Two terms it does not cover, each
/// for its own reason.
///
/// **The issue-#606 recreate leg** (issue #1114). When the respawn finds no
/// record and a close is still holding the pane, it waits out to
/// [`crate::agent_pty::PANE_CLOSE_RECREATE_TIMEOUT`] (30s) rather than the 6s
/// settle window, because giving up there costs the role for the rest of the
/// session while waiting costs only a delayed delegate. Deriving this constant
/// from THAT instead would put a 38s ceiling on every `pane restart`, including
/// the overwhelming majority that never touch a closing pane, and #1114's own
/// measurements say the long wait is reached roughly once in 120 runs at 80x
/// over-subscription — so the ceiling would be paid always to cover a case that
/// is nearly never hit. `PANE_CLOSE_SETTLE_TIMEOUT` remains the right term here
/// precisely because it is what a healthy daemon spends; the residue is the
/// `NoReply` this doc's last paragraph already describes, on a restart the
/// daemon is still carrying out.
/// `handle_restart_role_with_state` blocks on
/// [`crate::agent_pty::AgentPtyRegistry::pane_dispatch_lock`] *before* it
/// respawns, and `dispatch_one_owned` takes that same per-pane lock as its
/// first statement and holds it across `wait_for_session_start`
/// (`SESSION_START_WAIT_TIMEOUT`, 30s) plus one readiness buffer. So a
/// `pane restart <role>` issued while a `clear = true` delegate to that same
/// role is mid-flight waits behind the lock, blows this 14s budget, and
/// surfaces to the caller as [`SocketReply::NoReply`] — a reported failure
/// for a restart the daemon is still holding and will carry out (or refuse,
/// per the post-lock crash recheck) once the lock frees.
///
/// Issue #544 (PR #1398 review): the delegate's own wait for the worker's
/// unsent draft — up to the draft-deferral cap — is NOT in that list. The
/// pointer write sets the dispatch lock down while it waits
/// ([`crate::agent_pty::PaneDispatchHold`]), so a restart landing then
/// proceeds at once and the parked pointer is refused.
///
/// **Sizing the constant to cover that was considered and rejected**, because
/// no compile-time constant can honestly bound it. The buffer term is not a
/// fixed 1s: `dispatch_one_owned` pays one of `DELEGATE_READINESS_BUFFER`
/// (1s), `WRAPPER_INTERFACE_READINESS_BUFFER` (5s) or
/// `NO_SIGNAL_READINESS_BUFFER` (8s) depending on the readiness path — the
/// 8s one is a SKIP of the wait rather than a release from it, and the 1s
/// one also covers the timeout fallback and an unresolved agent. An
/// operator can override the buffer via
/// `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS` up to
/// `MAX_DELEGATE_READINESS_BUFFER` (30s) — a runtime value nothing here can
/// see. The lock also has no cap on queue depth, so queued same-pane
/// delegates compound. A derivation ending in
/// `+ DELEGATE_READINESS_BUFFER` would therefore reproduce exactly the
/// over-claim this paragraph exists to correct, while making a genuinely
/// failing `pane restart` take ~45s to say so.
///
/// The mitigation is the caller-facing text instead: this verb's `NoReply`
/// arm in `main.rs` is deliberately cause-agnostic — it tells the operator
/// the restart "is still in flight and may have already succeeded", and to
/// check the pane before retrying because retrying a successful restart
/// kills and respawns it again. The residue is an over-reported failure with
/// accurate advice attached, not a silent wrong answer.
///
/// [`SPAWN_ROLE_REPLY_TIMEOUT`] is unaffected by any of this:
/// `handle_spawn_role_with_state` takes no dispatch lock.
pub const RESTART_ROLE_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(
    crate::agent_pty::AGENT_TERMINATE_GRACE.as_secs()
        + crate::agent_pty::PANE_CLOSE_SETTLE_TIMEOUT.as_secs()
        + 5,
);

/// [`send_and_await_reply`], but bounded by [`RESTART_ROLE_REPLY_TIMEOUT`]
/// instead of [`DELEGATE_REPLY_TIMEOUT`] — see that constant's doc for why
/// `pane restart` needs a larger budget than `delegate`'s.
pub fn send_and_await_restart_role_reply(json: &str) -> SocketReply {
    request_from_socket_inner(json, Some(RESTART_ROLE_REPLY_TIMEOUT))
}

/// PR #918 review fix round: `pane spawn`'s own CLI round-trip budget. It
/// used to share [`DELEGATE_REPLY_TIMEOUT`] (5s), which was fine while a
/// timeout silently mapped to success; now that a `NoReply` timeout is a hard
/// failure (see [`SocketReply::NoReply`]'s doc), that 5s budget turns a
/// slow-but-successful spawn under load into a false failure. `pane spawn`
/// does not need [`RESTART_ROLE_REPLY_TIMEOUT`]'s full margin, though:
/// `handle_spawn_role_with_state` does no terminate-and-respawn — it's just
/// `spawn_agent` plus two `state.write().await` acquisitions, with none of
/// restart's `AGENT_TERMINATE_GRACE` + `PANE_CLOSE_SETTLE_TIMEOUT` budget. 10s gives comfortable margin over that without inheriting restart's
/// larger budget for a cheaper operation.
pub const SPAWN_ROLE_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// [`send_and_await_reply`], but bounded by [`SPAWN_ROLE_REPLY_TIMEOUT`]
/// instead of [`DELEGATE_REPLY_TIMEOUT`] — see that constant's doc for why
/// `pane spawn` needs a larger budget than `delegate`'s.
pub fn send_and_await_spawn_role_reply(json: &str) -> SocketReply {
    request_from_socket_inner(json, Some(SPAWN_ROLE_REPLY_TIMEOUT))
}

/// Issue #1129: the budget for a fire-and-forget signal's acknowledgement —
/// `work-done` and `dispatch`.
///
/// The same 5s [`DELEGATE_REPLY_TIMEOUT`] gives `delegate`, and deliberately
/// not a smaller number even though this reply is cheaper to produce. Cheaper
/// because the daemon writes it at the provenance gate, **before** the handler
/// runs (`DaemonMessage::provenance_ack_reply`), so it costs one registry lookup
/// and a socket write — where `delegate`'s reply is written after
/// `handle_delegate` has run under the `state` read lock that the 5s exists to
/// cover. So a budget sized for the more expensive of the two is generous here
/// by construction, and the margin is spent only when the daemon cannot answer
/// promptly — wedged, overloaded, or with a full accept queue — which is exactly
/// when a *smaller* budget would convert a slow-but-delivered signal into a
/// reported failure.
///
/// It is also the first deadline these two verbs have ever had. They went
/// through [`send_to_socket`], whose connect and write are unbounded, so a
/// wedged daemon hung the calling agent indefinitely; now it fails in 5s. That
/// is a liveness improvement and a reclassification at once —
/// [`SocketReply::Unreachable`] on a connect that blows the budget — and the
/// CLI reports it exactly as it already reported a failed `send_to_socket`.
pub const SIGNAL_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Send a fire-and-forget hook-socket signal and await the daemon's
/// [`crate::event::SignalAck`], bounded by [`SIGNAL_ACK_TIMEOUT`].
///
/// [`send_and_await_reply`]'s classification is what this needs and why it is
/// reused rather than re-rolled: [`SocketReply::NoReply`] is a daemon that
/// predates the ack, which must stay a success or every `work-done` against an
/// older daemon starts reporting a phantom failure, while
/// [`SocketReply::Unreachable`] is the one case the caller may report as "not
/// delivered".
pub fn send_and_await_signal_ack(json: &str) -> SocketReply {
    request_from_socket_inner(json, Some(SIGNAL_ACK_TIMEOUT))
}

pub fn send_to_socket(json: &str) -> Option<()> {
    send_to_socket_at(&client_socket_path(), json)
}

/// [`send_to_socket`] against an explicit endpoint, rather than one resolved
/// from the environment. Lets the socket tests exercise "no daemon listening
/// at this path" by passing a temp-dir path directly, instead of mutating the
/// process-global `DOT_AGENT_DECK_SOCKET` env var that production
/// `client_socket_path()` reads.
fn send_to_socket_at(path: &std::path::Path, json: &str) -> Option<()> {
    let mut stream = crate::platform::ipc::IpcClient::connect(path).ok()?;
    let msg = format!("{json}\n");
    stream.write_all(msg.as_bytes()).ok()?;
    stream.flush().ok()?;
    Some(())
}

/// Issue #243 audit F3: [`send_to_socket`] with every blocking step bounded by
/// `timeout` — the connect (`IpcClient::connect_timeout`) and the write/flush
/// (`IpcClient::set_timeouts`).
///
/// [`send_to_socket`] is deliberately unbounded and stays that way: its callers
/// are one-shot CLI invocations and wrapper tee threads, where blocking until the
/// daemon accepts is the right trade and dropping an event silently is not. This
/// variant exists for the one caller whose thread must not outlive its usefulness
/// no matter what the daemon is doing — `crate::wrap`'s interface-ready
/// announcement, which fires from the wrapper's supervisory loop.
///
/// Fire-and-forget by design: a failure is a lost readiness event, which costs
/// the readiness gate its fast path and nothing else, so there is no outcome for
/// a caller on a detached thread to act on.
pub fn send_to_socket_bounded(json: &str, timeout: std::time::Duration) {
    let _ = send_to_socket_bounded_at(&client_socket_path(), json, timeout);
}

/// [`send_to_socket_bounded`] against an explicit endpoint. Same rationale as
/// [`send_to_socket_at`]: it lets a test point at a temp-dir path instead of
/// mutating the process-global `DOT_AGENT_DECK_SOCKET`.
fn send_to_socket_bounded_at(
    path: &std::path::Path,
    json: &str,
    timeout: std::time::Duration,
) -> Option<()> {
    let mut stream = crate::platform::ipc::IpcClient::connect_timeout(path, timeout).ok()?;
    // A failure here leaves the stream blocking-without-deadline, which is the
    // pre-#243 behaviour and no worse than not trying; the connect bound has
    // already done the part that matters most.
    let _ = stream.set_timeouts(timeout);
    let msg = format!("{json}\n");
    stream.write_all(msg.as_bytes()).ok()?;
    stream.flush().ok()?;
    Some(())
}

/// A per-read/per-write **idle** timeout (`SO_RCVTIMEO`/`SO_SNDTIMEO` on
/// Unix) applied to the socket used by [`request_from_socket`], bounding how
/// long a single blocking read or write may sit with no bytes moving before
/// failing. An idle timeout alone is not enough on its own, though: it resets
/// on every byte moved, so a peer that keeps trickling single bytes without
/// ever finishing the reply line could still make `get-seed` wait forever
/// even though the socket was never actually silent for a whole read.
/// [`request_from_socket_inner`]'s read loop therefore also re-arms this same
/// duration as a **total-operation** deadline it counts down from, closing
/// that gap.
///
/// 5s stays the right number for the total-operation deadline for the same
/// reason it was right as a per-read one: the daemon's `GetSeed` handler
/// never touches the `state` lock that `delegate`'s reply path contends on —
/// it only reads/clears an in-memory entry in `pty_registry` — so it is
/// strictly cheaper than the `delegate` reply already bounded at
/// [`DELEGATE_REPLY_TIMEOUT`] (5s, above). A caller waiting on
/// `get-seed`'s socket has no reason to be given a longer overall budget than
/// `delegate`'s own reply is allowed, and 5s is still comfortably above the
/// 300ms reply delay `error/socket/004` exercises, so a merely slow (not
/// wedged/adversarial) daemon is not mistaken for an absent one.
const GET_SEED_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// PRD #201: send a line to the daemon hook socket and read ONE line of reply
/// back on the same connection. Used by the read-only `get-seed` verb, for
/// which every failure to get an answer means the same thing. (It was the one
/// hook-socket message expecting a response when this was written; `delegate`
/// joined it in PR #466 and `work-done` / `dispatch` in issue #1129, each
/// through a helper that classifies rather than collapsing — see
/// [`send_and_await_reply`] and [`send_and_await_signal_ack`]. Raw
/// `agent-event` traffic is still answered by nothing.) Returns `None` if the socket is
/// absent/unreadable, if the request write breaks at any point, or if the
/// daemon goes completely silent — or keeps
/// dribbling bytes without ever finishing a reply line — for longer than
/// [`GET_SEED_REQUEST_TIMEOUT`]. Issue #434 split that write failure across
/// [`SocketReply::Unreachable`] and [`SocketReply::NoReply`] by how many
/// bytes left the process, which changes nothing here: this caller collapses
/// both to `None`. The caller (get-seed) treats all of these
/// identically as "no seed", so an older
/// daemon that never replies, a daemon that never even accepts the
/// connection, one that accepts the connection and then stops sending bytes
/// altogether, and one that never stops sending bytes but never completes a
/// line either, all degrade to the PTY-injection safety net rather than
/// hanging or erroring. A blank reply line is returned as
/// `Some(String::new())`.
///
/// [`GET_SEED_REQUEST_TIMEOUT`] bounds the **whole exchange**, not just a
/// single idle read — and since issue #435 that is literally rather than
/// approximately true, because the connect step goes through
/// [`crate::platform::ipc::IpcClient::connect_timeout`] instead of a blocking
/// `connect(2)` that no deadline reached. The read loop measures
/// elapsed wall-clock time against it and shrinks each individual blocking
/// read's own timeout to whatever is left, so a peer that keeps dribbling a
/// byte at a time without ever completing the reply line can no longer keep
/// the read alive indefinitely — a per-read/per-write idle timeout alone
/// resets on every byte moved and therefore never fires against a peer that
/// never goes idle for a whole read. This pass does not add a reply-size cap
/// — a peer can still
/// grow the in-progress `String` for up to the deadline before the read is
/// abandoned, which is a materially smaller exposure than the previous
/// unbounded one but is intentionally left for a follow-up with its own
/// size-cap justification.
pub fn request_from_socket(json: &str) -> Option<String> {
    match request_from_socket_inner(json, Some(GET_SEED_REQUEST_TIMEOUT)) {
        SocketReply::Line(line) => Some(line),
        SocketReply::NoReply | SocketReply::Unreachable => None,
    }
}

/// Outcome of [`request_from_socket_inner`]/[`request_from_socket_at`] —
/// richer than [`request_from_socket`]'s `Option<String>` because a caller
/// that needs to tell "never even reached the daemon" apart from "reached
/// it, but got no confirmation back" can report each honestly instead of
/// collapsing both to `None` the way [`request_from_socket`] does.
///
/// That caller is now real: [`send_and_await_reply`], behind
/// `dot-agent-deck delegate`.
///
/// The line the variants are cut along is **how much of the request left this
/// process**, because that is what a retry decision turns on: a caller may
/// resend only what it knows the daemon cannot already be holding. Issue #434
/// moved a partial write across that line, from [`SocketReply::Unreachable`]
/// to [`SocketReply::NoReply`] — see both variants below.
#[derive(Debug)]
pub enum SocketReply {
    /// Connected, wrote the request, and read a reply line (possibly empty).
    Line(String),
    /// The request may have reached the daemon, and nothing here is evidence
    /// that it did not — so a caller must not resend it blind.
    ///
    /// Two shapes land here. The request went out **in full** and no reply
    /// line came back before the deadline: the daemon closed without
    /// answering (an old daemon that doesn't know this request type), an
    /// individual read timed out, or the total-operation deadline elapsed
    /// while a peer kept dribbling bytes without ever finishing the reply
    /// line. Or — since issue #434 — the write **broke partway through**,
    /// with at least one byte already gone from this process; that case used
    /// to be reported as [`SocketReply::Unreachable`], which claimed more
    /// than the code knew.
    ///
    /// For `delegate` this is the pre-response contract: the verb was
    /// fire-and-forget before the daemon answered it at all, so a daemon that
    /// does not answer must stay a success — handed to the socket,
    /// unverifiable — rather than becoming a phantom failure on every
    /// mixed-version pair. It is deliberately NOT "delivered": a daemon killed
    /// between accept and read also lands here, as does the broken write
    /// above.
    NoReply,
    /// Provably nothing left this process: the connect failed, the socket
    /// could not be armed with the deadline, or the first write failed with
    /// the byte count still at zero. The one case a caller may report as "not
    /// delivered", and the one it may retry without risking a duplicate.
    ///
    /// **Issue #434: this used to be returned for any write failure**, from a
    /// `write_all(…).is_err() || flush().is_err()` whose `Result` cannot say
    /// how far it got. Since issue #419 the socket carries a write timeout, so
    /// a write can fail with part of the line already in the daemon's receive
    /// buffer — and a partial line is not harmlessly ignored on the other
    /// side: the daemon's reader treats a trailing unterminated line as a line
    /// (the EOF arm of [`crate::bounded_read::read_capped_line`], "a trailing
    /// partial line is still a line"), so a write that broke after the last
    /// JSON byte but before the `\n` hands the daemon a complete, actionable
    /// request. Reporting that as "never sent" is what would invite the retry
    /// that double-sends it. [`write_request_line`] tracks the byte count so
    /// this variant means what it says.
    Unreachable,
}

fn request_from_socket_inner(json: &str, timeout: Option<std::time::Duration>) -> SocketReply {
    request_from_socket_at(&client_socket_path(), json, timeout)
}

/// [`request_from_socket_inner`] against an explicit endpoint, rather than one
/// resolved from the environment. Lets the socket tests point a request at a
/// temp-dir stub-daemon socket without mutating the process-global
/// `DOT_AGENT_DECK_SOCKET` env var that production `request_from_socket_inner`
/// reads via `client_socket_path()` — `set_var`/`get_var` races on that var
/// are unsound under a multithreaded test binary regardless of the project's
/// own `STATE_DIR_ENV_LOCK` convention, since production `client_socket_path()`
/// reads it without taking that lock.
fn request_from_socket_at(
    path: &std::path::Path,
    json: &str,
    timeout: Option<std::time::Duration>,
) -> SocketReply {
    request_from_socket_at_detailed(path, json, timeout).0
}

/// [`request_from_socket_at`], plus the [`NoReplyCause`] behind a
/// [`SocketReply::NoReply`] when there is one.
///
/// The extra half is diagnostic only — `request_from_socket_at` above is
/// literally this function's first return value and nothing else, so
/// production behavior is unchanged. Issue #564: the socket tests assert on the reason as well
/// as the outcome, so a future failure prints *which* branch ended the
/// exchange rather than a bare `None` that four different causes could
/// produce.
fn request_from_socket_at_detailed(
    path: &std::path::Path,
    json: &str,
    timeout: Option<std::time::Duration>,
) -> (SocketReply, Option<NoReplyCause>) {
    request_from_socket_at_detailed_with(path, json, timeout, true)
}

/// PRD #1542: a held request — [`request_from_socket_at`] WITHOUT the
/// half-close after the write. The daemon reads the held connection for EOF to
/// learn that the producer stopped waiting (Claude Code kills the hook when the
/// keyboard answers No [observed]), so a half-close would read as exactly that
/// the moment the request landed.
fn request_held_at(
    path: &std::path::Path,
    json: &str,
    timeout: std::time::Duration,
) -> SocketReply {
    request_from_socket_at_detailed_with(path, json, Some(timeout), false).0
}

fn request_from_socket_at_detailed_with(
    path: &std::path::Path,
    json: &str,
    timeout: Option<std::time::Duration>,
    half_close: bool,
) -> (SocketReply, Option<NoReplyCause>) {
    // The total-operation deadline starts here, before connect, rather than
    // being re-armed with a fresh full budget once the connection is
    // established and the request written below. Connect and the write are
    // usually fast on a local Unix/named-pipe socket, but a wedged/overloaded
    // daemon can stall either one, and this is the only way the deadline
    // actually bounds the *whole* exchange the way this function's/
    // `request_from_socket`'s docs describe. The timeout value itself is
    // unchanged — only when the clock starts.
    let deadline = timeout.map(|budget| std::time::Instant::now() + budget);
    // Issue #435: connect through the deadline-aware entry point, not the bare
    // one. `IpcClient::connect` blocks uninterruptibly on Unix when the
    // daemon's accept queue is full, so starting the clock above bounded the
    // *rest* of the exchange while connect itself could still run past the
    // budget indefinitely — the whole-exchange bound this function's docs
    // promise was approximate rather than literal. A connect that blows the
    // budget lands in `Unreachable`, which is the honest classification: the
    // request was never sent.
    let connected = match timeout {
        Some(budget) => crate::platform::ipc::IpcClient::connect_timeout(path, budget),
        None => crate::platform::ipc::IpcClient::connect(path),
    };
    let mut stream = match connected {
        Ok(stream) => stream,
        Err(_) => return (SocketReply::Unreachable, None),
    };
    if let Some(deadline) = deadline {
        // Zero/negative remaining budget: do not hand the socket a zero
        // timeout — on some platforms that means "block forever", the
        // opposite of what an exhausted deadline should do (same guard
        // `read_reply_line` applies per-read below). A deadline that expired
        // during connect folds into `NoReply`, matching how the read loop
        // treats a deadline that expires mid-operation.
        let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return (
                SocketReply::NoReply,
                Some(NoReplyCause::Read(ReplyReadError::DeadlineExpired)),
            );
        };
        if stream.set_timeouts(remaining).is_err() {
            return (SocketReply::Unreachable, None);
        }
    }
    let msg = format!("{json}\n");
    match write_request_line(&mut stream, msg.as_bytes()) {
        RequestWrite::Written => {}
        RequestWrite::NothingWritten(err) => {
            tracing::debug!(
                reason = %err,
                "hook socket request failed before a single byte left this process"
            );
            return (SocketReply::Unreachable, None);
        }
        RequestWrite::PartiallyWritten { written, err } => {
            // Issue #434: bytes are already gone, so "never sent" is not a
            // claim this process can make any more. `warn!` where the read
            // side below settles for `debug!`, because the consequence is
            // heavier: a caller handed `NoReply` cannot resend without
            // risking a duplicate. It is also the only record production
            // keeps — `request_from_socket_at` drops the cause below, and
            // every production caller reaches the socket through it.
            let cause = NoReplyCause::PartialWrite {
                written,
                total: msg.len(),
            };
            tracing::warn!(
                cause = %cause,
                reason = %err,
                "hook socket request broke mid-write — part of the line may already be in \
                 the daemon's buffer, so the outcome is unconfirmed rather than undelivered"
            );
            return (SocketReply::NoReply, Some(cause));
        }
    }
    // Half-close our write side so the daemon's line reader sees EOF after our
    // single request and doesn't block waiting for more (it reads in a loop).
    // Best-effort: on a transport without a half-close primitive (Windows named
    // pipes) this is a no-op, which is why the read below must not depend on EOF.
    //
    // PRD #1542: except for a held request, whose connection the daemon reads
    // for EOF to learn that the producer stopped waiting.
    if half_close {
        let _ = stream.shutdown_write();
    }
    // Read exactly the ONE reply line the daemon writes, rather than to EOF.
    //
    // PRD #163 M4: reading to EOF made this a deadlock on Windows. The daemon
    // answers `get-seed` and then keeps its side open reading further lines, so it
    // only closes once *we* do — and a named pipe has no half-close with which to
    // tell it we are done while still wanting its reply. Stopping at the newline
    // is EOF-independent and returns the identical value on Unix (the daemon
    // writes exactly one JSON line). An absent/older daemon that never answers
    // still terminates: it either closes without writing a byte (EOF) or hits
    // the total-operation deadline below (when `timeout` is set), and both
    // fold into `SocketReply::NoReply` here / the caller's documented "no
    // seed" → PTY-injection fallback for `get-seed`.
    match read_reply_line(&mut stream, deadline) {
        Ok(line) => (SocketReply::Line(line), None),
        Err(err) => {
            // Every reason still folds into the one `NoReply` the callers
            // already handle — the wire contract is unchanged. It is only
            // recorded on the way past, because issue #564's two occurrences
            // both had to be diagnosed from a nextest *duration* (0.4s of a 5s
            // budget) after the fact: the reply path had no way to say which of
            // its four terminal branches fired.
            let cause = NoReplyCause::Read(err);
            tracing::debug!(reason = %cause, "hook socket request read no reply line");
            (SocketReply::NoReply, Some(cause))
        }
    }
}

/// How far [`write_request_line`] got before it stopped — the evidence
/// [`SocketReply::Unreachable`]'s "provably nothing left this process" claim
/// rests on, and which `write_all`'s own `Result` does not carry.
#[derive(Debug)]
enum RequestWrite {
    /// Every byte of the request line was accepted by the transport and the
    /// flush that followed succeeded. Says nothing about whether the daemon
    /// read them — only that this process is done handing them over.
    Written,
    /// The write or the flush failed with at least one byte already gone from
    /// this process. `written` is how many bytes the transport had accepted.
    PartiallyWritten { written: usize, err: std::io::Error },
    /// The failure came with the byte count still at zero.
    NothingWritten(std::io::Error),
}

impl RequestWrite {
    /// Classify a write/flush failure by the one thing that decides whether a
    /// resend is safe: whether any byte had already left the process.
    fn from_failure(written: usize, err: std::io::Error) -> Self {
        if written == 0 {
            Self::NothingWritten(err)
        } else {
            Self::PartiallyWritten { written, err }
        }
    }
}

/// Write one request line and report how far it got, in place of the
/// `write_all` + `flush` pair this replaced — whose `Result` cannot tell a
/// write that failed before the first byte from one that failed after some of
/// them, which is exactly the distinction [`SocketReply::Unreachable`]
/// promises its callers (issue #434).
///
/// Generic over [`std::io::Write`] rather than taking
/// [`crate::platform::ipc::IpcClient`] so the classification can be unit
/// tested against a stub writer that fails at a chosen byte offset. A real
/// socket can be driven into a partial write (`socket/013` does), but the
/// kernel picks where it stops, so a test cannot pin the count that way.
///
/// Matches `write_all`'s two documented behaviours, so swapping it in changes
/// what is reported and not what is written: an `ErrorKind::Interrupted` is
/// retried rather than counted as a failed write (the write-side twin of
/// [`is_transient_read_error`]'s `EINTR` arm, issue #564), and a `write` that
/// returns `Ok(0)` for a non-empty buffer becomes an `ErrorKind::WriteZero`
/// failure rather than an endless loop.
fn write_request_line<W: std::io::Write>(stream: &mut W, msg: &[u8]) -> RequestWrite {
    let mut written = 0usize;
    while written < msg.len() {
        match stream.write(&msg[written..]) {
            Ok(0) => {
                return RequestWrite::from_failure(
                    written,
                    std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "failed to write whole request line",
                    ),
                );
            }
            Ok(n) => written += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return RequestWrite::from_failure(written, err),
        }
    }
    match stream.flush() {
        Ok(()) => RequestWrite::Written,
        // Routed through the same classifier rather than assumed partial. For
        // a non-empty line every byte is already gone by the time the flush
        // runs, but it is the byte count that says so, and going through the
        // classifier keeps that true if this is ever handed an empty buffer.
        Err(err) => RequestWrite::from_failure(written, err),
    }
}

/// Why a request ended in [`SocketReply::NoReply`] — diagnostic only, and
/// never load-bearing: every cause folds into the one variant the callers
/// match on.
#[derive(Debug)]
enum NoReplyCause {
    /// The request write broke after bytes had already left this process:
    /// `written` of `total` had been accepted by the transport (issue #434).
    PartialWrite { written: usize, total: usize },
    /// The request went out in full and no reply line came back.
    Read(ReplyReadError),
}

impl std::fmt::Display for NoReplyCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PartialWrite { written, total } => write!(
                f,
                "request write broke after {written} of {total} bytes had left this process"
            ),
            // Verbatim, so the read path's log line reads exactly as it did
            // before this wrapper existed.
            Self::Read(err) => write!(f, "{err}"),
        }
    }
}

/// Why [`read_reply_line`] returned no reply line.
///
/// Every variant folds into [`SocketReply::NoReply`] at the boundary, so this
/// changes no caller's behavior; it exists so a failure names itself instead
/// of collapsing four distinct causes into one `None`. Issue #564: a
/// `get-seed` that silently degrades to PTY injection looks identical to one
/// that never had a daemon to talk to, which is exactly the ambiguity that
/// made a macOS-only flake take two occurrences and a log excavation to place.
#[derive(Debug)]
enum ReplyReadError {
    /// The total-operation budget was gone before a reply line completed —
    /// a genuinely wedged or unreachably slow daemon.
    DeadlineExpired,
    /// The peer closed without writing a single byte: an older daemon that
    /// does not know this verb. See [`SocketReply::NoReply`].
    ClosedWithoutReply,
    /// A read failed for a reason that is not transient — transient ones
    /// are retried, see [`is_transient_read_error`]. A failed per-read
    /// timeout *re-arm* used to land here too and no longer does (issue
    /// #642): it is logged and the read is attempted anyway, for the reasons
    /// at that call site.
    Io(std::io::Error),
    /// A reply line arrived but was not valid UTF-8.
    InvalidUtf8,
}

impl std::fmt::Display for ReplyReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeadlineExpired => write!(f, "total-operation deadline expired"),
            Self::ClosedWithoutReply => write!(f, "peer closed without writing any bytes"),
            Self::Io(err) => write!(f, "read failed: {err} (kind {:?})", err.kind()),
            Self::InvalidUtf8 => write!(f, "reply line was not valid UTF-8"),
        }
    }
}

/// Read one reply line off `stream`, bounded by a **total-operation**
/// deadline rather than the per-read idle timeout the caller already puts on
/// the socket via `IpcClient::set_timeouts`.
///
/// The previous implementation drove a `BufRead::read_line` directly against
/// `stream`, which keeps issuing reads until it sees a newline, EOF, or an
/// error — and every successful read resets the socket's idle timer, so a
/// peer that dribbles one byte per interval, always comfortably under the
/// per-read bound, could keep the read alive indefinitely even though the
/// line never completed. This loop instead tracks wall-clock elapsed time
/// against `deadline` and re-arms the socket's per-read timeout to whatever
/// is left before each individual read, so the *operation as a whole* —
/// regardless of how the peer paces its bytes — cannot run past `deadline`.
/// Once the deadline has passed, a read fails non-transiently, the peer
/// closes before sending a single byte, or the line is not valid UTF-8, this
/// returns the matching [`ReplyReadError`]; the caller folds every one of
/// them into `SocketReply::NoReply` exactly as it already did for a timed-out
/// read — see [`request_from_socket`]. The typed reason exists only so a
/// failure can say which of the four it was (issue #564). A peer that closes
/// *after* writing part of a line, but before the newline, is a distinct
/// case: the partial line is still returned as `Ok(partial)`, not folded into
/// an error.
///
/// "A read fails" is deliberately narrower than "`read` returned `Err`":
/// `EINTR`, and a per-read timeout that fires while the operation's own
/// budget still has time on it, are retried rather than treated as the end of
/// the exchange — see [`is_transient_read_error`]. Reading `Err` as final
/// regardless is what let a daemon replying 300ms into a 5s budget be
/// reported as an absent one on macOS CI.
///
/// The same narrowing applies to the re-arm itself (issue #642): a
/// `set_timeouts` that fails is logged and the read attempted anyway, because
/// on macOS the ordinary "daemon answered, then closed" ending makes every
/// later `setsockopt` on this socket fail with `EINVAL` while the reply is
/// already buffered and waiting. See the call site.
///
/// `deadline` is computed once by the caller — [`request_from_socket_at`] —
/// from a point **before** it connects, not re-derived here from a fresh
/// `Instant::now()`: doing the latter would let connect and the request write
/// each consume wall-clock time against nothing, so only the read phase would
/// actually be bounded and the exchange as a whole could run past what the
/// caller's `timeout` advertises.
///
/// A `deadline` of `None` falls back to unbounded blocking reads with no
/// re-arming, for callers that pass no deadline at all.
fn read_reply_line(
    stream: &mut crate::platform::ipc::IpcClient,
    deadline: Option<std::time::Instant>,
) -> Result<String, ReplyReadError> {
    let mut buf = [0u8; 512];
    let mut line = Vec::new();
    loop {
        if let Some(deadline) = deadline {
            // `checked_duration_since` yields `Some(ZERO)` at the exact instant
            // the deadline lands, and `set_read_timeout(ZERO)` is an
            // `InvalidInput` error rather than a timeout — so an exhausted
            // budget is reported as what it is instead of as an I/O failure.
            let remaining = match deadline.checked_duration_since(std::time::Instant::now()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => return Err(ReplyReadError::DeadlineExpired),
            };
            // Issue #642: a re-arm that FAILS is not on its own evidence that
            // the exchange is over either — and on macOS it is routinely not.
            //
            // XNU's `sosetoptlock` refuses EVERY `setsockopt` with `EINVAL`
            // once a socket carries both `SS_CANTSENDMORE` and
            // `SS_CANTRCVMORE`: "the socket has been shutdown, no more
            // sockopt's". This path sets the first of those itself — the
            // caller half-closes its write side before reading — and the peer
            // sets the second the instant it closes. The daemon's hook loop
            // does exactly that: it writes the `get-seed` reply, reads EOF
            // from our half-close on its very next pass, and drops the
            // connection. So on macOS a second trip round this loop after the
            // daemon has answered finds the re-arm failing with `EINVAL`
            // (`InvalidInput`) while the reply itself is already sitting,
            // complete, in our receive buffer — and treating that as fatal
            // threw the reply away and reported the daemon as absent. Linux's
            // `sock_setsockopt` has no such rule, which is why this only ever
            // showed up on macOS.
            //
            // Reading anyway is safe rather than merely hopeful, on two
            // counts. A socket in the state that produces this error cannot
            // block in `read(2)` at all: `SS_CANTRCVMORE` means the next read
            // returns the buffered bytes or EOF immediately. And more
            // generally, the re-arm only ever TIGHTENS a bound that is
            // already in place — `request_from_socket_at_detailed` arms the
            // socket before it writes — so a failed one leaves the previous,
            // never-larger-than-the-budget timeout standing rather than
            // leaving the read unbounded, and the loop head above still ends
            // the operation with `DeadlineExpired` the moment the budget is
            // gone.
            if let Err(err) = stream.set_timeouts(remaining) {
                tracing::debug!(
                    reason = %err,
                    "hook socket reply read could not re-arm its per-read timeout; reading anyway"
                );
            }
        }
        let n = match stream.read(&mut buf) {
            Ok(n) => n,
            // Issue #564: a read error is NOT on its own evidence that the
            // exchange is over. Two classes have to be reconciled with the
            // deadline before they can end it, and folding them straight into
            // "no reply" is what let a daemon replying 300ms into a 5s budget
            // be reported as an absent one.
            //
            // `Interrupted` (EINTR) is transient by definition — a signal
            // landed on this thread while it sat in `read(2)`. It says nothing
            // about the peer and nothing about the clock, and `std` does not
            // retry it for us here the way `Write::write_all` does on the send
            // side. Retrying is always correct: the loop re-arms from
            // `deadline` on the next pass, so the operation stays bounded.
            //
            // `WouldBlock`/`TimedOut` (EAGAIN/EWOULDBLOCK/ETIMEDOUT) are the
            // socket's OWN per-read `SO_RCVTIMEO` firing, which is a different
            // clock from the total-operation deadline this function
            // advertises. Normally the two coincide, because the re-arm above
            // sets the per-read timeout to exactly the remaining budget — but
            // treating the per-read result as final regardless meant the
            // operation ended on the socket's word rather than on the
            // deadline's, so any early or spurious fire cut an exchange short
            // that still had seconds left. Consult the deadline instead: while
            // budget remains, go back and read again; once it is gone, the
            // loop head above returns `DeadlineExpired`.
            //
            // This cannot spin. A `WouldBlock` from a `SO_RCVTIMEO` armed at
            // `remaining` has by definition just consumed `remaining`, so the
            // loop head finds no budget and ends the operation on the very
            // next pass — the ordinary case costs exactly one extra trip round
            // the loop. The retry earns its keep only when a fire is EARLY,
            // which is the case that has no other defence, and even a fire
            // that were somehow instant is bounded by the same deadline rather
            // than by a retry count. `socket_003`/`socket_005` pin that end of
            // it: both still return at ~5.01s, unchanged by this.
            Err(err) if is_transient_read_error(&err, deadline) => continue,
            Err(err) => return Err(ReplyReadError::Io(err)),
        };
        if n == 0 {
            // EOF with nothing received at all: the daemon closed without
            // answering — exactly the "old daemon that doesn't know this
            // request type" case `SocketReply::NoReply`'s own doc comment
            // already names, so this must fold into `None`/`NoReply`, not
            // `Some(String::new())`/`Line("")` (an empty `Line` is meant to
            // mean the daemon explicitly sent a blank reply line, which this
            // is not). A *partial*, unterminated line — the daemon wrote some
            // bytes then closed before the newline — is left as
            // `Line(partial)` unchanged: that is a distinct scenario this fix
            // does not touch.
            if line.is_empty() {
                return Err(ReplyReadError::ClosedWithoutReply);
            }
            break;
        }
        if let Some(newline_pos) = buf[..n].iter().position(|&b| b == b'\n') {
            line.extend_from_slice(&buf[..newline_pos]);
            break;
        }
        line.extend_from_slice(&buf[..n]);
    }
    match String::from_utf8(line) {
        Ok(line) => Ok(line.trim_end_matches('\r').to_string()),
        Err(_) => Err(ReplyReadError::InvalidUtf8),
    }
}

/// Should this `read(2)` failure send [`read_reply_line`] back for another
/// pass rather than ending the exchange? See the call site for why each class
/// is here; `deadline` is what decides the timeout-class ones, so a caller
/// that passed no deadline at all (unbounded blocking reads) keeps treating
/// them as final rather than spinning on them forever.
fn is_transient_read_error(err: &std::io::Error, deadline: Option<std::time::Instant>) -> bool {
    match err.kind() {
        std::io::ErrorKind::Interrupted => true,
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
            deadline.is_some_and(|deadline| deadline > std::time::Instant::now())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    /// Issue #1182: what the pane's card, the desktop overview and the prompt
    /// history show for a seeded Claude agent. Before this they showed
    /// `<pasted_content id="239f">You a…` — the producer's wrapper, spending a
    /// quarter of the recorded budget and hiding the prompt's opening words,
    /// which is how `prompt/new-pane/016` found it.
    #[test]
    fn a_pasted_turn_is_recorded_as_the_prompt_it_wrapped() {
        let prompt = "You are an ordinary assistant.\nDo the thing.";
        assert_eq!(
            record_submitted_prompt(&format!(
                "\n\n<pasted_content id=\"239f\">\n{prompt}\n</pasted_content id=\"239f\">"
            )),
            prompt
        );
    }

    /// A turn someone composed AROUND a paste is theirs, not ours, and is
    /// recorded as they wrote it. Same bound the delivery matcher draws.
    #[test]
    fn a_turn_that_merely_contains_a_paste_is_recorded_unchanged() {
        let turn =
            "please run this: <pasted_content id=\"239f\">\nstuff\n</pasted_content id=\"239f\">";
        assert_eq!(record_submitted_prompt(turn), turn);
    }

    /// Unenveloped prompts are recorded exactly as before, truncation included.
    #[test]
    fn an_ordinary_prompt_is_recorded_exactly_as_before() {
        assert_eq!(record_submitted_prompt("ls -la"), "ls -la");
        let long = "x".repeat(crate::prompt_delivery::USER_PROMPT_MAX_LEN + 40);
        assert_eq!(
            record_submitted_prompt(&long),
            truncate(&long, crate::prompt_delivery::USER_PROMPT_MAX_LEN)
        );
    }

    /// The whole recorded budget goes on the prompt rather than partly on the
    /// wrapper, so an enveloped long prompt keeps MORE of itself than it would
    /// have if the envelope had been truncated along with it.
    #[test]
    fn unwrapping_spends_the_budget_on_the_prompt() {
        let long = "y".repeat(crate::prompt_delivery::USER_PROMPT_MAX_LEN * 2);
        let enveloped = format!("<pasted_content id=\"239f\">\n{long}");
        assert_eq!(
            record_submitted_prompt(&enveloped),
            truncate(&long, crate::prompt_delivery::USER_PROMPT_MAX_LEN)
        );
    }

    #[test]
    fn map_session_start() {
        assert_eq!(
            map_event_type("SessionStart"),
            Some(EventType::SessionStart)
        );
    }

    #[test]
    fn map_pre_tool_use() {
        assert_eq!(map_event_type("PreToolUse"), Some(EventType::ToolStart));
    }

    #[test]
    fn map_post_tool_use() {
        assert_eq!(map_event_type("PostToolUse"), Some(EventType::ToolEnd));
    }

    #[test]
    fn map_notification() {
        assert_eq!(
            map_event_type("Notification"),
            Some(EventType::WaitingForInput)
        );
    }

    #[test]
    fn map_permission_request() {
        assert_eq!(
            map_event_type("PermissionRequest"),
            Some(EventType::PermissionRequest)
        );
    }

    #[test]
    fn map_stop() {
        assert_eq!(map_event_type("Stop"), Some(EventType::Idle));
    }

    #[test]
    fn map_session_end() {
        assert_eq!(map_event_type("SessionEnd"), Some(EventType::SessionEnd));
    }

    #[test]
    fn map_unknown_returns_none() {
        assert_eq!(map_event_type("SomethingElse"), None);
    }

    #[test]
    fn tool_detail_bash_command() {
        let input: Value = serde_json::json!({"command": "ls -la\necho hello"});
        let detail = extract_tool_detail(Some("Bash"), Some(&input));
        assert_eq!(detail.as_deref(), Some("ls -la"));
    }

    #[test]
    fn tool_detail_bash_truncates_long_command() {
        let long_cmd = "x".repeat(200);
        let input: Value = serde_json::json!({"command": long_cmd});
        let detail = extract_tool_detail(Some("Bash"), Some(&input)).unwrap();
        assert!(detail.len() <= 124); // 120 + "…" (3 bytes)
    }

    #[test]
    fn tool_detail_read_file_path() {
        let input: Value = serde_json::json!({"file_path": "/src/main.rs"});
        let detail = extract_tool_detail(Some("Read"), Some(&input));
        assert_eq!(detail.as_deref(), Some("/src/main.rs"));
    }

    #[test]
    fn tool_detail_edit_file_path() {
        let input: Value =
            serde_json::json!({"file_path": "/src/lib.rs", "old_string": "a", "new_string": "b"});
        let detail = extract_tool_detail(Some("Edit"), Some(&input));
        assert_eq!(detail.as_deref(), Some("/src/lib.rs"));
    }

    #[test]
    fn tool_detail_grep_pattern() {
        let input: Value = serde_json::json!({"pattern": "fn main"});
        let detail = extract_tool_detail(Some("Grep"), Some(&input));
        assert_eq!(detail.as_deref(), Some("fn main"));
    }

    #[test]
    fn tool_detail_glob_pattern() {
        let input: Value = serde_json::json!({"pattern": "**/*.rs"});
        let detail = extract_tool_detail(Some("Glob"), Some(&input));
        assert_eq!(detail.as_deref(), Some("**/*.rs"));
    }

    #[test]
    fn tool_detail_agent_description() {
        let input: Value = serde_json::json!({"description": "explore codebase"});
        let detail = extract_tool_detail(Some("Agent"), Some(&input));
        assert_eq!(detail.as_deref(), Some("explore codebase"));
    }

    #[test]
    fn tool_detail_unknown_tool_uses_first_string() {
        let input: Value = serde_json::json!({"query": "SELECT 1", "timeout": 30});
        let detail = extract_tool_detail(Some("SQL"), Some(&input));
        assert_eq!(detail.as_deref(), Some("SELECT 1"));
    }

    #[test]
    fn tool_detail_none_when_no_input() {
        let detail = extract_tool_detail(Some("Bash"), None);
        assert!(detail.is_none());
    }

    #[test]
    fn tool_detail_none_when_no_tool_name() {
        let input: Value = serde_json::json!({"command": "ls"});
        let detail = extract_tool_detail(None, Some(&input));
        assert!(detail.is_none());
    }

    #[test]
    fn build_event_session_start() {
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "SessionStart".into(),
            cwd: Some("/tmp".into()),
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: None,
            source: None,
            subagent_id: None,
            _extra: HashMap::new(),
            ..Default::default()
        };
        let event = build_event(input).unwrap();
        assert_eq!(event.session_id, "test-123");
        assert_eq!(event.event_type, EventType::SessionStart);
        assert_eq!(event.cwd.as_deref(), Some("/tmp"));
        assert!(event.tool_name.is_none());
        assert!(event.user_prompt.is_none());
    }

    #[test]
    fn build_event_tool_start_with_detail() {
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "PreToolUse".into(),
            cwd: None,
            tool_name: Some("Read".into()),
            tool_input: Some(serde_json::json!({"file_path": "/src/main.rs"})),
            tool_use_id: None,
            prompt: None,
            source: None,
            subagent_id: None,
            _extra: HashMap::new(),
            ..Default::default()
        };
        let event = build_event(input).unwrap();
        assert_eq!(event.event_type, EventType::ToolStart);
        assert_eq!(event.tool_name.as_deref(), Some("Read"));
        assert_eq!(event.tool_detail.as_deref(), Some("/src/main.rs"));
    }

    #[test]
    fn build_event_unknown_hook_returns_none() {
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "UnknownHook".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: None,
            source: None,
            subagent_id: None,
            _extra: HashMap::new(),
            ..Default::default()
        };
        assert!(build_event(input).is_none());
    }

    #[test]
    fn build_event_user_prompt_submit_extracts_prompt() {
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "UserPromptSubmit".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: Some("fix the login bug".into()),
            source: None,
            subagent_id: None,
            _extra: HashMap::new(),
            ..Default::default()
        };
        let event = build_event(input).unwrap();
        assert_eq!(event.event_type, EventType::Thinking);
        assert_eq!(event.user_prompt.as_deref(), Some("fix the login bug"));
    }

    #[test]
    fn build_event_prompt_truncated_to_200() {
        let long_prompt = "x".repeat(300);
        let input = ClaudeCodeHookInput {
            session_id: "test-123".into(),
            hook_event_name: "UserPromptSubmit".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: Some(long_prompt),
            source: None,
            subagent_id: None,
            _extra: HashMap::new(),
            ..Default::default()
        };
        let event = build_event(input).unwrap();
        let prompt = event.user_prompt.unwrap();
        assert!(prompt.len() <= 204); // 200 + "…" (3 bytes)
        assert!(prompt.ends_with('…'));
    }

    /// `source` degrades the same way an unexpected-shape field elsewhere in
    /// this struct already does: a strict `Option<String>` would fail the
    /// WHOLE decode on a non-string `source` (object, number, bool, array),
    /// and `handle_hook` swallows that error silently (`Err(_) => return
    /// ExitCode::SUCCESS`) for all four producer arms. `lenient_string` must
    /// degrade a non-string `source` to `None` instead of dropping the event.
    #[test]
    fn source_001_non_string_source_does_not_drop_the_event() {
        for (label, source_json) in [
            ("object", r#"{"kind":"clear"}"#),
            ("number", "3"),
            ("bool", "true"),
            ("array", r#"["clear"]"#),
        ] {
            let payload = format!(
                r#"{{"session_id":"test-123","hook_event_name":"SessionStart","source":{source_json}}}"#
            );
            let hook_input: ClaudeCodeHookInput =
                serde_json::from_str(&payload).unwrap_or_else(|e| {
                    panic!("a non-string ({label}) source must not fail the whole decode: {e}")
                });
            assert!(
                hook_input.source.is_none(),
                "a non-string ({label}) source must degrade to None, not a decode error"
            );
            let event = build_event(hook_input)
                .expect("the rest of the event must survive a non-string source");
            assert_eq!(event.session_id, "test-123");
            assert_eq!(event.event_type, EventType::SessionStart);
        }

        // `null` already works and must keep working.
        let payload = r#"{"session_id":"test-123","hook_event_name":"SessionStart","source":null}"#;
        let hook_input: ClaudeCodeHookInput =
            serde_json::from_str(payload).expect("a null source must decode fine");
        assert!(hook_input.source.is_none());
    }

    #[test]
    fn send_to_missing_socket_returns_none() {
        // With no daemon running, send should silently fail
        let result = send_to_socket_at(
            std::path::Path::new("/tmp/nonexistent-test-socket.sock"),
            r#"{"test": true}"#,
        );

        assert!(result.is_none());
    }

    /// Scenario: A stub daemon accepts one connection, reads the request
    /// line, then deliberately holds the connection open forever without
    /// replying and without closing — simulating a wedged daemon.
    /// `request_from_socket` used to rely entirely on the daemon closing the
    /// connection, with no read/write bound of its own, so against this
    /// daemon it hung forever; it now returns `None` within the 5s bound the
    /// fix added. Run it on a worker thread and bound the wait with
    /// `recv_timeout` well above that 5s, so an unbounded
    /// `request_from_socket` would fail fast with a clear panic instead of
    /// hanging the CI runner until nextest's own timeout.
    #[spec("error/socket/003")]
    #[test]
    #[cfg(unix)]
    fn socket_003_unbounded_daemon_does_not_hang_forever() {
        let _tmp = tempfile::tempdir().expect("create temp dir for stub daemon socket");
        let socket_path = _tmp.path().join("s.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind stub daemon socket");

        // Stub daemon: read the one request line, then go silent forever —
        // never replies, never closes.
        let _daemon_thread = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = std::io::BufReader::new(&stream);
                let mut line = String::new();
                let _ = std::io::BufRead::read_line(&mut reader, &mut line);
                std::thread::sleep(std::time::Duration::from_secs(60));
                drop(stream);
            }
        });

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = match request_from_socket_at(
                &socket_path,
                r#"{"type":"get-seed"}"#,
                Some(GET_SEED_REQUEST_TIMEOUT),
            ) {
                SocketReply::Line(line) => Some(line),
                SocketReply::NoReply | SocketReply::Unreachable => None,
            };
            let _ = tx.send(result);
        });

        let outcome = rx.recv_timeout(std::time::Duration::from_secs(15));

        match outcome {
            Ok(value) => assert_eq!(
                value, None,
                "request_from_socket must fold a timed-out/unbounded daemon into None \
                 (\"no seed\"), identical to a daemon that replies with nothing — got {value:?}"
            ),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!(
                "request_from_socket did not return within 15s against a daemon that reads \
                 the request and then never replies and never closes — it has no read/write \
                 bound of its own"
            ),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!(
                "the worker thread running request_from_socket dropped its channel without \
                 sending a result"
            ),
        }
    }

    /// Scenario: A stub daemon accepts the connection, reads the request
    /// line, waits a short delay well inside the timeout bound, then writes
    /// one JSON reply line. `request_from_socket` must still return that
    /// line as `Some(...)`. This guards against the specific way the
    /// idle-timeout fix could have made things worse — a bound that fires
    /// too eagerly would mistake a merely-slow daemon for an absent one and
    /// silently fall back to PTY injection — so it is a correctness control
    /// rather than a timing measurement, and it passed both before and after
    /// that fix landed.
    #[spec("error/socket/004")]
    #[test]
    #[cfg(unix)]
    fn socket_004_slow_but_replying_daemon_still_returns_the_reply() {
        const REPLY_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

        let _tmp = tempfile::tempdir().expect("create temp dir for stub daemon socket");
        let socket_path = _tmp.path().join("s.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind stub daemon socket");

        // Stub daemon: read the request line, wait a short delay comfortably
        // inside the 5s bound, then reply with one line. It reports what it
        // actually managed to do rather than swallowing every error into `_`,
        // so a failure can separate "the client dropped a reply that WAS
        // written" from "the daemon never wrote one" — a distinction issue
        // #564 had no way to make from either of its CI occurrences.
        let daemon_thread = std::thread::spawn(move || match listener.accept() {
            Err(err) => format!("accept failed: {err}"),
            Ok((mut stream, _)) => {
                let mut reader = std::io::BufReader::new(&stream);
                let mut line = String::new();
                if let Err(err) = std::io::BufRead::read_line(&mut reader, &mut line) {
                    return format!("reading the request line failed: {err}");
                }
                std::thread::sleep(REPLY_DELAY);
                // Two writes, not one: this also keeps the client's read loop
                // going round a second time for the newline, which is the
                // path that has to survive a transient read error.
                let written = std::io::Write::write_all(&mut stream, br#"{"seed":"abc123"}"#)
                    .and_then(|()| std::io::Write::write_all(&mut stream, b"\n"))
                    .and_then(|()| std::io::Write::flush(&mut stream));
                match written {
                    Ok(()) => "wrote and flushed the reply line".to_string(),
                    Err(err) => format!("writing the reply failed: {err}"),
                }
            }
        });

        let started = std::time::Instant::now();
        let (reply, read_error) = request_from_socket_at_detailed(
            &socket_path,
            r#"{"type":"get-seed"}"#,
            Some(GET_SEED_REQUEST_TIMEOUT),
        );
        let elapsed = started.elapsed();
        let daemon_report = daemon_thread
            .join()
            .unwrap_or_else(|_| "the stub daemon thread panicked".to_string());

        // Deliberately NOT collapsed into `Option<String>` before asserting.
        // The old assertion printed a bare `left: None` — a value `NoReply`
        // and `Unreachable` produce alike, naming neither the cause nor how
        // much of the budget had been spent. Both of issue #564's macOS
        // occurrences had to be placed by reading the *duration* out of a
        // nextest line after the fact (0.435s and 0.401s of a 5s budget,
        // against a 0.307s honest cost), which is what showed the deadline
        // was never in play and the exchange had been ended by a read error
        // instead. That should come out of the assertion itself next time.
        assert!(
            matches!(&reply, SocketReply::Line(line) if line == r#"{"seed":"abc123"}"#),
            "a daemon that replies well inside the timeout bound must not be mistaken for \
             an absent one — got {reply:?} (read error: {read_error:?}) after {elapsed:?} of \
             a {GET_SEED_REQUEST_TIMEOUT:?} budget, having slept {REPLY_DELAY:?} before \
             replying; the stub daemon reports: {daemon_report}. An elapsed time well short \
             of the budget means something other than the deadline ended the exchange — see \
             `is_transient_read_error`."
        );
    }

    /// Scenario: A stub daemon accepts the connection, reads the request
    /// line, then dribbles a single non-newline byte at a fixed interval
    /// forever — never sending the newline `read_line` is waiting for. The
    /// per-read idle timeout added for `error/socket/003` re-armed on every
    /// byte received, so each dribbled byte used to restart it before it
    /// could fire and `request_from_socket` never returned, even though the
    /// peer never went silent for as long as one read — the total-operation
    /// deadline the fix added fires anyway, so the call now comes back. Run
    /// it on a worker thread and bound the wait with `recv_timeout` at a
    /// ceiling generous enough to hold that deadline, so an unbounded
    /// `request_from_socket` would fail with a clear panic instead of
    /// hanging the CI runner.
    #[spec("error/socket/005")]
    #[test]
    #[cfg(unix)]
    fn socket_005_slow_drip_daemon_does_not_hang_forever() {
        let _tmp = tempfile::tempdir().expect("create temp dir for stub daemon socket");
        let socket_path = _tmp.path().join("s.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind stub daemon socket");

        // 200ms is ~25x under the 5s per-read SO_RCVTIMEO bound, so every
        // dribbled byte comfortably resets the timer well before it
        // could fire even under CI scheduler jitter — this deterministically
        // exercises the reset-on-every-byte behaviour that opened the gap,
        // rather than racing it.
        // The daemon dribbles for DRIP_TOTAL (20s), safely longer than
        // ASSERT_CEILING (15s) below, so the drip is still ongoing for the
        // *entire* assertion wait — the failure can only come from the
        // channel timing out, never from the peer going quiet on its own.
        const DRIP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
        const DRIP_TOTAL: std::time::Duration = std::time::Duration::from_secs(20);

        let _daemon_thread = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut reader = std::io::BufReader::new(&stream);
                let mut line = String::new();
                let _ = std::io::BufRead::read_line(&mut reader, &mut line);

                let deadline = std::time::Instant::now() + DRIP_TOTAL;
                while std::time::Instant::now() < deadline {
                    std::thread::sleep(DRIP_INTERVAL);
                    // A single non-newline byte: never completes the line
                    // `read_line` is waiting for, but is enough on its own
                    // to reset SO_RCVTIMEO on the reader side.
                    if std::io::Write::write_all(&mut stream, b".").is_err() {
                        break;
                    }
                    let _ = std::io::Write::flush(&mut stream);
                }
                drop(stream);
            }
        });

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = match request_from_socket_at(
                &socket_path,
                r#"{"type":"get-seed"}"#,
                Some(GET_SEED_REQUEST_TIMEOUT),
            ) {
                SocketReply::Line(line) => Some(line),
                SocketReply::NoReply | SocketReply::Unreachable => None,
            };
            let _ = tx.send(result);
        });

        // Deliberately generous relative to the 5s per-read timeout so
        // this does not pin the exact operation-level deadline the fix
        // settled on — it only needs to hold any sane deadline, while still
        // failing well before it would hang CI's own per-test timeout.
        const ASSERT_CEILING: std::time::Duration = std::time::Duration::from_secs(15);
        let outcome = rx.recv_timeout(ASSERT_CEILING);

        match outcome {
            // Any return within the ceiling proves the operation is bounded
            // in total time — this test pins that property, not a specific
            // reply shape (the peer never sends a valid reply line at all).
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!(
                "request_from_socket did not return within {ASSERT_CEILING:?} against a peer \
                 that dribbles one non-newline byte every {DRIP_INTERVAL:?} — each byte resets \
                 the per-read idle timeout before it can fire, so read_line() never sees a \
                 newline, EOF, or an error and blocks indefinitely"
            ),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!(
                "the worker thread running request_from_socket dropped its channel without \
                 sending a result"
            ),
        }
    }

    /// Scenario: A stub daemon accepts the connection, reads the request
    /// line, then closes immediately without writing a single byte back —
    /// simulating an old daemon that doesn't understand the request type at
    /// all. `read_reply_line` used to fold this into `Some(String::new())` —
    /// `SocketReply::Line("")` — even though `SocketReply::NoReply`'s own doc
    /// comment already names exactly this scenario ("the daemon closed
    /// without answering") as a `NoReply` case. Asserts the fixed behavior:
    /// `request_from_socket_at` returns `SocketReply::NoReply`, not
    /// `SocketReply::Line(String::new())`.
    #[spec("error/socket/006")]
    #[test]
    #[cfg(unix)]
    fn socket_006_silent_close_returns_no_reply_not_empty_line() {
        let _tmp = tempfile::tempdir().expect("create temp dir for stub daemon socket");
        let socket_path = _tmp.path().join("s.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind stub daemon socket");

        // Stub daemon: read the one request line, then close without
        // writing anything back at all.
        let daemon_thread = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = std::io::BufReader::new(&stream);
                let mut line = String::new();
                let _ = std::io::BufRead::read_line(&mut reader, &mut line);
                drop(stream);
            }
        });

        let result = request_from_socket_at(
            &socket_path,
            r#"{"type":"get-seed"}"#,
            Some(GET_SEED_REQUEST_TIMEOUT),
        );

        let _ = daemon_thread.join();

        assert!(
            matches!(result, SocketReply::NoReply),
            "a daemon that closes without writing any bytes must fold into \
             SocketReply::NoReply, matching its own doc comment's \"daemon closed without \
             answering\" case, not SocketReply::Line(\"\") — got {result:?}"
        );
    }

    /// Scenario: A stub daemon accepts the connection and reads the request
    /// line, then holds its reply back until the test says the signalling is
    /// over — while the test hammers the *client* thread with `SIGUSR1`,
    /// whose handler is installed deliberately without `SA_RESTART` so that
    /// every signal landing while the client sits in `read(2)` surfaces as
    /// `EINTR`. The handler counts its own deliveries, and the test keeps
    /// signalling until it has seen enough of them, so "a signal really did
    /// land inside the read" is something the test observes rather than
    /// something it hopes a sleep arranged. The reply must still come back
    /// intact: a signal on the reading thread says nothing about the peer and
    /// nothing about the clock, so it must not be mistaken for an absent
    /// daemon.
    #[spec("error/socket/007")]
    #[test]
    #[cfg(unix)]
    fn socket_007_signal_interrupted_read_still_returns_the_reply() {
        /// How many `SIGUSR1` deliveries the handler must have COUNTED before
        /// the daemon is released to reply. Every one of them lands in the
        /// window between "the daemon has read our request line" and "the
        /// daemon has been told it may answer", during which the client has
        /// nothing left to do but sit in `read(2)` — so this is a floor on
        /// the number of `EINTR`s the code under test had to retry through,
        /// not a guess about when a sleep lines up with a read.
        const REQUIRED_DELIVERIES: usize = 20;
        /// Pace between `pthread_kill`s. `SIGUSR1` is not queued, so a signal
        /// sent while one is already pending merges into it; spacing the
        /// sends lets each be taken before the next arrives, which is what
        /// makes the delivery count reach its floor promptly instead of
        /// asymptotically.
        const SIGNAL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2);
        /// Backstop on the signalling loop only, so a machine that cannot
        /// deliver 20 signals fails HERE, naming that, instead of letting the
        /// client's own 5s budget expire and reporting the confusing
        /// `DeadlineExpired` that would follow. Nominal cost of the loop is
        /// ~40-60ms, so this is a ~40x margin and pins no timing.
        const SIGNAL_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
        /// Generous: the operation itself is bounded at 5s, so this only has
        /// to fail before nextest's own kill window rather than pin any
        /// timing.
        const ASSERT_CEILING: std::time::Duration = std::time::Duration::from_secs(15);

        /// Counted by the handler itself. A relaxed `fetch_add` is the whole
        /// body, which keeps the handler async-signal-safe (a lock-free
        /// atomic add on every target this crate builds for).
        static DELIVERIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

        extern "C" fn counting_signal_handler(_signum: libc::c_int) {
            DELIVERIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        let _tmp = tempfile::tempdir().expect("create temp dir for stub daemon socket");
        let socket_path = _tmp.path().join("s.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind stub daemon socket");

        // Install the handler WITHOUT `SA_RESTART`. With it (or with the
        // default disposition) the kernel would restart the interrupted
        // `read(2)` itself and there would be no `EINTR` for the code under
        // test to mishandle — clearing it is the whole point. Scoped: the
        // previous disposition is restored at the end, and `SIGUSR1` is used
        // nowhere else in the crate.
        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = counting_signal_handler as *const () as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = 0;
            assert_eq!(
                libc::sigaction(libc::SIGUSR1, &action, &mut previous),
                0,
                "installing the SIGUSR1 handler must succeed"
            );
        }

        // Issue #642: the two edges of the signalling window are HANDSHAKES,
        // not sleeps. `request_read_tx` fires once the daemon has the
        // client's request line, which is proof that connect and the write
        // are already behind us — the thing the old 50ms lead-in could only
        // make likely — and `reply_now_rx` holds the reply until the last
        // signal has been sent, so the reply can never race the storm. On a
        // loaded macOS runner both of those wall-clock bets came up wrong,
        // and the test hard-reds four required checks when they do.
        let (request_read_tx, request_read_rx) = std::sync::mpsc::channel::<()>();
        let (reply_now_tx, reply_now_rx) = std::sync::mpsc::channel::<()>();
        let daemon_thread = std::thread::spawn(move || match listener.accept() {
            Err(err) => format!("accept failed: {err}"),
            Ok((mut stream, _)) => {
                let mut reader = std::io::BufReader::new(&stream);
                let mut line = String::new();
                if let Err(err) = std::io::BufRead::read_line(&mut reader, &mut line) {
                    return format!("reading the request line failed: {err}");
                }
                if request_read_tx.send(()).is_err() {
                    return "the test hung up before the request line was reported".to_string();
                }
                if reply_now_rx.recv_timeout(ASSERT_CEILING).is_err() {
                    return "never released to reply — the signalling loop did not finish"
                        .to_string();
                }
                let written = std::io::Write::write_all(&mut stream, br#"{"seed":"abc123"}"#)
                    .and_then(|()| std::io::Write::write_all(&mut stream, b"\n"))
                    .and_then(|()| std::io::Write::flush(&mut stream));
                match written {
                    Ok(()) => "wrote and flushed the reply line".to_string(),
                    Err(err) => format!("writing the reply failed: {err}"),
                }
            }
        });

        let (thread_tx, thread_rx) = std::sync::mpsc::channel::<usize>();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let client_socket_path = socket_path.clone();
        let client_thread = std::thread::spawn(move || {
            // `pthread_t` is an integer on Linux and an opaque pointer on
            // macOS; `usize` is the one shape that carries both across the
            // channel without a `cfg`.
            let _ = thread_tx.send(unsafe { libc::pthread_self() } as usize);
            let _ = result_tx.send(request_from_socket_at_detailed(
                &client_socket_path,
                r#"{"type":"get-seed"}"#,
                Some(GET_SEED_REQUEST_TIMEOUT),
            ));
            // Outlive the signalling loop, not merely the read: `pthread_kill`
            // against a thread that has already exited is undefined behaviour,
            // so this thread may not return until the last signal has been
            // sent.
            let _ = release_rx.recv();
        });

        let client_thread_id = thread_rx
            .recv_timeout(ASSERT_CEILING)
            .expect("the client thread must report its thread id");
        request_read_rx.recv_timeout(ASSERT_CEILING).expect(
            "the stub daemon must report having read the request line — until it has, the \
             client may still be in connect or write, and a signal landing there would fail \
             this test for an unrelated reason",
        );

        let signalling_started = std::time::Instant::now();
        let mut sent = 0usize;
        while DELIVERIES.load(std::sync::atomic::Ordering::Relaxed) < REQUIRED_DELIVERIES {
            assert!(
                signalling_started.elapsed() < SIGNAL_BUDGET,
                "only {} of {REQUIRED_DELIVERIES} SIGUSR1 deliveries were counted after \
                 {sent} sends in {SIGNAL_BUDGET:?} — the test never got to exercise the \
                 EINTR retry, so its result would say nothing either way",
                DELIVERIES.load(std::sync::atomic::Ordering::Relaxed)
            );
            unsafe { libc::pthread_kill(client_thread_id as libc::pthread_t, libc::SIGUSR1) };
            sent += 1;
            std::thread::sleep(SIGNAL_INTERVAL);
        }
        let delivered = DELIVERIES.load(std::sync::atomic::Ordering::Relaxed);
        // Released only now: the loop above has finished, so no further
        // `pthread_kill` can race the reply, and the client has spent the
        // whole storm parked in `read(2)` with nothing to return to it.
        let _ = reply_now_tx.send(());

        let outcome = result_rx.recv_timeout(ASSERT_CEILING);
        // Safe to release now: the signalling loop above has finished, so no
        // further `pthread_kill` can race the thread's exit.
        let _ = release_tx.send(());
        let daemon_report = daemon_thread
            .join()
            .unwrap_or_else(|_| "the stub daemon thread panicked".to_string());
        unsafe {
            libc::sigaction(libc::SIGUSR1, &previous, std::ptr::null_mut());
        }

        let (reply, read_error) = outcome.expect(
            "the client thread must return within the ceiling — the operation is bounded at \
             GET_SEED_REQUEST_TIMEOUT, so a timeout here means an EINTR retry loop that does \
             not consult the deadline",
        );
        assert!(
            matches!(&reply, SocketReply::Line(line) if line == r#"{"seed":"abc123"}"#),
            "a read interrupted by a signal must be retried, not mistaken for an absent \
             daemon — got {reply:?} (read error: {read_error:?}) after {delivered} counted \
             SIGUSR1 deliveries from {sent} sends, every one of them while the client had \
             nothing to do but sit in read(2); the stub daemon reports: {daemon_report}."
        );

        // Joined last, deliberately. If the client thread ever failed to
        // return within the ceiling the assertions above are the only thing
        // that can say so, and joining a wedged thread first would hang the
        // test until nextest killed it — losing exactly the diagnostic the
        // failure exists to produce.
        let _ = client_thread.join();
    }

    /// Scenario: A stub daemon writes its one reply line and closes at once —
    /// so by the time the client's read loop arms its per-read timeout the
    /// socket is already half-closed by us and closed by the peer. The
    /// buffered reply must still come back: on macOS every `setsockopt` on a
    /// socket in that state fails with `EINVAL`, and a per-read timeout that
    /// cannot be re-armed is no evidence that there is nothing left to read.
    #[spec("error/socket/008")]
    #[test]
    #[cfg(unix)]
    fn socket_008_reply_survives_a_peer_that_closed_before_the_read_re_armed() {
        const REPLY: &str = r#"{"seed":"abc123"}"#;

        let _tmp = tempfile::tempdir().expect("create temp dir for stub daemon socket");
        let socket_path = _tmp.path().join("s.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind stub daemon socket");

        // Both ends in hand before a byte moves: connect, then accept. Every
        // step below is ordered by this one thread, so nothing here depends
        // on a sleep or on which thread the scheduler picks — which is the
        // point, since the condition being pinned is a RACE in the flaky
        // sibling `error/socket/007` and a certainty in production.
        let mut client = crate::platform::ipc::IpcClient::connect(&socket_path)
            .expect("connect to the stub daemon socket");
        let (mut server, _) = listener.accept().expect("accept the client connection");

        // Mirror the production prelude exactly:
        // `request_from_socket_at_detailed` arms the socket before it writes,
        // and half-closes its write side before it reads. That half-close is
        // what leaves `SS_CANTSENDMORE` set here, so the peer's close below
        // completes the `SS_CANTRCVMORE | SS_CANTSENDMORE` pair that makes
        // XNU refuse every subsequent `setsockopt` with `EINVAL`.
        client
            .set_timeouts(GET_SEED_REQUEST_TIMEOUT)
            .expect("arm the socket the way the production prelude does");
        std::io::Write::write_all(&mut server, REPLY.as_bytes()).expect("write the reply");
        std::io::Write::write_all(&mut server, b"\n").expect("write the reply terminator");
        std::io::Write::flush(&mut server).expect("flush the reply");
        let _ = client.shutdown_write();
        // The daemon's hook loop closes the moment it reads EOF from our
        // half-close, which is its very next pass after answering — so "the
        // peer is already gone when the read loop starts" is the ORDINARY
        // ending of a `get-seed`, not a corner case.
        drop(server);

        let deadline = std::time::Instant::now() + GET_SEED_REQUEST_TIMEOUT;
        let outcome = read_reply_line(&mut client, Some(deadline));

        assert!(
            matches!(&outcome, Ok(line) if line == REPLY),
            "a reply already buffered by the kernel must survive a per-read timeout that \
             can no longer be re-armed — got {outcome:?}. `Io(Os {{ code: 22 }})` here is \
             macOS refusing `setsockopt` on a fully shut-down socket, which says nothing \
             about whether there is a reply waiting, and there is one."
        );
    }

    /// One scripted outcome for a single [`std::io::Write::write`] call on a
    /// [`ScriptedWriter`].
    #[derive(Debug, Clone, Copy)]
    enum WriteStep {
        /// Accept at most this many bytes of whatever is offered.
        Accept(usize),
        /// Fail with this kind, having accepted nothing on this call.
        Fail(std::io::ErrorKind),
        /// Return `Ok(0)` for a non-empty buffer — the short circuit
        /// `write_all` turns into an `ErrorKind::WriteZero` error, and which
        /// a naive loop would spin on forever.
        Zero,
    }

    /// A [`std::io::Write`] that fails at a byte offset the test picked.
    ///
    /// Issue #434's classification turns on how many bytes had left the
    /// process when the write failed, and that is the one thing a real socket
    /// cannot be asked for on demand: `error/socket/013` drives a genuine
    /// partial write through a real Unix socket, but it can only assert that
    /// SOME prefix went, never that exactly seven bytes did. The stub pins
    /// the classifier; the socket test pins the wiring.
    ///
    /// Once the script is exhausted every remaining byte is accepted, so a
    /// test only scripts the part it is about.
    struct ScriptedWriter {
        steps: std::collections::VecDeque<WriteStep>,
        /// Every byte the stub accepted, in order — the independent count a
        /// test checks [`RequestWrite`]'s own against.
        accepted: Vec<u8>,
        /// What [`std::io::Write::flush`] returns.
        flush_failure: Option<std::io::ErrorKind>,
    }

    impl ScriptedWriter {
        fn new(steps: &[WriteStep]) -> Self {
            Self {
                steps: steps.iter().copied().collect(),
                accepted: Vec::new(),
                flush_failure: None,
            }
        }

        fn failing_flush(mut self, kind: std::io::ErrorKind) -> Self {
            self.flush_failure = Some(kind);
            self
        }
    }

    impl std::io::Write for ScriptedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match self.steps.pop_front() {
                Some(WriteStep::Accept(n)) => {
                    let n = n.min(buf.len());
                    self.accepted.extend_from_slice(&buf[..n]);
                    Ok(n)
                }
                Some(WriteStep::Fail(kind)) => {
                    Err(std::io::Error::new(kind, "scripted write failure"))
                }
                Some(WriteStep::Zero) => Ok(0),
                None => {
                    self.accepted.extend_from_slice(buf);
                    Ok(buf.len())
                }
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            match self.flush_failure {
                Some(kind) => Err(std::io::Error::new(kind, "scripted flush failure")),
                None => Ok(()),
            }
        }
    }

    /// The request line every `write_request_line` unit test below writes.
    const STUB_REQUEST: &[u8] = b"{\"type\":\"get-seed\"}\n";

    /// Acceptance case (c): nothing fails, so the whole line goes and the
    /// classifier says so. The control the other two are read against.
    #[test]
    fn write_request_line_reports_written_when_every_byte_is_accepted() {
        let mut writer = ScriptedWriter::new(&[WriteStep::Accept(5), WriteStep::Accept(6)]);
        let outcome = write_request_line(&mut writer, STUB_REQUEST);
        assert!(
            matches!(outcome, RequestWrite::Written),
            "a write that completes across several passes is `Written` — got {outcome:?}"
        );
        assert_eq!(
            writer.accepted, STUB_REQUEST,
            "every byte of the request line must have reached the writer"
        );
    }

    /// Acceptance case (a): the writer takes seven bytes and then errors.
    /// Issue #434 — `write_all`'s `Result` cannot say those seven went, which
    /// is how a partial write came back as `SocketReply::Unreachable`'s
    /// "the request was never sent".
    #[test]
    fn write_request_line_classifies_a_failure_after_a_partial_write_as_partially_written() {
        let mut writer = ScriptedWriter::new(&[
            WriteStep::Accept(7),
            WriteStep::Fail(std::io::ErrorKind::WouldBlock),
        ]);
        let outcome = write_request_line(&mut writer, STUB_REQUEST);
        assert!(
            matches!(&outcome, RequestWrite::PartiallyWritten { written, .. } if *written == 7),
            "a write that fails having already handed 7 bytes over is `PartiallyWritten {{ \
             written: 7 }}`, never `NothingWritten` — got {outcome:?}"
        );
        assert_eq!(
            writer.accepted.len(),
            7,
            "the classifier's byte count must be the count that really left the caller"
        );
    }

    /// Acceptance case (b): the very first write fails, so nothing left the
    /// process and `SocketReply::Unreachable`'s retry-safe claim holds.
    #[test]
    fn write_request_line_classifies_a_failure_before_the_first_byte_as_nothing_written() {
        let mut writer =
            ScriptedWriter::new(&[WriteStep::Fail(std::io::ErrorKind::ConnectionRefused)]);
        let outcome = write_request_line(&mut writer, STUB_REQUEST);
        assert!(
            matches!(&outcome, RequestWrite::NothingWritten(err)
                if err.kind() == std::io::ErrorKind::ConnectionRefused),
            "a write that fails before a single byte moves is `NothingWritten`, carrying the \
             cause — got {outcome:?}"
        );
        assert!(
            writer.accepted.is_empty(),
            "nothing may have reached the writer"
        );
    }

    /// `write_all` retries `ErrorKind::Interrupted` rather than reporting a
    /// failed write, and the replacement has to as well: an `EINTR` on the
    /// write side says nothing about the peer, exactly as `error/socket/007`
    /// established for the read side. Treating one as a failed write would
    /// newly classify a signalled `delegate` as unsent.
    #[test]
    fn write_request_line_retries_an_interrupted_write_exactly_as_write_all_does() {
        let mut writer = ScriptedWriter::new(&[
            WriteStep::Fail(std::io::ErrorKind::Interrupted),
            WriteStep::Accept(3),
            WriteStep::Fail(std::io::ErrorKind::Interrupted),
        ]);
        let outcome = write_request_line(&mut writer, STUB_REQUEST);
        assert!(
            matches!(outcome, RequestWrite::Written),
            "an interrupted write is retried, not reported — got {outcome:?}"
        );
        assert_eq!(
            writer.accepted, STUB_REQUEST,
            "the retry must resume at the byte the interruption left off at"
        );
    }

    /// A `write` returning `Ok(0)` for a non-empty buffer is `write_all`'s
    /// `ErrorKind::WriteZero`, and it is classified by the same byte count as
    /// any other failure — partial when bytes had already gone, nothing when
    /// they had not. A loop that merely retried it would never return.
    #[test]
    fn write_request_line_treats_a_short_circuiting_zero_write_as_a_classified_failure() {
        let mut after = ScriptedWriter::new(&[WriteStep::Accept(4), WriteStep::Zero]);
        let outcome = write_request_line(&mut after, STUB_REQUEST);
        assert!(
            matches!(&outcome, RequestWrite::PartiallyWritten { written, err }
                if *written == 4 && err.kind() == std::io::ErrorKind::WriteZero),
            "an `Ok(0)` after 4 bytes is a `WriteZero` failure with 4 bytes gone — got \
             {outcome:?}"
        );

        let mut immediately = ScriptedWriter::new(&[WriteStep::Zero]);
        let outcome = write_request_line(&mut immediately, STUB_REQUEST);
        assert!(
            matches!(&outcome, RequestWrite::NothingWritten(err)
                if err.kind() == std::io::ErrorKind::WriteZero),
            "an `Ok(0)` on the first call left nothing behind — got {outcome:?}"
        );
    }

    /// The flush is the second half of what `write_all(..).is_err() ||
    /// flush().is_err()` collapsed, and it is classified by the same count:
    /// by the time a non-empty line reaches the flush every byte is already
    /// gone, so a flush failure cannot mean "nothing sent". Read rather than
    /// assumed: `platform::ipc::windows`'s `flush` returns a literal `Ok(())`
    /// and `platform::ipc::unix`'s delegates to `std`'s `UnixStream`, so this
    /// pins the arm rather than reproducing a failure seen in production.
    #[test]
    fn write_request_line_counts_a_flush_failure_after_a_complete_write_as_bytes_gone() {
        let mut writer = ScriptedWriter::new(&[]).failing_flush(std::io::ErrorKind::BrokenPipe);
        let outcome = write_request_line(&mut writer, STUB_REQUEST);
        assert!(
            matches!(&outcome, RequestWrite::PartiallyWritten { written, .. }
                if *written == STUB_REQUEST.len()),
            "a flush that fails after the whole line went is still bytes-gone, not \
             nothing-sent — got {outcome:?}"
        );
    }

    /// How many bytes one Unix-domain stream socket on this host accepts from a
    /// writer before the writer would block, with nobody reading the other end
    /// — measured on a fresh `UnixStream::pair`, which gets the same default
    /// buffers as the connected pair `socket_013` uses.
    ///
    /// Measured rather than assumed because it is host tuning, and it is not
    /// the knob it looks like. On Linux an `AF_UNIX` stream writer is bounded
    /// by its **own** `SO_SNDBUF` (`net.core.wmem_default` for a socket that
    /// never sets one), not by the reader's `SO_RCVBUF`: shrinking the stub
    /// daemon's receive buffer to 4 KiB, which PR #1232's second revision
    /// did, left the client writing exactly 219,264 bytes either way. On
    /// macOS/BSD the reader's receive buffer does take part. A probe of the
    /// real socket type covers both without the test having to know which.
    #[cfg(unix)]
    fn unix_stream_write_capacity() -> usize {
        use std::io::Write as _;
        /// Well past any buffer a host plausibly configures; reaching it
        /// means the probe is not measuring what it thinks it is.
        const PROBE_LIMIT: usize = 1 << 30;
        let (writer, _reader) =
            std::os::unix::net::UnixStream::pair().expect("create Unix stream pair for probe");
        writer
            .set_nonblocking(true)
            .expect("make probe writer non-blocking");
        let chunk = vec![0u8; 64 * 1024];
        let mut total = 0usize;
        loop {
            match (&writer).write(&chunk) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => panic!("probe write failed after {total} bytes: {err}"),
            }
            assert!(
                total < PROBE_LIMIT,
                "probe wrote {total} bytes without blocking — the reader end is not holding"
            );
        }
        total
    }

    /// Scenario: A stub daemon accepts the connection and then never reads a
    /// byte, while the client sends a request line several times larger than
    /// this host's measured Unix-socket buffering under a 300ms deadline.
    /// The kernel takes the prefix that fits and the rest of the write times
    /// out, so part of the line is already in the daemon's buffer when the
    /// write fails. That must classify as `SocketReply::NoReply` — possibly
    /// sent, unconfirmed — and not as `SocketReply::Unreachable`, whose doc
    /// promises a caller that nothing left the process and a retry cannot
    /// duplicate anything.
    #[spec("error/socket/013")]
    #[test]
    #[cfg(unix)]
    fn socket_013_a_write_that_breaks_after_bytes_left_is_no_reply_not_unreachable() {
        /// The payload floor: what this test sent before the probe existed,
        /// about 18x Linux's default ~230 KiB of buffering, so on an untuned
        /// host the probe changes nothing.
        const MIN_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;
        /// Short enough to keep the test quick, long enough that the first
        /// write is not racing the deadline for the prefix that does fit.
        const BUDGET: std::time::Duration = std::time::Duration::from_millis(300);

        // PR #1232 review: a fixed 4 MiB payload made the outcome depend on
        // host tuning — raise `net.core.wmem_default` past it and the whole
        // line fits, the classifier correctly reports `Read(DeadlineExpired)`,
        // and the second assertion below fails with nothing wrong in the
        // code under test. Four times the measured capacity keeps the line
        // out of reach wherever the host puts that number. It can exceed the
        // daemon's 8 MiB `MAX_HOOK_LINE_BYTES` on a heavily tuned host, which
        // does not matter here: the stub never reads, so no line cap applies.
        let capacity = unix_stream_write_capacity();
        let payload_bytes = MIN_PAYLOAD_BYTES.max(capacity.saturating_mul(4));

        let _tmp = tempfile::tempdir().expect("create temp dir for stub daemon socket");
        let socket_path = _tmp.path().join("s.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind stub daemon socket");

        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        // Stub daemon: accept, then hold the connection open without reading
        // a single byte, so the client's write fills the socket's buffering
        // and stalls. Released only after the client has returned — closing
        // early would hand the client an `EPIPE` on an empty buffer instead.
        let daemon_thread = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let _ = release_rx.recv();
                drop(stream);
            }
        });

        let request = format!(
            r#"{{"type":"get-seed","pad":"{}"}}"#,
            "a".repeat(payload_bytes)
        );
        let (reply, cause) = request_from_socket_at_detailed(&socket_path, &request, Some(BUDGET));

        let _ = release_tx.send(());
        daemon_thread.join().expect("stub daemon thread panicked");

        assert!(
            matches!(reply, SocketReply::NoReply),
            "a write that broke with bytes already in the daemon's buffer must be \
             SocketReply::NoReply — `Unreachable` tells its caller nothing was sent and the \
             request is safe to resend, and the daemon's reader treats a trailing \
             unterminated line as a line, so a resend can double-send. Got {reply:?} \
             (cause: {cause:?})."
        );
        assert!(
            matches!(&cause, Some(NoReplyCause::PartialWrite { written, total })
                if *written > 0 && *written < *total),
            "and it must be NoReply for the WRITE reason, with a prefix gone and a \
             remainder not: a `Read(DeadlineExpired)` here would mean the whole line fit \
             after all and this test proved nothing about partial writes. Got {cause:?} \
             (probed capacity {capacity} bytes, payload {payload_bytes} bytes)."
        );
    }

    /// The other side of `error/socket/013`: with no listener at all the
    /// connect fails, nothing is written, and `SocketReply::Unreachable`
    /// keeps the strong meaning its doc claims. Plain `#[test]` rather than a
    /// catalog entry because it pins a classification that has not changed —
    /// it is here because, as `error/socket/003`'s catalog entry records, no
    /// test asserted `Unreachable` at all before issue #434 made the variant
    /// load-bearing.
    #[test]
    #[cfg(unix)]
    fn an_absent_socket_is_unreachable_with_no_cause_recorded() {
        let _tmp = tempfile::tempdir().expect("create temp dir for the absent socket path");
        let absent = _tmp.path().join("nobody-is-listening.sock");

        let (reply, cause) = request_from_socket_at_detailed(
            &absent,
            r#"{"type":"get-seed"}"#,
            Some(std::time::Duration::from_millis(300)),
        );

        assert!(
            matches!(reply, SocketReply::Unreachable) && cause.is_none(),
            "a connect that never succeeded wrote nothing, so the caller may report \"not \
             delivered\" and retry — got {reply:?} (cause: {cause:?})"
        );
    }

    #[test]
    fn deserialize_claude_code_hook_input() {
        let json = r#"{
            "session_id": "abc-123",
            "hook_event_name": "PreToolUse",
            "cwd": "/home/user",
            "tool_name": "Bash",
            "tool_input": {"command": "ls -la"},
            "source": "claude_code"
        }"#;
        let input: ClaudeCodeHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.session_id, "abc-123");
        assert_eq!(input.hook_event_name, "PreToolUse");
        assert_eq!(input.tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn deserialize_minimal_hook_input() {
        let json = r#"{
            "session_id": "abc-123",
            "hook_event_name": "SessionStart"
        }"#;
        let input: ClaudeCodeHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.session_id, "abc-123");
        assert!(input.cwd.is_none());
        assert!(input.tool_name.is_none());
        assert!(input.tool_input.is_none());
    }

    // --- OpenCode tests ---

    #[test]
    fn map_opencode_session_created() {
        assert_eq!(
            map_opencode_event_type("session.created", None),
            Some(EventType::SessionStart)
        );
    }

    #[test]
    fn map_opencode_session_deleted() {
        assert_eq!(
            map_opencode_event_type("session.deleted", None),
            Some(EventType::SessionEnd)
        );
    }

    #[test]
    fn map_opencode_session_idle() {
        assert_eq!(
            map_opencode_event_type("session.idle", None),
            Some(EventType::Idle)
        );
    }

    #[test]
    fn map_opencode_session_error() {
        assert_eq!(
            map_opencode_event_type("session.error", None),
            Some(EventType::Error)
        );
    }

    #[test]
    fn map_opencode_session_status_default() {
        assert_eq!(
            map_opencode_event_type("session.status", None),
            Some(EventType::Thinking)
        );
        assert_eq!(
            map_opencode_event_type("session.status", Some("busy")),
            Some(EventType::Thinking)
        );
        assert_eq!(
            map_opencode_event_type("session.status.updated", Some("retry")),
            Some(EventType::Thinking)
        );
    }

    #[test]
    fn map_opencode_session_status_idle() {
        assert_eq!(
            map_opencode_event_type("session.status", Some("idle")),
            Some(EventType::Idle)
        );
    }

    #[test]
    fn map_opencode_permission_asked() {
        assert_eq!(
            map_opencode_event_type("permission.asked", None),
            Some(EventType::PermissionRequest)
        );
    }

    #[test]
    fn map_opencode_session_status_error() {
        assert_eq!(
            map_opencode_event_type("session.status", Some("error")),
            Some(EventType::Error)
        );
    }

    #[test]
    fn map_opencode_tool_before() {
        assert_eq!(
            map_opencode_event_type("tool.execute.before", None),
            Some(EventType::ToolStart)
        );
    }

    #[test]
    fn map_opencode_tool_after() {
        assert_eq!(
            map_opencode_event_type("tool.execute.after", None),
            Some(EventType::ToolEnd)
        );
    }

    #[test]
    fn map_opencode_unknown_returns_none() {
        assert_eq!(map_opencode_event_type("unknown.event", None), None);
    }

    #[test]
    fn build_opencode_event_session_created() {
        let input = OpenCodeHookInput {
            session_id: "oc-123".into(),
            event: "session.created".into(),
            tool_name: None,
            tool_input: None,
            status: None,
            cwd: Some("/tmp".into()),
            prompt: None,
            _extra: HashMap::new(),
            ..Default::default()
        };
        let event = build_opencode_event(input).unwrap();
        assert_eq!(event.session_id, "oc-123");
        assert_eq!(event.agent_type, AgentType::OpenCode);
        assert_eq!(event.event_type, EventType::SessionStart);
        assert_eq!(event.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn build_opencode_event_tool_with_detail() {
        let input = OpenCodeHookInput {
            session_id: "oc-123".into(),
            event: "tool.execute.before".into(),
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "cargo build"})),
            status: None,
            cwd: None,
            prompt: None,
            _extra: HashMap::new(),
            ..Default::default()
        };
        let event = build_opencode_event(input).unwrap();
        assert_eq!(event.event_type, EventType::ToolStart);
        assert_eq!(event.tool_name.as_deref(), Some("Bash"));
        assert_eq!(event.tool_detail.as_deref(), Some("cargo build"));
    }

    #[test]
    fn build_opencode_event_unknown_returns_none() {
        let input = OpenCodeHookInput {
            session_id: "oc-123".into(),
            event: "unknown.event".into(),
            tool_name: None,
            tool_input: None,
            status: None,
            cwd: None,
            prompt: None,
            _extra: HashMap::new(),
            ..Default::default()
        };
        assert!(build_opencode_event(input).is_none());
    }

    #[test]
    fn deserialize_opencode_hook_input() {
        let json = r#"{
            "session_id": "oc-456",
            "event": "tool.execute.before",
            "tool_name": "Read",
            "tool_input": {"file_path": "/src/main.rs"},
            "cwd": "/home/user",
            "extra_field": "ignored"
        }"#;
        let input: OpenCodeHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.session_id, "oc-456");
        assert_eq!(input.event, "tool.execute.before");
        assert_eq!(input.tool_name.as_deref(), Some("Read"));
        assert!(input.status.is_none());
    }

    #[test]
    fn deserialize_minimal_opencode_input() {
        let json = r#"{
            "session_id": "oc-456",
            "event": "session.created"
        }"#;
        let input: OpenCodeHookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.session_id, "oc-456");
        assert!(input.tool_name.is_none());
        assert!(input.status.is_none());
        assert!(input.cwd.is_none());
    }

    /// Serialize env-var-mutating tests to avoid races.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Issue #424: a launcher's Claude-shaped `SessionStart` can declare that it
    /// is a BOOTSTRAP, and that declaration has to survive the hook builder —
    /// the whole incoming `metadata` object used to be discarded here, which is
    /// what made a launcher indistinguishable from an initialized session.
    #[test]
    fn session_start_origin_survives_the_claude_hook_builder() {
        let origin_payload = |event: &str, value: &str| {
            let mut extra = HashMap::new();
            extra.insert(
                "metadata".to_string(),
                serde_json::json!({ crate::event::SESSION_START_ORIGIN_METADATA_KEY: value }),
            );
            ClaudeCodeHookInput {
                session_id: "bootstrap-pane-1".into(),
                hook_event_name: event.into(),
                cwd: None,
                tool_name: None,
                tool_input: None,
                tool_use_id: None,
                prompt: None,
                source: None,
                subagent_id: None,
                _extra: extra,
                ..Default::default()
            }
        };

        let launcher = build_event(origin_payload(
            "SessionStart",
            crate::event::WRAPPER_FORK_SESSION_START_ORIGIN,
        ))
        .expect("SessionStart maps to an event");
        assert!(
            launcher.is_wrapper_fork_session_start(),
            "a launcher's declared boot provenance must reach the daemon: {:?}",
            launcher.metadata
        );

        // Narrow on purpose: only this key, only this value, only on a
        // `SessionStart`. Everything else stays ignored, so an arbitrary
        // producer cannot push free-form metadata through the hook builder.
        let unknown_value = build_event(origin_payload("SessionStart", "something-else"))
            .expect("SessionStart maps to an event");
        assert!(!unknown_value.is_wrapper_fork_session_start());
        assert!(unknown_value.metadata.is_empty());
        let wrong_event = build_event(origin_payload(
            "UserPromptSubmit",
            crate::event::WRAPPER_FORK_SESSION_START_ORIGIN,
        ))
        .expect("UserPromptSubmit maps to an event");
        assert!(!wrong_event.is_wrapper_fork_session_start());
        assert!(wrong_event.metadata.is_empty());

        // And an ordinary agent's `SessionStart`, which carries no metadata at
        // all, is still read as a genuine initialized session.
        let genuine = build_event(ClaudeCodeHookInput {
            session_id: "real".into(),
            hook_event_name: "SessionStart".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: None,
            source: None,
            subagent_id: None,
            _extra: HashMap::new(),
            ..Default::default()
        })
        .expect("SessionStart maps to an event");
        assert!(!genuine.is_wrapper_fork_session_start());
    }

    /// `ClaudeCodeHookInput.source == "clear"` on a `SessionStart` must
    /// forward `CLEAR_SESSION_START_METADATA_KEY` / `CLEAR_SESSION_START_METADATA_VALUE`
    /// into `AgentEvent.metadata` — narrowly, mirroring
    /// `session_start_origin_survives_the_claude_hook_builder` above for the
    /// sibling `SESSION_START_ORIGIN_METADATA_KEY` forwarding. Also covers a
    /// non-`ClaudeCode` `agent_type` (built via `build_event_typed` directly,
    /// since `build_event` hardcodes `ClaudeCode`) not forwarding the key
    /// even when `source == "clear"`.
    #[test]
    fn clear_session_start_source_forwards_narrowly() {
        let payload = |event: &str, source: Option<&str>| ClaudeCodeHookInput {
            session_id: "clear-pane-1".into(),
            hook_event_name: event.into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: None,
            source: source.map(str::to_string),
            subagent_id: None,
            _extra: HashMap::new(),
            ..Default::default()
        };

        // A `SessionStart` with `source: "clear"` forwards the key.
        let cleared = build_event(payload("SessionStart", Some("clear")))
            .expect("SessionStart maps to an event");
        assert_eq!(
            cleared
                .metadata
                .get(crate::event::CLEAR_SESSION_START_METADATA_KEY)
                .map(String::as_str),
            Some(crate::event::CLEAR_SESSION_START_METADATA_VALUE),
            "a `/clear`-originated SessionStart must forward the metadata key: {:?}",
            cleared.metadata
        );

        // A `SessionStart` with a different (or missing) `source` does NOT
        // forward the key — only the literal `"clear"` value is narrow-cased.
        let startup = build_event(payload("SessionStart", Some("startup")))
            .expect("SessionStart maps to an event");
        assert!(
            !startup
                .metadata
                .contains_key(crate::event::CLEAR_SESSION_START_METADATA_KEY),
            "source: \"startup\" must not forward the clear-session-start key: {:?}",
            startup.metadata
        );

        let missing_source =
            build_event(payload("SessionStart", None)).expect("SessionStart maps to an event");
        assert!(
            !missing_source
                .metadata
                .contains_key(crate::event::CLEAR_SESSION_START_METADATA_KEY),
            "a SessionStart with no source field must not forward the clear-session-start \
             key: {:?}",
            missing_source.metadata
        );

        // A non-SessionStart event carrying source: "clear" does NOT forward
        // the key either — proves the narrowing is on event_type too, not
        // just on the source value.
        let wrong_event = build_event(payload("UserPromptSubmit", Some("clear")))
            .expect("UserPromptSubmit maps to an event");
        assert!(
            !wrong_event
                .metadata
                .contains_key(crate::event::CLEAR_SESSION_START_METADATA_KEY),
            "a non-SessionStart event must not forward the clear-session-start key even \
             when source is \"clear\": {:?}",
            wrong_event.metadata
        );

        // This feature is Claude-Code only — a `SessionStart` with
        // `source: "clear"` stamped with a non-`ClaudeCode` agent_type must
        // NOT forward the key, even though every other condition is met.
        // Goes through `build_event_typed` directly (not the `build_event`
        // convenience wrapper, which hardcodes `AgentType::ClaudeCode`) so
        // this actually exercises the `agent_type == AgentType::ClaudeCode`
        // gate — deleting that condition would break no other test.
        let non_claude_code =
            build_event_typed(payload("SessionStart", Some("clear")), AgentType::Codex)
                .expect("SessionStart maps to an event");
        assert!(
            !non_claude_code
                .metadata
                .contains_key(crate::event::CLEAR_SESSION_START_METADATA_KEY),
            "a non-ClaudeCode agent_type must not forward the clear-session-start key even \
             when source is \"clear\": {:?}",
            non_claude_code.metadata
        );
    }

    #[test]
    fn pane_id_propagated_from_env_claude_code() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let key = DOT_AGENT_DECK_PANE_ID;
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, "pane-42") };

        let input = ClaudeCodeHookInput {
            session_id: "s1".into(),
            hook_event_name: "SessionStart".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            tool_use_id: None,
            prompt: None,
            source: None,
            subagent_id: None,
            _extra: HashMap::new(),
            ..Default::default()
        };
        let event = build_event(input).unwrap();
        assert_eq!(event.pane_id.as_deref(), Some("pane-42"));

        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn pane_id_propagated_from_env_opencode() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let key = DOT_AGENT_DECK_PANE_ID;
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, "pane-99") };

        let input = OpenCodeHookInput {
            session_id: "oc-1".into(),
            event: "session.created".into(),
            cwd: None,
            tool_name: None,
            tool_input: None,
            prompt: None,
            status: None,
            _extra: HashMap::new(),
            ..Default::default()
        };
        let event = build_opencode_event(input).unwrap();
        assert_eq!(event.pane_id.as_deref(), Some("pane-99"));

        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn build_event_bash_tool_start_stores_full_command() {
        let full_cmd = "kubectl get pods -n production\nkubectl get svc -n production";
        let input = ClaudeCodeHookInput {
            session_id: "s1".into(),
            hook_event_name: "PreToolUse".into(),
            cwd: None,
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": full_cmd})),
            tool_use_id: None,
            prompt: None,
            source: None,
            subagent_id: None,
            _extra: HashMap::new(),
            ..Default::default()
        };
        let event = build_event(input).unwrap();
        assert_eq!(
            event.metadata.get("bash_command").map(String::as_str),
            Some(full_cmd),
        );
        // tool_detail should only have the first line (truncated)
        assert_eq!(
            event.tool_detail.as_deref(),
            Some("kubectl get pods -n production"),
        );
    }

    #[test]
    fn build_event_non_bash_tool_start_no_bash_command() {
        let input = ClaudeCodeHookInput {
            session_id: "s1".into(),
            hook_event_name: "PreToolUse".into(),
            cwd: None,
            tool_name: Some("Read".into()),
            tool_input: Some(serde_json::json!({"file_path": "/src/main.rs"})),
            tool_use_id: None,
            prompt: None,
            source: None,
            subagent_id: None,
            _extra: HashMap::new(),
            ..Default::default()
        };
        let event = build_event(input).unwrap();
        assert!(!event.metadata.contains_key("bash_command"));
    }

    #[test]
    fn build_event_bash_tool_end_no_bash_command() {
        let input = ClaudeCodeHookInput {
            session_id: "s1".into(),
            hook_event_name: "PostToolUse".into(),
            cwd: None,
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "ls -la"})),
            tool_use_id: None,
            prompt: None,
            source: None,
            subagent_id: None,
            _extra: HashMap::new(),
            ..Default::default()
        };
        let event = build_event(input).unwrap();
        assert!(!event.metadata.contains_key("bash_command"));
    }

    #[test]
    fn build_opencode_event_bash_tool_start_stores_full_command() {
        let full_cmd = "helm status my-release --namespace prod";
        let input = OpenCodeHookInput {
            session_id: "oc-1".into(),
            event: "tool.execute.before".into(),
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": full_cmd})),
            status: None,
            cwd: None,
            prompt: None,
            _extra: HashMap::new(),
            ..Default::default()
        };
        let event = build_opencode_event(input).unwrap();
        assert_eq!(
            event.metadata.get("bash_command").map(String::as_str),
            Some(full_cmd),
        );
    }

    fn claude_payload(event: &str) -> ClaudeCodeHookInput {
        ClaudeCodeHookInput {
            session_id: "s-714".into(),
            hook_event_name: event.into(),
            ..Default::default()
        }
    }

    /// Scenario: Install the deck's Claude Code hooks into an existing settings
    /// file with the StopFailure gate open, then feed the hook builder a
    /// StopFailure for every error kind Claude Code names, a Notification with
    /// a type, and transcript paths that must be refused. Every StopFailure ends
    /// as Blocked or Error (never dropped), the notification type is forwarded,
    /// and the tail read is bounded and refuses a FIFO and a non-.jsonl path.
    #[spec("status/blocked/011")]
    #[test]
    fn status_blocked_011_stop_failure_is_installed_and_never_leaves_thinking() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, r#"{"model":"opus","hooks":{}}"#).unwrap();
        crate::hooks_manage::install_to_gated(&settings, "/opt/deck/dot-agent-deck", true).unwrap();
        let written: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
        assert_eq!(written["model"], "opus");
        let stop_failure = written["hooks"]["StopFailure"].to_string();
        assert!(
            stop_failure.contains("hook --agent claude-code"),
            "{stop_failure}"
        );
        assert!(
            written["hooks"]["Stop"]
                .to_string()
                .contains("hook --agent claude-code")
        );

        // Every error kind maps to a terminal status.
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"assistant","isSidechain":false,"error":"rate_limit","isApiErrorMessage":true,"#,
                r#""quotaLimits":{"status":"rejected","resetsAt":1790409000}}"#,
                "\n"
            ),
        )
        .unwrap();
        for kind in [
            "authentication_failed",
            "oauth_org_not_allowed",
            "billing_error",
            "rate_limit",
            "invalid_request",
            "server_error",
            "overloaded",
            "max_output_tokens",
            "model_not_found",
            "unknown",
        ] {
            let mut input = claude_payload("StopFailure");
            input.error = Some(kind.into());
            input.transcript_path = Some(transcript.to_string_lossy().into_owned());
            input.last_assistant_message = Some("provider said\u{202e} no".into());
            let event = build_event(input).unwrap_or_else(|| panic!("{kind} was dropped"));
            match kind {
                "billing_error" | "rate_limit" => {
                    assert_eq!(event.event_type, EventType::QuotaBlocked, "{kind}");
                    assert_eq!(
                        event.metadata[crate::quota_block::QUOTA_BLOCKED_DETAIL_METADATA_KEY],
                        "provider said no"
                    );
                }
                _ => assert_eq!(event.event_type, EventType::Error, "{kind}"),
            }
        }
        let mut rate = claude_payload("StopFailure");
        rate.error = Some("rate_limit".into());
        rate.transcript_path = Some(transcript.to_string_lossy().into_owned());
        let event = build_event(rate).unwrap();
        assert_eq!(
            event.metadata[crate::quota_block::QUOTA_BLOCKED_KIND_METADATA_KEY],
            "usage_limit"
        );
        assert_eq!(
            event.metadata[crate::quota_block::QUOTA_BLOCKED_RESETS_AT_MS_METADATA_KEY],
            "1790409000000"
        );
        // No error kind, or no transcript for `rate_limit`: still Error.
        assert_eq!(
            build_event(claude_payload("StopFailure"))
                .unwrap()
                .event_type,
            EventType::Error
        );
        // Codex fires no StopFailure, and a Codex-shaped one is not classified.
        let mut codex = claude_payload("StopFailure");
        codex.error = Some("billing_error".into());
        assert_eq!(
            build_event_typed(codex, AgentType::Codex)
                .unwrap()
                .event_type,
            EventType::Error
        );

        // The notification type rides the WaitingForInput event.
        let mut notification = claude_payload("Notification");
        notification.notification_type = Some("idle_prompt".into());
        let event = build_event(notification).unwrap();
        assert_eq!(event.event_type, EventType::WaitingForInput);
        assert_eq!(
            event.metadata[crate::quota_block::NOTIFICATION_TYPE_METADATA_KEY],
            "idle_prompt"
        );

        // The tail read: bounded, and refused for a non-.jsonl path, a
        // relative path, and a FIFO.
        let big = dir.path().join("big.jsonl");
        let mut body = vec![b'x'; 300 * 1024];
        body.extend_from_slice(b"\n{\"type\":\"assistant\"}\n");
        std::fs::write(&big, &body).unwrap();
        let tail = crate::quota_signals::read_claude_transcript_tail(&big.to_string_lossy())
            .expect("a regular .jsonl file is read");
        assert!(tail.len() as u64 <= crate::quota_signals::CLAUDE_TRANSCRIPT_TAIL_BYTES);
        assert_eq!(
            tail, b"{\"type\":\"assistant\"}\n",
            "the cut first line is dropped"
        );
        // A window that starts exactly on a record boundary keeps its first
        // record: when that record is the quota error that ended the turn,
        // dropping it would leave the card Error instead of Blocked.
        let boundary = dir.path().join("boundary.jsonl");
        let record = r#"{"type":"assistant","isSidechain":false,"error":"rate_limit","isApiErrorMessage":true,"quotaLimits":{"status":"rejected","resetsAt":1790409000}}"#;
        let window = crate::quota_signals::CLAUDE_TRANSCRIPT_TAIL_BYTES as usize;
        let trailer_frame = r#"{"type":"system","subtype":"turn_duration","pad":""}"#.len();
        let pad = "p".repeat(window - record.len() - 1 - trailer_frame - 1);
        let mut body = b"{\"type\":\"user\"}\n".to_vec();
        body.extend_from_slice(record.as_bytes());
        body.push(b'\n');
        body.extend_from_slice(
            format!(r#"{{"type":"system","subtype":"turn_duration","pad":"{pad}"}}"#).as_bytes(),
        );
        body.push(b'\n');
        std::fs::write(&boundary, &body).unwrap();
        let tail = crate::quota_signals::read_claude_transcript_tail(&boundary.to_string_lossy())
            .expect("a regular .jsonl file is read");
        assert_eq!(
            crate::quota_signals::classify_claude_stop_failure(
                Some("rate_limit"),
                crate::quota_signals::last_claude_api_error_record(&tail).as_ref(),
            ),
            crate::quota_signals::FailureOutcome::Blocked {
                kind: crate::quota_block::BlockedKind::UsageLimit,
                resets_at_ms: Some(1_790_409_000_000),
            },
            "a quota record starting exactly at the window boundary still blocks"
        );
        assert_eq!(tail.len(), window, "the whole window is complete records");
        let txt = dir.path().join("t.txt");
        std::fs::write(&txt, "{}").unwrap();
        assert!(
            crate::quota_signals::read_claude_transcript_tail(&txt.to_string_lossy()).is_none()
        );
        assert!(crate::quota_signals::read_claude_transcript_tail("t.jsonl").is_none());
        #[cfg(unix)]
        {
            let fifo = dir.path().join("fifo.jsonl");
            let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
            // SAFETY: a valid NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            assert!(
                crate::quota_signals::read_claude_transcript_tail(&fifo.to_string_lossy())
                    .is_none(),
                "a FIFO is refused without blocking the hook"
            );
        }
    }

    /// Issue #714: a Codex hook hands the daemon's rollout tailer its session
    /// log and turn — on `SessionStart`, `UserPromptSubmit` and `Stop` only —
    /// and an OpenCode `session.error` classifies its structured fields.
    #[test]
    fn codex_rollout_keys_and_opencode_error_fields_are_forwarded() {
        use crate::codex_rollout_tail::{
            CODEX_TRANSCRIPT_PATH_METADATA_KEY, CODEX_TURN_ID_METADATA_KEY,
        };
        let codex = |event: &str| {
            let mut input = claude_payload(event);
            input.transcript_path = Some("/h/.codex/sessions/rollout-a.jsonl".into());
            input.turn_id = Some("t1".into());
            build_event_typed(input, AgentType::Codex).unwrap()
        };
        let prompt = codex("UserPromptSubmit");
        assert_eq!(prompt.metadata[CODEX_TURN_ID_METADATA_KEY], "t1");
        assert_eq!(
            prompt.metadata[CODEX_TRANSCRIPT_PATH_METADATA_KEY],
            "/h/.codex/sessions/rollout-a.jsonl"
        );
        assert!(
            codex("SessionStart")
                .metadata
                .contains_key(CODEX_TRANSCRIPT_PATH_METADATA_KEY)
        );
        assert_eq!(codex("Stop").metadata[CODEX_TURN_ID_METADATA_KEY], "t1");
        assert!(
            !codex("PreToolUse")
                .metadata
                .contains_key(CODEX_TURN_ID_METADATA_KEY)
        );
        let mut claude = claude_payload("UserPromptSubmit");
        claude.transcript_path = Some("/x.jsonl".into());
        claude.turn_id = Some("t".into());
        assert!(build_event(claude).unwrap().metadata.is_empty());
        let mut long = claude_payload("UserPromptSubmit");
        long.turn_id = Some("x".repeat(crate::codex_rollout_tail::MAX_METADATA_BYTES + 1));
        assert!(
            !build_event_typed(long, AgentType::Codex)
                .unwrap()
                .metadata
                .contains_key(CODEX_TURN_ID_METADATA_KEY)
        );

        let opencode: OpenCodeHookInput = serde_json::from_str(
            r#"{"session_id":"oc","event":"session.error","error_name":"APIError",
                "error_message":"The usage limit has been reached",
                "response_markers":{"error":{"type":"usage_limit_reached","resets_at":1790001000}},
                "response_headers":{"Retry-After":"60"}}"#,
        )
        .unwrap();
        let event = build_opencode_event(opencode).unwrap();
        assert_eq!(event.event_type, EventType::QuotaBlocked);
        assert_eq!(
            event.metadata[crate::quota_block::QUOTA_BLOCKED_DETAIL_METADATA_KEY],
            "The usage limit has been reached"
        );
        let bare: OpenCodeHookInput = serde_json::from_str(
            r#"{"session_id":"oc","event":"session.error","error_name":"APIError",
                "response_markers":{"error":{"type":"rate_limit_error"}},
                "response_headers":"not an object"}"#,
        )
        .unwrap();
        assert_eq!(
            build_opencode_event(bare).unwrap().event_type,
            EventType::Error
        );
    }

    /// Issue #1354: the payload's `agent_id` — set by Claude Code and Codex
    /// only on a hook fired inside a subagent — is forwarded as
    /// `SUBAGENT_ID_METADATA_KEY` for exactly those two agents, and never
    /// mistaken for the deck's own `AgentEvent::agent_id`.
    #[test]
    fn subagent_agent_id_forwards_for_claude_and_codex_only() {
        let payload = |agent_id: Value| {
            serde_json::from_value::<ClaudeCodeHookInput>(serde_json::json!({
                "session_id": "sub-1",
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": "ls"},
                "agent_id": agent_id,
                "agent_type": "general-purpose",
            }))
            .expect("a Claude-shaped payload decodes whatever `agent_id` holds")
        };
        let key = crate::event::SUBAGENT_ID_METADATA_KEY;

        for agent_type in [AgentType::ClaudeCode, AgentType::Codex] {
            let event = build_event_typed(payload("a7c1".into()), agent_type.clone()).unwrap();
            assert_eq!(
                event.metadata.get(key).map(String::as_str),
                Some("a7c1"),
                "{agent_type:?} must forward the subagent id"
            );
            assert!(event.is_from_subagent());
            assert_ne!(
                event.agent_id.as_deref(),
                Some("a7c1"),
                "the payload's agent_id is not the deck's agent id"
            );
        }

        // Devin's hook input has no such field; a future one of unknown meaning
        // must not start suppressing its card's status.
        let devin = build_event_typed(payload("a7c1".into()), AgentType::Devin).unwrap();
        assert!(!devin.is_from_subagent());

        // An empty id, a non-string shape, and an absent key are all "main thread".
        for odd in [
            Value::from(""),
            Value::from(7),
            serde_json::json!({"id": "x"}),
            Value::Null,
        ] {
            let event = build_event(payload(odd.clone())).unwrap();
            assert!(!event.is_from_subagent(), "agent_id = {odd}");
            assert_eq!(event.event_type, EventType::ToolStart, "agent_id = {odd}");
        }
    }

    /// Issues #714 and #1354: a `StopFailure` fired inside a subagent ends that
    /// subagent's run, not the main thread's turn — its failure reaches the
    /// main thread as the subagent's result, and the main thread reports its
    /// own quota refusal if it meets one. So it is neither classified into a
    /// `QuotaBlocked` nor left as an `Error`, both of which assert the parent
    /// card's status; it arrives as the `SubagentStop` it stands in for.
    #[test]
    fn subagent_stop_failure_does_not_block_or_error_the_parent() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"assistant","isSidechain":false,"error":"rate_limit","isApiErrorMessage":true,"#,
                r#""quotaLimits":{"status":"rejected","resetsAt":1790409000}}"#,
                "\n"
            ),
        )
        .unwrap();
        for kind in ["rate_limit", "billing_error", "server_error"] {
            let mut input = claude_payload("StopFailure");
            input.error = Some(kind.into());
            input.transcript_path = Some(transcript.to_string_lossy().into_owned());
            input.last_assistant_message = Some("provider said no".into());
            input.subagent_id = Some("a7c1".into());
            let event = build_event(input).expect("a subagent's StopFailure is kept");
            assert_eq!(event.event_type, EventType::SubagentStop, "{kind}");
            assert!(event.is_from_subagent());
            assert!(
                crate::quota_block::QUOTA_BLOCKED_METADATA_KEYS
                    .iter()
                    .all(|key| !event.metadata.contains_key(*key)),
                "{kind}: no quota reason travels without the status"
            );
        }
        // The main thread's own StopFailure is still classified.
        let mut main = claude_payload("StopFailure");
        main.error = Some("rate_limit".into());
        main.transcript_path = Some(transcript.to_string_lossy().into_owned());
        assert_eq!(
            build_event(main).unwrap().event_type,
            EventType::QuotaBlocked
        );
    }
}

/// PRD #1542: the questions the hook CLI builds from real agent payloads
/// (`tests/fixtures/agent-questions/`), and the held hook's exchange with the
/// daemon.
#[cfg(test)]
mod question_tests {
    use super::*;
    use crate::question::{
        AnswerChannel, OptionRole, PendingQuestion, QuestionKind, QuestionReply, ReleaseReason,
        ResolvedAnswer,
    };
    use spec::spec;

    fn fixture(name: &str) -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/agent-questions")
            .join(name);
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read fixture"))
            .expect("parse fixture")
    }

    fn event_for(agent: AgentType, payload: &Value) -> AgentEvent {
        let input: ClaudeCodeHookInput =
            serde_json::from_value(payload.clone()).expect("a Claude-shaped payload");
        build_event_typed(input, agent).expect("the payload maps to an event")
    }

    fn question_for(agent: AgentType, payload: &Value) -> PendingQuestion {
        event_for(agent, payload)
            .question()
            .expect("the event carries a question")
    }

    fn labels(question: &PendingQuestion, index: usize) -> Vec<&str> {
        question.questions[index]
            .options
            .iter()
            .map(|o| o.label.as_str())
            .collect()
    }

    /// Scenario: Claude Code 2.1.289's captured `PermissionRequest` for a Bash
    /// command, whose first suggestion is `addDirectories`, becomes a held
    /// Permission question: Yes, "Yes, and always allow access to <dir> from
    /// this project" with the directory as its scope, and No.
    #[spec("question/detect/001")]
    #[test]
    fn question_detect_001_claude_bash_permission_request() {
        let event = event_for(
            AgentType::ClaudeCode,
            &fixture("claude-permission-bash.json"),
        );
        assert_eq!(event.event_type, EventType::PermissionRequest);
        let q = event.question().unwrap();
        assert_eq!(q.kind, QuestionKind::Permission);
        assert_eq!(q.channel, AnswerChannel::Held);
        assert!(q.id.starts_with("q-") && crate::question::is_valid_question_id(&q.id));
        assert_eq!(
            labels(&q, 0),
            vec![
                "Yes",
                "Yes, and always allow access to /work/proj from this project",
                "No"
            ]
        );
        let roles: Vec<OptionRole> = q.questions[0].options.iter().map(|o| o.role).collect();
        assert_eq!(
            roles,
            vec![
                OptionRole::AllowOnce,
                OptionRole::AllowAlways,
                OptionRole::Deny
            ]
        );
        // The confirmation says what is sent: the suggestion's destination
        // is `session`, whatever Claude's own label says.
        assert_eq!(
            q.questions[0].options[1].scope.as_deref(),
            Some("access to /work/proj for the rest of this session")
        );
        let tool = q.tool.as_ref().unwrap();
        assert_eq!(tool.name, "Bash");
        assert_eq!(tool.detail.as_deref(), Some("touch created_m1.txt"));
        assert!(
            claude_held_question(&event, &fixture("claude-permission-bash.json").to_string())
                .is_none(),
            "no pane id in a unit test, so nothing to hold for"
        );
    }

    /// Scenario: Claude Code's captured `PermissionRequest` for a Write, whose
    /// suggestion is `setMode acceptEdits`, names option 2 "Yes, and switch to
    /// accept edits for this session".
    #[spec("question/detect/002")]
    #[test]
    fn question_detect_002_claude_write_permission_names_accept_edits() {
        let q = question_for(
            AgentType::ClaudeCode,
            &fixture("claude-permission-write.json"),
        );
        assert_eq!(
            labels(&q, 0),
            vec![
                "Yes",
                "Yes, and switch to accept edits for this session",
                "No"
            ]
        );
        assert_eq!(q.questions[0].options[1].role, OptionRole::AllowAlways);
        assert!(q.questions[0].options[1].scope.is_some());
    }

    /// Scenario: A two-question `AskUserQuestion` form, the second
    /// multi-select, becomes a held Choice question with both questions, each
    /// followed by Claude Code's own "Type something." (free text) and "Chat
    /// about this" (keyboard-only).
    #[spec("question/detect/003")]
    #[test]
    fn question_detect_003_claude_ask_user_question_form() {
        let q = question_for(
            AgentType::ClaudeCode,
            &fixture("claude-ask-user-question-form.json"),
        );
        assert_eq!(q.kind, QuestionKind::Choice);
        assert_eq!(q.channel, AnswerChannel::Held);
        assert_eq!(q.questions.len(), 2);
        assert_eq!(q.questions[0].prompt, "Which colour?");
        assert_eq!(q.questions[0].header.as_deref(), Some("Colour"));
        assert!(!q.questions[0].multi_select);
        assert!(q.questions[1].multi_select);
        assert_eq!(
            labels(&q, 0),
            vec!["Red", "Green", "Blue", "Type something.", "Chat about this"]
        );
        let appended = &q.questions[1].options[3..];
        assert_eq!(appended[0].role, OptionRole::FreeText);
        assert!(!appended[0].keyboard_only);
        assert!(appended[1].keyboard_only);
        assert_eq!(q.tool.as_ref().unwrap().name, "AskUserQuestion");
    }

    /// Scenario: Claude Code's plan approval is a Plan question answered by
    /// keys, not held — a hook decision does not dismiss the plan dialog — with
    /// its third option keyboard-only.
    #[spec("question/detect/004")]
    #[test]
    fn question_detect_004_claude_plan_is_keys_and_not_held() {
        let payload = fixture("claude-permission-plan.json");
        let mut event = event_for(AgentType::ClaudeCode, &payload);
        event.pane_id = Some("pane-plan".into());
        let q = event.question().unwrap();
        assert_eq!(q.kind, QuestionKind::Plan);
        assert_eq!(q.channel, AnswerChannel::Keys);
        assert_eq!(q.questions[0].options.len(), 3);
        assert!(q.questions[0].options[2].keyboard_only);
        assert!(
            claude_held_question(&event, &payload.to_string()).is_none(),
            "a plan approval must not be held"
        );
        let keys = crate::question::answer_keys(
            &AgentType::ClaudeCode,
            &q,
            &q.validate(
                &[crate::question::QuestionAnswer {
                    question_index: 0,
                    option_indices: vec![2],
                    text: None,
                }],
                false,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(keys, vec!["2"]);
    }

    /// Scenario: Codex 0.160.0's captured `PreToolUse` for a two-question
    /// `request_user_input` stays a ToolStart but carries a Choice question
    /// whose id is the call's `tool_use_id`, answered by keys, each question
    /// ending in a keyboard-only "None of the above".
    #[spec("question/detect/005")]
    #[test]
    fn question_detect_005_codex_request_user_input_form() {
        let event = event_for(AgentType::Codex, &fixture("codex-request-user-input.json"));
        assert_eq!(event.event_type, EventType::ToolStart);
        let q = event.question().unwrap();
        assert_eq!(q.id, "call_3X65ZJFbHuMzyWg4gOyBg1vK");
        assert_eq!(
            q.tool.as_ref().unwrap().use_id.as_deref(),
            Some("call_3X65ZJFbHuMzyWg4gOyBg1vK")
        );
        assert_eq!(q.kind, QuestionKind::Choice);
        assert_eq!(q.channel, AnswerChannel::Keys);
        assert_eq!(
            labels(&q, 0),
            vec!["Red", "Green", "Blue", "None of the above"]
        );
        assert_eq!(labels(&q, 1), vec!["Small", "Large", "None of the above"]);
        assert!(q.questions[1].options[2].keyboard_only);
        assert!(q.questions.iter().all(|q| !q.multi_select));
    }

    /// Scenario: A Codex `PermissionRequest` becomes a Permission question
    /// answered by the keys `1`, `p` and `3`, and Codex's `Interrupt` hook — what
    /// fires after a keyboard "No" — reads as Idle.
    #[spec("question/detect/006")]
    #[test]
    fn question_detect_006_codex_permission_keys_and_interrupt() {
        let q = question_for(AgentType::Codex, &fixture("codex-permission-request.json"));
        assert_eq!(q.kind, QuestionKind::Permission);
        assert_eq!(q.channel, AnswerChannel::Keys);
        assert_eq!(
            q.tool.as_ref().unwrap().detail.as_deref(),
            Some("touch codex_b.txt")
        );
        let keys: Vec<String> = (1..=3)
            .map(|index| {
                let resolved = q
                    .validate(
                        &[crate::question::QuestionAnswer {
                            question_index: 0,
                            option_indices: vec![index],
                            text: None,
                        }],
                        true,
                    )
                    .unwrap();
                crate::question::answer_keys(&AgentType::Codex, &q, &resolved)
                    .unwrap()
                    .join("")
            })
            .collect();
        assert_eq!(keys, vec!["1", "p", "3"]);
        assert!(
            q.questions[0].options[1]
                .scope
                .as_deref()
                .is_some_and(|s| s.contains("touch codex_b.txt"))
        );
        assert_eq!(map_event_type("Interrupt"), Some(EventType::Idle));
    }

    /// Scenario: A Devin `PermissionRequest` becomes a Permission question
    /// from the docs-derived table in which only Allow once and Deny are
    /// answerable; the four "allow for …" scopes and the two edit options are
    /// keyboard-only.
    #[spec("question/detect/009")]
    #[test]
    fn question_detect_009_devin_permission_is_docs_derived() {
        let payload = serde_json::json!({
            "session_id": "devin-1",
            "hook_event_name": "PermissionRequest",
            "tool_name": "exec",
            "tool_input": {"command": "rm -rf build"},
        });
        let q = question_for(AgentType::Devin, &payload);
        assert_eq!(q.channel, AnswerChannel::Keys);
        let answerable: Vec<&str> = q.questions[0]
            .options
            .iter()
            .filter(|o| o.answerable())
            .map(|o| o.label.as_str())
            .collect();
        assert_eq!(answerable, vec!["Allow once", "Deny"]);
        assert_eq!(q.questions[0].options.len(), 8);
    }

    /// Scenario: A Pi extension's `select`, `confirm` and `input` dialogs, as
    /// the deck's Pi extension describes them to `await-answer`, become held
    /// questions, and each answer maps back to the value the asking extension
    /// receives — the option string from the dialog's own list, true or false,
    /// or the typed text. The wrapper around Pi's `ctx.ui` that produces those
    /// descriptions is covered by `pi-extension/test/questions.test.ts`, run
    /// here when Node can strip TypeScript types.
    #[spec("question/detect/008")]
    #[test]
    fn question_detect_008_pi_dialogs_map_both_ways() {
        use crate::question::{PiDialog, pi_dialog, pi_value};
        let dialog = |kind: &str| PiDialog {
            id: "q-pi1".into(),
            kind: kind.into(),
            title: "Pick a colour".into(),
            message: Some("for the bikeshed".into()),
            options: vec!["Red".into(), "Green".into(), "Blue".into()],
            placeholder: Some("colour".into()),
        };
        let reply = |q: &PendingQuestion, index: u32, text: Option<&str>| {
            let resolved = q
                .validate(
                    &[crate::question::QuestionAnswer {
                        question_index: 0,
                        option_indices: vec![index],
                        text: text.map(str::to_string),
                    }],
                    false,
                )
                .unwrap();
            QuestionReply::answered(&q.id, resolved)
        };

        let select = dialog("select");
        let q = pi_dialog(&select, 1).unwrap();
        assert_eq!(q.channel, AnswerChannel::Held);
        assert_eq!(labels(&q, 0), vec!["Red", "Green", "Blue"]);
        assert_eq!(
            pi_value(&select, &q, &reply(&q, 3, None)),
            Some(serde_json::json!({"value": "Blue"}))
        );

        let confirm = dialog("confirm");
        let q = pi_dialog(&confirm, 1).unwrap();
        assert_eq!(q.kind, QuestionKind::Confirm);
        assert_eq!(q.questions[0].prompt, "Pick a colour\nfor the bikeshed");
        assert_eq!(
            pi_value(&confirm, &q, &reply(&q, 1, None)),
            Some(serde_json::json!({"value": true}))
        );
        assert_eq!(
            pi_value(&confirm, &q, &reply(&q, 2, None)),
            Some(serde_json::json!({"value": false}))
        );

        let input = dialog("input");
        let q = pi_dialog(&input, 1).unwrap();
        assert_eq!(q.questions[0].options[0].role, OptionRole::FreeText);
        assert_eq!(q.questions[0].options[0].label, "colour");
        assert_eq!(
            pi_value(&input, &q, &reply(&q, 1, Some("teal"))),
            Some(serde_json::json!({"value": "teal"}))
        );
        assert_eq!(pi_dialog(&dialog("editor"), 1), None);
        assert_eq!(
            pi_value(
                &select,
                &q,
                &QuestionReply::released(&q.id, ReleaseReason::Cleared)
            ),
            None
        );

        // The JS half, when this Node can run TypeScript directly (23.6+).
        let node = std::process::Command::new("node").arg("--version").output();
        let Ok(node) = node else {
            eprintln!("SKIP: node is not available for pi-extension/test/questions.test.ts");
            return;
        };
        let version = String::from_utf8_lossy(&node.stdout);
        let mut parts = version.trim().trim_start_matches('v').split('.');
        let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        if (major, minor) < (23, 6) {
            eprintln!("SKIP: node {version} cannot strip TypeScript types");
            return;
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("pi-extension");
        let out = std::process::Command::new("node")
            .args(["--test", "test/questions.test.ts"])
            .current_dir(&dir)
            .output()
            .expect("run node --test");
        assert!(
            out.status.success(),
            "pi-extension/test/questions.test.ts failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The OpenCode half of `await-answer`: OpenCode's captured `question.asked`
    /// and `permission.asked` become held questions on the plugin's session,
    /// and an answer maps back onto OpenCode's reply bodies by index.
    #[test]
    fn opencode_await_answer_builds_and_maps_both_questions() {
        let raw = serde_json::json!({
            "session_id": "ses_1",
            "event": "question.asked",
            "properties": fixture("opencode-question-asked.json"),
            "cwd": "/work",
        });
        let (event, q, props) = opencode_question_event(&raw.to_string()).unwrap();
        assert_eq!(event.event_type, EventType::WaitingForInput);
        assert_eq!(event.agent_type, AgentType::OpenCode);
        assert_eq!(q.id, "que_1043ff915001MjL9vryfDO6HZX");
        assert_eq!(q.channel, AnswerChannel::Held);
        assert_eq!(
            labels(&q, 0),
            vec!["Red", "Green", "Blue", "Type your own answer"]
        );
        assert!(q.questions[1].multi_select);
        let reply = QuestionReply::answered(
            &q.id,
            resolved(&q, &[(0, &[4], Some("teal")), (1, &[1, 3], None)]),
        );
        assert_eq!(
            crate::question::opencode_reply(&q, &props, &reply),
            Some(serde_json::json!({
                "kind": "question",
                "request_id": "que_1043ff915001MjL9vryfDO6HZX",
                "body": {"answers": [["teal"], ["Small", "Large"]]}
            }))
        );

        let raw = serde_json::json!({
            "session_id": "ses_1",
            "event": "permission.asked",
            "properties": fixture("opencode-permission-asked.json"),
        });
        let (event, q, props) = opencode_question_event(&raw.to_string()).unwrap();
        assert_eq!(event.event_type, EventType::PermissionRequest);
        assert_eq!(event.tool_detail.as_deref(), Some("touch oc_a.txt"));
        assert_eq!(labels(&q, 0), vec!["Allow once", "Allow always", "Reject"]);
        assert_eq!(
            q.questions[0].options[1].scope.as_deref(),
            Some("requests matching touch *")
        );
        for (index, expected) in [(1, "once"), (2, "always"), (3, "reject")] {
            let reply = QuestionReply::answered(&q.id, resolved(&q, &[(0, &[index], None)]));
            assert_eq!(
                crate::question::opencode_reply(&q, &props, &reply).unwrap()["body"]["reply"],
                expected
            );
        }
        assert!(opencode_question_event(r#"{"session_id":"s","event":"session.idle"}"#).is_none());
    }

    /// A one-shot stand-in daemon: accepts one connection, reads one line,
    /// answers with `reply` (or closes without one), and hands back the line.
    #[cfg(unix)]
    fn stub_daemon(
        reply: Option<String>,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::thread::JoinHandle<String>,
    ) {
        use std::io::BufRead as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hook.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if let Some(reply) = reply {
                let mut stream = reader.into_inner();
                let _ = writeln!(stream, "{reply}");
            }
            line
        });
        (dir, path, handle)
    }

    /// Scenario: A Codex approval — answered by keys, so the hook does not
    /// wait for it — leaves the hook as a `question` message that is not held,
    /// carrying the pane's hook token, and the daemon's acknowledgement ends
    /// it. When the daemon answers nothing (an older daemon) or refuses the
    /// token, the hook sends the event again as a plain one with no question
    /// in it, so the card still reads Needs Input.
    #[cfg(unix)]
    #[spec("question/detect/010")]
    #[test]
    fn question_detect_010_unheld_questions_go_through_the_provenance_gate() {
        let mut event = event_for(AgentType::Codex, &fixture("codex-permission-request.json"));
        event.pane_id = Some("pane-unheld".into());
        assert!(carries_question_metadata(&event));
        let ack =
            crate::question::QuestionReply::released("x", crate::question::ReleaseReason::NotHeld);
        let (_dir, path, server) = stub_daemon(Some(serde_json::to_string(&ack).unwrap()));
        assert_eq!(
            send_question_unheld_at(&path, event.clone(), std::time::Duration::from_secs(10)),
            UnheldSend::Attested
        );
        let sent: Value = serde_json::from_str(&server.join().unwrap()).unwrap();
        assert_eq!(sent["message_type"], "question");
        assert_eq!(sent["hold"], false);
        assert_eq!(sent["pane_id"], "pane-unheld");
        assert!(sent["event"]["metadata"][crate::event::QUESTION_METADATA_KEY].is_string());

        // An older daemon: it reads the line and answers nothing. The fallback
        // is a plain event on a second connection, without the question.
        use std::io::BufRead as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hook.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let mut lines = Vec::new();
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                std::io::BufReader::new(stream)
                    .read_line(&mut line)
                    .unwrap();
                lines.push(line);
            }
            lines
        });
        assert_eq!(
            send_question_unheld_at(&path, event.clone(), std::time::Duration::from_millis(300)),
            UnheldSend::Plain
        );
        let lines = server.join().unwrap();
        let plain: AgentEvent = serde_json::from_str(lines[1].trim()).unwrap();
        assert_eq!(plain.event_type, EventType::PermissionRequest);
        assert!(!carries_question_metadata(&plain));

        // A refusal from the provenance gate: the same fallback.
        let refused =
            crate::question::QuestionReply::released("x", crate::question::ReleaseReason::Refused);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hook.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let reply = serde_json::to_string(&refused).unwrap();
        let server = std::thread::spawn(move || {
            let mut lines = Vec::new();
            for i in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if i == 0 {
                    let _ = writeln!(reader.into_inner(), "{reply}");
                }
                lines.push(line);
            }
            lines
        });
        assert_eq!(
            send_question_unheld_at(&path, event, std::time::Duration::from_secs(10)),
            UnheldSend::Plain
        );
        let lines = server.join().unwrap();
        let plain: AgentEvent = serde_json::from_str(lines[1].trim()).unwrap();
        assert!(!carries_question_metadata(&plain));
    }

    fn held_event(payload: &Value) -> (AgentEvent, PendingQuestion) {
        let mut event = event_for(AgentType::ClaudeCode, payload);
        event.pane_id = Some("pane-held".into());
        let q = event.question().unwrap();
        (event, q)
    }

    fn resolved(
        q: &PendingQuestion,
        answers: &[(u32, &[u32], Option<&str>)],
    ) -> Vec<ResolvedAnswer> {
        q.validate(
            &answers
                .iter()
                .map(|(qi, oi, text)| crate::question::QuestionAnswer {
                    question_index: *qi,
                    option_indices: oi.to_vec(),
                    text: text.map(str::to_string),
                })
                .collect::<Vec<_>>(),
            true,
        )
        .unwrap()
    }

    /// Scenario: A held Claude Code permission hook sends its question to a
    /// stand-in daemon as a held `question` message and turns each answer into
    /// exactly the decision Claude Code takes: allow; allow with the payload's
    /// first suggestion as `updatedPermissions`; deny; and, for a form, allow
    /// with `updatedInput` carrying the payload's questions unchanged plus the
    /// answers — a label, an array of labels for the multi-select question, or
    /// the typed text.
    #[cfg(unix)]
    #[spec("question/hold/001")]
    #[test]
    fn question_hold_001_held_hook_prints_the_decision_for_its_own_question() {
        let payload = fixture("claude-permission-bash.json");
        let (event, q) = held_event(&payload);
        let decision = |answers: &[(u32, &[u32], Option<&str>)]| {
            let reply = QuestionReply::answered(&q.id, resolved(&q, answers));
            let (_dir, path, server) = stub_daemon(Some(serde_json::to_string(&reply).unwrap()));
            let got = hold_question_at(&path, &event, std::time::Duration::from_secs(10));
            let sent: Value = serde_json::from_str(&server.join().unwrap()).unwrap();
            assert_eq!(sent["message_type"], "question");
            assert_eq!(sent["hold"], true);
            assert_eq!(sent["pane_id"], "pane-held");
            crate::question::claude_decision(
                &q,
                payload.get("tool_input"),
                payload.get("permission_suggestions"),
                &got.expect("a reply line"),
            )
            .expect("a decision")
        };
        let allow = decision(&[(0, &[1], None)]);
        assert_eq!(
            allow,
            serde_json::json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest",
                "decision": {"behavior": "allow"}}})
        );
        let always = decision(&[(0, &[2], None)]);
        assert_eq!(
            always["hookSpecificOutput"]["decision"]["updatedPermissions"],
            serde_json::json!([payload["permission_suggestions"][0]]),
            "only the update the option stands for"
        );
        let deny = decision(&[(0, &[3], None)]);
        assert_eq!(deny["hookSpecificOutput"]["decision"]["behavior"], "deny");

        let form = fixture("claude-ask-user-question-form.json");
        let (event, q) = held_event(&form);
        let reply =
            QuestionReply::answered(&q.id, resolved(&q, &[(0, &[2], None), (1, &[1, 3], None)]));
        let (_dir, path, server) = stub_daemon(Some(serde_json::to_string(&reply).unwrap()));
        let got = hold_question_at(&path, &event, std::time::Duration::from_secs(10)).unwrap();
        server.join().unwrap();
        let decision =
            crate::question::claude_decision(&q, form.get("tool_input"), None, &got).unwrap();
        let input = &decision["hookSpecificOutput"]["decision"]["updatedInput"];
        assert_eq!(input["questions"], form["tool_input"]["questions"]);
        assert_eq!(
            input["answers"],
            serde_json::json!({"Which colour?": "Green", "Which sizes?": ["Small", "Large"]})
        );
        let typed = QuestionReply::answered(
            &q.id,
            resolved(
                &q,
                &[(0, &[4], Some("A hamster named Bob")), (1, &[2], None)],
            ),
        );
        let decision =
            crate::question::claude_decision(&q, form.get("tool_input"), None, &typed).unwrap();
        assert_eq!(
            decision["hookSpecificOutput"]["decision"]["updatedInput"]["answers"]["Which colour?"],
            "A hamster named Bob"
        );
    }

    /// Scenario: The held hook decides nothing — Claude Code then shows its own
    /// dialog as if there were no hook — when the daemon closes without a
    /// reply, releases the question, cannot be reached, or answers a different
    /// question; and a provenance refusal is recognised, so the hook re-sends
    /// the event as a plain one.
    #[cfg(unix)]
    #[spec("question/hold/002")]
    #[test]
    fn question_hold_002_held_hook_prints_nothing_without_its_own_answer() {
        let payload = fixture("claude-permission-bash.json");
        let (event, q) = held_event(&payload);
        let decide = |reply: Option<String>| {
            let (_dir, path, server) = stub_daemon(reply);
            let got = hold_question_at(&path, &event, std::time::Duration::from_secs(10));
            server.join().unwrap();
            got.and_then(|reply| {
                crate::question::claude_decision(&q, payload.get("tool_input"), None, &reply)
            })
        };
        assert_eq!(decide(None), None, "EOF");
        let released = QuestionReply::released(&q.id, ReleaseReason::Cleared);
        assert_eq!(
            decide(Some(serde_json::to_string(&released).unwrap())),
            None
        );
        let other = QuestionReply::answered("q-someone-else", resolved(&q, &[(0, &[1], None)]));
        assert_eq!(decide(Some(serde_json::to_string(&other).unwrap())), None);
        assert_eq!(decide(Some("not json".into())), None);

        let missing = tempfile::tempdir().unwrap();
        assert!(
            hold_question_at(
                &missing.path().join("absent.sock"),
                &event,
                std::time::Duration::from_secs(2)
            )
            .is_none()
        );

        assert!(QuestionReply::released(&q.id, ReleaseReason::Refused).refused());
        assert!(!released.refused());
    }
}
