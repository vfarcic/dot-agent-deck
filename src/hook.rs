use std::collections::HashMap;
use std::io::Read as _;
use std::io::Write as _;
use std::process::ExitCode;

use chrono::Utc;
use serde::Deserialize;
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
    #[serde(flatten)]
    _extra: HashMap<String, Value>,
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

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
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

    let user_prompt = prompt.map(|p| truncate(&p, 200));
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
    let user_prompt = input.prompt.map(|p| truncate(&p, 200));
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
/// `DELEGATE_REPLY_TIMEOUT` (5s, `src/main.rs`). A caller waiting on
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
/// single idle read: [`request_from_socket_inner`]'s read loop measures
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
/// it, but got no confirmation back" (as the not-yet-submitted `delegate`
/// wiring will) can report each honestly instead of collapsing both to
/// `None` the way [`request_from_socket`] does.
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
    NoReply,
    /// Could not connect to the daemon, or failed while writing — the
    /// request was never sent.
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
    // The total-operation deadline starts here, before connect, rather than
    // being re-armed with a fresh full budget once the connection is
    // established and the request written below. Connect and the write are
    // usually fast on a local Unix/named-pipe socket, but a wedged/overloaded
    // daemon can stall either one, and this is the only way the deadline
    // actually bounds the *whole* exchange the way this function's/
    // `request_from_socket`'s docs describe. The timeout value itself is
    // unchanged — only when the clock starts.
    let deadline = timeout.map(|budget| std::time::Instant::now() + budget);
    let mut stream = match crate::platform::ipc::IpcClient::connect(path) {
        Ok(stream) => stream,
        Err(_) => return SocketReply::Unreachable,
    };
    if let Some(deadline) = deadline {
        // Zero/negative remaining budget: do not hand the socket a zero
        // timeout — on some platforms that means "block forever", the
        // opposite of what an exhausted deadline should do (same guard
        // `read_reply_line` applies per-read below). A deadline that expired
        // during connect folds into `NoReply`, matching how the read loop
        // treats a deadline that expires mid-operation.
        let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return SocketReply::NoReply;
        };
        if stream.set_timeouts(remaining).is_err() {
            return SocketReply::Unreachable;
        }
    }
    let msg = format!("{json}\n");
    if stream.write_all(msg.as_bytes()).is_err() || stream.flush().is_err() {
        return SocketReply::Unreachable;
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
        Some(line) => SocketReply::Line(line),
        None => SocketReply::NoReply,
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
/// Once the deadline has passed, any read fails, the peer closes before
/// sending a single byte, or the line is not valid UTF-8, this returns
/// `None`; the caller folds that into `SocketReply::NoReply` exactly as it
/// already did for a timed-out read — see [`request_from_socket`]. A peer
/// that closes *after* writing part of a line, but before the newline, is a
/// distinct case: the partial line is still returned as `Some(partial)`, not
/// folded into `None`.
///
/// `deadline` is computed once by the caller — [`request_from_socket_at`] —
/// from a point **before** it connects, not re-derived here from a fresh
/// `Instant::now()`: doing the latter would let connect and the request write
/// each consume wall-clock time against nothing, so only the read phase would
/// actually be bounded and the exchange as a whole could run past what the
/// caller's `timeout` advertises.
///
/// `None` falls back to unbounded blocking reads with no re-arming, for
/// callers that pass no deadline at all.
fn read_reply_line(
    stream: &mut crate::platform::ipc::IpcClient,
    deadline: Option<std::time::Instant>,
) -> Option<String> {
    let mut buf = [0u8; 512];
    let mut line = Vec::new();
    loop {
        if let Some(deadline) = deadline {
            let remaining = deadline.checked_duration_since(std::time::Instant::now())?;
            stream.set_timeouts(remaining).ok()?;
        }
        let n = stream.read(&mut buf).ok()?;
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
                return None;
            }
            break;
        }
        if let Some(newline_pos) = buf[..n].iter().position(|&b| b == b'\n') {
            line.extend_from_slice(&buf[..newline_pos]);
            break;
        }
        line.extend_from_slice(&buf[..n]);
    }
    String::from_utf8(line)
        .ok()
        .map(|s| s.trim_end_matches('\r').to_string())
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
            _extra: HashMap::new(),
        };
        let event = build_event(input).unwrap();
        let prompt = event.user_prompt.unwrap();
        assert!(prompt.len() <= 204); // 200 + "…" (3 bytes)
        assert!(prompt.ends_with('…'));
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
        let _tmp = tempfile::tempdir().expect("create temp dir for stub daemon socket");
        let socket_path = _tmp.path().join("s.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind stub daemon socket");

        // Stub daemon: read the request line, wait a short delay comfortably
        // inside the 5s bound the fix will add, then reply with one line.
        let daemon_thread = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut reader = std::io::BufReader::new(&stream);
                let mut line = String::new();
                let _ = std::io::BufRead::read_line(&mut reader, &mut line);
                std::thread::sleep(std::time::Duration::from_millis(300));
                let _ = std::io::Write::write_all(&mut stream, br#"{"seed":"abc123"}"#);
                let _ = std::io::Write::write_all(&mut stream, b"\n");
                let _ = std::io::Write::flush(&mut stream);
            }
        });

        let result = match request_from_socket_at(
            &socket_path,
            r#"{"type":"get-seed"}"#,
            Some(GET_SEED_REQUEST_TIMEOUT),
        ) {
            SocketReply::Line(line) => Some(line),
            SocketReply::NoReply | SocketReply::Unreachable => None,
        };

        let _ = daemon_thread.join();

        assert_eq!(
            result,
            Some(r#"{"seed":"abc123"}"#.to_string()),
            "a daemon that replies well inside the timeout bound must not be mistaken for \
             an absent one"
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
