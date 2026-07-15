use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    ToolStart,
    ToolEnd,
    Thinking,
    Compacting,
    SubagentStart,
    SubagentStop,
    WaitingForInput,
    PermissionRequest,
    Idle,
    Error,
    SessionStart,
    SessionEnd,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentType {
    ClaudeCode,
    OpenCode,
    Pi,
    /// OpenAI Codex CLI (PRD #20 M7) — the first wrapper-strategy agent. Wired
    /// in via `dot-agent-deck wrap -- codex …`; its events are synthesized from
    /// stdout by [`crate::wrap`] and ride the existing raw-`AgentEvent` socket.
    /// Serializes to `"codex"` (snake_case); resolved from the `codex` binary
    /// basename through the registry ([`crate::agent_registry`]).
    Codex,
    /// "No recognized agent type." Produced by [`AgentType::from_command`] for
    /// any unrecognized binary, mapped to `Option::None` by
    /// [`crate::state::SessionState::live_snapshot`], and rendered as the "No
    /// agent" dashboard placeholder.
    ///
    /// PRD #201 forward-compat catch-all: this variant also carries
    /// `#[serde(other)]`, so any UNRECOGNIZED wire value (e.g. a `pi` record
    /// reaching a pre-Pi reader, or a future agent type reaching today's
    /// build) deserializes here instead of failing the whole `AgentEvent` /
    /// `AgentRecord` decode. That mirrors [`crate::state::SessionStatus::Unknown`]
    /// — but reuses the pre-existing neutral variant rather than adding a new
    /// one: `None` already means "type not known", already maps to the "No
    /// agent" placeholder, and is exactly what an unrecognized binary yields
    /// via `from_command`, so an unknown wire value landing on `None` is the
    /// consistent, non-active outcome (it never masquerades as a real agent).
    /// `#[serde(other)]` is deserialize-only; `None` still serializes to
    /// `"none"` (and `"none"` still deserializes back to `None` via this same
    /// catch-all), so round-trips are unchanged.
    #[serde(other)]
    None,
}

impl AgentType {
    /// PRD #76 M2.13: best-effort inference of agent type from the binary
    /// name in a spawn command. Used by TUI spawn sites to populate
    /// `StartAgentOptions.agent_type` so the daemon's registry can echo it
    /// back via `list_agents` and a remote reconnect can build placeholder
    /// sessions with the correct type instead of "No agent".
    ///
    /// Returns `Some(AgentType)` only for recognized agent binaries
    /// (`claude` → `ClaudeCode`, `opencode` → `OpenCode`, `pi` → `Pi`,
    /// `codex` → `Codex`);
    /// unknown commands and `None` input return `None` so the daemon stores
    /// "type not known yet" rather than misclassifying. Whitespace
    /// before the binary name is ignored to match shell-style invocations.
    ///
    /// PRD #20 M2: the per-agent basename→type mapping now lives in the agent
    /// registry ([`crate::agent_registry`]); this fn keeps the command-parsing
    /// (basename extraction, arg stripping) and delegates the lookup. The
    /// recognized set and the "unknown → `None`" behaviour are unchanged.
    ///
    /// PRD #20 finding #19: the parser is no longer limited to the first
    /// whitespace token. It tokenizes the command with quote awareness and
    /// conservatively looks through common launch forms so a Wrapper-strategy
    /// agent behind a launcher is still detected and wrapped:
    /// - leading `VAR=VALUE` assignments and an `env`/`sudo` prefix (with their
    ///   own option flags/assignments) are skipped to reach the real binary;
    /// - a quoted executable path (`"/opt/OpenAI Codex/codex"`) resolves by its
    ///   basename;
    /// - a shell launcher (`sh -c '<script>'`, `bash -lc "<script>"`) is
    ///   recursed into via its `-c` script argument.
    ///
    /// Everything still degrades to `None` for an unrecognized binary, so a
    /// non-agent command is never misclassified.
    pub fn from_command(cmd: Option<&str>) -> Option<Self> {
        let tokens = tokenize_command(cmd?);
        detect_from_tokens(&tokens, DETECT_RECURSION_BUDGET)
    }
}

/// PRD #20 finding R20-016: hard cap on shell-launcher recursion, decremented
/// across [`detect_from_tokens`] calls (NOT reset per call). A deeply nested
/// `sh -c "sh -c \"sh -c …\""` can otherwise recurse until stack exhaustion;
/// with an explicit budget each `-c` level costs one unit and the chain
/// terminates safely at `None`.
const DETECT_RECURSION_BUDGET: usize = 8;

/// Short options (matched WITH their leading dash) that consume the FOLLOWING
/// token as their argument for a given launcher, so command detection skips both
/// the option and its argument to reach the real binary (`sudo -u root codex`,
/// `env -u FOO codex`). Conservative allow-list: only these consume the next
/// token; every other flag is treated as self-contained.
fn launcher_option_takes_arg(launcher: &str, opt: &str) -> bool {
    match launcher {
        "sudo" => matches!(
            opt,
            "-u" | "-g" | "-h" | "-p" | "-C" | "-D" | "-R" | "-T" | "-U" | "-r" | "-t"
        ),
        "env" => matches!(opt, "-u" | "-C" | "-S"),
        _ => false,
    }
}

/// Whether `arg` is a shell SHORT-option cluster that selects command mode
/// (`-c`) — e.g. `-c`, `-lc`, `-ic`. Only a single-dash cluster of ASCII-letter
/// options containing `c` counts; a LONG option like `--rcfile` (which merely
/// happens to contain the letter `c`) is deliberately NOT command mode, so a
/// startup-file path is never mistaken for a `-c` script.
fn is_shell_command_flag(arg: &str) -> bool {
    match arg.strip_prefix('-') {
        Some(rest) if !rest.is_empty() && !rest.starts_with('-') => {
            rest.chars().all(|c| c.is_ascii_alphabetic()) && rest.contains('c')
        }
        _ => false,
    }
}

/// Whether `token` looks like a leading `NAME=VALUE` environment assignment
/// (e.g. `FOO=1`) rather than a program or path. Conservative: `NAME` must be a
/// shell-identifier-shaped run before the first `=`, so a path that merely
/// contains `=` isn't misread as an assignment.
fn is_env_assignment(token: &str) -> bool {
    match token.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && name.chars().next().is_some_and(|c| !c.is_ascii_digit())
        }
        None => false,
    }
}

/// Resolve the agent type from an already-tokenized command, looking through
/// `env`/`sudo`/shell-launcher prefixes. See [`AgentType::from_command`].
fn detect_from_tokens(tokens: &[String], budget: usize) -> Option<AgentType> {
    // R20-016: the recursion budget is decremented across calls (a `-c` script
    // recurse below spends one unit), so a deeply nested `sh -c "sh -c …"` can no
    // longer recurse until stack exhaustion — it terminates at `None`.
    if budget == 0 {
        return None;
    }
    let mut idx = 0;
    // Bound the number of prefix hops (`env`/`sudo` chains) within one frame.
    for _ in 0..8 {
        // Skip a run of leading environment assignments (`FOO=1 codex`).
        while tokens.get(idx).is_some_and(|t| is_env_assignment(t)) {
            idx += 1;
        }
        let token = tokens.get(idx)?;
        let basename = std::path::Path::new(token)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(token.as_str());

        // `env` / `sudo` prefix: skip the launcher and any of its own flags,
        // `VAR=VALUE` assignments, and — R20-016 — the ARGUMENT of an
        // option that consumes one (`sudo -u root`, `env -u FOO`), then
        // re-resolve from the next real token. `--` ends option parsing.
        if basename == "env" || basename == "sudo" {
            idx += 1;
            while let Some(next) = tokens.get(idx) {
                if next == "--" {
                    idx += 1;
                    break;
                } else if is_env_assignment(next) {
                    idx += 1;
                } else if next.starts_with('-') {
                    // A single-dash short option may consume the following
                    // token as its argument (e.g. `-u root`); a `--long` option
                    // either bundles its value (`--unset=FOO`) or is a flag, so
                    // never over-consume for those.
                    let consumes =
                        !next.starts_with("--") && launcher_option_takes_arg(basename, next);
                    idx += 1;
                    if consumes && tokens.get(idx).is_some_and(|t| !t.starts_with('-')) {
                        idx += 1;
                    }
                } else {
                    break;
                }
            }
            continue;
        }

        // Shell launcher: `sh -c '<script>'`, `bash -lc "<script>"`. Recurse into
        // the script argument that follows a valid command-mode short-option
        // cluster ([`is_shell_command_flag`]); a shell with no `-c` (or one given
        // only a `--rcfile`-style long option) stays an unrecognized binary.
        if matches!(basename, "sh" | "bash" | "zsh" | "dash" | "ksh" | "ash") {
            let mut j = idx + 1;
            while let Some(arg) = tokens.get(j) {
                if arg.starts_with('-') {
                    if is_shell_command_flag(arg)
                        && let Some(script) = tokens.get(j + 1)
                    {
                        let inner = tokenize_command(script);
                        return detect_from_tokens(&inner, budget - 1);
                    }
                    j += 1;
                } else {
                    break;
                }
            }
            return crate::agent_registry::detect_from_basename(basename);
        }

        // An ordinary binary token — resolve it directly.
        return crate::agent_registry::detect_from_basename(basename);
    }
    None
}

/// Split a command string into whitespace-separated tokens, honoring single and
/// double quotes (the quote characters are stripped, and whitespace inside a
/// quoted run is preserved so a quoted executable path stays one token). This is
/// a deliberately small shell-word splitter — enough for agent detection
/// ([`AgentType::from_command`]), not a full POSIX parser.
fn tokenize_command(cmd: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut in_single = false;
    let mut in_double = false;
    for c in cmd.chars() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                started = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                started = true;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if started {
                    tokens.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        tokens.push(cur);
    }
    tokens
}

/// PRD #20 M3: the concrete handle, if any, through which a session's input is
/// delivered — the `kind` half of a [`LiveTarget`] descriptor. Serializes
/// kebab-case (`process`, `pty`, `tmux`, `sdk`, `none`). Purely descriptive: it
/// tells the UI what *kind* of thing (if any) backs the session; whether it can
/// actually be written to *now* is the separate [`Writable`] axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    /// A child process the producer owns (e.g. a `dot-agent-deck wrap` child).
    Process,
    /// A pseudo-terminal the daemon controls — the live, writable Claude
    /// Code / OpenCode / Pi pane case.
    Pty,
    /// A `tmux` pane/window.
    Tmux,
    /// An in-process SDK/agent handle.
    Sdk,
    /// No concrete handle — the session is known only from history/logs.
    ///
    /// Forward-compat catch-all (matches [`AgentType::None`]): a future/unknown
    /// `kind` on the wire deserializes here via `#[serde(other)]` instead of
    /// failing the whole `LiveTarget`/`AgentEvent` decode. Deserialize-only —
    /// `None` still serializes to `"none"`.
    #[serde(other)]
    None,
}

/// PRD #20 M3: whether the dashboard can deliver input to a session right now —
/// the `writable` half of a [`LiveTarget`] descriptor. Serializes kebab-case
/// (`live`, `history-only`, `none`).
///
/// A dashboard-visible session is not necessarily a live, writable target:
/// today's Claude/OpenCode/Pi panes are `Live` (a PTY the daemon drives), but a
/// wrapped Codex session surfaced via [`crate::wrap`] is `HistoryOnly` — the
/// user's keystrokes reach the child through the inherited terminal, not a
/// daemon-controlled handle, so the *dashboard* cannot inject live input. The UI
/// reads this to render non-`Live` sessions distinctly and to refuse (with
/// honest feedback) an attempt to type into a card that can't accept input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Writable {
    /// Input can be delivered to the running session now.
    Live,
    /// The session can only be resumed/replayed from history — no live write.
    HistoryOnly,
    /// Neither live write nor history resume — view-only.
    ///
    /// Forward-compat catch-all (matches [`AgentType::None`]): a future/unknown
    /// `writable` on the wire deserializes here via `#[serde(other)]` — the
    /// safe, non-writable outcome — instead of failing the decode.
    /// Deserialize-only; `None` still serializes to `"none"`.
    #[serde(other)]
    None,
}

/// PRD #20 M3: a per-session descriptor of whether/how a session can receive
/// input. Carried on [`AgentEvent::live_target`] (optional + additive) so an
/// adapter can declare that the session it surfaces is a live PTY target, a
/// history-only wrapper session, or view-only — and the UI never invites users
/// to type into a card that can't accept input.
///
/// See the "Liveness & Write Semantics" section of PRD #20: the `kind`
/// ([`TargetKind`]) names the concrete handle and `writable` ([`Writable`])
/// names what can be done with it now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveTarget {
    pub kind: TargetKind,
    pub writable: Writable,
}

/// PRD #20 M3: the honest outcome of delivering input to a session, returned
/// instead of a fire-and-forget `Result<(), _>`. Serializes kebab-case; every
/// variant keeps a distinct public wire value so a caller can tell accepted
/// input apart from a stale, wrong, or unwritable target.
///
/// Rides the daemon wire on [`crate::daemon_protocol::AttachResponse::send_result`]
/// as an additive, optional field (a missing value decodes to `None`), so it is
/// forward-compatible and needs no `PROTOCOL_VERSION` bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SendResult {
    /// Delivered to the live target.
    Applied,
    /// Accepted, not yet confirmed applied.
    Queued,
    /// Target moved on / our view was behind.
    Stale,
    /// The handle no longer maps to the session we meant.
    WrongSession,
    /// No live target — only history resume is possible.
    HistoryOnly,
    /// Nothing to write to.
    NoLiveTarget,
}

/// PRD #201 M1.2 (test-plan row 3): map a lifecycle **state** string an agent's
/// extension reports via `dot-agent-deck agent-event --type <state>` to the
/// [`EventType`] that drives the target pane's card status. This is the single
/// production seam the CLI subcommand and the fast-tier status tests share.
///
/// The canonical `--type` vocabulary is exactly three states — `running`,
/// `waiting`, `finished`. Anything else returns `None` so the subcommand can
/// reject an unknown `--type` with a clear non-zero error instead of silently
/// emitting a wrong (or default) status. The Phase 2 extension and the docs
/// MUST use the same three strings.
pub fn agent_event_type_from_state(state: &str) -> Option<EventType> {
    match state {
        "running" => Some(EventType::Thinking),
        "waiting" => Some(EventType::WaitingForInput),
        "finished" => Some(EventType::Idle),
        _ => None,
    }
}

/// `AgentEvent.metadata` key carrying a human-friendly card title (PRD #127
/// finding #2). The daemon's live-surface path (`surface_spawned_pane`) sets
/// this to the schedule's task name so an ALREADY-ATTACHED TUI titles the
/// live card with the friendly name — matching what a disconnect/reconnect
/// already renders from the daemon registry's `display_name`. Real agent hooks
/// don't emit it; consumers treat its absence as "no friendly name known".
pub const DISPLAY_NAME_METADATA_KEY: &str = "display_name";

/// PRD #20 M1: current schema version of the [`AgentEvent`] JSON wire shape.
///
/// This versions the **payload shape of a single `AgentEvent` record** — the
/// stable public JSON schema documented on [`AgentEvent`] below. It is
/// DISTINCT from [`crate::daemon_protocol::PROTOCOL_VERSION`], which versions
/// the **attach-socket handshake/framing** between a TUI and the daemon. The
/// two move independently: adding an optional, serde-skipped field to
/// `AgentEvent` (as M1 does) bumps neither, because old and new peers stay
/// wire-compatible; a breaking change to the *attach handshake* bumps only
/// `PROTOCOL_VERSION`; a breaking change to the *event record shape* would
/// bump only this constant. Do not conflate them.
///
/// Producers MAY stamp [`AgentEvent::schema_version`] with this value to
/// advertise which schema they wrote. It stays at `1` for the current shape;
/// a future, non-additive change to the record's fields bumps it. Because the
/// field is optional and skipped when `None`, existing producers that leave it
/// unset emit byte-identical JSON to before, and a consumer treats a missing
/// `schema_version` as the baseline (v1) schema.
pub const AGENT_EVENT_SCHEMA_VERSION: u32 = 1;

/// Stable public JSON schema for a single agent event.
///
/// `AgentEvent` is the wire record every agent integration (Claude Code hooks,
/// the OpenCode plugin, Pi's `agent-event` CLI, and future wrapper adapters)
/// serializes to the daemon's hook socket, and that the daemon re-broadcasts to
/// attached TUIs over `KIND_EVENT` (wrapped in [`BroadcastMsg::Event`]). Third
/// parties author events against this schema, so it is a **stable public API**:
/// fields are added additively (optional + serde-skipped so old and new
/// payloads round-trip unchanged), never repurposed. The record's schema
/// version is [`AGENT_EVENT_SCHEMA_VERSION`] (distinct from the attach-wire
/// [`crate::daemon_protocol::PROTOCOL_VERSION`] — see that constant's docs).
///
/// ## JSON schema (field · type · optionality · meaning · producers)
///
/// - **`session_id`** · string · **required** · stable id that groups events
///   into a single dashboard card. Set by every producer.
/// - **`agent_type`** · enum ([`AgentType`], snake_case) · **required** · which
///   agent produced the event. Claude hooks set `claude_code`, the OpenCode
///   plugin sets `open_code`, Pi's `agent-event` CLI sets `pi`, the
///   live-surface path derives it from the spawn command via
///   [`AgentType::from_command`]. Unrecognized values decode to `none` (the
///   `#[serde(other)]` catch-all), never failing the whole-record decode.
/// - **`event_type`** · enum ([`EventType`], snake_case) · **required** · the
///   lifecycle/tool event that drives the card's status. Set by every producer.
/// - **`tool_name`** · string · optional (omitted/`null` ⇒ `None`) · the tool
///   for `tool_start` / `tool_end` events. Set by the Claude/OpenCode hook
///   builders; `None` for pure lifecycle events.
/// - **`tool_detail`** · string · optional · short human-readable detail for a
///   tool event (e.g. the file path or command). Set by the hook builders.
/// - **`cwd`** · string · optional · working directory of the session. Set by
///   hooks and the live-surface path; used for orchestration bucketing.
/// - **`timestamp`** · string (RFC 3339 / ISO 8601 UTC) · **required** · when
///   the event was produced. Set by every producer.
/// - **`user_prompt`** · string · optional · truncated text of the user prompt
///   that triggered the turn. Set by hooks when a prompt is present.
/// - **`metadata`** · object (string→string) · optional (defaults to empty) ·
///   free-form extra keys, e.g. [`DISPLAY_NAME_METADATA_KEY`], `bash_command`,
///   `permission_state`. Consumers treat unknown keys as ignorable.
/// - **`pane_id`** · string · optional · the `DOT_AGENT_DECK_PANE_ID` the event
///   routes to. Populated from the env var the daemon injects at spawn; `None`
///   for events not scoped to a known pane.
/// - **`agent_id`** · string · optional · daemon-side registry id of the
///   producing agent (from `DOT_AGENT_DECK_AGENT_ID`). Lets agent-id-scoped
///   filters (e.g. post-respawn `SessionStart` waits) target the right agent;
///   `None` payloads simply don't match those filters.
/// - **`agent_version`** · string · optional (**PRD #20 M1**, added additively)
///   · self-reported version of the agent binary/integration that produced the
///   event (e.g. a Codex/Claude CLI version), for diagnostics and
///   version-aware rendering. No current producer sets it; `None` (the default,
///   omitted from the wire) means "version not reported".
/// - **`schema_version`** · integer · optional (**PRD #20 M1**, added
///   additively) · the [`AGENT_EVENT_SCHEMA_VERSION`] the producer wrote, for
///   forward compatibility. The wrapper adapter ([`crate::wrap`]) stamps it on
///   every event it emits; native hooks currently omit it. `None` (the default,
///   omitted from the wire) is read as the baseline (v1) schema. This is the
///   **event-record** schema version, NOT the attach-wire
///   [`crate::daemon_protocol::PROTOCOL_VERSION`].
/// - **`live_target`** · object (`{ "kind": <TargetKind>, "writable":
///   <Writable> }`, both kebab-case) · optional (**PRD #20 M3**, added
///   additively; omitted/`null` ⇒ `None`) · declares whether/how the session
///   can receive dashboard input (see [`LiveTarget`]). Producers: the wrapper
///   adapter ([`crate::wrap`]) stamps it on every event — `pty`/`live` when it
///   runs inside a daemon-managed pane (its child is reachable through that
///   live PTY), else `process`/`history-only` for a standalone wrap. Native
///   PTY panes (Claude/OpenCode/Pi) omit it, which the UI reads as the
///   historical `live`/writable default. Absence never fails the decode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub session_id: String,
    pub agent_type: AgentType,
    pub event_type: EventType,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_detail: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub user_prompt: Option<String>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    #[serde(default)]
    pub pane_id: Option<String>,
    /// PRD #92 F9 followup-7: daemon-side registry id of the agent
    /// that produced this hook event. Populated by the agent's hook
    /// script from the `DOT_AGENT_DECK_AGENT_ID` env var the daemon
    /// injects at spawn time (same pattern as
    /// [`crate::agent_pty::DOT_AGENT_DECK_PANE_ID`]). Lets the
    /// post-respawn dispatch task scope its `SessionStart` wait to
    /// the NEW agent's id, so a late `SessionStart` from the OLD
    /// agent — emitted in the subscribe→kill window — can't be
    /// mis-accepted as the NEW agent's readiness signal. Optional
    /// because hook payloads from external agents (or test forgers)
    /// may omit it; events with `None` simply won't match
    /// agent-id-scoped filters.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// PRD #20 M1: self-reported version of the agent binary/integration that
    /// produced this event (e.g. a wrapped Codex or Claude CLI version), for
    /// diagnostics and version-aware rendering. Optional and additive:
    /// `#[serde(default)]` lets older payloads that lack the field deserialize
    /// to `None`, and `skip_serializing_if` omits it from the wire when unset —
    /// so existing producers emit byte-identical JSON and old/new peers stay
    /// compatible. No current producer sets it; `None` means "version not
    /// reported".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    /// PRD #20 M1: the [`AGENT_EVENT_SCHEMA_VERSION`] the producer wrote, for
    /// forward compatibility of this record's JSON shape. This is the
    /// **event-record** schema version and is DISTINCT from the attach-socket
    /// [`crate::daemon_protocol::PROTOCOL_VERSION`] (see those docs). Optional
    /// and additive for the same reasons as `agent_version`: a missing value
    /// deserializes to `None` and is read as the baseline (v1) schema, and it
    /// is omitted from the wire when unset. The wrapper adapter
    /// ([`crate::wrap`]) stamps it on every event it emits; native hooks
    /// currently leave it unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    /// PRD #20 M3: per-session live-target descriptor declaring whether/how the
    /// session can receive input (see [`LiveTarget`]). Optional and additive:
    /// `#[serde(default)]` lets legacy payloads that predate the field
    /// deserialize to `None`, and `skip_serializing_if` omits it from the wire
    /// when unset — so existing producers emit byte-identical JSON and old/new
    /// peers stay compatible. The wrapper adapter ([`crate::wrap`]) stamps
    /// `pty`/`live` when it runs inside a daemon-managed pane and
    /// `process`/`history-only` for a standalone wrap; native PTY panes leave
    /// it `None`, which the UI reads as the historical live/writable default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_target: Option<LiveTarget>,
}

/// Envelope for messages sent to the daemon over the Unix socket.
///
/// Existing hook senders transmit raw `AgentEvent` JSON (no `message_type` field).
/// New message types (e.g. `WorkDone`) include `"message_type": "work_done"` so the
/// daemon can distinguish them.  The daemon tries `DaemonMessage` first, then falls
/// back to `AgentEvent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "message_type")]
pub enum DaemonMessage {
    /// Orchestrator delegates work to one or more worker roles.
    #[serde(rename = "delegate")]
    Delegate(DelegateSignal),
    /// Worker (or orchestrator with `done`) reports task completion.
    #[serde(rename = "work_done")]
    WorkDone(WorkDoneSignal),
    /// PRD #201 native prompt delivery: a READ-ONLY request for the seed the
    /// daemon prepared for a pane, so the pane's extension can deliver it
    /// NATIVELY (`pi.sendUserMessage`) instead of the daemon typing it into the
    /// PTY. Unlike the two fire-and-forget signals above, this one gets a
    /// reply: the daemon writes a [`GetSeedResponse`] JSON line back on the
    /// same connection, then the seed is cleared (delivered exactly once). An
    /// older daemon that doesn't know this variant fails to parse it and sends
    /// no reply — the `get-seed` CLI then reports an empty seed, the extension
    /// no-sends, and the daemon's PTY-injection safety net still delivers. So
    /// this variant degrades gracefully across versions (see the rule-12 note
    /// in `docs/develop/versioning.md`): it rides the unversioned hook socket
    /// and does NOT move the attach `PROTOCOL_VERSION`.
    #[serde(rename = "get_seed")]
    GetSeed(GetSeedRequest),
}

/// PRD #201: payload of [`DaemonMessage::GetSeed`] — the pane whose pending
/// seed the caller wants. Sourced from `DOT_AGENT_DECK_PANE_ID` by the
/// `get-seed` CLI (same pane-scoping the delegate / work-done / agent-event
/// verbs use).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetSeedRequest {
    pub pane_id: String,
}

/// PRD #201: the daemon's reply to a [`DaemonMessage::GetSeed`], written as a
/// single JSON line back on the hook-socket connection. `seed` is `None`
/// (serialized as `null`) when no seed is pending for the pane — the pane is
/// unknown, or the seed was already delivered (pulled or injected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetSeedResponse {
    #[serde(default)]
    pub seed: Option<String>,
}

/// Signal sent by the orchestrator via `dot-agent-deck delegate`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegateSignal {
    pub pane_id: String,
    pub task: String,
    /// Role names to delegate to (one or more).
    pub to: Vec<String>,
    pub timestamp: DateTime<Utc>,
}

/// Daemon → attached-TUI broadcast (PRD #76 M2.17). The daemon publishes
/// one of these per ingested hook event; subscribers receive them as
/// `KIND_EVENT` frames on the attach socket.
///
/// PRD #93 round-5: the `Delegate` / `WorkDone` variants used to ride this
/// channel too, because the daemon couldn't validate or dispatch them
/// locally in external-daemon mode (the role map lived on the TUI side).
/// The daemon now owns the role map and the PTY registry, so it dispatches
/// those signals directly into the target pane's PTY — no broadcast hop,
/// no replay buffer, no salvage. Only hook events keep using this channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum BroadcastMsg {
    /// A hook event (existing M2.17 wire shape, now wrapped).
    #[serde(rename = "event")]
    Event(AgentEvent),
    /// PRD #120: a daemon-spawned ORCHESTRATION (the issue-dispatch path),
    /// pushed to already-attached TUIs so they can build the orchestration tab
    /// LIVE — mid-session, with no reconnect. The single-agent live-surface
    /// path (a synthetic [`EventType::SessionStart`] painted as a flat
    /// dashboard card by [`crate::state::AppState::apply_event`]) cannot
    /// reconstruct a multi-role tab, and orchestration tabs were previously
    /// rebuilt ONLY at TUI hydration (startup / reconnect). This variant
    /// carries the structural membership the TUI's
    /// `open_orchestration_tab_with_existing_role_panes` machinery needs to
    /// build the tab on the fly.
    ///
    /// Adding this variant changes the `KIND_EVENT` payload schema (an older
    /// peer would mis-parse the new `kind` tag), so it bumps
    /// [`crate::daemon_protocol::PROTOCOL_VERSION`].
    #[serde(rename = "orchestration_surface")]
    OrchestrationSurface(OrchestrationSurface),
}

/// PRD #120: the structural membership of a daemon-spawned orchestration,
/// pushed to attached TUIs (via [`BroadcastMsg::OrchestrationSurface`]) so they
/// can build the orchestration tab live. Mirrors what the hydration partition
/// (`OrchestrationHydrationBucket`) reconstructs from per-pane
/// [`crate::agent_pty::TabMembership`] records at reconnect — but for a spawn
/// that happens WHILE a TUI is attached.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrchestrationSurface {
    /// Canonical orchestration name — the tab IDENTITY and (absent a
    /// `display_title`) the tab-strip LABEL.
    pub name: String,
    /// Absolute orchestration cwd shared by every role pane — the tab's cwd and
    /// the hydration partition's bucket key.
    pub cwd: String,
    /// Optional user-facing tab title; `None` falls back to `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_title: Option<String>,
    /// The spawned role panes, in role order.
    pub roles: Vec<OrchestrationSurfaceRole>,
}

/// One role pane of a live-surfaced orchestration (see [`OrchestrationSurface`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrchestrationSurfaceRole {
    /// The `DOT_AGENT_DECK_PANE_ID` the daemon tagged the pane with — reused as
    /// the TUI-side local pane id so hook events keep routing correctly. The TUI
    /// attaches to the live PTY by resolving THIS pane id through `list_agents`
    /// (see `EmbeddedPaneController::hydrate_pane`), not by a registry agent id —
    /// so no `agent_id` rides on the wire.
    pub pane_id: String,
    /// Position of this role in the orchestration config's `roles`.
    pub role_index: usize,
    /// Role name (e.g. `orchestrator`, `worker`).
    pub role_name: String,
    /// Whether this is the start (orchestrator) role.
    pub is_start_role: bool,
}

/// Signal sent by a worker via `dot-agent-deck work-done`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkDoneSignal {
    pub pane_id: String,
    pub task: String,
    /// When true, the orchestrator signals that the entire orchestration is complete.
    #[serde(default)]
    pub done: bool,
    pub timestamp: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_event() {
        let json = r#"{
            "session_id": "abc-123",
            "agent_type": "claude_code",
            "event_type": "tool_start",
            "tool_name": "Read",
            "tool_detail": "src/main.rs",
            "cwd": "/home/user/project",
            "timestamp": "2026-03-22T10:00:00Z",
            "metadata": {"key": "value"}
        }"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.session_id, "abc-123");
        assert_eq!(event.agent_type, AgentType::ClaudeCode);
        assert_eq!(event.event_type, EventType::ToolStart);
        assert_eq!(event.tool_name.as_deref(), Some("Read"));
    }

    #[test]
    fn parse_minimal_event() {
        let json = r#"{
            "session_id": "abc-123",
            "agent_type": "claude_code",
            "event_type": "idle",
            "timestamp": "2026-03-22T10:00:00Z"
        }"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        assert!(event.tool_name.is_none());
        assert!(event.tool_detail.is_none());
        assert!(event.cwd.is_none());
        assert!(event.metadata.is_empty());
    }

    // PRD #20 M1: an AgentEvent JSON written by an OLDER producer — one that
    // predates the `agent_version` / `schema_version` fields — must still
    // deserialize. This pins the backward-compatibility half of the "stable
    // public JSON schema" contract: adding the two optional fields cannot break
    // decoding of any previously-emitted payload.
    #[test]
    fn parse_event_without_new_version_fields_defaults_to_none() {
        let json = r#"{
            "session_id": "abc-123",
            "agent_type": "claude_code",
            "event_type": "idle",
            "timestamp": "2026-03-22T10:00:00Z"
        }"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        assert!(
            event.agent_version.is_none(),
            "a payload lacking agent_version must decode it as None"
        );
        assert!(
            event.schema_version.is_none(),
            "a payload lacking schema_version must decode it as None (read as baseline v1)"
        );
    }

    // PRD #20 M1: with the new fields SET, the event round-trips through JSON
    // unchanged — the forward half of the schema contract (a newer producer's
    // richer payload survives a serialize→deserialize cycle).
    #[test]
    fn round_trip_event_with_new_version_fields() {
        let event = AgentEvent {
            session_id: "rt-1".into(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::Thinking,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-03-22T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: None,
            agent_id: None,
            agent_version: Some("codex-1.2.3".into()),
            schema_version: Some(AGENT_EVENT_SCHEMA_VERSION),
            live_target: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        // Both fields must appear on the wire when set…
        assert!(json.contains("\"agent_version\":\"codex-1.2.3\""), "{json}");
        assert!(json.contains("\"schema_version\":1"), "{json}");
        // …and survive the decode.
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent_version.as_deref(), Some("codex-1.2.3"));
        assert_eq!(back.schema_version, Some(AGENT_EVENT_SCHEMA_VERSION));
    }

    // PRD #20 M1: `skip_serializing_if = "Option::is_none"` means an event that
    // leaves the new fields unset emits BYTE-IDENTICAL JSON to before they
    // existed — the keys are absent, not `null`. This is what keeps existing
    // producers behaviour-preserving and old/new peers wire-compatible.
    #[test]
    fn none_version_fields_are_omitted_from_the_wire() {
        let event = AgentEvent {
            session_id: "min-1".into(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::Idle,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-03-22T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: None,
            agent_id: None,
            agent_version: None,
            schema_version: None,
            live_target: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            !json.contains("agent_version"),
            "None agent_version must be omitted from the wire, not serialized as null: {json}"
        );
        assert!(
            !json.contains("schema_version"),
            "None schema_version must be omitted from the wire, not serialized as null: {json}"
        );
    }

    #[test]
    fn parse_event_with_user_prompt() {
        let json = r#"{
            "session_id": "abc-123",
            "agent_type": "claude_code",
            "event_type": "thinking",
            "user_prompt": "fix the login bug",
            "timestamp": "2026-03-22T10:00:00Z"
        }"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.user_prompt.as_deref(), Some("fix the login bug"));
    }

    #[test]
    fn parse_event_without_user_prompt() {
        let json = r#"{
            "session_id": "abc-123",
            "agent_type": "claude_code",
            "event_type": "tool_start",
            "timestamp": "2026-03-22T10:00:00Z"
        }"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        assert!(event.user_prompt.is_none());
    }

    #[test]
    fn reject_invalid_event_type() {
        let json = r#"{
            "session_id": "abc-123",
            "agent_type": "claude_code",
            "event_type": "unknown_type",
            "timestamp": "2026-03-22T10:00:00Z"
        }"#;
        assert!(serde_json::from_str::<AgentEvent>(json).is_err());
    }

    // PRD #76 M2.13: pin the AgentType::from_command inference rules.
    // Spawn-site callers (orchestration roles, new-pane form, session
    // restore) feed the daemon's `StartAgent.agent_type` through this
    // helper so the hydrated dashboard card on reconnect has the right
    // type. The mapping must be stable: a regression that flips the
    // `claude` → ClaudeCode arm would silently strand every reconnected
    // pane back at "No agent".
    #[test]
    fn agent_type_from_command_recognizes_claude() {
        assert_eq!(
            AgentType::from_command(Some("claude")),
            Some(AgentType::ClaudeCode)
        );
        // Full path also resolves via file_name().
        assert_eq!(
            AgentType::from_command(Some("/usr/local/bin/claude")),
            Some(AgentType::ClaudeCode)
        );
        // Args after the binary are ignored.
        assert_eq!(
            AgentType::from_command(Some("claude --dangerously-skip-permissions")),
            Some(AgentType::ClaudeCode)
        );
    }

    #[test]
    fn agent_type_from_command_recognizes_opencode() {
        assert_eq!(
            AgentType::from_command(Some("opencode")),
            Some(AgentType::OpenCode)
        );
        assert_eq!(
            AgentType::from_command(Some("/opt/bin/opencode --foo")),
            Some(AgentType::OpenCode)
        );
    }

    // PRD #201 M1.1 (test-plan row 1): pin the `pi` → AgentType::Pi mapping
    // so a plain `pi` pane and a scheduled `pi` job are recognized as a
    // first-class agent type, and reassert claude/opencode as a regression
    // guard — the same detection path feeds all three. Mirrors the path/arg
    // shapes covered for claude/opencode above.
    #[test]
    fn agent_type_from_command_recognizes_pi() {
        assert_eq!(AgentType::from_command(Some("pi")), Some(AgentType::Pi));
        // Full path also resolves via file_name().
        assert_eq!(
            AgentType::from_command(Some("/usr/local/bin/pi")),
            Some(AgentType::Pi)
        );
        // Args after the binary are ignored.
        assert_eq!(
            AgentType::from_command(Some("pi --some-flag")),
            Some(AgentType::Pi)
        );
        // No regression: claude/opencode still map to their own types.
        assert_eq!(
            AgentType::from_command(Some("claude")),
            Some(AgentType::ClaudeCode)
        );
        assert_eq!(
            AgentType::from_command(Some("opencode")),
            Some(AgentType::OpenCode)
        );
    }

    #[test]
    fn agent_type_from_command_returns_none_for_unknown_or_empty() {
        // Non-agent commands must NOT misclassify — the daemon would
        // otherwise echo a wrong type via list_agents and the dashboard
        // would mislabel non-agent panes on reconnect.
        assert!(AgentType::from_command(Some("sh")).is_none());
        assert!(AgentType::from_command(Some("/bin/bash")).is_none());
        assert!(AgentType::from_command(Some("vim")).is_none());
        assert!(AgentType::from_command(None).is_none());
        // Whitespace-only / empty input also stays None.
        assert!(AgentType::from_command(Some("")).is_none());
        assert!(AgentType::from_command(Some("   ")).is_none());
    }

    // PRD #201 (rule-12 wire safety): `AgentType` gained a wire-serialized `Pi`
    // variant. Without a `#[serde(other)]` fallback, a NEWER daemon emitting
    // `agent_type = "pi"` would break an OLDER reader's WHOLE-response decode
    // (`list_agents` / the `KIND_EVENT` broadcast), stranding its agent list.
    // The `#[serde(other)]` on `AgentType::None` makes any unrecognized value —
    // a `pi` record at a pre-Pi reader, OR a future agent type at today's build
    // — deserialize to the neutral `None` ("No agent") placeholder instead of
    // erroring, so this class of break can never repeat for a future type.
    #[test]
    fn unknown_agent_type_deserializes_to_none_fallback() {
        // The enum directly: an entirely unknown future value.
        let ty: AgentType = serde_json::from_str("\"someunknownfuturetype\"").unwrap();
        assert_eq!(ty, AgentType::None);

        // The value carried in a full `AgentEvent` (the real wire shape a
        // subscriber decodes over `KIND_EVENT`): the unknown `agent_type` must
        // NOT fail the whole-event decode.
        let json = r#"{
            "session_id": "fwd-compat-1",
            "agent_type": "someunknownfuturetype",
            "event_type": "thinking",
            "timestamp": "2026-03-22T10:00:00Z"
        }"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.agent_type, AgentType::None);
        assert_eq!(event.event_type, EventType::Thinking);

        // `#[serde(other)]` is deserialize-only: `None` still round-trips
        // through its own `"none"` name, so serialization is unaffected.
        assert_eq!(serde_json::to_string(&AgentType::None).unwrap(), "\"none\"");
        assert_eq!(
            serde_json::from_str::<AgentType>("\"none\"").unwrap(),
            AgentType::None
        );
        // And the recognized values still map to their own variants (regression
        // guard: the catch-all must not swallow known types).
        assert_eq!(
            serde_json::from_str::<AgentType>("\"pi\"").unwrap(),
            AgentType::Pi
        );
        assert_eq!(
            serde_json::from_str::<AgentType>("\"claude_code\"").unwrap(),
            AgentType::ClaudeCode
        );
    }

    #[test]
    fn parse_open_code_event() {
        let json = r#"{
            "session_id": "oc-456",
            "agent_type": "open_code",
            "event_type": "session_start",
            "timestamp": "2026-03-22T10:00:00Z"
        }"#;
        let event: AgentEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.agent_type, AgentType::OpenCode);
        assert_eq!(event.event_type, EventType::SessionStart);
    }

    #[test]
    fn serialize_deserialize_delegate_signal() {
        let signal = DelegateSignal {
            pane_id: "pane-1".into(),
            task: "Implement login".into(),
            to: vec!["coder".into()],
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-04-17T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let msg = DaemonMessage::Delegate(signal);
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: DaemonMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonMessage::Delegate(s) => {
                assert_eq!(s.pane_id, "pane-1");
                assert_eq!(s.task, "Implement login");
                assert_eq!(s.to, vec!["coder"]);
            }
            _ => panic!("expected Delegate"),
        }
    }

    #[test]
    fn serialize_deserialize_work_done_signal() {
        let signal = WorkDoneSignal {
            pane_id: "pane-2".into(),
            task: "Implemented login".into(),
            done: false,
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-04-17T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let msg = DaemonMessage::WorkDone(signal);
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: DaemonMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonMessage::WorkDone(s) => {
                assert_eq!(s.pane_id, "pane-2");
                assert_eq!(s.task, "Implemented login");
                assert!(!s.done);
            }
            _ => panic!("expected WorkDone"),
        }
    }

    #[test]
    fn serialize_deserialize_get_seed_request() {
        // PRD #201: the get-seed request carries the pane id and tags itself
        // `message_type: "get_seed"` so the daemon's hook loop can distinguish
        // it from the fire-and-forget delegate / work-done signals.
        let msg = DaemonMessage::GetSeed(GetSeedRequest {
            pane_id: "pane-7".into(),
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            json.contains("\"message_type\":\"get_seed\""),
            "get-seed must be tagged so an OLD daemon that doesn't know it fails \
             to parse and simply doesn't reply (graceful degradation): {json}"
        );
        let parsed: DaemonMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            DaemonMessage::GetSeed(r) => assert_eq!(r.pane_id, "pane-7"),
            _ => panic!("expected GetSeed"),
        }
    }

    #[test]
    fn serialize_deserialize_get_seed_response() {
        // Some(seed) round-trips…
        let resp = GetSeedResponse {
            seed: Some("Read .dot-agent-deck/worker-task-coder.md for your task.".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: GetSeedResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.seed.as_deref(),
            Some("Read .dot-agent-deck/worker-task-coder.md for your task.")
        );
        // …and "no seed" is a null the get-seed CLI reads as "print nothing".
        let none = GetSeedResponse { seed: None };
        let json = serde_json::to_string(&none).unwrap();
        assert_eq!(json, "{\"seed\":null}");
        let back: GetSeedResponse = serde_json::from_str(&json).unwrap();
        assert!(back.seed.is_none());
    }

    #[test]
    fn work_done_signal_defaults() {
        let json = r#"{
            "message_type": "work_done",
            "pane_id": "pane-2",
            "task": "Done",
            "timestamp": "2026-04-17T10:00:00Z"
        }"#;
        let msg: DaemonMessage = serde_json::from_str(json).unwrap();
        match msg {
            DaemonMessage::WorkDone(s) => {
                assert!(!s.done);
            }
            _ => panic!("expected WorkDone"),
        }
    }

    // PRD #120: the live-orchestration-surface broadcast must round-trip
    // through the same `BroadcastMsg` wire the daemon forwards over KIND_EVENT,
    // and tag itself `orchestration_surface` so it's distinguishable from the
    // `event` variant an older peer expects (the reason PROTOCOL_VERSION bumped).
    #[test]
    fn orchestration_surface_broadcast_round_trips() {
        let msg = BroadcastMsg::OrchestrationSurface(OrchestrationSurface {
            name: "issue-work".into(),
            cwd: "/work/github-issues/.worktrees/issue-1".into(),
            display_title: None,
            roles: vec![
                OrchestrationSurfaceRole {
                    pane_id: "sched-github-issues-0-r0".into(),
                    role_index: 0,
                    role_name: "orchestrator".into(),
                    is_start_role: true,
                },
                OrchestrationSurfaceRole {
                    pane_id: "sched-github-issues-0-r1".into(),
                    role_index: 1,
                    role_name: "worker".into(),
                    is_start_role: false,
                },
            ],
        });
        let json = serde_json::to_string(&msg).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["kind"], "orchestration_surface");
        // `display_title: None` is omitted from the wire (skip_serializing_if).
        assert!(
            v.as_object().unwrap().get("display_title").is_none(),
            "None display_title must be omitted from the wire payload"
        );

        let back: BroadcastMsg = serde_json::from_str(&json).unwrap();
        let BroadcastMsg::OrchestrationSurface(s) = back else {
            panic!("expected a BroadcastMsg::OrchestrationSurface");
        };
        assert_eq!(s.name, "issue-work");
        assert_eq!(s.roles.len(), 2);
        assert_eq!(s.roles[0].role_name, "orchestrator");
        assert!(s.roles[0].is_start_role);
        assert_eq!(s.roles[1].pane_id, "sched-github-issues-0-r1");
        assert_eq!(s.roles[1].role_index, 1);
    }

    // PRD #201 M1.2 (test-plan row 3): pin the lifecycle-state → EventType
    // mapping the `dot-agent-deck agent-event --type <state>` subcommand and
    // the fast-tier status tests both consume. The three canonical states must
    // map to the exact EventTypes that drive Thinking / WaitingForInput / Idle
    // card statuses, and any other string must return None so the CLI rejects
    // an unknown `--type` non-zero rather than emitting a wrong status.
    #[test]
    fn agent_event_type_from_state_maps_canonical_lifecycle_states() {
        assert_eq!(
            agent_event_type_from_state("running"),
            Some(EventType::Thinking)
        );
        assert_eq!(
            agent_event_type_from_state("waiting"),
            Some(EventType::WaitingForInput)
        );
        assert_eq!(
            agent_event_type_from_state("finished"),
            Some(EventType::Idle)
        );
        // Unknown / malformed states map to None (the CLI turns this into a
        // clear non-zero error). Includes casing and near-miss variants.
        assert_eq!(agent_event_type_from_state("idle"), None);
        assert_eq!(agent_event_type_from_state("Running"), None);
        assert_eq!(agent_event_type_from_state("done"), None);
        assert_eq!(agent_event_type_from_state(""), None);
    }

    #[test]
    fn agent_event_not_parseable_as_daemon_message() {
        let json = r#"{
            "session_id": "abc-123",
            "agent_type": "claude_code",
            "event_type": "idle",
            "timestamp": "2026-03-22T10:00:00Z"
        }"#;
        assert!(serde_json::from_str::<DaemonMessage>(json).is_err());
    }
}
