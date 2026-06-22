//! Shared wire types for the dot-agent-deck daemon attach protocol.
//!
//! PRD #176 M1.1 extracts these out of the `dot-agent-deck` binary into their
//! own workspace crate so every client of the daemon — the TUI, the CLI daemon
//! commands, the integration tests, and (M1.2 onward) the desktop GUI core —
//! depends on exactly ONE definition of the protocol. The daemon SERVER (PTY
//! ownership, request handlers, the accept loop) stays in the binary; only the
//! shared wire shapes live here.
//!
//! # Layout
//!
//! - [`event`] — hook-event and orchestration-signal payloads
//!   ([`AgentEvent`], [`BroadcastMsg`], [`DaemonMessage`], …).
//! - [`record`] — the [`AgentRecord`] / [`TabMembership`] snapshot types echoed
//!   back over `list-agents`.
//! - [`wire`] — the length-prefixed frame codec, frame kinds,
//!   [`AttachRequest`] / [`AttachResponse`], and [`PROTOCOL_VERSION`].
//! - [`socket`] — daemon Unix-socket-path discovery
//!   ([`attach_socket_path`] / [`socket_path`]) shared by the TUI, the CLI,
//!   and the GUI core (PRD #176 M1.2) so no client invents a divergent path.
//!
//! Everything public is re-exported at the crate root so callers can write
//! `protocol::AttachRequest`, `protocol::AgentRecord`, `protocol::read_frame`,
//! `protocol::attach_socket_path`, etc., without naming the submodule.

pub mod event;
pub mod record;
pub mod socket;
pub mod wire;

pub use event::*;
pub use record::*;
pub use socket::*;
pub use wire::*;
