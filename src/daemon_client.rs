//! Client side of the M1.2 streaming attach protocol (PRD #76, M1.3).
//!
//! The TUI's stream-backed pane drives the daemon through this module — never
//! by reaching into [`crate::daemon_protocol`]'s frame helpers directly. The
//! protocol layer takes generic [`AsyncRead`]/[`AsyncWrite`] so the same code
//! paths run over a Unix socket today and will run over piped stdio in M2.1
//! (`daemon attach`). Only [`DaemonClient::connect`] and the resulting
//! [`AttachConnection`] are Unix-socket specific.
//!
//! Wire types ([`AttachRequest`], [`AttachResponse`], frame kinds) are
//! re-exported from [`crate::daemon_protocol`] — there is exactly one
//! definition of the wire format in the crate.

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

pub use crate::agent_pty::{AgentRecord, TabMembership, validate_tab_membership};
use crate::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_DETACH, KIND_REQ, KIND_RESP, KIND_STREAM_END,
    KIND_STREAM_OUT, read_frame, write_frame,
};

/// Errors returned by the client. Server-side error responses are surfaced
/// as [`ClientError::Server`] with the daemon's message; transport problems
/// surface as [`ClientError::Io`].
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("I/O error talking to daemon: {0}")]
    Io(#[from] io::Error),
    #[error("daemon returned error: {0}")]
    Server(String),
    #[error("daemon attach socket {0} does not exist (is the daemon running?)")]
    SocketMissing(PathBuf),
    #[error("malformed daemon response: {0}")]
    Malformed(String),
}

/// Owned counterpart of [`AttachRequest::StartAgent`]. Owned (vs. borrowed)
/// because callers are typically blocking threads that need to hand the
/// options off to an async task running on the tokio runtime.
#[derive(Debug, Clone)]
pub struct StartAgentOptions {
    pub command: Option<String>,
    pub cwd: Option<String>,
    /// Human-readable label captured into the daemon's per-agent registry
    /// (M2.11). Forwarded as `AttachRequest::StartAgent.display_name`; the
    /// daemon validates it via `is_valid_display_name` and stores `None`
    /// on failure. `None` here omits the field from the wire payload so
    /// older daemons keep accepting the request.
    pub display_name: Option<String>,
    pub rows: u16,
    pub cols: u16,
    pub env: Vec<(String, String)>,
    /// PRD #76 M2.12: which tab the TUI placed this agent pane in
    /// (mode / orchestration). Forwarded as
    /// `AttachRequest::StartAgent.tab_membership` so the daemon can
    /// echo it back via `list_agents` and the TUI can rebuild tab
    /// structure on reconnect. `None` here means "dashboard pane" and
    /// omits the field from the wire payload so older daemons keep
    /// accepting the request.
    pub tab_membership: Option<TabMembership>,
}

impl Default for StartAgentOptions {
    fn default() -> Self {
        Self {
            command: None,
            cwd: None,
            display_name: None,
            rows: 24,
            cols: 80,
            env: Vec::new(),
            tab_membership: None,
        }
    }
}

// ---------------------------------------------------------------------------
// I/O-generic protocol helpers (transport-independent — work over UnixStream
// today and over piped stdio in M2.1).
// ---------------------------------------------------------------------------

/// Send a single REQ frame carrying a JSON-encoded [`AttachRequest`].
pub async fn send_request<W: AsyncWrite + Unpin>(
    wr: &mut W,
    req: &AttachRequest,
) -> io::Result<()> {
    let payload = serde_json::to_vec(req)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    write_frame(wr, KIND_REQ, &payload).await
}

/// Read a single RESP frame and decode it. Errors out on EOF, wrong frame
/// kind, or malformed JSON.
pub async fn read_response<R: AsyncRead + Unpin>(
    rd: &mut R,
) -> Result<AttachResponse, ClientError> {
    match read_frame(rd).await? {
        None => Err(ClientError::Malformed(
            "daemon closed connection before sending RESP".into(),
        )),
        Some((KIND_RESP, payload)) => serde_json::from_slice(&payload)
            .map_err(|e| ClientError::Malformed(format!("RESP JSON: {e}"))),
        Some((kind, _)) => Err(ClientError::Malformed(format!(
            "expected RESP, got frame kind 0x{kind:02x}"
        ))),
    }
}

/// One-shot request/response: send `req`, read one RESP, return it. Used for
/// non-streaming operations (`list-agents`, `start-agent`, `stop-agent`).
pub async fn issue_command<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    rd: &mut R,
    wr: &mut W,
    req: &AttachRequest,
) -> Result<AttachResponse, ClientError> {
    send_request(wr, req).await?;
    read_response(rd).await
}

/// Clamp `record.tab_membership` to `None` if the embedded `name` fails
/// [`validate_tab_membership`]. Defense in depth at the wire boundary
/// (M2.12 fixup auditor #1): the daemon validates on `StartAgent`, but
/// a malformed or older daemon could still echo an invalid record. A
/// rejected membership is logged via `tracing::warn!` (the agent is
/// real — we just don't trust the bucketing hint).
fn sanitize_record_tab_membership(rec: &mut AgentRecord) {
    if let Some(tm) = rec.tab_membership.take() {
        let name_len = tm.name().len();
        match validate_tab_membership(tm) {
            Some(v) => rec.tab_membership = Some(v),
            None => {
                tracing::warn!(
                    agent_id = %rec.id,
                    name_len,
                    "list_agents: clamping invalid tab_membership.name from daemon record to None — pane lands on dashboard"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unix-socket transport
// ---------------------------------------------------------------------------

/// Thin handle around the daemon's attach socket path. Cheap to clone — every
/// operation opens its own short-lived `UnixStream` (matching the daemon's
/// per-connection state machine in [`crate::daemon_protocol`]).
#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket_path: PathBuf,
}

impl DaemonClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Surface a clear "daemon not running" error before any I/O is
    /// attempted. The remote-deck-local TUI calls this at startup so the
    /// user doesn't see a generic ECONNREFUSED.
    pub fn ensure_socket_exists(&self) -> Result<(), ClientError> {
        if !self.socket_path.exists() {
            return Err(ClientError::SocketMissing(self.socket_path.clone()));
        }
        Ok(())
    }

    async fn connect(&self) -> io::Result<UnixStream> {
        UnixStream::connect(&self.socket_path).await
    }

    /// List daemon-side agents. Returns one [`AgentRecord`] per agent,
    /// preferring the daemon's new `agent_records` field (which carries
    /// each agent's spawn-time `DOT_AGENT_DECK_PANE_ID`). Falls back to
    /// the legacy `agents`-only field with `pane_id_env: None` so a
    /// newer TUI keeps working against an older daemon — at the cost of
    /// not being able to preserve pane ids on rehydration there.
    ///
    /// M2.12 fixup auditor #1: re-validates each record's
    /// `tab_membership` at this wire boundary. The daemon validates
    /// `StartAgent.tab_membership` before storing it, but a malformed
    /// or older daemon could still echo back an invalid `name` here. An
    /// invalid membership is cleared to `None` (the agent is real, it
    /// just lands on the dashboard) and a `tracing::warn!` surfaces the
    /// drift — we never propagate a control-byte name into
    /// bucketing/logging/tab lookup.
    pub async fn list_agents(&self) -> Result<Vec<AgentRecord>, ClientError> {
        let stream = self.connect().await?;
        let (mut rd, mut wr) = stream.into_split();
        let resp = issue_command(&mut rd, &mut wr, &AttachRequest::ListAgents).await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error.unwrap_or_else(|| "list-agents failed".into()),
            ));
        }
        if let Some(mut records) = resp.agent_records {
            for rec in &mut records {
                sanitize_record_tab_membership(rec);
            }
            return Ok(records);
        }
        Ok(resp
            .agents
            .unwrap_or_default()
            .into_iter()
            .map(|id| AgentRecord {
                id,
                pane_id_env: None,
                display_name: None,
                cwd: None,
                tab_membership: None,
            })
            .collect())
    }

    pub async fn start_agent(&self, opts: StartAgentOptions) -> Result<String, ClientError> {
        let stream = self.connect().await?;
        let (mut rd, mut wr) = stream.into_split();
        let req = AttachRequest::StartAgent {
            command: opts.command,
            cwd: opts.cwd,
            display_name: opts.display_name,
            rows: opts.rows,
            cols: opts.cols,
            env: opts.env,
            tab_membership: opts.tab_membership,
        };
        let resp = issue_command(&mut rd, &mut wr, &req).await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error.unwrap_or_else(|| "start-agent failed".into()),
            ));
        }
        resp.id
            .ok_or_else(|| ClientError::Malformed("start-agent ok but no id in response".into()))
    }

    /// Push a TUI pane resize through to the daemon's PTY. Idempotent on the
    /// wire: each call opens a fresh short-lived connection (matching the
    /// pattern used for `stop_agent` / `list_agents`). Callers that fire
    /// resize on every layout pass should treat transient errors as
    /// best-effort — the next resize will reconcile.
    pub async fn resize_agent(&self, id: &str, rows: u16, cols: u16) -> Result<(), ClientError> {
        let stream = self.connect().await?;
        let (mut rd, mut wr) = stream.into_split();
        let resp = issue_command(
            &mut rd,
            &mut wr,
            &AttachRequest::Resize {
                id: id.to_string(),
                rows,
                cols,
            },
        )
        .await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error.unwrap_or_else(|| "resize failed".into()),
            ));
        }
        Ok(())
    }

    /// Update the daemon-side display_name and/or cwd for an agent (M2.11).
    /// Passing `None` for either field clears it. The daemon validates both
    /// values independently and silently drops anything that fails — see
    /// `AgentPtyRegistry::set_agent_label` for the rules. Best-effort: the
    /// TUI calls this from the rename flow on every keystroke commit, so a
    /// transient daemon error here is logged at the call site, not
    /// propagated.
    pub async fn set_agent_label(
        &self,
        id: &str,
        display_name: Option<String>,
        cwd: Option<String>,
    ) -> Result<(), ClientError> {
        let stream = self.connect().await?;
        let (mut rd, mut wr) = stream.into_split();
        let resp = issue_command(
            &mut rd,
            &mut wr,
            &AttachRequest::SetAgentLabel {
                id: id.to_string(),
                display_name,
                cwd,
            },
        )
        .await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error
                    .unwrap_or_else(|| "set-agent-label failed".into()),
            ));
        }
        Ok(())
    }

    pub async fn stop_agent(&self, id: &str) -> Result<(), ClientError> {
        let stream = self.connect().await?;
        let (mut rd, mut wr) = stream.into_split();
        let resp = issue_command(
            &mut rd,
            &mut wr,
            &AttachRequest::StopAgent { id: id.to_string() },
        )
        .await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error.unwrap_or_else(|| "stop-agent failed".into()),
            ));
        }
        Ok(())
    }

    /// Open an attach-stream connection. Returns once the daemon has
    /// confirmed the attach with a successful RESP — i.e. the next frame on
    /// the wire is the consistent scrollback snapshot, followed by live
    /// STREAM_OUT frames (see [`crate::daemon_protocol`]'s state-machine
    /// docs).
    pub async fn attach(&self, id: &str) -> Result<AttachConnection, ClientError> {
        let stream = self.connect().await?;
        let (mut rd, mut wr) = stream.into_split();
        let resp = issue_command(
            &mut rd,
            &mut wr,
            &AttachRequest::AttachStream { id: id.to_string() },
        )
        .await?;
        if !resp.ok {
            return Err(ClientError::Server(
                resp.error.unwrap_or_else(|| "attach-stream failed".into()),
            ));
        }
        Ok(AttachConnection { rd, wr })
    }
}

/// Live attach-stream connection. After a successful [`DaemonClient::attach`]
/// the next read returns the daemon-supplied scrollback snapshot, then live
/// STREAM_OUT frames until the agent exits or the client detaches.
pub struct AttachConnection {
    rd: OwnedReadHalf,
    wr: OwnedWriteHalf,
}

impl AttachConnection {
    /// Read the next chunk of agent output. Returns `Ok(None)` on
    /// `STREAM_END` or peer EOF — the stream is over and the caller should
    /// drop the connection. Unexpected frame kinds are logged via `tracing`
    /// and treated as EOF (the daemon closes the connection on protocol
    /// violations rather than sending `STREAM_END`).
    pub async fn next_output(&mut self) -> io::Result<Option<Vec<u8>>> {
        match read_frame(&mut self.rd).await? {
            None => Ok(None),
            Some((KIND_STREAM_OUT, bytes)) => Ok(Some(bytes)),
            Some((KIND_STREAM_END, _)) => Ok(None),
            Some((kind, _)) => {
                tracing::warn!("unexpected frame kind 0x{kind:02x} on attach stream — ending");
                Ok(None)
            }
        }
    }

    /// Forward a chunk of keystrokes to the daemon's PTY writer.
    pub async fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        write_frame(&mut self.wr, crate::daemon_protocol::KIND_STREAM_IN, bytes).await
    }

    /// Send an explicit DETACH frame. Best-effort — if the write fails the
    /// daemon will still observe the close as detach when the socket is
    /// dropped.
    pub async fn detach(mut self) -> io::Result<()> {
        write_frame(&mut self.wr, KIND_DETACH, &[]).await
    }

    /// Split into owned halves for callers that drive read and write tasks
    /// concurrently (the typical pane wiring).
    pub fn into_split(self) -> (OwnedReadHalf, OwnedWriteHalf) {
        (self.rd, self.wr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    use crate::agent_pty::AgentPtyRegistry;
    use crate::daemon_protocol::{bind_attach_listener, serve_attach};

    /// Mirror the harness lock from `tests/daemon_protocol.rs`: `bind_socket`
    /// flips the process-global umask while binding, and a tempdir created
    /// inside that window inherits 0o600, breaking later binds. Hold this
    /// across tempdir+bind for any in-process attach server.
    static BIND_LOCK: Mutex<()> = Mutex::new(());

    async fn spawn_test_server() -> (TempDir, PathBuf, Arc<AgentPtyRegistry>) {
        let registry = Arc::new(AgentPtyRegistry::new());
        let (dir, path, listener) = {
            let _g = BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("attach.sock");
            let listener = bind_attach_listener(&path).expect("bind");
            (dir, path, listener)
        };
        let reg = registry.clone();
        tokio::spawn(async move {
            let _ = serve_attach(listener, reg).await;
        });
        (dir, path, registry)
    }

    #[tokio::test]
    async fn ensure_socket_exists_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.sock");
        let client = DaemonClient::new(missing.clone());
        let err = client.ensure_socket_exists().unwrap_err();
        assert!(matches!(err, ClientError::SocketMissing(p) if p == missing));
    }

    #[tokio::test]
    async fn start_list_stop_round_trip() {
        let (_dir, path, registry) = spawn_test_server().await;
        let client = DaemonClient::new(path);

        let id = client
            .start_agent(StartAgentOptions {
                command: Some("/bin/sh".into()),
                ..Default::default()
            })
            .await
            .expect("start should succeed");

        let agents = client.list_agents().await.unwrap();
        let ids: Vec<String> = agents.iter().map(|a| a.id.clone()).collect();
        assert_eq!(ids, vec![id.clone()]);

        client.stop_agent(&id).await.expect("stop should succeed");
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn start_agent_blank_command_returns_server_error() {
        let (_dir, path, _registry) = spawn_test_server().await;
        let client = DaemonClient::new(path);
        let err = client
            .start_agent(StartAgentOptions {
                command: Some("   ".into()),
                ..Default::default()
            })
            .await
            .expect_err("blank command should fail");
        assert!(matches!(err, ClientError::Server(_)));
    }

    #[tokio::test]
    async fn attach_streams_output_and_input() {
        let (_dir, path, registry) = spawn_test_server().await;
        let client = DaemonClient::new(path);

        let id = client
            .start_agent(StartAgentOptions {
                command: Some("/bin/sh".into()),
                ..Default::default()
            })
            .await
            .unwrap();

        let mut conn = client.attach(&id).await.expect("attach");

        // Drive output via STREAM_IN; observe it via STREAM_OUT.
        conn.write_input(b"echo CLIENT-MARKER\n").await.unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut acc = Vec::new();
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline - tokio::time::Instant::now();
            match tokio::time::timeout(remaining, conn.next_output()).await {
                Ok(Ok(Some(bytes))) => {
                    acc.extend_from_slice(&bytes);
                    if acc
                        .windows(b"CLIENT-MARKER".len())
                        .any(|w| w == b"CLIENT-MARKER")
                    {
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(
            acc.windows(b"CLIENT-MARKER".len())
                .any(|w| w == b"CLIENT-MARKER"),
            "expected marker in stream; got {:?}",
            String::from_utf8_lossy(&acc)
        );

        registry.close_agent(&id).unwrap();
    }

    #[test]
    fn sanitize_record_tab_membership_strips_invalid_name() {
        // M2.12 fixup auditor #1: the daemon validates `tab_membership`
        // on `StartAgent`, but a malformed or older daemon could echo
        // back a record carrying an invalid `name`. The client-side
        // boundary sanitizer must clamp the membership to `None` so the
        // TUI's bucketing / tracing never sees control bytes — the
        // agent is still real and lands on the dashboard.
        let mut rec = AgentRecord {
            id: "7".into(),
            pane_id_env: None,
            display_name: None,
            cwd: None,
            tab_membership: Some(TabMembership::Mode {
                name: "\x1b[31mevil".into(),
            }),
        };
        sanitize_record_tab_membership(&mut rec);
        assert!(rec.tab_membership.is_none(), "invalid name must be cleared");

        // And a valid record round-trips untouched.
        let mut ok = AgentRecord {
            id: "8".into(),
            pane_id_env: None,
            display_name: None,
            cwd: None,
            tab_membership: Some(TabMembership::Orchestration {
                name: "tdd-cycle".into(),
                role_index: 2,
            }),
        };
        sanitize_record_tab_membership(&mut ok);
        assert_eq!(
            ok.tab_membership,
            Some(TabMembership::Orchestration {
                name: "tdd-cycle".into(),
                role_index: 2,
            }),
        );
    }
}
