//! PRD #1859: the pure-data core of `dot-agent-deck-collector`, the
//! per-machine process that reports each agent run's CPU and memory, and each
//! worktree's disk use, as OTLP metrics that join the deck's run traces on
//! `deck.run.id`.
//!
//! The collector runs outside the deck: nothing here is called by the daemon,
//! the spawn path or a client. It groups processes by the `deck.run.id` they
//! inherit in `OTEL_RESOURCE_ATTRIBUTES` ([`identity`]), falling back to their
//! parent's run, and credits CPU per process so that a child's time is counted
//! once whether it is seen alive or only through its parent's reaped-children
//! time ([`aggregate`]).
//!
//! This module holds what can be tested without a live `/proc`: the parsers
//! ([`procfs`]), the attribution and accounting state machine ([`aggregate`]),
//! worktree identity and the disk walk ([`disk`]), the configuration
//! ([`config`]) and the OTLP/JSON encoding ([`otlp`]). Reading `/proc`,
//! scheduling the walks and exporting belong to the binary
//! (`src/bin/dot-agent-deck-collector.rs`).

pub mod aggregate;
pub mod config;
pub mod disk;
pub mod identity;
pub mod otlp;
pub mod procfs;

/// The instrumentation scope, and the default `service.name`.
pub const SCOPE_NAME: &str = "dot-agent-deck-collector";

/// `deck.collector.schema_version`: changes when the set or meaning of the
/// collector's metrics or attributes changes.
pub const SCHEMA_VERSION: &str = "1";

/// Resource and data point attribute keys the collector writes.
pub mod keys {
    pub const SCHEMA_VERSION: &str = "deck.collector.schema_version";
    pub const ATTRIBUTION: &str = "deck.attribution";
    pub const RUN_ID: &str = "deck.run.id";
    pub const WORKTREE_ID: &str = "deck.worktree.id";
    pub const WORKTREE_PATH: &str = "deck.worktree.path";
    pub const MEMORY_KIND: &str = "deck.memory.kind";
}

/// Metric names.
pub mod metrics {
    pub const CPU_TIME: &str = "deck.run.cpu.time";
    pub const MEMORY_PSS: &str = "deck.run.memory.pss";
    pub const PROCESSES: &str = "deck.run.processes";
    pub const DISK_USAGE: &str = "deck.worktree.disk.usage";
}
