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
//! word of a program that prints or searches text. No other value on a span is
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

pub mod ci;
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
pub use exporter::{PreparedRun, Telemetry};
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
    registry: &crate::agent_pty::AgentPtyRegistry,
) -> Arc<Telemetry> {
    start_from(
        &config::process_env(),
        exporter::default_config_dir(),
        crate::platform::paths::executable_path().as_deref(),
        event_tx,
        registry,
    )
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
/// [`UNTIMED_TEARDOWNS`] the flush keeps its own budget.
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

/// The span names the deck emits.
pub const SPAN_AGENT_RUN: &str = "agent.run";
pub const SPAN_AGENT_STATUS: &str = "agent.status";
pub const SPAN_AGENT_OPERATION: &str = "agent.operation";
pub const SPAN_AGENT_CI: &str = "agent.ci";

/// Every span name, for the tests that check what reaches a payload.
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
    OperationOutcome,
    OperationsCount,
    CiFrom,
    CiTo,
    CiResult,
    CiChecksPassing,
    CiChecksFailing,
    CiChecksSkipped,
    CiRoundsCount,
    CiFailuresCount,
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
        AttrKey::OperationProgram,
        AttrKey::OperationWord,
        AttrKey::OperationOutcome,
        AttrKey::OperationsCount,
        AttrKey::CiFrom,
        AttrKey::CiTo,
        AttrKey::CiResult,
        AttrKey::CiChecksPassing,
        AttrKey::CiChecksFailing,
        AttrKey::CiChecksSkipped,
        AttrKey::CiRoundsCount,
        AttrKey::CiFailuresCount,
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
            AttrKey::OperationOutcome => "deck.operation.outcome",
            AttrKey::OperationsCount => "deck.operations.count",
            AttrKey::CiFrom => "deck.ci.from",
            AttrKey::CiTo => "deck.ci.to",
            AttrKey::CiResult => "deck.ci.result",
            AttrKey::CiChecksPassing => "deck.ci.checks.passing",
            AttrKey::CiChecksFailing => "deck.ci.checks.failing",
            AttrKey::CiChecksSkipped => "deck.ci.checks.skipped",
            AttrKey::CiRoundsCount => "deck.ci.rounds.count",
            AttrKey::CiFailuresCount => "deck.ci.failures.count",
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

    /// A Devin configuration a crashed or SIGKILLed daemon left behind is
    /// removed when the next one starts, exporting or not; nothing else in the
    /// directory is touched.
    #[tokio::test]
    async fn starting_for_the_daemon_removes_leftover_devin_configs() {
        for env in [config::EnvMap::new(), exporting_env()] {
            let dir = tempfile::tempdir().unwrap();
            let leftovers = [
                format!("devin-{}.json", "0123456789abcdef".repeat(2)),
                format!("devin-{}.json", span::new_run_id()),
            ]
            .map(|n| dir.path().join(n));
            let kept = [
                "devin-notes.txt",
                "other.json",
                "devin.json",
                "devin-run-1.json",
                "devin-0123456789ABCDEF0123456789abcdef.json",
                "devin-0123456789abcdef0123456789abcde.json",
            ]
            .map(|n| dir.path().join(n));
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
