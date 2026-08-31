use std::collections::HashMap;
use std::io::Read as _;
use std::io::Write as _;
use std::process::ExitCode;

use chrono::Utc;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::agent_pty::{DOT_AGENT_DECK_AGENT_ID, DOT_AGENT_DECK_PANE_ID};
use crate::config::socket_path;
use crate::event::{AgentEvent, AgentType, EventType};

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
struct OpenCodeHookInput {
    session_id: String,
    event: String,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    status: Option<String>,
    cwd: Option<String>,
    prompt: Option<String>,
    #[serde(flatten)]
    _extra: HashMap<String, Value>,
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

    let json = match serde_json::to_string(&event) {
        Ok(j) => j,
        Err(_) => return ExitCode::SUCCESS,
    };

    let _ = send_to_socket(&json);
    ExitCode::SUCCESS
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
        // NOTE: there is no `StopFailure` hook event in Claude Code or Codex
        // 0.144.4 (Codex's installed events are enumerated in
        // [`crate::codex_hooks_manage`]). A mid-session failure surfaces instead
        // through a FAILED `PostToolUse` (`tool_response` reports a non-zero
        // exit / error), which [`build_event_typed`] promotes to
        // [`EventType::Error`] — see [`tool_response_is_failure`].
        "PreCompact" => Some(EventType::Compacting),
        "PostCompact" => Some(EventType::Thinking),
        // Devin's spelling of the post-compaction event. Devin fires ONLY the
        // post event (it has no `PreCompact`), so this is the sole compaction
        // signal a Devin session produces.
        "PostCompaction" => Some(EventType::Thinking),
        "SubagentStart" => Some(EventType::SubagentStart),
        "SubagentStop" => Some(EventType::SubagentStop),
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
        _extra: extra,
    } = input;

    let mut event_type = map_event_type(&hook_event_name)?;
    // PRD #20 W3-Pass-2 (finding #9): a FAILED tool call arrives as an ordinary
    // `PostToolUse` (→ `ToolEnd`) whose `tool_response` reports the failure.
    // Promote it to a mid-session `Error` (emitted before the process exits) so
    // the card surfaces the failure with Working/Idle/Error parity, rather than
    // discarding `tool_response` and showing a benign `ToolEnd`. `tool_response`
    // rides the flattened `_extra` (it is not a first-class field), keeping the
    // existing input/struct shape unchanged.
    if event_type == EventType::ToolEnd
        && extra
            .get("tool_response")
            .is_some_and(tool_response_is_failure)
    {
        event_type = EventType::Error;
    }
    let tool_detail = extract_tool_detail(tool_name.as_deref(), tool_input.as_ref());

    let user_prompt = prompt.map(|p| truncate(&p, crate::prompt_delivery::USER_PROMPT_MAX_LEN));
    let pane_id = std::env::var(DOT_AGENT_DECK_PANE_ID).ok();
    // PRD #92 F9 followup-7: the daemon injects DOT_AGENT_DECK_AGENT_ID
    // on spawn (same pattern as DOT_AGENT_DECK_PANE_ID). Forwarding it
    // here lets the post-respawn dispatch task scope its SessionStart
    // wait to the NEW agent and reject a late SessionStart from the
    // OLD agent that fires within the subscribe→kill window.
    let agent_id = std::env::var(DOT_AGENT_DECK_AGENT_ID).ok();

    let mut metadata = HashMap::new();
    if let Some(tool_use_id) = tool_use_id {
        metadata.insert("tool_use_id".to_string(), tool_use_id);
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

    // Store full bash command for reactive pane routing (tool_detail truncates).
    if matches!(event_type, EventType::ToolStart)
        && tool_name.as_deref() == Some("Bash")
        && let Some(ref input) = tool_input
        && let Some(cmd) = input.get("command").and_then(|v| v.as_str())
    {
        metadata.insert("bash_command".to_string(), cmd.to_string());
    }

    Some(AgentEvent {
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
    })
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
        _ => None,
    }
}

fn build_opencode_event(input: OpenCodeHookInput) -> Option<AgentEvent> {
    let event_type = map_opencode_event_type(&input.event, input.status.as_deref())?;
    let tool_detail = extract_tool_detail(input.tool_name.as_deref(), input.tool_input.as_ref());
    let user_prompt = input
        .prompt
        .map(|p| truncate(&p, crate::prompt_delivery::USER_PROMPT_MAX_LEN));
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

    // Store full bash command for reactive pane routing (tool_detail truncates).
    if matches!(event_type, EventType::ToolStart)
        && input.tool_name.as_deref() == Some("Bash")
        && let Some(ref tool_input) = input.tool_input
        && let Some(cmd) = tool_input.get("command").and_then(|v| v.as_str())
    {
        metadata.insert("bash_command".to_string(), cmd.to_string());
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
/// [`SocketReply::Unreachable`] means the signal was not delivered.
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

pub fn send_to_socket(json: &str) -> Option<()> {
    send_to_socket_at(&socket_path(), json)
}

/// [`send_to_socket`] against an explicit endpoint, rather than one resolved
/// from the environment. Lets the socket tests exercise "no daemon listening
/// at this path" by passing a temp-dir path directly, instead of mutating the
/// process-global `DOT_AGENT_DECK_SOCKET` env var that production
/// `socket_path()` reads.
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
    let _ = send_to_socket_bounded_at(&socket_path(), json, timeout);
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
/// back on the same connection. Used by the read-only `get-seed` verb, the one
/// hook-socket message that expects a response (the delegate / work-done /
/// agent-event senders are fire-and-forget). Returns `None` if the socket is
/// absent/unreadable, or if the daemon goes completely silent — or keeps
/// dribbling bytes without ever finishing a reply line — for longer than
/// [`GET_SEED_REQUEST_TIMEOUT`]. The caller (get-seed) treats all of these
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
#[derive(Debug)]
pub enum SocketReply {
    /// Connected, wrote the request, and read a reply line (possibly empty).
    Line(String),
    /// Connected and wrote the request, but no reply arrived before the
    /// deadline — either the daemon closed without answering (an old daemon
    /// that doesn't know this request type), an individual read timed out,
    /// or the total-operation deadline elapsed while a peer kept dribbling
    /// bytes without ever finishing the reply line. The request was still
    /// sent.
    ///
    /// For `delegate` this is the pre-response contract: the verb was
    /// fire-and-forget before the daemon answered it at all, so a daemon that
    /// does not answer must stay a success — handed to the socket,
    /// unverifiable — rather than becoming a phantom failure on every
    /// mixed-version pair. It is deliberately NOT "delivered": a daemon killed
    /// between accept and read also lands here.
    NoReply,
    /// Could not connect to the daemon, or failed while writing — the
    /// request was never sent. The only case a caller may report as "not
    /// delivered".
    Unreachable,
}

fn request_from_socket_inner(json: &str, timeout: Option<std::time::Duration>) -> SocketReply {
    request_from_socket_at(&socket_path(), json, timeout)
}

/// [`request_from_socket_inner`] against an explicit endpoint, rather than one
/// resolved from the environment. Lets the socket tests point a request at a
/// temp-dir stub-daemon socket without mutating the process-global
/// `DOT_AGENT_DECK_SOCKET` env var that production `request_from_socket_inner`
/// reads via `socket_path()` — `set_var`/`get_var` races on that var are
/// unsound under a multithreaded test binary regardless of the project's own
/// `STATE_DIR_ENV_LOCK` convention, since production `socket_path()` reads it
/// without taking that lock.
fn request_from_socket_at(
    path: &std::path::Path,
    json: &str,
    timeout: Option<std::time::Duration>,
) -> SocketReply {
    request_from_socket_at_detailed(path, json, timeout).0
}

/// [`request_from_socket_at`], plus the [`ReplyReadError`] behind a
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
) -> (SocketReply, Option<ReplyReadError>) {
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
            return (SocketReply::NoReply, Some(ReplyReadError::DeadlineExpired));
        };
        if stream.set_timeouts(remaining).is_err() {
            return (SocketReply::Unreachable, None);
        }
    }
    let msg = format!("{json}\n");
    if stream.write_all(msg.as_bytes()).is_err() || stream.flush().is_err() {
        return (SocketReply::Unreachable, None);
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
            tracing::debug!(reason = %err, "hook socket request read no reply line");
            (SocketReply::NoReply, Some(err))
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
            _extra: HashMap::new(),
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
            _extra: HashMap::new(),
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
            _extra: HashMap::new(),
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
            _extra: HashMap::new(),
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
            _extra: HashMap::new(),
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
    /// `request_from_socket` relies entirely on the daemon closing the
    /// connection and has no read/write bound of its own, so against this
    /// daemon it hangs forever. Run it on a worker thread and bound the wait
    /// with `recv_timeout` well above the production timeout the fix will add
    /// (5s), so a still-unbounded `request_from_socket` fails fast with a
    /// clear panic instead of hanging the CI runner until nextest's own
    /// timeout.
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
    /// line, waits a short delay well inside the coming timeout bound, then
    /// writes one JSON reply line. `request_from_socket` must still return
    /// that line as `Some(...)`. This guards against the specific way the
    /// idle-timeout fix could make things worse: a bound that fires too
    /// eagerly would mistake a merely-slow daemon for an absent one and
    /// silently fall back to PTY injection. This test is a correctness
    /// control, not a timing measurement, and is expected to pass both before
    /// and after the fix.
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
    /// per-read idle timeout added for `error/socket/003` resets on every
    /// byte received, so each dribbled byte restarts it before it can fire,
    /// and `request_from_socket` never returns even though the peer never
    /// goes silent for as long as one read. Run it on a worker thread and
    /// bound the wait with
    /// `recv_timeout` at a ceiling generous enough to hold whatever
    /// operation-level deadline the fix chooses, so a still-unbounded
    /// `request_from_socket` fails with a clear panic instead of hanging the
    /// CI runner.
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
        // exercises the "resets on every read" gap rather than racing it.
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
        // this does not pin the exact operation-level deadline the fix has
        // not chosen yet — it only needs to hold whatever sane deadline the
        // fix picks, while still failing well before it would hang CI's own
        // per-test timeout.
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
                _extra: extra,
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
            _extra: HashMap::new(),
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
            _extra: HashMap::new(),
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
            _extra: HashMap::new(),
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
            _extra: HashMap::new(),
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
            _extra: HashMap::new(),
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
            _extra: HashMap::new(),
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
        };
        let event = build_opencode_event(input).unwrap();
        assert_eq!(
            event.metadata.get("bash_command").map(String::as_str),
            Some(full_cmd),
        );
    }
}
