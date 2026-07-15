//! PRD #20 M6 — the generic stdout-wrapper integration strategy
//! (`dot-agent-deck wrap -- <agent-command> <args...>`).
//!
//! Some agents don't emit events natively — no hook, no plugin, no bundled
//! extension. For those, the deck wraps the agent's process: it spawns the
//! command, passes stdio through **transparently** so the agent stays fully
//! interactive for the user, and simultaneously **tees** the child's
//! stdout/stderr through a pattern-detection layer that maps recognised output
//! to [`AgentEvent`]s. Those events ride the **existing** raw-`AgentEvent` hook
//! socket ([`crate::hook::send_to_socket`]) — the same path the `agent-event`
//! CLI verb and native hooks use — so there is **no new wire** and no protocol
//! change (rule 12): the wrapper is just another `AgentEvent` producer.
//!
//! The pattern-detection seam is a small, data-driven [`RuleSet`] consulted by
//! the pure [`classify_line`] function, and a [`Detector`] state machine that
//! debounces repeated classifications into one event per state change. PRD #20
//! M7 proves the seam: Codex plugs in purely as **data** — the [`CODEX`]
//! [`RuleSet`], selected by [`ruleset_for`] off the resolved agent type —
//! without rewriting the wrapper runtime. This is the PRD's "open design dial":
//! how far pattern data lives in config vs. code is decided incrementally, and
//! the seam is deliberately just enough to make the generic case work and the
//! Codex case a data add.
//!
//! The [`IntegrationStrategy::Wrapper`](crate::agent_registry::IntegrationStrategy::Wrapper)
//! registry variant names this mechanism; Codex is its first consumer (M7).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::io::RawFd;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

use crate::agent_pty::{DOT_AGENT_DECK_AGENT_ID, DOT_AGENT_DECK_PANE_ID};
use crate::event::{
    AGENT_EVENT_SCHEMA_VERSION, AgentEvent, AgentType, EventType, LiveTarget, TargetKind, Writable,
};

/// A coarse activity state detected from a single line of wrapped output.
///
/// Deliberately minimal for the generic wrapper: the card only needs to know
/// whether the agent is working, has hit an error, or has gone quiet. Each
/// value maps to the [`EventType`] that drives the corresponding card status
/// (see [`DetectedEvent::event_type`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedEvent {
    /// The agent produced substantive output — it is actively working.
    Working,
    /// The line looks like an error / failure report.
    Error,
    /// The line signals the agent went quiet / finished a turn.
    Idle,
}

impl DetectedEvent {
    /// Map a detected activity state to the wire [`EventType`] that drives the
    /// dashboard card status.
    pub fn event_type(self) -> EventType {
        match self {
            DetectedEvent::Working => EventType::Thinking,
            DetectedEvent::Error => EventType::Error,
            DetectedEvent::Idle => EventType::Idle,
        }
    }
}

/// A data-driven set of line-classification rules — the pattern-detection seam.
///
/// The rules are plain data (case-insensitive substrings) so a new agent's
/// patterns are added as a new `RuleSet` value, not new control flow. The
/// generic wrapper ships [`GENERIC`]; the [`CODEX`] set lives alongside it and
/// [`ruleset_for`] selects between them by agent type, without touching
/// [`classify_line_with`] or the wrapper runtime.
pub struct RuleSet {
    /// Case-insensitive substrings that mark a line as an error/failure.
    /// Checked first, so an error line is never misread as generic activity.
    pub error_markers: &'static [&'static str],
    /// Case-insensitive substrings that mark a line as an explicit
    /// idle/completion signal. Empty for the generic set (which relies on
    /// process-exit quiescence instead); a per-agent set may populate it.
    pub idle_markers: &'static [&'static str],
}

/// The GENERIC, agent-agnostic rule set used when no agent-specific rules
/// apply. Basic on purpose (PRD "Risks": keep wrapper patterns simple with a
/// generic fallback): any non-blank line is activity, a few common failure
/// markers flip to error, and mid-session idleness is left to process-exit
/// quiescence rather than guessed from a single line.
pub static GENERIC: RuleSet = RuleSet {
    error_markers: &["error", "panic", "traceback", "exception", "fatal"],
    idle_markers: &[],
};

/// PRD #20 M7 — the Codex (`codex exec --json`) rule set.
///
/// Codex emits one compact JSON object per line on stdout (JSONL). Rather than
/// wait for process-exit quiescence like the generic set, we key card state off
/// the record's `type` discriminator: a `turn.completed` record ends the turn
/// (Idle) while the process is still alive, an `error` record is a failure, and
/// every other record (`turn.started`, `item.started` reasoning /
/// `command_execution`, …) is active work via the generic non-blank fallback.
/// Markers match the compact `"type":"…"` discriminator specifically so
/// incidental occurrences of the word "error" inside reasoning/command text
/// never flip the card. Selected by [`ruleset_for`] when the resolved agent is
/// [`AgentType::Codex`]; no change to [`classify_line_with`] or the runtime.
pub static CODEX: RuleSet = RuleSet {
    error_markers: &["\"type\":\"error\""],
    idle_markers: &["\"type\":\"turn.completed\""],
};

/// Select the line-classification [`RuleSet`] for a resolved agent type. Codex
/// gets its JSONL-aware [`CODEX`] rules; every other (or unknown) agent falls
/// back to the agent-agnostic [`GENERIC`] rules. This is the M7 seam that keeps
/// per-agent patterns as data — a new agent adds a `RuleSet` and an arm here,
/// not new runtime control flow.
fn ruleset_for(agent_type: &AgentType) -> &'static RuleSet {
    match agent_type {
        AgentType::Codex => &CODEX,
        _ => &GENERIC,
    }
}

/// Classify a single line of wrapped agent output using the [`GENERIC`] rules.
///
/// This is the pure, testable pattern-detection seam. `None` means "no state
/// change signalled by this line" (a blank/whitespace-only line) — the wrapper
/// still passes such lines through verbatim, it just emits no event for them.
pub fn classify_line(line: &str) -> Option<DetectedEvent> {
    classify_line_with(line, &GENERIC)
}

/// Classify a single line against an explicit [`RuleSet`]. [`classify_line`]
/// is the generic-ruleset shorthand; M7's Codex path calls this directly with
/// its own rules. Matching is case-insensitive substring containment.
pub fn classify_line_with(line: &str, rules: &RuleSet) -> Option<DetectedEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        // Blank line: pure whitespace / spacing. No state change.
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if rules.error_markers.iter().any(|m| lower.contains(m)) {
        return Some(DetectedEvent::Error);
    }
    if rules.idle_markers.iter().any(|m| lower.contains(m)) {
        return Some(DetectedEvent::Idle);
    }
    // Any other non-blank output is substantive activity.
    Some(DetectedEvent::Working)
}

/// Line-classification state machine that debounces a stream of classifications
/// into one event per *state change*.
///
/// A working agent emits many output lines; without debouncing the wrapper
/// would flood the daemon with identical `Working` events. `Detector` remembers
/// the last emitted state and yields `Some` only when the classification
/// changes, so a burst of activity lines produces exactly one `Working`
/// transition. A single `Detector` is shared across the stdout and stderr tees
/// so the card reflects one coherent session state.
pub struct Detector {
    rules: &'static RuleSet,
    last: Option<DetectedEvent>,
}

impl Detector {
    /// A detector using the [`GENERIC`] rules.
    pub fn new() -> Self {
        Self::with_rules(&GENERIC)
    }

    /// A detector using an explicit rule set (the M7 Codex seam).
    pub fn with_rules(rules: &'static RuleSet) -> Self {
        Self { rules, last: None }
    }

    /// Feed one line; return the event to emit, or `None` when the line is
    /// blank (no classification) or does not change the detected state.
    pub fn observe(&mut self, line: &str) -> Option<DetectedEvent> {
        self.observe_detected(classify_line_with(line, self.rules))
    }

    /// Debounce an already-classified event. The JSON-aware Codex path
    /// ([`classify_codex_line`]) classifies the line itself and feeds the
    /// result here so it shares the same one-event-per-state-change debouncing
    /// as the generic substring path. `None` (blank / unclassifiable line)
    /// never changes state.
    pub fn observe_detected(&mut self, detected: Option<DetectedEvent>) -> Option<DetectedEvent> {
        let detected = detected?;
        if self.last == Some(detected) {
            None
        } else {
            self.last = Some(detected);
            Some(detected)
        }
    }
}

/// PRD #20 finding #11: classify one line of Codex output. Codex emits JSONL
/// (`codex exec --json` writes one compact JSON object per line) and the
/// interactive `codex` TUI mixes JSON events with plain redraw text. Parse the
/// top-level `type` discriminator with `serde_json` (robust to insignificant
/// whitespace and field reordering, unlike a raw substring match), mapping:
/// `turn.completed` → `Idle`, `turn.failed` / `error` → `Error`, and every
/// other record (`turn.started`, `item.started` reasoning / command execution,
/// …) → `Working`. A non-JSON line (the interactive channel's plain text)
/// falls back to the substring [`CODEX`] rules, so bare `codex` still surfaces
/// activity instead of staying stuck until process exit.
pub fn classify_codex_line(line: &str) -> Option<DetectedEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(kind) = value.get("type").and_then(|t| t.as_str())
    {
        return Some(match kind {
            "turn.completed" | "task.completed" => DetectedEvent::Idle,
            "turn.failed" | "task.failed" | "error" => DetectedEvent::Error,
            _ => DetectedEvent::Working,
        });
    }
    classify_line_with(trimmed, &CODEX)
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}

/// Carries the fixed identity of a wrapped session and emits [`AgentEvent`]s for
/// it over the existing hook socket. Cheap to `clone` the `Arc` into each tee
/// thread.
struct Emitter {
    agent_type: AgentType,
    session_id: String,
    pane_id: Option<String>,
    agent_id: Option<String>,
    cwd: Option<String>,
    /// PRD #20 M3: the live-target descriptor every event this wrapper emits
    /// carries. A wrapped session is the first place the live/history-only
    /// distinction bites: the child's stdin is the user's inherited terminal,
    /// not a daemon-controlled PTY, so the *dashboard* has no live write target
    /// — the session is `history-only`. Stamped on the card so a wrapped Codex
    /// pane renders view-only and refuses live input (M4).
    live_target: LiveTarget,
}

impl Emitter {
    /// Build an [`AgentEvent`] for `event_type` and send it to the daemon over
    /// the existing raw-`AgentEvent` hook socket. Send failures are ignored so
    /// the wrapper stays a transparent passthrough even with no daemon (the
    /// "arbitrary commands as a basic fallback" success criterion).
    fn emit(&self, event_type: EventType) {
        let event = AgentEvent {
            session_id: self.session_id.clone(),
            agent_type: self.agent_type.clone(),
            event_type,
            tool_name: None,
            tool_detail: None,
            cwd: self.cwd.clone(),
            timestamp: Utc::now(),
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: self.pane_id.clone(),
            agent_id: self.agent_id.clone(),
            agent_version: None,
            // PRD #20 M6: stamp the schema version the wrapper writes.
            schema_version: Some(AGENT_EVENT_SCHEMA_VERSION),
            // PRD #20 M3: a wrapped session is history-only from the dashboard's
            // perspective (see `Emitter::live_target`).
            live_target: Some(self.live_target),
        };
        if let Ok(json) = serde_json::to_string(&event) {
            let _ = crate::hook::send_to_socket(&json);
        }
    }
}

/// PRD #20 finding #13: cap the retained classification buffer. A newline-free
/// stream (or a CR-only progress redraw an arbitrary producer emits) must not
/// grow memory without bound — beyond this the accumulated bytes are classified
/// and flushed as one line. 64 KiB is far above any real single output line.
const MAX_CLASSIFY_LINE: usize = 64 * 1024;

/// Pump bytes from `reader` to `writer` **verbatim** (transparent passthrough),
/// while feeding each completed line to `on_line` for classification.
///
/// Reads in chunks and writes+flushes immediately, so a prompt printed without
/// a trailing newline (e.g. `Enter your name: `) still reaches the user at once
/// — line-oriented buffering would stall interactivity. Line accumulation for
/// classification happens on `\n` OR `\r` (a PTY child's cooked output arrives
/// as `\r\n`, and TUIs redraw with a bare `\r`; empty segments between the two
/// are skipped so `\r\n` yields one clean line). A trailing partial line is
/// classified when the stream ends. Bytes that don't form valid UTF-8 are
/// passed through but skipped for classification.
///
/// PRD #20 finding #13: the retained line buffer is BOUNDED
/// ([`MAX_CLASSIFY_LINE`]), and a parent-output write/flush error (e.g. `EPIPE`
/// from an early-closing consumer) STOPS the tee so the caller can terminate
/// the child side rather than draining forever.
fn tee<R: Read, W: Write>(mut reader: R, mut writer: W, mut on_line: impl FnMut(&str)) {
    let mut buf = [0u8; 8192];
    let mut line: Vec<u8> = Vec::new();
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                // Transparent passthrough first — the user must see output with
                // minimal latency regardless of classification. On a write/flush
                // failure (broken pipe), stop: the downstream is gone.
                if writer.write_all(chunk).is_err() || writer.flush().is_err() {
                    break;
                }
                for &b in chunk {
                    if b == b'\n' || b == b'\r' {
                        if !line.is_empty() {
                            if let Ok(s) = std::str::from_utf8(&line) {
                                on_line(s);
                            }
                            line.clear();
                        }
                    } else {
                        line.push(b);
                        if line.len() >= MAX_CLASSIFY_LINE {
                            if let Ok(s) = std::str::from_utf8(&line) {
                                on_line(s);
                            }
                            line.clear();
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
    if !line.is_empty()
        && let Ok(s) = std::str::from_utf8(&line)
    {
        on_line(s);
    }
}

/// PRD #20 M8 — rewrite a bare launch command into its `dot-agent-deck wrap`
/// invocation when the resolved agent uses the
/// [`IntegrationStrategy::Wrapper`](crate::agent_registry::IntegrationStrategy::Wrapper)
/// mechanism, so the agent's stdout is monitored transparently.
///
/// Called at the TUI new-agent spawn site: a Wrapper-strategy agent (Codex now;
/// Gemini later) launches as
/// `dot-agent-deck wrap --agent <registry-basename> -- <command>` while its
/// Command field, `last_command`, and persisted metadata keep the bare base
/// command. `--agent <basename>` pins the identity through the registry
/// ([`resolve_agent_type`]) so events attribute to the right agent even when the
/// wrapped binary is a path or alias.
///
/// Idempotent: a command that is already a `dot-agent-deck wrap` invocation is
/// returned unchanged (never double-wrapped), so restoring an already-bare saved
/// command re-wraps exactly once. Non-Wrapper agents (and the neutral unknown
/// type) are returned unchanged.
pub fn wrap_launch_command(command: &str, agent_type: &AgentType) -> String {
    let spec = crate::agent_registry::spec(agent_type);
    if spec.strategy != Some(crate::agent_registry::IntegrationStrategy::Wrapper)
        || is_wrap_invocation(command)
    {
        return command.to_string();
    }
    // Prefer the registry detection basename (the stable `--agent` alias the
    // wrapper resolves back through `detect_from_basename`); fall back to the
    // label only if an entry somehow ships without one.
    let name = spec.detect_basenames.first().copied().unwrap_or(spec.label);
    format!("dot-agent-deck wrap --agent {name} -- {command}")
}

/// Whether `command` is already a `dot-agent-deck wrap …` invocation — the
/// idempotency guard for [`wrap_launch_command`]. Tolerant of a leading path on
/// the binary (`/usr/local/bin/dot-agent-deck wrap …`).
fn is_wrap_invocation(command: &str) -> bool {
    let mut tokens = command.split_whitespace();
    match (tokens.next(), tokens.next()) {
        (Some(program), Some(subcommand)) => {
            std::path::Path::new(program)
                .file_name()
                .and_then(|s| s.to_str())
                == Some("dot-agent-deck")
                && subcommand == "wrap"
        }
        _ => false,
    }
}

/// Resolve the agent identity emitted events should carry.
///
/// An explicit `--agent` override wins and is resolved through the registry, so
/// a name the registry doesn't know yet becomes the neutral
/// [`AgentType::None`] rather than a guess. Otherwise the type is inferred from
/// the wrapped binary exactly like the TUI spawn sites
/// ([`AgentType::from_command`]). Either way, with Codex in the registry (M7),
/// `wrap -- codex` (or `--agent codex`) resolves to it and [`ruleset_for`]
/// selects the [`CODEX`] rules — with no change here.
fn resolve_agent_type(agent_override: Option<&str>, program: &str) -> AgentType {
    if let Some(name) = agent_override {
        return crate::agent_registry::detect_from_basename(name).unwrap_or(AgentType::None);
    }
    AgentType::from_command(Some(program)).unwrap_or(AgentType::None)
}

/// Derive the session id events are grouped under. When run inside a managed
/// pane it mirrors the `agent-event` verb's `{pane_id}-session` convention so
/// events land on the pane's card; standalone (no pane) it derives a stable id
/// from the wrapped binary's basename.
fn session_id_for(pane_id: Option<&str>, program: &str) -> String {
    match pane_id {
        Some(p) => format!("{p}-session"),
        None => {
            let base = std::path::Path::new(program)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(program);
            format!("wrap-{base}")
        }
    }
}

/// A `Write` that writes UNBUFFERED to a raw fd (the outer stdout), so the
/// child's output passes through with minimal latency and no line buffering.
struct FdWriter(RawFd);

impl Write for FdWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = unsafe { libc::write(self.0, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Read the current window size of the terminal on `fd` via `TIOCGWINSZ`.
/// `None` when `fd` isn't a terminal or the ioctl fails / reports a zero size.
fn terminal_size(fd: RawFd) -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
        Some((ws.ws_row, ws.ws_col))
    } else {
        None
    }
}

/// RAII guard that puts the outer terminal (`fd`) into raw mode for the wrap
/// session and restores the original attributes on drop. Raw mode is required
/// so keystrokes — including `Ctrl+C` (INTR) and `\r` — reach the wrapper as
/// bytes and are forwarded to the INNER PTY, whose own line discipline turns
/// `Ctrl+C` into `SIGINT` for the child and maps `\r`→`\n` for canonical reads.
/// A no-op (and harmless) when `fd` is not a terminal (e.g. piped stdin).
struct RawModeGuard {
    fd: RawFd,
    original: Option<libc::termios>,
}

impl RawModeGuard {
    fn enable(fd: RawFd) -> Self {
        if unsafe { libc::isatty(fd) } != 1 {
            return Self { fd, original: None };
        }
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
            return Self { fd, original: None };
        }
        let original = termios;
        unsafe {
            libc::cfmakeraw(&mut termios);
            libc::tcsetattr(fd, libc::TCSANOW, &termios);
        }
        Self {
            fd,
            original: Some(original),
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Some(orig) = self.original {
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, &orig);
            }
        }
    }
}

/// Entry point for `dot-agent-deck wrap [--agent <name>] -- <command> <args...>`.
///
/// PRD #20 blocker-1: spawns `command` on a fresh INNER pseudo-terminal and
/// proxies all three streams, so the child sees `isatty(0/1/2) == true` and an
/// interactive agent (bare `codex`) keeps its full TUI. The inner PTY's master
/// output is tee'd through pattern detection into `AgentEvent`s (the child's
/// terminal is never replaced with pipes), the outer terminal is put in raw
/// mode so keystrokes / `Ctrl+C` pass through, and outer resizes are forwarded
/// to the inner PTY (so the child receives `SIGWINCH`). Returns the child's
/// exit code as the wrapper's exit code.
pub fn run_wrap(agent_override: Option<&str>, command: &[String]) -> ExitCode {
    let Some((program, args)) = command.split_first() else {
        eprintln!(
            "Error: `wrap` requires a command after `--`, e.g. `dot-agent-deck wrap -- codex`."
        );
        return ExitCode::FAILURE;
    };

    let agent_type = resolve_agent_type(agent_override, program);
    let pane_id = std::env::var(DOT_AGENT_DECK_PANE_ID).ok();
    // Optional — the daemon injects this on spawn (same pattern as the hook /
    // agent-event paths); a standalone wrap has none.
    let agent_id = std::env::var(DOT_AGENT_DECK_AGENT_ID).ok();
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from));
    let session_id = session_id_for(pane_id.as_deref(), program);

    // PRD #20 blocker-2: a wrapper running INSIDE a daemon-managed pane
    // (`DOT_AGENT_DECK_PANE_ID` set) is backed by that live PTY — the daemon's
    // dashboard writes reach the child through the pane PTY → this wrapper's
    // stdin → the inner PTY — so it declares `Pty`/`Live`. A STANDALONE wrap has
    // no deck-controlled target (the child's terminal is the user's own), so it
    // stays history-only.
    let live_target = if pane_id.is_some() {
        LiveTarget {
            kind: TargetKind::Pty,
            writable: Writable::Live,
        }
    } else {
        LiveTarget {
            kind: TargetKind::Process,
            writable: Writable::HistoryOnly,
        }
    };

    let emitter = Arc::new(Emitter {
        agent_type,
        session_id,
        pane_id,
        agent_id,
        cwd,
        live_target,
    });

    // Open the inner PTY at the outer terminal's current size so the child's
    // first frame paints at the right geometry (falls back to 24×80 when the
    // outer stream isn't a terminal).
    let (rows, cols) = terminal_size(libc::STDIN_FILENO)
        .or_else(|| terminal_size(libc::STDOUT_FILENO))
        .unwrap_or((24, 80));
    let pty_system = NativePtySystem::default();
    let pair = match pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: failed to open a pseudo-terminal for `{program}`: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Build the child command. `CommandBuilder` inherits the wrapper's env (which
    // carries `DOT_AGENT_DECK_PANE_ID` / `_AGENT_ID` injected by the daemon), so
    // the child's own hooks and this wrapper's events attribute to the same pane.
    let mut cmd = CommandBuilder::new(program);
    for arg in args {
        cmd.arg(arg);
    }
    // Pin the child's cwd to the wrapper's own working directory. Unlike
    // `std::process::Command`, `portable_pty`'s `CommandBuilder` does NOT
    // resolve a relative program (e.g. `./tty-probe.sh`) against the process
    // cwd when none is set — it would fail to find it — so set it explicitly.
    if let Ok(dir) = std::env::current_dir() {
        cmd.cwd(dir);
    }

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: failed to spawn `{program}`: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Interact through the master side only; the child holds its own slave fds.
    drop(pair.slave);

    let master = pair.master;
    let reader = match master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: failed to read the wrapped `{program}` terminal: {e}");
            let _ = child.kill();
            return ExitCode::FAILURE;
        }
    };
    let writer = match master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("Error: failed to write the wrapped `{program}` terminal: {e}");
            let _ = child.kill();
            return ExitCode::FAILURE;
        }
    };

    // The session has begun — surface the card immediately.
    emitter.emit(EventType::SessionStart);

    // Raw-mode the outer terminal so keystrokes (incl. Ctrl+C and CR) pass
    // through to the inner PTY unmodified; restored on drop.
    let _raw_guard = RawModeGuard::enable(libc::STDIN_FILENO);

    // Output pump (inner master → outer stdout, tee'd through classification).
    // PRD #20 M7: the rule set is keyed off the resolved agent type; Codex uses
    // JSON-aware classification, any other command keeps the generic fallback.
    // Auditor: recover from a poisoned detector mutex instead of panicking.
    let is_codex = emitter.agent_type == AgentType::Codex;
    let detector = Arc::new(Mutex::new(Detector::with_rules(ruleset_for(
        &emitter.agent_type,
    ))));
    let output_thread = {
        let emitter = Arc::clone(&emitter);
        let detector = Arc::clone(&detector);
        std::thread::spawn(move || {
            tee(reader, FdWriter(libc::STDOUT_FILENO), |line| {
                let mut det = detector.lock().unwrap_or_else(|p| p.into_inner());
                let ev = if is_codex {
                    det.observe_detected(classify_codex_line(line))
                } else {
                    det.observe(line)
                };
                drop(det);
                if let Some(ev) = ev {
                    emitter.emit(ev.event_type());
                }
            });
        })
    };

    // Input pump (outer stdin → inner master). Detached: on child exit the main
    // loop below returns and the process exits, reaping this blocked reader.
    {
        let mut writer = writer;
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                let n = unsafe {
                    libc::read(
                        libc::STDIN_FILENO,
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n <= 0 {
                    break;
                }
                if writer.write_all(&buf[..n as usize]).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });
    }

    // Main loop: forward outer resizes to the inner PTY (so the child receives
    // SIGWINCH) and poll for child exit. Keeping `master` here avoids sharing a
    // non-Send/Sync handle across threads. `try_wait` reaps the child once it
    // returns `Some`, so its status is captured and reused (no second wait).
    let mut last_size = (rows, cols);
    let status = loop {
        if let Some(size) = terminal_size(libc::STDIN_FILENO)
            && size != last_size
        {
            let _ = master.resize(PtySize {
                rows: size.0,
                cols: size.1,
                pixel_width: 0,
                pixel_height: 0,
            });
            last_size = size;
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(e) => {
                eprintln!("Error: failed to wait on wrapped `{program}`: {e}");
                let _ = child.kill();
                break None;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    // Drain the output tee so no trailing output is lost before the final event.
    let _ = output_thread.join();

    // PRD #20 finding #14: emit Idle only after a SUCCESSFUL exit; a nonzero /
    // signalled failure (or a wait error) ends as a visible Error rather than a
    // false idle card. Preserve the child's numeric exit code as the wrapper's
    // own (truncated to a byte, as shells do); a wait failure maps to 1.
    let (success, code) = match status {
        Some(s) => (s.success(), s.exit_code() as u8),
        None => (false, 1),
    };
    emitter.emit(if success {
        EventType::Idle
    } else {
        EventType::Error
    });
    ExitCode::from(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure-data pattern-detection tests — plain `#[test]` unit tests (no
    // `#[spec]` / CATALOG reproducer needed: these assert a pure function, not
    // runtime TUI behaviour).

    /// A normal, substantive output line classifies as `Working`.
    #[test]
    fn normal_line_is_working() {
        assert_eq!(
            classify_line("Reading src/main.rs"),
            Some(DetectedEvent::Working)
        );
        assert_eq!(
            classify_line("  running `cargo build`"),
            Some(DetectedEvent::Working)
        );
    }

    /// Lines carrying a common failure marker classify as `Error`, regardless
    /// of case, and even when other text surrounds the marker.
    #[test]
    fn error_looking_lines_are_error() {
        assert_eq!(
            classify_line("error: cannot find value `x`"),
            Some(DetectedEvent::Error)
        );
        assert_eq!(
            classify_line("ERROR something broke"),
            Some(DetectedEvent::Error)
        );
        assert_eq!(
            classify_line("thread 'main' panicked at 'boom'"),
            Some(DetectedEvent::Error)
        );
        assert_eq!(
            classify_line("Traceback (most recent call last):"),
            Some(DetectedEvent::Error)
        );
        assert_eq!(
            classify_line("fatal: not a git repository"),
            Some(DetectedEvent::Error)
        );
    }

    /// Blank / whitespace-only lines signal no state change (`None`) — the
    /// wrapper still passes them through, it just emits no event.
    #[test]
    fn blank_lines_are_no_event() {
        assert_eq!(classify_line(""), None);
        assert_eq!(classify_line("   "), None);
        assert_eq!(classify_line("\t  \t"), None);
    }

    /// The error check wins over the generic activity fallback: a line that is
    /// non-blank AND contains an error marker is `Error`, not `Working`.
    #[test]
    fn error_marker_beats_generic_activity() {
        assert_eq!(
            classify_line("compiling: encountered an exception in module"),
            Some(DetectedEvent::Error)
        );
    }

    /// An explicit rule set drives classification: an idle marker maps to
    /// `Idle`, proving the M7 seam works without touching the generic path.
    #[test]
    fn explicit_ruleset_detects_idle_marker() {
        static CUSTOM: RuleSet = RuleSet {
            error_markers: &["boom"],
            idle_markers: &["done", "waiting for input"],
        };
        assert_eq!(
            classify_line_with("Task done", &CUSTOM),
            Some(DetectedEvent::Idle)
        );
        assert_eq!(
            classify_line_with("waiting for input", &CUSTOM),
            Some(DetectedEvent::Idle)
        );
        assert_eq!(
            classify_line_with("boom happened", &CUSTOM),
            Some(DetectedEvent::Error)
        );
        assert_eq!(
            classify_line_with("just chugging along", &CUSTOM),
            Some(DetectedEvent::Working)
        );
    }

    /// Each detected state maps to the wire `EventType` that drives the card.
    #[test]
    fn detected_event_maps_to_event_type() {
        assert_eq!(DetectedEvent::Working.event_type(), EventType::Thinking);
        assert_eq!(DetectedEvent::Error.event_type(), EventType::Error);
        assert_eq!(DetectedEvent::Idle.event_type(), EventType::Idle);
    }

    /// The detector debounces: a burst of activity lines yields exactly one
    /// `Working` transition, an error line flips to `Error`, and a return to
    /// activity flips back — blank lines never change state.
    #[test]
    fn detector_emits_only_on_state_change() {
        let mut d = Detector::new();
        assert_eq!(d.observe("doing work"), Some(DetectedEvent::Working));
        // Repeated activity: no new event.
        assert_eq!(d.observe("more work"), None);
        assert_eq!(d.observe(""), None); // blank: no classification
        // Transition to error.
        assert_eq!(d.observe("error: nope"), Some(DetectedEvent::Error));
        assert_eq!(d.observe("error again"), None);
        // Back to activity.
        assert_eq!(
            d.observe("recovered, continuing"),
            Some(DetectedEvent::Working)
        );
    }

    /// `tee` passes bytes through verbatim (including a trailing newline-less
    /// prompt) and classifies each completed line plus a trailing partial line.
    #[test]
    fn tee_passes_through_and_classifies_lines() {
        let input = b"line one\nerror: boom\nEnter name: ";
        let mut out: Vec<u8> = Vec::new();
        let mut lines: Vec<String> = Vec::new();
        tee(&input[..], &mut out, |l| lines.push(l.to_string()));
        // Everything the child wrote reached the writer unchanged.
        assert_eq!(out, input);
        // Two full lines plus the trailing partial prompt were classified.
        assert_eq!(lines, vec!["line one", "error: boom", "Enter name: "]);
    }

    /// Agent identity resolution: an explicit override resolves through the
    /// registry; otherwise it is inferred from the wrapped binary. An
    /// unrecognized command (the generic fallback) yields the neutral `None`.
    #[test]
    fn resolve_agent_type_override_and_inference() {
        // Override wins, resolved via the registry.
        assert_eq!(
            resolve_agent_type(Some("claude"), "somethingelse"),
            AgentType::ClaudeCode
        );
        // No override: inferred from the wrapped binary (path-tolerant).
        assert_eq!(
            resolve_agent_type(None, "/usr/local/bin/opencode"),
            AgentType::OpenCode
        );
        // Unknown command → neutral None (generic fallback, still passes through).
        assert_eq!(resolve_agent_type(None, "cat"), AgentType::None);
        // Unknown override name → neutral None rather than a guess.
        assert_eq!(resolve_agent_type(Some("nope"), "cat"), AgentType::None);
    }

    /// Session id mirrors the `agent-event` `{pane_id}-session` convention in a
    /// managed pane, and derives a stable basename id when standalone.
    #[test]
    fn session_id_derivation() {
        assert_eq!(session_id_for(Some("pane-7"), "codex"), "pane-7-session");
        assert_eq!(session_id_for(None, "/usr/bin/codex"), "wrap-codex");
    }

    /// PRD #20 M8: a Wrapper-strategy agent's bare command is rewritten to its
    /// `dot-agent-deck wrap --agent <basename> -- <command>` invocation, using
    /// the registry detection basename as the `--agent` alias.
    #[test]
    fn wrap_launch_command_wraps_wrapper_strategy() {
        assert_eq!(
            wrap_launch_command("codex", &AgentType::Codex),
            "dot-agent-deck wrap --agent codex -- codex"
        );
    }

    /// Non-Wrapper agents (and the neutral unknown type) launch bare — the
    /// transform only fires for the Wrapper strategy.
    #[test]
    fn wrap_launch_command_leaves_non_wrapper_agents_bare() {
        assert_eq!(
            wrap_launch_command("claude", &AgentType::ClaudeCode),
            "claude"
        );
        assert_eq!(
            wrap_launch_command("opencode", &AgentType::OpenCode),
            "opencode"
        );
        assert_eq!(wrap_launch_command("pi", &AgentType::Pi), "pi");
        assert_eq!(wrap_launch_command("cat", &AgentType::None), "cat");
    }

    /// Idempotent: a command that is already a `dot-agent-deck wrap …`
    /// invocation is returned unchanged, even with a leading binary path, so a
    /// restore never double-wraps.
    #[test]
    fn wrap_launch_command_is_idempotent() {
        assert_eq!(
            wrap_launch_command(
                "dot-agent-deck wrap --agent codex -- codex",
                &AgentType::Codex
            ),
            "dot-agent-deck wrap --agent codex -- codex"
        );
        assert_eq!(
            wrap_launch_command(
                "/usr/local/bin/dot-agent-deck wrap --agent codex -- codex",
                &AgentType::Codex
            ),
            "/usr/local/bin/dot-agent-deck wrap --agent codex -- codex"
        );
    }

    /// The idempotency guard recognises a `dot-agent-deck wrap` invocation (with
    /// or without a leading path) and rejects anything else.
    #[test]
    fn is_wrap_invocation_matches_only_wrap() {
        assert!(is_wrap_invocation(
            "dot-agent-deck wrap --agent codex -- codex"
        ));
        assert!(is_wrap_invocation("/opt/bin/dot-agent-deck wrap -- codex"));
        assert!(!is_wrap_invocation("codex"));
        assert!(!is_wrap_invocation("dot-agent-deck daemon serve"));
        assert!(!is_wrap_invocation(""));
    }
}
