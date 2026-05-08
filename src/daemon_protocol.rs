//! Streaming attach protocol for the daemon (PRD #76, M1.2).
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

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use crate::agent_pty::{AgentPtyRegistry, SpawnOptions};

// ---------------------------------------------------------------------------
// Frame kinds
// ---------------------------------------------------------------------------

pub const KIND_REQ: u8 = 0x01;
pub const KIND_RESP: u8 = 0x02;
pub const KIND_STREAM_OUT: u8 = 0x10;
pub const KIND_STREAM_IN: u8 = 0x11;
pub const KIND_STREAM_END: u8 = 0x12;
pub const KIND_DETACH: u8 = 0x13;

/// Hard cap on a single frame's payload length. Defends against a malicious
/// or buggy peer trying to allocate gigabytes off a forged length prefix.
/// 16 MiB is well above any reasonable PTY chunk or scrollback snapshot.
const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Wire I/O
// ---------------------------------------------------------------------------

/// Read a single frame. Returns `Ok(None)` on clean EOF before any header
/// bytes have been read (peer closed the connection).
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
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
}

fn default_rows() -> u16 {
    24
}
fn default_cols() -> u16 {
    80
}

/// Discriminated by the populated optional fields rather than a tag, since
/// each request type has a fixed shape and clients can decide what to read
/// based on which request they sent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AttachResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
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
    pub fn with_id(id: String) -> Self {
        Self {
            ok: true,
            id: Some(id),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Bind the attach socket and serve protocol connections forever. Cleans up
/// any stale socket file before binding. Runs until the listener errors out
/// or the future is dropped.
pub async fn run_attach_server(path: &Path, registry: Arc<AgentPtyRegistry>) -> io::Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = crate::daemon::bind_socket(path)?;
    // Defense in depth — the umask-before-bind in `bind_socket` already
    // creates the inode at 0o600; restating the mode here means any future
    // code path that bypasses `bind_socket` still ends up with the right
    // permissions.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    info!("Attach protocol listening on {}", path.display());

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let registry = registry.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, registry).await {
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

async fn handle_connection(
    mut stream: UnixStream,
    registry: Arc<AgentPtyRegistry>,
) -> io::Result<()> {
    let frame = match read_frame(&mut stream).await? {
        Some(f) => f,
        None => return Ok(()),
    };
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
            let ids = registry.agent_ids();
            write_resp(&mut stream, &AttachResponse::agents(ids)).await?;
        }
        AttachRequest::StartAgent {
            command,
            cwd,
            rows,
            cols,
            env,
        } => {
            let opts = SpawnOptions {
                command: command.as_deref(),
                cwd: cwd.as_deref(),
                rows,
                cols,
                env,
            };
            match registry.spawn_agent(opts) {
                Ok(id) => write_resp(&mut stream, &AttachResponse::with_id(id)).await?,
                Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
            }
        }
        AttachRequest::StopAgent { id } => match registry.close_agent(&id) {
            Ok(()) => write_resp(&mut stream, &AttachResponse::ok()).await?,
            Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
        },
        AttachRequest::Snapshot { id } => match registry.snapshot(&id) {
            Ok(bytes) => {
                write_resp(&mut stream, &AttachResponse::ok()).await?;
                if !bytes.is_empty() {
                    write_frame(&mut stream, KIND_STREAM_OUT, &bytes).await?;
                }
                write_frame(&mut stream, KIND_STREAM_END, &[]).await?;
            }
            Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
        },
        AttachRequest::AttachStream { id } => {
            handle_attach_stream(stream, registry, id).await?;
        }
    }
    Ok(())
}

async fn write_resp<W: AsyncWrite + Unpin>(w: &mut W, resp: &AttachResponse) -> io::Result<()> {
    let payload = serde_json::to_vec(resp).expect("AttachResponse must serialize");
    write_frame(w, KIND_RESP, &payload).await
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
    //    delivered via `rx` below.
    if !handle.snapshot.is_empty() {
        write_frame(&mut wr, KIND_STREAM_OUT, &handle.snapshot).await?;
    }

    let mut rx = handle.rx;
    let writer = handle.writer;

    // Output task: forward broadcast bytes → STREAM_OUT frames. Owns `wr`
    // for the duration of streaming.
    let output_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(bytes) => {
                    if write_frame(&mut wr, KIND_STREAM_OUT, &bytes).await.is_err() {
                        // Client gone; bail out.
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // Agent terminated (reader thread saw EOF).
                    let _ = write_frame(&mut wr, KIND_STREAM_END, &[]).await;
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // This subscriber fell behind beyond BROADCAST_CAPACITY.
                    // Better to disconnect than to deliver corrupted ANSI;
                    // the client can reattach and replay scrollback.
                    let _ = write_frame(&mut wr, KIND_STREAM_END, b"lagged").await;
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
                if w.write_all(&bytes).is_err() {
                    break;
                }
                let _ = w.flush();
            }
            Ok(Some((KIND_DETACH, _))) => break,
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
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        match back {
            AttachRequest::StartAgent { command, env, .. } => {
                assert_eq!(command.as_deref(), Some("/bin/sh"));
                assert_eq!(env, vec![("FOO".to_string(), "BAR".to_string())]);
            }
            _ => panic!("wrong variant"),
        }
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
}
