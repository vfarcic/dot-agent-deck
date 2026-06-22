//! Connect → `Hello` negotiate → bridge frames.
//!
//! Transport is the daemon's Unix attach socket. The handshake mirrors the
//! binary's `build_version_handshake::probe_daemon`: open the socket, send a
//! [`AttachRequest::Hello`] carrying our [`PROTOCOL_VERSION`], read the
//! [`AttachResponse`], and accept only when the daemon reports the same
//! version. Everything else (missing socket, refused connection, a rejected or
//! malformed reply, a version mismatch) is surfaced as a [`ConnectError`] the
//! shell renders as a clear connect/retry state.

use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use protocol::{
    AttachRequest, AttachResponse, KIND_REQ, KIND_RESP, PROTOCOL_VERSION, read_frame, write_frame,
};

/// One length-prefixed daemon frame bridged toward the webview. For M1.2 this
/// carries the raw `(kind, payload)` so the wiring is generic; the live
/// terminal stream (`KIND_STREAM_OUT`) and its efficient transport land in
/// M1.3. `payload` serializes as a JSON byte array — fine for the handshake +
/// wiring milestone; M1.3 will move the hot path onto a Tauri raw channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BridgeFrame {
    pub kind: u8,
    pub payload: Vec<u8>,
}

/// Errors from the connect + handshake path. The shell maps these to a
/// [`ConnectionState`] via [`ConnectionState::from_connect_error`].
#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("daemon attach socket {0} does not exist (is the daemon running?)")]
    SocketMissing(PathBuf),
    #[error("could not connect to daemon attach socket: {0}")]
    Connect(io::Error),
    #[error("I/O error during handshake: {0}")]
    Io(io::Error),
    #[error("daemon rejected the handshake: {0}")]
    Rejected(String),
    #[error("malformed handshake reply: {0}")]
    Malformed(String),
    #[error(
        "protocol version mismatch: this GUI speaks v{local}, daemon reports {remote:?} \
         (recycle the daemon so both run the same build)"
    )]
    VersionMismatch { local: u32, remote: Option<u32> },
}

/// The connection state the webview renders. `Serialize` so the Tauri shell
/// can emit it straight to the frontend as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum ConnectionState {
    /// A connect/handshake attempt is in flight.
    Connecting,
    /// Handshake succeeded; versions negotiated.
    Connected {
        protocol_version: u32,
        /// The daemon's semver tag (`DAD_VERSION`), if it reported one.
        daemon_version: Option<String>,
        /// The daemon's finer-grained build id (`DAD_BUILD_ID`), if reported.
        build_version: Option<String>,
    },
    /// Reached the daemon but the protocol versions are incompatible.
    VersionMismatch { local: u32, remote: Option<u32> },
    /// Could not connect, or the handshake failed — the webview shows a clear
    /// connect/retry affordance carrying `reason`.
    Disconnected { reason: String },
}

impl ConnectionState {
    /// Map a [`ConnectError`] to the state the webview should display.
    pub fn from_connect_error(err: &ConnectError) -> Self {
        match err {
            ConnectError::VersionMismatch { local, remote } => ConnectionState::VersionMismatch {
                local: *local,
                remote: *remote,
            },
            other => ConnectionState::Disconnected {
                reason: other.to_string(),
            },
        }
    }
}

/// The read half of an established connection: yields daemon frames.
#[derive(Debug)]
pub struct BridgeReader {
    rd: OwnedReadHalf,
}

impl BridgeReader {
    /// Read the next daemon frame. Returns `Ok(None)` on a clean EOF (the
    /// daemon closed the connection between frames).
    pub async fn next_frame(&mut self) -> io::Result<Option<BridgeFrame>> {
        Ok(read_frame(&mut self.rd)
            .await?
            .map(|(kind, payload)| BridgeFrame { kind, payload }))
    }
}

/// The write half of an established connection: sends frames to the daemon
/// (e.g. `KIND_STREAM_IN` keystrokes once the terminal pane lands in M1.3).
#[derive(Debug)]
pub struct BridgeWriter {
    wr: OwnedWriteHalf,
}

impl BridgeWriter {
    /// Send one length-prefixed frame to the daemon.
    pub async fn send_frame(&mut self, kind: u8, payload: &[u8]) -> io::Result<()> {
        write_frame(&mut self.wr, kind, payload).await
    }
}

/// A live, handshake-completed connection to the daemon.
#[derive(Debug)]
pub struct DaemonConnection {
    /// The negotiated protocol version (equals [`PROTOCOL_VERSION`]).
    pub protocol_version: u32,
    /// The daemon's reported semver tag (`DAD_VERSION`), if any.
    pub daemon_version: Option<String>,
    /// The daemon's reported build id (`DAD_BUILD_ID`), if any.
    pub build_version: Option<String>,
    reader: BridgeReader,
    writer: BridgeWriter,
}

impl DaemonConnection {
    /// The [`ConnectionState::Connected`] view of this connection, ready to
    /// emit to the webview.
    pub fn state(&self) -> ConnectionState {
        ConnectionState::Connected {
            protocol_version: self.protocol_version,
            daemon_version: self.daemon_version.clone(),
            build_version: self.build_version.clone(),
        }
    }

    /// Read the next frame the daemon sent (`Ok(None)` on clean EOF).
    pub async fn next_frame(&mut self) -> io::Result<Option<BridgeFrame>> {
        self.reader.next_frame().await
    }

    /// Send a frame to the daemon.
    pub async fn send_frame(&mut self, kind: u8, payload: &[u8]) -> io::Result<()> {
        self.writer.send_frame(kind, payload).await
    }

    /// Split into independent halves so the shell can run a read-pump task
    /// while keeping the writer for input.
    pub fn into_split(self) -> (BridgeReader, BridgeWriter) {
        (self.reader, self.writer)
    }
}

/// Connect to the daemon at `socket_path`, perform the `Hello` handshake, and
/// return a live [`DaemonConnection`] on a matching protocol version.
///
/// `client_build_version` is forwarded as `Hello.client_build_version` (the
/// daemon logs it but never rejects on it); pass `None` if the caller has no
/// build id to advertise.
pub async fn connect_and_handshake(
    socket_path: &Path,
    client_build_version: Option<String>,
) -> Result<DaemonConnection, ConnectError> {
    // Surface a clear "daemon not running" error before any I/O — mirrors the
    // TUI's `DaemonClient::ensure_socket_exists`.
    if !socket_path.exists() {
        return Err(ConnectError::SocketMissing(socket_path.to_path_buf()));
    }

    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(ConnectError::Connect)?;
    let (rd, wr) = stream.into_split();
    let mut reader = BridgeReader { rd };
    let mut writer = BridgeWriter { wr };

    // Send Hello (mirrors src/build_version_handshake.rs::probe_daemon).
    let hello = AttachRequest::Hello {
        client_version: PROTOCOL_VERSION,
        client_build_version,
    };
    let payload = serde_json::to_vec(&hello).map_err(|e| ConnectError::Malformed(e.to_string()))?;
    write_frame(&mut writer.wr, KIND_REQ, &payload)
        .await
        .map_err(ConnectError::Io)?;

    let resp = read_handshake_response(&mut reader.rd).await?;
    if !resp.ok {
        return Err(ConnectError::Rejected(
            resp.error.unwrap_or_else(|| "handshake rejected".into()),
        ));
    }

    match resp.server_version {
        Some(v) if v == PROTOCOL_VERSION => Ok(DaemonConnection {
            protocol_version: v,
            daemon_version: resp.daemon_version,
            build_version: resp.build_version,
            reader,
            writer,
        }),
        other => Err(ConnectError::VersionMismatch {
            local: PROTOCOL_VERSION,
            remote: other,
        }),
    }
}

/// Read one `RESP` frame and decode the [`AttachResponse`]. EOF, a wrong frame
/// kind, or malformed JSON are all reported as a [`ConnectError`].
async fn read_handshake_response(rd: &mut OwnedReadHalf) -> Result<AttachResponse, ConnectError> {
    match read_frame(rd).await.map_err(ConnectError::Io)? {
        None => Err(ConnectError::Malformed(
            "daemon closed the connection before replying to Hello".into(),
        )),
        Some((KIND_RESP, payload)) => serde_json::from_slice(&payload)
            .map_err(|e| ConnectError::Malformed(format!("Hello RESP JSON: {e}"))),
        Some((kind, _)) => Err(ConnectError::Malformed(format!(
            "expected RESP, got frame kind 0x{kind:02x}"
        ))),
    }
}

/// Pump every daemon frame from `reader` into `tx` until the daemon closes the
/// stream or the receiver (the webview pump) is dropped. The Tauri shell
/// spawns this and forwards each [`BridgeFrame`] to the webview as an event.
pub async fn run_bridge(mut reader: BridgeReader, tx: mpsc::Sender<BridgeFrame>) -> io::Result<()> {
    while let Some(frame) = reader.next_frame().await? {
        if tx.send(frame).await.is_err() {
            // Receiver gone — nothing left to bridge to.
            break;
        }
    }
    Ok(())
}
