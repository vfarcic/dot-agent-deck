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
//! one of a fixed set of names, and the number and CI result of the card's pull
//! request. One value is read from an event's text, and only through a filter:
//! a shell command's program and first word ([`operation::classify`]),
//! each kept only when it has the shape of a command word, and never the first
//! word of a program that prints or searches text, and the same two of each
//! later command of a chained line and of the command a known wrapper runs
//! ([`operation::describe`]). No other value on a span is
//! read from an event: the rest come from the
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
//! for a producer that emits four span names.
//!
//! The module is self-contained: the daemon and the agent spawn path each reach
//! it through one call per lifecycle point — [`start_for_daemon`] and
//! [`teardown_began`], [`prepare_spawn`] and [`commit_spawn`], [`agent_exited`] and
//! [`observe_pane_input`] — so what telemetry does at each of them is decided
//! here, not at the call site. The deck's entrypoint has one more: the `PATH`
//! shim that switches on Codex's and Devin's own telemetry runs there
//! ([`shim::run_if_invoked`]).

/// PRD #1858: what each run consumed and how it ended, read from each
/// agent's own usage record and exported on the run's span. The record's
/// work-item and roll-up halves (PRD #633's prototype) are for #1860 and are
/// exercised by its tests only, hence the `dead_code` allowance outside them.
#[cfg_attr(not(test), allow(dead_code))]
pub mod accounting;
pub mod ci;
/// PRD #1859: the core of `dot-agent-deck-collector`, a separate binary
/// (`src/bin/dot-agent-deck-collector.rs`). Nothing in the deck calls it.
#[cfg(unix)]
pub mod collector;
pub mod config;
pub mod exporter;
pub mod input;
pub mod launch;
pub mod operation;
pub mod otlp;
pub mod propagation;
pub mod resource;
pub mod run_context;
pub mod shim;
pub mod span;
pub mod tracker;

pub use config::{Protocol, TelemetryConfig};
pub use exporter::{DispatchNaming, PreparedRun, Telemetry};
pub use tracker::{EndReason, OrchestrationInfo, RunStart, RunTracker};

use std::sync::Arc;

/// Daemon start: clear the per-run agent configuration files a daemon that
/// ended without its teardown (a crash, a SIGKILL) left behind, read the
/// configuration from this process's environment, (re)write the `PATH` shim
/// ([`shim`]) running this binary, subscribe the tracker to `event_tx`, and
/// hand `registry` the handle so the agents it spawns get runs. With telemetry
/// off the returned handle does nothing, nothing is subscribed, no shim is
/// written, and `registry` is left alone.
///
/// Installs no signal handler: when the daemon's teardown begins is told by
/// the daemon itself ([`teardown_began`]).
pub fn start_for_daemon(
    event_tx: &tokio::sync::broadcast::Sender<crate::event::BroadcastMsg>,
    registry: &Arc<crate::agent_pty::AgentPtyRegistry>,
) -> Arc<Telemetry> {
    let telemetry = start_from(
        &config::process_env(),
        exporter::default_config_dir(),
        crate::platform::paths::executable_path().as_deref(),
        event_tx,
        registry,
    );
    let registry = Arc::downgrade(registry);
    telemetry.set_dispatch_lookup(Box::new(move |run| {
        dispatch_name(&*registry.upgrade()?, run)
    }));
    telemetry
}

/// `deck.dispatch.name` for the open run `run`: the name, exactly as it was
/// dispatched, of the live unit the deck's record of dispatched units says
/// its generation belongs to
/// ([`crate::dispatched_units::DispatchedUnits::unit_of_generation`]). Never
/// read from the pane id or the branch, which hold a shortened slug of it.
fn dispatch_name(
    registry: &crate::agent_pty::AgentPtyRegistry,
    run: &tracker::RunGeneration,
) -> Option<String> {
    registry
        .dispatched_units()
        .unit_of_generation(
            Some(&run.pane_id),
            run.agent_id.as_deref()?,
            run.orchestration_id.as_deref(),
        )
        .map(|unit| unit.name.clone())
}

/// The dispatch path, before it spawns the unit `name` in `worktree`: name
/// the runs the spawn prepares there from their start
/// ([`Telemetry::begin_dispatch`]). Drop the result once the spawn is over,
/// passing it to [`dispatch_spawned`] first when the spawn succeeded.
pub fn begin_dispatch(
    registry: &crate::agent_pty::AgentPtyRegistry,
    name: &str,
    worktree: &std::path::Path,
) -> Option<DispatchNaming> {
    registry.telemetry()?.begin_dispatch(name, worktree)
}

/// The dispatch path, once the spawn [`begin_dispatch`] named runs for
/// started the unit as `handle`: name the runs of its generations and of its
/// orchestration instance.
pub fn dispatch_spawned(naming: Option<DispatchNaming>, handle: &crate::spawn::SpawnHandle) {
    let Some(naming) = naming else {
        return;
    };
    let agents: Vec<(String, String)> = handle
        .agents
        .iter()
        .map(|agent| (agent.pane_id.clone(), agent.id.clone()))
        .collect();
    naming.spawned(&agents, handle.orchestration_id.as_deref());
}

/// `config_dir` holds this daemon's per-run agent configuration files and its
/// shim directory; `deck` is the binary the shim runs, and no shim is written
/// without one. The leftovers are removed whether or not telemetry is on now:
/// no run of this daemon exists yet to own one, and the daemon before it may
/// have exported.
fn start_from(
    env: &config::EnvMap,
    config_dir: std::path::PathBuf,
    deck: Option<&str>,
    event_tx: &tokio::sync::broadcast::Sender<crate::event::BroadcastMsg>,
    registry: &crate::agent_pty::AgentPtyRegistry,
) -> Arc<Telemetry> {
    launch::remove_leftover_config_files(&config_dir);
    exporter::remove_leftover_usage_files(&config_dir);
    let mut telemetry = Telemetry::start_in(env, config_dir.clone());
    #[cfg(unix)]
    if telemetry.is_enabled()
        && let Some(deck) = deck
    {
        match shim::install(&config_dir, deck) {
            Ok(dir) => telemetry.set_shim_dir(dir),
            Err(e) => tracing::warn!(
                "telemetry: the PATH shim could not be written ({e}); Codex and Devin will not \
                 switch on their own telemetry"
            ),
        }
    }
    #[cfg(not(unix))]
    let _ = deck;
    let telemetry = Arc::new(telemetry);
    telemetry.subscribe(event_tx.subscribe());
    if telemetry.is_enabled() {
        registry.set_telemetry(Arc::clone(&telemetry));
    }
    telemetry
}

/// The teardowns no stop client is timing, by the cause the daemon names them
/// with: a restart, whose client waits for the old daemon's endpoint and the
/// successor's answer for far longer than the final flush's own budget, and a
/// lost attach socket, which leaves no client able to wait at all.
const UNTIMED_TEARDOWNS: [&str; 2] = ["restart-daemon", "attach-lost"];

/// The daemon began a teardown it accepted, for `cause`: called where it names
/// what that teardown stops ([`crate::daemon_stop::log_teardown_inventory`]),
/// ahead of the agents' drain, on each path that drains them — a termination
/// signal, `StopDaemon`, `KIND_SHUTDOWN`, a restart, a lost attach socket. A
/// stop the daemon does not act on, such as a signal after it committed to a
/// restart, does not reach it. Where a stop client is timing the daemon's exit
/// its window opened about now, and the final flush is sized to what is left
/// of it ([`Telemetry::teardown_began`]); after one of the
/// `UNTIMED_TEARDOWNS` the flush keeps its own budget.
pub fn teardown_began(telemetry: Option<Arc<Telemetry>>, cause: &str) {
    if let Some(telemetry) = telemetry
        && !UNTIMED_TEARDOWNS.contains(&cause)
    {
        telemetry.teardown_began(std::time::Instant::now());
    }
}

/// The spawn path, before the agent process exists: prepare this generation's
/// run and add its identity to the agent's environment. `None` when telemetry
/// is off or the pane has no id; drop the result to abandon the run when the
/// spawn is refused, and pass it to [`commit_spawn`] once the agent started.
///
/// With no run prepared, [`shim::RUN_ID_ENV`] is set blank unless the spawn
/// sets it itself, which every reader takes as unset: a daemon started inside
/// another deck's agent would otherwise pass that agent's run id on next to
/// this deck's own pane id, and a tool's spans would join the other run
/// (#1902). That holds with telemetry off too, where the work does run under
/// the other deck's agent: the agent's `DOT_AGENT_DECK_PANE_ID` is this deck's
/// either way, so the pair would name a run and a pane that never belonged
/// together.
pub fn prepare_spawn(
    telemetry: Option<Arc<Telemetry>>,
    pane_id: Option<&str>,
    agent_id: &str,
    opts: &mut crate::agent_pty::SpawnOptions<'_>,
    membership: Option<&crate::agent_pty::TabMembership>,
) -> Option<PreparedRun> {
    let run = prepare_run_for_spawn(telemetry, pane_id, agent_id, opts, membership);
    if run.is_none() && !opts.env.iter().any(|(k, _)| k == shim::RUN_ID_ENV) {
        opts.env.push((shim::RUN_ID_ENV.to_string(), String::new()));
    }
    if run.is_none() {
        // PRD #1858: only a run's own usage file, which a prepared run sets,
        // over one the daemon or the spawn inherited from another run.
        opts.env.push((
            accounting::usage_file::USAGE_FILE_ENV.to_string(),
            String::new(),
        ));
    }
    run
}

fn prepare_run_for_spawn(
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
/// is on, and `exit_status` (the agent's child's, once it has exited) only
/// for an agent that went by itself: its child can close its terminal a
/// moment before it exits, so it is asked again for up to
/// [`EXIT_CODE_WAIT`].
pub fn agent_exited(
    telemetry: Option<Arc<Telemetry>>,
    pane_id: Option<&str>,
    agent_id: &str,
    shutting_down: bool,
    still_registered: impl FnOnce() -> bool,
    exit_status: impl Fn() -> Option<portable_pty::ExitStatus>,
) {
    let (Some(telemetry), Some(pane_id)) = (telemetry, pane_id) else {
        return;
    };
    let reason = end_reason(shutting_down, still_registered);
    let code = (reason == EndReason::Exited)
        .then(|| {
            let deadline = std::time::Instant::now() + EXIT_CODE_WAIT;
            loop {
                if let Some(status) = exit_status() {
                    break exit_code(&status);
                }
                if std::time::Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        })
        .flatten();
    telemetry.end_run(pane_id, Some(agent_id), reason, code);
}

/// The exit code of a child that exited with `status`, as the run carries
/// it; `None` when a signal ended it, which `portable_pty` names only in its
/// `Display`, or for a code past `i32`.
fn exit_code(status: &portable_pty::ExitStatus) -> Option<i32> {
    let signalled = status.to_string().starts_with("Terminated by");
    if signalled {
        None
    } else {
        i32::try_from(status.exit_code()).ok()
    }
}

/// How long [`agent_exited`] waits for an agent's exit code after its
/// terminal closed.
pub const EXIT_CODE_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

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
pub const SCHEMA_VERSION: &str = "3";

/// The service name a span batch carries when `OTEL_SERVICE_NAME` is unset.
pub const DEFAULT_SERVICE_NAME: &str = "dot-agent-deck";

/// The span names the deck emits.
pub const SPAN_AGENT_RUN: &str = "agent.run";
pub const SPAN_AGENT_STATUS: &str = "agent.status";
pub const SPAN_AGENT_OPERATION: &str = "agent.operation";
pub const SPAN_AGENT_CI: &str = "agent.ci";

/// Every span name, for the tests that check what reaches a payload.
#[cfg(test)]
pub const SPAN_NAMES: [&str; 4] = [
    SPAN_AGENT_RUN,
    SPAN_AGENT_STATUS,
    SPAN_AGENT_OPERATION,
    SPAN_AGENT_CI,
];

/// The span event names the deck emits on `agent.run`.
pub const EVENT_STATUS_CHANGE: &str = "deck.status_change";
pub const EVENT_INTERVENTION: &str = "deck.intervention";
pub const EVENT_CI_CHANGE: &str = "deck.ci.change";
pub use accounting::EVENT_MODEL_USAGE;

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
    OperationProgram,
    OperationWord,
    OperationCommands,
    OperationWrappedProgram,
    OperationWrappedWord,
    OperationsCount,
    CiFrom,
    CiTo,
    CiResult,
    CiChecksPassing,
    CiChecksFailing,
    CiChecksSkipped,
    CiRoundsCount,
    CiFailuresCount,
    AgentSource,
    DispatchName,
    /// PRD #1858: a run's usage and termination, and one model's usage on a
    /// `deck.model_usage` event ([`accounting::AccountingKey`]).
    Accounting(accounting::AccountingKey),
}

impl AttrKey {
    /// Every key, for the allow-list test.
    #[cfg(test)]
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
        AttrKey::OperationProgram,
        AttrKey::OperationWord,
        AttrKey::OperationCommands,
        AttrKey::OperationWrappedProgram,
        AttrKey::OperationWrappedWord,
        AttrKey::OperationsCount,
        AttrKey::CiFrom,
        AttrKey::CiTo,
        AttrKey::CiResult,
        AttrKey::CiChecksPassing,
        AttrKey::CiChecksFailing,
        AttrKey::CiChecksSkipped,
        AttrKey::CiRoundsCount,
        AttrKey::CiFailuresCount,
        AttrKey::AgentSource,
        AttrKey::DispatchName,
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
            AttrKey::OperationProgram => "deck.operation.program",
            AttrKey::OperationWord => "deck.operation.word",
            AttrKey::OperationCommands => "deck.operation.commands",
            AttrKey::OperationWrappedProgram => "deck.operation.wrapped.program",
            AttrKey::OperationWrappedWord => "deck.operation.wrapped.word",
            AttrKey::OperationsCount => "deck.operations.count",
            AttrKey::CiFrom => "deck.ci.from",
            AttrKey::CiTo => "deck.ci.to",
            AttrKey::CiResult => "deck.ci.result",
            AttrKey::CiChecksPassing => "deck.ci.checks.passing",
            AttrKey::CiChecksFailing => "deck.ci.checks.failing",
            AttrKey::CiChecksSkipped => "deck.ci.checks.skipped",
            AttrKey::CiRoundsCount => "deck.ci.rounds.count",
            AttrKey::CiFailuresCount => "deck.ci.failures.count",
            AttrKey::AgentSource => "deck.agent.source",
            AttrKey::DispatchName => "deck.dispatch.name",
            AttrKey::Accounting(key) => key.as_str(),
        }
    }

    /// Whether `key` is one the deck may emit.
    #[cfg(test)]
    pub fn is_allowed(key: &str) -> bool {
        AttrKey::ALL.iter().any(|k| k.as_str() == key)
            || accounting::AccountingKey::ALL
                .iter()
                .any(|k| k.as_str() == key)
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

    /// Issue #1981 M6: `deck.dispatch.name` comes from the deck's record of
    /// dispatched units, as the unit was named — a name longer than its pane
    /// id and branch can hold, whose slug differs from it — for the single
    /// unit's own generation and for every role of an orchestration unit, and
    /// for no other generation.
    #[test]
    fn a_runs_dispatch_name_is_the_registrys_exact_name() {
        use crate::dispatched_units::{Dispatcher, NewUnit, UnitKind, slug_of};
        let registry = crate::agent_pty::AgentPtyRegistry::new();
        let single = "Fix: the Very Long unit name — with spaces, punctuation and more than \
                      sixty-four bytes, which no pane id keeps whole";
        let team = "Team UNIT #2";
        assert_ne!(slug_of(single), single);
        assert_ne!(slug_of(team), team);
        for (name, kind) in [
            (
                single,
                UnitKind::Single {
                    pane_id: "sched-dispatch-fix-the-very-long-0".into(),
                    agent_id: "a1".into(),
                },
            ),
            (
                team,
                UnitKind::Orchestration {
                    orchestration_id: "orch-7".into(),
                    name: "tdd".into(),
                    terminal_pane_id: "sched-dispatch-team-unit-2-0".into(),
                    terminal_agent_id: "a2".into(),
                },
            ),
        ] {
            registry.dispatched_units().register(NewUnit {
                name: name.into(),
                worktree: format!("/w/{}", slug_of(name)).into(),
                branch: format!("agent/dispatch-{}", slug_of(name)),
                clone_dir: "/w".into(),
                dispatcher: Dispatcher {
                    pane_id: "p0".into(),
                    agent_id: "a0".into(),
                },
                kind,
                dispatched_at_ms: 1,
            });
        }
        let run = |pane: &str, agent: &str, orchestration: Option<&str>| tracker::RunGeneration {
            run_id: "r".into(),
            pane_id: pane.into(),
            agent_id: Some(agent.into()),
            orchestration_id: orchestration.map(str::to_string),
        };
        assert_eq!(
            dispatch_name(
                &registry,
                &run("sched-dispatch-fix-the-very-long-0", "a1", None)
            )
            .as_deref(),
            Some(single)
        );
        assert_eq!(
            dispatch_name(&registry, &run("any-role-pane", "a9", Some("orch-7"))).as_deref(),
            Some(team),
            "every role of the orchestration"
        );
        assert_eq!(
            dispatch_name(
                &registry,
                &run("sched-dispatch-fix-the-very-long-0", "a-next", None)
            ),
            None,
            "another generation of the pane is not the unit"
        );
        assert_eq!(dispatch_name(&registry, &run("p5", "a5", None)), None);
    }

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
        agent_exited(
            None,
            Some("p1"),
            "a1",
            false,
            || panic!("registration is not asked with telemetry off"),
            || panic!("the exit status is not asked with telemetry off"),
        );
        observe_pane_input(None, true, "p1", b"x");
        commit_spawn(None);
    }

    /// The handler installed for `sig` right now: `SIG_DFL` until something in
    /// this process registers a listener for it.
    #[cfg(unix)]
    fn disposition(sig: libc::c_int) -> libc::sighandler_t {
        // SAFETY: a null `act` only reads the current action into `old`.
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            assert_eq!(libc::sigaction(sig, std::ptr::null(), &mut old), 0);
            old.sa_sigaction
        }
    }

    fn exporting_env() -> config::EnvMap {
        config::EnvMap::from([(
            "OTEL_EXPORTER_OTLP_ENDPOINT".to_string(),
            "http://127.0.0.1:9".to_string(),
        )])
    }

    /// Starting telemetry, exporting, leaves both stop signals' disposition
    /// as it found it: only the daemon's own watch decides what a stop signal
    /// does.
    #[cfg(unix)]
    #[tokio::test]
    async fn starting_for_the_daemon_installs_no_signal_handler() {
        let before = (disposition(libc::SIGTERM), disposition(libc::SIGINT));
        let (event_tx, _) = tokio::sync::broadcast::channel(8);
        let registry = crate::agent_pty::AgentPtyRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let telemetry = start_from(
            &exporting_env(),
            dir.path().to_path_buf(),
            None,
            &event_tx,
            &registry,
        );
        assert!(telemetry.is_exporting());
        assert_eq!(
            (disposition(libc::SIGTERM), disposition(libc::SIGINT)),
            before
        );
    }

    /// PRD #1858: a daemon that ends without reading its runs' usage files —
    /// crashed, or killed — leaves them, and the next daemon to start on the
    /// same state directory removes them before it has a run of its own.
    ///
    /// A plain test with a runtime of its own, entered, rather than an async
    /// one: its file work then runs on the test's thread, outside any task.
    #[cfg(unix)]
    #[test]
    fn a_crashed_daemons_usage_files_go_when_the_next_one_starts() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let (event_tx, _) = tokio::sync::broadcast::channel(8);
        let registry = crate::agent_pty::AgentPtyRegistry::new();
        let crashed = start_from(
            &exporting_env(),
            dir.path().to_path_buf(),
            None,
            &event_tx,
            &registry,
        );
        let mut opts = crate::agent_pty::SpawnOptions {
            agent_type: Some(crate::event::AgentType::Pi),
            ..Default::default()
        };
        let run = prepare_spawn(
            Some(Arc::clone(&crashed)),
            Some("p1"),
            "a1",
            &mut opts,
            None,
        )
        .expect("a prepared run");
        let file = opts
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == accounting::usage_file::USAGE_FILE_ENV)
            .map(|(_, v)| std::path::PathBuf::from(v))
            .unwrap();
        commit_spawn(Some(run));
        assert!(file.is_file());
        // The crash: the daemon never ends the run nor shuts down, and its
        // process ends, which closes the descriptor holding its usage lease.
        let lease = crashed.usage_lease_fd().expect("a usage lease");
        std::mem::forget(crashed);
        // SAFETY: the descriptor belongs to the leaked handle, which never
        // closes it.
        unsafe { libc::close(lease) };
        let registry = crate::agent_pty::AgentPtyRegistry::new();
        let next = start_from(
            &exporting_env(),
            dir.path().to_path_buf(),
            None,
            &event_tx,
            &registry,
        );
        assert!(!file.exists(), "left over after a restart");
        runtime.block_on(next.shutdown(std::time::Duration::from_millis(100)));
    }

    /// A Devin configuration a crashed or SIGKILLed daemon left behind is
    /// removed when the next one starts, exporting or not; nothing else in the
    /// directory is touched, nor, on Unix, a file whose Devin still runs.
    #[tokio::test]
    async fn starting_for_the_daemon_removes_leftover_devin_configs() {
        // A process that has exited; its id may be reused before the sweep,
        // which fails the test rather than passing it wrongly.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        for env in [config::EnvMap::new(), exporting_env()] {
            let dir = tempfile::tempdir().unwrap();
            let leftovers = [
                format!("devin-{}-{dead}.json", "0123456789abcdef".repeat(2)),
                format!("devin-{}-{dead}.json", span::new_run_id()),
            ]
            .map(|n| dir.path().join(n));
            #[cfg(unix)]
            let live = [format!("devin-{}-1.json", span::new_run_id())];
            #[cfg(not(unix))]
            let live: [String; 0] = [];
            let kept = [
                "devin-notes.txt",
                "other.json",
                "devin.json",
                "devin-run-1.json",
                "devin-0123456789ABCDEF0123456789abcdef.json",
                "devin-0123456789abcdef0123456789abcde.json",
                "devin-0123456789abcdef0123456789abcdef.json",
            ]
            .map(|n| dir.path().join(n));
            let kept: Vec<_> = kept
                .into_iter()
                .chain(live.iter().map(|n| dir.path().join(n)))
                .collect();
            for path in leftovers.iter().chain(&kept) {
                std::fs::write(path, "{}").unwrap();
            }
            std::fs::create_dir(dir.path().join("devin-dir.json")).unwrap();
            let (event_tx, _) = tokio::sync::broadcast::channel(8);
            let registry = crate::agent_pty::AgentPtyRegistry::new();
            let telemetry = start_from(&env, dir.path().to_path_buf(), None, &event_tx, &registry);
            for path in &leftovers {
                assert!(!path.exists(), "left over: {}", path.display());
            }
            for path in &kept {
                assert!(path.exists(), "removed: {}", path.display());
            }
            assert!(dir.path().join("devin-dir.json").is_dir());
            telemetry
                .shutdown(std::time::Duration::from_millis(100))
                .await;
        }
        let (event_tx, _) = tokio::sync::broadcast::channel(8);
        let registry = crate::agent_pty::AgentPtyRegistry::new();
        let missing = tempfile::tempdir().unwrap().path().join("absent");
        start_from(&exporting_env(), missing, None, &event_tx, &registry);
    }

    /// The daemon writes the `PATH` shim only when telemetry is on, and an
    /// agent's `PATH` gains it only then: with telemetry off there is no shim
    /// directory and nothing is added to any agent.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_shim_is_written_and_used_only_when_telemetry_is_on() {
        let mut on = exporting_env();
        on.insert("PATH".into(), "/usr/bin".into());
        let off: config::EnvMap = [("PATH".to_string(), "/usr/bin".to_string())].into();
        for (env, enabled) in [(off, false), (on, true)] {
            let dir = tempfile::tempdir().unwrap();
            let (event_tx, _) = tokio::sync::broadcast::channel(8);
            let registry = crate::agent_pty::AgentPtyRegistry::new();
            let telemetry = start_from(
                &env,
                dir.path().to_path_buf(),
                Some("/opt/deck"),
                &event_tx,
                &registry,
            );
            let shim = dir.path().join(shim::DIR_NAME);
            assert_eq!(shim.join("codex").is_file(), enabled);
            assert_eq!(shim.join("devin").is_file(), enabled);
            assert_eq!(shim.exists(), enabled, "no shim directory when off");
            let mut opts = crate::agent_pty::SpawnOptions {
                command: Some("codex"),
                env: crate::test_isolation::pin_unreachable_endpoints(Vec::new()),
                ..Default::default()
            };
            let run = prepare_spawn(registry.telemetry(), Some("p1"), "a1", &mut opts, None);
            assert_eq!(run.is_some(), enabled);
            let path = opts
                .env
                .iter()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| v.clone());
            assert_eq!(
                path,
                enabled.then(|| format!("{}:/usr/bin", shim.display())),
            );
            telemetry
                .shutdown(std::time::Duration::from_millis(100))
                .await;
        }
    }

    /// Issue #1902: a daemon that inherited another deck's run id hands each
    /// agent it starts a run for that run's own id, shim or no shim, and a
    /// blank one to an agent it starts no run for — telemetry off, or no pane
    /// id — unless the spawn sets its own. The last value for a variable in
    /// the spawn's environment is the one the agent gets.
    #[tokio::test]
    async fn an_agent_gets_its_own_run_id_and_never_an_inherited_one() {
        const INHERITED: &str = "0123456789abcdef0123456789abcdef";
        fn run_id(opts: &crate::agent_pty::SpawnOptions<'_>) -> Option<String> {
            opts.env
                .iter()
                .rev()
                .find(|(k, _)| k == shim::RUN_ID_ENV)
                .map(|(_, v)| v.clone())
        }
        let daemon_env = |on: bool| {
            let mut env = if on {
                exporting_env()
            } else {
                config::EnvMap::new()
            };
            env.insert("PATH".into(), "/usr/bin".into());
            env.insert(shim::RUN_ID_ENV.into(), INHERITED.into());
            env
        };
        // Telemetry on: with the shim (Unix only writes one) and without it.
        let decks: &[Option<&str>] = if cfg!(unix) {
            &[Some("/opt/deck"), None]
        } else {
            &[None]
        };
        for &deck in decks {
            let dir = tempfile::tempdir().unwrap();
            let (event_tx, _) = tokio::sync::broadcast::channel(8);
            let registry = crate::agent_pty::AgentPtyRegistry::new();
            let telemetry = start_from(
                &daemon_env(true),
                dir.path().to_path_buf(),
                deck,
                &event_tx,
                &registry,
            );
            assert_eq!(dir.path().join(shim::DIR_NAME).exists(), deck.is_some());
            let mut ids = Vec::new();
            for pane in ["p1", "p2"] {
                let mut opts = crate::agent_pty::SpawnOptions {
                    command: Some("codex"),
                    env: crate::test_isolation::pin_unreachable_endpoints(Vec::new()),
                    ..Default::default()
                };
                let run = prepare_spawn(registry.telemetry(), Some(pane), "a1", &mut opts, None)
                    .unwrap_or_else(|| panic!("no run, shim {deck:?}"));
                let id = run_id(&opts).unwrap();
                assert!(launch::is_run_id(&id), "{id:?}, shim {deck:?}");
                assert_ne!(id, INHERITED, "shim {deck:?}");
                assert!(
                    run.env
                        .iter()
                        .any(|(k, v)| k == shim::RUN_ID_ENV && *v == id),
                    "the run's own id, shim {deck:?}"
                );
                assert_eq!(
                    opts.env.iter().any(|(k, _)| k == "PATH"),
                    deck.is_some(),
                    "PATH changes only for the shim"
                );
                ids.push(id);
            }
            assert_ne!(ids[0], ids[1], "each spawn its own run");
            // A spawn with no pane id gets no run, so no id either.
            let mut opts = crate::agent_pty::SpawnOptions::default();
            assert!(prepare_spawn(registry.telemetry(), None, "a1", &mut opts, None).is_none());
            assert_eq!(run_id(&opts).as_deref(), Some(""));
            telemetry
                .shutdown(std::time::Duration::from_millis(100))
                .await;
        }
        // Telemetry off: no run, and the inherited id is blanked rather than
        // passed through; an id the spawn sets itself is kept as it is.
        let dir = tempfile::tempdir().unwrap();
        let (event_tx, _) = tokio::sync::broadcast::channel(8);
        let registry = crate::agent_pty::AgentPtyRegistry::new();
        let telemetry = start_from(
            &daemon_env(false),
            dir.path().to_path_buf(),
            Some("/opt/deck"),
            &event_tx,
            &registry,
        );
        assert!(registry.telemetry().is_none());
        let mut opts = crate::agent_pty::SpawnOptions::default();
        assert!(prepare_spawn(registry.telemetry(), Some("p1"), "a1", &mut opts, None).is_none());
        // PRD #1858: an inherited usage file is blanked the same way.
        let usage_file = (
            accounting::usage_file::USAGE_FILE_ENV.to_string(),
            String::new(),
        );
        assert_eq!(
            opts.env,
            [
                (shim::RUN_ID_ENV.to_string(), String::new()),
                usage_file.clone()
            ]
        );
        let own = [(shim::RUN_ID_ENV.to_string(), "mine".to_string())];
        let mut opts = crate::agent_pty::SpawnOptions {
            env: own.to_vec(),
            ..Default::default()
        };
        assert!(prepare_spawn(registry.telemetry(), Some("p1"), "a1", &mut opts, None).is_none());
        assert_eq!(opts.env, [own[0].clone(), usage_file]);
        telemetry
            .shutdown(std::time::Duration::from_millis(100))
            .await;
    }

    fn exporting_registry() -> (Arc<Telemetry>, Arc<crate::agent_pty::AgentPtyRegistry>) {
        let (event_tx, _) = tokio::sync::broadcast::channel(8);
        let registry = Arc::new(crate::agent_pty::AgentPtyRegistry::new());
        let dir = tempfile::tempdir().unwrap();
        let telemetry = start_from(
            &exporting_env(),
            dir.path().to_path_buf(),
            None,
            &event_tx,
            &registry,
        );
        (telemetry, registry)
    }

    /// A teardown a stop client is timing — a signal, `StopDaemon`,
    /// `KIND_SHUTDOWN` — marks when it began, through the inventory every
    /// draining path logs first, so the final flush fits that client's
    /// window. A restart and a lost attach socket leave the flush its own
    /// budget: Qodo's case, where a restart's flush was cut short.
    #[tokio::test]
    async fn a_timed_teardown_marks_when_it_began_and_a_restart_does_not() {
        for cause in ["signal", "stop-daemon", "shutdown-frame"] {
            let (telemetry, registry) = exporting_registry();
            let state: crate::state::SharedState = Default::default();
            let before = std::time::Instant::now();
            crate::daemon_stop::log_teardown_inventory(&state, &registry, cause).await;
            let began = telemetry.teardown_began_at().expect(cause);
            assert!(
                began >= before && began <= std::time::Instant::now(),
                "{cause}"
            );
        }
        for cause in UNTIMED_TEARDOWNS {
            let (telemetry, registry) = exporting_registry();
            let state: crate::state::SharedState = Default::default();
            crate::daemon_stop::log_teardown_inventory(&state, &registry, cause).await;
            assert_eq!(telemetry.teardown_began_at(), None, "{cause}");
        }
        teardown_began(None, "signal");
    }

    /// The untimed causes are spelled as the daemon spells them, so a
    /// renamed cause cannot quietly turn a restart back into a timed stop.
    #[test]
    fn the_untimed_teardowns_are_causes_the_daemon_names() {
        let sources = [
            include_str!("../daemon.rs"),
            include_str!("../daemon_protocol.rs"),
            include_str!("../attach_recovery.rs"),
        ];
        let named: Vec<&str> = sources
            .iter()
            .flat_map(|src| src.split("log_teardown_inventory(").skip(1))
            .filter_map(|call| call.split(')').next()?.split('"').nth(1))
            .collect();
        for cause in UNTIMED_TEARDOWNS {
            assert!(named.contains(&cause), "{cause} not among {named:?}");
        }
        for cause in ["signal", "stop-daemon", "shutdown-frame"] {
            assert!(named.contains(&cause), "{cause} not among {named:?}");
        }
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
