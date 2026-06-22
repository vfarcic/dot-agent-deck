//! Streaming attach protocol for the daemon (PRD #76, M1.2).
//!
//! # Protocol versioning
//!
//! [`PROTOCOL_VERSION`] is the on-the-wire shape of this module. Bump it when
//! a change would cause an older or newer peer to mis-parse a frame:
//!
//! - **Bump:** new `KIND_*` codes, payload-schema changes that aren't
//!   forward-compatible (renames, type changes, removed fields without a
//!   `#[serde(default)]` shim), new [`AttachRequest`] variants.
//! - **Do NOT bump:** additive optional fields tagged
//!   `#[serde(default, skip_serializing_if = "Option::is_none")]` — those are
//!   forward-compatible by design (older peer ignores the field, newer peer
//!   tolerates its absence).
//!
//! The handshake itself ([`AttachRequest::Hello`]) is enforced only by the
//! laptop-side `connect` flow — single-binary in-process call sites already
//! match versions by construction and don't need the check.
//!
//! # Wire format
//!
//! Length-prefixed binary frames:
//!
//! ```text
//! +-------+--------------------+----------------------+
//! | 1 B   | 4 B (big-endian)   | N bytes              |
//! | kind  | payload length     | payload              |
//! +-------+--------------------+----------------------+
//! ```
//!
//! Justification: PRD line 294 explicitly rules out gRPC / JSON-RPC and
//! "extra build deps". We have `tokio` and `serde_json` already, so control
//! frames carry JSON and stream frames carry raw PTY bytes — no new deps,
//! and the framing is portable to stdio (M2.1). No socket-only assumptions
//! (no fd passing, no `SCM_RIGHTS`).
//!
//! # Frame kinds
//!
//! | Kind            | Direction         | Payload                       |
//! |-----------------|-------------------|-------------------------------|
//! | `KIND_REQ`      | client → server   | JSON [`AttachRequest`]        |
//! | `KIND_RESP`     | server → client   | JSON [`AttachResponse`]       |
//! | `KIND_STREAM_OUT` | server → client | raw PTY bytes                 |
//! | `KIND_STREAM_IN`  | client → server | raw bytes for PTY stdin       |
//! | `KIND_DETACH`     | client → server | empty — detach, leave agent   |
//! | `KIND_STREAM_END` | server → client | optional reason (e.g. lagged) |
//! | `KIND_EVENT`      | server → client | JSON [`crate::event::BroadcastMsg`] (M2.17/M2.19, after a `SubscribeEvents` request) |
//! | `KIND_SHUTDOWN`   | client → server | empty — shut the daemon down (PRD #92 F1) |
//! | `KIND_SHUTDOWN_ACK` | server → client | empty — acknowledges `KIND_SHUTDOWN` before teardown begins (PRD #92 F1 followup) |
//!
//! # Per-connection state machine
//!
//! 1. Client sends a single `KIND_REQ` with one of the [`AttachRequest`]
//!    variants.
//! 2. Server replies with `KIND_RESP` carrying [`AttachResponse`].
//! 3. For non-streaming ops (`list-agents`, `start-agent`, `stop-agent`,
//!    `snapshot`) the server then closes the connection. `snapshot` may
//!    emit one `KIND_STREAM_OUT` frame with the scrollback bytes, followed
//!    by `KIND_STREAM_END` and close.
//! 4. For `attach-stream`, the server immediately follows the OK response
//!    with a single `KIND_STREAM_OUT` carrying the consistent scrollback
//!    snapshot, then enters streaming mode: live PTY bytes flow as
//!    `KIND_STREAM_OUT`, client keystrokes flow as `KIND_STREAM_IN`, and
//!    either side may end via `KIND_DETACH` (client) or `KIND_STREAM_END`
//!    (server, e.g. agent died or subscriber lagged).
//!
//! # Concurrent attach
//!
//! Multiple clients may attach to the same agent. They share a single
//! [`crate::agent_pty::AgentBus`]: each subscriber gets its own broadcast
//! receiver, so PTY output fans out to every attached client. Each client's
//! `KIND_STREAM_IN` is forwarded through a shared writer (under
//! `tokio::sync::Mutex`), so concurrent keystrokes interleave at byte
//! granularity — last writer wins per byte, which matches PRD line 199's
//! "daemon is the single source of truth" model.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tracing::{error, info, warn};

pub use crate::agent_pty::TabMembership;
use crate::agent_pty::{AgentPtyRegistry, AgentRecord, SpawnOptions};
use crate::agent_pty::{DOT_AGENT_DECK_PANE_ID, is_valid_pane_id_env};
use crate::event::{AgentType, BroadcastMsg};
use crate::pane_input::escape_bytes_for_log;
use crate::state::SharedState;

// ---------------------------------------------------------------------------
// Frame kinds
// ---------------------------------------------------------------------------

pub const KIND_REQ: u8 = 0x01;
pub const KIND_RESP: u8 = 0x02;
pub const KIND_STREAM_OUT: u8 = 0x10;
pub const KIND_STREAM_IN: u8 = 0x11;
pub const KIND_STREAM_END: u8 = 0x12;
pub const KIND_DETACH: u8 = 0x13;
/// PRD #76 M2.17: server → client JSON-encoded `AgentEvent` forwarded over
/// a long-lived `SubscribeEvents` connection. The TUI's remote-mode
/// `AppState` is otherwise disconnected from the daemon's hook ingestion
/// loop; this frame is the bridge.
pub const KIND_EVENT: u8 = 0x14;
/// PRD #92 F1: client → server header-only frame meaning "shut the daemon
/// down now." Triggered by the **Stop** option in the Ctrl+C dialog. The
/// daemon validates the frame is header-only (rejects any non-empty
/// payload — see followup hardening at the handler), sends back a
/// [`KIND_SHUTDOWN_ACK`] **before** beginning teardown, then iterates
/// its agent registry, SIGTERMs each child with a short grace before
/// SIGKILL, then exits. Idempotent on the daemon side
/// (`AgentPtyRegistry::shutdown_all_graceful` guards via a latch).
pub const KIND_SHUTDOWN: u8 = 0x15;
/// PRD #92 F1 followup: server → client header-only frame acknowledging
/// receipt of a well-formed [`KIND_SHUTDOWN`]. Sent **before** the daemon
/// begins teardown so the TUI can distinguish "daemon acknowledged"
/// from "old daemon closed the connection on an unknown frame" — the
/// original F1 wire used socket-close as the implicit ack, which was
/// indistinguishable from the upgrade-mismatch case (a daemon predating
/// `PROTOCOL_VERSION = 2` would close the connection on an unknown
/// frame kind). With the explicit ack the client treats
/// EOF-without-ack, an unrecognised frame, and the 1-second timeout
/// alike as errors, surfaces them via `ui.status_message`, and does
/// not exit the TUI — the user can retry, Detach, or `pkill` from a
/// shell.
pub const KIND_SHUTDOWN_ACK: u8 = 0x16;

/// PRD #76 M2.21: wire-format version for the attach socket. Bump every time
/// the on-the-wire shape changes in a way an older client/daemon would
/// mis-parse — new `KIND_*` codes, payload schema changes, new request
/// variants. PRD #76 has accumulated several silent bumps (M2.17 added
/// `KIND_EVENT`, M2.19 changed its payload to `BroadcastMsg`, earlier
/// milestones added `Resize` / `SetAgentLabel` / `SubscribeEvents`); this
/// constant starts at the first post-M2.19 version so older daemons fail the
/// handshake instead of silently dropping live updates.
///
/// Additive `#[serde(default, skip_serializing_if = "Option::is_none")]`
/// fields do NOT require a bump — they're forward-compatible by design. See
/// the module-level "Protocol versioning" section for the full bump policy.
pub const PROTOCOL_VERSION: u32 = 3;

/// Hard cap on a single frame's payload length. Defends against a malicious
/// or buggy peer trying to allocate gigabytes off a forged length prefix.
/// 16 MiB is well above any reasonable PTY chunk or scrollback snapshot.
const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Bounded timeout for a single STREAM_OUT/STREAM_END write to a client. If
/// a client stops draining its socket, the OS send buffer fills and our
/// `write_all` blocks forever — which would also block lag detection (we
/// can't drain the broadcast receiver). With a per-write timeout, a wedged
/// client is dropped within this many seconds instead of pinning the output
/// task. 5s is a generous upper bound for "client can't accept a frame";
/// the client can reattach and replay scrollback.
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Wire I/O
// ---------------------------------------------------------------------------

/// Read a single frame. Returns `Ok(None)` on clean EOF before any header
/// bytes have been read (peer closed the connection cleanly between frames).
/// EOF *after* one or more header bytes is a truncated frame and returns
/// `Err(UnexpectedEof)` — the peer closed mid-header. Likewise EOF inside
/// the payload returns an error via `read_exact`.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    let mut filled = 0usize;
    while filled < header.len() {
        let n = r.read(&mut header[filled..]).await?;
        if n == 0 {
            if filled == 0 {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("truncated frame header: {filled}/5 bytes before EOF"),
            ));
        }
        filled += n;
    }
    let kind = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds {MAX_FRAME_LEN}"),
        ));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload).await?;
    }
    Ok(Some((kind, payload)))
}

/// Try to write a single frame within `CLIENT_WRITE_TIMEOUT`. Returns
/// `true` on success and `false` if the write timed out or errored — the
/// caller should treat both as "client gone" and bail out.
async fn write_or_timeout<W: AsyncWrite + Unpin>(w: &mut W, kind: u8, payload: &[u8]) -> bool {
    matches!(
        tokio::time::timeout(CLIENT_WRITE_TIMEOUT, write_frame(w, kind, payload)).await,
        Ok(Ok(()))
    )
}

/// Write a single frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    kind: u8,
    payload: &[u8],
) -> io::Result<()> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame length {} exceeds {MAX_FRAME_LEN}", payload.len()),
        ));
    }
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&header).await?;
    if !payload.is_empty() {
        w.write_all(payload).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Message types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum AttachRequest {
    ListAgents,
    /// Spawn an agent process attached to a PTY.
    ///
    /// **Trust boundary.** The attach socket is bound at mode `0o600` and
    /// only accepts connections from the same OS user as the daemon, so
    /// any peer reaching this request can already exec arbitrary code as
    /// that user. We deliberately do **not** sandbox `command`, `cwd`, or
    /// `env`: there is no allowlist, no policy layer, no shell-quoting
    /// validation. Adding any of those here would be security theater —
    /// the same user has equivalent local-exec capability via `sh -c`,
    /// and the daemon's job is to expose PTY plumbing, not to be a
    /// privilege boundary. Multi-tenant or remote scenarios must be
    /// handled at a different layer (separate UID, container, SSH).
    StartAgent {
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default = "default_rows")]
        rows: u16,
        #[serde(default = "default_cols")]
        cols: u16,
        #[serde(default)]
        env: Vec<(String, String)>,
        /// M2.11: human-readable label captured into the daemon's per-agent
        /// state. `skip_serializing_if` keeps the on-the-wire shape
        /// backwards-compatible with daemons predating this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
        /// M2.12: which tab (mode / orchestration) the spawning UI placed
        /// this agent pane in. Stored on the daemon-side registry and
        /// echoed back via `list_agents` so the TUI can rebuild tab
        /// structure on reconnect. `None` = dashboard pane. Same
        /// `skip_serializing_if` pattern as `display_name` for forward
        /// compat with daemons that don't know about this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_membership: Option<TabMembership>,
        /// M2.13: which AI agent the spawn command runs (inferred at the
        /// TUI spawn site via `AgentType::from_command`). Stored on the
        /// daemon-side registry and echoed back via `list_agents` so a
        /// remote reconnect can build placeholder sessions with the
        /// correct agent_type instead of "No agent". Same
        /// `skip_serializing_if` pattern as the other M2.x fields for
        /// forward compat with daemons that don't know about it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_type: Option<AgentType>,
    },
    StopAgent {
        id: String,
    },
    AttachStream {
        id: String,
    },
    Snapshot {
        id: String,
    },
    /// Propagate a TUI-side pane resize to the daemon's PTY. The daemon
    /// ioctls `TIOCSWINSZ` on the master, which the kernel mirrors to the
    /// slave and SIGWINCH's the foreground process. Without this op,
    /// stream-backed panes show width/height mismatches versus the local
    /// vt100 view (see PRD #76, M2.10).
    Resize {
        id: String,
        rows: u16,
        cols: u16,
    },
    /// M2.11: update the daemon-side display_name and cwd for an agent.
    /// Either field may be `None` to clear it. Used by the TUI's rename
    /// flow so renamed panes survive an ssh drop without a separate file
    /// on disk — the daemon's per-agent state is the source of truth.
    SetAgentLabel {
        id: String,
        #[serde(default)]
        display_name: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    /// PRD #100: atomic write-and-submit RPC. Routes the client's
    /// `pane_id` + `text` straight to
    /// [`crate::agent_pty::AgentPtyRegistry::write_to_pane_and_submit`]
    /// on the daemon side, which holds the per-agent writer mutex across
    /// the full `payload → SUBMIT_DELAY → CR` sequence (PRD #93 round-8
    /// atomic contract). Lets a TUI client trigger the same atomic
    /// byte stream the daemon-initiated orchestration-delegate path
    /// already produces — without the two-`STREAM_IN`-frames-with-150ms-gap
    /// pattern, whose mid-sequence mutex release lets a concurrent
    /// daemon-initiated write interleave and fuse a daemon-side CR onto
    /// the user's payload, submitting it prematurely. The user's
    /// trailing CR then lands in an empty input box and is rendered as
    /// a newline — PRD #100's "Enter inserted a newline instead of
    /// submitting" symptom.
    WriteAndSubmit {
        pane_id: String,
        text: String,
    },
    /// PRD #76 M2.17: long-lived subscription to the daemon's
    /// `AgentEvent` broadcast. Server replies with an OK `RESP` then
    /// streams `KIND_EVENT` frames (one per hook event) until either side
    /// closes the connection or the broadcast receiver lags. The TUI in
    /// remote mode opens exactly one of these on startup so its
    /// `AppState` mirrors the daemon's view of live agent activity (agent
    /// type, tool counts, prompts, last-activity timestamps).
    SubscribeEvents,
    /// PRD #76 M2.21: protocol-version handshake. Client sends its
    /// [`PROTOCOL_VERSION`]; server replies with its own in
    /// [`AttachResponse::server_version`]. The daemon never rejects on
    /// `client_version` — only the client decides whether to fail (the
    /// `connect` strict path) or continue (call sites that have no version
    /// dependency).
    ///
    /// PRD #103 M1.2: optional `client_build_version` carries the client's
    /// compiled-in `DAD_BUILD_ID`. The daemon logs it but never rejects on
    /// it — mirroring the server-policy on `client_version`. Older clients
    /// omit the field; deserialization tolerates that via
    /// `#[serde(default)]`.
    Hello {
        client_version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_build_version: Option<String>,
    },
    /// PRD #127 M1.3: re-read the global `schedules.toml` and diff/replace the
    /// daemon's registered scheduled-task set without a restart. The handler
    /// replies `ok = true` with the names of the now-registered ENABLED tasks
    /// in [`AttachResponse::agents`]. The CLI's mutating subcommands send this
    /// after an atomic write so a running daemon picks the change up live.
    ReloadSchedules,
    /// PRD #127 M1.5: fire a registered scheduled task's callback immediately
    /// (the `schedule run-now` CLI door). Replies `ok = true` if the run
    /// started or was skipped (prior run still active), `ok = false` if no
    /// such task is registered.
    RunNow {
        name: String,
    },
}

fn default_rows() -> u16 {
    24
}
fn default_cols() -> u16 {
    80
}

/// PRD #161 M1.1: a snapshot of the agents the daemon is currently managing,
/// carried additively on the [`AttachResponse`] reply to an
/// [`AttachRequest::Hello`]. The shared TUI↔daemon restart prompt (Part A)
/// and the remote `connect` nudge (Part B) both need to state
/// "N running agents: alpha, beta" *before* recycling the daemon, so the
/// handshake reply carries both the `count` and the human-readable `names`.
///
/// Additive + optional on the wire: an older daemon omits the enclosing
/// `running_agents` field entirely (it deserializes to `None` via
/// `#[serde(default)]`), and an older client simply ignores it. This needs
/// no `PROTOCOL_VERSION` bump. `count` and `names` are kept as separate
/// fields (rather than relying on `names.len()`) so a future option B
/// classification can advertise a count without enumerating names if it ever
/// wants to — keeping the shape forward-compatible (PRD #161 D2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningAgentsSummary {
    /// Number of agents the daemon is currently managing.
    pub count: usize,
    /// Display names of those agents, in registry order. Each entry is the
    /// agent's `display_name` when set, falling back to its id, so the prompt
    /// always has a label to show. `#[serde(default)]` lets a payload that
    /// carried only a count decode the names as an empty `Vec`.
    #[serde(default)]
    pub names: Vec<String>,
}

impl RunningAgentsSummary {
    /// Build a summary from the daemon's live [`AgentRecord`]s. The label for
    /// each agent is its `display_name` when present, otherwise its id, so the
    /// restart prompt / connect nudge always has something to print.
    pub fn from_records(records: &[AgentRecord]) -> Self {
        let names = records
            .iter()
            .map(|r| r.display_name.clone().unwrap_or_else(|| r.id.clone()))
            .collect::<Vec<_>>();
        Self {
            count: records.len(),
            names,
        }
    }
}

/// Discriminated by the populated optional fields rather than a tag, since
/// each request type has a fixed shape and clients can decide what to read
/// based on which request they sent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AttachResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Legacy listing field: just the ids. Always populated by current
    /// daemons so older clients (which only know about `agents`) keep
    /// working. New clients prefer `agent_records` when present so they
    /// also get the captured `DOT_AGENT_DECK_PANE_ID` per agent — see the
    /// M2.x rehydration path in `embedded_pane::hydrate_from_daemon`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<String>>,
    /// Additive companion to `agents`, carrying each agent's spawn-time
    /// `DOT_AGENT_DECK_PANE_ID`. Older daemons omit this field; newer
    /// clients fall back to `agents` when it's `None` so a stale daemon
    /// is forward-compatible (panes hydrate with freshly-allocated ids
    /// instead of preserved ones).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_records: Option<Vec<AgentRecord>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// PRD #76 M2.21: server's [`PROTOCOL_VERSION`], populated in response to
    /// a [`AttachRequest::Hello`] request. Optional so the field is omitted
    /// on unrelated responses and absent on the wire from pre-M2.21 daemons
    /// (in which case the client treats `None` as "incompatible").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<u32>,
    /// PRD #103 M1.1: daemon's compiled-in `env!("DAD_BUILD_ID")` — a
    /// finer-grained identifier than [`PROTOCOL_VERSION`] (it includes the
    /// commit hash and dirty marker) used by the laptop to detect
    /// same-tag-different-commit handler-code skew the protocol version
    /// can't catch. Optional so the field is omitted on unrelated responses
    /// and absent from pre-PRD-103 daemons (the client treats `None` as
    /// "incompatible — recycle the daemon").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_version: Option<String>,
    /// PRD #161 M1.1: snapshot of the agents the daemon is managing at
    /// handshake time (count + display names). Additive + optional — a
    /// pre-PRD-161 daemon omits it (deserializes to `None`), so the field is
    /// forward-compatible and needs no `PROTOCOL_VERSION` bump. The Part-A
    /// restart prompt and the Part-B `connect` nudge read it to say
    /// "N running agents: …" before recycling the daemon. Populated on the
    /// daemon side from the live registry in the `Hello` handler; `None` on
    /// unrelated responses and on the static `daemon hello` CLI probe (which
    /// has no registry to enumerate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_agents: Option<RunningAgentsSummary>,
    /// PRD #161 M1.1: the daemon binary's `env!("DAD_VERSION")` (e.g.
    /// `0.31.1`) — the semver tag *without* the `-g<sha>[-dirty]` build suffix
    /// that `build_version` carries. Additive + optional so a future option B
    /// version-compatibility classification (deferred — see PRD #161 D2)
    /// becomes a non-breaking add: the field is already on the wire, an older
    /// daemon omits it (`None`), and `PROTOCOL_VERSION` is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
}

impl AttachResponse {
    pub fn ok() -> Self {
        Self {
            ok: true,
            ..Default::default()
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            ..Default::default()
        }
    }
    pub fn agents(ids: Vec<String>) -> Self {
        Self {
            ok: true,
            agents: Some(ids),
            ..Default::default()
        }
    }
    /// Build a list-agents response that populates *both* the legacy
    /// `agents` field (just ids) and the new `agent_records` field (ids
    /// plus captured pane env). The dual shape is what keeps older
    /// clients reading just `agents` working alongside newer clients
    /// preferring `agent_records`.
    pub fn agent_records(records: Vec<AgentRecord>) -> Self {
        let ids = records.iter().map(|r| r.id.clone()).collect();
        Self {
            ok: true,
            agents: Some(ids),
            agent_records: Some(records),
            ..Default::default()
        }
    }
    pub fn with_id(id: String) -> Self {
        Self {
            ok: true,
            id: Some(id),
            ..Default::default()
        }
    }
    /// PRD #76 M2.21: protocol-version handshake reply. `version` is the
    /// daemon's [`PROTOCOL_VERSION`]; the client compares it against its own.
    ///
    /// PRD #103 M1.1: also carries the daemon's compiled-in `DAD_BUILD_ID`
    /// so the laptop can detect handler-code skew (same protocol version,
    /// different commit / dirty tree) the protocol version alone can't
    /// catch.
    pub fn hello(version: u32) -> Self {
        Self {
            ok: true,
            server_version: Some(version),
            // `local_build_id()` returns the compile-time
            // `env!("DAD_BUILD_ID")` in production; integration tests
            // (PRD #103 M4.2) inject a synthetic value via the
            // `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` env var so they can
            // simulate same-tag / different-commit skew without
            // rebuilding the binary.
            build_version: Some(crate::build_id::local_build_id()),
            // PRD #161 M1.1: also advertise the daemon's compiled-in
            // `DAD_VERSION` (the semver tag, e.g. `0.31.1`). Always known at
            // compile time like `build_version`, so it rides every hello
            // reply — including the static `daemon hello` CLI probe. Additive
            // and optional; a future option B classifies on this field.
            daemon_version: Some(env!("DAD_VERSION").to_string()),
            ..Default::default()
        }
    }

    /// PRD #161 M1.1: attach a running-agent summary to a handshake reply.
    /// The daemon's `Hello` handler calls this with a snapshot of the live
    /// registry so the client's restart prompt / connect nudge can name the
    /// agents that recycling the daemon would stop. Static probes that have
    /// no registry (the `daemon hello` CLI) leave `running_agents` as `None`.
    pub fn with_running_agents(mut self, summary: RunningAgentsSummary) -> Self {
        self.running_agents = Some(summary);
        self
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Bind the attach socket and return the listener, ready for `serve_attach`.
/// Cleans up any stale socket file before binding. Split from `run_attach_server`
/// so callers (notably tests) can synchronously confirm the listener is ready
/// to accept connections before spawning the async serve loop — this removes
/// the bind/accept readiness race that the old probe-and-retry pattern was
/// papering over.
pub fn bind_attach_listener(path: &Path) -> io::Result<UnixListener> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = crate::daemon::bind_socket(path)?;
    // Defense in depth — the umask-before-bind in `bind_socket` already
    // creates the inode at 0o600; restating the mode here means any future
    // code path that bypasses `bind_socket` still ends up with the right
    // permissions.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Accept-loop half of the attach server. Runs until the listener errors out
/// or the future is dropped. Pairs with `bind_attach_listener`.
///
/// `event_tx` is the daemon-wide `BroadcastMsg` broadcast (PRD #76
/// M2.17 for hook events; extended in M2.19 to also carry delegate
/// signals). It is held here so each accepted connection can call
/// `subscribe()` if the client opens a `SubscribeEvents` stream. The
/// cost of holding a `Sender` with zero subscribers is negligible —
/// `send` only succeeds when at least one `Receiver` exists.
pub async fn serve_attach(
    listener: UnixListener,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
) -> io::Result<()> {
    // Discard counter and use an empty state so callers that don't care
    // about idle shutdown or daemon-side orchestration don't need to
    // construct either. The daemon's main path uses
    // [`serve_attach_with_counter`] with its real state.
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::RwLock;
    let dummy_count = Arc::new(AtomicUsize::new(0));
    let dummy_state: SharedState = Arc::new(RwLock::new(crate::state::AppState::default()));
    // No-counter callers (tests, the local daemon_client fallback) don't drive
    // the scheduler; hand them an empty stand-in so `ReloadSchedules`/`RunNow`
    // resolve against an empty registry rather than needing a real one.
    let dummy_scheduler = Arc::new(crate::scheduler::Scheduler::with_stderr_notifier());
    let dummy_reuse = crate::spawn::new_reuse_registry();
    serve_attach_with_counter(
        listener,
        registry,
        event_tx,
        dummy_count,
        dummy_state,
        None,
        dummy_scheduler,
        dummy_reuse,
    )
    .await
}

/// PRD #93 M1.2 variant of [`serve_attach`] that maintains `client_count`
/// across the lifetime of each accepted connection. The daemon's idle
/// monitor reads this count alongside the PTY registry size to decide when
/// the daemon may exit (both must be zero for the configured idle window).
///
/// The counter is incremented immediately after `accept` returns and
/// decremented in the per-connection task's exit branch (panic or not — the
/// `tokio::spawn` future is wrapped so the decrement always runs).
#[allow(clippy::too_many_arguments)]
pub async fn serve_attach_with_counter(
    listener: UnixListener,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
    client_count: Arc<std::sync::atomic::AtomicUsize>,
    state: SharedState,
    shutdown: Option<Arc<tokio::sync::Notify>>,
    scheduler: Arc<crate::scheduler::Scheduler>,
    reuse_registry: crate::spawn::ReuseRegistry,
) -> io::Result<()> {
    use std::sync::atomic::Ordering;
    use tokio::sync::Notify;
    // PRD #93 round-2 reviewer REV-1: the same Notify the registry uses for
    // spawn/close/exit transitions also fires on every attach-counter
    // transition. The daemon's edge-triggered idle monitor waits on it, so
    // a brief detach+reconnect wakes the monitor before any timer can fire.
    // Cloned once per accepted connection — `notify_one` is cheap and tokio
    // Notify stores a permit if no waiter is registered, so a signal sent
    // between the monitor's loop iterations isn't lost.
    let change_notify: Arc<Notify> = registry.change_notify();
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let registry = registry.clone();
                let event_tx = event_tx.clone();
                let counter = client_count.clone();
                let state = state.clone();
                let notify = change_notify.clone();
                let shutdown = shutdown.clone();
                let scheduler = scheduler.clone();
                let reuse_registry = reuse_registry.clone();
                tokio::spawn(async move {
                    // RAII guard: increments on creation, decrements on drop,
                    // so a `handle_connection` task that panics or is dropped
                    // still releases its slot in the client count. Without
                    // the guard, an unwinding task would leak a slot and
                    // keep the daemon alive past the idle threshold.
                    //
                    // The guard also signals `change_notify` on drop so the
                    // edge-triggered idle monitor wakes immediately on
                    // disconnect (PRD #93 round-2 reviewer REV-1).
                    struct ClientGuard {
                        counter: Arc<std::sync::atomic::AtomicUsize>,
                        notify: Arc<Notify>,
                    }
                    impl Drop for ClientGuard {
                        fn drop(&mut self) {
                            self.counter.fetch_sub(1, Ordering::SeqCst);
                            self.notify.notify_one();
                        }
                    }
                    counter.fetch_add(1, Ordering::SeqCst);
                    // Signal the increment too so the monitor cancels any
                    // pending shutdown timer the moment a fresh client
                    // connects, not after the next decrement.
                    notify.notify_one();
                    let _guard = ClientGuard {
                        counter: counter.clone(),
                        notify: notify.clone(),
                    };
                    if let Err(e) = handle_connection(
                        stream,
                        registry,
                        event_tx,
                        state,
                        shutdown,
                        scheduler,
                        reuse_registry,
                    )
                    .await
                    {
                        warn!("attach protocol connection error: {e}");
                    }
                });
            }
            Err(e) => {
                error!("attach accept failed: {e}");
                return Err(e);
            }
        }
    }
}

/// Bind the attach socket and serve protocol connections forever. Cleans up
/// any stale socket file before binding. Runs until the listener errors out
/// or the future is dropped.
pub async fn run_attach_server(
    path: &Path,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
) -> io::Result<()> {
    let listener = bind_attach_listener(path)?;
    info!("Attach protocol listening on {}", path.display());
    serve_attach(listener, registry, event_tx).await
}

/// PRD #93 M1.2 counter-aware sibling of [`run_attach_server`]. The daemon
/// loop uses this so the idle monitor sees attached-client transitions in
/// real time.
pub async fn run_attach_server_with_counter(
    path: &Path,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
    client_count: Arc<std::sync::atomic::AtomicUsize>,
    state: SharedState,
) -> io::Result<()> {
    let listener = bind_attach_listener(path)?;
    info!("Attach protocol listening on {}", path.display());
    let dummy_scheduler = Arc::new(crate::scheduler::Scheduler::with_stderr_notifier());
    let dummy_reuse = crate::spawn::new_reuse_registry();
    serve_attach_with_counter(
        listener,
        registry,
        event_tx,
        client_count,
        state,
        None,
        dummy_scheduler,
        dummy_reuse,
    )
    .await
}

async fn handle_connection(
    mut stream: UnixStream,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
    state: SharedState,
    shutdown: Option<Arc<tokio::sync::Notify>>,
    scheduler: Arc<crate::scheduler::Scheduler>,
    reuse_registry: crate::spawn::ReuseRegistry,
) -> io::Result<()> {
    let frame = match read_frame(&mut stream).await? {
        Some(f) => f,
        None => return Ok(()),
    };
    // PRD #92 F1: client → server `KIND_SHUTDOWN` is a header-only frame
    // that means "shut the daemon down now." It comes before any
    // `KIND_REQ`, so handle it before the usual request-decoding path.
    //
    // PRD #92 F1 followup hardening:
    //   (a) Reject any non-empty payload — `KIND_SHUTDOWN` is contractually
    //       header-only, and an attacker (or an upgrade-mismatch peer)
    //       smuggling bytes alongside the kind byte must not be able to
    //       trigger daemon teardown by mistake. Drop the frame silently
    //       (close the connection, do not initiate shutdown).
    //   (b) Send a `KIND_SHUTDOWN_ACK` **before** initiating teardown so
    //       the client can distinguish "daemon acknowledged" from
    //       "old daemon closed the connection on an unknown frame."
    //       Teardown can take ≥3 seconds (SIGTERM grace + SIGKILL), so
    //       the ack must be on the wire first.
    if frame.0 == KIND_SHUTDOWN {
        if !frame.1.is_empty() {
            warn!(
                payload_len = frame.1.len(),
                "KIND_SHUTDOWN rejected — frame is contractually header-only"
            );
            return Ok(());
        }
        info!("KIND_SHUTDOWN received — sending ack and beginning graceful daemon shutdown");
        // Ack first: the client's `send_shutdown` waits up to 1s for this
        // frame and treats absence as a hard error. Writing the ack
        // before kicking off the registry drain keeps the wire ordering
        // honest even if the teardown itself takes the full 3-second
        // SIGTERM grace.
        if let Err(e) = write_frame(&mut stream, KIND_SHUTDOWN_ACK, &[]).await {
            warn!(error = %e, "failed to write KIND_SHUTDOWN_ACK before shutdown — proceeding anyway");
        }
        // Drop the registry's children with a 3-second grace window for
        // SIGTERM to take effect; survivors get SIGKILL via the existing
        // teardown.
        let registry_for_shutdown = registry.clone();
        tokio::task::spawn_blocking(move || {
            registry_for_shutdown.shutdown_all_graceful(Duration::from_secs(3));
        })
        .await
        .ok();
        if let Some(s) = shutdown {
            s.notify_one();
        } else {
            // `serve_attach` (test/harness path) doesn't pass a shutdown
            // notify because tests don't run the production hook loop.
            // The registry was still drained, so the test can assert on
            // that side effect.
            warn!(
                "KIND_SHUTDOWN handled but no daemon-shutdown notify wired (likely a test harness)"
            );
        }
        return Ok(());
    }
    if frame.0 != KIND_REQ {
        let resp = AttachResponse::err(format!("expected REQ frame, got kind 0x{:02x}", frame.0));
        write_resp(&mut stream, &resp).await?;
        return Ok(());
    }
    let req: AttachRequest = match serde_json::from_slice(&frame.1) {
        Ok(r) => r,
        Err(e) => {
            let resp = AttachResponse::err(format!("malformed request: {e}"));
            write_resp(&mut stream, &resp).await?;
            return Ok(());
        }
    };

    match req {
        AttachRequest::ListAgents => {
            let records = registry.agent_records();
            write_resp(&mut stream, &AttachResponse::agent_records(records)).await?;
        }
        AttachRequest::StartAgent {
            command,
            cwd,
            rows,
            cols,
            env,
            display_name,
            tab_membership,
            agent_type,
        } => {
            // PRD #92 F1 followup hardening: refuse to start a new agent
            // while the registry's `shutting_down` latch is set. The
            // latch is flipped at the start of `shutdown_all_graceful`
            // so a race between an in-flight `StartAgent` and a
            // `KIND_SHUTDOWN` cannot spawn a new child the teardown is
            // about to miss. Reply with a clean error rather than
            // letting the spawn race the drain.
            if registry.is_shutting_down() {
                write_resp(
                    &mut stream,
                    &AttachResponse::err("start-agent: daemon is shutting down"),
                )
                .await?;
                return Ok(());
            }
            // Trust boundary: same OS user, same exec capability — see the
            // `AttachRequest::StartAgent` docs. We forward `command`/`cwd`/
            // `env` to the spawn path verbatim. The only check here is a
            // sanity guard against an empty/whitespace-only `command`,
            // which is almost certainly a client bug rather than an
            // attack: it would otherwise resolve to a binary named "" or
            // " " and fail with a confusing OS error. This is *not* an
            // allowlist.
            if let Some(c) = command.as_deref()
                && c.trim().is_empty()
            {
                write_resp(
                    &mut stream,
                    &AttachResponse::err("start-agent: command is empty or whitespace-only"),
                )
                .await?;
                return Ok(());
            }

            // PRD #93 round-5: capture the bits we need to populate the
            // daemon's `AppState` role map BEFORE the spawn (we'll need
            // the pane id from env and the orchestration metadata from
            // tab_membership). The spawn moves `opts`, so we clone what
            // we need first.
            let pane_id_env: Option<String> = env
                .iter()
                .find(|(k, _)| k == DOT_AGENT_DECK_PANE_ID)
                .map(|(_, v)| v.clone())
                .and_then(|v| {
                    if is_valid_pane_id_env(&v) {
                        Some(v)
                    } else {
                        None
                    }
                });
            // Round-11 auditor #C: also pull `orchestration_cwd` out of
            // the membership so the daemon can use it (not StartAgent.cwd)
            // as the disambiguator in `pane_orchestration_map`. This keeps
            // round-9 #2's "workers can have different per-pane cwds"
            // contract intact — pane_cwd_map gets StartAgent.cwd
            // per-pane, but pane_orchestration_map keys on the shared
            // orchestration cwd from the TabMembership.
            let orchestration_meta: Option<(String, String, bool, Option<String>)> =
                tab_membership.as_ref().and_then(|tm| match tm {
                    TabMembership::Orchestration {
                        name,
                        role_name,
                        is_start_role,
                        orchestration_cwd,
                        ..
                    } if !role_name.is_empty() => Some((
                        name.clone(),
                        role_name.clone(),
                        *is_start_role,
                        orchestration_cwd.clone(),
                    )),
                    _ => None,
                });
            let cwd_for_state = cwd.clone();

            let opts = SpawnOptions {
                command: command.as_deref(),
                cwd: cwd.as_deref(),
                display_name: display_name.as_deref(),
                rows,
                cols,
                env,
                tab_membership,
                agent_type,
            };
            match registry.spawn_agent(opts) {
                Ok(id) => {
                    // PRD #93 round-5: populate daemon-side role maps so
                    // `handle_delegate` / `handle_work_done` can resolve
                    // the worker pane and orchestrator pane purely from
                    // daemon state — no TUI round-trip, no broadcast hop.
                    // We do this only for orchestration panes; dashboard
                    // and mode panes don't participate in delegate
                    // dispatch.
                    if let (
                        Some(pane_id),
                        Some((orch_name, role_name, is_start_role, orchestration_cwd)),
                    ) = (pane_id_env.as_deref(), orchestration_meta)
                    {
                        // Round-11 auditor #C: scope the orchestration
                        // identity by `(name, orchestration_cwd)` so
                        // two unnamed orchestrations in different cwds
                        // (`~/a/foo` and `~/b/foo`, both resolving
                        // `name` to "foo") don't collide. The
                        // `orchestration_cwd` is shared across every
                        // role pane in one orchestration tab (round-9
                        // #2: per-pane cwd may diverge, but the
                        // orchestration's identity does not). Older
                        // clients that don't carry the field fall back
                        // to StartAgent.cwd — preserves backwards
                        // compat at the cost of re-opening the
                        // collision; `Some` vs `None` is detectable so
                        // this is documented behavior, not a silent
                        // misroute.
                        let orch_cwd = orchestration_cwd
                            .or_else(|| cwd_for_state.clone())
                            .unwrap_or_default();
                        let mut st = state.write().await;
                        st.register_pane(pane_id.to_string());
                        st.pane_role_map
                            .insert(pane_id.to_string(), role_name.clone());
                        st.pane_orchestration_map
                            .insert(pane_id.to_string(), (orch_name, orch_cwd));
                        if let Some(c) = cwd_for_state.clone() {
                            st.pane_cwd_map.insert(pane_id.to_string(), c);
                        }
                        if is_start_role {
                            st.orchestrator_pane_ids.insert(pane_id.to_string());
                        }
                    }
                    write_resp(&mut stream, &AttachResponse::with_id(id)).await?
                }
                Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
            }
        }
        AttachRequest::StopAgent { id } => {
            // PRD #93 round-5: capture the agent's `pane_id_env` BEFORE
            // close_agent removes the registry entry, so we can clean up
            // the daemon's per-pane role-map entries after a successful
            // close. Without this, a closed pane's role/cwd would linger
            // in the maps and a subsequent `handle_delegate` aimed at
            // that role would still resolve the dead pane.
            let pane_id_env = registry
                .agent_records()
                .into_iter()
                .find(|r| r.id == id)
                .and_then(|r| r.pane_id_env);
            // PRD #92 F8 followup (auditor #1): `close_agent` runs the
            // synchronous SIGTERM-with-grace loop in
            // `terminate_child_with_grace_and_wait`, which calls
            // `std::thread::sleep` for up to 3 s while polling the
            // child's `try_wait`. Calling that from inside the async
            // attach-connection task would block a Tokio worker thread
            // for the duration of the grace window — under load this
            // can starve other connections. Mirror the
            // `KIND_SHUTDOWN` handler's pattern: hop the blocking work
            // onto a `spawn_blocking` pool task, await the
            // `JoinHandle`, and surface a join error as a failed
            // close.
            let registry_for_close = registry.clone();
            let id_for_close = id.clone();
            let close_result =
                tokio::task::spawn_blocking(move || registry_for_close.close_agent(&id_for_close))
                    .await;
            let close_outcome: Result<(), String> = match close_result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(join_err) => {
                    tracing::warn!(
                        error = %join_err,
                        agent_id = %id,
                        "spawn_blocking for close_agent panicked or was cancelled"
                    );
                    Err(format!("close_agent task failed: {join_err}"))
                }
            };
            match close_outcome {
                Ok(()) => {
                    if let Some(pane_id) = pane_id_env {
                        state.write().await.unregister_pane(&pane_id);
                    }
                    write_resp(&mut stream, &AttachResponse::ok()).await?
                }
                Err(msg) => write_resp(&mut stream, &AttachResponse::err(msg)).await?,
            }
        }
        AttachRequest::SetAgentLabel {
            id,
            display_name,
            cwd,
        } => match registry.set_agent_label(&id, display_name, cwd) {
            Ok(()) => write_resp(&mut stream, &AttachResponse::ok()).await?,
            Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
        },
        AttachRequest::Snapshot { id } => match registry.snapshot(&id) {
            Ok(bytes) => {
                write_resp(&mut stream, &AttachResponse::ok()).await?;
                // Mirror the attach-stream / subscribe-events policy: bound the
                // body and STREAM_END writes with `CLIENT_WRITE_TIMEOUT`. A
                // client that opened a `Snapshot` connection and stopped
                // reading after the OK response could otherwise park this task
                // forever on `write_all` (kernel send buffer fills, the write
                // never completes). On timeout, best-effort STREAM_END with a
                // typed reason and return Ok(()) — a stuck client doesn't
                // justify failing the dispatcher task.
                if !bytes.is_empty()
                    && !write_or_timeout(&mut stream, KIND_STREAM_OUT, &bytes).await
                {
                    let _ = write_or_timeout(&mut stream, KIND_STREAM_END, b"timeout").await;
                    return Ok(());
                }
                if !write_or_timeout(&mut stream, KIND_STREAM_END, &[]).await {
                    return Ok(());
                }
            }
            Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
        },
        AttachRequest::AttachStream { id } => {
            handle_attach_stream(stream, registry, id).await?;
        }
        AttachRequest::Resize { id, rows, cols } => match registry.resize(&id, rows, cols) {
            Ok(()) => write_resp(&mut stream, &AttachResponse::ok()).await?,
            Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
        },
        AttachRequest::WriteAndSubmit { pane_id, text } => {
            match registry.write_to_pane_and_submit(&pane_id, &text).await {
                Ok(()) => write_resp(&mut stream, &AttachResponse::ok()).await?,
                Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
            }
        }
        AttachRequest::SubscribeEvents => {
            handle_subscribe_events(stream, event_tx).await?;
        }
        AttachRequest::Hello {
            client_version: _,
            client_build_version,
        } => {
            // PRD #76 M2.21: the daemon never enforces or rejects on
            // `client_version` — we always reply with our own
            // `PROTOCOL_VERSION` and let the caller decide. Centralizing the
            // policy on the client side means a newer client talking to an
            // older daemon (the upgrade-skew direction the daemon can't
            // detect anyway) still gets a sensible mismatch error instead of
            // the daemon rejecting what *would* be its own future shape.
            //
            // PRD #103 M1.2: log the client's build_version when present
            // for post-hoc debugging of mismatch reports. Same server
            // policy — never reject; the laptop decides.
            //
            // `client_build_version` is advisory, not trust-bearing: a
            // hostile or buggy client could embed newlines / ANSI escapes
            // that would corrupt log files or terminal display when an
            // operator tails the log. Pass through `escape_debug` to
            // render any control bytes as printable escapes before
            // formatting. The daemon-side `local_build_id()` is from our
            // own compile-time env and doesn't need the same treatment,
            // but escaping both keeps the log line consistently quoted.
            if let Some(cbv) = client_build_version.as_deref() {
                let daemon_build = crate::build_id::local_build_id();
                let cbv_safe = cbv.escape_debug().to_string();
                let daemon_build_safe = daemon_build.escape_debug().to_string();
                info!(
                    target: "daemon_protocol",
                    "Hello from client build_version=\"{cbv_safe}\" (daemon build_version=\"{daemon_build_safe}\")",
                );
            }
            // PRD #161 M1.1: enumerate the live registry so the reply carries
            // the running-agent summary (count + display names). The Part-A
            // restart prompt and Part-B connect nudge read it to say
            // "N running agents: …" before recycling the daemon. Additive and
            // optional — an older daemon omits it and the client tolerates
            // its absence.
            //
            // PRD #161 FIX 1 test knob: `DOT_AGENT_DECK_TEST_OMIT_RUNNING_AGENTS`
            // makes the reply OMIT `running_agents` (leave it `None`),
            // simulating a pre-#161 daemon so the cross-version None-agents
            // fallback (handshake FIX 1) can be exercised at L2. Gated behind
            // the same `cfg(any(test, debug_assertions))` as
            // `DOT_AGENT_DECK_BUILD_ID_OVERRIDE`, so a shipped release binary
            // compiles the hook out and can never be tricked into hiding its
            // live agents.
            #[cfg(any(test, debug_assertions))]
            let omit_running_agents =
                std::env::var_os("DOT_AGENT_DECK_TEST_OMIT_RUNNING_AGENTS").is_some();
            #[cfg(not(any(test, debug_assertions)))]
            let omit_running_agents = false;
            let mut resp = AttachResponse::hello(PROTOCOL_VERSION);
            if !omit_running_agents {
                let summary = RunningAgentsSummary::from_records(&registry.agent_records());
                resp = resp.with_running_agents(summary);
            }
            write_resp(&mut stream, &resp).await?;
        }
        AttachRequest::ReloadSchedules => {
            // PRD #127 M1.3: re-read the global config and diff/replace the
            // registered task set. A bad entry is surfaced via the notifier
            // and skipped; it never fails the reload. Then wake the idle
            // monitor (via the registry's change_notify) so a reload that
            // dropped the last enabled schedule lets the idle gate fire, and
            // one that added a schedule re-arms the carve-out.
            let loaded = crate::config::LoadedSchedules::load();
            scheduler.report_config_errors(&loaded.errors);
            scheduler.reload_apply(
                &loaded.tasks,
                crate::daemon::schedule_callback_factory(
                    registry.clone(),
                    reuse_registry.clone(),
                    event_tx.clone(),
                ),
            );
            registry.change_notify().notify_one();
            let names = scheduler.registered_names();
            let mut resp = AttachResponse::ok();
            resp.agents = Some(names);
            write_resp(&mut stream, &resp).await?;
        }
        AttachRequest::RunNow { name } => {
            // PRD #127 M1.5: fire the task now (the `schedule run-now` door).
            // Both started and skipped-still-running mean the task IS
            // registered → ok=true (so `wait_for_schedule_registered` and the
            // CLI treat it as success). PRD #127 C5: surface the started-vs-
            // skipped outcome in `agents` so the caller can report it
            // distinctly; an unknown task → ok=false.
            match scheduler.run_now(&name) {
                Ok(started) => {
                    let token = if started { "started" } else { "skipped" };
                    let mut resp = AttachResponse::ok();
                    resp.agents = Some(vec![token.to_string()]);
                    write_resp(&mut stream, &resp).await?
                }
                Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
            }
        }
    }
    Ok(())
}

// CodeRabbit Fix C fixup: bound the response write with `CLIENT_WRITE_TIMEOUT`.
// Every dispatch arm calls `write_resp` first; without a timeout, a same-UID
// client that connected and then stopped reading could pin the dispatcher task
// on this initial OK/Err write (kernel send buffer fills, `write_all` never
// completes). On timeout, surface `io::ErrorKind::TimedOut` so existing `?`
// callers propagate up and let the connection drop.
#[doc(hidden)]
pub async fn write_resp<W: AsyncWrite + Unpin>(w: &mut W, resp: &AttachResponse) -> io::Result<()> {
    let payload = serde_json::to_vec(resp).expect("AttachResponse must serialize");
    match tokio::time::timeout(CLIENT_WRITE_TIMEOUT, write_frame(w, KIND_RESP, &payload)).await {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "write_resp: client did not drain RESP within CLIENT_WRITE_TIMEOUT",
        )),
    }
}

/// Long-lived `SubscribeEvents` handler (PRD #76 M2.17). Confirms the
/// subscription with an OK `RESP`, then forwards every hook
/// [`BroadcastMsg::Event`] from the daemon-wide broadcast as a
/// `KIND_EVENT` frame. Each write is bounded by `CLIENT_WRITE_TIMEOUT`
/// so a wedged client can't pin this task forever. A lagged receiver
/// (the client fell further behind than the broadcast capacity) closes
/// the connection with `KIND_STREAM_END` carrying `"lagged"`; the
/// TUI's reconnect path drains a `list_agents` snapshot to recover.
/// Client disconnect is detected by racing a one-byte read against
/// `rx.recv()` so the broadcast `Receiver` is dropped promptly when
/// the client goes away between messages — otherwise the
/// per-connection task and its receiver would leak for the lifetime
/// of the daemon.
///
/// PRD #93 round-5: orchestration signals (delegate / work-done) used
/// to ride this channel via `BroadcastMsg::Delegate` / `WorkDone`,
/// guarded by a replay buffer (`PendingBroadcasts`), a salvage loop on
/// detach, and a test gate to drive the salvage race. All of that is
/// gone — orchestration prompts now flow directly into target PTYs
/// (see [`AppState::handle_delegate`] /
/// [`AppState::handle_work_done`]) and the surviving PTY scrollback
/// makes a separate replay path unnecessary.
async fn handle_subscribe_events(
    stream: UnixStream,
    event_tx: broadcast::Sender<BroadcastMsg>,
) -> io::Result<()> {
    let mut rx = event_tx.subscribe();
    let (mut rd, mut wr) = stream.into_split();
    write_resp(&mut wr, &AttachResponse::ok()).await?;

    loop {
        tokio::select! {
            recv = rx.recv() => {
                match recv {
                    Ok(msg) => {
                        let payload = match serde_json::to_vec(&msg) {
                            Ok(b) => b,
                            Err(e) => {
                                // A BroadcastMsg that can't serialize is a daemon
                                // bug — log and skip rather than tear the
                                // subscription down for every other client.
                                warn!("subscribe-events: skipping unserializable broadcast: {e}");
                                continue;
                            }
                        };
                        if !write_or_timeout(&mut wr, KIND_EVENT, &payload).await {
                            let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"timeout").await;
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Daemon's event_tx dropped — daemon is shutting down.
                        let _ = write_or_timeout(&mut wr, KIND_STREAM_END, &[]).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Client fell behind beyond EVENT_BROADCAST_CAPACITY.
                        // Tear the subscription down with a typed reason so the
                        // client can drop and reconnect.
                        let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"lagged").await;
                        break;
                    }
                }
            }
            // Disconnect detector: the client never writes after the
            // SubscribeEvents request, so any read result here means the
            // socket is gone (EOF / error) or the client is misbehaving.
            // Either way, exit so the receiver drops.
            _ = rd.read_u8() => {
                break;
            }
        }
    }
    Ok(())
}

async fn handle_attach_stream(
    stream: UnixStream,
    registry: Arc<AgentPtyRegistry>,
    id: String,
) -> io::Result<()> {
    let handle = match registry.subscribe(&id) {
        Ok(h) => h,
        Err(e) => {
            let mut s = stream;
            write_resp(&mut s, &AttachResponse::err(e.to_string())).await?;
            return Ok(());
        }
    };

    let (mut rd, mut wr) = stream.into_split();

    // 1. Confirm the attach succeeded.
    write_resp(&mut wr, &AttachResponse::ok()).await?;
    // 2. Replay the consistent scrollback snapshot before live bytes start
    //    flowing. `subscribe()` guarantees no overlap or gap with the bytes
    //    delivered via `rx` below. The write is bounded by
    //    `CLIENT_WRITE_TIMEOUT` for the same reason live STREAM_OUT writes
    //    are: a client wedged at attach time would otherwise pin this task
    //    forever (kernel send buffer fills, `write_all` never completes,
    //    and the output task never even starts so lag detection can't
    //    fire). On timeout, mirror the output-task policy — best-effort
    //    bounded STREAM_END, then drop the writer and bail.
    if !handle.snapshot.is_empty()
        && !write_or_timeout(&mut wr, KIND_STREAM_OUT, &handle.snapshot).await
    {
        let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"timeout").await;
        return Ok(());
    }

    let mut rx = handle.rx;
    let writer = handle.writer;

    // PRD #128 trace-field-symmetry: cache the agent's `pane_id_env`
    // once per attach so the per-frame STREAM_IN trace can emit
    // `pane_id` alongside `agent_id` without re-locking the registry on
    // every frame. `pane_id_env` is fixed for an agent's lifetime, so
    // one lookup is enough. Three states are distinguished — the M1.4
    // cross-path diff needs to tell them apart, not just see an empty
    // string. Angle-bracket sentinels (`<agent-gone>` / `<no-pane>`)
    // can never collide with a real `pane_id_env` value because
    // `is_valid_pane_id_env` rejects `<` and `>`.
    let pane_id: String = match registry.pane_id_env_for_agent(&id) {
        Some(Some(s)) => s,
        Some(None) => "<no-pane>".to_string(),
        None => "<agent-gone>".to_string(),
    };

    // Output task: forward broadcast bytes → STREAM_OUT frames. Owns `wr`
    // for the duration of streaming.
    //
    // Every write goes through `CLIENT_WRITE_TIMEOUT`. Without it, a client
    // that stops draining its socket pins this task on `write_all` (the
    // kernel send buffer fills and the write never completes) — which also
    // suppresses lag detection, since we can't reach the next `rx.recv()`
    // to observe `RecvError::Lagged`. With the timeout, a wedged client is
    // detected within bounded time and the connection is dropped.
    let output_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(bytes) => {
                    if !write_or_timeout(&mut wr, KIND_STREAM_OUT, &bytes).await {
                        // Client wedged or socket error: try one bounded
                        // STREAM_END, then give up. If even STREAM_END
                        // can't get through, dropping `wr` here closes the
                        // socket — the client observes EOF either way.
                        let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"timeout").await;
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // Agent terminated (reader thread saw EOF).
                    let _ = write_or_timeout(&mut wr, KIND_STREAM_END, &[]).await;
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // This subscriber fell behind beyond BROADCAST_CAPACITY.
                    // Better to disconnect than to deliver corrupted ANSI;
                    // the client can reattach and replay scrollback. The
                    // bounded write timeout matters here too: if the client
                    // also wedged its socket, we still need to drop within
                    // a known time rather than block on STREAM_END.
                    let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"lagged").await;
                    break;
                }
            }
        }
    });

    // Input loop: STREAM_IN bytes are forwarded to the shared PTY writer;
    // DETACH (or unknown frame / EOF) ends the loop.
    loop {
        match read_frame(&mut rd).await {
            Ok(Some((KIND_STREAM_IN, bytes))) => {
                use std::io::Write;
                let mut w = writer.lock().await;
                // PRD #128 (cherry-picked from PR #122): byte-level trace
                // of STREAM_IN frames forwarded to the per-agent PTY
                // writer. Useful for confirming that bytes the TUI
                // queued arrived as distinct frames and that no other
                // path interleaved a write on the same writer mutex
                // between them. Gated by `RUST_LOG=trace`. Emitted
                // INSIDE the writer mutex so trace order matches actual
                // write order. Both `agent_id` and `pane_id` are
                // emitted so the M1.4 diff against the daemon-initiated
                // trace in `AgentPtyRegistry::write_to_pane_internal`
                // can join on either key (`pane_id` is cached once per
                // attach above).
                tracing::trace!(
                    target: "pane_write",
                    source = "stream_in",
                    agent_id = %id,
                    pane_id = %pane_id,
                    payload_len = bytes.len(),
                    payload = %escape_bytes_for_log(&bytes),
                    "STREAM_IN forwarded to PTY writer"
                );
                if w.write_all(&bytes).is_err() {
                    break;
                }
                let _ = w.flush();
                drop(w);
                // PRD #127 M2.2: a STREAM_IN frame is a *user* keystroke —
                // stamp the pane's deliver-on-idle debounce clock so a
                // concurrent scheduled reuse fire queues its prompt instead of
                // interrupting active typing. Keyed by `pane_id_env` (the same
                // key the reuse path delivers to).
                registry.note_user_input(&pane_id);
            }
            Ok(Some((KIND_DETACH, _))) => {
                // Explicit M2.5 detach: client signalled intent to leave the
                // agent running. Plain socket EOF takes the `Ok(None)` arm
                // below and is intentionally *not* counted as a detach —
                // only voluntary detaches bump the registry counter.
                registry.record_detach();
                break;
            }
            Ok(Some((kind, _))) => {
                warn!("unexpected frame kind 0x{kind:02x} on attach stream — closing");
                break;
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    // Stop the output task; aborting is fine because either we already saw
    // STREAM_END and the loop exited on its own, or we're detaching and the
    // client doesn't expect more bytes.
    output_task.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    #[tokio::test]
    async fn frame_round_trip() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, KIND_STREAM_OUT, b"hello")
            .await
            .unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let (kind, payload) = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(kind, KIND_STREAM_OUT);
        assert_eq!(payload, b"hello");
    }

    #[tokio::test]
    async fn frame_eof_returns_none() {
        let buf: Vec<u8> = Vec::new();
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cursor).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn frame_partial_header_returns_err() {
        // 1, 2, 3, 4 bytes followed by EOF must each be reported as a
        // truncated frame (Err), not a clean disconnect (Ok(None)). Only
        // 0-bytes-then-EOF is a clean disconnect.
        for n in 1usize..=4 {
            let buf: Vec<u8> = vec![0u8; n];
            let mut cursor = std::io::Cursor::new(buf);
            let err = read_frame(&mut cursor)
                .await
                .expect_err(&format!("expected Err for {n}-byte partial header"));
            assert_eq!(
                err.kind(),
                io::ErrorKind::UnexpectedEof,
                "wrong error kind for {n}-byte partial header"
            );
        }
    }

    #[tokio::test]
    async fn frame_partial_body_returns_err() {
        // Header claims 16 bytes of payload; only 5 supplied before EOF.
        // The body read must fail as truncated.
        let mut buf: Vec<u8> = Vec::new();
        buf.push(KIND_STREAM_OUT);
        buf.extend_from_slice(&16u32.to_be_bytes());
        buf.extend_from_slice(b"hello"); // 5 bytes — short
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame(&mut cursor)
            .await
            .expect_err("expected Err for truncated body");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn frame_zero_length_payload() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, KIND_STREAM_END, &[]).await.unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let (kind, payload) = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(kind, KIND_STREAM_END);
        assert!(payload.is_empty());
    }

    #[tokio::test]
    async fn frame_rejects_oversize() {
        // Hand-crafted header claiming 32 MiB payload — must be rejected
        // before any allocation happens.
        let mut buf: Vec<u8> = vec![KIND_STREAM_OUT];
        buf.extend_from_slice(&((MAX_FRAME_LEN as u32 + 1).to_be_bytes()));
        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn request_serde_round_trip() {
        let req = AttachRequest::StartAgent {
            command: Some("/bin/sh".into()),
            cwd: None,
            rows: 24,
            cols: 80,
            env: vec![("FOO".into(), "BAR".into())],
            display_name: Some("auditor".into()),
            tab_membership: None,
            agent_type: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        match back {
            AttachRequest::StartAgent {
                command,
                env,
                display_name,
                tab_membership,
                ..
            } => {
                assert_eq!(command.as_deref(), Some("/bin/sh"));
                assert_eq!(env, vec![("FOO".to_string(), "BAR".to_string())]);
                assert_eq!(display_name.as_deref(), Some("auditor"));
                assert!(tab_membership.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn start_agent_omits_display_name_when_none() {
        // Forward compat: older daemons must accept a StartAgent payload
        // that doesn't carry `display_name`, and the field must not be
        // present in JSON when it's None.
        let req = AttachRequest::StartAgent {
            command: Some("/bin/sh".into()),
            cwd: None,
            rows: 24,
            cols: 80,
            env: vec![],
            display_name: None,
            tab_membership: None,
            agent_type: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("display_name"),
            "display_name=None should be omitted from the wire payload"
        );
        assert!(
            !v.as_object().unwrap().contains_key("tab_membership"),
            "tab_membership=None should be omitted from the wire payload"
        );
        assert!(
            !v.as_object().unwrap().contains_key("agent_type"),
            "agent_type=None should be omitted from the wire payload"
        );
    }

    #[test]
    fn start_agent_with_mode_tab_membership_round_trip() {
        // PRD #76 M2.12: tab_membership round-trips through the wire format
        // and survives `serde_json::from_str` on a foreign client.
        let req = AttachRequest::StartAgent {
            command: Some("claude".into()),
            cwd: Some("/work".into()),
            rows: 24,
            cols: 80,
            env: vec![],
            display_name: Some("k8s-ops".into()),
            tab_membership: Some(TabMembership::Mode {
                name: "k8s-ops".into(),
            }),
            agent_type: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Wire shape sanity: tagged enum with snake_case kind.
        assert_eq!(v["tab_membership"]["kind"], "mode");
        assert_eq!(v["tab_membership"]["name"], "k8s-ops");
        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        match back {
            AttachRequest::StartAgent { tab_membership, .. } => {
                assert_eq!(
                    tab_membership,
                    Some(TabMembership::Mode {
                        name: "k8s-ops".into()
                    })
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn start_agent_with_orchestration_tab_membership_round_trip() {
        let req = AttachRequest::StartAgent {
            command: Some("claude".into()),
            cwd: Some("/work".into()),
            rows: 24,
            cols: 80,
            env: vec![],
            display_name: Some("coder".into()),
            tab_membership: Some(TabMembership::Orchestration {
                name: "tdd-cycle".into(),
                role_index: 2,
                role_name: "coder".into(),
                is_start_role: false,
                orchestration_cwd: None,
                display_title: None,
            }),
            agent_type: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["tab_membership"]["kind"], "orchestration");
        assert_eq!(v["tab_membership"]["name"], "tdd-cycle");
        assert_eq!(v["tab_membership"]["role_index"], 2);
        assert_eq!(v["tab_membership"]["role_name"], "coder");
        assert_eq!(v["tab_membership"]["is_start_role"], false);
        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        match back {
            AttachRequest::StartAgent { tab_membership, .. } => {
                assert_eq!(
                    tab_membership,
                    Some(TabMembership::Orchestration {
                        name: "tdd-cycle".into(),
                        role_index: 2,
                        role_name: "coder".into(),
                        is_start_role: false,
                        orchestration_cwd: None,
                        display_title: None,
                    })
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn agent_record_with_tab_membership_round_trip() {
        // PRD #76 M2.12: the daemon's echo via `list_agents` must serialize
        // tab_membership so the TUI can rebuild tabs on reconnect. Older
        // clients ignore the unknown field; older daemons omit it (None).
        let rec = AgentRecord {
            id: "7".into(),
            pane_id_env: Some("pid-7".into()),
            display_name: Some("coder".into()),
            cwd: Some("/work".into()),
            tab_membership: Some(TabMembership::Orchestration {
                name: "tdd-cycle".into(),
                role_index: 1,
                role_name: "coder".into(),
                is_start_role: false,
                orchestration_cwd: None,
                display_title: None,
            }),
            agent_type: None,
            rows: 0,
            cols: 0,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: AgentRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tab_membership, rec.tab_membership);
    }

    #[test]
    fn agent_record_omits_tab_membership_when_none() {
        let rec = AgentRecord {
            id: "1".into(),
            pane_id_env: None,
            display_name: None,
            cwd: None,
            tab_membership: None,
            agent_type: None,
            rows: 0,
            cols: 0,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("tab_membership"),
            "tab_membership=None should be omitted from the wire payload"
        );
        let back: AgentRecord =
            serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert!(back.tab_membership.is_none());
    }

    #[test]
    fn agent_record_without_tab_membership_field_deserializes() {
        // Forward compat: an older daemon that doesn't know about
        // tab_membership omits the field. A newer TUI must deserialize the
        // payload with `tab_membership: None` and treat the agent as a
        // dashboard pane on hydration.
        let json = r#"{"id":"1","display_name":"foo","cwd":"/tmp"}"#;
        let rec: AgentRecord = serde_json::from_str(json).unwrap();
        assert!(rec.tab_membership.is_none());
    }

    #[test]
    fn start_agent_deserializes_old_client_shape_without_tab_membership() {
        // M2.12 fixup auditor #4: explicit compat test using a
        // hand-crafted JSON literal in the *old* client shape — no
        // `tab_membership` field at all. A newer daemon must accept
        // the payload and decode `tab_membership: None`. Asserting via
        // round-trip of the current struct doesn't catch this: it'd
        // serialize the (`skip_serializing_if = None`) field as
        // absent, but only because our struct produces that shape.
        // This test pins the actual wire surface an older client
        // would send.
        let json = r#"{
            "op": "start-agent",
            "command": "/bin/sh",
            "cwd": "/tmp",
            "rows": 24,
            "cols": 80,
            "env": [],
            "display_name": "auditor"
        }"#;
        let req: AttachRequest = serde_json::from_str(json).unwrap();
        match req {
            AttachRequest::StartAgent {
                command,
                cwd,
                display_name,
                tab_membership,
                rows,
                cols,
                ..
            } => {
                assert_eq!(command.as_deref(), Some("/bin/sh"));
                assert_eq!(cwd.as_deref(), Some("/tmp"));
                assert_eq!(display_name.as_deref(), Some("auditor"));
                assert_eq!(rows, 24);
                assert_eq!(cols, 80);
                assert!(
                    tab_membership.is_none(),
                    "old-client payload without tab_membership must decode as None"
                );
            }
            _ => panic!("expected StartAgent variant, got {req:?}"),
        }
    }

    #[test]
    fn agent_record_deserializes_old_daemon_shape_without_tab_membership() {
        // M2.12 fixup auditor #4 (sibling case): hand-crafted JSON
        // literal in the *old* daemon shape — `AgentRecord` without a
        // `tab_membership` field. A newer TUI must accept the payload
        // and decode `tab_membership: None`, treating the agent as a
        // dashboard pane on hydration.
        let json = r#"{
            "id": "42",
            "pane_id_env": "pid-42",
            "display_name": "auditor",
            "cwd": "/work"
        }"#;
        let rec: AgentRecord = serde_json::from_str(json).unwrap();
        assert_eq!(rec.id, "42");
        assert_eq!(rec.pane_id_env.as_deref(), Some("pid-42"));
        assert_eq!(rec.display_name.as_deref(), Some("auditor"));
        assert_eq!(rec.cwd.as_deref(), Some("/work"));
        assert!(
            rec.tab_membership.is_none(),
            "old-daemon record without tab_membership must decode as None"
        );
    }

    /// Scenario: Build a `SessionSnapshot` for every `SessionStatus` variant
    /// and round-trip it through JSON, asserting the status (and the agent
    /// type / active tool / tool count / prompts) survive; attach one to an
    /// `AgentRecord` and confirm `live` round-trips as `Some`; finally decode
    /// an older-daemon `AgentRecord` JSON that predates the `live` field and
    /// assert it deserializes with `live == None` (additive optional — no
    /// `PROTOCOL_VERSION` bump).
    #[spec("session/live/001")]
    #[test]
    fn live_001_session_snapshot_serde_and_agent_record_back_compat() {
        use crate::event::AgentType;
        use crate::state::{ActiveTool, SessionSnapshot, SessionStatus};

        // (a) Every SessionStatus variant survives a SessionSnapshot round-trip.
        for status in [
            SessionStatus::Idle,
            SessionStatus::Working,
            SessionStatus::Thinking,
            SessionStatus::WaitingForInput,
            SessionStatus::Compacting,
            SessionStatus::Error,
        ] {
            let snap = SessionSnapshot {
                status: status.clone(),
                agent_type: Some(AgentType::ClaudeCode),
                active_tool: Some(ActiveTool {
                    name: "Read".into(),
                    detail: Some("src/main.rs".into()),
                }),
                tool_count: 3,
                first_prompts: vec!["build the feature".into()],
                last_user_prompt: Some("build the feature".into()),
            };
            let json = serde_json::to_string(&snap).expect("SessionSnapshot serializes");
            let back: SessionSnapshot =
                serde_json::from_str(&json).expect("SessionSnapshot deserializes");
            assert_eq!(back.status, status, "status must round-trip for {status:?}");
            assert_eq!(back.agent_type, Some(AgentType::ClaudeCode));
            assert_eq!(
                back.active_tool.as_ref().map(|t| t.name.as_str()),
                Some("Read"),
                "active tool name must round-trip"
            );
            assert_eq!(back.tool_count, 3);
            assert_eq!(back.first_prompts, vec!["build the feature".to_string()]);
            assert_eq!(back.last_user_prompt.as_deref(), Some("build the feature"));
        }

        // (b) An AgentRecord carrying a live snapshot round-trips with live == Some.
        let rec = AgentRecord {
            id: "9".into(),
            pane_id_env: Some("pane-9".into()),
            display_name: None,
            cwd: None,
            tab_membership: None,
            agent_type: None,
            rows: 0,
            cols: 0,
            live: Some(SessionSnapshot {
                status: SessionStatus::Working,
                agent_type: Some(AgentType::ClaudeCode),
                active_tool: None,
                tool_count: 0,
                first_prompts: Vec::new(),
                last_user_prompt: None,
            }),
        };
        let json = serde_json::to_string(&rec).expect("AgentRecord serializes");
        let back: AgentRecord = serde_json::from_str(&json).expect("AgentRecord deserializes");
        let live = back
            .live
            .expect("live snapshot must survive the AgentRecord round-trip");
        assert_eq!(live.status, SessionStatus::Working);
        assert_eq!(live.agent_type, Some(AgentType::ClaudeCode));

        // (c) Back-compat: an older daemon's AgentRecord JSON has no `live`
        // field at all. It must decode via `#[serde(default)]` with
        // live == None — additive optional, no PROTOCOL_VERSION bump.
        let legacy = r#"{
            "id": "42",
            "pane_id_env": "pid-42",
            "display_name": "auditor",
            "cwd": "/work"
        }"#;
        let old: AgentRecord = serde_json::from_str(legacy)
            .expect("older daemon shape must decode via #[serde(default)] on live");
        assert!(
            old.live.is_none(),
            "older AgentRecord without a live field must decode as None"
        );
    }

    #[test]
    fn set_agent_label_serde_round_trip() {
        let req = AttachRequest::SetAgentLabel {
            id: "7".into(),
            display_name: Some("coder".into()),
            cwd: Some("/tmp/work".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["op"], "set-agent-label");
        assert_eq!(v["id"], "7");
        assert_eq!(v["display_name"], "coder");
        assert_eq!(v["cwd"], "/tmp/work");
        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        match back {
            AttachRequest::SetAgentLabel {
                id,
                display_name,
                cwd,
            } => {
                assert_eq!(id, "7");
                assert_eq!(display_name.as_deref(), Some("coder"));
                assert_eq!(cwd.as_deref(), Some("/tmp/work"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn resize_request_serde_round_trip() {
        // Wire shape must be `op = "resize"` (kebab-case) so existing
        // dispatcher matches the same way as start-agent / stop-agent.
        let req = AttachRequest::Resize {
            id: "agent-7".into(),
            rows: 50,
            cols: 200,
        };
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["op"], "resize");
        assert_eq!(v["id"], "agent-7");
        assert_eq!(v["rows"], 50);
        assert_eq!(v["cols"], 200);

        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        match back {
            AttachRequest::Resize { id, rows, cols } => {
                assert_eq!(id, "agent-7");
                assert_eq!(rows, 50);
                assert_eq!(cols, 200);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn subscribe_events_request_serde_round_trip() {
        // PRD #76 M2.17: SubscribeEvents has no payload fields, so the
        // wire shape is just `{"op": "subscribe-events"}`. Older daemons
        // would respond with `expected REQ frame, got kind 0x...` —
        // adding the variant doesn't break the existing dispatch.
        let req = AttachRequest::SubscribeEvents;
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["op"], "subscribe-events");
        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AttachRequest::SubscribeEvents));
    }

    #[tokio::test]
    async fn kind_event_frame_round_trip() {
        // The KIND_EVENT payload is a JSON-encoded BroadcastMsg.
        // PRD #93 round-5: only the Event variant rides this channel
        // now — Delegate / WorkDone are dispatched directly into PTYs
        // by the daemon. Pin the on-wire shape so a future rename of
        // the enum tag or the variant name trips the build instead of
        // silently breaking remote-mode TUIs.
        use crate::event::{AgentEvent, AgentType, BroadcastMsg, EventType};
        use chrono::Utc;
        use std::collections::HashMap;

        let event = AgentEvent {
            session_id: "sess-1".into(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::ToolStart,
            tool_name: Some("Read".into()),
            tool_detail: Some("src/main.rs".into()),
            cwd: Some("/work".into()),
            timestamp: Utc::now(),
            user_prompt: Some("fix the login bug".into()),
            metadata: HashMap::new(),
            pane_id: Some("7".into()),
            agent_id: None,
        };
        let payload = serde_json::to_vec(&BroadcastMsg::Event(event)).unwrap();

        // Pin the on-wire JSON shape. A self-symmetric round-trip
        // would pass even if someone renamed `#[serde(tag = "kind")]`
        // to `tag = "type"` or renamed the `Event` variant rename
        // from `"event"`. Spell the contract out so a future
        // structural rename trips the test.
        //
        // The TUI's `apply_event` reads `tool_detail`, `cwd`,
        // `timestamp`, and `user_prompt` in addition to the discriminator
        // fields, so pin those too — a self-symmetric round-trip would
        // otherwise hide a rename or omission that breaks the remote UI.
        let wire: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(wire["kind"], "event");
        assert_eq!(wire["session_id"], "sess-1");
        assert_eq!(wire["agent_type"], "claude_code");
        assert_eq!(wire["event_type"], "tool_start");
        assert_eq!(wire["tool_name"], "Read");
        assert_eq!(wire["tool_detail"], "src/main.rs");
        assert_eq!(wire["cwd"], "/work");
        assert!(wire["timestamp"].is_string());
        assert_eq!(wire["user_prompt"], "fix the login bug");
        assert_eq!(wire["pane_id"], "7");

        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, KIND_EVENT, &payload).await.unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let (kind, body) = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(kind, KIND_EVENT);
        let back: BroadcastMsg = serde_json::from_slice(&body).unwrap();
        let BroadcastMsg::Event(e) = back;
        assert_eq!(e.session_id, "sess-1");
        assert_eq!(e.event_type, EventType::ToolStart);
        assert_eq!(e.tool_name.as_deref(), Some("Read"));
        assert_eq!(e.pane_id.as_deref(), Some("7"));
    }

    #[test]
    fn response_helpers() {
        let r = AttachResponse::ok();
        assert!(r.ok);
        assert!(r.error.is_none());

        let r = AttachResponse::err("nope");
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("nope"));

        let r = AttachResponse::agents(vec!["1".into(), "2".into()]);
        assert!(r.ok);
        assert_eq!(
            r.agents.as_deref(),
            Some(&["1".to_string(), "2".to_string()][..])
        );

        let r = AttachResponse::with_id("42".into());
        assert!(r.ok);
        assert_eq!(r.id.as_deref(), Some("42"));
    }

    #[test]
    fn hello_request_serde_round_trip() {
        // PRD #76 M2.21: pin the on-wire JSON shape so a future structural
        // change to the AttachRequest enum trips the test rather than
        // silently breaking the handshake. Mirrors the
        // `kind_event_frame_round_trip` precedent.
        let req = AttachRequest::Hello {
            client_version: PROTOCOL_VERSION,
            client_build_version: Some(env!("DAD_BUILD_ID").to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["op"], "hello");
        assert_eq!(v["client_version"], PROTOCOL_VERSION);
        assert_eq!(v["client_build_version"], env!("DAD_BUILD_ID"));

        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        match back {
            AttachRequest::Hello {
                client_version,
                client_build_version,
            } => {
                assert_eq!(client_version, PROTOCOL_VERSION);
                assert_eq!(client_build_version.as_deref(), Some(env!("DAD_BUILD_ID")));
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn hello_request_omits_client_build_version_when_none() {
        // PRD #103 M1.2: when a (legacy) client doesn't populate
        // `client_build_version`, the wire payload must not carry the
        // field. Older daemons would reject any unknown key as a strictness
        // failure (they don't, but the contract holds anyway).
        let req = AttachRequest::Hello {
            client_version: PROTOCOL_VERSION,
            client_build_version: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("client_build_version"),
            "client_build_version=None must be omitted from the wire payload"
        );
    }

    #[test]
    fn hello_request_deserializes_legacy_shape_without_client_build_version() {
        // PRD #103 M1.2: a pre-PRD-103 client emits only `client_version`.
        // The daemon side must accept the payload and decode
        // `client_build_version` as None — `#[serde(default)]` makes this
        // work, but the test pins the wire contract.
        let json = r#"{"op":"hello","client_version":2}"#;
        let req: AttachRequest = serde_json::from_str(json).unwrap();
        match req {
            AttachRequest::Hello {
                client_version,
                client_build_version,
            } => {
                assert_eq!(client_version, 2);
                assert!(client_build_version.is_none());
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn hello_response_serde_round_trip() {
        let resp = AttachResponse::hello(PROTOCOL_VERSION);
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["server_version"], PROTOCOL_VERSION);
        // PRD #103 M1.1: hello() must populate `build_version` from the
        // daemon's compiled-in DAD_BUILD_ID so the laptop can detect
        // handler-code skew. The exact value is build-time-derived; we just
        // require it's present and non-empty here.
        let wire_build_version = v["build_version"]
            .as_str()
            .expect("hello() must emit build_version on the wire");
        assert!(
            !wire_build_version.is_empty(),
            "build_version must be non-empty"
        );
        assert_eq!(wire_build_version, env!("DAD_BUILD_ID"));

        let back: AttachResponse = serde_json::from_str(&json).unwrap();
        assert!(back.ok);
        assert_eq!(back.server_version, Some(PROTOCOL_VERSION));
        assert_eq!(back.build_version.as_deref(), Some(env!("DAD_BUILD_ID")));
    }

    #[test]
    fn response_omits_build_version_when_none() {
        // PRD #103 M1.1: forward compat. An unrelated response (e.g.
        // list-agents) must NOT carry `build_version` on the wire — older
        // peers ignore the field, newer peers treat its absence on a hello
        // reply as "incompatible / recycle the daemon".
        let resp = AttachResponse::ok();
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("build_version"),
            "build_version=None should be omitted from the wire payload"
        );
    }

    #[test]
    fn response_deserializes_legacy_shape_without_build_version() {
        // PRD #103 M1.1: a pre-PRD-103 daemon emits `server_version` but
        // not `build_version`. The newer client must accept the payload
        // and decode the field as None — which is what the mismatch logic
        // uses to flag "daemon too old / recycle it".
        let json = r#"{"ok":true,"server_version":2}"#;
        let resp: AttachResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.server_version, Some(2));
        assert!(resp.build_version.is_none());
    }

    #[test]
    fn response_omits_server_version_when_none() {
        // Forward compat: an unrelated response (e.g. list-agents) must NOT
        // carry `server_version` on the wire. Pre-M2.21 clients/daemons
        // ignore the field; newer clients use its absence as the signal to
        // treat the peer as protocol-too-old.
        let resp = AttachResponse::ok();
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("server_version"),
            "server_version=None should be omitted from the wire payload"
        );
    }

    #[test]
    fn response_deserializes_legacy_shape_without_server_version() {
        // A pre-M2.21 daemon never emits `server_version`. A newer client
        // must accept the payload and decode the field as None — which is
        // what the protocol-mismatch logic looks for to detect "remote too
        // old to know about the handshake".
        let json = r#"{"ok":true}"#;
        let resp: AttachResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert!(resp.server_version.is_none());
    }
}
