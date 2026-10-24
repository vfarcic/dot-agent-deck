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
    // PRD #1631: the request id `permission.replied`, `question.replied` and
    // `question.rejected` name, so the deck clears exactly that question.
    #[serde(default, deserialize_with = "lenient_string")]
    request_id: Option<String>,
    // PRD #1631: the raw `properties` of a `permission.asked` or
    // `question.asked`, which the plugin hands to `await-answer` so the same
    // builder that makes the status event makes the question from them
    // ([`build_opencode_event_with_question`]).
    #[serde(default, deserialize_with = "lenient_object")]
    properties: Option<Value>,
    // Issue #1678: the raw id of the child session a subagent's report comes
    // from, set by the deck's plugin on `subagent.start` / `subagent.stop` and
    // on that child's tool calls (`crate::opencode_manage`).
    #[serde(default, deserialize_with = "lenient_string")]
    subagent_id: Option<String>,
    // PR #1698 review: OpenCode's `callID` for a `tool.execute.before` /
    // `tool.execute.after`, which its `permission.asked` names as
    // `tool.callID`, so a permission question clears on its own call's end.
    #[serde(default, deserialize_with = "lenient_string")]
    call_id: Option<String>,
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

/// The one parser for a hook payload (PRD #1631): `input`, the JSON `agent`'s
/// hook or plugin wrote, as the [`AgentEvent`] the deck ingests — its status,
/// tool and detail, and, where the payload raises one, the question
/// ([`AgentEvent::question`]). Every reader of a prompt reads what this
/// returns: the card's Needs Input, reading mode's permission announcement
/// (which keys on the event's type, tool and detail) and the snapshot's
/// `pending_question` all come from this one parse, never from a second parse
/// of the same payload. `None` for a payload that does not parse or maps to no
/// event.
pub(crate) fn hook_payload_event(agent: &str, input: &str) -> Option<AgentEvent> {
    match agent {
        "opencode" => {
            let hook_input: OpenCodeHookInput = serde_json::from_str(input).ok()?;
            build_opencode_event(hook_input)
        }
        // PRD #20 W1: Codex ships a Claude-Code-compatible hooks engine, so its
        // command hooks POST the SAME stdin JSON shape as Claude
        // ([`ClaudeCodeHookInput`]). We reuse the whole ingestion path, only
        // stamping [`AgentType::Codex`] and letting the Codex-aware
        // `extract_tool_detail` arms (`shell`, `apply_patch`) sharpen the detail.
        "codex" => {
            let hook_input: ClaudeCodeHookInput = serde_json::from_str(input).ok()?;
            build_event_typed(hook_input, AgentType::Codex)
        }
        // Devin CLI ships a Claude-Code-compatible hooks engine too, so its
        // command hooks POST the SAME stdin JSON shape ([`ClaudeCodeHookInput`])
        // and reuse the whole ingestion path, only stamping
        // [`AgentType::Devin`]. Its `exec` tool carries a plain-string
        // `command`, which the `extract_tool_detail` `"exec"` arm sharpens.
        "devin" => {
            let hook_input: ClaudeCodeHookInput = serde_json::from_str(input).ok()?;
            build_event_typed(hook_input, AgentType::Devin)
        }
        _ => {
            let hook_input: ClaudeCodeHookInput = serde_json::from_str(input).ok()?;
            build_event_typed(hook_input, AgentType::ClaudeCode)
        }
    }
}

pub fn handle_hook(agent: &str) -> ExitCode {
    let input = match read_stdin() {
        Some(s) if !s.is_empty() => s,
        _ => return ExitCode::SUCCESS,
    };

    let Some(mut event) = hook_payload_event(agent, &input) else {
        return ExitCode::SUCCESS;
    };

    // PRD #1666 S6: what Claude Code's own hooks do for the deck's inbox, in a
    // deck pane only — every Claude session runs these global hooks.
    let mut announce = None;
    // A test standing in for a hook from before the deck's messages does
    // nothing for the inbox at all.
    let pre_inbox = crate::inbox_oneshot::stands_in_for_pre_inbox_hooks();
    if matches!(agent, "claude-code" | "claude_code" | "claude")
        && !pre_inbox
        && crate::inbox_prompt::in_deck_pane()
        && let Some(output) = claude_inbox_hooks(&input, &mut event, &mut announce)
    {
        output.print();
    }
    // PRD #1666 S7: Codex's side. A `Stop` that hands messages over keeps the
    // turn going, so it reports no Idle: the doorbell must not ring for the
    // rest while Codex works on what it was just given.
    if agent == "codex" && !pre_inbox && crate::inbox_prompt::in_deck_pane() {
        match codex_inbox_hooks(&input, &mut event, &mut announce) {
            CodexInbox::Nothing => {}
            CodexInbox::Print(output) => output.print(),
            // PRD #1666 part 3 (H4): a hand-over whose output was written and
            // flushed whole with a block in it goes on with the turn. One the
            // fence refused whole printed nothing, and one whose write or
            // flush failed reports the turn's end too, its blocks only
            // possibly handed — although a write that stopped at the
            // trailing newline, or a flush that failed after every byte was
            // taken, can leave a whole decision Codex still reads and goes
            // on with (inbox/receipt/033): then the card reads idle while
            // Codex works, and a block it did take may come again, marked as
            // a possible repeat.
            #[cfg(unix)]
            CodexInbox::TurnEnd(injection) => {
                if injection.print().continues_turn() {
                    return ExitCode::SUCCESS;
                }
            }
        }
    }

    // PRD #1631: a Claude Code question the deck can answer through this hook
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
        return ExitCode::SUCCESS;
    }

    // PRD #1631 (audit A9): a frame that raises or clears a question goes
    // through the daemon's provenance gate as an unheld `question` message —
    // a bare event carrying either is applied as a plain status.
    if carries_question_metadata(&event) {
        send_question_unheld_at(&client_socket_path(), event, UNHELD_QUESTION_WAIT);
        return ExitCode::SUCCESS;
    }

    // PRD #1497: the turn's final reply, for the daemon's reading
    // subscribers. Read from the raw payload, so a reply of a strange shape
    // costs the reply and never the event.
    let reply = serde_json::from_str::<Value>(&input)
        .ok()
        .and_then(|payload| match agent {
            "opencode" => extract_opencode_turn_reply(&payload),
            "codex" => extract_codex_hook_turn_reply(&payload),
            _ => extract_turn_reply(&payload),
        });

    // Issue #318: present this pane's hook capability token, so the daemon can
    // tell this pane's own report from one naming it from outside.
    let token = crate::hook_provenance::token_from_env();
    let json = match crate::event::agent_event_line(&event, token.as_deref())
        .and_then(|line| with_turn_reply(line, reply.as_ref()))
    {
        Ok(j) => j,
        Err(_) => return ExitCode::SUCCESS,
    };

    // The announce goes FIRST on the same connection (design §6.5): the daemon
    // reads one connection's lines in order, so it knows a native waiter is
    // coming before the Idle this SessionStart reports could ring the doorbell.
    let _ = match announce {
        Some(announce) => send_lines_and_drain(&format!("{announce}\n{json}")),
        None => send_to_socket(&json),
    };
    // PRD #1666 part 2: a daemon that did not confirm it serves receipts or
    // did not answer in time, or a socket this user may not use, was handed
    // nothing, and the hook said so on stderr. Claude Code shows the stderr of a hook that exits
    // non-zero (other than 2) and goes on with the prompt.
    if crate::inbox_oneshot::reported_refusal() {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// PRD #1666 S6: send `lines` on one connection, half-close it, and read
/// whatever the daemon answers until it closes — the announce's `SignalAck` —
/// so the connection never ends under a reply still being written. An older
/// daemon that answers nothing closes at the half-close, so this waits on no
/// reply; the whole exchange is bounded by [`SIGNAL_ACK_TIMEOUT`].
fn send_lines_and_drain(lines: &str) -> Option<()> {
    let mut stream =
        crate::platform::ipc::IpcClient::connect_timeout(&client_socket_path(), SIGNAL_ACK_TIMEOUT)
            .ok()?;
    let _ = stream.set_timeouts(SIGNAL_ACK_TIMEOUT);
    stream.write_all(format!("{lines}\n").as_bytes()).ok()?;
    stream.flush().ok()?;
    stream.shutdown_write().ok()?;
    let deadline = std::time::Instant::now() + SIGNAL_ACK_TIMEOUT;
    let mut buf = [0u8; 1024];
    while std::time::Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    Some(())
}

/// PRD #1666 S6: Claude Code's side of the inbox, for one hook payload in a
/// deck pane. Returns what the hook prints on stdout, if anything.
///
/// - `SessionStart` prints the session framing as `additionalContext` (a
///   static text, so no daemon round trip) and, when Claude's settings carry
///   the deck's waiter ([`crate::hooks_manage::claude_waiter_installed`]),
///   queues an `inbox_announce` `Native` line for the caller to send ahead of
///   the status event. Native wake-up is enabled by this announcement, never
///   by the launch command.
/// - `UserPromptSubmit` recognises the deck's own prompts
///   ([`crate::inbox_prompt`]): a doorbell line is answered by fetching the
///   waiting content and handing it to the model, and the event is marked so
///   the daemon acknowledges what the prompt carried and labels the card.
fn claude_inbox_hooks(
    input: &str,
    event: &mut AgentEvent,
    announce: &mut Option<String>,
) -> Option<HookOutput> {
    let payload: Value = serde_json::from_str(input).ok()?;
    let hook_event_name = payload.get("hook_event_name").and_then(Value::as_str)?;
    match hook_event_name {
        "SessionStart" => {
            // Review SF-2: only a Claude whose settings carry the deck's
            // waiter will arm one, so only it announces Native.
            if crate::hooks_manage::claude_waiter_installed() {
                *announce = inbox_announce(
                    crate::event::InboxComponent::ClaudeHooks,
                    crate::event::InboxWake::Native,
                );
            }
            Some(HookOutput::text(crate::inbox_prompt::hook_output(
                "SessionStart",
                crate::inbox_hub::SESSION_FRAMING,
            )))
        }
        "UserPromptSubmit" => {
            let prompt = payload.get("prompt").and_then(Value::as_str)?;
            deck_prompt_submit(
                prompt,
                crate::event::InboxComponent::ClaudeHooks,
                crate::inbox_hub::SESSION_FRAMING,
                event,
            )
        }
        _ => None,
    }
}

/// What a hook prints for the inbox.
pub struct HookOutput {
    stdout: String,
    /// Part 2 §1.2: a doorbell's hand-over on receipts, which prints and
    /// reports its own attempts instead.
    #[cfg(unix)]
    one_shot: Option<crate::inbox_prompt::OneShotInjection>,
}

impl HookOutput {
    fn text(stdout: String) -> Self {
        Self {
            stdout,
            #[cfg(unix)]
            one_shot: None,
        }
    }

    /// Write the output. A one-shot hand-over commits what it wrote in full,
    /// on each attempt's own cap.
    fn print(self) {
        #[cfg(unix)]
        if let Some(one_shot) = self.one_shot {
            one_shot.print();
            return;
        }
        let mut out = std::io::stdout();
        let _ = writeln!(out, "{}", self.stdout).and_then(|()| out.flush());
    }
}

/// PR #1735 third review: what the deck said to one `ack`, for a component
/// that confirms its acknowledgements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckConfirmation {
    /// The deck recorded it, now or before.
    Recorded,
    /// The deck refused it for this id (`unknown`, `wrong_agent`,
    /// `undeliverable`): asking again changes nothing.
    Refused(&'static str),
    /// No answer the deck stands behind — none at all, or a refusal of the
    /// request rather than of the id: it may be asked again.
    Unconfirmed,
}

/// Classify the deck's answer to one `ack` request.
pub fn ack_confirmation(reply: &SocketReply) -> AckConfirmation {
    use crate::event::{AckDelivery, SignalAck};
    let SocketReply::Line(line) = reply else {
        return AckConfirmation::Unconfirmed;
    };
    let Some(ack) = serde_json::from_str::<SignalAck>(line)
        .ok()
        .filter(|ack| ack.is_signal_ack() && ack.accepted)
    else {
        return AckConfirmation::Unconfirmed;
    };
    match ack.delivery {
        Some(delivery) if delivery.recorded() => AckConfirmation::Recorded,
        Some(AckDelivery::Unknown) => AckConfirmation::Refused("unknown"),
        Some(AckDelivery::WrongAgent) => AckConfirmation::Refused("wrong_agent"),
        Some(AckDelivery::Undeliverable) => AckConfirmation::Refused("undeliverable"),
        _ => AckConfirmation::Unconfirmed,
    }
}

/// What the deck's Codex hook does for the inbox with one payload.
enum CodexInbox {
    Nothing,
    /// Print this, then report the event as usual.
    Print(HookOutput),
    /// PRD #1666 part 3 (H4): hand this over as the `Stop` block; a hand-over
    /// written and flushed whole goes on with the turn and reports nothing.
    #[cfg(unix)]
    TurnEnd(crate::inbox_prompt::OneShotInjection),
}

/// PRD #1666 S7: Codex's side of the inbox, for one hook payload in a deck
/// pane. Codex is woken by the doorbell when idle; its hooks run outside its
/// sandbox, so they, not the model, take the messages for it.
///
/// - `SessionStart` queues an `inbox_announce` `TurnEnd` line, sent ahead of
///   the status event.
/// - `UserPromptSubmit` answers a doorbell line with the waiting content as
///   `additionalContext` ([`deck_prompt_submit`]).
/// - `Stop` hands over what is waiting as `{"decision":"block","reason":…}`,
///   so Codex continues the turn with it. Nothing waiting, or no answer from
///   the daemon, and the turn ends as usual.
///
/// PRD #1666 part 3 (H4): both hand-overs are one-shot receipt receivers, as
/// Claude Code's prompt hook is (part 2 §1.4): each asks the deck whether it
/// serves Codex's hooks, fetches on that connection, and commits what it wrote
/// and flushed; nothing is acknowledged by id. Each is headed by
/// [`crate::inbox_hub::AGENT_FRAMING`], and its whole JSON — envelope, framing
/// and escaped blocks — stays within
/// [`crate::event::InboxComponent::CodexHooks`]'s byte budget; what does not
/// fit stays queued for the next `Stop` or doorbell. The one-shots are Unix
/// only: elsewhere Codex's hooks hand nothing over, as Claude Code's do not.
fn codex_inbox_hooks(
    input: &str,
    event: &mut AgentEvent,
    announce: &mut Option<String>,
) -> CodexInbox {
    let Ok(payload) = serde_json::from_str::<Value>(input) else {
        return CodexInbox::Nothing;
    };
    let framing = crate::inbox_hub::AGENT_FRAMING;
    match payload.get("hook_event_name").and_then(Value::as_str) {
        Some("SessionStart") => {
            *announce = inbox_announce(
                crate::event::InboxComponent::CodexHooks,
                crate::event::InboxWake::TurnEnd,
            );
            CodexInbox::Nothing
        }
        Some("UserPromptSubmit") => payload
            .get("prompt")
            .and_then(Value::as_str)
            .and_then(|prompt| {
                deck_prompt_submit(
                    prompt,
                    crate::event::InboxComponent::CodexHooks,
                    framing,
                    event,
                )
            })
            .map_or(CodexInbox::Nothing, CodexInbox::Print),
        #[cfg(unix)]
        Some("Stop") => crate::inbox_prompt::one_shot_injection(
            crate::inbox_prompt::HookHandOver {
                component: crate::event::InboxComponent::CodexHooks,
                consumer: crate::event::InboxFetchConsumer::TurnEndHook,
                rung: None,
                envelope: &crate::inbox_prompt::Envelope::Block,
                framing,
            },
            std::time::Instant::now() + crate::inbox_prompt::prompt_receipts_wait(),
        )
        .map_or(CodexInbox::Nothing, CodexInbox::TurnEnd),
        _ => CodexInbox::Nothing,
    }
}

/// PRD #1666 S6: the `UserPromptSubmit` side of the inbox, agent-neutral so
/// every agent whose hook accepts `hookSpecificOutput.additionalContext`
/// shares it (Claude Code, and Codex since S7), each with its `component`'s
/// budget and the `framing` that heads an injection. An ordinary prompt costs
/// a local scan and no daemon call. A doorbell line fetches only while the
/// message it names is queued.
///
/// Both are on receipts (part 2 §1.2, §1.3; Codex's since part 3's H4): the
/// blocks the prompt carries whole are committed by their exact attempts
/// ([`crate::inbox_prompt::commit_delivered`]), never acknowledged by id, and
/// a doorbell's hand-over is a one-shot receiver that commits what it wrote.
pub fn deck_prompt_submit(
    prompt: &str,
    component: crate::event::InboxComponent,
    framing: &str,
    event: &mut AgentEvent,
) -> Option<HookOutput> {
    let ids = crate::inbox_prompt::scan(prompt);
    if ids.is_empty() {
        return None;
    }
    // One socket-wait budget shared by the commit and the doorbell fetch
    // (what it leaves out: `prompt_receipts_wait`): the hook blocks the
    // user's prompt while it runs.
    let receipts_by = std::time::Instant::now() + crate::inbox_prompt::prompt_receipts_wait();
    crate::inbox_prompt::commit_delivered(
        prompt,
        crate::inbox_prompt::sender_of(component),
        receipts_by,
    );
    #[cfg(unix)]
    {
        let envelope = crate::inbox_prompt::Envelope::Context {
            hook_event_name: "UserPromptSubmit".to_string(),
        };
        let injection = ids.rung.first().and_then(|rung| {
            crate::inbox_prompt::one_shot_injection(
                crate::inbox_prompt::HookHandOver {
                    component,
                    consumer: crate::event::InboxFetchConsumer::DoorbellHook,
                    rung: Some(rung),
                    envelope: &envelope,
                    framing,
                },
                receipts_by,
            )
        });
        let fetched = injection
            .as_ref()
            .map(crate::inbox_prompt::OneShotInjection::ids)
            .unwrap_or_default();
        crate::inbox_prompt::mark_event(event, &ids, &fetched);
        injection.map(|injection| HookOutput {
            stdout: String::new(),
            one_shot: Some(injection),
        })
    }
    // The one-shot receivers are Unix only: no body is injected here, so a
    // doorbell does not deliver to Claude Code or Codex on Windows yet.
    #[cfg(not(unix))]
    {
        let _ = (component, framing);
        crate::inbox_prompt::mark_event(event, &ids, &[]);
        None
    }
}

/// PRD #1666 S8: send an `inbox_announce` on its own connection, for a
/// component that announces outside a hook event (OpenCode's server plugin, as
/// it loads in the full TUI). Nothing is sent without the pane's token.
pub(crate) fn send_inbox_announce(
    component: crate::event::InboxComponent,
    wake: crate::event::InboxWake,
) {
    if let Some(announce) = inbox_announce(component, wake) {
        let _ = send_lines_and_drain(&announce);
    }
}

/// The `inbox_announce` line an agent's `SessionStart` hook sends, or `None`
/// without the pane's token (the daemon would refuse it).
fn inbox_announce(
    component: crate::event::InboxComponent,
    wake: crate::event::InboxWake,
) -> Option<String> {
    let pane_id = std::env::var(DOT_AGENT_DECK_PANE_ID)
        .ok()
        .filter(|pane| !pane.is_empty())?;
    let token = crate::hook_provenance::token_from_env()?;
    let request = crate::event::DaemonMessage::InboxAnnounce(crate::event::InboxAnnounceRequest {
        pane_id,
        agent_id: std::env::var(DOT_AGENT_DECK_AGENT_ID)
            .ok()
            .filter(|agent| !agent.is_empty()),
        token: Some(token),
        component,
        component_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        wake,
        #[cfg(unix)]
        pid: Some(std::os::unix::process::parent_id()),
        #[cfg(not(unix))]
        pid: None,
    });
    serde_json::to_string(&request).ok()
}

/// PRD #1497: `line` with `reply` added under
/// [`crate::turn_reply::TURN_REPLY_LINE_KEY`], or unchanged when there is none —
/// so an event with no reply is sent exactly as before.
fn with_turn_reply(
    line: String,
    reply: Option<&crate::daemon_protocol::FinalReply>,
) -> serde_json::Result<String> {
    let Some(reply) = reply else {
        return Ok(line);
    };
    let mut value: Value = serde_json::from_str(&line)?;
    if let Value::Object(map) = &mut value {
        map.insert(
            crate::turn_reply::TURN_REPLY_LINE_KEY.to_string(),
            serde_json::to_value(reply)?,
        );
    }
    serde_json::to_string(&value)
}

/// PRD #1497: the final reply a Claude-compatible hook payload carries — the
/// `last_assistant_message` of a main-agent `Stop`, or of a `StopFailure`
/// (marked failed). `None` for every other hook and for a `Stop` fired inside
/// a subagent (one whose payload names an `agent_id`). A main-agent turn end
/// whose message is missing, not a string, or blank is still a turn end, and
/// answers an empty reply (audit A2): the turn ended with nothing to read. The
/// text is cut to [`crate::daemon_protocol::MAX_TURN_REPLY_BYTES`] at a UTF-8
/// boundary, and a `turn_id` (Codex reports one) is kept.
pub fn extract_turn_reply(payload: &Value) -> Option<crate::daemon_protocol::FinalReply> {
    let failed = match payload.get("hook_event_name").and_then(Value::as_str)? {
        "Stop" => false,
        "StopFailure" => true,
        _ => return None,
    };
    if payload
        .get("agent_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
    {
        return None;
    }
    let text = payload
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Some(crate::turn_reply::normalize(
        crate::daemon_protocol::FinalReply {
            turn_id: payload
                .get("turn_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            text: text.to_owned(),
            failed,
        },
    ))
}

/// PRD #1497: [`extract_turn_reply`] for a Codex hook payload, except that a
/// `Stop` that does not carry `last_assistant_message` at all reports nothing
/// rather than an empty reply. Codex also reports each turn in its rollout
/// (`crate::codex_rollout_tail`), and whichever report reaches the daemon
/// first is the turn's one frame (`crate::turn_reply::TurnReplyHub::publish`),
/// so a `Stop` that cannot say whether there was a reply must not claim there
/// was none. A `null` or blank message is Codex saying there was none.
pub fn extract_codex_hook_turn_reply(
    payload: &Value,
) -> Option<crate::daemon_protocol::FinalReply> {
    payload.get("last_assistant_message")?;
    extract_turn_reply(payload)
}

/// PRD #1497: the final reply the deck's OpenCode plugin attaches to the report
/// of a session going idle (`session.idle`, or `session.status` with status
/// `idle`) — the text of the session's last assistant message, under `reply`,
/// with `reply_failed` set when the turn ended on a `session.error`
/// (`crate::opencode_manage`). `None` for every other event and for a missing
/// or non-string `reply`, which the plugin leaves off an idle report that is
/// not a known main session's turn end. A blank `reply` is that turn ending
/// with nothing to read, and answers an empty reply (audit A2).
pub fn extract_opencode_turn_reply(payload: &Value) -> Option<crate::daemon_protocol::FinalReply> {
    let idle = match payload.get("event").and_then(Value::as_str)? {
        "session.idle" => true,
        "session.status" | "session.status.updated" => payload
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|s| s.eq_ignore_ascii_case("idle")),
        _ => false,
    };
    if !idle {
        return None;
    }
    let text = payload.get("reply")?.as_str()?;
    Some(crate::turn_reply::normalize(
        crate::daemon_protocol::FinalReply {
            turn_id: None,
            text: text.to_owned(),
            failed: payload
                .get("reply_failed")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
    ))
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

/// PRD #1631 (audit A9): send `event` — a question the deck answers by keys
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
        let signal =
            crate::event::DaemonMessage::Question(Box::new(crate::event::QuestionSignal {
                pane_id,
                token: crate::hook_provenance::token_from_env(),
                event: event.clone(),
                hold: false,
                ack: false,
                confirm_hold: false,
            }));
        if let Ok(json) = serde_json::to_string(&signal)
            && let (SocketReply::Line(line), _) =
                request_from_socket_at_detailed(path, &json, Some(wait))
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
    send_plain_at(path, &event);
    UnheldSend::Plain
}

/// PRD #1631: how long a held Claude Code `PermissionRequest` hook waits for
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

/// PRD #1631: send `event` as a held [`crate::event::DaemonMessage::Question`]
/// and wait up to `deadline` for the daemon's reply, falling back to a plain
/// event when the hold never starts ([`settle_hold`]). `None` when no
/// [`crate::question::QuestionReply`] came back. That is a missing line — no
/// daemon, a closed connection, the deadline — or a line that arrived but does
/// not parse as a reply. Both are handled alike; what follows depends on when.
/// In place of the daemon's first line, the hold never started and the plain
/// event is sent. After the daemon's `held` line, the hold ended without a
/// reply and nothing more is sent, since the question is already on the card.
/// A caller acts on the reply only through
/// [`crate::question::QuestionReply::answers_for`], which also refuses a reply
/// naming another question.
fn hold_question(
    event: &AgentEvent,
    deadline: std::time::Duration,
) -> Option<crate::question::QuestionReply> {
    hold_question_at(&client_socket_path(), event, deadline)
}

/// PRD #1631 (PR #1698 review): how long a holding producer waits for the
/// daemon to say it holds the question ([`crate::question::ReplyOutcome::Held`])
/// before deciding it never will. A daemon that knows `question` says so as
/// soon as it has applied the event; one from before `question` existed
/// (0.46.0 and earlier) logs the line as a malformed event and answers
/// nothing, keeping the connection open until its own 60 s idle bound. Past
/// this the producer shows the prompt as a plain event, so the card reads
/// Needs Input within seconds instead of never.
pub const HOLD_START_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// How a held `question` message ended, as its producer saw it.
#[derive(Debug)]
enum HoldEnd {
    /// The daemon never took the hold: none could be reached, or it said
    /// neither "held" nor anything else it could have meant within
    /// [`HOLD_START_WAIT`] — an older daemon that does not know `question`,
    /// a closed connection, a line that is not a reply. Nothing it carried
    /// was applied.
    Unstarted,
    /// The daemon's reply: an answer, a release, or a refusal — after a
    /// `held` line, or in place of one when there was no hold to start.
    Reply(crate::question::QuestionReply),
    /// The daemon held the question, and then the connection closed, the
    /// deadline passed, or a line arrived that is not a reply.
    Ended,
}

/// PRD #1631: what a producer does once its hold is over, and the reply it
/// acts on. A hold that never started, or one the provenance gate refused,
/// applied nothing: the event goes again as a plain one, without the
/// question, so the card still reads Needs Input exactly as it would have
/// before the deck could answer questions. A hold the daemon took already
/// showed the question, and is left as it ended.
fn settle_hold(
    path: &std::path::Path,
    event: &AgentEvent,
    end: HoldEnd,
) -> Option<crate::question::QuestionReply> {
    let applied_nothing = match &end {
        HoldEnd::Unstarted => true,
        HoldEnd::Reply(reply) => reply.refused(),
        HoldEnd::Ended => false,
    };
    if applied_nothing {
        let mut event = event.clone();
        event.metadata.remove(crate::event::QUESTION_METADATA_KEY);
        send_plain_at(path, &event);
    }
    match end {
        HoldEnd::Reply(reply) => Some(reply),
        HoldEnd::Unstarted | HoldEnd::Ended => None,
    }
}

/// A question's fallback: `event` as the plain status line every other hook
/// event is, this pane's hook token included (issue #318), so the daemon's
/// provenance gate treats it as this pane's own report.
fn send_plain_at(path: &std::path::Path, event: &AgentEvent) {
    let token = crate::hook_provenance::token_from_env();
    if let Ok(json) = crate::event::agent_event_line(event, token.as_deref()) {
        let _ = send_to_socket_at(path, &json);
    }
}

/// [`hold_question`] against an explicit endpoint.
fn hold_question_at(
    path: &std::path::Path,
    event: &AgentEvent,
    deadline: std::time::Duration,
) -> Option<crate::question::QuestionReply> {
    hold_question_within_at(path, event, deadline, HOLD_START_WAIT)
}

/// [`hold_question_at`], giving the daemon `start_wait` to say it holds the
/// question.
fn hold_question_within_at(
    path: &std::path::Path,
    event: &AgentEvent,
    deadline: std::time::Duration,
    start_wait: std::time::Duration,
) -> Option<crate::question::QuestionReply> {
    let (end, _) = start_hold_at(path, event, deadline, start_wait, false);
    settle_hold(path, event, end)
}

/// PRD #1631: send `event` as a held [`crate::event::DaemonMessage::Question`]
/// that asks the daemon to confirm the hold, and wait for how it ends: up to
/// `start_wait` for the daemon's first line, then, once it says `held`, up to
/// `deadline` (counted from the start) for the reply. The connection comes
/// back with a reply, so a producer that acknowledges (`ack`) can write its
/// acknowledgement on it.
fn start_hold_at(
    path: &std::path::Path,
    event: &AgentEvent,
    deadline: std::time::Duration,
    start_wait: std::time::Duration,
    ack: bool,
) -> (HoldEnd, Option<crate::platform::ipc::IpcClient>) {
    use crate::question::{QuestionReply, ReplyOutcome};
    let Some(pane_id) = event.pane_id.clone() else {
        return (HoldEnd::Unstarted, None);
    };
    let signal = crate::event::DaemonMessage::Question(Box::new(crate::event::QuestionSignal {
        pane_id,
        token: crate::hook_provenance::token_from_env(),
        event: event.clone(),
        hold: true,
        ack,
        confirm_hold: true,
    }));
    let Ok(json) = serde_json::to_string(&signal) else {
        return (HoldEnd::Unstarted, None);
    };
    let started = std::time::Instant::now();
    let until = started + deadline;
    let start_by = until.min(started + start_wait);
    let Ok(mut stream) =
        crate::platform::ipc::IpcClient::connect_timeout(path, start_wait.min(deadline))
    else {
        return (HoldEnd::Unstarted, None);
    };
    // No half-close after the write: the daemon reads a held connection for
    // EOF to learn that the producer stopped waiting (Claude Code kills the
    // hook when the keyboard answers No [observed]), so a half-close would
    // read as exactly that the moment the request landed.
    if stream.set_timeouts(deadline).is_err()
        || !matches!(
            write_request_line(&mut stream, format!("{json}\n").as_bytes()),
            RequestWrite::Written
        )
    {
        return (HoldEnd::Unstarted, None);
    }
    let mut carry = Vec::new();
    let mut next_reply = |stream: &mut crate::platform::ipc::IpcClient, by| {
        read_reply_line_carrying(stream, Some(by), &mut carry)
            .ok()
            .and_then(|line| serde_json::from_str::<QuestionReply>(&line).ok())
    };
    match next_reply(&mut stream, start_by) {
        Some(reply) if reply.outcome == ReplyOutcome::Held => {}
        Some(reply) => return (HoldEnd::Reply(reply), Some(stream)),
        None => return (HoldEnd::Unstarted, None),
    }
    match next_reply(&mut stream, until) {
        Some(reply) => (HoldEnd::Reply(reply), Some(stream)),
        None => (HoldEnd::Ended, None),
    }
}

/// PRD #1631: the `await-answer` verb, for producers that are not a hook
/// process of their own — the OpenCode plugin and the Pi extension. Builds the
/// question from what the producer passed (`stdin` for OpenCode, `--question`
/// for Pi, whose exec helper has no stdin), holds it with the daemon, and
/// prints ONE line: what the producer should do with the answer
/// ([`crate::question::opencode_reply`], [`crate::question::pi_value`]).
/// Prints nothing — the producer then leaves the agent's own prompt alone — on
/// every other outcome, and always exits 0.
///
/// With `ack` (the OpenCode plugin) the question is the FIRST line of stdin,
/// and the answer is acknowledged: see [`opencode_await_answer_acked_at`].
pub fn handle_await_answer(agent: &str, question_arg: Option<&str>, ack: bool) -> ExitCode {
    if ack && agent == "opencode" && question_arg.is_none() {
        let mut first = String::new();
        if std::io::stdin().read_line(&mut first).is_ok() && !first.trim().is_empty() {
            opencode_await_answer_acked_at(
                &client_socket_path(),
                first.trim(),
                &mut std::io::stdout(),
                || read_stdin_line_within(AWAIT_ANSWER_ACK_WAIT),
            );
        }
        return ExitCode::SUCCESS;
    }
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

/// The OpenCode `await-answer` event and question for `raw` — the plugin's
/// description of a question: the event's type and its raw `properties`, plus
/// the session the plugin normalised — or `None` when it describes neither a
/// permission nor a question.
///
/// PRD #1631: built by [`build_opencode_event_with_question`], the one parser
/// for OpenCode's events, so the status event every client reads (reading
/// mode's permission announcement included) and the question the snapshot
/// carries come from the same parse of the same payload.
fn opencode_question_event(
    raw: &str,
) -> Option<(AgentEvent, crate::question::PendingQuestion, Value)> {
    let input: OpenCodeHookInput = serde_json::from_str(raw).ok()?;
    if !matches!(input.event.as_str(), "permission.asked" | "question.asked") {
        return None;
    }
    let properties = input.properties.clone()?;
    let (event, question) = build_opencode_event_with_question(input)?;
    Some((event, question?, properties))
}

fn opencode_await_answer(raw: &str) -> Option<Value> {
    let (event, question, properties) = opencode_question_event(raw)?;
    let reply = hold_question(&event, AWAIT_ANSWER_DEADLINE);
    crate::question::opencode_reply(&question, &properties, &reply?)
}

/// PRD #1631: how long an acknowledging `await-answer` child waits for the
/// OpenCode plugin to say what OpenCode's reply API made of the answer —
/// shorter than the daemon's [`crate::daemon::HELD_REPLY_ACK_WAIT`], so the
/// daemon hears the plugin's report rather than timing out first.
pub const AWAIT_ANSWER_ACK_WAIT: std::time::Duration = std::time::Duration::from_secs(4);

/// One line of stdin, if one arrives within `wait`. The read runs on a thread
/// of its own, left behind when the wait ends: the process exits right after.
fn read_stdin_line_within(wait: std::time::Duration) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let read = std::io::stdin().read_line(&mut line);
        let _ = tx.send(read.ok().filter(|n| *n > 0).map(|_| line));
    });
    rx.recv_timeout(wait).ok().flatten()
}

/// PRD #1631: the OpenCode plugin's `await-answer --ack`. Holds the question
/// like [`opencode_await_answer`] and prints the reply OpenCode should get;
/// then reads ONE line — `ack_line`, the plugin's
/// [`crate::question::ReplyAck`] for what OpenCode's reply API said — and
/// passes it to the daemon on the held connection, which reports the deck's
/// answer only once it has it. An answer that cannot be turned into OpenCode's
/// reply is acknowledged as not taken, and nothing is printed. A missing or
/// unreadable acknowledgement is passed on as nothing, which the daemon
/// reports as unconfirmed.
fn opencode_await_answer_acked_at(
    path: &std::path::Path,
    raw: &str,
    out: &mut dyn std::io::Write,
    ack_line: impl FnOnce() -> Option<String>,
) {
    if let Some((event, question, properties)) = opencode_question_event(raw) {
        opencode_hold_acked_at(path, event, &question, &properties, out, ack_line);
    }
}

/// [`opencode_await_answer_acked_at`] once the question is built.
fn opencode_hold_acked_at(
    path: &std::path::Path,
    event: AgentEvent,
    question: &crate::question::PendingQuestion,
    properties: &Value,
    out: &mut dyn std::io::Write,
    ack_line: impl FnOnce() -> Option<String>,
) {
    let (end, stream) = start_hold_at(path, &event, AWAIT_ANSWER_DEADLINE, HOLD_START_WAIT, true);
    let (Some(reply), Some(mut stream)) = (settle_hold(path, &event, end), stream) else {
        return;
    };
    if reply.answers_for(&question.id).is_none() {
        return;
    }
    let ack = match crate::question::opencode_reply(question, properties, &reply) {
        Some(line) => {
            let printed = writeln!(out, "{line}").and_then(|()| out.flush()).is_ok();
            printed.then(ack_line).flatten().and_then(|line| {
                serde_json::from_str::<crate::question::ReplyAck>(line.trim()).ok()
            })
        }
        None => Some(crate::question::ReplyAck {
            delivered: false,
            detail: Some("the answer could not be turned into OpenCode's reply".to_string()),
        }),
    };
    if let Some(json) = ack.and_then(|ack| serde_json::to_string(&ack).ok()) {
        let _ = write_request_line(&mut stream, format!("{json}\n").as_bytes());
    }
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
        background: None,
        background_revision: None,
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
        // PRD #1631: Codex fires `Interrupt` when a turn is interrupted — after
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
    crate::submitted_prompt::truncate_on_char_boundary(s, max)
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
/// * the [`crate::submitted_prompt::USER_PROMPT_MAX_LEN`] budget below is spent
///   on the prompt rather than partly on a wrapper.
///
/// **This does not replace [`crate::submitted_prompt`]'s envelope shape and must
/// not be read as making it unreachable.** The `dot-agent-deck` invoked inside
/// a pane comes from the agent's own hook configuration and can be OLDER than
/// the daemon it reports to — `crate::hook_provenance`'s `missing_token`
/// refusal exists for exactly that population — so a daemon still receives
/// enveloped reports from binaries that predate this, and the matcher still has
/// to read them.
///
/// Unwrapping is refused for anything but a turn that is wholly one paste (see
/// [`crate::submitted_prompt::paste_envelope_payload`]): a turn where someone
/// typed prose AROUND a paste is reported as they composed it.
fn record_submitted_prompt(prompt: &str) -> String {
    // The closing delimiter is stripped ONLY when an opening one was found and
    // accepted. Applying it unconditionally would cut a trailing
    // `</pasted_content …>` off a turn this deliberately declined to unwrap —
    // pinned by `a_turn_that_merely_contains_a_paste_is_recorded_unchanged`,
    // which is what caught it.
    let recorded = match crate::submitted_prompt::paste_envelope_payload(prompt) {
        Some((payload, _)) => crate::submitted_prompt::strip_paste_envelope_close(payload),
        None => prompt,
    };
    truncate(recorded, crate::submitted_prompt::USER_PROMPT_MAX_LEN)
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
    //
    // Issue #1678: an EMPTY id is still a subagent's — the key's presence is
    // the attribution, and the main thread sends no key at all. Dropping it
    // turned such a call into a main-thread one that set Working on a card
    // whose turn was over. The background-work tracker refuses to key on an
    // empty or oversized id; the event keeps its attribution either way.
    let subagent_id =
        subagent_id.filter(|_| matches!(agent_type, AgentType::ClaudeCode | AgentType::Codex));
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
    // PR #1698 review: a `PermissionRequest` names no call, so the state ties
    // its question to the `PreToolUse` with the same input
    // ([`crate::event::TOOL_INPUT_DIGEST_METADATA_KEY`]).
    if matches!(
        event_type,
        EventType::ToolStart | EventType::PermissionRequest
    ) && let Some(input) = &tool_input
    {
        metadata.insert(
            crate::event::TOOL_INPUT_DIGEST_METADATA_KEY.to_string(),
            crate::event::tool_input_digest(input),
        );
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
    // from an initialized session: its boot-time start read as the agent's
    // conversation announcing itself.
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
    // to believe from anyone; an interface origin is a producer CLAIMING that its
    // child is up, which is the kind of claim a privilege gets keyed on (today
    // it only lets a wrapped agent's `Idle` stand in for its own hooks' until
    // they report). So this CLI does not carry it, and should not be taught to.
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
    // it. A privilege keyed on an interface origin has to be gated where it is
    // USED, by asking whether this daemon spawned the named agent as a wrapper
    // — see `crate::event::WRAPPER_INTERFACE_READY_SESSION_START_ORIGIN`.
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

    // PRD #1631: the question this event raises, built from the payload and
    // the agent's option table.
    let question = hook_question(
        &agent_type,
        &hook_event_name,
        tool_name.as_deref(),
        tool_input.as_ref(),
        tool_detail.clone(),
        metadata.get("tool_use_id").map(String::as_str),
        extra.get("permission_suggestions"),
        extra.get("permission_mode").and_then(Value::as_str),
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
        background: None,
        background_revision: None,
    };
    if let Some(mut question) = question {
        question.subagent_id = subagent_id_for_question;
        event.set_question(&question);
    }
    Some(event)
}

/// PRD #1631: the question a Claude-shaped hook payload raises, if any.
///
/// - Claude Code `PermissionRequest` → [`crate::question::claude_permission_request`].
/// - Codex `PermissionRequest` → [`crate::question::codex_permission_request`];
///   Codex `PreToolUse` of `request_user_input` →
///   [`crate::question::codex_request_user_input`].
/// - Devin `PermissionRequest` → [`crate::question::devin_permission_request`].
///
/// The agent's version is not in any of these payloads, so the tables are used
/// as verified (`None`).
#[allow(clippy::too_many_arguments)]
fn hook_question(
    agent_type: &AgentType,
    hook_event_name: &str,
    tool_name: Option<&str>,
    tool_input: Option<&Value>,
    tool_detail: Option<String>,
    tool_use_id: Option<&str>,
    permission_suggestions: Option<&Value>,
    permission_mode: Option<&str>,
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
                permission_mode,
                now,
                None,
            )
            .question,
        ),
        (AgentType::Codex, "PermissionRequest") => {
            // The whole command, for the "don't ask again" confirmation:
            // `tool_detail` is its first line, cut at 120 bytes.
            let command = tool_input
                .and_then(|input| input.get("command"))
                .and_then(codex_shell_command);
            Some(crate::question::codex_permission_request(
                id(),
                tool_name?,
                tool_detail,
                command.as_deref(),
                now,
                None,
            ))
        }
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
        // PRD #1631: OpenCode's `question` tool. The plugin forwards the ask
        // through `await-answer`, which builds the event itself; this arm is
        // for a plugin that forwards it as a plain event.
        "question.asked" => Some(EventType::WaitingForInput),
        "question.replied" | "question.rejected" => Some(EventType::Thinking),
        "subagent.start" => Some(EventType::SubagentStart),
        "subagent.stop" => Some(EventType::SubagentStop),
        _ => None,
    }
}

pub(crate) fn build_opencode_event(input: OpenCodeHookInput) -> Option<AgentEvent> {
    build_opencode_event_with_question(input).map(|(event, _)| event)
}

/// PRD #1631: the question an OpenCode event raises, from its raw
/// `properties` — a `permission.asked` (`crate::question::opencode_permission_asked`)
/// or a `question.asked` (`crate::question::opencode_question_asked`). `None`
/// for every other event, and for one that arrived without `properties` (an
/// older plugin, which forwards the status only).
fn opencode_question(
    event: &str,
    properties: Option<&Value>,
    raised_at_ms: i64,
) -> Option<crate::question::PendingQuestion> {
    let properties = properties?;
    match event {
        "permission.asked" => crate::question::opencode_permission_asked(properties, raised_at_ms),
        "question.asked" => crate::question::opencode_question_asked(properties, raised_at_ms),
        _ => None,
    }
}

/// [`build_opencode_event`], plus the question the event raises (PRD #1631):
/// ONE parse of the payload yields both the status event — `permission.asked`
/// is `PermissionRequest`, which reading mode announces — and the
/// `pending_question` it carries ([`crate::event::AgentEvent::set_question`]).
/// A permission's tool fills the event's `tool_name` / `tool_detail` when the
/// payload named none, so what reading mode says the agent wants is what the
/// question asks.
pub(crate) fn build_opencode_event_with_question(
    input: OpenCodeHookInput,
) -> Option<(AgentEvent, Option<crate::question::PendingQuestion>)> {
    // One parse is one instant: the question's `raised_at_ms`, the permission's
    // `tool_use_id`, the quota reset's reference point and the event's
    // `timestamp` all read this one clock reading.
    let now = Utc::now();
    let now_ms = now.timestamp_millis();
    let mut question = opencode_question(&input.event, input.properties.as_ref(), now_ms);
    let mut event_type = map_opencode_event_type(&input.event, input.status.as_deref())?;
    let tool_detail = extract_tool_detail(input.tool_name.as_deref(), input.tool_input.as_ref());
    let user_prompt = input.prompt.as_deref().map(record_submitted_prompt);
    let pane_id = std::env::var(DOT_AGENT_DECK_PANE_ID).ok();
    let agent_id = std::env::var(DOT_AGENT_DECK_AGENT_ID).ok();

    let mut metadata = HashMap::new();
    // An answered prompt: a permission, or a question form answered or
    // dismissed. Read only on a subagent's report, below.
    if matches!(
        input.event.as_str(),
        "permission.replied" | "question.replied" | "question.rejected"
    ) {
        metadata.insert(
            crate::event::PERMISSION_STATE_METADATA_KEY.to_string(),
            crate::event::PERMISSION_STATE_REPLIED.to_string(),
        );
    }
    if matches!(event_type, EventType::PermissionRequest) {
        metadata.insert(
            crate::event::PERMISSION_STATE_METADATA_KEY.to_string(),
            "pending".to_string(),
        );
        metadata.insert(
            "tool_use_id".to_string(),
            format!("perm-{}-{}", input.session_id, now_ms),
        );
    }

    // PRD #1631: OpenCode reports the question answered, by its request id.
    // A reply whose id is absent or malformed clears nothing — never an
    // arbitrary question — so that question stays until its deadline or a
    // superseding frame (`docs/develop/agent-questions.md`).
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

    // Issue #1678: a child session's report is its subagent's (issue #1354),
    // so it does not move the card's status and the daemon counts the child as
    // background work after the turn ends. Only on the events the plugin tags:
    // a child's lifecycle, tool calls, permission prompts and (PRD #1631)
    // question forms. A child's answered prompt arrives as a `Thinking` marked
    // replied, which ends that subagent's wait and nothing else
    // (`AgentEvent::is_subagent_permission_reply`).
    if let Some(subagent_id) = input.subagent_id.filter(|_| {
        matches!(
            input.event.as_str(),
            "subagent.start"
                | "subagent.stop"
                | "tool.execute.before"
                | "tool.execute.after"
                | "permission.asked"
                | "permission.replied"
                | "question.asked"
                | "question.replied"
                | "question.rejected"
        )
    }) {
        metadata.insert(
            crate::event::SUBAGENT_ID_METADATA_KEY.to_string(),
            subagent_id,
        );
    }

    // PR #1698 review: the call a tool event is about, so a permission question
    // tied to it ([`crate::question::opencode_permission_asked`]) clears on
    // that call's end and not on a parallel one's.
    if matches!(
        input.event.as_str(),
        "tool.execute.before" | "tool.execute.after"
    ) && let Some(call_id) = input.call_id
    {
        metadata.insert("tool_use_id".to_string(), call_id);
    }

    // Store the full bash command (tool_detail truncates).
    if matches!(event_type, EventType::ToolStart)
        && input.tool_name.as_deref() == Some("Bash")
        && let Some(ref tool_input) = input.tool_input
        && let Some(cmd) = tool_input.get("command").and_then(|v| v.as_str())
    {
        metadata.insert("bash_command".to_string(), cmd.to_string());
    }

    // OpenCode publishes the user interrupting a turn as a `session.error`
    // named `MessageAbortedError` — its own TUI skips exactly that name rather
    // than showing an error — so it is the turn ending, not a failure. That
    // matters since an Error card stays Error through the `session.idle` that
    // follows it (`AppState::apply_event`).
    if input.event == "session.error" && input.error_name.as_deref() == Some("MessageAbortedError")
    {
        event_type = EventType::Idle;
    }
    // Issue #714: a `session.error` whose structured fields name a provider
    // quota or credit refusal is a block; every other one stays `Error`. See
    // `crate::quota_signals::classify_opencode_error`.
    if input.event == "session.error" && event_type == EventType::Error {
        let fields = crate::quota_signals::OpenCodeErrorFields {
            error_name: input.error_name,
            response_markers: input.response_markers,
            response_headers: input.response_headers,
        };
        if let crate::quota_signals::FailureOutcome::Blocked { kind, resets_at_ms } =
            crate::quota_signals::classify_opencode_error(&fields, now_ms)
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

    let mut event = AgentEvent {
        session_id: input.session_id,
        agent_type: AgentType::OpenCode,
        event_type,
        tool_name: input.tool_name,
        tool_detail,
        cwd: input.cwd,
        timestamp: now,
        user_prompt,
        metadata,
        pane_id,
        agent_id,
        agent_version: None,
        schema_version: None,
        live_target: None,
        background: None,
        background_revision: None,
    };
    if let Some(question) = &mut question {
        // PRD #1631: a subagent's prompt (issue #1678) is that subagent's
        // question, as a Claude Code subagent's is.
        question.subagent_id = event
            .metadata
            .get(crate::event::SUBAGENT_ID_METADATA_KEY)
            .cloned();
        if event.tool_name.is_none()
            && let Some(tool) = &question.tool
        {
            event.tool_name = Some(tool.name.clone());
            event.tool_detail = tool.detail.clone();
        }
        event.set_question(question);
    }
    Some((event, question))
}

/// The optional card detail `dot-agent-deck agent-event` carries beside its
/// `--type` (issue #622). Each is what an extension already has in hand when it
/// reports: the session's directory, the prompt it is about to run, the tool it
/// is starting or finishing and a short description of that call.
#[derive(Debug, Default, Clone)]
pub struct AgentEventDetail {
    pub cwd: Option<String>,
    pub prompt: Option<String>,
    pub tool_name: Option<String>,
    pub tool_detail: Option<String>,
}

/// Build the raw [`AgentEvent`] `dot-agent-deck agent-event` sends for a pane
/// (PRD #201 M1.2; detail since issue #622).
///
/// The event is keyed on `<pane_id>-session` so repeated reports update one
/// card, and typed [`AgentType::Pi`] — safe because `apply_event` only upgrades
/// `None` to a concrete type and never overwrites a known one.
///
/// The detail is bounded the way the hook builders above bound theirs, because
/// it arrives on argv from a producer rather than from the deck: a blank value
/// is dropped rather than sent, the prompt goes through
/// [`record_submitted_prompt`] exactly as a Claude or OpenCode prompt does, and
/// the tool detail keeps only its first line, cut to the same 120 bytes as
/// [`extract_tool_detail`]'s shell arms. The daemon scrubs both tool strings on
/// ingest regardless.
pub fn build_agent_event_cli(
    pane_id: String,
    agent_id: Option<String>,
    event_type: EventType,
    detail: AgentEventDetail,
) -> AgentEvent {
    let non_blank = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
    let tool_detail = non_blank(detail.tool_detail)
        .map(|d| truncate(d.lines().next().unwrap_or(&d), 120))
        .filter(|d| !d.trim().is_empty());
    let metadata = std::collections::HashMap::new();
    let prompt = non_blank(detail.prompt);
    let mut event = AgentEvent {
        session_id: format!("{pane_id}-session"),
        agent_type: AgentType::Pi,
        event_type,
        tool_name: non_blank(detail.tool_name).map(|n| truncate(&n, 80)),
        tool_detail,
        cwd: non_blank(detail.cwd),
        timestamp: Utc::now(),
        user_prompt: prompt.as_deref().map(record_submitted_prompt),
        metadata,
        pane_id: Some(pane_id),
        agent_id,
        agent_version: None,
        schema_version: None,
        live_target: None,
        background: None,
        background_revision: None,
    };
    // PRD #1666 (pi_001): the Pi extension hands a deck message over as a
    // prompt of its own — the framing, then the blocks. It is the deck's, not
    // the user's: the daemon labels the card `✉ <subject>` from the ids named
    // here and keeps the text out of the prompt history. The extension
    // acknowledges the message itself when Pi commits it, so every id counts as
    // handed over and none is acknowledged again here.
    if let Some(prompt) = prompt.as_deref() {
        let ids = crate::inbox_prompt::scan(prompt);
        if !ids.is_empty() {
            let handed_over: Vec<String> = ids.opened.clone();
            crate::inbox_prompt::mark_event(&mut event, &ids, &handed_over);
        }
    }
    event
}

/// How long `dot-agent-deck agent-event --turn-reply-stdin` waits for its
/// stdin to end. The bundled Pi extension writes the reply and closes stdin
/// as it spawns the CLI, so the read ends at once; the bound is for a stdin
/// nobody closes (a terminal, a held-open pipe), which costs the report its
/// reply and never the report.
pub const TURN_REPLY_STDIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// PRD #1497: the reply `dot-agent-deck agent-event --turn-reply-stdin` reads
/// from `input`: at most [`crate::daemon_protocol::MAX_TURN_REPLY_BYTES`] plus
/// the three bytes that complete a character straddling the bound — the
/// daemon keeps no more ([`crate::turn_reply::normalize`] cuts it there) — so
/// a longer input is never read in full. A character the read cut short is
/// dropped; any other invalid UTF-8 becomes U+FFFD. `None` when `input` gives
/// nothing, fails, or has not ended within `timeout`: the reader runs on a
/// thread of its own, so a stdin nobody closes cannot hold the report.
///
/// What follows the bound is read and discarded until `input` ends, still
/// within `timeout`, so a writer blocked on a full pipe is released instead of
/// waiting on a reader that stopped reading (PR #1617's macOS hang). A writer
/// that has not finished by then costs the wait, never the reply.
pub fn read_turn_reply_stdin<R>(input: R, timeout: std::time::Duration) -> Option<String>
where
    R: std::io::Read + Send + 'static,
{
    let deadline = std::time::Instant::now() + timeout;
    let limit = (crate::daemon_protocol::MAX_TURN_REPLY_BYTES + 3) as u64;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let (drained_tx, drained) = std::sync::mpsc::sync_channel::<()>(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut input = input.take(limit);
        let read = input.read_to_end(&mut bytes).map(|_| bytes);
        let full = matches!(&read, Ok(bytes) if bytes.len() as u64 == limit);
        let _ = tx.send(read);
        if full {
            let _ = std::io::copy(&mut input.into_inner(), &mut std::io::sink());
        }
        let _ = drained_tx.send(());
    });
    let bytes = rx.recv_timeout(timeout).ok()?.ok()?;
    let _ = drained.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) if err.utf8_error().error_len().is_none() => {
            let valid = err.utf8_error().valid_up_to();
            let mut bytes = err.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).ok()?
        }
        Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
    };
    (!text.is_empty()).then_some(text)
}

/// PRD #1497: the final reply `dot-agent-deck agent-event --turn-reply-stdin`
/// carries — the bundled Pi extension's last assistant text for a settled turn —
/// bounded like every other route ([`crate::turn_reply::normalize`]). `text` is
/// `None` when the report did not ask for a reply (no `--turn-reply-stdin`), and
/// what stdin gave otherwise, empty included. Only a turn end (`--type
/// finished`, an [`EventType::Idle`]) carries one; on any other `--type`, and
/// without the flag, it is `None`, so the line is sent exactly as before. A
/// turn end whose reply is blank answers an empty reply: the turn ended with
/// nothing to read (audit A2).
pub fn agent_event_cli_turn_reply(
    event_type: &EventType,
    text: Option<String>,
    failed: bool,
) -> Option<crate::daemon_protocol::FinalReply> {
    if *event_type != EventType::Idle {
        return None;
    }
    Some(crate::turn_reply::normalize(
        crate::daemon_protocol::FinalReply {
            turn_id: None,
            text: text?,
            failed,
        },
    ))
}

/// The hook-socket line `dot-agent-deck agent-event` sends: `event` with this
/// pane's token ([`crate::event::agent_event_line`]) and, beside it, the
/// settled turn's `reply` under [`crate::turn_reply::TURN_REPLY_LINE_KEY`] —
/// the shape a Claude-compatible hook's line takes, so the daemon reads both
/// the same way.
pub fn agent_event_cli_line(
    event: &AgentEvent,
    token: Option<&str>,
    reply: Option<&crate::daemon_protocol::FinalReply>,
) -> serde_json::Result<String> {
    crate::event::agent_event_line(event, token).and_then(|line| with_turn_reply(line, reply))
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
/// respawns, and `dispatch_one_owned` holds that same per-pane lock while it
/// hands a delegated task over — including a `clear = true` respawn of the
/// worker. So a `pane restart <role>` issued while such a delegate to that same
/// role is mid-flight waits behind the lock, can blow this 14s budget, and
/// surfaces to the caller as [`SocketReply::NoReply`] — a reported failure for
/// a restart the daemon is still holding and will carry out (or refuse, per the
/// post-lock crash recheck) once the lock frees. The lock has no cap on queue
/// depth, so queued same-pane delegates compound, which is why no compile-time
/// constant here can honestly bound it.
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
/// The same 5s [`DELEGATE_REPLY_TIMEOUT`] gives `delegate`. For `dispatch` the
/// reply is cheaper to produce: the daemon writes it at the provenance gate,
/// **before** the handler runs (`DaemonMessage::provenance_ack_reply`), so it
/// costs one registry lookup and a socket write. Since PRD #1631 `work-done`'s
/// is written once its report is queued, after `prepare_work_done` has run
/// under the `state` read lock — the same lock `delegate`'s reply waits behind,
/// and the one the 5s exists to cover — or behind a full delivery queue
/// (`crate::pane_delivery_queue::MAX_PENDING_PANE_DELIVERIES`). The margin is
/// spent only when the daemon cannot answer promptly — wedged, overloaded, or
/// with a full accept queue — which is exactly when a *smaller* budget would
/// convert a slow-but-delivered signal into a reported failure. A reply that
/// misses it is [`SocketReply::NoReply`], which the CLI reports as handed to
/// the daemon rather than as a failure.
///
/// So `work-done`'s ordering holds only for an ack the CLI received within
/// these 5s: after a `NoReply` the report may not be queued yet, and a report
/// the same worker sends next can be queued ahead of it.
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
/// PRD #1666 part 3, round 5 (plan D1): what [`send_gated`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gated {
    /// The daemon did not confirm the verb, so the mutation was withheld:
    /// nothing was sent. `refusal` is the daemon's own `(code, message)`
    /// when it refused the question; `why` names the rest, for the log.
    NotConfirmed {
        refusal: Option<(String, String)>,
        why: &'static str,
    },
    /// The mutation was attempted and no well-formed answer came back: it
    /// may or may not have been taken. `why` names the case, for the log.
    Unconfirmed(&'static str),
    /// The daemon's answer to the mutation, one line.
    Reply(String),
}

/// How long [`send_gated`] waits, in all, to connect, ask its question and
/// read the whole answer; the one write under way when it ends may run past
/// it ([`write_gated`]).
pub const GATED_QUESTION_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
/// How long [`send_gated`] waits, in all, to write its mutation and read the
/// whole answer: the daemon answers once the report is kept, after a bounded
/// wait on any earlier report of the same task. The one write under way when
/// it ends may run past it ([`write_gated`]).
pub const GATED_REPLY_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

/// The longest line [`send_gated`] reads.
const GATED_MAX_LINE: usize = 64 * 1024;

/// PRD #1666 part 3, round 5 (plan D1): ask the daemon, on one connection,
/// whether it serves a verb (`question`), and send `mutation` on the same
/// connection only when `confirms` accepts the answer. It never reconnects,
/// and never falls back to an older verb. Once the mutation is attempted,
/// every outcome but one well-formed line is [`Gated::Unconfirmed`]: a write
/// error or partial write, the connection closing, the deadline.
pub fn send_gated(
    question: &str,
    mutation: &str,
    confirms: impl Fn(&str) -> Result<(), Option<(String, String)>>,
) -> Gated {
    send_gated_at(
        &client_socket_path(),
        question,
        mutation,
        confirms,
        GATED_QUESTION_WAIT,
        GATED_REPLY_WAIT,
    )
}

/// [`send_gated`] against an explicit endpoint and deadlines.
pub fn send_gated_at(
    path: &std::path::Path,
    question: &str,
    mutation: &str,
    confirms: impl Fn(&str) -> Result<(), Option<(String, String)>>,
    question_wait: std::time::Duration,
    reply_wait: std::time::Duration,
) -> Gated {
    let not_confirmed = |why| Gated::NotConfirmed { refusal: None, why };
    // Each wait is a deadline for its whole exchange, not a timeout per read
    // or write: it is checked before every read and before every write of at
    // most `GATED_WRITE_CHUNK` bytes, so a peer that sends or takes a byte at
    // a time stretches the exchange past it by no more than the one write
    // under way when it passes (`write_gated`).
    let question_deadline = std::time::Instant::now() + question_wait;
    let Ok(mut client) = crate::platform::ipc::IpcClient::connect_timeout(path, question_wait)
    else {
        return not_confirmed("no daemon answered");
    };
    if write_gated(
        &mut client,
        format!("{question}\n").as_bytes(),
        question_deadline,
    )
    .is_err()
    {
        return not_confirmed("the question could not be sent");
    }
    let answer = match read_gated_line(&mut client, question_deadline) {
        Ok(Some(line)) => line,
        Ok(None) => return not_confirmed("the daemon closed the connection unanswered"),
        Err(_) => return not_confirmed("no answer within the wait"),
    };
    if let Err(refusal) = confirms(&answer) {
        return Gated::NotConfirmed {
            refusal,
            why: "the daemon did not confirm the verb",
        };
    }
    let reply_deadline = std::time::Instant::now() + reply_wait;
    if arm_gated(&client, reply_deadline).is_err() {
        return not_confirmed("the connection could not be bounded");
    }
    if write_gated(
        &mut client,
        format!("{mutation}\n").as_bytes(),
        reply_deadline,
    )
    .is_err()
    {
        return Gated::Unconfirmed("the report could not be written whole");
    }
    match read_gated_line(&mut client, reply_deadline) {
        Ok(Some(line)) => Gated::Reply(line),
        Ok(None) => Gated::Unconfirmed("the daemon closed the connection with no answer"),
        Err(_) => Gated::Unconfirmed("no answer within the wait"),
    }
}

/// Bound `client`'s next read and write by what is left until `deadline`; an
/// error once it has passed.
fn arm_gated(
    client: &crate::platform::ipc::IpcClient,
    deadline: std::time::Instant,
) -> std::io::Result<()> {
    match deadline.checked_duration_since(std::time::Instant::now()) {
        Some(left) if !left.is_zero() => match client.set_timeouts(left) {
            // macOS refuses a socket option on a connection that can neither
            // send nor receive any more (`EINVAL`), which is where a daemon
            // that answered and closed leaves it. A read there returns what is
            // buffered and then end of file, and a write fails, without
            // blocking, so the deadline still holds.
            #[cfg(target_os = "macos")]
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => Ok(()),
            result => result,
        },
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the wait is over",
        )),
    }
}

/// The most [`write_gated`] hands one write call. A blocking write to a
/// local socket keeps waiting for room for as long as its peer makes some,
/// each wait getting the socket's whole timeout again, so one large write
/// could outlast any deadline; a small one returns, and the deadline is
/// checked again before the next.
const GATED_WRITE_CHUNK: usize = 16 * 1024;

/// Write all of `bytes` by `deadline`, then flush: an error for a write
/// failure or the deadline. The deadline is checked before each write of at
/// most [`GATED_WRITE_CHUNK`] bytes, which gets what is left of it, so a peer
/// that drains a little at a time stretches the whole write past it by no
/// more than the one write under way when it passes. How long that one write
/// takes is the platform socket's and the peer's to decide, not this
/// function's: a strict bound would need a nonblocking write, which
/// `IpcClient` does not offer.
fn write_gated(
    client: &mut crate::platform::ipc::IpcClient,
    mut bytes: &[u8],
    deadline: std::time::Instant,
) -> std::io::Result<()> {
    while !bytes.is_empty() {
        arm_gated(client, deadline)?;
        match client.write(&bytes[..bytes.len().min(GATED_WRITE_CHUNK)]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "the connection took nothing",
                ));
            }
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    client.flush()
}

/// One line, at most [`GATED_MAX_LINE`] bytes, read by `deadline`: `Ok(None)`
/// at a clean end of the connection, an error for a read failure, the
/// deadline or an over-long line. Each read is bounded by what is left of the
/// deadline, so the whole line is.
fn read_gated_line(
    client: &mut crate::platform::ipc::IpcClient,
    deadline: std::time::Instant,
) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        arm_gated(client, deadline)?;
        match client.read(&mut byte)? {
            0 if line.is_empty() => return Ok(None),
            0 => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the line was cut",
                ));
            }
            _ if byte[0] == b'\n' => {
                return String::from_utf8(line).map(Some).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "not UTF-8")
                });
            }
            _ if line.len() >= GATED_MAX_LINE => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "the line is too long",
                ));
            }
            _ => line.push(byte[0]),
        }
    }
}

/// PRD #1666 part 3, round 5 (plan D1): whether a capabilities answer
/// confirms task-scoped completion, by equality with the version this build
/// speaks. A refusal of the question comes back with its code and message.
pub fn confirms_task_scoped_completion(line: &str) -> Result<(), Option<(String, String)>> {
    let Ok(reply) = serde_json::from_str::<crate::event::HookCapabilitiesReply>(line.trim()) else {
        return Err(None);
    };
    if reply.task_scoped_completion == Some(crate::event::TASK_SCOPED_COMPLETION) {
        return Ok(());
    }
    Err(reply.code.zip(reply.message))
}

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
/// Fire-and-forget by design: a failure is a lost interface-ready event, which
/// leaves the card and the doorbell waiting for the agent's own hooks to report.
/// For Codex, whose own `SessionStart` arrives with its first turn, that can
/// leave an inbox task pending until a native event or manual input starts a
/// turn (see `crate::wrap`'s `INTERFACE_READY_SEND_TIMEOUT`). A caller on a
/// detached thread has nothing it could do about either, so there is no outcome
/// to return.
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
    let _ = stream.shutdown_write();
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
    read_reply_line_carrying(stream, deadline, &mut Vec::new())
}

/// [`read_reply_line`] for a connection that carries more than one line:
/// bytes read past the newline are kept in `carry`, and the next call starts
/// from them, so a second line that arrived in the same read is not lost
/// (PRD #1631: a hold's `held` line and the reply after it).
fn read_reply_line_carrying(
    stream: &mut crate::platform::ipc::IpcClient,
    deadline: Option<std::time::Instant>,
    carry: &mut Vec<u8>,
) -> Result<String, ReplyReadError> {
    let mut buf = [0u8; 512];
    let mut line = std::mem::take(carry);
    if let Some(newline_pos) = line.iter().position(|&b| b == b'\n') {
        carry.extend_from_slice(&line[newline_pos + 1..]);
        line.truncate(newline_pos);
        return reply_line_text(line);
    }
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
            carry.extend_from_slice(&buf[newline_pos + 1..n]);
            break;
        }
        line.extend_from_slice(&buf[..n]);
    }
    reply_line_text(line)
}

fn reply_line_text(line: Vec<u8>) -> Result<String, ReplyReadError> {
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
        let long = "x".repeat(crate::submitted_prompt::USER_PROMPT_MAX_LEN + 40);
        assert_eq!(
            record_submitted_prompt(&long),
            truncate(&long, crate::submitted_prompt::USER_PROMPT_MAX_LEN)
        );
    }

    /// The whole recorded budget goes on the prompt rather than partly on the
    /// wrapper, so an enveloped long prompt keeps MORE of itself than it would
    /// have if the envelope had been truncated along with it.
    #[test]
    fn unwrapping_spends_the_budget_on_the_prompt() {
        let long = "y".repeat(crate::submitted_prompt::USER_PROMPT_MAX_LEN * 2);
        let enveloped = format!("<pasted_content id=\"239f\">\n{long}");
        assert_eq!(
            record_submitted_prompt(&enveloped),
            truncate(&long, crate::submitted_prompt::USER_PROMPT_MAX_LEN)
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

    /// Issue #622: the `agent-event` CLI's detail lands on the fields every
    /// other producer fills, bounded the same way.
    #[test]
    fn agent_event_cli_carries_the_card_detail() {
        let event = build_agent_event_cli(
            "pane-7".into(),
            Some("agent-7".into()),
            EventType::ToolStart,
            AgentEventDetail {
                cwd: Some("/work/repo".into()),
                prompt: None,
                tool_name: Some("bash".into()),
                tool_detail: Some("touch a.txt\necho second line".into()),
            },
        );
        assert_eq!(event.session_id, "pane-7-session");
        assert_eq!(event.agent_type, AgentType::Pi);
        assert_eq!(event.event_type, EventType::ToolStart);
        assert_eq!(event.pane_id.as_deref(), Some("pane-7"));
        assert_eq!(event.agent_id.as_deref(), Some("agent-7"));
        assert_eq!(event.cwd.as_deref(), Some("/work/repo"));
        assert_eq!(event.tool_name.as_deref(), Some("bash"));
        assert_eq!(event.tool_detail.as_deref(), Some("touch a.txt"));
        assert!(event.user_prompt.is_none());
    }

    /// PRD #1666 (pi_001): the Pi extension hands a deck message over as
    /// the framing followed by the block, and reports that prompt. It is the
    /// deck's prompt, not the user's: the raw text is dropped and its ids are
    /// named for the daemon to label the card `✉ <subject>` and keep it out
    /// of the user's prompt history.
    #[test]
    fn a_pi_prompt_carrying_a_deck_message_is_marked_as_the_decks() {
        let id = "m-k3xzq2ab-0f3c9a71d2e4b850";
        let prompt = format!(
            "{}\n\n[dot-agent-deck message {id} from dot-agent-deck (the prompt this session \
             was started with)]\nSent by: the user.\ndo the task\n[end of dot-agent-deck \
             message {id}]",
            crate::inbox_hub::AGENT_FRAMING
        );
        for event_type in [EventType::Thinking, EventType::Idle] {
            let event = build_agent_event_cli(
                "pane-7".into(),
                Some("agent-7".into()),
                event_type,
                AgentEventDetail {
                    cwd: None,
                    prompt: Some(prompt.clone()),
                    tool_name: None,
                    tool_detail: None,
                },
            );
            assert_eq!(event.user_prompt, None, "{event:?}");
            assert_eq!(
                event.metadata[crate::inbox_prompt::DECK_MESSAGE_IDS_METADATA_KEY],
                id
            );
        }
        let ordinary = build_agent_event_cli(
            "pane-7".into(),
            Some("agent-7".into()),
            EventType::Thinking,
            AgentEventDetail {
                cwd: None,
                prompt: Some("please fix the build".into()),
                tool_name: None,
                tool_detail: None,
            },
        );
        assert_eq!(
            ordinary.user_prompt.as_deref(),
            Some("please fix the build")
        );
        assert!(
            !ordinary
                .metadata
                .contains_key(crate::inbox_prompt::DECK_MESSAGE_IDS_METADATA_KEY)
        );
    }

    #[test]
    fn agent_event_cli_prompt_is_recorded_like_any_producer_prompt() {
        let long = "p".repeat(crate::submitted_prompt::USER_PROMPT_MAX_LEN + 50);
        let event = build_agent_event_cli(
            "pane-7".into(),
            None,
            EventType::Thinking,
            AgentEventDetail {
                prompt: Some(long.clone()),
                ..Default::default()
            },
        );
        assert_eq!(
            event.user_prompt.as_deref(),
            Some(record_submitted_prompt(&long).as_str())
        );
        assert!(event.user_prompt.unwrap().chars().count() < long.chars().count());
    }

    #[test]
    fn agent_event_cli_drops_blank_detail_and_bounds_tool_text() {
        let event = build_agent_event_cli(
            "pane-7".into(),
            None,
            EventType::Idle,
            AgentEventDetail {
                cwd: Some("  ".into()),
                prompt: Some("".into()),
                tool_name: Some("\t".into()),
                tool_detail: Some("\nsecond".into()),
            },
        );
        assert!(event.cwd.is_none());
        assert!(event.user_prompt.is_none());
        assert!(event.tool_name.is_none());
        assert!(event.tool_detail.is_none());

        let long = build_agent_event_cli(
            "pane-7".into(),
            None,
            EventType::ToolStart,
            AgentEventDetail {
                tool_name: Some("t".repeat(300)),
                tool_detail: Some("d".repeat(300)),
                ..Default::default()
            },
        );
        assert_eq!(long.tool_name, Some(truncate(&"t".repeat(300), 80)));
        assert_eq!(long.tool_detail, Some(truncate(&"d".repeat(300), 120)));
    }

    /// PRD #1497: `agent-event --turn-reply-stdin` reads the reply whole when
    /// it fits, never more than the daemon keeps plus a straddling character,
    /// drops a character the bound cut short, and gives up on a stdin that
    /// does not end instead of holding the report.
    #[test]
    fn read_turn_reply_stdin_is_bounded_and_never_blocks() {
        use crate::daemon_protocol::MAX_TURN_REPLY_BYTES;
        use std::io::Cursor;
        use std::time::{Duration, Instant};
        let wait = Duration::from_secs(5);

        let reply = "All 42 tests pass.\n\n--not-a-flag é😀";
        assert_eq!(
            read_turn_reply_stdin(Cursor::new(reply.as_bytes().to_vec()), wait).as_deref(),
            Some(reply)
        );
        assert_eq!(read_turn_reply_stdin(Cursor::new(Vec::new()), wait), None);

        // Far past the bound: what is read stops at the bound plus three bytes,
        // and normalizing it gives exactly what normalizing all of it would.
        let long = format!("a{}", "€".repeat(MAX_TURN_REPLY_BYTES));
        let read = read_turn_reply_stdin(Cursor::new(long.clone().into_bytes()), wait)
            .expect("a long reply");
        assert!(read.len() <= MAX_TURN_REPLY_BYTES + 3, "{}", read.len());
        assert!(read.len() > MAX_TURN_REPLY_BYTES);
        let normalize = |text: String| {
            crate::turn_reply::normalize(crate::daemon_protocol::FinalReply {
                turn_id: None,
                text,
                failed: false,
            })
        };
        assert_eq!(normalize(read), normalize(long));

        // A character the bound cuts short is dropped, never turned into U+FFFD.
        let straddling = format!("{}😀tail", "a".repeat(MAX_TURN_REPLY_BYTES));
        let read = read_turn_reply_stdin(Cursor::new(straddling.into_bytes()), wait).unwrap();
        assert_eq!(read, "a".repeat(MAX_TURN_REPLY_BYTES));
        // Invalid bytes inside the text are replaced, not fatal.
        let read = read_turn_reply_stdin(Cursor::new(b"ok \xff done".to_vec()), wait).unwrap();
        assert_eq!(read, "ok \u{FFFD} done");

        /// A stdin nobody closes: `read` blocks until the test drops `_open`.
        struct NeverEnds(std::sync::mpsc::Receiver<()>);
        impl std::io::Read for NeverEnds {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                let _ = self.0.recv();
                Ok(0)
            }
        }
        let (_open, held) = std::sync::mpsc::channel();
        let started = Instant::now();
        assert_eq!(
            read_turn_reply_stdin(NeverEnds(held), Duration::from_millis(100)),
            None
        );
        assert!(started.elapsed() < wait, "{:?}", started.elapsed());
    }

    /// PR #1617: a writer that sends far more than the bound through a real
    /// pipe — past the 64 KiB a Linux pipe holds, so a reader that stopped at
    /// the bound would leave it blocked until the reader went away and then
    /// fail it with EPIPE — finishes its write whole, and the reply is still
    /// the bounded prefix.
    #[cfg(unix)]
    #[test]
    fn read_turn_reply_stdin_drains_a_writer_past_the_bound() {
        use crate::daemon_protocol::MAX_TURN_REPLY_BYTES;
        use std::io::Write as _;
        use std::time::Duration;

        let (reader, mut writer) = std::io::pipe().expect("pipe");
        let input = "x".repeat(256 * 1024);
        let writing = {
            let input = input.clone();
            std::thread::spawn(move || writer.write_all(input.as_bytes()))
        };
        let read = read_turn_reply_stdin(reader, Duration::from_secs(10)).expect("a reply");
        assert_eq!(
            &read[..MAX_TURN_REPLY_BYTES],
            &input[..MAX_TURN_REPLY_BYTES]
        );
        writing
            .join()
            .expect("writer thread")
            .expect("the whole write is consumed, never refused");
    }

    /// A lifecycle report with no detail is the frame it has always been.
    /// PRD #1497: the Pi extension's `--turn-reply-stdin` on a `--type finished`
    /// report becomes the line's `turn_reply`, which the daemon reads as it
    /// reads a hook's; no other `--type` carries one, a blank text carries
    /// none, and a line without one is the line it always was.
    #[test]
    fn agent_event_cli_turn_reply_rides_the_line_beside_a_settled_event() {
        let finished = build_agent_event_cli(
            "pane-7".into(),
            Some("agent-3".into()),
            EventType::Idle,
            AgentEventDetail::default(),
        );
        let reply = agent_event_cli_turn_reply(
            &EventType::Idle,
            Some("All 42 tests pass.\n\nNothing changed.".into()),
            false,
        )
        .expect("a settled turn's reply");
        let line = agent_event_cli_line(&finished, Some("tok"), Some(&reply)).unwrap();
        assert_eq!(
            crate::turn_reply::reply_from_line(&line),
            Some(crate::daemon_protocol::FinalReply {
                turn_id: None,
                text: "All 42 tests pass.\n\nNothing changed.".into(),
                failed: false,
            })
        );
        // The event itself is unchanged and still parses as one.
        let parsed: AgentEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed.event_type, EventType::Idle);
        assert_eq!(parsed.agent_id.as_deref(), Some("agent-3"));

        let failed = agent_event_cli_turn_reply(&EventType::Idle, Some("--boom".into()), true)
            .expect("a failed turn's reply");
        assert!(failed.failed);
        assert_eq!(failed.text, "--boom");

        for other in [
            EventType::Thinking,
            EventType::ToolStart,
            EventType::WaitingForInput,
        ] {
            assert_eq!(
                agent_event_cli_turn_reply(&other, Some("text".into()), false),
                None,
                "{other:?}"
            );
        }
        // Audit A2: a flagged turn end with a blank reply is a turn that
        // ended with nothing to read, reported as an empty reply; without the
        // flag there is no reply at all.
        let blank = agent_event_cli_turn_reply(&EventType::Idle, Some("  \n ".into()), false)
            .expect("a turn end with nothing to read");
        assert!(blank.is_empty());
        assert!(!blank.failed);
        assert_eq!(
            agent_event_cli_turn_reply(&EventType::Idle, None, false),
            None
        );
        let plain = agent_event_cli_line(&finished, Some("tok"), None).unwrap();
        assert_eq!(
            plain,
            crate::event::agent_event_line(&finished, Some("tok")).unwrap()
        );
        assert_eq!(crate::turn_reply::reply_from_line(&plain), None);
    }

    /// PRD #1497: an over-long reply is clamped to the daemon's bound before
    /// it is sent, the same as every other route's.
    #[test]
    fn agent_event_cli_turn_reply_is_bounded() {
        let long = "é".repeat(crate::daemon_protocol::MAX_TURN_REPLY_BYTES);
        let reply = agent_event_cli_turn_reply(&EventType::Idle, Some(long), false).unwrap();
        assert!(reply.text.len() <= crate::daemon_protocol::MAX_TURN_REPLY_BYTES);
    }

    #[test]
    fn agent_event_cli_without_detail_keeps_the_legacy_frame() {
        let event = build_agent_event_cli(
            "pane-7".into(),
            None,
            EventType::Thinking,
            AgentEventDetail::default(),
        );
        assert!(event.cwd.is_none());
        assert!(event.user_prompt.is_none());
        assert!(event.tool_name.is_none());
        assert!(event.tool_detail.is_none());
        assert!(event.metadata.is_empty());
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

    /// OpenCode publishes an interrupted turn as a `session.error` named
    /// `MessageAbortedError`; that is the turn ending, so it reads Idle, while
    /// any other `session.error` stays Error.
    #[test]
    fn an_opencode_abort_is_idle_and_other_errors_stay_errors() {
        let aborted: OpenCodeHookInput = serde_json::from_str(
            r#"{"session_id":"oc","event":"session.error","error_name":"MessageAbortedError"}"#,
        )
        .unwrap();
        assert_eq!(
            build_opencode_event(aborted).unwrap().event_type,
            EventType::Idle
        );
        let failed: OpenCodeHookInput = serde_json::from_str(
            r#"{"session_id":"oc","event":"session.error","error_name":"UnknownError"}"#,
        )
        .unwrap();
        assert_eq!(
            build_opencode_event(failed).unwrap().event_type,
            EventType::Error
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

    /// Issue #1678: the OpenCode plugin's `subagent.start` / `subagent.stop`
    /// decode to `SubagentStart` / `SubagentStop` carrying the child session's
    /// id as `SUBAGENT_ID_METADATA_KEY`, and a child's tool call and permission
    /// prompt keep that attribution, its answered prompt marked replied. On any
    /// other event the field attributes nothing, and a non-string one is
    /// ignored.
    #[test]
    fn opencode_subagent_id_is_forwarded_on_child_session_reports_only() {
        let decode = |payload: Value| {
            build_opencode_event(serde_json::from_value(payload).unwrap())
                .expect("a known OpenCode event")
        };
        let key = crate::event::SUBAGENT_ID_METADATA_KEY;
        for (event, expected) in [
            ("subagent.start", EventType::SubagentStart),
            ("subagent.stop", EventType::SubagentStop),
            ("tool.execute.before", EventType::ToolStart),
            ("tool.execute.after", EventType::ToolEnd),
            ("permission.asked", EventType::PermissionRequest),
            ("permission.replied", EventType::Thinking),
        ] {
            let decoded = decode(serde_json::json!({
                "session_id": "root", "event": event, "subagent_id": "child", "tool_name": "bash",
            }));
            assert_eq!(
                decoded.is_subagent_permission_reply(),
                event == "permission.replied",
                "{event}"
            );
            assert_eq!(decoded.event_type, expected, "{event}");
            assert_eq!(
                decoded.metadata.get(key).map(String::as_str),
                Some("child"),
                "{event}"
            );
            assert_eq!(decoded.session_id, "root", "{event}");
        }
        for (event, status) in [
            ("session.idle", None),
            ("session.status", Some("busy")),
            ("session.prompt", None),
        ] {
            let decoded = decode(serde_json::json!({
                "session_id": "root", "event": event, "status": status, "subagent_id": "child",
            }));
            assert!(!decoded.is_from_subagent(), "{event}");
        }
        // The main thread's answered prompt is marked replied too, and stays
        // the main thread's `Thinking`.
        let main_reply =
            decode(serde_json::json!({"session_id": "root", "event": "permission.replied"}));
        assert!(!main_reply.is_from_subagent());
        assert!(!main_reply.is_subagent_permission_reply());
        let odd = decode(serde_json::json!({
            "session_id": "root", "event": "tool.execute.before", "subagent_id": 7,
        }));
        assert!(!odd.is_from_subagent());
        assert_eq!(odd.event_type, EventType::ToolStart);
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

        // Issue #1678: an empty id is a subagent's still — the key's presence
        // is the attribution — and the tracker, not the hook, refuses to key on
        // it.
        let empty = build_event(payload(Value::from(""))).unwrap();
        assert_eq!(empty.metadata.get(key).map(String::as_str), Some(""));
        assert!(empty.is_from_subagent());

        // A non-string shape and an absent key are "main thread".
        for odd in [Value::from(7), serde_json::json!({"id": "x"}), Value::Null] {
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

/// PRD #1631: the questions the hook CLI builds from real agent payloads
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
    /// this project" with the directory as its scope, the keyboard-only "Yes,
    /// and switch to auto mode" Claude draws for Bash in the default mode, and
    /// No.
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
                "Yes, and switch to auto mode",
                "No"
            ]
        );
        let roles: Vec<OptionRole> = q.questions[0].options.iter().map(|o| o.role).collect();
        assert_eq!(
            roles,
            vec![
                OptionRole::AllowOnce,
                OptionRole::AllowAlways,
                OptionRole::AllowAlways,
                OptionRole::Deny
            ]
        );
        assert!(q.questions[0].options[2].keyboard_only);
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

    /// Scenario: Claude Code 2.1.296 asks to run a Bash command in the default
    /// permission mode with no suggestion, and draws Yes, "Yes, and switch to
    /// auto mode" and No. The deck lists the same three options with the same
    /// numbers — the auto-mode one keyboard-only, since the hook cannot switch
    /// modes — so "3" is No, as on screen. In another mode, or for another
    /// tool, Claude draws no such option and neither does the deck.
    #[spec("question/detect/018")]
    #[test]
    fn question_detect_018_claude_bash_lists_the_auto_mode_option_claude_draws() {
        let payload = fixture("claude-permission-bash-auto-mode.json");
        let q = question_for(AgentType::ClaudeCode, &payload);
        assert_eq!(
            labels(&q, 0),
            vec!["Yes", "Yes, and switch to auto mode", "No"]
        );
        let options = &q.questions[0].options;
        let numbers: Vec<u32> = options.iter().map(|o| o.index).collect();
        assert_eq!(numbers, vec![1, 2, 3], "the numbers Claude draws");
        assert_eq!(options[0].role, OptionRole::AllowOnce);
        assert!(!options[0].keyboard_only);
        assert!(
            options[1].keyboard_only,
            "the hook cannot switch Claude's mode, so the deck never picks it"
        );
        assert_eq!(options[1].scope, None);
        assert_eq!(options[2].role, OptionRole::Deny);
        assert!(!options[2].keyboard_only);

        // Beside a suggestion, the "always" option comes first, then auto
        // mode, then No — Claude's own order.
        let mut suggested = fixture("claude-permission-bash.json");
        assert_eq!(suggested["permission_mode"], "default");
        let q = question_for(AgentType::ClaudeCode, &suggested);
        assert_eq!(
            labels(&q, 0),
            vec![
                "Yes",
                "Yes, and always allow access to /work/proj from this project",
                "Yes, and switch to auto mode",
                "No"
            ]
        );
        assert_eq!(
            q.questions[0].options[1].scope.as_deref(),
            Some("access to /work/proj for the rest of this session"),
            "the auto-mode option does not take the suggestion's scope"
        );
        assert_eq!(q.questions[0].options[2].scope, None);

        // Not offered outside the default and accept-edits modes, nor for a
        // tool whose dialog has no such option.
        for mode in ["plan", "auto", "bypassPermissions", "dontAsk"] {
            suggested["permission_mode"] = Value::from(mode);
            let q = question_for(AgentType::ClaudeCode, &suggested);
            assert!(
                !labels(&q, 0).contains(&"Yes, and switch to auto mode"),
                "{mode}"
            );
        }
        suggested["permission_mode"] = Value::from("acceptEdits");
        let q = question_for(AgentType::ClaudeCode, &suggested);
        assert!(labels(&q, 0).contains(&"Yes, and switch to auto mode"));
        let write = question_for(
            AgentType::ClaudeCode,
            &fixture("claude-permission-write.json"),
        );
        assert!(!labels(&write, 0).contains(&"Yes, and switch to auto mode"));
    }

    /// Scenario: Claude Code asks to run a Bash command in the default mode,
    /// with and without an "always" suggestion. Whether Claude draws "Yes, and
    /// switch to auto mode" depends on things the payload does not carry, so
    /// the deck marks that one option as one Claude may leave out — and no
    /// other option, of this prompt or of any agent's table.
    #[spec("question/detect/019")]
    #[test]
    fn question_detect_019_only_claude_s_auto_mode_option_may_be_hidden() {
        fn hidden(q: &PendingQuestion) -> Vec<&str> {
            q.questions
                .iter()
                .flat_map(|question| &question.options)
                .filter(|o| o.maybe_hidden)
                .map(|o| o.label.as_str())
                .collect()
        }
        for name in [
            "claude-permission-bash-auto-mode.json",
            "claude-permission-bash.json",
        ] {
            let q = question_for(AgentType::ClaudeCode, &fixture(name));
            assert_eq!(hidden(&q), vec!["Yes, and switch to auto mode"], "{name}");
        }
        let mut plan_mode = fixture("claude-permission-bash.json");
        plan_mode["permission_mode"] = Value::from("plan");
        assert!(hidden(&question_for(AgentType::ClaudeCode, &plan_mode)).is_empty());
        for (agent, name) in [
            (AgentType::ClaudeCode, "claude-permission-write.json"),
            (AgentType::ClaudeCode, "claude-permission-plan.json"),
            (AgentType::ClaudeCode, "claude-ask-user-question-form.json"),
            (AgentType::Codex, "codex-permission-request.json"),
            (AgentType::Codex, "codex-request-user-input.json"),
        ] {
            let q = question_for(agent, &fixture(name));
            assert!(hidden(&q).is_empty(), "{name}: {:?}", hidden(&q));
        }

        // Every table of every agent: the auto-mode entry is the only one.
        let marked: Vec<&str> = crate::agent_registry::ALL
            .iter()
            .filter_map(|spec| spec.questions)
            .flat_map(|tables| tables.menus.iter())
            .flat_map(|menu| menu.options.iter())
            .filter(|option| option.maybe_hidden)
            .map(|option| option.label)
            .collect();
        assert_eq!(marked, vec!["Yes, and switch to auto mode"]);
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

    /// Scenario: The OpenCode plugin's `await-answer --ack` child holds a
    /// permission with a stand-in daemon, asking to acknowledge. The daemon
    /// answers Allow once; the child prints OpenCode's `once` reply for the
    /// plugin, reads the plugin's report that OpenCode refused it, and passes
    /// that report on to the daemon on the same connection. A report that
    /// never comes, or does not parse, is passed on as nothing.
    #[cfg(unix)]
    #[spec("question/hold/016")]
    #[test]
    fn question_hold_016_the_opencode_child_passes_on_what_opencode_made_of_the_reply() {
        use std::io::BufRead as _;
        let raw = serde_json::json!({
            "session_id": "ses_1",
            "event": "permission.asked",
            "properties": fixture("opencode-permission-asked.json"),
        });
        let run = |ack: Option<&str>| -> (Value, Option<String>, String) {
            let (mut event, q, props) = opencode_question_event(&raw.to_string()).unwrap();
            event.pane_id = Some("pane-oc".into());
            let reply = QuestionReply::answered(&q.id, resolved(&q, &[(0, &[1], None)]));
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("hook.sock");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let reply_line = serde_json::to_string(&reply).unwrap();
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let mut held = String::new();
                reader.read_line(&mut held).unwrap();
                writeln!(reader.get_mut(), "{reply_line}").unwrap();
                let mut acked = String::new();
                let n = reader.read_line(&mut acked).unwrap();
                (
                    serde_json::from_str::<Value>(&held).unwrap(),
                    (n > 0).then(|| acked.trim().to_string()),
                )
            });
            let mut out = Vec::new();
            let ack = ack.map(str::to_string);
            opencode_hold_acked_at(&path, event, &q, &props, &mut out, || ack);
            let (held, acked) = server.join().unwrap();
            (held, acked, String::from_utf8(out).unwrap())
        };

        let (held, acked, printed) = run(Some(
            r#"{"delivered":false,"detail":"OpenCode answered 500"}"#,
        ));
        assert_eq!(held["hold"], true);
        assert_eq!(held["ack"], true, "the child asks to acknowledge");
        let printed: Value = serde_json::from_str(printed.trim()).unwrap();
        assert_eq!(printed["body"]["reply"], "once");
        assert_eq!(
            serde_json::from_str::<crate::question::ReplyAck>(&acked.unwrap()).unwrap(),
            crate::question::ReplyAck {
                delivered: false,
                detail: Some("OpenCode answered 500".into()),
            }
        );

        for silent in [None, Some("not json")] {
            let (_, acked, printed) = run(silent);
            assert!(!printed.is_empty(), "the reply is still printed");
            assert_eq!(acked, None, "{silent:?}: nothing passed on");
        }
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
        let deny = decision(&[(0, &[4], None)]);
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

    /// Scenario: A held Claude Code permission hook meets a daemon that never
    /// takes the hold — one that reads the `question` line and then answers
    /// nothing with the connection still open, as a 0.46.0 daemon does, or one
    /// that closes the connection — and sends the event again as a plain one,
    /// without its question, within the hold-start bound instead of at the end
    /// of its wait. A daemon that says it holds the question and later
    /// releases it gets no plain event.
    #[cfg(unix)]
    #[spec("question/hold/021")]
    #[test]
    fn question_hold_021_a_hold_the_daemon_never_takes_is_shown_plainly_at_once() {
        use std::io::BufRead as _;
        use std::time::{Duration, Instant};
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum Daemon {
            Silent,
            Closes,
            HoldsThenReleases,
        }
        const START_WAIT: Duration = Duration::from_millis(500);
        const DEADLINE: Duration = Duration::from_secs(20);
        let payload = fixture("claude-permission-bash.json");
        let (event, q) = held_event(&payload);
        let held = serde_json::to_string(&QuestionReply::held(&q.id)).unwrap();
        let released =
            serde_json::to_string(&QuestionReply::released(&q.id, ReleaseReason::Cleared)).unwrap();
        let run = |daemon: Daemon| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("hook.sock");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let (returned, hook_returned) = std::sync::mpsc::channel::<()>();
            let (held, released) = (held.clone(), released.clone());
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let mut sent = String::new();
                reader.read_line(&mut sent).unwrap();
                let kept = match daemon {
                    Daemon::Silent => Some(reader),
                    Daemon::Closes => None,
                    Daemon::HoldsThenReleases => {
                        writeln!(reader.get_mut(), "{held}").unwrap();
                        writeln!(reader.get_mut(), "{released}").unwrap();
                        Some(reader)
                    }
                };
                // The plain event, if any, is sent before the hook returns.
                hook_returned
                    .recv_timeout(DEADLINE * 2)
                    .expect("the hook returns");
                listener.set_nonblocking(true).unwrap();
                let plain = listener.accept().ok().map(|(stream, _)| {
                    stream.set_nonblocking(false).unwrap();
                    let mut line = String::new();
                    std::io::BufReader::new(stream)
                        .read_line(&mut line)
                        .unwrap();
                    serde_json::from_str::<AgentEvent>(line.trim()).unwrap()
                });
                drop(kept);
                (serde_json::from_str::<Value>(&sent).unwrap(), plain)
            });
            let started = Instant::now();
            let got = hold_question_within_at(&path, &event, DEADLINE, START_WAIT);
            let took = started.elapsed();
            returned.send(()).unwrap();
            let (sent, plain) = server.join().unwrap();
            assert_eq!(sent["hold"], true, "{daemon:?}");
            assert_eq!(
                sent["confirm_hold"], true,
                "{daemon:?}: the hook asks the daemon to say it holds the question"
            );
            (got, plain, took)
        };

        for daemon in [Daemon::Silent, Daemon::Closes] {
            let (got, plain, took) = run(daemon);
            assert_eq!(got, None, "{daemon:?}: no decision");
            assert!(
                took < START_WAIT + Duration::from_secs(5),
                "{daemon:?}: the hook gave up on the hold after {took:?}, not within the \
                 {START_WAIT:?} hold-start bound"
            );
            let plain = plain.unwrap_or_else(|| {
                panic!(
                    "{daemon:?}: the hook sent no plain event, so the card never reads Needs Input"
                )
            });
            assert_eq!(plain.event_type, EventType::PermissionRequest);
            assert_eq!(plain.pane_id.as_deref(), Some("pane-held"));
            assert!(!carries_question_metadata(&plain), "{daemon:?}");
        }

        let (got, plain, _) = run(Daemon::HoldsThenReleases);
        assert_eq!(
            got,
            Some(QuestionReply::released(&q.id, ReleaseReason::Cleared)),
            "the reply after the held line is the one the hook acts on"
        );
        assert!(
            plain.is_none(),
            "a question the daemon held is already on the card: {plain:?}"
        );
    }

    /// Scenario: Claude Code's captured Bash `PermissionRequest` names no call,
    /// but the `PreToolUse` that call ran first carries its `tool_use_id` and
    /// the same input. The deck ties the question to that call, so a parallel
    /// Bash call with another command ending leaves it pending and Needs Input,
    /// and the call's own end clears it. OpenCode's captured `permission.asked`
    /// names its call (`tool.callID`), its `tool.execute.after` carries that
    /// id, and the same holds there.
    #[spec("question/detect/020")]
    #[test]
    fn question_detect_020_a_permission_question_is_tied_to_its_own_call() {
        use crate::state::{AppState, SessionStatus};
        let digest = crate::event::TOOL_INPUT_DIGEST_METADATA_KEY;

        // Claude Code: the call's id comes from its `PreToolUse`.
        let asked = fixture("claude-permission-bash.json");
        let pre_tool_use = |use_id: &str, input: &Value| {
            let mut payload = asked.clone();
            payload["hook_event_name"] = Value::from("PreToolUse");
            payload["tool_use_id"] = Value::from(use_id);
            payload["tool_input"] = input.clone();
            event_for(AgentType::ClaudeCode, &payload)
        };
        let post_tool_use = |use_id: &str| {
            let mut payload = asked.clone();
            payload["hook_event_name"] = Value::from("PostToolUse");
            payload["tool_use_id"] = Value::from(use_id);
            payload["tool_response"] = serde_json::json!({"stdout": "", "stderr": ""});
            event_for(AgentType::ClaudeCode, &payload)
        };
        let ask = event_for(AgentType::ClaudeCode, &asked);
        assert_eq!(
            ask.metadata.get("tool_use_id"),
            None,
            "the payload names no call"
        );
        let start_a = pre_tool_use("toolu_A", &asked["tool_input"]);
        let start_b = pre_tool_use("toolu_B", &serde_json::json!({"command": "ls"}));
        assert_eq!(start_a.metadata.get(digest), ask.metadata.get(digest));
        assert_ne!(start_b.metadata.get(digest), ask.metadata.get(digest));

        let session = ask.session_id.clone();
        let mut state = AppState::default();
        for e in [start_a, start_b, ask] {
            state.apply_event(e);
        }
        let tied = |state: &AppState| {
            state.sessions[&session]
                .pending_question
                .as_ref()
                .map(|q| q.tool.as_ref().and_then(|t| t.use_id.clone()))
        };
        assert_eq!(tied(&state), Some(Some("toolu_A".to_string())));
        state.apply_event(post_tool_use("toolu_B"));
        assert_eq!(tied(&state), Some(Some("toolu_A".to_string())));
        assert_eq!(
            state.sessions[&session].status,
            SessionStatus::WaitingForInput
        );
        state.apply_event(post_tool_use("toolu_A"));
        assert_eq!(tied(&state), None);

        // OpenCode: the call's id is in the question and the tool's end.
        let props = fixture("opencode-permission-asked.json");
        let call = props["tool"]["callID"].as_str().unwrap().to_string();
        let q = crate::question::opencode_permission_asked(&props, 1).unwrap();
        assert_eq!(
            q.tool.as_ref().unwrap().use_id.as_deref(),
            Some(call.as_str())
        );
        let opencode = |event: &str, call_id: Option<&str>, properties: Option<Value>| {
            build_opencode_event(OpenCodeHookInput {
                session_id: "oc-1".into(),
                event: event.into(),
                tool_name: Some("bash".into()),
                call_id: call_id.map(str::to_string),
                properties,
                ..Default::default()
            })
            .unwrap()
        };
        let end_other = opencode("tool.execute.after", Some("call_other"), None);
        assert_eq!(
            end_other.metadata.get("tool_use_id").map(String::as_str),
            Some("call_other")
        );
        let mut state = AppState::default();
        state.apply_event(opencode("permission.asked", None, Some(props.clone())));
        let pending = |state: &AppState| {
            state
                .sessions
                .values()
                .find_map(|s| s.pending_question.as_ref().map(|q| q.id.clone()))
        };
        assert_eq!(pending(&state).as_deref(), props["id"].as_str());
        state.apply_event(end_other);
        assert_eq!(pending(&state).as_deref(), props["id"].as_str());
        state.apply_event(opencode("tool.execute.after", Some(&call), None));
        assert_eq!(pending(&state), None);
    }
}

/// Issue #1678: an OpenCode subagent's (child session's) permission prompt,
/// decoded by the real hook builder and applied by the daemon's reducer.
#[cfg(test)]
mod opencode_child_permission_tests {
    use std::sync::Arc;

    use super::build_opencode_event;
    use crate::event::GenerationVerdict;
    use crate::state::{AgentOwnership, AppState, Ownership, SessionStatus};

    const PANE: &str = "oc-permission-pane";
    const AGENT: &str = "oc-permission-agent";

    struct OwnsPane;

    impl AgentOwnership for OwnsPane {
        fn generation_ownership(&self, pane_id: Option<&str>, agent_id: Option<&str>) -> Ownership {
            if pane_id == Some(PANE) && agent_id == Some(AGENT) {
                Ownership::Owned
            } else {
                Ownership::Unclaimed
            }
        }

        fn pane_generation_verdict(
            &self,
            pane_id: &str,
            agent_id: &str,
        ) -> Option<GenerationVerdict> {
            (pane_id == PANE && agent_id == AGENT).then_some(GenerationVerdict::Current)
        }
    }

    /// A daemon-side state owning the pane, and the oracle it holds weakly.
    fn daemon() -> (AppState, Arc<dyn AgentOwnership>) {
        let mut state = AppState::default();
        let ownership: Arc<dyn AgentOwnership> = Arc::new(OwnsPane);
        state.set_agent_ownership(Arc::downgrade(&ownership));
        state.register_pane(PANE.into());
        (state, ownership)
    }

    /// Feed one plugin payload (filed under `root`) through the hook builder.
    fn send(state: &mut AppState, payload: serde_json::Value) {
        let mut payload = payload;
        payload["session_id"] = "root".into();
        let mut event = build_opencode_event(serde_json::from_value(payload).unwrap())
            .expect("a known OpenCode event");
        event.pane_id = Some(PANE.into());
        event.agent_id = Some(AGENT.into());
        state.apply_event(event);
    }

    /// The card's status, and its published background subagent count.
    fn card(state: &AppState) -> (SessionStatus, u32) {
        let live = state.live_session_for(AGENT, Some(PANE)).expect("the card");
        (
            live.status,
            live.background.map_or(0, |work| work.subagents),
        )
    }

    /// The root's turn starts, a child session goes active, and the root's
    /// turn ends with the child still running.
    fn root_ended_with_child_running(state: &mut AppState) {
        send(state, serde_json::json!({"event": "session.created"}));
        send(
            state,
            serde_json::json!({"event": "session.status", "status": "busy"}),
        );
        send(
            state,
            serde_json::json!({"event": "subagent.start", "subagent_id": "child"}),
        );
        send(
            state,
            serde_json::json!({"event": "session.status", "status": "idle"}),
        );
        assert_eq!(card(state), (SessionStatus::Idle, 1), "precondition");
    }

    fn ask(subagent: Option<&str>) -> serde_json::Value {
        let mut payload = serde_json::json!({"event": "permission.asked", "prompt": "Allow bash?"});
        if let Some(id) = subagent {
            payload["subagent_id"] = id.into();
        }
        payload
    }

    fn reply(subagent: Option<&str>) -> serde_json::Value {
        let mut payload = serde_json::json!({"event": "permission.replied"});
        if let Some(id) = subagent {
            payload["subagent_id"] = id.into();
        }
        payload
    }

    /// Scenario: after the root's turn ended, a running child session asks for
    /// permission and the user answers it. The card shows Needs Input and then
    /// Idle again, the background subagent stays shown, and the root's turn is
    /// not restarted: a later child report still leaves the card Idle.
    #[test]
    fn opencode_child_permission_reply_after_the_turn_ended_returns_the_card_to_idle() {
        let (mut state, _ownership) = daemon();
        root_ended_with_child_running(&mut state);
        send(&mut state, ask(Some("child")));
        assert_eq!(card(&state), (SessionStatus::WaitingForInput, 1));
        send(&mut state, reply(Some("child")));
        assert_eq!(
            card(&state),
            (SessionStatus::Idle, 1),
            "the answer ends the subagent's wait without restarting the root's turn"
        );
        // A reply with no wait outstanding changes nothing.
        send(&mut state, reply(Some("child")));
        assert_eq!(card(&state), (SessionStatus::Idle, 1));
        send(
            &mut state,
            serde_json::json!({"event": "subagent.stop", "subagent_id": "child"}),
        );
        assert_eq!(card(&state), (SessionStatus::Idle, 0));
    }

    /// Scenario: after the root's turn ended, a running child asks for
    /// permission and then fails (its stop) without the prompt being answered.
    /// Its prompt goes with it: the card returns to Idle with no background.
    #[test]
    fn opencode_child_permission_without_a_reply_ends_with_the_child() {
        let (mut state, _ownership) = daemon();
        root_ended_with_child_running(&mut state);
        send(&mut state, ask(Some("child")));
        send(
            &mut state,
            serde_json::json!({"event": "subagent.stop", "subagent_id": "child"}),
        );
        assert_eq!(card(&state), (SessionStatus::Idle, 0));
    }

    /// Scenario: during the root's turn, the root and a child both ask for
    /// permission, in either order, and the child's prompt is answered. The
    /// root's own prompt is still waiting, so the card stays on Needs Input;
    /// with only the child's prompt open, its answer returns the card to
    /// Thinking.
    #[test]
    fn opencode_child_permission_reply_leaves_the_main_threads_wait() {
        for child_first in [false, true] {
            let (mut state, _ownership) = daemon();
            send(&mut state, serde_json::json!({"event": "session.created"}));
            send(
                &mut state,
                serde_json::json!({"event": "session.status", "status": "busy"}),
            );
            send(
                &mut state,
                serde_json::json!({"event": "subagent.start", "subagent_id": "child"}),
            );
            let order = if child_first {
                [ask(Some("child")), ask(None)]
            } else {
                [ask(None), ask(Some("child"))]
            };
            for payload in order {
                send(&mut state, payload);
            }
            send(&mut state, reply(Some("child")));
            assert_eq!(
                card(&state),
                (SessionStatus::WaitingForInput, 0),
                "child_first={child_first}: the main thread's prompt is still open"
            );
            send(
                &mut state,
                serde_json::json!({"event": "subagent.stop", "subagent_id": "child"}),
            );
            assert_eq!(card(&state).0, SessionStatus::WaitingForInput);
        }

        let (mut state, _ownership) = daemon();
        send(&mut state, serde_json::json!({"event": "session.created"}));
        send(
            &mut state,
            serde_json::json!({"event": "session.status", "status": "busy"}),
        );
        send(&mut state, ask(Some("child")));
        send(&mut state, reply(Some("child")));
        assert_eq!(card(&state), (SessionStatus::Thinking, 0));
    }

    fn tool_end(subagent: &str) -> serde_json::Value {
        serde_json::json!({
            "event": "tool.execute.after",
            "tool_name": "bash",
            "subagent_id": subagent,
        })
    }

    /// Scenario (Qodo / Greptile on PR #1688): after the root's turn ended,
    /// child A asks for permission and child B, which never asked, then
    /// finishes a tool call. A's prompt is still unanswered, so the card stays
    /// on Needs Input; A's own tool end (the control) then releases it to Idle.
    #[test]
    fn opencode_child_permission_another_childs_tool_end_keeps_the_prompt() {
        let (mut state, _ownership) = daemon();
        root_ended_with_child_running(&mut state);
        send(
            &mut state,
            serde_json::json!({"event": "subagent.start", "subagent_id": "other"}),
        );
        send(&mut state, ask(Some("child")));
        send(&mut state, tool_end("other"));
        assert_eq!(
            card(&state),
            (SessionStatus::WaitingForInput, 2),
            "another child's tool end must not hide this child's prompt"
        );
        // A repeat, and one from a subagent the card never saw, change nothing.
        send(&mut state, tool_end("other"));
        send(&mut state, tool_end("stranger"));
        assert_eq!(card(&state).0, SessionStatus::WaitingForInput);
        send(&mut state, tool_end("child"));
        assert_eq!(
            card(&state),
            (SessionStatus::Idle, 2),
            "the asking child's own tool end releases its share"
        );
    }

    /// Scenario (Qodo / Greptile on PR #1688): during the root's turn the root
    /// asks for permission, and a running child then finishes a tool call. The
    /// root's prompt is the main thread's, which no subagent's report can
    /// answer, so the card stays on Needs Input — also when the child had
    /// joined the wait with a prompt of its own and its tool end releases it.
    #[test]
    fn opencode_child_permission_a_childs_tool_end_keeps_the_main_threads_prompt() {
        for child_asked in [false, true] {
            let (mut state, _ownership) = daemon();
            send(&mut state, serde_json::json!({"event": "session.created"}));
            send(
                &mut state,
                serde_json::json!({"event": "session.status", "status": "busy"}),
            );
            send(
                &mut state,
                serde_json::json!({"event": "subagent.start", "subagent_id": "child"}),
            );
            send(&mut state, ask(None));
            if child_asked {
                send(&mut state, ask(Some("child")));
            }
            send(&mut state, tool_end("child"));
            assert_eq!(
                card(&state).0,
                SessionStatus::WaitingForInput,
                "child_asked={child_asked}: the root's prompt is still open"
            );
        }
    }

    /// Scenario (audit AU-5): after the root's turn ended, two child sessions
    /// whose ids are over the bound — each attributed to its own stand-in, and
    /// never reported active — both ask for permission, and only the first is
    /// answered. The second's prompt is still open, so the card stays on Needs
    /// Input until it is answered too.
    #[test]
    fn opencode_child_permission_two_oversized_children_keep_their_own_prompts() {
        let (mut state, _ownership) = daemon();
        root_ended_with_child_running(&mut state);
        let first = format!("oversized-id:300:{}", "0".repeat(16));
        let second = format!("oversized-id:300:{}", "1".repeat(16));
        send(&mut state, ask(Some(&first)));
        send(&mut state, ask(Some(&second)));
        send(&mut state, reply(Some(&first)));
        assert_eq!(
            card(&state),
            (SessionStatus::WaitingForInput, 1),
            "the second oversized child's prompt is still open"
        );
        send(&mut state, reply(Some(&second)));
        assert_eq!(card(&state), (SessionStatus::Idle, 1));
    }

    /// Scenario (audit AU-7, the mixed-version pairing): during the root's
    /// turn the root and a child both ask for permission and only the child's
    /// prompt is answered. A new reducer keeps the root's Needs Input. An
    /// older client knows neither the subagent attribution on that answer nor
    /// its `permission_state`, so it folds the same event as a plain Thinking —
    /// modelled here by stripping both before the fold — and shows Thinking,
    /// which is what it showed for that answer before issue #1678. This is the
    /// accepted old-client display limitation docs/develop/background-work.md
    /// records, not a contract break.
    #[test]
    fn opencode_child_permission_reply_an_older_client_folds_as_thinking() {
        let events = || {
            [
                serde_json::json!({"event": "session.created"}),
                serde_json::json!({"event": "session.status", "status": "busy"}),
                ask(None),
                ask(Some("child")),
                reply(Some("child")),
            ]
            .map(|payload| {
                let mut payload = payload;
                payload["session_id"] = "root".into();
                let mut event = build_opencode_event(serde_json::from_value(payload).unwrap())
                    .expect("a known OpenCode event");
                event.pane_id = Some(PANE.into());
                event.agent_id = Some(AGENT.into());
                event
            })
        };
        let status = |state: &AppState| state.live_session_for(AGENT, Some(PANE)).unwrap().status;

        let mut new_client = AppState::default();
        new_client.register_pane(PANE.into());
        for event in events() {
            new_client.apply_event(event);
        }
        assert_eq!(status(&new_client), SessionStatus::WaitingForInput);

        let mut old_client = AppState::default();
        old_client.register_pane(PANE.into());
        let mut stripped = 0;
        for mut event in events() {
            if event.is_subagent_permission_reply() {
                stripped += 1;
                event
                    .metadata
                    .remove(crate::event::PERMISSION_STATE_METADATA_KEY);
                event
                    .metadata
                    .remove(crate::event::SUBAGENT_ID_METADATA_KEY);
            }
            old_client.apply_event(event);
        }
        assert_eq!(stripped, 1, "the child's answer is the tagged reply");
        assert_eq!(
            status(&old_client),
            SessionStatus::Thinking,
            "the old-client view of the answer is the legacy Thinking"
        );
    }
}

/// PRD #1666 part 3, round 5 (plan D1, §6.5): the task-scoped `work-done`
/// sender against a scripted daemon on a socket of its own: what it sends,
/// on which connection, and what it reads as unconfirmed.
#[cfg(all(test, unix))]
mod gated_send_tests {
    use std::io::BufRead as _;
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use spec::spec;

    use super::*;

    /// What the scripted daemon does after it reads the question.
    #[derive(Clone)]
    enum Script {
        /// Answer nothing, holding the connection open.
        Silent,
        /// Close the connection without answering.
        Close,
        /// Answer this line, after `delay`.
        Answer(&'static str, Duration),
        /// Confirm, read the mutation, then answer `reply` (`None`: close).
        Confirm(Option<&'static str>),
        /// Confirm, read part of the mutation, then reset the connection.
        ConfirmThenCut,
        /// Write the confirmation one byte at a time, this far apart.
        DripConfirm(Duration),
        /// Confirm, read the mutation, then write `reply` one byte at a time,
        /// this far apart.
        ConfirmThenDrip(&'static str, Duration),
        /// Confirm, then read the mutation at most this many bytes at a time,
        /// this far apart, and never answer.
        ConfirmThenDrain(usize, Duration),
    }

    const CONFIRMED: &str = r#"{"task_scoped_completion":1}"#;

    struct Stub {
        path: std::path::PathBuf,
        _dir: tempfile::TempDir,
        /// Every line the stub read, by connection.
        lines: Arc<Mutex<Vec<Vec<String>>>>,
    }

    fn stub(script: Script) -> Stub {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("hook.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let lines: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let seen = Arc::clone(&lines);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut read = Vec::new();
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                read.push(line.trim().to_string());
                let index = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(read.clone());
                    seen.len() - 1
                };
                let record = |line: &str| seen.lock().unwrap()[index].push(line.trim().to_string());
                match script.clone() {
                    Script::Silent => std::thread::sleep(Duration::from_secs(2)),
                    Script::Close => {}
                    Script::Answer(answer, delay) => {
                        std::thread::sleep(delay);
                        let _ = writeln!(stream, "{answer}");
                        let mut more = String::new();
                        if reader.read_line(&mut more).unwrap_or(0) > 0 {
                            record(&more);
                        }
                    }
                    Script::Confirm(reply) => {
                        writeln!(stream, "{CONFIRMED}").unwrap();
                        let mut mutation = String::new();
                        if reader.read_line(&mut mutation).unwrap_or(0) > 0 {
                            record(&mutation);
                        }
                        if let Some(reply) = reply {
                            let _ = writeln!(stream, "{reply}");
                        }
                    }
                    Script::ConfirmThenCut => {
                        writeln!(stream, "{CONFIRMED}").unwrap();
                        let mut first = [0u8; 8];
                        let _ = std::io::Read::read(&mut reader, &mut first);
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    }
                    Script::DripConfirm(every) => {
                        for byte in format!("{CONFIRMED}\n").bytes() {
                            std::thread::sleep(every);
                            if stream.write_all(&[byte]).is_err() {
                                break;
                            }
                        }
                        let mut more = String::new();
                        if reader.read_line(&mut more).unwrap_or(0) > 0 {
                            record(&more);
                        }
                    }
                    Script::ConfirmThenDrip(reply, every) => {
                        writeln!(stream, "{CONFIRMED}").unwrap();
                        let mut mutation = String::new();
                        if reader.read_line(&mut mutation).unwrap_or(0) > 0 {
                            record(&mutation);
                        }
                        for byte in format!("{reply}\n").bytes() {
                            std::thread::sleep(every);
                            if stream.write_all(&[byte]).is_err() {
                                break;
                            }
                        }
                    }
                    Script::ConfirmThenDrain(chunk, every) => {
                        writeln!(stream, "{CONFIRMED}").unwrap();
                        let mut buf = vec![0u8; chunk];
                        loop {
                            std::thread::sleep(every);
                            match std::io::Read::read(&mut reader, &mut buf) {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                    }
                }
            }
        });
        Stub {
            path,
            _dir: dir,
            lines,
        }
    }

    fn send(stub: &Stub) -> Gated {
        send_gated_at(
            &stub.path,
            r#"{"message_type":"hook_capabilities","pane_id":"p"}"#,
            r#"{"message_type":"work_done_task","pane_id":"p","mode":{"kind":"uncorrelated"},"report":"r","timestamp":"2026-01-01T00:00:00Z"}"#,
            confirms_task_scoped_completion,
            Duration::from_millis(300),
            Duration::from_millis(500),
        )
    }

    fn mutations(stub: &Stub) -> usize {
        std::thread::sleep(Duration::from_millis(100));
        stub.lines
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .filter(|line| line.contains("work_done_task"))
            .count()
    }

    /// Scenario: A daemon that answers the capabilities question with nothing, within the wait, gets no report: the sender says the daemon did not confirm, and the stub never reads a mutation.
    #[spec("inbox/protocol/024")]
    #[test]
    fn protocol_024_a_silent_daemon_gets_no_report() {
        let stub = stub(Script::Silent);
        assert!(matches!(send(&stub), Gated::NotConfirmed { .. }));
        assert_eq!(mutations(&stub), 0);
    }

    /// Scenario: A daemon that answers the capabilities question one byte at a time, each byte well inside the wait but the whole answer far past it, gets no report: the question's wait bounds the whole answer, not each read, so the sender says the daemon did not confirm within about the wait, and the stub never reads a mutation.
    #[spec("inbox/protocol/024")]
    #[test]
    fn protocol_024_a_dripped_answer_is_bounded_by_the_whole_wait() {
        let stub = stub(Script::DripConfirm(Duration::from_millis(60)));
        let started = std::time::Instant::now();
        assert!(matches!(send(&stub), Gated::NotConfirmed { .. }));
        assert!(
            started.elapsed() < Duration::from_millis(1000),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(mutations(&stub), 0);
    }

    /// Scenario: A daemon that confirms, reads the report, and then drips its answer one byte at a time past the reply's wait: the sender stops at the wait and says the result is unconfirmed, having sent the report once.
    #[spec("inbox/protocol/028")]
    #[test]
    fn protocol_028_a_dripped_reply_is_bounded_by_the_whole_wait() {
        let reply = r#"{"disposition":"uncorrelated","destination":"inbox","report_id":"0123456789abcdef0123456789abcdef"}"#;
        let stub = stub(Script::ConfirmThenDrip(reply, Duration::from_millis(30)));
        let started = std::time::Instant::now();
        assert!(matches!(send(&stub), Gated::Unconfirmed(_)));
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(mutations(&stub), 1);
    }

    /// Scenario: A daemon that confirms, then takes a large report a few kilobytes at a time, each read well inside the reply's wait but the whole report far past it: the sender checks the reply's deadline between small writes, so it stops soon after the wait rather than when the report is drained, and says the result is unconfirmed.
    #[spec("inbox/protocol/028")]
    #[test]
    fn protocol_028_a_slowly_drained_report_gives_up_long_before_it_drains() {
        let stub = stub(Script::ConfirmThenDrain(4096, Duration::from_millis(20)));
        // At 4 KiB every 20 ms the report takes about 40 s to drain.
        let mutation = format!(
            r#"{{"message_type":"work_done_task","pane_id":"p","mode":{{"kind":"uncorrelated"}},"report":"{}","timestamp":"2026-01-01T00:00:00Z"}}"#,
            "x".repeat(8 * 1024 * 1024)
        );
        let started = std::time::Instant::now();
        let sent = send_gated_at(
            &stub.path,
            r#"{"message_type":"hook_capabilities","pane_id":"p"}"#,
            &mutation,
            confirms_task_scoped_completion,
            Duration::from_millis(300),
            Duration::from_millis(500),
        );
        assert!(matches!(sent, Gated::Unconfirmed(_)), "{sent:?}");
        assert!(
            started.elapsed() < Duration::from_millis(2000),
            "{:?}",
            started.elapsed()
        );
    }

    /// Scenario: A daemon that answers the question with something else — an empty object, a legacy acknowledgement, a refusal with no capability — gets no report.
    #[spec("inbox/protocol/025")]
    #[test]
    fn protocol_025_a_wrong_answer_withholds_the_report() {
        for answer in [
            "{}",
            r#"{"kind":"signal_ack","accepted":true}"#,
            r#"{"task_scoped_completion":null}"#,
        ] {
            let stub = stub(Script::Answer(answer, Duration::ZERO));
            assert!(
                matches!(send(&stub), Gated::NotConfirmed { .. }),
                "{answer}"
            );
            assert_eq!(mutations(&stub), 0, "{answer}");
        }
    }

    /// Scenario: A malformed answer, a closed connection, and an answer that arrives after the wait each withhold the report; nothing is sent after them.
    #[spec("inbox/protocol/026")]
    #[test]
    fn protocol_026_malformed_closed_and_late_answers_withhold_the_report() {
        for script in [
            Script::Answer("not json at all", Duration::ZERO),
            Script::Close,
            Script::Answer(CONFIRMED, Duration::from_millis(700)),
        ] {
            let stub = stub(script);
            assert!(matches!(send(&stub), Gated::NotConfirmed { .. }));
            assert_eq!(mutations(&stub), 0);
        }
    }

    /// Scenario: A daemon that confirms gets the report on the same connection that asked, once, and its one answer is what the sender reads.
    #[spec("inbox/protocol/027")]
    #[test]
    fn protocol_027_a_confirmed_report_rides_the_asking_connection() {
        let stub = stub(Script::Confirm(Some(r#"{"disposition":"uncorrelated"}"#)));
        assert_eq!(
            send(&stub),
            Gated::Reply(r#"{"disposition":"uncorrelated"}"#.to_string())
        );
        let lines = stub.lines.lock().unwrap().clone();
        assert_eq!(lines.len(), 1, "one connection: {lines:?}");
        assert_eq!(
            lines[0].len(),
            2,
            "the question, then the report: {lines:?}"
        );
    }

    /// Scenario: The report is sent and the daemon closes without answering: the sender reads it as unconfirmed, and does not send it again.
    #[spec("inbox/protocol/028")]
    #[test]
    fn protocol_028_a_lost_answer_is_unconfirmed() {
        let stub = stub(Script::Confirm(None));
        assert!(matches!(send(&stub), Gated::Unconfirmed(_)));
        assert_eq!(mutations(&stub), 1, "sent once, never again");
    }

    /// Scenario: A capability of 2, of -1, or given as a string is not version 1: the report is withheld (equality, not at-least).
    #[spec("inbox/protocol/030")]
    #[test]
    fn protocol_030_only_version_one_confirms() {
        for answer in [
            r#"{"task_scoped_completion":2}"#,
            r#"{"task_scoped_completion":-1}"#,
            r#"{"task_scoped_completion":"1"}"#,
        ] {
            let stub = stub(Script::Answer(answer, Duration::ZERO));
            assert!(
                matches!(send(&stub), Gated::NotConfirmed { .. }),
                "{answer}"
            );
            assert_eq!(mutations(&stub), 0, "{answer}");
        }
    }

    /// Scenario: The daemon resets the connection partway through reading the report: unconfirmed, sent once. An answer whose disposition this build does not know is read as unconfirmed too, with exit 1 and the wording for its mode.
    #[spec("inbox/protocol/031")]
    #[test]
    fn protocol_031_cut_reports_and_unknown_dispositions_are_unconfirmed() {
        let stub = stub(Script::ConfirmThenCut);
        assert!(matches!(send(&stub), Gated::Unconfirmed(_)));
        let reply: crate::event::WorkDoneTaskReply =
            serde_json::from_str(r#"{"disposition":"from_the_future"}"#).unwrap();
        assert_eq!(
            reply.disposition,
            crate::event::WorkDoneDisposition::Unknown
        );
        for (task, terminal) in [(Some("m-1"), false), (None, true), (None, false)] {
            let answer = crate::work_done_text::cli_answer(&reply, task, terminal);
            assert!(!answer.success);
            assert_eq!(
                answer.stderr,
                Some(crate::work_done_text::unconfirmed(task, terminal)),
                "{task:?} {terminal}"
            );
        }
        assert!(!crate::work_done_text::unconfirmed(None, true).contains("note"));
    }
}
