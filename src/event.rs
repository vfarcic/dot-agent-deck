//! Re-export shim (PRD #176 M1.1).
//!
//! The hook-event and orchestration-signal wire types — `AgentEvent`,
//! `AgentType`, `EventType`, `BroadcastMsg`, `DaemonMessage`, `DelegateSignal`,
//! `WorkDoneSignal`, and `DISPLAY_NAME_METADATA_KEY` — now live in the shared
//! `protocol` crate so the TUI, CLI, tests, and the desktop GUI core all depend
//! on one definition. They're re-exported here so the binary's existing
//! `crate::event::…` call sites compile unchanged. The round-trip tests moved
//! with the types into `protocol::event`.

pub use protocol::event::{
    AgentEvent, AgentType, BroadcastMsg, DISPLAY_NAME_METADATA_KEY, DaemonMessage, DelegateSignal,
    EventType, WorkDoneSignal,
};
