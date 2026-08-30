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
//!
//! PRD #42 M8 (merge with #20): the wrapper RUNTIME (inner PTY via `openpty`,
//! raw-mode termios, POSIX signal forwarding, `killpg`, `setsid`/`TIOCSCTTY`
//! pre-exec) is Unix-only and carries a per-item `#[cfg(unix)]` gate; on Windows
//! `run_wrap` is a compiling stub (a ConPTY port is #163/#164, matching the
//! daemon/attach story). The PURE detection layer ([`classify_line`],
//! [`Detector`], [`RuleSet`], [`CODEX`]) and the pure command rewrite
//! ([`wrap_launch_command`], called by the cross-platform `agent_pty`/`ui` spawn
//! seams) stay cross-platform. On non-Unix the Unix-only helpers compile out, so
//! the pure helpers they alone consume are dead there — hence the conditional
//! `allow` below (Unix keeps full linting).
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Command as StdCommand, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::agent_pty::{DOT_AGENT_DECK_AGENT_ID, DOT_AGENT_DECK_PANE_ID};
use crate::event::{
    AGENT_EVENT_SCHEMA_VERSION, AgentEvent, AgentType, EventType, LiveTarget,
    SESSION_START_ORIGIN_METADATA_KEY, TargetKind, WRAPPER_FORK_SESSION_START_ORIGIN, Writable,
};
// Issue #243: the interface-origin values are named only by `InterfaceFact`,
// which reads the inner pty's termios and so exists only where `run_wrap_pty`
// does. Importing them ungated would be an unused import on Windows.
#[cfg(unix)]
use crate::event::{
    WRAPPER_INTERFACE_READY_SESSION_START_ORIGIN, WRAPPER_INTERFACE_SETTLED_SESSION_START_ORIGIN,
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
        self.emit_with_metadata(event_type, HashMap::new());
    }

    /// PRD #225 M3: the fork-time `SessionStart` this wrapper emits the moment
    /// `cmd.spawn()` returns. Its ONLY job is to surface the dashboard card so a
    /// slow-booting agent isn't invisible — the child is typically just the
    /// launcher (`devbox`, a shell) at this point, seconds away from the real
    /// agent TUI. Stamping [`SESSION_START_ORIGIN_METADATA_KEY`] lets readiness
    /// gates tell this apart from a genuine "the session is up and accepting
    /// input" signal; see [`crate::state::wait_for_session_start`].
    fn emit_fork_session_start(&self) {
        let mut metadata = HashMap::new();
        metadata.insert(
            SESSION_START_ORIGIN_METADATA_KEY.to_string(),
            WRAPPER_FORK_SESSION_START_ORIGIN.to_string(),
        );
        self.emit_with_metadata(EventType::SessionStart, metadata);
    }

    /// Issue #243: the INTERFACE-READY `SessionStart` this wrapper emits once it
    /// has observed the wrapped child's interface come up — the pre-prompt
    /// readiness signal a delegate/scheduler gate can actually wait for.
    ///
    /// Distinct from BOTH the events that existed before it. It is not
    /// [`Self::emit_fork_session_start`], which fires at `cmd.spawn()` when the
    /// child is typically still a launcher; and it is not the agent's own native
    /// `SessionStart`, which for codex-cli fires when the first TURN starts — a
    /// consequence of the very prompt the gate is withholding, which is why the
    /// gate never fast-pathed and every Codex delegate cost ~31 s.
    ///
    /// It carries the origin value of the FACT that fired
    /// ([`InterfaceFact::origin`]) rather than arriving unmarked, so a consumer
    /// can tell "the deck WATCHED this interface come up" from "an agent session
    /// announced itself" — and, since issue #243's review, can also tell the
    /// strong observation (raw input mode) from the weak guess (output settled)
    /// and price them differently. This event carries the WRAPPER's session id,
    /// not the agent's, so it must never bind a conversation. See
    /// [`crate::event::AgentEvent::is_wrapper_session_start`].
    ///
    /// **Sent off the supervisory loop** (issue #243 audit F3). Every other
    /// `Emitter` call site is a tee thread; this one is the wrapper's 50 ms main
    /// loop, the loop that forwards the user's `Ctrl+C`/SIGTERM to the child
    /// group, and [`crate::hook::send_to_socket`] is a blocking connect + write
    /// with no timeout. A wedged, SIGSTOPped or backlogged daemon would stall
    /// signal forwarding. The per-fact latches bound this to at most TWO threads
    /// per wrapped session — one for the settle guess, one for the raw-mode
    /// observation that may upgrade it — and the send is bounded so neither can
    /// linger either.
    ///
    /// `#[cfg(unix)]` because [`InterfaceFact`] is: the watch reads a pty's
    /// termios, and only `run_wrap_pty` — itself Unix-only — has one.
    #[cfg(unix)]
    fn emit_interface_ready(&self, fact: InterfaceFact) {
        let mut metadata = HashMap::new();
        metadata.insert(
            SESSION_START_ORIGIN_METADATA_KEY.to_string(),
            fact.origin().to_string(),
        );
        let Ok(json) = serde_json::to_string(&self.build_event(EventType::SessionStart, metadata))
        else {
            return;
        };
        std::thread::spawn(move || {
            crate::hook::send_to_socket_bounded(&json, INTERFACE_READY_SEND_TIMEOUT);
        });
    }

    fn emit_with_metadata(&self, event_type: EventType, metadata: HashMap<String, String>) {
        let event = self.build_event(event_type, metadata);
        if let Ok(json) = serde_json::to_string(&event) {
            let _ = crate::hook::send_to_socket(&json);
        }
    }

    /// Issue #243 audit F3: build the [`AgentEvent`] without sending it, so a
    /// caller that must not block on the daemon can do the (cheap, pure) build on
    /// its own thread and hand only the serialized line to a sender.
    fn build_event(&self, event_type: EventType, metadata: HashMap<String, String>) -> AgentEvent {
        AgentEvent {
            session_id: self.session_id.clone(),
            agent_type: self.agent_type.clone(),
            event_type,
            tool_name: None,
            tool_detail: None,
            cwd: self.cwd.clone(),
            timestamp: Utc::now(),
            user_prompt: None,
            metadata,
            pane_id: self.pane_id.clone(),
            agent_id: self.agent_id.clone(),
            agent_version: None,
            // PRD #20 M6: stamp the schema version the wrapper writes.
            schema_version: Some(AGENT_EVENT_SCHEMA_VERSION),
            // PRD #20 M3: a wrapped session is history-only from the dashboard's
            // perspective (see `Emitter::live_target`).
            live_target: Some(self.live_target),
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
    let deck = deck_binary_for_wrap();
    format!("{deck} wrap --agent {name} -- {command}")
}

/// Test-only override for the binary [`wrap_launch_command`] names. Set to an
/// absolute path so a test can point the rewrite at a recorder instead of the
/// co-located build; production never sets it. Read in the *spawning* process, so
/// a test sets it on itself (nextest gives each test its own process).
pub const DOT_AGENT_DECK_WRAP_BIN: &str = "DOT_AGENT_DECK_WRAP_BIN";

/// Which `dot-agent-deck` binary the rewrite should name: THIS process's own
/// executable where it can be identified, otherwise a bare `dot-agent-deck` for
/// `$PATH` to resolve.
///
/// The rewritten command runs through a login shell, which re-reads the user's
/// profile and therefore resolves `$PATH` independently of whatever the spawning
/// process was launched with. A bare name there silently runs a DIFFERENT BUILD
/// than the one doing the spawning — `~/.local/bin/dot-agent-deck` in practice.
/// Two consequences, one of which is a test-integrity hole:
///
/// - the test suite exercised the freshly-built deck as daemon/TUI but the
///   INSTALLED RELEASE as the wrapper, so wrapper behaviour was validated
///   against whatever the developer happened to have installed, and a wrapper
///   fix could not be observed end-to-end by the suite at all;
/// - a running deck could pair its daemon with a wrapper of another version.
///
/// Same rationale (and the same fix) as [`crate::daemon_attach`] locating the
/// daemon via `current_exe` rather than `$PATH`.
///
/// Falls back to the bare name when the resolved path is unusable, so behaviour
/// only ever improves on what `$PATH` would have found:
/// - a test-harness executable — those live in `target/<profile>/deps/`, so a
///   sibling `dot-agent-deck` one level up is preferred when present, which is
///   what lets in-process tests drive the build they just compiled;
/// - a path that no longer exists: Linux reports a replaced binary as
///   `<path> (deleted)`, routine while rebuilding during development;
/// - a path containing whitespace, which the shell would re-split (nothing
///   quotes this command string).
fn deck_binary_for_wrap() -> String {
    const BARE: &str = "dot-agent-deck";
    fn usable(path: &std::path::Path) -> Option<String> {
        let text = path.to_str()?;
        (path.file_name()? == BARE && !text.chars().any(char::is_whitespace) && path.is_file())
            .then(|| text.to_string())
    }

    // Explicit override, consulted first. Resolving the co-located build is what
    // makes the suite honest, but it also takes away the one seam a test had for
    // observing the rewrite: planting a fake `dot-agent-deck` on `$PATH`. This is
    // that seam back, in the same spirit as
    // `daemon_attach::spawn_daemon_serve_detached_with_exe` — an explicit path so
    // tests can point at a recorder. Production never sets it.
    if let Ok(explicit) = std::env::var(DOT_AGENT_DECK_WRAP_BIN)
        && !explicit.is_empty()
    {
        return explicit;
    }

    let Ok(exe) = std::env::current_exe() else {
        return BARE.to_string();
    };
    if let Some(found) = usable(&exe) {
        return found;
    }
    let Some(dir) = exe.parent() else {
        return BARE.to_string();
    };
    usable(&dir.join(BARE))
        .or_else(|| {
            (dir.file_name() == Some(std::ffi::OsStr::new("deps")))
                .then(|| dir.parent().map(|up| up.join(BARE)))
                .flatten()
                .as_deref()
                .and_then(usable)
        })
        .unwrap_or_else(|| BARE.to_string())
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
/// An explicit `--agent` override wins and is resolved through
/// [`crate::agent_registry::resolve_declared_agent`], so a name the registry
/// doesn't know yet becomes the neutral [`AgentType::None`] rather than a
/// guess. That is the SAME function the `agent = "…"` config key resolves
/// through (issue #308), so the two declaration surfaces cannot drift.
/// Otherwise the type is inferred from the wrapped binary exactly like the TUI
/// spawn sites ([`AgentType::from_command`]).
///
/// Either way, with Codex in the registry (M7),
/// `wrap -- codex` (or `--agent codex`) resolves to it and [`ruleset_for`]
/// selects the [`CODEX`] rules — with no change here.
fn resolve_agent_type(agent_override: Option<&str>, program: &str) -> AgentType {
    if let Some(name) = agent_override {
        return crate::agent_registry::resolve_declared_agent(name);
    }
    AgentType::from_command(Some(program)).unwrap_or(AgentType::None)
}

/// Derive the session id events are grouped under. When run inside a managed
/// pane it mirrors the `agent-event` verb's `{pane_id}-session` convention so
/// events land on the pane's card.
///
/// PRD #20 Greptile finding #4/#5 (standalone uniqueness): a STANDALONE wrap (no
/// `DOT_AGENT_DECK_PANE_ID`) previously derived a FIXED id from the binary's
/// basename (e.g. `wrap-codex`), so two concurrent standalone Codex terminals
/// collided on one session id and their events/card-status overwrote each other.
/// The standalone id now folds in a per-session `nonce` (the wrapper pid) so
/// concurrent standalone sessions stay distinct. The managed-pane id is
/// intentionally left pane-derived (stable across a `/clear` for card continuity).
fn session_id_for(pane_id: Option<&str>, program: &str, standalone_nonce: &str) -> String {
    match pane_id {
        Some(p) => format!("{p}-session"),
        None => {
            let base = std::path::Path::new(program)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(program);
            format!("wrap-{base}-{standalone_nonce}")
        }
    }
}

/// A `Write` that writes UNBUFFERED to a raw fd (the outer stdout), so the
/// child's output passes through with minimal latency and no line buffering.
#[cfg(unix)]
struct FdWriter(RawFd);

#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
struct RawModeGuard {
    fd: RawFd,
    original: Option<libc::termios>,
}

#[cfg(unix)]
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

#[cfg(unix)]
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Some(orig) = self.original {
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, &orig);
            }
        }
    }
}

/// Whether the wrapped program's basename is `codex`. PRD #20 W1 keys the hook
/// INSTALL off the ACTUAL program (in addition to the resolved identity), not the
/// identity alone: `wrap --agent codex -- /bin/sh` (the non-interactive I/O test)
/// carries Codex identity but launches a shell outside any pane, and must not
/// write to the user's `~/.codex`.
fn program_is_codex(program: &str) -> bool {
    std::path::Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        == Some("codex")
}

/// The outcome of [`codex_spawn_prep`]: the vetted `CODEX_HOME` (resolved ONCE) to
/// PIN on the spawned child so the home the deck installed into and trusted is
/// exactly the home Codex loads.
struct CodexSpawnPrep {
    /// `CODEX_HOME` to set explicitly on the spawned child's environment (finding
    /// #2). `None` when this invocation installs no Codex hooks / resolves no home.
    pinned_home: Option<std::path::PathBuf>,
}

/// PRD #20 W1 spawn wiring. Decides, for this wrap invocation, whether to install
/// the deck's native Codex hooks, which `CODEX_HOME` to pin, and — for the hooks
/// it just authored — records SCOPED trust so Codex will actually run them.
///
/// - **Hooks install** fires whenever a Codex-identity agent will actually run
///   under this wrapper: a `codex` program (bare or path), OR a deck-spawned pane
///   (`pane_id` present) whose declared identity is Codex. The latter covers a
///   launcher/wrapper SCRIPT the deck spawned but whose argv we can't reach
///   into — the hooks are `CODEX_HOME`-scoped, so they apply however codex is
///   ultimately launched. It deliberately does NOT fire for a standalone
///   `wrap --agent codex -- /bin/sh` (no pane, non-codex program), so a
///   non-interactive I/O wrap never writes to the user's `~/.codex`.
/// - **`CODEX_HOME` pin** (finding #2): the vetted home is resolved ONCE and
///   returned so the caller can set it EXPLICITLY on the child, keeping install,
///   trust, and launch on the same deck-controlled home instead of a value that
///   could drift.
/// - **Trust** (PRD #20 §4.1, Greptile P1): there is NO invocation-global
///   `--dangerously-bypass-hook-trust` any more — nothing trust-related reaches
///   argv, so no launcher (PATH shim, script, alias) can receive, forward, or
///   re-home it. Instead
///   [`crate::codex_hooks_manage::trust_deck_hooks_in`] records per-hook,
///   hash-pinned trust for EXACTLY the entries the deck authored in the pinned
///   home. That is launch-method agnostic (bare `codex`, `/abs/path/codex`,
///   `./launcher.sh`, `devbox run codex-big` all behave identically) and strictly
///   narrower than the old bypass: a third-party hook in the same `hooks.json`
///   stays untrusted. Any failure is warned and the spawn continues — Codex then
///   won't run the hooks and events degrade to stdout classification (fail-closed).
fn codex_spawn_prep(
    program: &str,
    agent_type: &AgentType,
    pane_id: Option<&str>,
) -> CodexSpawnPrep {
    let program_codex = program_is_codex(program);
    let codex_identity = *agent_type == AgentType::Codex;
    let installs_hooks = codex_identity && (program_codex || pane_id.is_some());
    // Resolve the vetted home ONCE, so the SAME path is installed into, trusted,
    // and pinned on the child — they can't drift apart (finding #2).
    let pinned_home = if installs_hooks {
        crate::codex_hooks_manage::auto_install();
        crate::codex_hooks_manage::active_codex_home()
    } else {
        None
    };

    // Scoped trust for the hooks just installed, in the SAME pinned home. The
    // child's cwd is the wrapper's cwd, which is what Codex resolves hooks for.
    if let Some(home) = pinned_home.as_deref() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| home.to_path_buf());
        match crate::codex_hooks_manage::trust_deck_hooks_in(home, &cwd) {
            Ok(count) => tracing::debug!(count, "codex: recorded scoped trust for deck hooks"),
            Err(e) => tracing::warn!(
                "codex: could not record scoped hook trust ({e}); deck events degrade to stdout \
                 classification"
            ),
        }
    }

    CodexSpawnPrep { pinned_home }
}

/// PRD #20 R20-002: the last catchable termination signal delivered to the
/// wrapper (0 = none). Set by [`handle_wrap_signal`] (async-signal-safe: a lone
/// atomic store) and observed by BOTH wrap reap loops (PTY and pipe), which
/// forward it to the child's process group, escalate after a grace window, and
/// return through normal cleanup so [`RawModeGuard`] restores the terminal and
/// the child is always reaped — no raw-mode-left-on-signal, no orphaned child.
static WRAP_PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn handle_wrap_signal(sig: libc::c_int) {
    WRAP_PENDING_SIGNAL.store(sig, Ordering::SeqCst);
}

/// Test-only self-defense for the wrapper: the orphan watchdog + max-lifetime
/// backstop that `daemon serve` has had all along (`daemon::run_daemon_with`),
/// applied to `wrap`.
///
/// Both env vars were previously read ONLY by the daemon, so a wrapper whose
/// test died without running its cleanup `Drop` (SIGKILL / panic-abort / nextest
/// timeout) leaked to PID 1 and stayed there **forever** — no orphan exit, no
/// lifetime cap. Observed in the wild: three `wrap --agent codex` stubs alive
/// for three days from a worktree that had already been deleted, two of them
/// spinning a `while [ ! -e "$WRAP_START" ]; do sleep 0.01; done` shell loop at
/// ~100 wakeups/second against a sentinel whose tempdir was long gone.
///
/// Termination is requested through [`WRAP_PENDING_SIGNAL`] rather than by
/// exiting or killing directly, so it takes the SAME audited path a real
/// SIGTERM takes: the reap loop forwards to the child's process group and
/// escalates to `SIGKILL` after [`crate::agent_pty::WRAP_TERMINATE_GRACE`]
/// (so a child that traps `TERM` still dies), and [`RawModeGuard`] still
/// restores the terminal. Both reap loops poll on a 50 ms cadence and call
/// `SignalForwarder::tick`, so the store is observed promptly.
///
/// Env-gated and OFF by default: a production wrapper sets neither var and so
/// never arms the thread.
#[cfg(unix)]
fn arm_wrap_self_defense() {
    use crate::agent_pty::{
        DOT_AGENT_DECK_EXIT_WHEN_ORPHANED, DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS,
    };
    use crate::daemon::{parse_bool_flag, parse_max_lifetime_secs, should_exit_orphaned};

    let exit_when_orphaned = std::env::var(DOT_AGENT_DECK_EXIT_WHEN_ORPHANED)
        .map(|v| parse_bool_flag(&v))
        .unwrap_or(false);
    let max_lifetime = std::env::var(DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS)
        .ok()
        .and_then(|v| parse_max_lifetime_secs(&v));
    // `checked_add`, not `+`: `parse_max_lifetime_secs` accepts every positive
    // `u64`, and `Instant + Duration` PANICS on overflow — so an exported
    // `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS=18446744073709551615` would abort
    // the wrapper at startup rather than bound it. An unrepresentable deadline
    // is one nothing can reach, so it degrades to "no cap", which is what the
    // absurd value asked for; the guard below then declines to arm a thread
    // with nothing left to watch instead of leaking one that spins forever.
    // The harness clamps to 300 s (`tests/common/child_lifetime_bound.rs`)
    // before this is ever read — this is the path for a value that did not come
    // from the harness.
    let deadline = max_lifetime.and_then(|d| Instant::now().checked_add(d));
    if !exit_when_orphaned && deadline.is_none() {
        return;
    }

    let original_ppid = crate::platform::proc::current_ppid();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let orphaned = exit_when_orphaned
                && should_exit_orphaned(original_ppid, crate::platform::proc::current_ppid());
            let expired = deadline.is_some_and(|d| Instant::now() >= d);
            if orphaned || expired {
                WRAP_PENDING_SIGNAL.store(libc::SIGTERM, Ordering::SeqCst);
                return;
            }
        }
    });
}

/// Poll cadence of the forked child-group backstop below.
///
/// Also the width of its pid-reuse window, and the honest framing of that is a
/// **bounded same-UID residual, not an impossibility**. The reaper signals ONLY
/// at its deadline and only after re-checking that the group is still there, so
/// for it to reach an unrelated group the original would have to die and the
/// kernel hand the same number back out inside one tick. At 250 ms that needs
/// the whole pid space to wrap in a quarter of a second — >130k forks/second
/// even on a host pinned to the 32768 default, and correspondingly less on a
/// small pid namespace, which is the case this estimate does not generalise to.
/// It is a narrow window rather than a closed one: a revalidated numeric PGID
/// carries no identity, so nothing here can *prove* the group it signals is the
/// one it armed on. Unix permission checks still rule out signalling another
/// user; what remains possible is same-UID self-harm inside that window. A
/// strict guarantee would need an OS-owned containment mechanism that names the
/// processes rather than a number — a dedicated cgroup or equivalent — which is
/// a larger change than this backstop. See also `docs/develop/e2e-temp-dirs.md`
/// on why this codebase refuses to *infer* pid reuse; this bounds the window
/// instead of guessing at it.
#[cfg(unix)]
const CHILD_GROUP_BACKSTOP_POLL: Duration = Duration::from_millis(250);

/// Floor for the forked reaper's fallback close loop: never scan fewer
/// descriptors than this, whatever `RLIMIT_NOFILE` says. A wrapper holds well
/// under a dozen (the inner PTY master, the redirected-descriptor pipes, the std
/// fds), so 1024 covers every one it opened itself; `close(2)` on an
/// already-closed descriptor is a harmless `EBADF`.
///
/// This USED to be the whole story, and it was not enough. Descriptor *count*
/// does not bound descriptor *number*: a caller can enter `wrap` holding a
/// non-`CLOEXEC` descriptor above 1023, and once enough low numbers are taken
/// `openpty` will hand back the inner master up there too. The reaper never
/// `exec`s, so `FD_CLOEXEC` does nothing for it — anything it keeps open, it
/// keeps until the cap, which for a retained inner master means postponing the
/// very hangup this backstop exists to guarantee. Measured on the dev box this
/// was found on: `RLIMIT_NOFILE` soft is **524288**, i.e. 512x this floor.
#[cfg(unix)]
const CHILD_GROUP_BACKSTOP_MIN_FD: libc::c_int = 1024;

/// Ceiling for that same fallback loop, so a container's enormous
/// `RLIMIT_NOFILE` cannot turn it into a stall.
///
/// The loop is one `close(2)` per number and its cost is linear; measured on the
/// dev box, in a release build: 1024 calls take **0.41 ms**, 65 536 take
/// **28.8 ms**, and 1 048 576 take **416 ms**. A soft limit of 1 073 741 816 is
/// an ordinary container setting, so the unclamped loop is minutes of syscalls
/// in a process whose whole job is to poll every 250 ms. 65 536 keeps the worst
/// case inside a single poll tick, is paid once per reaper, and still covers
/// 64x more descriptor space than the old fixed bound.
///
/// On Linux none of this runs: `close_range(2)` closes the entire table in one
/// syscall (measured at **1.4 µs** for `close_range(900, UINT_MAX)`), and the
/// loop is only reached if that syscall is unavailable — a pre-5.9 kernel, a
/// seccomp policy that denies it — or the platform is not Linux at all.
#[cfg(unix)]
const CHILD_GROUP_BACKSTOP_MAX_FD: libc::c_int = 65_536;

/// How far the forked reaper's fallback close loop should count, read BEFORE the
/// `fork` so the post-fork path stays async-signal-safe.
///
/// `getrlimit` is not on POSIX's async-signal-safe list, and neither is
/// `sysconf(_SC_OPEN_MAX)`; that is exactly why the old code used a hard-coded
/// number instead of asking. Asking here and passing the answer down as a plain
/// integer keeps the child arm to `close`/`syscall` and gets a real bound.
/// Clamped into [`CHILD_GROUP_BACKSTOP_MIN_FD`]..=[`CHILD_GROUP_BACKSTOP_MAX_FD`]
/// so neither a tiny limit nor `RLIM_INFINITY` can make it useless or endless.
#[cfg(unix)]
fn child_group_backstop_close_ceiling() -> libc::c_int {
    // SAFETY: `getrlimit` fills the `rlimit` it is handed and touches nothing
    // else. A failure leaves the zeroed value, which the clamp lifts to the
    // floor — the old behaviour.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur
    } else {
        0
    };
    let soft = libc::c_int::try_from(soft).unwrap_or(libc::c_int::MAX);
    soft.clamp(CHILD_GROUP_BACKSTOP_MIN_FD, CHILD_GROUP_BACKSTOP_MAX_FD)
}

/// Issue #657: a test-only hard bound on the WRAPPED CHILD's process group that
/// deliberately does NOT depend on this wrapper staying alive.
///
/// [`arm_wrap_self_defense`] bounds the *wrapper*. The child is bounded only
/// transitively — by a reap loop calling [`kill_pid_group`], which requires the
/// wrapper to still be running to call it. Every path that ends a wrapper
/// *without* letting it reap therefore strands the child: an uncatchable
/// `SIGKILL` (the deck's own escalation past
/// [`crate::agent_pty::AGENT_TERMINATE_GRACE`], a
/// registry `force_kill_and_wait`, an OOM kill, a nextest timeout) is not
/// something [`SignalGuard`] can convert into a tidy teardown. And the child is
/// [`child_pre_exec`]'d into its own session, so once the wrapper is gone
/// *nothing above it can signal that group at all* — not the daemon, not the
/// deck, not the test harness's `killpg` of its own group.
///
/// Measured on 2026-08-23: four such Codex children alive at 21–29 minutes with
/// `ppid=1` and `pgrp == sid == own pid`, and historically one at 8 days, with
/// 385 directories / 14.2 GB accrued behind them. Note what an orphan does and
/// does not do to those roots: `cargo xtask clean-e2e-tmp` reads ownership off
/// the **test process's** pid in the root's NAME, not off the orphan, so a root
/// whose owning test is dead is `dead-pid` and reapable once the dead-owner
/// floor passes even with an orphan still sitting in a deleted directory under
/// it.
/// What the orphan costs instead is a persistent unkillable process, the deleted
/// files' disk blocks retained for as long as it runs, a polluted `ps` for every
/// later diagnosis, and the chance of it writing paths back under a root that
/// was just removed.
///
/// So fork a reaper that outlives us. It:
/// - `setsid`s out of the wrapper's process group FIRST — the deck tears a
///   wrapper down with `killpg(wrapper_pgid, …)`, which would otherwise take the
///   reaper down alongside the very wrapper whose death it exists to survive;
/// - closes every inherited descriptor, so it cannot hold the inner PTY master
///   or the child's stdin pipe open and suppress the EOF/`SIGHUP` that would
///   otherwise end the child on its own;
/// - polls the child's group and exits the moment it is gone — the normal case,
///   within one [`CHILD_GROUP_BACKSTOP_POLL`] of any clean teardown;
/// - and, if the group is still alive at the deadline, walks the same
///   `SIGTERM` → [`crate::agent_pty::WRAP_TERMINATE_GRACE`] → `SIGKILL` path the
///   reap loop walks.
///
/// It is itself bounded by deadline + grace and holds no descriptor, so it can
/// never become the leak it exists to prevent.
///
/// **Telling a reaper apart from the leak it hunts.** It is a `fork` of this
/// wrapper, so it keeps the wrapper's argv and shows up in `ps` looking like a
/// second `dot-agent-deck wrap --agent … -- …` at `ppid=1`. Given #657 is partly
/// a story about stale processes producing misleading evidence, the two
/// discriminators are worth knowing, and both were measured on a live pair:
/// a reaper has an **empty** `/proc/<pid>/fd` (it closed everything) where a real
/// wrapper holds ~9, and its `pgid == sid != its own pid` (it inherited the
/// intermediate's session) where a stranded wrapper or agent child has
/// `pgid == sid == its own pid`.
///
/// Env-gated on `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` exactly like
/// [`arm_wrap_self_defense`], and for the same reason: a production wrapper never
/// sets it, forks nothing, and behaves precisely as before.
///
/// Deliberately NOT a substitute for the `setsid` in [`child_pre_exec`]: the
/// wrapper needs the child in its own group so `killpg` targets the child and
/// its descendants and nothing else. This adds a second, independent holder of
/// that same kill rather than trading the grouping away.
#[cfg(unix)]
fn arm_child_group_backstop(child_pid: libc::pid_t) {
    use crate::agent_pty::DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS;
    use crate::daemon::parse_max_lifetime_secs;

    if child_pid <= 0 {
        return;
    }
    let Some(max_lifetime) = std::env::var(DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS)
        .ok()
        .and_then(|v| parse_max_lifetime_secs(&v))
    else {
        return;
    };

    // Read BEFORE the fork: `getrlimit` is not async-signal-safe, and the child
    // arm below must be. It travels down as a plain integer.
    let close_ceiling = child_group_backstop_close_ceiling();

    // SAFETY: `fork` from a threaded process is defined as long as the child
    // touches nothing but async-signal-safe libc calls before `_exit` — which is
    // all either child arm below does. No allocation, no locks, no `tracing`, no
    // Rust destructor runs on those paths.
    let forked = unsafe { libc::fork() };
    match forked {
        -1 => {
            // Non-fatal: the wrapper's own backstop is unaffected, and this net
            // only exists under test. Say so rather than failing the launch.
            tracing::warn!(
                "could not fork the wrapped-child lifetime backstop; a SIGKILL'd \
                 wrapper may strand its agent child"
            );
        }
        // SAFETY (both child arms): see the note on the `fork` above.
        0 => unsafe {
            // Intermediate: escape the wrapper's group, then fork the reaper and
            // exit at once so the reaper is reparented to init instead of
            // lingering as a zombie under a wrapper that never waits for it.
            libc::setsid();
            if libc::fork() == 0 {
                child_group_backstop_main(child_pid, max_lifetime, close_ceiling);
            }
            libc::_exit(0);
        },
        intermediate => {
            // Reap the intermediate immediately; all it does is `setsid`, fork
            // and `_exit`, so this cannot block, and targeting its pid cannot
            // steal the wrapped child's status from the reap loop. `SignalGuard`
            // is installed by now and deliberately does not set `SA_RESTART`, so
            // a signal landing in this window would `EINTR` the wait and leave a
            // zombie behind — retry rather than leak one.
            let mut status: libc::c_int = 0;
            loop {
                // SAFETY: `intermediate` is this process's own child, just forked.
                let reaped = unsafe { libc::waitpid(intermediate, &mut status, 0) };
                if reaped >= 0
                    || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    break;
                }
            }
        }
    }
}

/// Body of the forked reaper described on [`arm_child_group_backstop`]. Runs in
/// a fresh single-threaded process and so restricts itself to async-signal-safe
/// libc calls (`signal`, `close`, `clock_gettime`, `nanosleep`, `killpg`,
/// `_exit`). Never returns.
#[cfg(unix)]
fn child_group_backstop_main(
    child_pid: libc::pid_t,
    max_lifetime: Duration,
    close_ceiling: libc::c_int,
) -> ! {
    // SAFETY: every call here is async-signal-safe, as required after `fork` in
    // a threaded process. `child_pid` is the wrapper's own child, `setsid`'d in
    // its pre-exec, so it is also its group id.
    unsafe {
        // A forked copy inherits `SignalGuard`'s handlers, and a reaper that
        // swallows SIGTERM is its own leak.
        for signo in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            libc::signal(signo, libc::SIG_DFL);
        }
        close_all_descriptors(close_ceiling);

        let deadline = monotonic_millis().saturating_add(millis_saturating(max_lifetime));
        while monotonic_millis() < deadline {
            if !pid_group_alive(child_pid) {
                libc::_exit(0);
            }
            sleep_millis(millis_saturating(CHILD_GROUP_BACKSTOP_POLL));
        }
        if !pid_group_alive(child_pid) {
            libc::_exit(0);
        }
        libc::killpg(child_pid, libc::SIGTERM);

        let escalate = monotonic_millis()
            .saturating_add(millis_saturating(crate::agent_pty::WRAP_TERMINATE_GRACE));
        while monotonic_millis() < escalate {
            if !pid_group_alive(child_pid) {
                libc::_exit(0);
            }
            sleep_millis(50);
        }
        libc::killpg(child_pid, libc::SIGKILL);
        libc::_exit(0);
    }
}

/// Close every descriptor the forked reaper inherited, including 0/1/2.
///
/// The reaper must hold nothing: a retained inner PTY master would suppress the
/// hangup that ends the child on its own, and a retained file or socket would
/// keep a resource alive in a process the wrapper never waits for. It never
/// `exec`s, so `FD_CLOEXEC` is no help to it — closing is the only lever.
///
/// Two strategies, in order. On Linux, `close_range(2)` closes the whole table
/// in one syscall regardless of how high the numbers go; that is a single trap
/// with no libc state behind it, so it is safe after `fork`. Everywhere else —
/// and on a Linux too old for it (pre-5.9) or one whose seccomp policy denies it
/// — fall back to a bounded `close` loop, counting to a ceiling
/// [`child_group_backstop_close_ceiling`] read before the fork.
///
/// `close(2)` on an already-closed descriptor is a harmless `EBADF`, so the loop
/// does not need to know which numbers are live. Both arms are
/// async-signal-safe.
///
/// # Safety
///
/// Runs after `fork` in a threaded process, so the caller must not rely on any
/// descriptor afterwards. Every call here is async-signal-safe.
#[cfg(unix)]
unsafe fn close_all_descriptors(ceiling: libc::c_int) {
    #[cfg(target_os = "linux")]
    {
        // The raw syscall rather than glibc's `close_range` wrapper: the wrapper
        // exists only on `target_env = "gnu"`, and this is the same trap on musl.
        // SAFETY: `close_range` takes three integers and returns one; it touches
        // no memory of ours.
        if unsafe { libc::syscall(libc::SYS_close_range, 0_u32, libc::c_uint::MAX, 0_i32) } == 0 {
            return;
        }
    }
    for fd in 0..ceiling {
        // SAFETY: `close` on an arbitrary integer is defined — an unopened one
        // simply returns `EBADF`.
        unsafe { libc::close(fd) };
    }
}

/// A [`Duration`] as whole milliseconds in the reaper's `i64` clock domain,
/// saturating instead of wrapping.
///
/// `d.as_millis()` is a `u128`, and a bare `as i64` **truncates**: it keeps the
/// low 64 bits and reinterprets them signed. So `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS`
/// values that are merely absurd become actively wrong rather than merely large
/// — `2^54` seconds truncates to a *negative* offset, which makes the deadline
/// already past and the reaper `SIGTERM`s the child on its first tick. That is
/// the exact opposite of what an enormous cap asked for, and it is silent.
/// `i64::MAX` milliseconds is ~292 million years, so saturating is
/// indistinguishable from "no deadline" in practice while staying total.
///
/// Async-signal-safe: pure arithmetic, no allocation, no libc call.
#[cfg(unix)]
fn millis_saturating(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// `CLOCK_MONOTONIC` in milliseconds. Async-signal-safe (`clock_gettime` is on
/// POSIX's list), unlike anything that would allocate or lock — which is why the
/// reaper carries its own clock instead of using [`Instant`].
#[cfg(unix)]
fn monotonic_millis() -> i64 {
    // SAFETY: `clock_gettime` fills the `timespec` it is handed and touches
    // nothing else.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as i64)
        .saturating_mul(1000)
        .saturating_add((ts.tv_nsec as i64) / 1_000_000)
}

/// Async-signal-safe sleep for the forked reaper. A short sleep cut off by a
/// signal simply shortens one poll tick; both loops re-check their deadline
/// against the clock rather than counting ticks, so an early return cannot move
/// a deadline.
#[cfg(unix)]
fn sleep_millis(millis: i64) {
    let ts = libc::timespec {
        tv_sec: (millis / 1000) as libc::time_t,
        tv_nsec: ((millis % 1000) * 1_000_000) as _,
    };
    // SAFETY: `nanosleep` reads one `timespec` and writes nothing through the
    // null remainder pointer.
    unsafe {
        libc::nanosleep(&ts, std::ptr::null_mut());
    }
}

/// Whether any process still belongs to process group `pgid`. A failed
/// `killpg(pgid, 0)` means `ESRCH` here — the reaper and the group share a uid,
/// so `EPERM` is not reachable — i.e. the group is gone and there is nothing
/// left to bound.
#[cfg(unix)]
fn pid_group_alive(pgid: libc::pid_t) -> bool {
    // SAFETY: signal 0 performs the existence/permission check only; it delivers
    // nothing.
    unsafe { libc::killpg(pgid, 0) == 0 }
}

/// PRD #20 finding #12: a RESTORABLE guard that installs async handlers for
/// `SIGTERM` / `SIGHUP` / `SIGINT` and restores the previous dispositions on
/// drop. It is installed BEFORE the child is spawned so a signal arriving in the
/// spawn/setup window is still recorded (and forwarded by the reap loop) instead
/// of terminating the wrapper outright and orphaning the child. `SA_RESTART` is
/// intentionally NOT set so a blocked read returns `EINTR` and the loops react
/// promptly; the pump read loops treat `Interrupted` as retry, not end-of-stream.
#[cfg(unix)]
struct SignalGuard {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

#[cfg(unix)]
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

#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
struct SignalForwarder {
    pid: libc::pid_t,
    escalate_deadline: Option<Instant>,
}

#[cfg(unix)]
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
            // Strictly shorter than the deck's own grace against THIS wrapper —
            // we are the only process that can signal the agent's group, so we
            // have to finish before the deck kills us. See
            // `WRAP_TERMINATE_GRACE`.
            self.escalate_deadline = Some(Instant::now() + crate::agent_pty::WRAP_TERMINATE_GRACE);
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
#[cfg(unix)]
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
    // `TIOCSCTTY`'s integer type differs by platform (e.g. `c_ulong` on Linux,
    // `c_uint` on macOS); cast to `ioctl`'s request type so this compiles on both.
    if ctty_fd >= 0 && unsafe { libc::ioctl(ctty_fd, libc::TIOCSCTTY as _, 0) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Open a fresh inner pseudo-terminal sized to `(rows, cols)`, returning the
/// owned master and slave descriptors.
///
/// Issue #668: the **master** is marked `FD_CLOEXEC` before it can be inherited
/// by anything. `openpty` hands back a plain descriptor, so without this the
/// wrapped child inherits the master of the very terminal it is sitting on —
/// measured on a live stand-in as `fd 3 -> /dev/ptmx` whose `fdinfo`
/// `tty-index` is its own slave. That keeps the master's reference count off
/// zero for as long as the child lives, so when the wrapper dies the slave never
/// hangs up, the child's `read` never returns, and it blocks forever: 221 such
/// orphans were censused on one dev box, the oldest alive 9.4 days, each still
/// holding a working directory the tooling had already deleted (they do NOT pin
/// that root `live-pid` — `clean-e2e-tmp` keys on the test process's pid in the
/// root's name, not on the orphan; what an orphan actually costs is a persistent unkillable process, the deleted files' disk blocks retained for as long as it runs, a polluted `ps` for every later diagnosis, and the chance of it writing paths back under a root that was just removed).
/// `portable_pty` — the crate every *unwrapped* pane is spawned through, and
/// whose panes were measured leaking 0 times in 39 trials — does exactly this at
/// its `unix.rs:57`, which is why the leak is wrapper-shaped.
///
/// This is structural rather than env-gated, so unlike
/// [`arm_child_group_backstop`] it holds on a Ctrl-C'd developer run, an OOM
/// kill, a panic-abort and the deck's own `killpg(SIGKILL)` escalation alike. It
/// is not inert in production, deliberately: a wrapped agent whose wrapper is
/// `SIGKILL`ed now exits with its terminal instead of surviving as an unkillable
/// process holding a dead one.
///
/// The **slave** is marked too, and for a smaller but real reason. `route` hands
/// `Stdio` clones of it (`OwnedFd::try_clone` is `F_DUPFD_CLOEXEC`, so the clones
/// are marked as well), and `std` `dup2`s those onto 0/1/2, which CLEARS
/// `FD_CLOEXEC` on the copy. So the child keeps its terminal on 0/1/2 and, via
/// `TIOCSCTTY` in [`child_pre_exec`], its controlling terminal — `pre_exec` runs
/// *before* exec, so `FD_CLOEXEC` cannot reach its `ioctl` at all. What closes at
/// exec is only the ORIGINAL, which the child has no use for: measured on a live
/// stand-in as a fourth `/dev/pts/<n>` entry at fd 4 beside the intended three.
///
/// That spare cannot retain the master side, so unlike the master it does not
/// recreate the self-pinning defect — when the last master closes, every slave
/// descriptor hangs up together. It is still not merely untidy. It is a
/// read/write **terminal capability** that survives the child or any descendant
/// closing or redirecting its standard streams, from which they can go on
/// reading input or writing wrapper-observed output outside the routes the
/// wrapper set up, and can hold the slave open long enough to force the bounded
/// post-exit drain/kill path below.
#[cfg(unix)]
fn open_inner_pty(rows: u16, cols: u16) -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let mut ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `openpty` fills both descriptors on success; each is turned into an
    // `OwnedFd` exactly once so ownership (and close-on-drop) is unambiguous.
    let rc = unsafe {
        // macOS declares `termp`/`winp` as `*mut` while Linux uses `*const`;
        // `null_mut()` and `&mut ws` satisfy the `*mut` signature and coerce to
        // `*const` on Linux, so this call compiles on both.
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(ws),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `openpty` returned two fresh, valid descriptors; each becomes an
    // `OwnedFd` exactly once here, so close-on-drop is unambiguous from now on.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // Owned first, then flagged: if either fails, both descriptors are already
    // owned and close on the early return instead of leaking (the ordering
    // `portable_pty` uses at `unix.rs:52-58`, and for the same reason). Both
    // ends, exactly as `portable_pty` marks both at that line — the master
    // because inheriting it is the defect, the slave because the child gets its
    // terminal from the `dup2`'d copies on 0/1/2 and never needs the original.
    set_cloexec(&master)?;
    set_cloexec(&slave)?;
    Ok((master, slave))
}

/// Mark `fd` close-on-exec, preserving any other descriptor flags.
///
/// Read-modify-write rather than a bare `F_SETFD`: `FD_CLOEXEC` is the only
/// descriptor flag POSIX defines today, but clobbering the word would be a
/// silent trap for anything a future platform adds there.
#[cfg(unix)]
fn set_cloexec(fd: &OwnedFd) -> std::io::Result<()> {
    // SAFETY: `fd` is a live descriptor this process owns; `F_GETFD`/`F_SETFD`
    // take and return an int, no pointers.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Resize an open PTY master, which sends `SIGWINCH` to its foreground process
/// group (the wrapped child).
#[cfg(unix)]
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

/// Issue #243: how long the wrapped child's output must stay QUIET, after it has
/// written at least one byte, before [`InterfaceWatch`] calls its interface up.
///
/// The signal is the SETTLING, not the first byte. A child that is still coming
/// up is either silent (a launcher that has not printed yet — nothing fires) or
/// still painting (each chunk pushes the deadline out), so what this detects is
/// the transition from producing output to waiting for input. 750 ms is far
/// longer than the gap between two frames of a TUI painting itself and far
/// shorter than the 30 s fallback it replaces; a settle this long followed by an
/// injected prompt is exactly the sequence a human performs by hand.
#[cfg(unix)]
const INTERFACE_SETTLE_WINDOW: Duration = Duration::from_millis(750);

/// Issue #243 audit F3: how long the interface-ready send may spend on the daemon
/// before it gives up.
///
/// This send is the wrapper's ONLY daemon I/O that originates on its supervisory
/// loop, so it is the only one whose latency could reach the code that forwards
/// the user's `Ctrl+C` to the child group. It is already moved off that loop onto
/// a detached thread ([`Emitter::emit_interface_ready`]); this bounds the thread
/// so a wedged or SIGSTOPped daemon leaves nothing behind for the life of the
/// session either.
///
/// Five seconds, the same budget `crate::hook`'s request/response paths give a
/// daemon that is merely busy. Losing the event costs latency and nothing else —
/// the readiness gate falls back to the behaviour it has without this signal —
/// so the bound is deliberately generous rather than tight.
#[cfg(unix)]
const INTERFACE_READY_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Issue #243 (review finding 1): WHICH of [`InterfaceWatch`]'s two facts fired.
///
/// Reported rather than collapsed to a bool because the two are not equally
/// strong and are priced differently by the daemon — see [`InterfaceWatch`] for
/// what each one is worth and why.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterfaceFact {
    /// The child cleared `ICANON`/`ECHO` on the inner PTY — fact 1, an
    /// observation of input-readiness.
    RawInputMode,
    /// The child wrote at least one byte and then stayed quiet for
    /// [`INTERFACE_SETTLE_WINDOW`] — fact 2, a guess.
    OutputSettled,
}

#[cfg(unix)]
impl InterfaceFact {
    /// The `session_start_origin` value this fact rides to the daemon. Distinct
    /// per fact so the daemon can price them separately; see
    /// [`InterfaceWatch`].
    fn origin(self) -> &'static str {
        match self {
            Self::RawInputMode => WRAPPER_INTERFACE_READY_SESSION_START_ORIGIN,
            Self::OutputSettled => WRAPPER_INTERFACE_SETTLED_SESSION_START_ORIGIN,
        }
    }

    /// A short, stable label for the log line — the answer to "why did readiness
    /// fire on this pane?", which used to be computed and then discarded.
    fn reason(self) -> &'static str {
        match self {
            Self::RawInputMode => "raw-input-mode",
            Self::OutputSettled => "output-settled",
        }
    }
}

/// Issue #243: the wrapper's answer to "does the child's interface exist yet?".
///
/// The wrapper is the only party that can answer this. The daemon sees events;
/// the wrapper OWNS the inner PTY the child is painting on and reads every byte
/// that crosses it, so it can watch the interface come up instead of inferring it
/// from an event that arrives too late (Codex's native `SessionStart`) or never
/// (OpenCode has none at all).
///
/// TWO independent facts are accepted — [`InterfaceFact`] — and they are NOT
/// equally strong, which is why the watch reports WHICH one fired rather than a
/// bare bool. They ride distinct `session_start_origin` values so the daemon can
/// price them separately (issue #243 review finding 1), and they are latched
/// SEPARATELY so a session that produces the weak one can still produce the
/// strong one afterwards — which, for the production launch shape, is every
/// session (see [`InterfaceWatch::claim`]):
///
/// 1. **The child took the inner PTY out of cooked mode**
///    ([`InterfaceFact::RawInputMode`]) — it cleared `ICANON` and/or `ECHO`. A
///    genuine OBSERVATION, and the strong one: a child reading raw keystrokes is,
///    by construction, a program that consumes input rather than echoing it, so
///    this cannot be satisfied by a launcher. `devbox`, a shell script and `node`
///    starting up never do it; a full-screen TUI does.
///
///    **What it observes is the AGENT taking the terminal, not the agent being
///    ready for a prompt**, and the difference cost this issue two rounds. It was
///    written here as "a genuine observation of INPUT-READINESS, the exact
///    inverse of the defect the readiness gate exists to prevent" — PRD #225's
///    prompt loss being bytes written into a still-canonical line discipline,
///    echoed back and swallowed. The inverse half is true and the readiness half
///    is not, because a TUI does this as it INITIALIZES, *before* it paints:
///    measured on real codex-cli 0.149.0 at 85 ms after a direct exec, and by
///    `orchestration/delegate/009` at fork + 100 ms, where a prompt written on it
///    parked unsubmitted in the composer and no turn ever started. So this fact
///    is the best RELEASE signal the deck has and still owes a post-readiness
///    buffer on the daemon side; see
///    `crate::state::WRAPPER_INTERFACE_READINESS_BUFFER`.
/// 2. **Output SETTLED** ([`InterfaceFact::OutputSettled`]) — the child wrote
///    something and then stopped for [`INTERFACE_SETTLE_WINDOW`]. The fallback
///    for an interface that stays in cooked mode (a line-oriented REPL, and the
///    test stand-ins), and for the redirected-descriptor paths where no inner PTY
///    termios exists to read.
///
///    **This one is a GUESS, and the wrapper cannot make it a better one AT THE
///    MOMENT IT FIRES.** Silence says the child stopped producing output; it
///    cannot say whether the thing that stopped is an interface waiting at its
///    prompt or a LAUNCHER stalled part-way through its own boot. The production
///    shape is `devbox run codex-big`, which prints one banner line at ~0.1 s and
///    then evaluates its shellenv in silence for a measured 2750–4132 ms before
///    `codex` is exec'd at all — so it satisfies this fact while the pty is still
///    canonical, which is PRD #225 Defect 1 exactly. Nothing observable at this
///    seam distinguishes the two cases *yet*.
///
///    What the wrapper CAN do is not make the guess final. The watch stays armed
///    after announcing it ([`InterfaceWatch::claim`] latches per fact, not per
///    session), so if the launcher was merely slow the strong fact follows later
///    on the same wrapper session — a few seconds warm, and a measured ~15 s on a
///    cold `devbox` that installs packages first — and the daemon holds an upgrade
///    window on fact 2 for exactly that reason rather than releasing on it
///    immediately. See
///    `crate::event::WRAPPER_INTERFACE_SETTLED_SESSION_START_ORIGIN` and
///    `crate::state::INTERFACE_UPGRADE_WINDOW`.
///
/// What is deliberately NOT accepted: elapsed time since `exec`, and the child's
/// FIRST byte. A wall-clock timer fires for a child that has not started, and a
/// first-byte rule fires for a launcher's `Starting…` banner — both would put the
/// deck straight back into writing a prompt into something that is not an agent.
/// A child that writes nothing and never leaves cooked mode is never announced
/// ready by this watch at all, which is correct: nothing about it is observable,
/// so the gate falls back to the behaviour it has today.
///
/// **What this watch observes is the inner PTY, which is not private to the
/// child.** A same-uid process can find it (`/proc/<wrapper-pid>/fd` → the pts
/// node, mode `0620`) and can therefore make either fact fire for a child that is
/// not ready — `tcsetattr` away `ICANON`/`ECHO`, or write one byte and go quiet.
/// The wrapper's report would be honest and the fact would be wrong. That costs
/// nothing beyond what the same process can already do by writing a forged event
/// straight to the daemon's hook socket (issue #243 audit F1/F2), but it does
/// mean "observation, not announcement" is a statement about the HONEST case and
/// not a security property. Do not build a new privilege on it.
#[cfg(unix)]
struct InterfaceWatch {
    /// [`InterfaceFact::RawInputMode`] has been announced. Latching this ends
    /// the watch outright: nothing the child does afterwards can produce a
    /// STRONGER fact, and re-announcing the weaker one would be a downgrade.
    announced_ready: AtomicBool,
    /// [`InterfaceFact::OutputSettled`] has been announced. Latched SEPARATELY
    /// from `announced_ready`, which is the whole of issue #243's regression
    /// fix: a launcher settles, the daemon holds a bounded upgrade window, and
    /// the real agent then clears `ICANON`/`ECHO` behind it. A single shared
    /// latch made the weak fact the LAST word for that session, so the strong
    /// one the wrapper went on to observe was computed and thrown away.
    announced_settled: AtomicBool,
    /// [`monotonic_millis`] of the last byte the child wrote, or `0` while it has
    /// written nothing at all.
    last_output_ms: AtomicI64,
    /// The inner PTY's `c_lflag` as `openpty` handed it over, sampled BEFORE the
    /// child ran, so fact 1 above compares against what this pty actually started
    /// as rather than against an assumed default. `None` when it could not be
    /// read, which simply disables fact 1.
    cooked_lflag: Option<libc::tcflag_t>,
}

#[cfg(unix)]
impl InterfaceWatch {
    fn new(cooked_lflag: Option<libc::tcflag_t>) -> Self {
        Self {
            announced_ready: AtomicBool::new(false),
            announced_settled: AtomicBool::new(false),
            last_output_ms: AtomicI64::new(0),
            cooked_lflag,
        }
    }

    /// Record that the child produced output. Called from every tee, on the raw
    /// byte chunk rather than on a classified line: a TUI can paint a whole frame
    /// of escape sequences without a single `\n` or `\r`, and this must see that.
    fn note_output(&self) {
        // A monotonic clock never yields 0 in practice, but clamp anyway so the
        // "nothing written yet" sentinel cannot be produced by a real write.
        self.last_output_ms
            .store(monotonic_millis().max(1), Ordering::SeqCst);
    }

    /// Has the child cleared `ICANON`/`ECHO` on the inner PTY since it started?
    ///
    /// Read from the MASTER descriptor: a pty's termios is one shared structure,
    /// so the master's `tcgetattr` reports the line discipline the slave-side
    /// child installed.
    fn child_took_raw_input(&self, master_fd: RawFd) -> bool {
        let Some(cooked) = self.cooked_lflag else {
            return false;
        };
        let Some(current) = pty_lflag(master_fd) else {
            return false;
        };
        // Only a CLEARED bit counts. A child that sets additional lflags has not
        // said anything about whether it consumes keystrokes.
        (cooked & !current & (libc::ICANON | libc::ECHO)) != 0
    }

    /// Claim the next unannounced interface fact, returning WHICH one fired.
    /// `None` while nothing new is observable, and `None` forever once
    /// [`InterfaceFact::RawInputMode`] has been claimed.
    ///
    /// The identity of the fact is part of the answer, not a diagnostic
    /// afterthought: the two are priced differently downstream (see
    /// [`InterfaceWatch`]), so a caller that collapses them back to a bool
    /// reintroduces the launcher-settle hazard fact 2 carries.
    ///
    /// **Per fact, not per session** (issue #243 regression fix). This used to
    /// latch once for the whole wrapper, which made whichever fact happened to
    /// fire FIRST the only one the daemon would ever hear — and for the
    /// production launch shape that is always the weak one: `devbox run
    /// codex-big` prints a banner at ~0.1 s and then computes its shellenv in
    /// silence for 2750–4132 ms, so the settle guess fires 2005–3370 ms before
    /// the real `codex` has even been exec'd, let alone taken the terminal out
    /// of cooked mode. Measured over 13 launcher probes and 8 wrapper spawns:
    /// output-settled fired 21/21 and raw-input-mode NEVER fired first, not
    /// once. With one latch the strong fact was computed on the very next tick
    /// after `codex` came up and silently dropped, and the daemon was left
    /// pricing a launcher as an interface.
    ///
    /// So the ORDER a caller can observe is: nothing, or fact 2, or fact 1, or
    /// fact 2 then fact 1 — never fact 1 then fact 2. A child that goes raw
    /// without ever settling still announces only fact 1, exactly as before.
    ///
    /// The cost of staying armed is one `tcgetattr` per supervisory tick for a
    /// child that settles and never goes raw — i.e. forever, for a cooked-mode
    /// REPL. That is the same order as the up-to-three `terminal_size` ioctls
    /// the same 50 ms tick already performs unconditionally, and it buys the
    /// only signal that can tell the two cases apart at all.
    fn claim(&self, master_fd: RawFd) -> Option<InterfaceFact> {
        if self.announced_ready.load(Ordering::SeqCst) {
            return None;
        }
        if self.child_took_raw_input(master_fd) {
            return (!self.announced_ready.swap(true, Ordering::SeqCst))
                .then_some(InterfaceFact::RawInputMode);
        }
        if self.announced_settled.load(Ordering::SeqCst) {
            return None;
        }
        let last = self.last_output_ms.load(Ordering::SeqCst);
        if last == 0 {
            return None;
        }
        if monotonic_millis().saturating_sub(last) < millis_saturating(INTERFACE_SETTLE_WINDOW) {
            return None;
        }
        (!self.announced_settled.swap(true, Ordering::SeqCst))
            .then_some(InterfaceFact::OutputSettled)
    }
}

/// Read a pty's local-mode flags through `fd`, or `None` when it is not a
/// terminal / the call fails.
#[cfg(unix)]
fn pty_lflag(fd: RawFd) -> Option<libc::tcflag_t> {
    // SAFETY: `tcgetattr` fills the `termios` it is handed and touches nothing
    // else; a bad descriptor is reported by the return code, not by a write.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
        return None;
    }
    Some(termios.c_lflag)
}

/// Issue #243: a passthrough writer that tells an [`InterfaceWatch`] the child
/// produced output.
///
/// Wraps the tee's downstream writer rather than changing [`tee`] itself, so the
/// observation sits on the same bytes the user sees and costs one atomic store
/// per chunk. The store happens whether or not the downstream write succeeds —
/// the child wrote those bytes either way, and whether the wrapper could forward
/// them is a different question.
#[cfg(unix)]
struct ActivityWriter<W: Write> {
    inner: W,
    watch: Arc<InterfaceWatch>,
}

#[cfg(unix)]
impl<W: Write> Write for ActivityWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !buf.is_empty() {
            self.watch.note_output();
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
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
///
/// PRD #42 M8: Windows compiling stub — the interactive wrapper runtime needs a
/// ConPTY + Job-Object port (tracked by #163/#164), so on non-Unix this returns
/// a failure rather than silently pretending to wrap. The pure
/// [`wrap_launch_command`] rewrite and the detection layer remain available
/// cross-platform.
#[cfg(not(unix))]
pub fn run_wrap(_agent_override: Option<&str>, _command: &[String]) -> ExitCode {
    eprintln!("Error: `dot-agent-deck wrap` is not supported on this platform yet.");
    ExitCode::FAILURE
}

#[cfg(unix)]
pub fn run_wrap(agent_override: Option<&str>, command: &[String]) -> ExitCode {
    let Some((program, args)) = command.split_first() else {
        eprintln!(
            "Error: `wrap` requires a command after `--`, e.g. `dot-agent-deck wrap -- codex`."
        );
        return ExitCode::FAILURE;
    };

    // Armed before either spawn path so a wrapper orphaned during setup is
    // still covered (mirrors `SignalGuard::install`'s spawn-window rationale).
    arm_wrap_self_defense();

    let agent_type = resolve_agent_type(agent_override, program);
    let pane_id = std::env::var(DOT_AGENT_DECK_PANE_ID).ok();
    // Optional — the daemon injects this on spawn (same pattern as the hook /
    // agent-event paths); a standalone wrap has none.
    let agent_id = std::env::var(DOT_AGENT_DECK_AGENT_ID).ok();
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from));
    // The standalone nonce is the wrapper's own pid: unique across concurrent
    // standalone `wrap` invocations, so two overlapping standalone sessions no
    // longer collide on a fixed `wrap-<program>` id (finding #4/#5). A managed
    // pane ignores the nonce and stays pane-derived.
    let session_id = session_id_for(pane_id.as_deref(), program, &std::process::id().to_string());

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
    // CODEX_HOME (for a `codex` program OR a deck-spawned Codex-identity launcher)
    // and record SCOPED, hash-pinned trust for exactly those hooks so Codex runs
    // them — no invocation-global bypass reaches argv (PRD #20 §4.1). Done once,
    // before either path spawns, and identically for every launch form.
    // PRD #20 Greptile finding #2: the returned `pinned_home` is set explicitly
    // on the spawned child so the home the deck installed into and trusted is
    // exactly the home Codex loads.
    let CodexSpawnPrep { pinned_home } =
        codex_spawn_prep(program, &emitter.agent_type, emitter.pane_id.as_deref());

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
        run_wrap_pty(program, args, &emitter, pinned_home.as_deref(), tty)
    } else {
        run_wrap_pipe(program, args, &emitter, pinned_home.as_deref())
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
#[cfg(unix)]
fn run_wrap_pty(
    program: &str,
    args: &[String],
    emitter: &Arc<Emitter>,
    pinned_codex_home: Option<&std::path::Path>,
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

    // Issue #243: the inner PTY's line discipline as `openpty` handed it over —
    // sampled BEFORE the child exists, so "the child took the terminal out of
    // cooked mode" is measured against this pty's real starting state rather than
    // an assumed default. See `InterfaceWatch::child_took_raw_input`.
    let interface = Arc::new(InterfaceWatch::new(pty_lflag(master.as_raw_fd())));

    // Build the child. `std::process::Command` inherits the wrapper's env (which
    // carries `DOT_AGENT_DECK_PANE_ID` / `_AGENT_ID` injected by the daemon), so
    // the child's own hooks and this wrapper's events attribute to the same pane.
    let mut cmd = StdCommand::new(program);
    cmd.args(args);
    // Finding #2: pin the vetted CODEX_HOME on the child so the home the deck
    // installed into and trusted is exactly the home Codex loads.
    if let Some(home) = pinned_codex_home {
        cmd.env("CODEX_HOME", home);
    }
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
    // Issue #657: armed AFTER the slave copies are dropped and before the reap
    // loop can be interrupted, so the child's group has a bound of its own even
    // if this wrapper is SIGKILL'd a moment from now. No-op outside tests.
    arm_child_group_backstop(child_pid);

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

    // The session has begun — surface the card immediately. PRD #225 M3: this is
    // a CARD-SURFACING signal, not a readiness signal (the child may still be
    // `devbox`/a shell for seconds before the agent TUI exists), so it carries
    // the wrapper-fork origin marker.
    emitter.emit_fork_session_start();

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
        let watch = Arc::clone(&interface);
        Some(std::thread::spawn(move || {
            tee(
                reader,
                ActivityWriter {
                    inner: FdWriter(out_fd),
                    watch,
                },
                |line| {
                    classify_and_emit(line, &detector, &emitter, is_codex);
                },
            );
            output_done.store(true, Ordering::SeqCst);
        }))
    } else {
        None
    };

    // Redirected output descriptors: tee each pipe to the matching real fd.
    let out_pipe_thread = pipe_out.map(|r| {
        spawn_pipe_tee(
            r,
            libc::STDOUT_FILENO,
            emitter,
            &detector,
            is_codex,
            Some(&interface),
        )
    });
    let err_pipe_thread = pipe_err.map(|r| {
        spawn_pipe_tee(
            r,
            libc::STDERR_FILENO,
            emitter,
            &detector,
            is_codex,
            Some(&interface),
        )
    });

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

        // Issue #243: the pre-prompt readiness signal. Polled here rather than
        // pushed from the tee threads because one of the two facts it reads —
        // the inner PTY's line discipline — is state, not an event, and because
        // the settle window has to be noticed by SOMEBODY once the output stops
        // (the tee is blocked in `read` at exactly that moment, so it cannot
        // notice its own silence). This loop already ticks every 50 ms for
        // resizes and child reaping, so the watch costs one atomic load plus, at
        // most, one `tcgetattr` per tick and nothing at all once it has fired.
        //
        // Issue #243 review finding 4 / audit closing note: the FACT is logged as
        // well as acted on. It used to be computed and then dropped
        // (`let _ = reason;`), which threw away the single most useful field
        // diagnostic this mechanism produces — "was this a genuine TUI raw-mode
        // release, or the settle guess?" — at the one place that knows.
        //
        // `info!`, not `debug!`, and the level is load-bearing. The default
        // filter is `dot_agent_deck=info` (`crate::logging`), so a `debug!` here
        // would reach a log file only for an operator who already knew to set
        // `RUST_LOG` — and `crate::state::dispatch_one_owned` tells the reader
        // this fact IS in the wrapper's log, as the alternative to a tuning knob.
        // It fires at most twice per wrapped session (see
        // `InterfaceWatch::claim`), so the volume argument for `debug!` does not
        // apply. Nothing is printed to the terminal in any case: `main`'s
        // `init_logging_from_env` installs a FILE subscriber or none at all, so
        // this cannot put a byte into the pane the child is painting.
        //
        // Called on every tick even after the settle guess has fired, because the
        // strong fact usually arrives SECOND — see `InterfaceWatch::claim`.
        if let Some(fact) = interface.claim(master_fd) {
            tracing::info!(
                reason = fact.reason(),
                origin = fact.origin(),
                "wrap: observed the child's interface; announcing pre-prompt readiness"
            );
            emitter.emit_interface_ready(fact);
        }

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
#[cfg(unix)]
fn run_wrap_pipe(
    program: &str,
    args: &[String],
    emitter: &Arc<Emitter>,
    pinned_codex_home: Option<&std::path::Path>,
) -> ExitCode {
    let mut cmd = StdCommand::new(program);
    cmd.args(args);
    // Finding #2: pin the vetted CODEX_HOME on the child so the home the deck
    // installed into and trusted is exactly the home Codex loads.
    if let Some(home) = pinned_codex_home {
        cmd.env("CODEX_HOME", home);
    }
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
    // Issue #657: same independent bound on the child's group as the PTY path.
    arm_child_group_backstop(child_pid);

    // PRD #225 M3: same fork-time card-surfacing event as the PTY path, and the
    // same marker — it says "a session exists", not "the agent is ready".
    emitter.emit_fork_session_start();

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
        None,
    );
    let err_thread = spawn_pipe_tee(
        child_stderr,
        libc::STDERR_FILENO,
        emitter,
        &detector,
        is_codex,
        None,
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
#[cfg(unix)]
fn spawn_pipe_tee<R: Read + Send + 'static>(
    reader: R,
    out_fd: RawFd,
    emitter: &Arc<Emitter>,
    detector: &Arc<Mutex<Detector>>,
    is_codex: bool,
    interface: Option<&Arc<InterfaceWatch>>,
) -> std::thread::JoinHandle<()> {
    let emitter = Arc::clone(emitter);
    let detector = Arc::clone(detector);
    // Issue #243: `Some` on the interactive path, where a redirected descriptor
    // is still one of the ways a wrapped child's interface can reach the user, so
    // its bytes count as the child painting. `None` on the wholly non-interactive
    // pipe path: there is no terminal there for an interface to exist on, and a
    // batch `codex exec --json` run is never the target of a readiness gate.
    let interface = interface.map(Arc::clone);
    std::thread::spawn(move || match interface {
        Some(watch) => tee(
            reader,
            ActivityWriter {
                inner: FdWriter(out_fd),
                watch,
            },
            |line| {
                classify_and_emit(line, &detector, &emitter, is_codex);
            },
        ),
        None => tee(reader, FdWriter(out_fd), |line| {
            classify_and_emit(line, &detector, &emitter, is_codex);
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure-data pattern-detection tests — plain `#[test]` unit tests (no
    // `#[spec]` / CATALOG reproducer needed: these assert a pure function, not
    // runtime TUI behaviour).

    /// The forked reaper's fallback close loop counts to a real limit, but a
    /// bounded one: never below the old fixed 1024 (so it can only improve on
    /// it), never above 65 536 (so a container's `RLIMIT_NOFILE` of ~1e9 cannot
    /// turn a 250 ms-poll process into 416 ms of `close(2)` calls at startup).
    ///
    /// Issue #668, audit Low 4. Asserts the bracket rather than the value,
    /// because the value is whatever this host's soft limit happens to be — on
    /// the box this was found on, 524 288, which clamps down to the ceiling.
    #[cfg(unix)]
    #[test]
    fn child_group_backstop_close_ceiling_is_clamped_both_ways() {
        let ceiling = child_group_backstop_close_ceiling();

        assert!(
            ceiling >= CHILD_GROUP_BACKSTOP_MIN_FD,
            "the reaper must still close at least the {CHILD_GROUP_BACKSTOP_MIN_FD} \
             descriptors the old fixed bound covered, got {ceiling}"
        );
        assert!(
            ceiling <= CHILD_GROUP_BACKSTOP_MAX_FD,
            "an unbounded ceiling makes the fallback loop a startup stall, got \
             {ceiling}"
        );
    }

    /// Milliseconds out of a `Duration` saturate instead of truncating.
    ///
    /// Issue #668, audit Medium 2: `d.as_millis()` is a `u128` and a bare
    /// `as i64` keeps the low 64 bits, so a cap large enough to set bit 63
    /// lands NEGATIVE — a deadline already in the past, which makes the reaper
    /// signal the child on its first tick instead of bounding it generously.
    /// That is the opposite of what an enormous cap asked for, and silent.
    #[cfg(unix)]
    #[test]
    fn millis_saturating_never_wraps_a_huge_duration() {
        assert_eq!(millis_saturating(Duration::from_millis(0)), 0);
        assert_eq!(millis_saturating(Duration::from_secs(300)), 300_000);
        assert_eq!(millis_saturating(Duration::from_secs(u64::MAX)), i64::MAX);
        // The specific shape that used to invert: `as i64` on this value is
        // negative, so the old code produced an already-expired deadline.
        let inverting = Duration::from_millis(1 << 63);
        assert!((inverting.as_millis() as i64) < 0, "precondition");
        assert_eq!(millis_saturating(inverting), i64::MAX);
    }

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
    /// managed pane (nonce ignored, stable for card continuity), and folds the
    /// per-session nonce into the standalone id so concurrent standalone wrappers
    /// stay distinct (PRD #20 Greptile finding #4/#5).
    #[test]
    fn session_id_derivation() {
        // Managed pane: pane-derived, nonce ignored.
        assert_eq!(
            session_id_for(Some("pane-7"), "codex", "4242"),
            "pane-7-session"
        );
        // Standalone: basename plus the per-session nonce.
        assert_eq!(
            session_id_for(None, "/usr/bin/codex", "4242"),
            "wrap-codex-4242"
        );
        // Distinct nonces (concurrent wrappers) yield distinct standalone ids.
        assert_ne!(
            session_id_for(None, "/bin/sh", "111"),
            session_id_for(None, "/bin/sh", "222"),
        );
    }

    /// PRD #20 M8: a Wrapper-strategy agent's bare command is rewritten to its
    /// `dot-agent-deck wrap --agent <basename> -- <command>` invocation, using
    /// the registry detection basename as the `--agent` alias.
    #[test]
    fn wrap_launch_command_wraps_wrapper_strategy() {
        // The binary is resolved, not hardcoded (see `deck_binary_for_wrap`), so
        // assert the shape: some `dot-agent-deck` followed by the exact wrap
        // invocation. Under `cargo test` the resolved binary is the sibling the
        // build just produced, so this is usually an absolute path.
        let rewritten = wrap_launch_command("codex", &AgentType::Codex);
        let (program, rest) = rewritten
            .split_once(' ')
            .expect("rewritten command has a program and arguments");
        assert_eq!(
            std::path::Path::new(program)
                .file_name()
                .and_then(|n| n.to_str()),
            Some("dot-agent-deck"),
            "the rewrite must name a dot-agent-deck binary; got {rewritten:?}"
        );
        assert_eq!(rest, "wrap --agent codex -- codex");
    }

    /// The rewrite must name THIS build, not whatever `$PATH` resolves to — the
    /// command is run through a login shell that re-reads the user's profile, so
    /// a bare name silently picks up an installed release. Pin that the resolved
    /// program is the binary sitting next to the test harness.
    #[test]
    fn wrap_launch_command_names_this_build_not_path() {
        let rewritten = wrap_launch_command("codex", &AgentType::Codex);
        let program = rewritten.split_once(' ').expect("program present").0;
        let sibling = std::env::current_exe().ok().and_then(|exe| {
            let dir = exe.parent()?;
            let direct = dir.join("dot-agent-deck");
            if direct.is_file() {
                return Some(direct);
            }
            (dir.file_name() == Some(std::ffi::OsStr::new("deps")))
                .then(|| dir.parent().map(|up| up.join("dot-agent-deck")))
                .flatten()
                .filter(|p| p.is_file())
        });
        match sibling {
            Some(expected) => assert_eq!(
                program,
                expected.to_str().expect("sibling path is UTF-8"),
                "must resolve the co-located build, not a $PATH lookup"
            ),
            // No co-located binary (e.g. a bare `cargo test` before any build of
            // the bin target) — falling back to the bare name is the contract.
            None => assert_eq!(program, "dot-agent-deck"),
        }
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

    #[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
    #[test]
    fn raw_mode_guard_restores_termios_on_drop() {
        let (_master, slave) = open_inner_pty(24, 80).expect("open inner pty");
        let fd = slave.as_raw_fd();
        // PENDIN/FLUSHO are driver-managed status bits, not line-discipline mode
        // bits: macOS's tty layer sets PENDIN as a side effect of a canonical-mode
        // change, while Linux leaves them clear. RawModeGuard restores the exact
        // termios it saved; mask these transient bits so the assertion checks the
        // mode the guard actually manages rather than driver status.
        let read_lflag = || {
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(fd, &mut t) }, 0, "tcgetattr");
            t.c_lflag & !(libc::PENDIN | libc::FLUSHO)
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
