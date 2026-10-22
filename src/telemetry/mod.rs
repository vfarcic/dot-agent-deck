//! PRD #1741: export what the deck knows about every agent run as
//! OpenTelemetry traces, and hand agents it starts the configuration and the
//! identifying attributes that join their own telemetry to the deck's run.
//!
//! **Off unless configured.** Everything starts from [`config::TelemetryConfig`],
//! which is `None` unless the daemon's environment names an OTLP endpoint (the
//! standard `OTEL_*` variables). With it `None` nothing here spawns a task,
//! opens a socket, or adds a single variable to a spawned agent's environment.
//!
//! **Metadata only, by construction.** Every attribute the deck puts on a span
//! or a span event is keyed by an [`AttrKey`], a closed enum, so the set of keys
//! the exporter can emit is the allow-list itself rather than a filter applied
//! to something wider. The values are identifiers, enum names, counts and
//! hashes. Prompts, output, tool details, titles, working directories, status
//! details, environment values and header values never enter a span. The
//! tracker matches an event to its run by pane and agent id and applies it to
//! that run's own copy of the card state ([`crate::state::AppState`]), which
//! does hold content; what it reads back is typed: the card's status, mapped to
//! one of a fixed set of names, and the number of the card's pull request.
//! No other value on a span is read from an event: the rest come from the
//! spawn (its ids, role and orchestration), the deck's own counts and enum
//! names, the configuration and host, and the git probes (a commit hash and the
//! repository's name). A test feeds
//! sentinel strings through every content-bearing field to prove they do not
//! come out the other end (`tests` in [`otlp`]), and another sends them through
//! the whole exporter over both protocols (`tests` in [`exporter`]).
//!
//! **Hand-rolled OTLP, deliberately.** The payload is the small subset of
//! `opentelemetry.proto.collector.trace.v1.ExportTraceServiceRequest` the deck
//! fills, encoded as protobuf ([`otlp::encode_protobuf`]) or OTLP/JSON
//! ([`otlp::encode_json`]) and posted with the `reqwest` client the crate
//! already depends on. The `opentelemetry` SDK crates would add `prost`,
//! `opentelemetry-proto` and the SDK runtime to a build that has none of them,
//! for a producer that emits two span names.
//!
//! The module is self-contained: the daemon and the agent spawn path each reach
//! it through one call per lifecycle point — [`start_for_daemon`],
//! [`prepare_spawn`] and [`commit_spawn`], [`agent_exited`] and
//! [`observe_pane_input`] — so what telemetry does at each of them is decided
//! here, not at the call site.

pub mod config;
pub mod exporter;
pub mod input;
pub mod otlp;
pub mod propagation;
pub mod resource;
pub mod run_context;
pub mod span;
pub mod tracker;

pub use config::{Protocol, TelemetryConfig};
pub use exporter::{PreparedRun, Telemetry};
pub use tracker::{EndReason, OrchestrationInfo, RunStart, RunTracker};

use std::sync::Arc;

/// Daemon start: read the configuration from this process's environment,
/// subscribe the tracker to `event_tx`, and hand `registry` the handle so the
/// agents it spawns get runs. With telemetry off the returned handle does
/// nothing, nothing is subscribed, and `registry` is left alone.
pub fn start_for_daemon(
    event_tx: &tokio::sync::broadcast::Sender<crate::event::BroadcastMsg>,
    registry: &crate::agent_pty::AgentPtyRegistry,
) -> Arc<Telemetry> {
    let telemetry = Arc::new(Telemetry::start(&config::process_env()));
    telemetry.subscribe(event_tx.subscribe());
    if telemetry.is_enabled() {
        registry.set_telemetry(Arc::clone(&telemetry));
    }
    telemetry
}

/// The spawn path, before the agent process exists: prepare this generation's
/// run and add its identity to the agent's environment. `None` when telemetry
/// is off or the pane has no id; drop the result to abandon the run when the
/// spawn is refused, and pass it to [`commit_spawn`] once the agent started.
pub fn prepare_spawn(
    telemetry: Option<Arc<Telemetry>>,
    pane_id: Option<&str>,
    agent_id: &str,
    opts: &mut crate::agent_pty::SpawnOptions<'_>,
    membership: Option<&crate::agent_pty::TabMembership>,
) -> Option<PreparedRun> {
    let telemetry = telemetry?;
    let start = RunStart::for_spawn(
        pane_id?,
        agent_id,
        opts.agent_type.as_ref(),
        opts.command,
        membership,
    );
    let run = telemetry.prepare_run(start, &opts.env, opts.cwd.map(std::path::PathBuf::from))?;
    opts.env.extend(run.env.iter().cloned());
    Some(run)
}

/// The spawn path, once the agent is published: open the run
/// [`prepare_spawn`] prepared.
pub fn commit_spawn(run: Option<PreparedRun>) {
    if let Some(run) = run {
        run.commit();
    }
}

/// The agent `agent_id` on `pane_id` reached the end of its PTY: end its run,
/// as `daemon_shutdown` while the daemon is shutting down, as `exited` when
/// the agent is still registered (nothing removed it, so it went by itself),
/// and as `closed` otherwise. `still_registered` is asked only when telemetry
/// is on.
pub fn agent_exited(
    telemetry: Option<Arc<Telemetry>>,
    pane_id: Option<&str>,
    agent_id: &str,
    shutting_down: bool,
    still_registered: impl FnOnce() -> bool,
) {
    let (Some(telemetry), Some(pane_id)) = (telemetry, pane_id) else {
        return;
    };
    telemetry.end_run(
        pane_id,
        Some(agent_id),
        end_reason(shutting_down, still_registered),
        None,
    );
}

fn end_reason(shutting_down: bool, still_registered: impl FnOnce() -> bool) -> EndReason {
    if shutting_down {
        EndReason::DaemonShutdown
    } else if still_registered() {
        EndReason::Exited
    } else {
        EndReason::Closed
    }
}

/// The pane writer recorded `bytes` the PTY accepted for `pane_id`. `human` is
/// whether they came from a person at a client — typed, or a voice answer —
/// rather than from the deck itself; only those can be an intervention.
pub fn observe_pane_input(
    telemetry: Option<Arc<Telemetry>>,
    human: bool,
    pane_id: &str,
    bytes: &[u8],
) {
    if let Some(telemetry) = telemetry.filter(|_| human) {
        telemetry.observe_client_input(pane_id, bytes);
    }
}

/// `deck.telemetry.schema_version`: bumped whenever the attribute set changes
/// in a way that makes data recorded before and after it incomparable.
pub const SCHEMA_VERSION: &str = "1";

/// The service name a span batch carries when `OTEL_SERVICE_NAME` is unset.
pub const DEFAULT_SERVICE_NAME: &str = "dot-agent-deck";

/// The two span names the deck emits.
pub const SPAN_AGENT_RUN: &str = "agent.run";
pub const SPAN_AGENT_STATUS: &str = "agent.status";

/// The span event names the deck emits on `agent.run`.
pub const EVENT_STATUS_CHANGE: &str = "deck.status_change";
pub const EVENT_INTERVENTION: &str = "deck.intervention";
pub const EVENT_DELEGATION: &str = "deck.delegation";

/// Every attribute key the deck can put on a span, a span event, or the
/// resource it builds itself. The closed set IS the allow-list: an attribute
/// cannot be constructed with a key outside it ([`span::Attribute`]).
///
/// The user's own `OTEL_RESOURCE_ATTRIBUTES` pairs are the one exception, and
/// only on the resource: they are configuration the user wrote for exactly this
/// purpose, passed through as written ([`resource::deck_resource`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttrKey {
    ServiceName,
    ServiceVersion,
    HostName,
    SchemaVersion,
    RunId,
    PaneId,
    Agent,
    Model,
    Role,
    OrchestrationName,
    OrchestrationId,
    Orchestrator,
    InstructionsCommit,
    RepositoryName,
    InvokingSkill,
    IssueNumber,
    PrNumber,
    EndReason,
    ExitCode,
    InterventionsCount,
    StatusChangesCount,
    Status,
    StatusFrom,
    StatusTo,
    InterventionKind,
    DelegationToPane,
    DelegationFromPane,
}

impl AttrKey {
    /// Every key, for the allow-list test and the docs.
    pub const ALL: &'static [AttrKey] = &[
        AttrKey::ServiceName,
        AttrKey::ServiceVersion,
        AttrKey::HostName,
        AttrKey::SchemaVersion,
        AttrKey::RunId,
        AttrKey::PaneId,
        AttrKey::Agent,
        AttrKey::Model,
        AttrKey::Role,
        AttrKey::OrchestrationName,
        AttrKey::OrchestrationId,
        AttrKey::Orchestrator,
        AttrKey::InstructionsCommit,
        AttrKey::RepositoryName,
        AttrKey::InvokingSkill,
        AttrKey::IssueNumber,
        AttrKey::PrNumber,
        AttrKey::EndReason,
        AttrKey::ExitCode,
        AttrKey::InterventionsCount,
        AttrKey::StatusChangesCount,
        AttrKey::Status,
        AttrKey::StatusFrom,
        AttrKey::StatusTo,
        AttrKey::InterventionKind,
        AttrKey::DelegationToPane,
        AttrKey::DelegationFromPane,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            AttrKey::ServiceName => "service.name",
            AttrKey::ServiceVersion => "service.version",
            AttrKey::HostName => "host.name",
            AttrKey::SchemaVersion => "deck.telemetry.schema_version",
            AttrKey::RunId => "deck.run.id",
            AttrKey::PaneId => "deck.pane.id",
            AttrKey::Agent => "deck.agent",
            AttrKey::Model => "deck.model",
            AttrKey::Role => "deck.role",
            AttrKey::OrchestrationName => "deck.orchestration.name",
            AttrKey::OrchestrationId => "deck.orchestration.id",
            AttrKey::Orchestrator => "deck.orchestrator",
            AttrKey::InstructionsCommit => "deck.instructions.commit",
            AttrKey::RepositoryName => "vcs.repository.name",
            AttrKey::InvokingSkill => "deck.invoking_skill",
            AttrKey::IssueNumber => "deck.issue.number",
            AttrKey::PrNumber => "deck.pr.number",
            AttrKey::EndReason => "deck.run.end_reason",
            AttrKey::ExitCode => "deck.run.exit_code",
            AttrKey::InterventionsCount => "deck.interventions.count",
            AttrKey::StatusChangesCount => "deck.status_changes.count",
            AttrKey::Status => "deck.status",
            AttrKey::StatusFrom => "deck.status.from",
            AttrKey::StatusTo => "deck.status.to",
            AttrKey::InterventionKind => "deck.intervention.kind",
            AttrKey::DelegationToPane => "deck.delegation.to_pane.id",
            AttrKey::DelegationFromPane => "deck.delegation.from_pane.id",
        }
    }

    /// Whether `key` is one the deck may emit.
    pub fn is_allowed(key: &str) -> bool {
        AttrKey::ALL.iter().any(|k| k.as_str() == key)
    }
}

/// The current wall-clock time as nanoseconds since the Unix epoch, the unit
/// every OTLP timestamp uses. A clock before the epoch reads as `0`.
pub fn now_unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_end_reason_follows_shutdown_then_registration() {
        assert_eq!(
            end_reason(true, || panic!("not asked during shutdown")),
            EndReason::DaemonShutdown
        );
        assert_eq!(end_reason(false, || true), EndReason::Exited);
        assert_eq!(end_reason(false, || false), EndReason::Closed);
    }

    #[test]
    fn the_seams_do_nothing_when_telemetry_is_off() {
        agent_exited(None, Some("p1"), "a1", false, || {
            panic!("registration is not asked with telemetry off")
        });
        observe_pane_input(None, true, "p1", b"x");
        commit_spawn(None);
    }

    #[test]
    fn every_key_is_listed_once_and_named_uniquely() {
        let names: std::collections::HashSet<_> = AttrKey::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(names.len(), AttrKey::ALL.len());
        for key in AttrKey::ALL {
            assert!(AttrKey::is_allowed(key.as_str()));
        }
        assert!(!AttrKey::is_allowed("deck.cwd"));
    }
}
