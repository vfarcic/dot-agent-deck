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
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};

use chrono::Utc;

use crate::agent_pty::{DOT_AGENT_DECK_AGENT_ID, DOT_AGENT_DECK_PANE_ID};
use crate::event::{AGENT_EVENT_SCHEMA_VERSION, AgentEvent, AgentType, EventType};

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
        let detected = classify_line_with(line, self.rules)?;
        if self.last == Some(detected) {
            None
        } else {
            self.last = Some(detected);
            Some(detected)
        }
    }
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
        };
        if let Ok(json) = serde_json::to_string(&event) {
            let _ = crate::hook::send_to_socket(&json);
        }
    }
}

/// Pump bytes from `reader` to `writer` **verbatim** (transparent passthrough),
/// while feeding each completed line to `on_line` for classification.
///
/// Reads in chunks and writes+flushes immediately, so a prompt printed without
/// a trailing newline (e.g. `Enter your name: `) still reaches the user at once
/// — line-oriented buffering would stall interactivity. Line accumulation for
/// classification happens on `\n`; a trailing partial line is classified when
/// the stream ends. Bytes that don't form valid UTF-8 are passed through but
/// skipped for classification.
fn tee<R: Read, W: Write>(mut reader: R, mut writer: W, mut on_line: impl FnMut(&str)) {
    let mut buf = [0u8; 8192];
    let mut line: Vec<u8> = Vec::new();
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                // Transparent passthrough first — the user must see output with
                // minimal latency regardless of classification.
                let _ = writer.write_all(chunk);
                let _ = writer.flush();
                for &b in chunk {
                    if b == b'\n' {
                        if let Ok(s) = std::str::from_utf8(&line) {
                            on_line(s);
                        }
                        line.clear();
                    } else {
                        line.push(b);
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

/// Translate a finished child's [`ExitStatus`] into the wrapper's own
/// [`ExitCode`], preserving the child's numeric code (truncated to a byte, as
/// shells do). A child killed by a signal has no code → `FAILURE`.
fn exit_code_from(status: ExitStatus) -> ExitCode {
    match status.code() {
        Some(code) => ExitCode::from(code as u8),
        None => ExitCode::FAILURE,
    }
}

/// Entry point for `dot-agent-deck wrap [--agent <name>] -- <command> <args...>`.
///
/// Spawns `command`, tees its stdout/stderr through pattern detection into
/// `AgentEvent`s while passing all stdio through transparently, and returns the
/// child's exit code as the wrapper's exit code.
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

    let emitter = Arc::new(Emitter {
        agent_type,
        session_id,
        pane_id,
        agent_id,
        cwd,
    });

    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: failed to spawn `{program}`: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The session has begun — surface the card immediately.
    emitter.emit(EventType::SessionStart);

    // One shared detector so stdout and stderr contribute to a single,
    // coherent session state (see `Detector`). PRD #20 M7: the rule set is
    // keyed off the resolved agent type so `wrap --agent codex` classifies
    // Codex JSONL, while any other command keeps the generic fallback.
    let detector = Arc::new(Mutex::new(Detector::with_rules(ruleset_for(
        &emitter.agent_type,
    ))));

    let stdout_thread = child.stdout.take().map(|out| {
        let emitter = Arc::clone(&emitter);
        let detector = Arc::clone(&detector);
        std::thread::spawn(move || {
            tee(out, std::io::stdout(), |line| {
                if let Some(ev) = detector.lock().unwrap().observe(line) {
                    emitter.emit(ev.event_type());
                }
            });
        })
    });

    let stderr_thread = child.stderr.take().map(|err| {
        let emitter = Arc::clone(&emitter);
        let detector = Arc::clone(&detector);
        std::thread::spawn(move || {
            tee(err, std::io::stderr(), |line| {
                if let Some(ev) = detector.lock().unwrap().observe(line) {
                    emitter.emit(ev.event_type());
                }
            });
        })
    });

    let status = child.wait();

    // Drain both tees so no trailing output is lost before we report Idle.
    if let Some(t) = stdout_thread {
        let _ = t.join();
    }
    if let Some(t) = stderr_thread {
        let _ = t.join();
    }

    // Process-exit quiescence: the wrapped session is done → idle.
    emitter.emit(EventType::Idle);

    match status {
        Ok(s) => exit_code_from(s),
        Err(e) => {
            eprintln!("Error: failed to wait on wrapped `{program}`: {e}");
            ExitCode::FAILURE
        }
    }
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
}
