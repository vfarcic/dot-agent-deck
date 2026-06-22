//! Streaming attach protocol wire types (PRD #76; extracted in PRD #176 M1.1).
//!
//! This module holds the *shared* shapes of the daemon attach protocol — the
//! length-prefixed frame codec, the frame kinds, [`AttachRequest`] /
//! [`AttachResponse`], and [`PROTOCOL_VERSION`]. The daemon SERVER (accept
//! loop, request handlers, PTY ownership) lives in the `dot-agent-deck` binary
//! and depends on these types; it is not part of this crate.
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
//! Control frames carry JSON and stream frames carry raw PTY bytes — no extra
//! build deps, and the framing is portable to stdio (no socket-only
//! assumptions: no fd passing, no `SCM_RIGHTS`).
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
//! | `KIND_EVENT`      | server → client | JSON [`crate::event::BroadcastMsg`] (after a `SubscribeEvents` request) |
//! | `KIND_SHUTDOWN`   | client → server | empty — shut the daemon down (PRD #92 F1) |
//! | `KIND_SHUTDOWN_ACK` | server → client | empty — acknowledges `KIND_SHUTDOWN` before teardown begins |

use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::event::AgentType;
use crate::record::{AgentRecord, TabMembership};

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
/// payload), sends back a [`KIND_SHUTDOWN_ACK`] **before** beginning
/// teardown, then iterates its agent registry, SIGTERMs each child with a
/// short grace before SIGKILL, then exits.
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
/// alike as errors, surfaces them, and does not exit the TUI.
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
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

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
    /// `pane_id` + `text` straight to the daemon's
    /// `write_to_pane_and_submit`, which holds the per-agent writer mutex
    /// across the full `payload → SUBMIT_DELAY → CR` sequence (PRD #93
    /// round-8 atomic contract). Lets a TUI client trigger the same atomic
    /// byte stream the daemon-initiated orchestration-delegate path
    /// already produces — without the two-`STREAM_IN`-frames-with-150ms-gap
    /// pattern, whose mid-sequence mutex release lets a concurrent
    /// daemon-initiated write interleave and fuse a daemon-side CR onto
    /// the user's payload, submitting it prematurely.
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
    /// PRD #103 M1.1 / PRD #161 M1.1: `build_version` (the daemon's
    /// compiled-in `DAD_BUILD_ID`) and `daemon_version` (its `DAD_VERSION`
    /// semver tag) are passed in by the caller, because those values come
    /// from the *binary's* build-time environment — which this protocol
    /// crate has no access to. The binary's `daemon_protocol::hello_response`
    /// helper fills them in from `env!`/`local_build_id()`.
    pub fn hello(version: u32, build_version: String, daemon_version: String) -> Self {
        Self {
            ok: true,
            server_version: Some(version),
            build_version: Some(build_version),
            daemon_version: Some(daemon_version),
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn start_agent_deserializes_old_client_shape_without_tab_membership() {
        // M2.12 fixup auditor #4: explicit compat test using a
        // hand-crafted JSON literal in the *old* client shape — no
        // `tab_membership` field at all. A newer daemon must accept
        // the payload and decode `tab_membership: None`.
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

    #[test]
    fn hello_response_sets_all_handshake_fields() {
        // The protocol-crate `hello` constructor takes the binary's
        // build/version strings as params (the crate has no `env!` access).
        // Pin that it populates every handshake field from them.
        let resp = AttachResponse::hello(PROTOCOL_VERSION, "build-xyz".into(), "0.31.1".into());
        assert!(resp.ok);
        assert_eq!(resp.server_version, Some(PROTOCOL_VERSION));
        assert_eq!(resp.build_version.as_deref(), Some("build-xyz"));
        assert_eq!(resp.daemon_version.as_deref(), Some("0.31.1"));
    }

    #[test]
    fn running_agents_summary_from_records_labels_by_display_name_then_id() {
        let records = vec![
            AgentRecord {
                id: "1".into(),
                pane_id_env: None,
                display_name: Some("alpha".into()),
                cwd: None,
                tab_membership: None,
                agent_type: None,
                rows: 0,
                cols: 0,
            },
            AgentRecord {
                id: "2".into(),
                pane_id_env: None,
                display_name: None,
                cwd: None,
                tab_membership: None,
                agent_type: None,
                rows: 0,
                cols: 0,
            },
        ];
        let summary = RunningAgentsSummary::from_records(&records);
        assert_eq!(summary.count, 2);
        assert_eq!(summary.names, vec!["alpha".to_string(), "2".to_string()]);
    }
}
