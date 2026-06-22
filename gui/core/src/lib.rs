//! Desktop GUI core (PRD #176 M1.2).
//!
//! This is the *thin, testable* half of the Tauri desktop app: a plain Rust
//! library — no webview, no Tauri, no JS — that
//!
//! 1. discovers the dot-agent-deck daemon's Unix attach socket (via the shared
//!    [`protocol::attach_socket_path`], the exact function the TUI uses);
//! 2. connects and performs the [`protocol`] `Hello` version-negotiation
//!    handshake (mirroring the binary's `build_version_handshake::probe_daemon`
//!    flow — send our [`protocol::PROTOCOL_VERSION`], compare the daemon's);
//! 3. bridges length-prefixed daemon frames to/from a channel the Tauri shell
//!    pumps into the webview.
//!
//! Splitting it out this way keeps the Rust gates (`cargo fmt` / `clippy` /
//! `test-fast`) authoritative over the connect/handshake/bridge logic: those
//! gates compile and exercise THIS crate, while the Tauri shell (which needs
//! the system WebKitGTK dev libraries) is a separate, workspace-`exclude`d
//! package layered on top — see `gui/src-tauri`.
//!
//! The GUI is a *fourth client* of the daemon (Design Decision #1): it holds
//! no business logic and reuses the exact wire types and socket-path discovery
//! the TUI uses, so a second front-end can never invent a divergent path or a
//! parallel protocol definition.

mod agent;
mod client;

pub use agent::{
    AgentStream, AgentStreamReader, AgentStreamWriter, ClientError, ResizeHandle, attach_stream,
    list_agents, resize_agent, resize_channel, run_resize_worker,
};
pub use client::{
    BridgeFrame, BridgeReader, BridgeWriter, ConnectError, ConnectionState, DaemonConnection,
    connect_and_handshake, run_bridge,
};

/// The per-agent snapshot the daemon echoes over `list-agents`, re-exported so
/// the Tauri shell names the same wire type the core returns from
/// [`list_agents`] (PRD #176 M1.3).
pub use protocol::AgentRecord;

/// The shared socket-path discovery, re-exported so the Tauri shell resolves
/// the daemon socket through the same function the TUI does (PRD #93).
pub use protocol::{attach_socket_path, socket_path};

/// The protocol version this GUI core was compiled against — the value sent in
/// the `Hello` handshake and matched against the daemon's reply.
pub use protocol::PROTOCOL_VERSION;
