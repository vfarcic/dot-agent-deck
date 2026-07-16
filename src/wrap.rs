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
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command as StdCommand, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;

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
            // R20-002: with catchable-signal handlers installed (no SA_RESTART),
            // a blocked read can return `Interrupted`; retry rather than treat it
            // as end-of-stream.
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
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

/// The `--dangerously-bypass-hook-trust` flag the deck passes when it launches a
/// wrapped `codex`. Codex requires non-managed command hooks to be trusted before
/// they run; the deck authors its OWN hook definition (see
/// [`crate::codex_hooks_manage`]), so it vets the source — itself.
///
/// PRD #20 W3-Pass-2 (finding #2 / M-1): this flag is INVOCATION-GLOBAL — it
/// trusts EVERY enabled hook in the active `CODEX_HOME`, not only the deck's. So
/// the deck injects it ONLY when the target `CODEX_HOME` contains no non-deck
/// command hooks (see [`crate::codex_hooks_manage::foreign_command_hooks_present`]),
/// i.e. when the only thing the bypass would trust is the deck's own vetted entry.
/// If a third-party hook is present the deck does NOT bypass, so a user's
/// unreviewed hook is never silently trusted.
const CODEX_BYPASS_HOOK_TRUST_FLAG: &str = "--dangerously-bypass-hook-trust";

/// Whether the wrapped program's basename is `codex`. PRD #20 W1 keys the
/// trust-flag injection off the ACTUAL program, not the resolved agent identity:
/// `wrap --agent codex -- /bin/sh` (the non-interactive I/O test) carries Codex
/// identity but launches a shell, which must NOT receive a codex-only flag; and
/// a launcher/script (`devbox run …`, `run_codex_agent.sh`) is not `codex`, so
/// the flag can't be auto-injected into its eventual codex call (documented in
/// `docs/develop/agent-adapters.md`).
fn program_is_codex(program: &str) -> bool {
    std::path::Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        == Some("codex")
}

/// PRD #20 W1 spawn wiring. Decides, for this wrap invocation, whether to
/// install the deck's native Codex hooks and whether to inject the hook-trust
/// bypass flag.
///
/// - **Hooks install** fires whenever a Codex-identity agent will actually run
///   under this wrapper: a direct `codex` program, OR a deck-spawned pane
///   (`pane_id` present) whose declared identity is Codex. The latter covers a
///   launcher/wrapper SCRIPT the deck spawned but whose argv we can't reach
///   into — the hooks are `CODEX_HOME`-scoped, so they apply however codex is
///   ultimately launched. It deliberately does NOT fire for a standalone
///   `wrap --agent codex -- /bin/sh` (no pane, non-codex program), so a
///   non-interactive I/O wrap never writes to the user's `~/.codex`.
/// - **The trust-bypass flag** is injected ONLY when ALL of the following hold,
///   so it can never silently trust something the deck didn't author:
///   1. the program is a direct `codex` (the one case where the wrapper controls
///      codex's argv — a launcher/script must add the flag to its own `codex …`
///      line, see the module docs and `docs/develop/agent-adapters.md`), AND
///   2. the resolved identity is [`AgentType::Codex`] — so
///      `wrap --agent claude -- codex` neither installs Codex hooks nor bypasses
///      trust (finding #2), AND
///   3. the active `CODEX_HOME` contains no non-deck command hooks
///      ([`crate::codex_hooks_manage::foreign_command_hooks_present`]) — because
///      the bypass is invocation-global, so with a third-party hook present it
///      would trust the user's unreviewed hook too (finding #2 / M-1). When a
///      foreign hook is present the deck skips the bypass and warns; its own
///      events then degrade to stdout classification.
///
/// Returns `true` when the caller should add [`CODEX_BYPASS_HOOK_TRUST_FLAG`].
fn codex_spawn_prep(program: &str, agent_type: &AgentType, pane_id: Option<&str>) -> bool {
    let program_codex = program_is_codex(program);
    let codex_identity = *agent_type == AgentType::Codex;
    if codex_identity && (program_codex || pane_id.is_some()) {
        crate::codex_hooks_manage::auto_install();
    }
    // Both a direct `codex` program AND Codex identity are required before the
    // wrapper will even consider the invocation-global bypass.
    if !(program_codex && codex_identity) {
        return false;
    }
    // Only bypass when nothing but the deck's own vetted hook would be trusted.
    match crate::codex_hooks_manage::foreign_command_hooks_present() {
        Ok(false) => true,
        Ok(true) => {
            tracing::warn!(
                "codex: third-party command hooks present in CODEX_HOME; not bypassing hook \
                 trust (deck events degrade to stdout classification)"
            );
            false
        }
        Err(e) => {
            tracing::warn!(
                "codex: could not inspect CODEX_HOME hooks ({e}); not bypassing hook trust"
            );
            false
        }
    }
}

/// PRD #20 R20-002: the last catchable termination signal delivered to the
/// wrapper (0 = none). Set by [`handle_wrap_signal`] (async-signal-safe: a lone
/// atomic store) and observed by BOTH wrap reap loops (PTY and pipe), which
/// forward it to the child's process group, escalate after a grace window, and
/// return through normal cleanup so [`RawModeGuard`] restores the terminal and
/// the child is always reaped — no raw-mode-left-on-signal, no orphaned child.
static WRAP_PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn handle_wrap_signal(sig: libc::c_int) {
    WRAP_PENDING_SIGNAL.store(sig, Ordering::SeqCst);
}

/// PRD #20 finding #12: a RESTORABLE guard that installs async handlers for
/// `SIGTERM` / `SIGHUP` / `SIGINT` and restores the previous dispositions on
/// drop. It is installed BEFORE the child is spawned so a signal arriving in the
/// spawn/setup window is still recorded (and forwarded by the reap loop) instead
/// of terminating the wrapper outright and orphaning the child. `SA_RESTART` is
/// intentionally NOT set so a blocked read returns `EINTR` and the loops react
/// promptly; the pump read loops treat `Interrupted` as retry, not end-of-stream.
struct SignalGuard {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl SignalGuard {
    fn install() -> Self {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = handle_wrap_signal as *const () as libc::sighandler_t;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        action.sa_flags = 0;
        let mut previous = Vec::new();
        for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
            unsafe {
                libc::sigaction(sig, &action, &mut old);
            }
            previous.push((sig, old));
        }
        Self { previous }
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (sig, old) in &self.previous {
            // SAFETY: `old` is the disposition this guard captured at install.
            unsafe {
                libc::sigaction(*sig, old, std::ptr::null_mut());
            }
        }
    }
}

/// Send `signal` to the wrapped child's entire process group (so descendants
/// that inherited its session are torn down too), falling back to the direct
/// child pid if the group send fails. The child is `setsid`'d in its pre-exec
/// (see [`child_pre_exec`]) so its process-group id equals its pid.
fn kill_pid_group(pid: libc::pid_t, signal: libc::c_int) {
    if pid <= 0 {
        return;
    }
    // SAFETY: `killpg`/`kill` are async-signal-safe; `pid` is this wrapper's own
    // child, made a session/group leader via `setsid`, so this targets only its
    // group (or, on fallback, the child itself).
    let sent = unsafe { libc::killpg(pid, signal) };
    if sent != 0 {
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

/// Per-loop signal forwarding + escalation shared by the PTY and pipe reap loops
/// (PRD #20 finding #12). Forwards the first catchable termination signal to the
/// child's process group, arms a grace window, and escalates to `SIGKILL` once
/// it elapses — so a signalled wrapper never orphans a long-running child.
struct SignalForwarder {
    pid: libc::pid_t,
    escalate_deadline: Option<Instant>,
}

impl SignalForwarder {
    fn new(pid: libc::pid_t) -> Self {
        Self {
            pid,
            escalate_deadline: None,
        }
    }

    /// Forward a pending signal (if any) to the child group and escalate to
    /// `SIGKILL` once the grace window elapses. Call once per reap-loop iteration.
    fn tick(&mut self) {
        let sig = WRAP_PENDING_SIGNAL.swap(0, Ordering::SeqCst);
        if sig != 0 {
            self.terminate_with(sig);
        }
        if let Some(deadline) = self.escalate_deadline
            && Instant::now() >= deadline
        {
            kill_pid_group(self.pid, libc::SIGKILL);
        }
    }

    /// Begin termination with `signal`, arming the escalation grace window once.
    /// Also used by the PTY path's downstream-closed teardown (R20-001).
    fn terminate_with(&mut self, signal: libc::c_int) {
        if self.escalate_deadline.is_none() {
            kill_pid_group(self.pid, signal);
            self.escalate_deadline = Some(Instant::now() + crate::agent_pty::AGENT_TERMINATE_GRACE);
        }
    }
}

/// Child pre-exec setup, run in the forked child before `exec` on both wrap
/// paths (PRD #20 finding #12). Resets inherited signal dispositions to their
/// defaults, starts a new session so the child owns its process group (the
/// [`kill_pid_group`] forwarding target), and — when `ctty_fd >= 0` — acquires
/// the inner PTY as the controlling terminal so line discipline (Ctrl+C→SIGINT),
/// job control, and SIGWINCH work for an interactive child. Only async-signal-
/// safe libc calls are used, as required between `fork` and `exec`.
fn child_pre_exec(ctty_fd: RawFd) -> std::io::Result<()> {
    for signo in [
        libc::SIGTERM,
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGCHLD,
        libc::SIGALRM,
    ] {
        // SAFETY: async-signal-safe; resets any inherited handler/ignore.
        unsafe {
            libc::signal(signo, libc::SIG_DFL);
        }
    }
    // SAFETY: async-signal-safe; new session → own process group.
    if unsafe { libc::setsid() } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: async-signal-safe; `ctty_fd` is one of the child's std fds backed
    // by the inner PTY slave. Acquire it as the controlling terminal.
    if ctty_fd >= 0 && unsafe { libc::ioctl(ctty_fd, libc::TIOCSCTTY, 0) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Open a fresh inner pseudo-terminal sized to `(rows, cols)`, returning the
/// owned master and slave descriptors.
fn open_inner_pty(rows: u16, cols: u16) -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `openpty` fills both descriptors on success; each is turned into an
    // `OwnedFd` exactly once so ownership (and close-on-drop) is unambiguous.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

/// Resize an open PTY master, which sends `SIGWINCH` to its foreground process
/// group (the wrapped child).
fn set_pty_size(fd: RawFd, rows: u16, cols: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `TIOCSWINSZ` reads exactly one `winsize` through the pointer.
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
    }
}

/// Classify one tee'd output `line` through the shared `detector` and emit the
/// resulting card event, if the state changed. Shared by every wrap tee (the PTY
/// master pump and the redirected-descriptor pipe pumps) so one coherent session
/// state drives the card.
fn classify_and_emit(
    line: &str,
    detector: &Arc<Mutex<Detector>>,
    emitter: &Emitter,
    is_codex: bool,
) {
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
}

/// Bounded wait for `flag` to become `true`, polling briefly. Returns whether it
/// was observed within `timeout` — used for the post-exit output drain (R20-001)
/// so a wrapper never blocks forever on an unbounded `join`.
fn wait_flag(flag: &AtomicBool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !flag.load(Ordering::SeqCst) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
}

/// Entry point for `dot-agent-deck wrap [--agent <name>] -- <command> <args...>`.
///
/// PRD #20 blocker-1: for an INTERACTIVE outer terminal, spawns `command` on a
/// fresh inner pseudo-terminal and proxies all three streams so the child sees
/// `isatty(0/1/2) == true` and an interactive agent (bare `codex`) keeps its
/// full TUI. R20-012: for a NON-INTERACTIVE outer terminal (piped/redirected),
/// uses a pipe-based path with SEPARATE stdout/stderr and byte-exact raw stdin
/// (no PTY line discipline), so `2>file`, stdout-only pipes, and binary/EOF
/// stdin are preserved. Both paths tee the child's output through pattern
/// detection into `AgentEvent`s and return the child's exit code.
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

    // PRD #20 W1: install the deck's native Codex hooks into the active
    // CODEX_HOME (for a direct `codex` OR a deck-spawned Codex-identity launcher)
    // and, for a direct `codex`, bypass Codex's hook-trust prompt for this
    // deck-vetted spawn via the returned flag. Done once, before either path
    // spawns. A launcher/script must add the flag to its own codex line.
    let add_trust_flag = codex_spawn_prep(program, &emitter.agent_type, emitter.pane_id.as_deref());

    // R20-012 / finding #11: genuine per-descriptor routing. Detect the
    // tty-or-redirected nature of EACH standard descriptor independently. If any
    // descriptor is a real terminal the child needs an inner PTY (so its
    // terminal descriptors see `isatty == true`); each redirected descriptor is
    // threaded to the wrapper's matching real fd rather than merged into the PTY.
    // When NONE is a terminal the wholly non-interactive pipe path runs
    // (separate stdout/stderr, byte-exact stdin).
    let tty = [
        unsafe { libc::isatty(libc::STDIN_FILENO) == 1 },
        unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 },
        unsafe { libc::isatty(libc::STDERR_FILENO) == 1 },
    ];

    if tty.iter().any(|&t| t) {
        run_wrap_pty(program, args, &emitter, add_trust_flag, tty)
    } else {
        run_wrap_pipe(program, args, &emitter, add_trust_flag)
    }
}

/// Interactive path: spawn the child on a fresh inner PTY with GENUINE
/// per-descriptor routing (PRD #20 finding #11). Each of stdin/stdout/stderr is
/// wired to the inner PTY slave when the matching outer descriptor is a terminal
/// (so the child sees `isatty == true` there) or to a pipe teed to the wrapper's
/// matching real fd when it is redirected — so `2>error.log` reaches only the
/// file and `>out.log` leaves stdin/stderr on their terminals, instead of
/// merging everything onto one PTY. Implements the R20-001 / finding #12
/// robustness contract: cancellable output pump, owned child process group,
/// catchable-signal forwarding + escalation, bounded post-exit drain, and an
/// always-run reap so the terminal is restored on every exit path.
fn run_wrap_pty(
    program: &str,
    args: &[String],
    emitter: &Arc<Emitter>,
    add_trust_flag: bool,
    tty: [bool; 3],
) -> ExitCode {
    let [stdin_tty, stdout_tty, stderr_tty] = tty;

    // Size the inner PTY from whichever descriptor is a real terminal so the
    // child's first frame paints at the right geometry (falls back to 24×80).
    let (rows, cols) = terminal_size(libc::STDIN_FILENO)
        .or_else(|| terminal_size(libc::STDOUT_FILENO))
        .or_else(|| terminal_size(libc::STDERR_FILENO))
        .unwrap_or((24, 80));

    let (master, slave) = match open_inner_pty(rows, cols) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("Error: failed to open a pseudo-terminal for `{program}`: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Build the child. `std::process::Command` inherits the wrapper's env (which
    // carries `DOT_AGENT_DECK_PANE_ID` / `_AGENT_ID` injected by the daemon), so
    // the child's own hooks and this wrapper's events attribute to the same pane.
    let mut cmd = StdCommand::new(program);
    if add_trust_flag {
        cmd.arg(CODEX_BYPASS_HOOK_TRUST_FLAG);
    }
    cmd.args(args);
    if let Ok(dir) = std::env::current_dir() {
        cmd.current_dir(dir);
    }

    // Per-descriptor routing: a terminal descriptor is backed by the inner PTY
    // slave (child sees a tty); a redirected descriptor is a pipe we tee to the
    // wrapper's own matching fd (child sees its real redirection).
    let route = |is_tty: bool, slave: &OwnedFd| -> std::io::Result<Stdio> {
        if is_tty {
            Ok(Stdio::from(File::from(slave.try_clone()?)))
        } else {
            Ok(Stdio::piped())
        }
    };
    let (child_stdin, child_stdout, child_stderr) = match (
        route(stdin_tty, &slave),
        route(stdout_tty, &slave),
        route(stderr_tty, &slave),
    ) {
        (Ok(i), Ok(o), Ok(e)) => (i, o, e),
        _ => {
            eprintln!("Error: failed to set up the wrapped `{program}` terminal descriptors");
            return ExitCode::FAILURE;
        }
    };
    cmd.stdin(child_stdin);
    cmd.stdout(child_stdout);
    cmd.stderr(child_stderr);

    // The first slave-backed std fd becomes the child's controlling terminal.
    let ctty_fd: RawFd = if stdin_tty {
        libc::STDIN_FILENO
    } else if stdout_tty {
        libc::STDOUT_FILENO
    } else {
        libc::STDERR_FILENO
    };
    // SAFETY: `child_pre_exec` performs only async-signal-safe libc calls.
    unsafe {
        cmd.pre_exec(move || child_pre_exec(ctty_fd));
    }

    // finding #12: record catchable signals BEFORE the child exists so a signal
    // in the spawn/setup window is forwarded through normal cleanup rather than
    // killing the wrapper and orphaning the child. Restored on drop.
    let _signal_guard = SignalGuard::install();

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: failed to spawn `{program}`: {e}");
            return ExitCode::FAILURE;
        }
    };
    // The parent keeps only the master; drop every slave copy so the master
    // reader EOFs once the child (and its descendants) release the slave.
    drop(slave);

    let child_pid = child.id() as libc::pid_t;

    // Take the pipe ends for any redirected descriptor.
    let pipe_in = if stdin_tty { None } else { child.stdin.take() };
    let pipe_out = if stdout_tty {
        None
    } else {
        child.stdout.take()
    };
    let pipe_err = if stderr_tty {
        None
    } else {
        child.stderr.take()
    };

    // The session has begun — surface the card immediately.
    emitter.emit(EventType::SessionStart);

    // Raw-mode the outer terminal ONLY when stdin is itself a terminal, so
    // keystrokes (incl. Ctrl+C and CR) reach the inner PTY unmodified; restored
    // on drop on every return path.
    let _raw_guard = stdin_tty.then(|| RawModeGuard::enable(libc::STDIN_FILENO));

    // One shared detector across every tee so the card reflects a single
    // coherent session state. PRD #20 M7: the rule set is keyed off the resolved
    // agent type; Codex uses JSON-aware classification, any other command keeps
    // the generic fallback. Recover from a poisoned mutex instead of panicking.
    let is_codex = emitter.agent_type == AgentType::Codex;
    let detector = Arc::new(Mutex::new(Detector::with_rules(ruleset_for(
        &emitter.agent_type,
    ))));

    // Terminal-output pump: the inner master carries whatever the child wrote to
    // the slave. Copy it to the real terminal fd (prefer stdout, else stderr),
    // tee'd through classification. R20-001: `output_done` reports pump
    // termination so a tee that stops on a downstream write failure (the outer
    // terminal closed) makes the main loop terminate the child rather than poll
    // forever while the child blocks on a full inner PTY. Only meaningful when a
    // terminal output descriptor exists; otherwise the master carries nothing.
    let has_tty_output = stdout_tty || stderr_tty;
    let output_done = Arc::new(AtomicBool::new(false));
    let output_thread = if has_tty_output {
        let out_fd = if stdout_tty {
            libc::STDOUT_FILENO
        } else {
            libc::STDERR_FILENO
        };
        let reader = match master.try_clone() {
            Ok(fd) => File::from(fd),
            Err(e) => {
                eprintln!("Error: failed to read the wrapped `{program}` terminal: {e}");
                kill_pid_group(child_pid, libc::SIGKILL);
                let _ = child.wait();
                return ExitCode::FAILURE;
            }
        };
        let emitter = Arc::clone(emitter);
        let detector = Arc::clone(&detector);
        let output_done = Arc::clone(&output_done);
        Some(std::thread::spawn(move || {
            tee(reader, FdWriter(out_fd), |line| {
                classify_and_emit(line, &detector, &emitter, is_codex);
            });
            output_done.store(true, Ordering::SeqCst);
        }))
    } else {
        None
    };

    // Redirected output descriptors: tee each pipe to the matching real fd.
    let out_pipe_thread =
        pipe_out.map(|r| spawn_pipe_tee(r, libc::STDOUT_FILENO, emitter, &detector, is_codex));
    let err_pipe_thread =
        pipe_err.map(|r| spawn_pipe_tee(r, libc::STDERR_FILENO, emitter, &detector, is_codex));

    // Input pump (outer stdin → inner master when stdin is a terminal, else →
    // the child's stdin pipe). Detached: on child exit the main loop returns and
    // process exit reaps this possibly-blocked reader. Dropping the writer on EOF
    // closes the child's stdin so an EOF-sensitive child finishes.
    let input_writer: Option<Box<dyn Write + Send>> = if stdin_tty {
        match master.try_clone() {
            Ok(fd) => Some(Box::new(File::from(fd))),
            Err(_) => None,
        }
    } else {
        pipe_in.map(|p| Box::new(p) as Box<dyn Write + Send>)
    };
    if let Some(mut writer) = input_writer {
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
                if n < 0 {
                    // finding #12: a handler-delivered signal interrupts the
                    // read; retry rather than tear down stdin forwarding.
                    if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    break;
                }
                if n == 0 {
                    break;
                }
                if writer.write_all(&buf[..n as usize]).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });
    }

    // Main loop: forward outer resizes to the inner PTY (so the child receives
    // SIGWINCH), forward + escalate catchable signals, notice a dead output pump,
    // and poll for child exit. `try_wait` reaps the child once it returns `Some`.
    let master_fd = master.as_raw_fd();
    let mut last_size = (rows, cols);
    let mut output_gone_at: Option<Instant> = None;
    let mut fwd = SignalForwarder::new(child_pid);
    let status = loop {
        if let Some(size) = terminal_size(libc::STDIN_FILENO)
            .or_else(|| terminal_size(libc::STDOUT_FILENO))
            .or_else(|| terminal_size(libc::STDERR_FILENO))
            && size != last_size
        {
            set_pty_size(master_fd, size.0, size.1);
            last_size = size;
        }

        // finding #12: forward a pending signal to the child group and escalate.
        fwd.tick();

        // R20-001: the terminal output pump ended while the child is still alive
        // → the downstream terminal consumer closed; after a short settle window
        // terminate the group so the child can't block on a full inner PTY.
        if has_tty_output && output_done.load(Ordering::SeqCst) {
            let since = output_gone_at.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(200) {
                fwd.terminate_with(libc::SIGTERM);
            }
        }

        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(e) => {
                eprintln!("Error: failed to wait on wrapped `{program}`: {e}");
                kill_pid_group(child_pid, libc::SIGKILL);
                break None;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    // R20-001 cleanup: the direct child exited (or was killed). Drain the
    // terminal-output tee with a BOUNDED wait instead of an unbounded join — a
    // background descendant that retained the slave PTY keeps the reader blocked,
    // so if the drain times out force the whole group down (releasing the slave
    // and EOFing the reader), then reap. Never `join()` that pump: process exit
    // reaps a still-blocked reader thread.
    drop(master);
    if has_tty_output && !wait_flag(&output_done, Duration::from_millis(300)) {
        kill_pid_group(child_pid, libc::SIGTERM);
        if !wait_flag(&output_done, crate::agent_pty::AGENT_TERMINATE_GRACE) {
            kill_pid_group(child_pid, libc::SIGKILL);
            let _ = wait_flag(&output_done, Duration::from_millis(500));
        }
    }
    let _ = child.wait();
    // The redirected-output tees see EOF once the child's pipe ends close (the
    // child is reaped, or the group was killed above); join them so all output
    // reaches the file/pipe before the final event.
    if let Some(t) = out_pipe_thread {
        let _ = t.join();
    }
    if let Some(t) = err_pipe_thread {
        let _ = t.join();
    }
    drop(output_thread);

    // PRD #20 finding #14: emit Idle only after a SUCCESSFUL exit; a nonzero /
    // signalled failure (or a wait error) ends as a visible Error rather than a
    // false idle card. Preserve the child's numeric exit code as the wrapper's
    // own (truncated to a byte, as shells do); a signalled/wait failure maps to 1.
    let (success, code) = match status {
        Some(s) => (s.success(), s.code().unwrap_or(1) as u8),
        None => (false, 1),
    };
    emitter.emit(if success {
        EventType::Idle
    } else {
        EventType::Error
    });
    ExitCode::from(code)
}

/// Non-interactive path (R20-012): the outer stream is piped/redirected, so
/// there is no TTY to proxy and no line discipline to honor. Spawn the child
/// with SEPARATE stdout/stderr pipes and a raw stdin pipe, copy each stream
/// verbatim to the matching outer fd (tee'd through classification), and forward
/// the outer stdin byte-for-byte — closing the child's stdin on EOF so an
/// EOF-sensitive child (`cat`) terminates. This preserves `2>file`, stdout-only
/// pipes, and binary/partial stdin that the merged-PTY path would mangle.
fn run_wrap_pipe(
    program: &str,
    args: &[String],
    emitter: &Arc<Emitter>,
    add_trust_flag: bool,
) -> ExitCode {
    let mut cmd = StdCommand::new(program);
    if add_trust_flag {
        cmd.arg(CODEX_BYPASS_HOOK_TRUST_FLAG);
    }
    cmd.args(args);
    if let Ok(dir) = std::env::current_dir() {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // finding #12: own the child's process group (no controlling terminal on
    // this non-interactive path) and reset inherited signal dispositions, so a
    // signal delivered to the wrapper is forwarded to and reaps the child.
    // SAFETY: `child_pre_exec` performs only async-signal-safe libc calls.
    unsafe {
        cmd.pre_exec(|| child_pre_exec(-1));
    }

    // finding #12: install the restorable signal guard BEFORE spawning so a
    // signal in the spawn window is recorded and forwarded, not fatal.
    let _signal_guard = SignalGuard::install();

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: failed to spawn `{program}`: {e}");
            return ExitCode::FAILURE;
        }
    };
    let child_pid = child.id() as libc::pid_t;

    emitter.emit(EventType::SessionStart);

    let child_stdout = child.stdout.take().expect("piped child stdout");
    let child_stderr = child.stderr.take().expect("piped child stderr");
    let child_stdin = child.stdin.take().expect("piped child stdin");

    // One shared detector across both output streams so the card reflects a
    // single coherent state (mirrors the PTY path).
    let is_codex = emitter.agent_type == AgentType::Codex;
    let detector = Arc::new(Mutex::new(Detector::with_rules(ruleset_for(
        &emitter.agent_type,
    ))));

    let out_thread = spawn_pipe_tee(
        child_stdout,
        libc::STDOUT_FILENO,
        emitter,
        &detector,
        is_codex,
    );
    let err_thread = spawn_pipe_tee(
        child_stderr,
        libc::STDERR_FILENO,
        emitter,
        &detector,
        is_codex,
    );

    // Input pump (outer stdin → child stdin, verbatim). On EOF/close of our
    // stdin, dropping `child_stdin` closes it so an EOF-sensitive child finishes.
    let in_thread = std::thread::spawn(move || {
        let mut child_stdin = child_stdin;
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe {
                libc::read(
                    libc::STDIN_FILENO,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }
            if n == 0 {
                break;
            }
            if child_stdin.write_all(&buf[..n as usize]).is_err() || child_stdin.flush().is_err() {
                break;
            }
        }
        drop(child_stdin);
    });

    // finding #12: reap through a NON-blocking loop (never a bare blocking
    // `child.wait`) so a catchable termination signal delivered to the wrapper
    // is forwarded to the child group and escalated, guaranteeing the child is
    // reaped rather than orphaned when the wrapper is signalled.
    let mut fwd = SignalForwarder::new(child_pid);
    let status = loop {
        fwd.tick();
        match child.try_wait() {
            Ok(Some(s)) => break Ok(s),
            Ok(None) => {}
            Err(e) => break Err(e),
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // The child exited (or was killed) → its stdout/stderr pipe write ends close
    // → the tees see EOF and finish. Join them so all output is flushed before
    // the final event.
    let _ = out_thread.join();
    let _ = err_thread.join();
    // The stdin pump may still block on our stdin; detach it (process exit reaps
    // it). Dropping the handle does not wait.
    drop(in_thread);

    let (success, code) = match status {
        Ok(s) => (s.success(), s.code().unwrap_or(1) as u8),
        Err(_) => (false, 1),
    };
    emitter.emit(if success {
        EventType::Idle
    } else {
        EventType::Error
    });
    ExitCode::from(code)
}

/// Spawn a tee thread copying `reader` verbatim to the raw fd `out_fd` while
/// feeding completed lines through the shared classification `detector`. Used
/// for every REDIRECTED descriptor on both wrap paths (the non-interactive pipe
/// path's stdout/stderr, and a PTY-path descriptor that was redirected) so each
/// stays separate on its own outer fd.
fn spawn_pipe_tee<R: Read + Send + 'static>(
    reader: R,
    out_fd: RawFd,
    emitter: &Arc<Emitter>,
    detector: &Arc<Mutex<Detector>>,
    is_codex: bool,
) -> std::thread::JoinHandle<()> {
    let emitter = Arc::clone(emitter);
    let detector = Arc::clone(detector);
    std::thread::spawn(move || {
        tee(reader, FdWriter(out_fd), |line| {
            classify_and_emit(line, &detector, &emitter, is_codex);
        });
    })
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

    // PRD #20 finding #12 targeted coverage for the edges the subprocess harness
    // (`tests/wrap_io.rs`, `codex/wrap/004`) left to the coder: the restorable
    // pre-spawn signal guard and terminal-state restoration. Both mutate
    // process-global disposition/termios but RESTORE it, and each `#[spec]`-free
    // unit test runs isolated under nextest, so they cannot leak into siblings.

    fn query_sigaction(sig: libc::c_int) -> libc::sigaction {
        let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: a null new-action only queries the current disposition.
        unsafe {
            libc::sigaction(sig, std::ptr::null(), &mut current);
        }
        current
    }

    /// The signal guard installs the wrap handler for SIGTERM/SIGHUP/SIGINT the
    /// moment it is constructed — the pre-spawn window finding #12 requires — and
    /// restores the previous disposition on drop so a returning wrapper leaves
    /// the process's signal state as it found it.
    #[test]
    fn signal_guard_installs_before_spawn_and_restores_on_drop() {
        let handler = handle_wrap_signal as *const () as libc::sighandler_t;
        let before = query_sigaction(libc::SIGHUP).sa_sigaction;
        {
            let _guard = SignalGuard::install();
            assert_eq!(query_sigaction(libc::SIGTERM).sa_sigaction, handler);
            assert_eq!(query_sigaction(libc::SIGHUP).sa_sigaction, handler);
            assert_eq!(query_sigaction(libc::SIGINT).sa_sigaction, handler);
        }
        assert_eq!(
            query_sigaction(libc::SIGHUP).sa_sigaction,
            before,
            "dropping the guard restores the previous SIGHUP disposition"
        );
    }

    /// The raw-mode guard puts a terminal into raw mode (clearing the canonical
    /// line-discipline flags) and restores the ORIGINAL termios on drop, so a
    /// signalled or normally-exiting wrapper never leaves the terminal in raw
    /// mode.
    #[test]
    fn raw_mode_guard_restores_termios_on_drop() {
        let (_master, slave) = open_inner_pty(24, 80).expect("open inner pty");
        let fd = slave.as_raw_fd();
        let read_lflag = || {
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(fd, &mut t) }, 0, "tcgetattr");
            t.c_lflag
        };
        let before = read_lflag();
        {
            let _guard = RawModeGuard::enable(fd);
            assert_ne!(
                read_lflag(),
                before,
                "raw mode clears canonical/echo/signal line-discipline flags"
            );
        }
        assert_eq!(
            read_lflag(),
            before,
            "termios restored to the original on drop"
        );
    }
}
