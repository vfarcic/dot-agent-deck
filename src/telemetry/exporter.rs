//! The daemon-facing half: [`Telemetry`] owns the run tracker, the task that
//! feeds it in order, and a background task that batches finished spans and
//! posts them over OTLP/HTTP.
//!
//! With telemetry off, [`Telemetry::start`] returns a handle that does nothing:
//! no task, no client, no environment for agents. With a `grpc` (or unknown)
//! protocol it warns once and only propagates. Export is asynchronous and
//! batched; a failure is logged at most once a minute, by status code or error
//! kind, never with a header or the endpoint's credentials, and never reaches
//! an agent, a pane or the daemon.
//!
//! **One order for everything a run sees.** The daemon's broadcast is read by
//! an async task, while agents start on the spawn path and end on their PTY
//! reader threads. So that a status the agent reported is never lost to that
//! race, the lifecycle reaches the tracker the same way the events do:
//!
//! - a run is *prepared* in the tracker before its agent starts, so an event
//!   the agent causes always finds it ([`super::tracker::RunTracker::prepare_run`]);
//! - committing, ending and human input are commands on one queue, read by the
//!   same task that reads the broadcast, and the task applies every event
//!   already broadcast before it applies a command — so an agent's last status,
//!   broadcast before its PTY closed, is applied before its run ends;
//! - human input waits with the run it was typed into, not only on the queue:
//!   the command is a reminder to count it, and a run ended first — by its
//!   pane's close broadcast, say — counts it as it ends;
//! - a run whose spawn is refused is forgotten when its [`PreparedRun`] is
//!   dropped uncommitted.
//!
//! **Delivery.** A failed send is retried with exponential backoff and jitter
//! when the failure is transient (a connection error, HTTP 429, 502, 503 or
//! 504), honouring `Retry-After`, until the export timeout or, at shutdown, the
//! remaining flush budget runs out. Redirects are not followed: a 3xx answer is
//! a failed export, so a header meant for the configured endpoint never goes
//! anywhere else.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::Instant;

use crate::event::BroadcastMsg;

use super::config::{EnvMap, TelemetryConfig, redacted_endpoint};
use super::input::{InputFilter, LONE_ESCAPE_WINDOW};
use super::propagation::{RunIdentity, agent_env};
use super::resource::{ResourceAttribute, deck_resource, host_name};
use super::run_context::{ProbeSettings, Probes};
use super::span::{SpanRecord, new_run_id};
use super::tracker::{EndReason, RunStart, RunTracker};
use super::{Protocol, now_unix_nanos, otlp};

/// Spans per request, at most.
pub const MAX_BATCH: usize = 512;
/// How long a finished span waits for company before it is sent anyway.
pub const BATCH_DELAY: Duration = Duration::from_secs(5);
/// Finished spans waiting for the exporter, at most. Past it new spans are
/// dropped and counted, so a backend that is down cannot grow the daemon.
pub const QUEUE_CAPACITY: usize = 4096;
/// Human input waiting for the tracker, per run, at most. Past it the input is
/// dropped: the run's intervention count is all it feeds.
pub const MAX_QUEUED_INPUT: usize = 1024;
/// Export failures are logged at most this often.
pub const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(60);
/// The first wait before a retry; each further one doubles, up to
/// [`RETRY_MAX_BACKOFF`], and is jittered down by up to half.
pub const RETRY_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
pub const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(5);
/// How much of the shutdown budget cancelling the run-context probes may use.
const PROBE_SHUTDOWN_BUDGET: Duration = Duration::from_millis(500);

enum ExportMsg {
    Span(Box<SpanRecord>),
    /// Send everything held, by `deadline` at the latest, then acknowledge.
    Flush {
        ack: oneshot::Sender<()>,
        deadline: Instant,
    },
}

/// What the ingest task applies, in the order it was sent — see the module
/// docs.
enum Command {
    Subscribe(broadcast::Receiver<BroadcastMsg>),
    Commit {
        run_id: String,
    },
    End {
        pane_id: String,
        agent_id: Option<String>,
        /// The run whose input filter [`Telemetry::end_run`] marked ended.
        run_id: Option<String>,
        reason: EndReason,
        exit_code: Option<i32>,
        now: u64,
    },
    /// `run_id` on `pane_id` has human input waiting ([`PaneInput::pending`]).
    HumanInput {
        pane_id: String,
        run_id: String,
    },
    /// Acknowledged once everything sent before it, broadcast included, is
    /// applied.
    Barrier(oneshot::Sender<()>),
    Shutdown {
        now: u64,
        ack: oneshot::Sender<()>,
    },
}

/// A run's input filter, from its commit until the tracker ends the run,
/// with the generation it is for.
struct PaneInput {
    run_id: String,
    agent_id: Option<String>,
    /// [`Telemetry::end_run`] was called for it: later input is not its.
    ended: bool,
    filter: InputFilter,
    /// When the lone `ESC` the filter holds arrived — monotonic, for the
    /// window, and wall-clock, for the intervention it may become.
    escape: Option<(std::time::Instant, u64)>,
    /// When the human input the filter accepted and the tracker has not
    /// counted yet was written, oldest first. Taken, once, by whichever comes
    /// first: the [`Command::HumanInput`] sent with the first of them, or the
    /// run ending.
    pending: Vec<u64>,
}

impl PaneInput {
    /// Everything this run's input still owes the tracker, oldest first:
    /// what is pending, then a held lone `ESC` when `settle` says it is the
    /// Escape key now.
    fn take_held(&mut self, settle: bool) -> Vec<u64> {
        let mut held = std::mem::take(&mut self.pending);
        if settle
            && let Some((_, written)) = self.escape.take()
            && self.filter.settle_lone_escape()
        {
            held.push(written);
        }
        held
    }
}

type Filters = Mutex<HashMap<String, Vec<PaneInput>>>;

fn lock_filters(filters: &Filters) -> std::sync::MutexGuard<'_, HashMap<String, Vec<PaneInput>>> {
    filters.lock().unwrap_or_else(|p| p.into_inner())
}

/// The run `run_id` on `pane_id` has ended: drop its input filter, and return
/// when the input it accepted and the tracker has not counted yet was written,
/// with a lone `ESC` it still held last — the Escape key, since nothing can
/// follow it now. Called by the tracker, with the tracker locked.
fn retire_input(filters: &Filters, pane_id: &str, run_id: &str) -> Vec<u64> {
    let mut filters = lock_filters(filters);
    let Some(inputs) = filters.get_mut(pane_id) else {
        return Vec::new();
    };
    let Some(i) = inputs.iter().position(|input| input.run_id == run_id) else {
        return Vec::new();
    };
    let mut input = inputs.remove(i);
    if inputs.is_empty() {
        filters.remove(pane_id);
    }
    input.take_held(true)
}

/// What the spawn path, the reader threads and the ingest task share.
struct Shared {
    tracker: Mutex<RunTracker>,
    queue: mpsc::Sender<ExportMsg>,
    dropped: Mutex<RateLimitedLog>,
    /// Each run's input filter, by pane, the newest generation last. The
    /// tracker retires one as it ends that run ([`retire_input`]), so a held
    /// lone `ESC` is counted however the run ends. Locked after the tracker,
    /// never before it.
    filters: Arc<Filters>,
    /// Wakes the ingest task to time a lone `ESC` that a write left held.
    escape_held: tokio::sync::Notify,
    /// Test seam: pauses the ingest task once inside its `escape_held`
    /// branch, after signalling the first half, until the second receives.
    #[cfg(test)]
    timer_gate: Mutex<Option<(oneshot::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

struct Exporting {
    shared: Arc<Shared>,
    commands: mpsc::UnboundedSender<Command>,
    probes: Probes,
    ingest: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// See the module docs. Cheap to share: clone the `Arc` it is held in.
pub struct Telemetry {
    config: Option<TelemetryConfig>,
    /// The daemon's `OTEL_*` variables (and Claude Code's switch), the base an
    /// agent's environment is computed from.
    otel_env: EnvMap,
    host: Option<String>,
    exporting: Option<Exporting>,
}

/// A run minted for a spawn that has not started its agent yet — see
/// [`Telemetry::prepare_run`]. Dropping it without [`Self::commit`] forgets the
/// run: its agent never started.
pub struct PreparedRun {
    /// The variables to add to the agent's environment.
    pub env: Vec<(String, String)>,
    telemetry: Arc<Telemetry>,
    run_id: String,
    pane_id: String,
    agent_id: Option<String>,
    cwd: Option<PathBuf>,
    committed: bool,
}

impl PreparedRun {
    /// The agent started: open its run.
    pub fn commit(mut self) {
        self.committed = true;
        self.telemetry.commit(&self);
    }
}

impl Drop for PreparedRun {
    fn drop(&mut self) {
        if !self.committed
            && let Some(exporting) = &self.telemetry.exporting
        {
            exporting.shared.with_tracker(|t| t.abort_run(&self.run_id));
        }
    }
}

/// How [`Telemetry`] runs, apart from the environment. Tests change it.
#[derive(Default)]
struct Settings {
    probes: ProbeSettings,
    retry: RetryPolicy,
    /// Test seam: hand finished spans here instead of to an HTTP exporter.
    #[cfg(test)]
    sink: Option<mpsc::Sender<ExportMsg>>,
}

impl Telemetry {
    /// A handle that does nothing — what [`Self::start`] returns when off.
    pub fn disabled() -> Self {
        Telemetry {
            config: None,
            otel_env: EnvMap::new(),
            host: None,
            exporting: None,
        }
    }

    /// Read the configuration from `env` (the daemon's environment at start)
    /// and, when it asks for an export the deck can do, spawn the exporter on
    /// the current tokio runtime.
    pub fn start(env: &EnvMap) -> Self {
        Self::start_with(env, Settings::default())
    }

    fn start_with(env: &EnvMap, settings: Settings) -> Self {
        let Some(config) = TelemetryConfig::from_env(env) else {
            return Self::disabled();
        };
        let otel_env: EnvMap = env
            .iter()
            .filter(|(k, _)| k.starts_with("OTEL_") || k.starts_with("CLAUDE_CODE_"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let host = host_name();
        let exporting = if !config.protocol.deck_can_export() {
            tracing::warn!(
                protocol = config.protocol.as_env_value(),
                "telemetry: the deck exports traces over http/protobuf or http/json only; \
                 nothing will be exported by the deck, agents still receive the OTLP configuration"
            );
            None
        } else if tokio::runtime::Handle::try_current().is_err() {
            tracing::warn!(
                "telemetry: no async runtime to export from; agent-run traces will not be \
                 exported, agents still receive the OTLP configuration"
            );
            None
        } else {
            match start_exporting(&config, host.as_deref(), settings) {
                Ok(exporting) => {
                    tracing::info!(
                        endpoint = %redacted_endpoint(&config.endpoint),
                        protocol = config.protocol.as_env_value(),
                        "telemetry: exporting agent-run traces over OTLP"
                    );
                    Some(exporting)
                }
                Err(e) => {
                    tracing::warn!(
                        "telemetry: the OTLP client could not be built ({e}); agent-run traces \
                         will not be exported, agents still receive the OTLP configuration"
                    );
                    None
                }
            }
        };
        Telemetry {
            config: Some(config),
            otel_env,
            host,
            exporting,
        }
    }

    /// Whether telemetry is configured at all (export or propagation).
    pub fn is_enabled(&self) -> bool {
        self.config.is_some()
    }

    /// Whether the deck exports its own spans.
    pub fn is_exporting(&self) -> bool {
        self.exporting.is_some()
    }

    /// An agent is about to start: mint its run, prepare it in the tracker, and
    /// compute the variables to add to its environment. Call it before the
    /// agent process exists and [`PreparedRun::commit`] once it does; a spawn
    /// refused in between drops the [`PreparedRun`], which forgets the run.
    /// `spawn_env` is the spawn's own overlay and is applied over the daemon's
    /// environment captured by [`Self::start`]: a value set in either counts as
    /// the user's. `cwd` is used only to resolve the run's git context, off
    /// this thread. `None` when telemetry is off.
    pub fn prepare_run(
        self: &Arc<Self>,
        mut start: RunStart,
        spawn_env: &[(String, String)],
        cwd: Option<PathBuf>,
    ) -> Option<PreparedRun> {
        let config = self.config.as_ref()?;
        if start.run_id.is_empty() {
            start.run_id = new_run_id();
        }
        let mut base = self.otel_env.clone();
        base.extend(spawn_env.iter().cloned());
        let env = agent_env(
            Some(config),
            &base,
            &RunIdentity {
                run_id: &start.run_id,
                pane_id: &start.pane_id,
                agent: &start.agent,
                orchestration: start.orchestration.as_ref(),
            },
            self.host.as_deref(),
        );
        let prepared = PreparedRun {
            env,
            telemetry: Arc::clone(self),
            run_id: start.run_id.clone(),
            pane_id: start.pane_id.clone(),
            agent_id: start.agent_id.clone(),
            cwd,
            committed: false,
        };
        if let Some(exporting) = &self.exporting {
            exporting
                .shared
                .with_tracker(|t| t.prepare_run(start, now_unix_nanos()));
        }
        Some(prepared)
    }

    fn commit(&self, run: &PreparedRun) {
        let Some(exporting) = &self.exporting else {
            return;
        };
        {
            let mut filters = exporting.shared.lock_filters();
            filters
                .entry(run.pane_id.clone())
                .or_default()
                .push(PaneInput {
                    run_id: run.run_id.clone(),
                    agent_id: run.agent_id.clone(),
                    ended: false,
                    filter: InputFilter::new(),
                    escape: None,
                    pending: Vec::new(),
                });
            // Sent before the filters are unlocked, so no notice of input to
            // this run can reach the tracker ahead of the run itself.
            let _ = exporting.commands.send(Command::Commit {
                run_id: run.run_id.clone(),
            });
        }
        // The git probes run off the spawn path; the run's context
        // attributes appear on its span once they have resolved.
        if let Some(cwd) = run.cwd.clone() {
            let shared = Arc::clone(&exporting.shared);
            let run_id = run.run_id.clone();
            exporting.probes.spawn(cwd, move |context| {
                shared.with_tracker(|t| t.set_context(&run_id, context));
            });
        }
    }

    /// An attached client wrote `bytes` into `pane_id`. Counted as a human
    /// intervention unless it is only terminal reports, including a report
    /// split across writes ([`super::input::InputFilter`]). A lone `ESC` — the
    /// Escape key — is counted, at the time it was written, once
    /// [`LONE_ESCAPE_WINDOW`] passes without the rest of a report, or when the
    /// run ends. Input the deck injects itself (a delegated task, an inbox
    /// delivery) must not be passed here.
    pub fn observe_client_input(&self, pane_id: &str, bytes: &[u8]) {
        self.observe_client_input_at(pane_id, bytes, now_unix_nanos(), std::time::Instant::now());
    }

    /// [`Self::observe_client_input`] at a given time: `now` on the wall clock,
    /// `at` on the monotonic one.
    fn observe_client_input_at(
        &self,
        pane_id: &str,
        bytes: &[u8],
        now: u64,
        at: std::time::Instant,
    ) {
        let Some(exporting) = &self.exporting else {
            return;
        };
        let shared = &exporting.shared;
        let mut human = Vec::with_capacity(2);
        let mut held = false;
        {
            let mut filters = shared.lock_filters();
            // No open run on this pane: nothing to count it against.
            let Some(input) = filters
                .get_mut(pane_id)
                .and_then(|inputs| inputs.last_mut())
                .filter(|input| !input.ended)
            else {
                return;
            };
            // A lone ESC whose window has passed was the Escape key, whatever
            // follows it now.
            if let Some((since, written)) = input.escape
                && at.saturating_duration_since(since) >= LONE_ESCAPE_WINDOW
            {
                input.escape = None;
                if input.filter.settle_lone_escape() {
                    human.push(written);
                }
            }
            if input.filter.feed(bytes) {
                human.push(now);
            }
            // A non-empty write leaving a lone ESC held ended with it.
            if !input.filter.holds_lone_escape() {
                input.escape = None;
            } else if !bytes.is_empty() {
                input.escape = Some((at, now));
                held = true;
            }
            // The input waits with its run; the first of it since the tracker
            // last took the run's input sends the reminder to take it.
            let wake = input.pending.is_empty() && !human.is_empty();
            let room = MAX_QUEUED_INPUT.saturating_sub(input.pending.len());
            input.pending.extend(human.into_iter().take(room));
            if wake {
                let _ = exporting.commands.send(Command::HumanInput {
                    pane_id: pane_id.to_string(),
                    run_id: input.run_id.clone(),
                });
            }
        }
        if held {
            shared.escape_held.notify_one();
        }
    }

    /// The agent `agent_id` on `pane_id` is gone. Ignored when a newer
    /// generation already holds the pane. Applied after every event already
    /// broadcast, so the agent's last status is part of its run. A lone `ESC`
    /// still in its window is counted as the run ends, whether by this or by
    /// the pane's close broadcast arriving first.
    pub fn end_run(
        &self,
        pane_id: &str,
        agent_id: Option<&str>,
        reason: EndReason,
        exit_code: Option<i32>,
    ) {
        let Some(exporting) = &self.exporting else {
            return;
        };
        let run_id = exporting
            .shared
            .lock_filters()
            .get_mut(pane_id)
            .and_then(|inputs| {
                inputs.iter_mut().rev().find(|input| {
                    !input.ended && (agent_id.is_none() || input.agent_id.as_deref() == agent_id)
                })
            })
            .map(|input| {
                input.ended = true;
                input.run_id.clone()
            });
        let _ = exporting.commands.send(Command::End {
            pane_id: pane_id.to_string(),
            agent_id: agent_id.map(str::to_string),
            run_id,
            reason,
            exit_code,
            now: now_unix_nanos(),
        });
    }

    /// Feed the daemon's broadcast to the tracker until it closes. Nothing
    /// happens when not exporting, so an off deck spawns nothing.
    pub fn subscribe(&self, rx: broadcast::Receiver<BroadcastMsg>) {
        if let Some(exporting) = &self.exporting {
            let _ = exporting.commands.send(Command::Subscribe(rx));
        }
    }

    /// Resolves once everything sent to the tracker before it — broadcast
    /// events included — has been applied. Immediately when not exporting.
    pub async fn settled(&self) {
        let Some(exporting) = &self.exporting else {
            return;
        };
        let (ack, done) = oneshot::channel();
        if exporting.commands.send(Command::Barrier(ack)).is_ok() {
            let _ = done.await;
        }
    }

    /// Daemon shutdown, bounded by `budget` in all: cancel the run-context
    /// probes (killing their processes), end every open run as
    /// `daemon_shutdown` once the events already broadcast are applied, and
    /// send what the exporter holds, retrying only while the budget lasts.
    pub async fn shutdown(&self, budget: Duration) {
        let Some(exporting) = &self.exporting else {
            return;
        };
        let deadline = deadline_after(budget);
        exporting
            .probes
            .shutdown(budget.min(PROBE_SHUTDOWN_BUDGET))
            .await;
        let (ack, ended) = oneshot::channel();
        let sent = exporting.commands.send(Command::Shutdown {
            now: now_unix_nanos(),
            ack,
        });
        if sent.is_ok() && tokio::time::timeout_at(deadline, ended).await.is_err() {
            tracing::warn!("telemetry: open runs could not be ended in time; spans dropped");
        }
        let (ack, flushed) = oneshot::channel();
        if exporting
            .shared
            .queue
            .try_send(ExportMsg::Flush { ack, deadline })
            .is_ok()
            && tokio::time::timeout_at(deadline, flushed).await.is_err()
        {
            tracing::warn!("telemetry: the final flush did not finish in time; spans dropped");
        }
        if let Some(ingest) = exporting
            .ingest
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            ingest.abort();
        }
    }

    /// Run-context probes queued or running.
    pub fn outstanding_probes(&self) -> usize {
        self.exporting
            .as_ref()
            .map_or(0, |exporting| exporting.probes.outstanding())
    }
}

impl Shared {
    fn with_tracker(&self, f: impl FnOnce(&mut RunTracker)) {
        let finished = {
            let mut tracker = self.tracker.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut tracker);
            tracker.take_finished()
        };
        self.enqueue(finished);
    }

    fn lock_filters(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<PaneInput>>> {
        lock_filters(&self.filters)
    }

    /// Count the human input waiting with `run_id` on `pane_id`, if the run
    /// has not ended and taken it already.
    fn take_input(&self, pane_id: &str, run_id: &str) {
        self.with_tracker(|t| {
            let held = self
                .lock_filters()
                .get_mut(pane_id)
                .and_then(|inputs| inputs.iter_mut().find(|input| input.run_id == run_id))
                .map(|input| input.take_held(false))
                .unwrap_or_default();
            for written in held {
                t.observe_run_input(pane_id, run_id, written);
            }
        });
    }

    /// Count every held lone `ESC` whose window has passed by `at` as human
    /// input at the time it was written, straight into its run, after any
    /// input still waiting with that run. Returns when the next one still
    /// held is due.
    ///
    /// Any write after the ESC settles it on the writer's side instead. The
    /// tracker stays locked throughout, so no run ends between a filter
    /// giving its ESC up and the run counting it.
    ///
    /// A filter whose run the tracker has not opened yet — its
    /// [`Command::Commit`] still queued behind a timer the task had already
    /// picked — is left as it is: the tracker would drop what it gave up.
    /// Its ESC is still timed: the write that left it held woke the task
    /// after that Commit was sent, and the task applies commands first.
    fn settle_escapes(&self, at: std::time::Instant) -> Option<std::time::Instant> {
        let mut next_due: Option<std::time::Instant> = None;
        self.with_tracker(|t| {
            let mut filters = self.lock_filters();
            for (pane_id, inputs) in filters.iter_mut() {
                for input in inputs.iter_mut() {
                    let Some((since, _)) = input.escape else {
                        continue;
                    };
                    if t.run_id(pane_id) != Some(input.run_id.as_str()) {
                        continue;
                    }
                    let due = since + LONE_ESCAPE_WINDOW;
                    if at >= due {
                        for written in input.take_held(true) {
                            t.observe_run_input(pane_id, &input.run_id, written);
                        }
                    } else {
                        next_due = Some(next_due.map_or(due, |d| d.min(due)));
                    }
                }
            }
        });
        next_due
    }

    /// Drop `run_id`'s filter on `pane_id` if [`Telemetry::end_run`] ended it
    /// and the tracker did not retire it: its run never opened. Another
    /// generation's filter is left alone, whatever its End says.
    fn forget_ended_input(&self, pane_id: &str, run_id: &str) {
        let mut filters = self.lock_filters();
        if let Some(inputs) = filters.get_mut(pane_id) {
            inputs.retain(|input| !(input.ended && input.run_id == run_id));
            if inputs.is_empty() {
                filters.remove(pane_id);
            }
        }
    }

    fn observe(&self, msg: &BroadcastMsg) {
        if let BroadcastMsg::Event(event) = msg {
            self.with_tracker(|t| t.observe_event(event, now_unix_nanos()));
        }
    }

    fn enqueue(&self, spans: Vec<SpanRecord>) {
        for span in spans {
            if self
                .queue
                .try_send(ExportMsg::Span(Box::new(span)))
                .is_err()
            {
                let mut log = self.dropped.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(suppressed) = log.record(std::time::Instant::now()) {
                    tracing::warn!(
                        suppressed,
                        "telemetry: the export queue is full; dropping spans"
                    );
                }
            }
        }
    }
}

fn start_exporting(
    config: &TelemetryConfig,
    host: Option<&str>,
    settings: Settings,
) -> Result<Exporting, reqwest::Error> {
    #[cfg(test)]
    let queue = match settings.sink {
        Some(sink) => sink,
        None => spawn_exporter(config, host, settings.retry)?,
    };
    #[cfg(not(test))]
    let queue = spawn_exporter(config, host, settings.retry)?;
    let filters: Arc<Filters> = Arc::default();
    let mut tracker = RunTracker::new();
    let held = Arc::clone(&filters);
    tracker.set_held_input(Box::new(move |pane_id, run_id| {
        retire_input(&held, pane_id, run_id)
    }));
    let shared = Arc::new(Shared {
        tracker: Mutex::new(tracker),
        queue,
        dropped: Mutex::new(RateLimitedLog::new(ERROR_LOG_INTERVAL)),
        filters,
        escape_held: tokio::sync::Notify::new(),
        #[cfg(test)]
        timer_gate: Mutex::new(None),
    });
    let (commands, rx) = mpsc::unbounded_channel();
    let ingest = tokio::spawn(run_ingest(Arc::clone(&shared), rx));
    Ok(Exporting {
        shared,
        commands,
        probes: Probes::new(settings.probes),
        ingest: Mutex::new(Some(ingest)),
    })
}

/// Events applied between two yields to the runtime while draining.
const DRAIN_YIELD_EVERY: usize = 256;

/// The ingest task: apply broadcast events as they come, each command after
/// every event broadcast before it, and a held lone `ESC` as the Escape key
/// once its window has passed.
async fn run_ingest(shared: Arc<Shared>, mut commands: mpsc::UnboundedReceiver<Command>) {
    let mut events: Option<broadcast::Receiver<BroadcastMsg>> = None;
    let mut escape_due: Option<std::time::Instant> = None;
    loop {
        tokio::select! {
            biased;
            command = commands.recv() => {
                let Some(command) = command else { break };
                if let Command::Subscribe(rx) = command {
                    events = Some(rx);
                    continue;
                }
                drain(&shared, &mut events).await;
                if !apply(&shared, command) {
                    break;
                }
            }
            () = shared.escape_held.notified() => {
                #[cfg(test)]
                if let Some((entered, resume)) = shared.timer_gate.lock().unwrap().take() {
                    let _ = entered.send(());
                    let _ = resume.recv();
                }
                escape_due = shared.settle_escapes(std::time::Instant::now());
            }
            () = sleep_until(escape_due) => {
                escape_due = shared.settle_escapes(std::time::Instant::now());
            }
            msg = next_event(&mut events) => match msg {
                Ok(msg) => shared.observe(&msg),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::debug!(missed, "telemetry: broadcast subscriber lagged");
                }
                Err(broadcast::error::RecvError::Closed) => events = None,
            },
        }
    }
}

async fn sleep_until(due: Option<std::time::Instant>) {
    match due {
        Some(due) => tokio::time::sleep_until(Instant::from_std(due)).await,
        None => std::future::pending().await,
    }
}

async fn next_event(
    events: &mut Option<broadcast::Receiver<BroadcastMsg>>,
) -> Result<BroadcastMsg, broadcast::error::RecvError> {
    match events {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Apply every event already broadcast — the ones waiting when the drain
/// starts, not the ones published while it runs, so a producer that never
/// stops cannot hold a command back. Yields to the runtime every
/// [`DRAIN_YIELD_EVERY`] events.
async fn drain(shared: &Shared, events: &mut Option<broadcast::Receiver<BroadcastMsg>>) {
    let Some(rx) = events else {
        return;
    };
    // The messages between this receiver and the sender's tail right now.
    let mut left = rx.len();
    let mut since_yield = 0usize;
    while left > 0 {
        match rx.try_recv() {
            Ok(msg) => {
                left -= 1;
                shared.observe(&msg);
            }
            // The receiver skipped `missed` of them, which it will not see.
            Err(broadcast::error::TryRecvError::Lagged(missed)) => {
                tracing::debug!(missed, "telemetry: broadcast subscriber lagged");
                left = left.saturating_sub(usize::try_from(missed).unwrap_or(usize::MAX));
            }
            Err(_) => break,
        }
        since_yield += 1;
        if since_yield >= DRAIN_YIELD_EVERY {
            since_yield = 0;
            tokio::task::yield_now().await;
        }
    }
}

/// Apply one command; `false` once the task is to stop.
fn apply(shared: &Shared, command: Command) -> bool {
    match command {
        Command::Subscribe(_) => {}
        Command::Commit { run_id } => shared.with_tracker(|t| t.commit_run(&run_id)),
        Command::End {
            pane_id,
            agent_id,
            run_id,
            reason,
            exit_code,
            now,
        } => {
            shared.with_tracker(|t| {
                t.end_agent_run(&pane_id, agent_id.as_deref(), reason, exit_code, now)
            });
            if let Some(run_id) = run_id {
                shared.forget_ended_input(&pane_id, &run_id);
            }
        }
        Command::HumanInput { pane_id, run_id } => shared.take_input(&pane_id, &run_id),
        Command::Barrier(ack) => {
            let _ = ack.send(());
        }
        Command::Shutdown { now, ack } => {
            // A run's last key, a lone ESC still in its window, is counted as
            // the tracker ends the run.
            shared.with_tracker(|t| t.end_all(EndReason::DaemonShutdown, now));
            shared.lock_filters().clear();
            let _ = ack.send(());
            return false;
        }
    }
    true
}

/// Counts occurrences and says when one may be logged: the first, then at most
/// one per interval, carrying how many were suppressed in between.
#[derive(Debug)]
pub struct RateLimitedLog {
    interval: Duration,
    last: Option<std::time::Instant>,
    suppressed: u64,
}

impl RateLimitedLog {
    pub fn new(interval: Duration) -> Self {
        RateLimitedLog {
            interval,
            last: None,
            suppressed: 0,
        }
    }

    /// `Some(suppressed since the last log)` when this occurrence may be
    /// logged, `None` when it is to be counted silently.
    pub fn record(&mut self, now: std::time::Instant) -> Option<u64> {
        match self.last {
            Some(last) if now.duration_since(last) < self.interval => {
                self.suppressed += 1;
                None
            }
            _ => {
                self.last = Some(now);
                Some(std::mem::take(&mut self.suppressed))
            }
        }
    }
}

fn build_headers(headers: &[(String, String)]) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    let mut invalid = 0usize;
    for (key, value) in headers {
        match (
            reqwest::header::HeaderName::from_bytes(key.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(mut value)) => {
                value.set_sensitive(true);
                map.insert(name, value);
            }
            _ => invalid += 1,
        }
    }
    if invalid > 0 {
        tracing::warn!(
            invalid,
            "telemetry: skipped OTLP headers that are not valid HTTP headers"
        );
    }
    map
}

/// The OTLP client: the configured headers on every request, and no redirect
/// followed, so those headers reach the configured endpoint and nothing else.
fn build_client(config: &TelemetryConfig) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(config.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .default_headers(build_headers(&config.headers))
        .build()
}

fn spawn_exporter(
    config: &TelemetryConfig,
    host: Option<&str>,
    retry: RetryPolicy,
) -> Result<mpsc::Sender<ExportMsg>, reqwest::Error> {
    let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
    let sender = Sender {
        client: build_client(config)?,
        endpoint: config.endpoint.clone(),
        protocol: config.protocol.clone(),
        resource: deck_resource(&config.service_name, host, &config.user_resource_attributes),
        timeout: config.timeout,
        retry,
        errors: RateLimitedLog::new(ERROR_LOG_INTERVAL),
        #[cfg(test)]
        panic_on_send: false,
    };
    tokio::spawn(run_exporter(sender, rx));
    Ok(tx)
}

/// The waits between attempts of one export.
#[derive(Debug, Clone, Copy)]
struct RetryPolicy {
    initial: Duration,
    max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            initial: RETRY_INITIAL_BACKOFF,
            max: RETRY_MAX_BACKOFF,
        }
    }
}

/// How one export ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SendOutcome {
    Delivered {
        attempts: u32,
    },
    /// The backend answered with a status that is not retried (a 3xx among
    /// them).
    Rejected {
        status: u16,
    },
    /// Every attempt failed transiently and the time ran out.
    GaveUp {
        attempts: u32,
    },
    /// A failure that is not retried, other than an HTTP status.
    Failed,
}

struct Sender {
    client: reqwest::Client,
    endpoint: String,
    protocol: Protocol,
    resource: Vec<ResourceAttribute>,
    timeout: Duration,
    retry: RetryPolicy,
    errors: RateLimitedLog,
    /// Test seam: every send panics once it has taken its batch.
    #[cfg(test)]
    panic_on_send: bool,
}

/// Whether an HTTP status is a transient failure the OTLP specification
/// retries.
fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

/// A `Retry-After` given in seconds. The HTTP-date form is not honoured; the
/// backoff applies instead.
fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// `Instant::now() + after`, or a day from now when that is not representable:
/// `OTEL_EXPORTER_OTLP_TIMEOUT` accepts any `u64` of milliseconds.
fn deadline_after(after: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(after)
        .unwrap_or_else(|| now + Duration::from_secs(86_400))
}

/// `backoff` scaled by a random factor in `[0.5, 1]`.
fn jittered(backoff: Duration) -> Duration {
    let mut bytes = [0u8; 4];
    let fraction = match getrandom::fill(&mut bytes) {
        Ok(()) => f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX),
        Err(_) => 1.0,
    };
    backoff.mul_f64(0.5 + 0.5 * fraction)
}

impl Sender {
    /// Send `batch`, retrying transient failures until the export timeout or
    /// `flush_deadline`, whichever comes first.
    async fn send(
        &mut self,
        batch: &mut Vec<SpanRecord>,
        flush_deadline: Option<Instant>,
    ) -> Option<SendOutcome> {
        if batch.is_empty() {
            return None;
        }
        let spans = std::mem::take(batch);
        #[cfg(test)]
        assert!(!self.panic_on_send, "telemetry test: a send that panics");
        let (body, content_type) = match self.protocol {
            Protocol::HttpJson => (
                otlp::encode_json(&self.resource, &spans),
                "application/json",
            ),
            _ => (
                otlp::encode_protobuf(&self.resource, &spans),
                "application/x-protobuf",
            ),
        };
        let mut deadline = deadline_after(self.timeout);
        if let Some(flush) = flush_deadline {
            deadline = deadline.min(flush);
        }
        let mut backoff = self.retry.initial;
        let mut attempts = 0u32;
        let mut failure = String::new();
        let outcome = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break SendOutcome::GaveUp { attempts };
            }
            attempts += 1;
            let result = self
                .client
                .post(&self.endpoint)
                .timeout(remaining)
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(body.clone())
                .send()
                .await;
            let wait = match result {
                Ok(response) if response.status().is_success() => {
                    break SendOutcome::Delivered { attempts };
                }
                Ok(response) if is_retryable_status(response.status().as_u16()) => {
                    failure = format!("the backend answered HTTP {}", response.status().as_u16());
                    retry_after(&response)
                }
                Ok(response) => {
                    let status = response.status().as_u16();
                    failure = format!("the backend answered HTTP {status}");
                    break SendOutcome::Rejected { status };
                }
                // `without_url`: the endpoint's userinfo or query can carry a
                // token.
                Err(e) if e.is_connect() || e.is_timeout() || e.is_request() => {
                    failure = e.without_url().to_string();
                    None
                }
                Err(e) => {
                    failure = e.without_url().to_string();
                    break SendOutcome::Failed;
                }
            };
            let wait = wait.unwrap_or_else(|| jittered(backoff));
            // Compared as durations: `now + wait` overflows `Instant` for a
            // `Retry-After` as large as `u64::MAX` seconds.
            if wait >= deadline.saturating_duration_since(Instant::now()) {
                break SendOutcome::GaveUp { attempts };
            }
            tokio::time::sleep(wait).await;
            backoff = (backoff * 2).min(self.retry.max);
        };
        if !matches!(outcome, SendOutcome::Delivered { .. })
            && let Some(suppressed) = self.errors.record(std::time::Instant::now())
        {
            tracing::warn!(
                spans = spans.len(),
                attempts,
                suppressed_failures = suppressed,
                endpoint = %redacted_endpoint(&self.endpoint),
                "telemetry: OTLP export failed: {failure}"
            );
        }
        Some(outcome)
    }
}

/// Await one export, so that a panic inside it costs that batch only: the
/// exporter task, and every later export, carries on. The batch was taken
/// out of the task's buffer when the send began. `Err` when it panicked.
async fn contained<F: std::future::Future>(send: F) -> Result<F::Output, Panicked> {
    use futures_util::FutureExt;
    std::panic::AssertUnwindSafe(send)
        .catch_unwind()
        .await
        .map_err(|_| Panicked)
}

/// An export that panicked; see [`contained`].
#[derive(Debug)]
struct Panicked;

impl Sender {
    /// [`Self::send`], with a panic inside it contained and logged like any
    /// other failed export: at most once per [`ERROR_LOG_INTERVAL`].
    async fn export(&mut self, batch: &mut Vec<SpanRecord>, flush_deadline: Option<Instant>) {
        if contained(self.send(batch, flush_deadline)).await.is_err()
            && let Some(suppressed) = self.errors.record(std::time::Instant::now())
        {
            tracing::warn!(
                suppressed_failures = suppressed,
                "telemetry: an OTLP export failed unexpectedly; its spans are dropped"
            );
        }
    }
}

async fn run_exporter(mut sender: Sender, mut rx: mpsc::Receiver<ExportMsg>) {
    let mut batch: Vec<SpanRecord> = Vec::new();
    // `interval_at`: a plain interval's first tick is immediate, which would
    // send the first span alone.
    let mut tick = tokio::time::interval_at(Instant::now() + BATCH_DELAY, BATCH_DELAY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(ExportMsg::Span(span)) => {
                    batch.push(*span);
                    if batch.len() >= MAX_BATCH {
                        sender.export(&mut batch, None).await;
                    }
                }
                Some(ExportMsg::Flush { ack, deadline }) => {
                    // Everything queued before the flush, in batches.
                    while let Ok(ExportMsg::Span(span)) = rx.try_recv() {
                        batch.push(*span);
                    }
                    while !batch.is_empty() && Instant::now() < deadline {
                        let rest = batch.split_off(batch.len().min(MAX_BATCH));
                        sender.export(&mut batch, Some(deadline)).await;
                        batch = rest;
                    }
                    batch.clear();
                    let _ = ack.send(());
                }
                None => {
                    sender.export(&mut batch, None).await;
                    break;
                }
            },
            _ = tick.tick() => {
                sender.export(&mut batch, None).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentType, EventType};
    use crate::telemetry::span::{AttrValue, Attribute};
    use crate::telemetry::tracker::tests::event;
    use crate::telemetry::{AttrKey, SPAN_AGENT_RUN, SPAN_AGENT_STATUS};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn run_start(pane: &str) -> RunStart {
        RunStart {
            run_id: String::new(),
            pane_id: pane.to_string(),
            agent_id: None,
            agent: AgentType::ClaudeCode,
            orchestration: None,
            model: None,
            invoking_skill: None,
            issue_number: None,
        }
    }

    fn generation(pane: &str, agent_id: &str) -> RunStart {
        RunStart {
            agent_id: Some(agent_id.to_string()),
            ..run_start(pane)
        }
    }

    /// What the spawn path does: prepare, add the variables, commit.
    fn begin(
        telemetry: &Arc<Telemetry>,
        start: RunStart,
        spawn_env: &[(String, String)],
    ) -> Vec<(String, String)> {
        let run = telemetry.prepare_run(start, spawn_env, None).unwrap();
        let added = run.env.clone();
        run.commit();
        added
    }

    fn get<'a>(added: &'a [(String, String)], key: &str) -> Option<&'a str> {
        added
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    const ON: [(&str, &str); 1] = [("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9")];

    /// A telemetry handle whose finished spans land in the returned receiver
    /// instead of an HTTP exporter.
    fn capturing(probes: ProbeSettings) -> (Arc<Telemetry>, mpsc::Receiver<ExportMsg>) {
        let (sink, rx) = mpsc::channel(QUEUE_CAPACITY);
        let telemetry = Telemetry::start_with(
            &env(&ON),
            Settings {
                probes,
                sink: Some(sink),
                ..Settings::default()
            },
        );
        assert!(telemetry.is_exporting());
        (Arc::new(telemetry), rx)
    }

    fn captured(rx: &mut mpsc::Receiver<ExportMsg>) -> Vec<SpanRecord> {
        let mut spans = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let ExportMsg::Span(span) = msg {
                spans.push(*span);
            }
        }
        spans
    }

    fn attr(span: &SpanRecord, key: AttrKey) -> Option<&AttrValue> {
        span.attributes
            .iter()
            .find(|a| a.key == key)
            .map(|a| &a.value)
    }

    fn statuses(spans: &[SpanRecord]) -> Vec<AttrValue> {
        spans
            .iter()
            .filter(|s| s.name == SPAN_AGENT_STATUS)
            .filter_map(|s| attr(s, AttrKey::Status).cloned())
            .collect()
    }

    fn of(pane: &str, agent_id: &str, event_type: EventType) -> BroadcastMsg {
        let mut e = event(pane, event_type);
        e.agent_id = Some(agent_id.to_string());
        BroadcastMsg::Event(e)
    }

    /// The daemon's own broadcast that it closed the pane (`spawn.rs`).
    fn pane_closed(pane: &str, agent_id: &str) -> BroadcastMsg {
        let mut e = event(pane, EventType::SessionEnd);
        e.agent_id = Some(agent_id.to_string());
        e.metadata.insert(
            crate::event::DAEMON_PANE_CLOSED_METADATA_KEY.into(),
            "1".into(),
        );
        BroadcastMsg::Event(e)
    }

    /// Wait until `done` holds. The outer bound is a watchdog for a hung
    /// test, far past anything a loaded runner needs, not a timing claim.
    async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !done() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting until {what}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// A monotonic time an hour ahead: a lone `ESC` written "then" is still
    /// in its window whatever the scheduler does, so only the run ending can
    /// settle it.
    fn window_held_open() -> std::time::Instant {
        std::time::Instant::now() + Duration::from_secs(3600)
    }

    /// The docs promise that a Claude Code switch the user set is left alone.
    /// The spawn overlay rarely carries it; the daemon's own environment, where
    /// the user exports it, usually does — and the overlay wins in the child's
    /// environment, so a `1` computed from the overlay alone would override
    /// the user's `0`.
    #[test]
    fn a_switch_set_in_the_daemon_environment_or_the_overlay_stands() {
        let telemetry = Arc::new(Telemetry::start(&env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ("CLAUDE_CODE_ENABLE_TELEMETRY", "0"),
            ("OTEL_METRICS_EXPORTER", "none"),
        ])));
        let overlay = vec![("OTEL_LOGS_EXPORTER".to_string(), "console".to_string())];
        let run = telemetry
            .prepare_run(run_start("p1"), &overlay, None)
            .unwrap();
        assert_eq!(get(&run.env, "CLAUDE_CODE_ENABLE_TELEMETRY"), None);
        assert_eq!(get(&run.env, "OTEL_METRICS_EXPORTER"), None);
        assert_eq!(get(&run.env, "OTEL_LOGS_EXPORTER"), None);
        assert_eq!(get(&run.env, "OTEL_EXPORTER_OTLP_PROTOCOL"), None);
        assert!(get(&run.env, "OTEL_RESOURCE_ATTRIBUTES").is_some());

        let unset = Arc::new(Telemetry::start(&env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
        ])));
        let run = unset.prepare_run(run_start("p1"), &[], None).unwrap();
        assert_eq!(get(&run.env, "CLAUDE_CODE_ENABLE_TELEMETRY"), Some("1"));
        assert_eq!(get(&run.env, "OTEL_METRICS_EXPORTER"), Some("otlp"));
        assert_eq!(get(&run.env, "OTEL_LOGS_EXPORTER"), Some("otlp"));
    }

    #[test]
    fn rate_limited_log_reports_the_first_then_one_per_interval() {
        let mut log = RateLimitedLog::new(Duration::from_secs(60));
        let t0 = std::time::Instant::now();
        assert_eq!(log.record(t0), Some(0));
        assert_eq!(log.record(t0 + Duration::from_secs(1)), None);
        assert_eq!(log.record(t0 + Duration::from_secs(59)), None);
        assert_eq!(log.record(t0 + Duration::from_secs(61)), Some(2));
        assert_eq!(log.record(t0 + Duration::from_secs(62)), None);
    }

    #[test]
    fn off_means_no_environment_and_nothing_recorded() {
        let telemetry = Arc::new(Telemetry::start(&env(&[("OTEL_SERVICE_NAME", "x")])));
        assert!(!telemetry.is_enabled());
        assert!(!telemetry.is_exporting());
        assert!(
            telemetry
                .prepare_run(run_start("p1"), &[], Some(PathBuf::from("/")))
                .is_none()
        );
    }

    #[tokio::test]
    async fn grpc_propagates_but_exports_nothing() {
        let telemetry = Arc::new(Telemetry::start(&env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
        ])));
        assert!(telemetry.is_enabled());
        assert!(!telemetry.is_exporting());
        let (tx, rx) = broadcast::channel(4);
        telemetry.subscribe(rx);
        drop(tx);
        let added = begin(&telemetry, run_start("p1"), &[]);
        assert!(added.iter().any(|(k, _)| k == "OTEL_RESOURCE_ATTRIBUTES"));
    }

    // ---- R1: lifecycle and broadcast in one order ----

    /// A fast agent's first status is broadcast — and read by the ingest task
    /// — before the spawn path commits its run. It is kept.
    #[tokio::test]
    async fn a_status_broadcast_before_the_commit_survives() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        let prepared = telemetry
            .prepare_run(generation("p1", "a1"), &[], None)
            .unwrap();
        tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
        tx.send(of("p1", "a1", EventType::ToolStart)).unwrap();
        telemetry.settled().await;
        // The events have been applied, and the run is not committed yet.
        prepared.commit();
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        let spans = captured(&mut rx);
        let seen = statuses(&spans);
        assert_eq!(
            seen.last(),
            Some(&AttrValue::Str("working".into())),
            "{seen:?}"
        );
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        assert_eq!(
            attr(run, AttrKey::StatusChangesCount),
            Some(&AttrValue::Int(seen.len() as i64))
        );
    }

    /// An agent's last status is broadcast just before its PTY closes, and the
    /// reader thread ends the run before the ingest task has read it. It is
    /// part of the run all the same.
    #[tokio::test]
    async fn a_status_broadcast_before_the_end_survives() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("p1", "a1"), &[]);
        tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
        telemetry.settled().await;
        let before = statuses(&captured(&mut rx));
        tx.send(of("p1", "a1", EventType::ToolStart)).unwrap();
        tx.send(of("p1", "a1", EventType::Idle)).unwrap();
        // No await between the broadcasts and the end: the task has not run.
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        let spans = captured(&mut rx);
        let mut seen = before;
        seen.extend(statuses(&spans));
        assert!(seen.contains(&AttrValue::Str("working".into())), "{seen:?}");
        assert_eq!(
            seen.last(),
            Some(&AttrValue::Str("idle".into())),
            "{seen:?}"
        );
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        assert_eq!(
            attr(run, AttrKey::EndReason),
            Some(&AttrValue::Str("exited".into()))
        );
    }

    /// The same, for daemon shutdown: events broadcast before it are applied
    /// before every run is ended.
    #[tokio::test]
    async fn a_status_broadcast_before_shutdown_survives() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        begin(&telemetry, generation("p1", "a1"), &[]);
        tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
        tx.send(of("p1", "a1", EventType::ToolStart)).unwrap();
        telemetry.shutdown(Duration::from_secs(2)).await;
        let spans = captured(&mut rx);
        let seen = statuses(&spans);
        assert_eq!(
            seen.last(),
            Some(&AttrValue::Str("working".into())),
            "{seen:?}"
        );
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        assert_eq!(
            attr(run, AttrKey::EndReason),
            Some(&AttrValue::Str("daemon_shutdown".into()))
        );
    }

    /// A spawn refused after `prepare_run` drops its `PreparedRun`: the run is
    /// forgotten, with the events it was holding.
    #[tokio::test]
    async fn a_refused_spawn_leaves_no_run() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        let prepared = telemetry
            .prepare_run(generation("p1", "a1"), &[], None)
            .unwrap();
        tx.send(of("p1", "a1", EventType::SessionStart)).unwrap();
        telemetry.settled().await;
        drop(prepared);
        telemetry.settled().await;
        let shared = &telemetry.exporting.as_ref().unwrap().shared;
        assert_eq!(shared.tracker.lock().unwrap().held(), (0, 0));
        telemetry.shutdown(Duration::from_secs(2)).await;
        assert!(captured(&mut rx).is_empty());
    }

    /// R3 / R6: what the handle keeps per run — the tracker's runs and the
    /// input filters — is gone once the runs end.
    #[tokio::test]
    async fn many_runs_return_the_handle_to_its_baseline() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let (tx, events) = broadcast::channel(1024);
        telemetry.subscribe(events);
        for i in 0..100 {
            let pane = format!("pane-{i}");
            let agent = format!("agent-{i}");
            begin(&telemetry, generation(&pane, &agent), &[]);
            tx.send(of(&pane, &agent, EventType::SessionStart)).unwrap();
            telemetry.observe_client_input(&pane, b"\x1b[24;");
            telemetry.end_run(&pane, Some(&agent), EndReason::Exited, None);
            telemetry.settled().await;
            captured(&mut rx);
        }
        let shared = &telemetry.exporting.as_ref().unwrap().shared;
        assert_eq!(shared.tracker.lock().unwrap().held(), (0, 0));
        assert_eq!(shared.tracker.lock().unwrap().live_shadow_states(), 0);
        assert!(shared.lock_filters().is_empty());
    }

    /// The run span of `spans`, its intervention count and the times of its
    /// intervention events.
    fn interventions(spans: &[SpanRecord]) -> (Option<AttrValue>, Vec<u64>) {
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        let times = run
            .events
            .iter()
            .filter(|e| e.name == crate::telemetry::EVENT_INTERVENTION)
            .map(|e| e.time_unix_nanos)
            .collect();
        (attr(run, AttrKey::InterventionsCount).cloned(), times)
    }

    /// N2: pressing Escape alone and then nothing else — the way to interrupt
    /// an agent — is an intervention, whether the window passes before the
    /// run ends, the run ends inside the window, or the daemon shuts down.
    #[tokio::test]
    async fn the_escape_key_alone_is_an_intervention() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let shared = Arc::clone(&telemetry.exporting.as_ref().unwrap().shared);

        // The window passes while the run is idle: the ingest task settles it.
        // Its timer is not a command, so wait for the settlement itself — the
        // filter giving the ESC up, which happens with the tracker locked, in
        // one step with the run counting it.
        begin(&telemetry, generation("p1", "a1"), &[]);
        let written = now_unix_nanos();
        telemetry.observe_client_input_at("p1", b"\x1b", written, std::time::Instant::now());
        eventually("the ingest task settled the Escape", || {
            shared.lock_filters()["p1"]
                .iter()
                .all(|input| input.escape.is_none() && !input.filter.is_holding())
        })
        .await;
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        let (count, times) = interventions(&captured(&mut rx));
        assert_eq!(count, Some(AttrValue::Int(1)));
        assert_eq!(times, vec![written], "counted at the time it was written");

        // The run ends inside the window.
        begin(&telemetry, generation("p1", "a2"), &[]);
        telemetry.observe_client_input("p1", b"\x1b");
        telemetry.end_run("p1", Some("a2"), EndReason::Exited, None);
        telemetry.settled().await;
        assert_eq!(interventions(&captured(&mut rx)).0, Some(AttrValue::Int(1)));

        // The daemon shuts down inside the window.
        begin(&telemetry, generation("p1", "a3"), &[]);
        telemetry.observe_client_input("p1", b"\x1b");
        telemetry.shutdown(Duration::from_secs(2)).await;
        assert_eq!(interventions(&captured(&mut rx)).0, Some(AttrValue::Int(1)));
    }

    /// N7: the daemon's pane-close broadcast ends the run while a lone ESC is
    /// still in its window — read by the ingest task before the reader
    /// thread's `end_run`, or already broadcast when `end_run` is called — and
    /// a new generation replacing the run does the same. The Escape is counted
    /// in the run it was pressed in, at the time it was written, every time.
    #[tokio::test]
    async fn the_escape_key_counts_however_the_run_ends() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let shared = Arc::clone(&telemetry.exporting.as_ref().unwrap().shared);
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        let run_of = |spans: &[SpanRecord], reason: &str| {
            let runs: Vec<_> = spans.iter().filter(|s| s.name == SPAN_AGENT_RUN).collect();
            assert_eq!(runs.len(), 1, "one run span");
            assert_eq!(
                attr(runs[0], AttrKey::EndReason),
                Some(&AttrValue::Str(reason.into()))
            );
            interventions(spans)
        };

        // The close broadcast is read first; the reader's end_run comes later.
        begin(&telemetry, generation("p1", "a1"), &[]);
        telemetry.settled().await;
        let written = now_unix_nanos();
        telemetry.observe_client_input_at("p1", b"\x1b", written, window_held_open());
        tx.send(pane_closed("p1", "a1")).unwrap();
        telemetry.settled().await;
        let (count, times) = run_of(&captured(&mut rx), "closed");
        assert_eq!(count, Some(AttrValue::Int(1)));
        assert_eq!(times, vec![written]);
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        assert!(captured(&mut rx).is_empty(), "the run ended once");

        // The close is already broadcast, unread, when end_run is called.
        begin(&telemetry, generation("p1", "a2"), &[]);
        telemetry.settled().await;
        let written = now_unix_nanos();
        telemetry.observe_client_input_at("p1", b"\x1b", written, window_held_open());
        tx.send(pane_closed("p1", "a2")).unwrap();
        // No await in between: the ingest task has read neither.
        telemetry.end_run("p1", Some("a2"), EndReason::Exited, None);
        telemetry.settled().await;
        let (count, times) = run_of(&captured(&mut rx), "closed");
        assert_eq!(count, Some(AttrValue::Int(1)));
        assert_eq!(times, vec![written]);

        // A new generation replaces the run with no end_run for it.
        begin(&telemetry, generation("p1", "a3"), &[]);
        telemetry.settled().await;
        telemetry.observe_client_input_at("p1", b"\x1b", now_unix_nanos(), window_held_open());
        begin(&telemetry, generation("p1", "a4"), &[]);
        telemetry.settled().await;
        let (count, _) = run_of(&captured(&mut rx), "unknown");
        assert_eq!(count, Some(AttrValue::Int(1)));

        // A late close for an older generation leaves the newer one's Escape
        // where it is.
        telemetry.observe_client_input_at("p1", b"\x1b", now_unix_nanos(), window_held_open());
        tx.send(pane_closed("p1", "a3")).unwrap();
        telemetry.settled().await;
        assert!(captured(&mut rx).is_empty());
        assert!(shared.lock_filters()["p1"][0].escape.is_some());
        telemetry.end_run("p1", Some("a4"), EndReason::Exited, None);
        telemetry.settled().await;
        let (count, _) = run_of(&captured(&mut rx), "exited");
        assert_eq!(count, Some(AttrValue::Int(1)));
        assert!(shared.lock_filters().is_empty());
    }

    /// N10: input already accepted from a person is counted in its run when
    /// the pane's close broadcast is applied before the notice of it — an
    /// ordinary key, and an Escape the writer settled as its window passed —
    /// and counted once, whichever of the notice and the close comes first.
    #[tokio::test]
    async fn accepted_input_counts_when_the_close_overtakes_it() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let shared = Arc::clone(&telemetry.exporting.as_ref().unwrap().shared);
        let (tx, events) = broadcast::channel(16);
        telemetry.subscribe(events);
        let closed = |spans: &[SpanRecord]| {
            let runs: Vec<_> = spans.iter().filter(|s| s.name == SPAN_AGENT_RUN).collect();
            assert_eq!(runs.len(), 1, "one run span");
            assert_eq!(
                attr(runs[0], AttrKey::EndReason),
                Some(&AttrValue::Str("closed".into()))
            );
            interventions(spans)
        };

        // A key, then the close, with no await between: the ingest task reads
        // the close while draining the broadcast for the input's notice.
        begin(&telemetry, generation("p1", "a1"), &[]);
        telemetry.settled().await;
        let written = now_unix_nanos();
        telemetry.observe_client_input_at("p1", b"a", written, std::time::Instant::now());
        tx.send(pane_closed("p1", "a1")).unwrap();
        telemetry.settled().await;
        let (count, times) = closed(&captured(&mut rx));
        assert_eq!(count, Some(AttrValue::Int(1)));
        assert_eq!(times, vec![written]);
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        assert!(captured(&mut rx).is_empty(), "the run ended once");
        assert!(shared.lock_filters().is_empty());

        // An Escape the next write finds past its window — a focus report, no
        // key — is settled by the writer into the same waiting input.
        begin(&telemetry, generation("p1", "a2"), &[]);
        telemetry.settled().await;
        let at = window_held_open();
        let written = now_unix_nanos();
        telemetry.observe_client_input_at("p1", b"\x1b", written, at);
        telemetry.observe_client_input_at("p1", b"\x1b[I", written + 1, at + LONE_ESCAPE_WINDOW);
        assert!(
            shared.lock_filters()["p1"][0].escape.is_none(),
            "the writer settled the Escape"
        );
        tx.send(pane_closed("p1", "a2")).unwrap();
        telemetry.settled().await;
        let (count, times) = closed(&captured(&mut rx));
        assert_eq!(count, Some(AttrValue::Int(1)));
        assert_eq!(times, vec![written]);

        // The notice applied before the close: counted then, not again as the
        // run ends.
        begin(&telemetry, generation("p1", "a3"), &[]);
        telemetry.settled().await;
        telemetry.observe_client_input("p1", b"a");
        telemetry.settled().await;
        tx.send(pane_closed("p1", "a3")).unwrap();
        telemetry.settled().await;
        assert_eq!(closed(&captured(&mut rx)).0, Some(AttrValue::Int(1)));
        assert!(shared.lock_filters().is_empty());
    }

    /// N12: a timer the ingest task picked before a new run's Commit was sent
    /// leaves that run's input alone — a key, and an Escape already past its
    /// window — and the Escape is still settled once the run opens, before
    /// anything else touches the run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_timer_picked_before_a_commit_leaves_the_new_runs_input_alone() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let shared = Arc::clone(&telemetry.exporting.as_ref().unwrap().shared);
        begin(&telemetry, generation("p0", "old"), &[]);
        telemetry.settled().await;

        // An Escape on another pane takes the task into its timer branch,
        // where it waits.
        let (entered, picked) = oneshot::channel();
        let (resume, paused) = std::sync::mpsc::channel();
        *shared.timer_gate.lock().unwrap() = Some((entered, paused));
        telemetry.observe_client_input("p0", b"\x1b");
        picked.await.unwrap();

        // A new run is committed and typed into, its Escape already past its
        // window, while the Commit waits behind the branch.
        begin(&telemetry, generation("p1", "new"), &[]);
        let at = std::time::Instant::now() - LONE_ESCAPE_WINDOW;
        let written = now_unix_nanos();
        telemetry.observe_client_input_at("p1", b"a", written, at);
        telemetry.observe_client_input_at("p1", b"\x1b", written + 6_000_000_000, at);
        resume.send(()).unwrap();

        eventually("the ingest task settled the new run's Escape", || {
            shared.lock_filters()["p1"]
                .iter()
                .all(|input| input.escape.is_none() && input.pending.is_empty())
        })
        .await;
        telemetry.end_run("p1", Some("new"), EndReason::Exited, None);
        telemetry.settled().await;
        let (count, times) = interventions(&captured(&mut rx));
        assert_eq!(count, Some(AttrValue::Int(2)));
        assert_eq!(times, vec![written, written + 6_000_000_000]);
    }

    /// N11: an End that names no agent cleans up the run it ended and no
    /// other — not a newer generation's held Escape whose own End is queued
    /// behind it.
    #[tokio::test]
    async fn an_end_naming_no_agent_leaves_the_next_generation_alone() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let shared = Arc::clone(&telemetry.exporting.as_ref().unwrap().shared);
        begin(&telemetry, generation("p1", "a"), &[]);
        telemetry.settled().await;
        telemetry.observe_client_input_at("p1", b"\x1b", now_unix_nanos(), window_held_open());
        telemetry.end_run("p1", None, EndReason::Exited, None);
        begin(&telemetry, generation("p1", "b"), &[]);
        telemetry.observe_client_input_at("p1", b"\x1b", now_unix_nanos(), window_held_open());
        telemetry.end_run("p1", None, EndReason::Exited, None);
        telemetry.settled().await;
        let counts: Vec<_> = captured(&mut rx)
            .iter()
            .filter(|s| s.name == SPAN_AGENT_RUN)
            .map(|s| attr(s, AttrKey::InterventionsCount).cloned())
            .collect();
        assert_eq!(
            counts,
            vec![Some(AttrValue::Int(1)), Some(AttrValue::Int(1))]
        );
        assert!(shared.lock_filters().is_empty());
    }

    /// N2: Escape followed by another key. Inside the window the two are one
    /// sequence (Alt+key); past it the Escape counts at its own time, and the
    /// key joins its burst or starts a new one by the usual rule.
    #[tokio::test]
    async fn the_escape_key_then_another_key_counts_by_when_each_was_pressed() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        let second = 1_000_000_000u64;
        let cases: [(Duration, i64, usize); 3] = [
            // Alt+a: one intervention, at the second write.
            (Duration::from_millis(10), 1, 1),
            // Past the window, inside a burst: one, at the Escape.
            (Duration::from_secs(1), 1, 0),
            // Past a burst: two.
            (Duration::from_secs(6), 2, 0),
        ];
        for (i, (later, count, first_at)) in cases.into_iter().enumerate() {
            let agent = format!("a{i}");
            begin(&telemetry, generation("p1", &agent), &[]);
            let t0 = 100 * second;
            let at = std::time::Instant::now();
            let t1 = t0 + later.as_nanos() as u64;
            telemetry.observe_client_input_at("p1", b"\x1b", t0, at);
            telemetry.observe_client_input_at("p1", b"a", t1, at + later);
            telemetry.end_run("p1", Some(&agent), EndReason::Exited, None);
            telemetry.settled().await;
            let (got, times) = interventions(&captured(&mut rx));
            assert_eq!(got, Some(AttrValue::Int(count)), "{later:?}");
            assert_eq!(times[0], [t0, t1][first_at], "{later:?}");
            if count == 2 {
                assert_eq!(times, vec![t0, t1]);
            }
        }

        // A report whose remainder arrives inside the window is still a report.
        begin(&telemetry, generation("p1", "r"), &[]);
        let at = std::time::Instant::now();
        telemetry.observe_client_input_at("p1", b"\x1b", 1, at);
        telemetry.observe_client_input_at("p1", b"[24;80R", 2, at + Duration::from_millis(5));
        telemetry.end_run("p1", Some("r"), EndReason::Exited, None);
        telemetry.settled().await;
        assert_eq!(interventions(&captured(&mut rx)).0, Some(AttrValue::Int(0)));
    }

    /// N3: with events published without pause — faster than the task applies
    /// them, so it also lags — an End and a Shutdown still take effect in
    /// bounded time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_producer_that_never_stops_does_not_hold_a_command_back() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        // Acknowledge the final flush, so the shutdown returns once it has
        // been applied rather than at the end of its budget.
        let (spans_tx, mut spans_rx) = mpsc::unbounded_channel();
        let exporter = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    ExportMsg::Span(span) => {
                        let _ = spans_tx.send(*span);
                    }
                    ExportMsg::Flush { ack, .. } => {
                        let _ = ack.send(());
                    }
                }
            }
        });
        let (tx, events) = broadcast::channel(8);
        telemetry.subscribe(events);
        begin(&telemetry, generation("p1", "a1"), &[]);
        begin(&telemetry, generation("p2", "a2"), &[]);
        // Runs whose agents are still starting: every event is checked against
        // each, which makes applying one far slower than publishing one.
        let starting: Vec<PreparedRun> = (0..20_000)
            .map(|i| {
                let id = format!("s{i}");
                telemetry
                    .prepare_run(generation(&id, &id), &[], None)
                    .unwrap()
            })
            .collect();
        telemetry.settled().await;
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicUsize::new(0));
        // One status, over and over: no status change, so no span floods the
        // export queue.
        let msg = of("p1", "a1", EventType::ToolStart);
        let producers: Vec<_> = (0..3)
            .map(|_| {
                let (tx, stop, msg) = (tx.clone(), Arc::clone(&stop), msg.clone());
                let running = Arc::clone(&running);
                std::thread::spawn(move || {
                    let _ = tx.send(msg.clone());
                    running.fetch_add(1, Ordering::SeqCst);
                    while !stop.load(Ordering::Relaxed) {
                        let _ = tx.send(msg.clone());
                    }
                })
            })
            .collect();
        // The pressure the commands must get through: every producer is
        // publishing, and the task is a whole channel behind.
        eventually("every producer is publishing", || {
            running.load(Ordering::SeqCst) == 3
        })
        .await;
        eventually("the ingest task lags a full channel", || tx.len() >= 8).await;
        // Watchdogs, not timing claims: a drain that follows the producers
        // never finishes, whatever the bound.
        let watchdog = Duration::from_secs(60);
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        let ended = tokio::time::timeout(watchdog, telemetry.settled()).await;
        let shut =
            tokio::time::timeout(watchdog, telemetry.shutdown(Duration::from_secs(30))).await;
        stop.store(true, Ordering::Relaxed);
        for producer in producers {
            producer.join().unwrap();
        }
        drop(starting);
        assert!(
            ended.is_ok(),
            "the End was applied while publication went on"
        );
        assert!(shut.is_ok(), "the shutdown returned");
        exporter.abort();
        let spans: Vec<SpanRecord> = std::iter::from_fn(|| spans_rx.try_recv().ok()).collect();
        let reasons: Vec<_> = spans
            .iter()
            .filter(|s| s.name == SPAN_AGENT_RUN)
            .filter_map(|s| attr(s, AttrKey::EndReason).cloned())
            .collect();
        assert!(
            reasons.contains(&AttrValue::Str("exited".into())),
            "{reasons:?}"
        );
        assert!(
            reasons.contains(&AttrValue::Str("daemon_shutdown".into())),
            "the Shutdown was applied, not timed out: {reasons:?}"
        );
    }

    /// R6: a terminal report split across two writes is not an intervention;
    /// a key is.
    #[tokio::test]
    async fn a_split_report_is_not_an_intervention() {
        let (telemetry, mut rx) = capturing(ProbeSettings::default());
        begin(&telemetry, generation("p1", "a1"), &[]);
        telemetry.observe_client_input("p1", b"\x1b[");
        telemetry.observe_client_input("p1", b"24;80R");
        telemetry.observe_client_input("p1", b"\x1b]11;rgb:0");
        telemetry.observe_client_input("p1", b"000/0000/0000\x07");
        telemetry.end_run("p1", Some("a1"), EndReason::Exited, None);
        telemetry.settled().await;
        let spans = captured(&mut rx);
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        assert_eq!(
            attr(run, AttrKey::InterventionsCount),
            Some(&AttrValue::Int(0))
        );

        begin(&telemetry, generation("p1", "a2"), &[]);
        telemetry.observe_client_input("p1", b"\x1b[24;80R");
        telemetry.observe_client_input("p1", b"y");
        telemetry.end_run("p1", Some("a2"), EndReason::Exited, None);
        telemetry.settled().await;
        let spans = captured(&mut rx);
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        assert_eq!(
            attr(run, AttrKey::InterventionsCount),
            Some(&AttrValue::Int(1))
        );
    }

    /// A.3: shutdown with a run-context probe hanging returns within its
    /// budget, having killed and reaped the probe.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_is_bounded_with_a_hanging_probe() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let pid_file = scratch.path().join("pids");
        let script = scratch.path().join("stalled-git.sh");
        std::fs::write(
            &script,
            format!("echo $$ >> '{}'\nexec sleep 30\n", pid_file.display()),
        )
        .unwrap();
        let (telemetry, mut rx) = capturing(ProbeSettings {
            program: Some((
                PathBuf::from("/bin/sh"),
                vec![script.to_string_lossy().into_owned()],
            )),
            deadline: Duration::from_secs(60),
            ..ProbeSettings::default()
        });
        let run = telemetry
            .prepare_run(
                generation("p1", "a1"),
                &[],
                Some(scratch.path().to_path_buf()),
            )
            .unwrap();
        run.commit();
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while !pid_file.exists() {
            assert!(std::time::Instant::now() < until, "the probe never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let started = std::time::Instant::now();
        telemetry.shutdown(Duration::from_secs(2)).await;
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(telemetry.outstanding_probes(), 0);
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "the probe was killed and reaped");
        let spans = captured(&mut rx);
        let run = spans.iter().find(|s| s.name == SPAN_AGENT_RUN).unwrap();
        assert!(attr(run, AttrKey::InstructionsCommit).is_none());
        assert!(attr(run, AttrKey::RepositoryName).is_none());
    }

    // ---- HTTP ----

    /// One request a loopback receiver took: its head (lowercased) and body.
    #[derive(Debug, Clone)]
    struct Request {
        head: String,
        body: Vec<u8>,
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Request> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            let n = socket.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map(|v| v.trim().parse().unwrap())
            .unwrap_or(0);
        while buf.len() < head_end + length {
            let n = socket.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        Some(Request {
            head,
            body: buf[head_end..head_end + length].to_vec(),
        })
    }

    /// A loopback receiver that answers each connection's one request with
    /// the next of `responses` (the last repeating), and reports every request
    /// it took.
    async fn receiver(
        responses: Vec<String>,
    ) -> (std::net::SocketAddr, mpsc::UnboundedReceiver<Request>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut served = 0usize;
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let Some(request) = read_request(&mut socket).await else {
                    continue;
                };
                let response = &responses[served.min(responses.len() - 1)];
                served += 1;
                let _ = tx.send(request);
                let _ = socket
                    .write_all(
                        format!("{response}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                            .as_bytes(),
                    )
                    .await;
                let _ = socket.shutdown().await;
            }
        });
        (addr, rx)
    }

    const OK: &str = "HTTP/1.1 200 OK";
    const UNAVAILABLE: &str = "HTTP/1.1 503 Service Unavailable";

    fn sender_for(endpoint: String, headers: &str, timeout: Duration) -> Sender {
        let mut pairs = vec![("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", endpoint.as_str())];
        if !headers.is_empty() {
            pairs.push(("OTEL_EXPORTER_OTLP_HEADERS", headers));
        }
        let config = TelemetryConfig::from_env(&env(&pairs)).unwrap();
        Sender {
            client: build_client(&config).unwrap(),
            endpoint: config.endpoint.clone(),
            protocol: config.protocol.clone(),
            resource: deck_resource(&config.service_name, Some("h"), &[]),
            timeout,
            retry: RetryPolicy {
                initial: Duration::from_millis(20),
                max: Duration::from_millis(80),
            },
            errors: RateLimitedLog::new(ERROR_LOG_INTERVAL),
            panic_on_send: false,
        }
    }

    fn one_span() -> Vec<SpanRecord> {
        let mut tracker = RunTracker::new();
        tracker.begin_run(crate::telemetry::tracker::tests::start("p1"), 1);
        tracker.end_run("p1", EndReason::Exited, None, 2);
        tracker.take_finished()
    }

    /// R5: a 503 is retried, and the retry delivers the same batch.
    #[tokio::test]
    async fn a_transient_failure_is_retried_until_it_delivers() {
        let (addr, mut requests) = receiver(vec![
            UNAVAILABLE.into(),
            "HTTP/1.1 429 Too Many".into(),
            OK.into(),
        ])
        .await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            "",
            Duration::from_secs(5),
        );
        let outcome = sender.send(&mut one_span(), None).await;
        assert_eq!(outcome, Some(SendOutcome::Delivered { attempts: 3 }));
        let bodies: Vec<Vec<u8>> = std::iter::from_fn(|| requests.try_recv().ok())
            .map(|r| r.body)
            .collect();
        assert_eq!(bodies.len(), 3);
        assert!(
            bodies.windows(2).all(|w| w[0] == w[1]),
            "the same batch each time"
        );
    }

    /// R5: a backend that never recovers is given up on within the export
    /// timeout, after more than one attempt.
    #[tokio::test]
    async fn a_backend_that_never_recovers_is_given_up_on_in_time() {
        let (addr, _requests) = receiver(vec![UNAVAILABLE.into()]).await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            "",
            Duration::from_millis(600),
        );
        let started = std::time::Instant::now();
        let outcome = sender.send(&mut one_span(), None).await;
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "{:?}",
            started.elapsed()
        );
        assert!(
            matches!(outcome, Some(SendOutcome::GaveUp { attempts }) if attempts >= 2),
            "{outcome:?}"
        );
        // A refused connection is transient too, and bounded the same way.
        let mut sender = sender_for(
            "http://127.0.0.1:9/v1/traces".into(),
            "",
            Duration::from_millis(300),
        );
        let started = std::time::Instant::now();
        assert!(matches!(
            sender.send(&mut one_span(), None).await,
            Some(SendOutcome::GaveUp { .. })
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// R5: the remaining shutdown budget bounds the retries, not only the
    /// export timeout.
    #[tokio::test]
    async fn the_flush_deadline_bounds_the_retries() {
        let (addr, _requests) = receiver(vec![UNAVAILABLE.into()]).await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            "",
            Duration::from_secs(30),
        );
        let started = std::time::Instant::now();
        let outcome = sender
            .send(
                &mut one_span(),
                Some(Instant::now() + Duration::from_millis(300)),
            )
            .await;
        assert!(
            matches!(outcome, Some(SendOutcome::GaveUp { .. })),
            "{outcome:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    /// R5: `Retry-After` sets the wait; one past the deadline ends the export
    /// at once.
    #[tokio::test]
    async fn retry_after_is_honoured() {
        let (addr, _requests) =
            receiver(vec![format!("{UNAVAILABLE}\r\nretry-after: 1"), OK.into()]).await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            "",
            Duration::from_secs(5),
        );
        let started = std::time::Instant::now();
        let outcome = sender.send(&mut one_span(), None).await;
        assert_eq!(outcome, Some(SendOutcome::Delivered { attempts: 2 }));
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "{:?}",
            started.elapsed()
        );

        let (addr, mut requests) =
            receiver(vec![format!("{UNAVAILABLE}\r\nretry-after: 30")]).await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            "",
            Duration::from_secs(2),
        );
        let started = std::time::Instant::now();
        let outcome = sender.send(&mut one_span(), None).await;
        assert_eq!(outcome, Some(SendOutcome::GaveUp { attempts: 1 }));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(requests.try_recv().is_ok());
        assert!(requests.try_recv().is_err(), "no second attempt");
    }

    /// N1: a `Retry-After` of any value ends in bounded time without a panic:
    /// one too large to add to a clock (`u64::MAX` seconds) or merely past the
    /// deadline gives up at once; one that is not a number of seconds falls
    /// back to the backoff; zero retries at once.
    #[tokio::test]
    async fn any_retry_after_ends_in_bounded_time() {
        let cases: [(&str, SendOutcome); 6] = [
            ("18446744073709551615", SendOutcome::GaveUp { attempts: 1 }),
            ("1000000000", SendOutcome::GaveUp { attempts: 1 }),
            (
                "99999999999999999999999",
                SendOutcome::Delivered { attempts: 2 },
            ),
            ("-5", SendOutcome::Delivered { attempts: 2 }),
            ("0", SendOutcome::Delivered { attempts: 2 }),
            (
                "Wed, 21 Oct 2015 07:28:00 GMT",
                SendOutcome::Delivered { attempts: 2 },
            ),
        ];
        for (value, expected) in cases {
            let (addr, _requests) = receiver(vec![
                format!("{UNAVAILABLE}\r\nretry-after: {value}"),
                OK.into(),
            ])
            .await;
            let mut sender = sender_for(
                format!("http://{addr}/v1/traces"),
                "",
                Duration::from_secs(2),
            );
            let started = std::time::Instant::now();
            let outcome = sender.send(&mut one_span(), None).await;
            assert_eq!(outcome, Some(expected), "retry-after: {value}");
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "retry-after: {value}: {:?}",
                started.elapsed()
            );
        }
    }

    /// N1: after a `Retry-After` of `u64::MAX` seconds, the exporter task is
    /// still there: a later batch is sent and delivered through it.
    #[tokio::test]
    async fn the_exporter_outlives_an_extreme_retry_after() {
        let (addr, mut requests) = receiver(vec![
            format!("{UNAVAILABLE}\r\nretry-after: 18446744073709551615"),
            OK.into(),
        ])
        .await;
        let config = TelemetryConfig::from_env(&env(&[(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            &format!("http://{addr}/v1/traces"),
        )]))
        .unwrap();
        let queue = spawn_exporter(&config, Some("h"), RetryPolicy::default()).unwrap();
        for _ in 0..2 {
            for span in one_span() {
                queue.send(ExportMsg::Span(Box::new(span))).await.unwrap();
            }
            let (ack, done) = oneshot::channel();
            queue
                .send(ExportMsg::Flush {
                    ack,
                    deadline: Instant::now() + Duration::from_secs(5),
                })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), done)
                .await
                .expect("the flush finished in time")
                .expect("the exporter task is alive to acknowledge it");
        }
        let taken: Vec<Request> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
        assert_eq!(taken.len(), 2, "the first batch once, the later one once");
    }

    /// An export timeout too large to add to a clock (the variable takes any
    /// number of milliseconds) still sends.
    #[tokio::test]
    async fn an_extreme_export_timeout_still_sends() {
        let (addr, _requests) = receiver(vec![OK.into()]).await;
        let config = TelemetryConfig::from_env(&env(&[
            (
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                &format!("http://{addr}/v1/traces"),
            ),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "18446744073709551615"),
        ]))
        .unwrap();
        let mut sender = sender_for(config.endpoint.clone(), "", config.timeout);
        assert_eq!(
            sender.send(&mut one_span(), None).await,
            Some(SendOutcome::Delivered { attempts: 1 })
        );
    }

    /// A panic inside one export is contained: the exporter task carries on.
    #[tokio::test]
    async fn a_panicking_export_is_contained() {
        assert!(
            contained(async { panic!("an export that panics") })
                .await
                .is_err()
        );
        let mut ran = false;
        assert!(contained(async { ran = true }).await.is_ok());
        assert!(ran);
    }

    /// N9: an export that panics batch after batch, through the exporter
    /// task itself, is logged like any failed export — once, then not again
    /// within the interval — and the task carries on acknowledging flushes.
    #[tokio::test]
    async fn repeated_export_panics_are_logged_at_the_error_rate() {
        use tracing_subscriber::layer::SubscriberExt;
        let logs = Captured::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(
                "dot_agent_deck::telemetry=trace",
            ))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(move || writer.clone()),
            );
        let _guard = crate::test_isolation::capture_tracing_on_this_thread(subscriber);

        let mut sender = sender_for(
            "http://127.0.0.1:9/v1/traces".into(),
            "",
            Duration::from_secs(1),
        );
        sender.panic_on_send = true;
        let (queue, rx) = mpsc::channel(QUEUE_CAPACITY);
        let exporter = tokio::spawn(run_exporter(sender, rx));
        for _ in 0..5 {
            for span in one_span() {
                queue.send(ExportMsg::Span(Box::new(span))).await.unwrap();
            }
            let (ack, done) = oneshot::channel();
            queue
                .send(ExportMsg::Flush {
                    ack,
                    deadline: Instant::now() + Duration::from_secs(60),
                })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(60), done)
                .await
                .expect("the flush finished")
                .expect("the exporter task is alive to acknowledge it");
        }
        drop(queue);
        exporter.await.unwrap();
        let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let warned = logged.matches("failed unexpectedly").count();
        assert_eq!(warned, 1, "five panics inside one interval: {logged}");
    }

    /// A rejection that is not transient is not retried.
    #[tokio::test]
    async fn a_bad_request_is_not_retried() {
        let (addr, mut requests) = receiver(vec!["HTTP/1.1 400 Bad Request".into()]).await;
        let mut sender = sender_for(
            format!("http://{addr}/v1/traces"),
            "",
            Duration::from_secs(5),
        );
        assert_eq!(
            sender.send(&mut one_span(), None).await,
            Some(SendOutcome::Rejected { status: 400 })
        );
        assert!(requests.try_recv().is_ok());
        assert!(requests.try_recv().is_err());
    }

    /// A.1: a redirect is a failed export. The location it names receives
    /// neither the request nor the configured header.
    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        let elsewhere = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let elsewhere_addr = elsewhere.local_addr().unwrap();
        for status in ["307 Temporary Redirect", "308 Permanent Redirect"] {
            let (addr, mut requests) = receiver(vec![format!(
                "HTTP/1.1 {status}\r\nlocation: http://{elsewhere_addr}/v1/traces"
            )])
            .await;
            let mut sender = sender_for(
                format!("http://{addr}/v1/traces"),
                "x-secret=SENTINEL-HEADER-VALUE",
                Duration::from_secs(5),
            );
            let outcome = sender.send(&mut one_span(), None).await;
            let code: u16 = status[..3].parse().unwrap();
            assert_eq!(outcome, Some(SendOutcome::Rejected { status: code }));
            let first = requests.try_recv().unwrap();
            assert!(first.head.contains("x-secret: sentinel-header-value"));
            assert!(requests.try_recv().is_err(), "not retried either");
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(500), elsewhere.accept())
                .await
                .is_err(),
            "the redirect target was never contacted"
        );
    }

    async fn export_one_run(protocol: &str) -> (String, Vec<u8>, String) {
        let (addr, mut requests) = receiver(vec![OK.into()]).await;
        let telemetry = Arc::new(Telemetry::start(&env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", &format!("http://{addr}")),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", protocol),
            ("OTEL_EXPORTER_OTLP_HEADERS", "x-api-key=k%20v"),
        ])));
        assert!(telemetry.is_exporting());
        let added = begin(&telemetry, run_start("p1"), &[]);
        let attrs = super::super::resource::parse_resource_attributes(
            get(&added, "OTEL_RESOURCE_ATTRIBUTES").unwrap(),
        );
        let run_id = attrs
            .iter()
            .find(|(k, _)| k == "deck.run.id")
            .unwrap()
            .1
            .clone();
        telemetry.observe_client_input("p1", b"\x1b[I");
        telemetry.observe_client_input("p1", b"y\r");
        telemetry.shutdown(Duration::from_secs(10)).await;
        let request = tokio::time::timeout(Duration::from_secs(10), requests.recv())
            .await
            .expect("the exporter posted within the budget")
            .unwrap();
        (request.head, request.body, run_id)
    }

    #[tokio::test]
    async fn exports_protobuf_to_the_traces_path_with_headers() {
        let (head, body, run_id) = export_one_run("http/protobuf").await;
        assert!(head.starts_with("post /v1/traces "), "{head}");
        assert!(head.contains("content-type: application/x-protobuf"));
        assert!(head.contains("x-api-key: k v"));
        let contains = |needle: &[u8]| body.windows(needle.len()).any(|w| w == needle);
        assert!(
            contains(run_id.as_bytes()),
            "the agent's run id is the span's"
        );
        assert!(contains(b"daemon_shutdown"));
        assert!(contains(b"agent.run"));
    }

    #[tokio::test]
    async fn exports_json_when_asked() {
        let (head, body, run_id) = export_one_run("http/json").await;
        assert!(head.contains("content-type: application/json"));
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let spans = &v["resourceSpans"][0]["scopeSpans"][0]["spans"];
        let run = &spans[0];
        assert_eq!(run["name"], "agent.run");
        let attr = |key: &str| {
            run["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|kv| kv["key"] == key)
                .map(|kv| kv["value"].clone())
        };
        assert_eq!(attr("deck.run.id").unwrap()["stringValue"], run_id);
        assert_eq!(attr("deck.interventions.count").unwrap()["intValue"], "1");
        assert_eq!(
            attr("deck.run.end_reason").unwrap()["stringValue"],
            "daemon_shutdown"
        );
    }

    // ---- A.4: the never-sent rule, through the exporter ----

    /// A `MakeWriter` collecting log output.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Sentinels in everything a run's input has — event fields, a valid pull
    /// request report, human input, the spawn's command, title and
    /// orchestration directory, the working directory and its git remote, an
    /// environment variable and the OTLP header — go through the whole path:
    /// spawn preparation, the broadcast, the ingest task, the git probes, the
    /// exporter (one failed request, one delivered) and its logs. None comes
    /// out in a request body's bytes, in a request head other than the
    /// header's own line, or in a log line the telemetry module wrote — over
    /// either protocol. The delivered body, decoded, carries the pull request's
    /// number and the repository's name, so the path really was taken.
    #[cfg(unix)]
    #[tokio::test]
    async fn no_sentinel_reaches_a_request_or_a_log_line_over_json() {
        no_sentinel_reaches_a_request_or_a_log_line("http/json").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn no_sentinel_reaches_a_request_or_a_log_line_over_protobuf() {
        no_sentinel_reaches_a_request_or_a_log_line("http/protobuf").await;
    }

    #[cfg(unix)]
    async fn no_sentinel_reaches_a_request_or_a_log_line(protocol: &str) {
        use crate::telemetry::otlp::tests::{
            SENTINEL_AGENT_ID, SENTINELS, json_values, loaded_event, protobuf_values,
        };
        use tracing_subscriber::layer::SubscriberExt;

        let logs = Captured::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(
                "dot_agent_deck::telemetry=trace",
            ))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(move || writer.clone()),
            );
        let _guard = crate::test_isolation::capture_tracing_on_this_thread(subscriber);

        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let cwd = scratch.path().join("SENTINEL-CWD");
        std::fs::create_dir_all(&cwd).unwrap();
        let git = |args: &[&str]| {
            assert!(
                crate::git_env::fixture_git(&cwd, scratch.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "--quiet"]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://SENTINEL-ENV-VALUE@example.com/SENTINEL-BRANCH/repo.git",
        ]);

        let (addr, mut requests) =
            receiver(vec!["HTTP/1.1 400 Bad Request".into(), OK.into()]).await;
        let telemetry = Arc::new(Telemetry::start(&env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", &format!("http://{addr}")),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", protocol),
            (
                "OTEL_EXPORTER_OTLP_HEADERS",
                "x-secret=SENTINEL-HEADER-VALUE",
            ),
            ("SOME_TOKEN", "SENTINEL-ENV-VALUE"),
        ])));
        let (tx, events) = broadcast::channel(64);
        telemetry.subscribe(events);
        let membership = crate::agent_pty::TabMembership::Orchestration {
            name: "tdd".into(),
            role_index: 0,
            role_name: "coder".into(),
            is_start_role: false,
            orchestration_cwd: Some("/x/SENTINEL-CWD".into()),
            display_title: Some("SENTINEL-TITLE".into()),
            orchestration_id: Some("orch-1".into()),
        };
        let start = |pane: &str| {
            RunStart::for_spawn(
                pane,
                SENTINEL_AGENT_ID,
                None,
                Some("claude --append SENTINEL-COMMAND SENTINEL-PROMPT"),
                Some(&membership),
            )
        };
        let spawn_env = vec![("SOME_TOKEN".to_string(), "SENTINEL-ENV-VALUE".to_string())];

        // Run 1: its export is rejected, so the failure is logged.
        let run = telemetry
            .prepare_run(start("pane-1"), &spawn_env, Some(cwd.clone()))
            .unwrap();
        for (k, v) in &run.env {
            for sentinel in SENTINELS {
                assert!(!v.contains(sentinel) && !k.contains(sentinel), "{k}={v}");
            }
        }
        run.commit();
        for event_type in [
            EventType::SessionStart,
            EventType::ToolStart,
            EventType::PullRequest,
        ] {
            tx.send(BroadcastMsg::Event(loaded_event("pane-1", event_type)))
                .unwrap();
        }
        telemetry.observe_client_input("pane-1", b"SENTINEL-INPUT\r");
        while telemetry.outstanding_probes() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        telemetry.end_run("pane-1", Some(SENTINEL_AGENT_ID), EndReason::Exited, None);
        telemetry.settled().await;
        let exporting = telemetry.exporting.as_ref().unwrap();
        let (ack, done) = oneshot::channel();
        exporting
            .shared
            .queue
            .send(ExportMsg::Flush {
                ack,
                deadline: Instant::now() + Duration::from_secs(5),
            })
            .await
            .unwrap();
        done.await.unwrap();

        // Run 2: delivered at shutdown.
        let run = telemetry
            .prepare_run(start("pane-2"), &spawn_env, Some(cwd.clone()))
            .unwrap();
        run.commit();
        let events: Vec<AgentEvent> = [
            EventType::SessionStart,
            EventType::PullRequest,
            EventType::Idle,
        ]
        .into_iter()
        .map(|t| loaded_event("pane-2", t))
        .collect();
        for e in events {
            tx.send(BroadcastMsg::Event(e)).unwrap();
        }
        telemetry.observe_client_input("pane-2", b"more SENTINEL-INPUT");
        while telemetry.outstanding_probes() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        telemetry.shutdown(Duration::from_secs(5)).await;

        let taken: Vec<Request> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
        assert_eq!(taken.len(), 2, "one rejected request, one delivered");
        let expected_type = match protocol {
            "http/json" => "content-type: application/json",
            _ => "content-type: application/x-protobuf",
        };
        assert!(taken[1].head.contains(expected_type), "{}", taken[1].head);
        let values = match protocol {
            "http/json" => json_values(&taken[1].body),
            _ => protobuf_values(&taken[1].body),
        };
        let has = |v: &str| values.iter().any(|x| x == v);
        assert!(
            has("4242"),
            "the valid report's number was sent: {values:?}"
        );
        assert!(has("repo"), "the repository name was resolved: {values:?}");
        for request in &taken {
            let body = &request.body;
            let head: String = request
                .head
                .lines()
                .filter(|l| !l.starts_with("x-secret:"))
                .collect::<Vec<_>>()
                .join("\n");
            for sentinel in SENTINELS {
                assert!(
                    !body
                        .windows(sentinel.len())
                        .any(|w| w == sentinel.as_bytes()),
                    "{sentinel} in a request body: {}",
                    String::from_utf8_lossy(body)
                );
                assert!(
                    !head.contains(&sentinel.to_ascii_lowercase()),
                    "{sentinel} in a request head: {head}"
                );
            }
        }
        let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(
            logged.contains("OTLP export failed"),
            "the failure path logged: {logged}"
        );
        for sentinel in SENTINELS {
            assert!(
                !logged.contains(sentinel),
                "{sentinel} in the log: {logged}"
            );
        }
        let _ = Attribute::str(AttrKey::RunId, "");
    }
}
