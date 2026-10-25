//! `dot-agent-deck-collector`: PRD #1859's per-machine collector. It runs
//! outside the deck, reads `/proc`, and sends each agent run's CPU and memory
//! and each worktree's disk use as OTLP metrics that join the deck's run
//! traces on `deck.run.id`. Linux only.
//!
//! The pure-data core is `dot_agent_deck::telemetry::collector`; this binary
//! holds the command line and the sampling-and-export loop. In the loop, each
//! sample runs on a blocking thread, off the async runtime, and each export
//! runs alongside the wait for a signal, so neither a slow `/proc` read nor a
//! slow backend holds up the stop.

use std::process::ExitCode;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("dot-agent-deck-collector: only Linux is supported");
    ExitCode::from(3)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::{BTreeSet, HashMap};
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use bytes::Bytes;
    use clap::{Parser, Subcommand};
    use dot_agent_deck::telemetry::collector::aggregate::{Aggregator, ProcessSample, Sample};
    use dot_agent_deck::telemetry::collector::config::{Export, Settings};
    use dot_agent_deck::telemetry::collector::discovery::{self, Discovery, FIRST_SIGHT_WAIT};
    use dot_agent_deck::telemetry::collector::disk::{
        self, DEFAULT_BUDGET, canonical_toplevel, worktree_id,
    };
    use dot_agent_deck::telemetry::collector::otlp::{
        PendingTerminals, Request, RunWorktrees, WorktreeRef, WorktreeUsage, build_request,
        encode_json, encode_protobuf,
    };
    use dot_agent_deck::telemetry::collector::procfs::parse_boot_time;
    use dot_agent_deck::telemetry::collector::reader::{self, Users};
    use dot_agent_deck::telemetry::collector::walks::{InUse, MAX_OUTSTANDING, WORKERS, Walks};
    use dot_agent_deck::telemetry::config::{Protocol, process_env, redacted_endpoint};
    use dot_agent_deck::telemetry::exporter::{
        MAX_RESPONSE_BODY, RETRY_INITIAL_BACKOFF, RETRY_MAX_BACKOFF, is_retryable_status, jittered,
        retry_after,
    };
    use dot_agent_deck::telemetry::otlp::{
        decode_metrics_partial_success_json, decode_partial_success_protobuf,
    };
    use dot_agent_deck::telemetry::resource::host_name;
    use tokio::task::JoinHandle;

    /// A failed export is logged at most this often.
    const FAILURE_LOG_EVERY: Duration = Duration::from_secs(60);
    /// What shutting down at SIGTERM or SIGINT may take, counted from the
    /// stop's arrival.
    #[derive(Debug, Clone, Copy)]
    struct ShutdownBudget {
        /// How long the final sample waits for the walks queued or running,
        /// so that its request carries their figures.
        walks: Duration,
        /// When everything left ends: a sample in progress and its export, an
        /// export in progress, the final sample and the final export. What
        /// has not finished then is abandoned, rather than wait on a `/proc`
        /// read or a backend that does not answer.
        total: Duration,
    }

    const SHUTDOWN: ShutdownBudget = ShutdownBudget {
        walks: Duration::from_secs(5),
        total: Duration::from_secs(10),
    };

    /// Report each agent run's CPU and memory, and each worktree's disk use,
    /// over OTLP, from the standard OTEL_EXPORTER_OTLP_* variables.
    #[derive(Parser)]
    #[command(name = "dot-agent-deck-collector", version = env!("DAD_VERSION"))]
    struct Cli {
        /// Seconds between samples.
        #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=3600))]
        interval: u64,
        /// Seconds between walks of each worktree in use; 0 turns disk metrics off.
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(0..=86400))]
        disk_interval: u64,
        /// Take one sample, export it and exit.
        #[arg(long)]
        once: bool,
        /// Print each request as one line of OTLP/JSON instead of sending it.
        #[arg(long)]
        stdout: bool,
        /// Also send each worktree's path (deck.worktree.path).
        #[arg(long)]
        send_worktree_paths: bool,
        /// Collect every user's processes it can read, not only its own
        /// user's, and label each run with its user (deck.run.uid).
        #[arg(long)]
        all_users: bool,
        #[command(subcommand)]
        command: Option<Command>,
    }

    #[derive(Subcommand)]
    enum Command {
        /// Print the deck.worktree.id of the git worktree holding PATH.
        WorktreeId { path: PathBuf },
    }

    pub fn main() -> ExitCode {
        let cli = Cli::parse();
        if let Some(Command::WorktreeId { path }) = &cli.command {
            return match canonical_toplevel(path) {
                Ok(toplevel) => {
                    println!("{}", worktree_id(&toplevel));
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("dot-agent-deck-collector: {e}");
                    ExitCode::from(2)
                }
            };
        }
        let env = process_env();
        let mut settings = Settings::from_env(&env);
        settings.label_uids = cli.all_users;
        if let Some(value) = &settings.unknown_temporality {
            eprintln!(
                "dot-agent-deck-collector: unknown temporality preference {value:?}; using cumulative"
            );
        }
        let export = if cli.stdout {
            None
        } else {
            match Export::from_env(&env) {
                Ok(Some(export)) => Some(export),
                Ok(None) => {
                    eprintln!("dot-agent-deck-collector: metrics export is disabled; exiting");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("dot-agent-deck-collector: {e}");
                    return ExitCode::from(2);
                }
            }
        };
        let sink = match export {
            Some(export) => match Sender::new(export, Duration::from_secs(cli.interval)) {
                Ok(sender) => Sink::Send(sender),
                Err(e) => {
                    eprintln!("dot-agent-deck-collector: cannot build the OTLP client: {e}");
                    return ExitCode::FAILURE;
                }
            },
            None => Sink::Stdout,
        };
        let sampler = match Sampler::new(&cli, settings) {
            Ok(sampler) => sampler,
            Err(e) => {
                eprintln!("dot-agent-deck-collector: {e}");
                return ExitCode::FAILURE;
            }
        };
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => {
                eprintln!("dot-agent-deck-collector: {e}");
                return ExitCode::FAILURE;
            }
        };
        let code = runtime.block_on(run(cli, sampler, sink));
        // A sample abandoned at shutdown can still be blocked reading
        // `/proc`: exit without waiting for it.
        runtime.shutdown_background();
        code
    }

    async fn run(cli: Cli, mut sampler: Sampler, mut sink: Sink) -> ExitCode {
        if cli.once {
            let request = sampler.sample_once();
            return if sink.emit(&request, true, None).await == Delivery::Delivered {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        let Ok(stop) = stop_signal() else {
            eprintln!("dot-agent-deck-collector: cannot install signal handlers");
            return ExitCode::FAILURE;
        };
        eprintln!(
            "dot-agent-deck-collector: sampling every {}s, {}",
            cli.interval,
            sink.describe()
        );
        let exporter = Exporter {
            sink,
            pending: PendingTerminals::default(),
        };
        let interval = Duration::from_secs(cli.interval);
        run_loop(interval, sampler, exporter, stop, SHUTDOWN).await
    }

    /// A future that ends at the first SIGTERM or SIGINT. Both handlers are
    /// installed before it is returned, so a signal sent after that is not
    /// missed.
    fn stop_signal() -> std::io::Result<impl Future<Output = ()>> {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        Ok(async move {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = interrupt.recv() => {}
            }
        })
    }

    /// What samples: [`Sampler`], or a test's stand-in.
    trait Sampling: Send + 'static {
        /// One sample, with the worktrees in use walked in the background.
        fn sample(&mut self) -> Request;
        /// The last sample, at shutdown: [`Self::sample`], then a wait until
        /// `walks_until` for the walks queued or running, so that its
        /// request carries what they measure.
        fn sample_final(&mut self, walks_until: Instant) -> Request;
    }

    /// An export in progress, which hands the [`Exporter`] back when it ends.
    type Exporting = Pin<Box<dyn Future<Output = Exporter>>>;

    /// What the loop is doing. The sampler and the exporter are each idle
    /// or lent to the work in progress: a sample on a blocking thread, or the
    /// export of the request it returned. The next sample starts at a tick
    /// after that export has ended.
    enum InProgress<S> {
        Nothing(S, Exporter),
        Sampling(JoinHandle<(S, Request)>, Exporter),
        Exporting(S, Exporting),
    }

    /// Sample every `interval` and export each request, until `stop` ends;
    /// then [`shutdown`]. The stop is received, and its instant taken,
    /// whatever is in progress.
    async fn run_loop<S: Sampling>(
        interval: Duration,
        sampler: S,
        exporter: Exporter,
        stop: impl Future<Output = ()>,
        budget: ShutdownBudget,
    ) -> ExitCode {
        tokio::pin!(stop);
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut work = InProgress::Nothing(sampler, exporter);
        loop {
            work = match work {
                InProgress::Nothing(mut sampler, exporter) => tokio::select! {
                    _ = ticker.tick() => {
                        let sampling = tokio::task::spawn_blocking(move || {
                            let request = sampler.sample();
                            (sampler, request)
                        });
                        InProgress::Sampling(sampling, exporter)
                    }
                    () = &mut stop => {
                        let work = InProgress::Nothing(sampler, exporter);
                        return shutdown(Instant::now(), work, budget).await;
                    }
                },
                InProgress::Sampling(mut sampling, exporter) => tokio::select! {
                    joined = &mut sampling => {
                        let Ok((sampler, request)) = joined else {
                            eprintln!("dot-agent-deck-collector: a sample failed; exiting");
                            return ExitCode::FAILURE;
                        };
                        InProgress::Exporting(sampler, Box::pin(exporter.export(request, None)))
                    }
                    () = &mut stop => {
                        let work = InProgress::Sampling(sampling, exporter);
                        return shutdown(Instant::now(), work, budget).await;
                    }
                },
                InProgress::Exporting(sampler, mut exporting) => tokio::select! {
                    exporter = &mut exporting => InProgress::Nothing(sampler, exporter),
                    () = &mut stop => {
                        let work = InProgress::Exporting(sampler, exporting);
                        return shutdown(Instant::now(), work, budget).await;
                    }
                },
            };
        }
    }

    /// Shut down after a stop that arrived at `stopped`, with `work` in
    /// progress. A sample in progress is finished and its request exported,
    /// since under delta temporality its CPU is in no other request; that
    /// export, or one already in progress, runs alongside the final sample,
    /// which waits up to `budget.walks` for the walks queued or running.
    /// The final request is exported last. All of it ends within
    /// `budget.total` of `stopped`: each export's deadline is cut to it, and
    /// what has not finished then is abandoned, the final request with it.
    async fn shutdown<S: Sampling>(
        stopped: Instant,
        work: InProgress<S>,
        budget: ShutdownBudget,
    ) -> ExitCode {
        let deadline = stopped + budget.total;
        let limit = tokio::time::Instant::from_std(deadline);
        let gave_up = || {
            eprintln!(
                "dot-agent-deck-collector: shutting down did not finish within {}s of the stop; exiting without the final request",
                budget.total.as_secs()
            );
            ExitCode::SUCCESS
        };
        let (mut sampler, earlier): (S, Exporting) = match work {
            InProgress::Nothing(sampler, exporter) => {
                (sampler, Box::pin(std::future::ready(exporter)))
            }
            InProgress::Exporting(sampler, exporting) => (sampler, exporting),
            InProgress::Sampling(sampling, exporter) => {
                match tokio::time::timeout_at(limit, sampling).await {
                    Ok(Ok((sampler, request))) => {
                        (sampler, Box::pin(exporter.export(request, Some(deadline))))
                    }
                    _ => return gave_up(),
                }
            }
        };
        let walks_until = (stopped + budget.walks).min(deadline);
        let last = tokio::task::spawn_blocking(move || sampler.sample_final(walks_until));
        let (exporter, last) = tokio::join!(
            tokio::time::timeout_at(limit, earlier),
            tokio::time::timeout_at(limit, last)
        );
        let (Ok(exporter), Ok(Ok(request))) = (exporter, last) else {
            return gave_up();
        };
        exporter.export(request, Some(deadline)).await;
        ExitCode::SUCCESS
    }

    fn now_unix_nanos() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }

    /// Reads `/proc`, accounts each sample and keeps the worktree lookups
    /// and walks.
    struct Sampler {
        aggregator: Aggregator,
        settings: Settings,
        host: Option<String>,
        send_paths: bool,
        /// The collector's own effective user, the only one it collects
        /// without `--all-users`.
        users: Users,
        run_worktrees: RunWorktrees,
        /// `None` when no lookup thread could start: then no run has a
        /// worktree and no disk is walked.
        discovery: Option<Discovery>,
        disks: Option<Walks>,
        last_incomplete_log: Option<Instant>,
        last_walk_failure_log: Option<Instant>,
    }

    impl Sampler {
        fn new(cli: &Cli, settings: Settings) -> Result<Self, String> {
            let boot = fs::read_to_string("/proc/stat")
                .ok()
                .and_then(|text| parse_boot_time(&text))
                .ok_or("cannot read the boot time from /proc/stat")?;
            // SAFETY: `sysconf` reads a constant and has no preconditions.
            let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
            let clk_tck = u64::try_from(clk_tck)
                .ok()
                .filter(|t| *t > 0)
                .unwrap_or(100);
            // SAFETY: `geteuid` has no preconditions and cannot fail.
            let uid = unsafe { libc::geteuid() };
            let disks = if cli.disk_interval == 0 {
                None
            } else {
                // `--once` walks each worktree once, whatever the interval.
                let interval = if cli.once {
                    Duration::MAX
                } else {
                    Duration::from_secs(cli.disk_interval)
                };
                match Walks::new(
                    interval,
                    WORKERS,
                    MAX_OUTSTANDING,
                    DEFAULT_BUDGET,
                    Arc::new(disk::walk),
                ) {
                    Ok(walks) => Some(walks),
                    Err(e) => {
                        eprintln!(
                            "dot-agent-deck-collector: cannot start the worktree walks ({e}); disk figures are off"
                        );
                        None
                    }
                }
            };
            let discovery = match Discovery::new(
                discovery::WORKERS,
                discovery::MAX_OUTSTANDING,
                Arc::new(discovery::toplevel),
            ) {
                Ok(discovery) => Some(discovery),
                Err(e) => {
                    eprintln!(
                        "dot-agent-deck-collector: cannot start the worktree lookups ({e}); runs are reported without worktrees and disk figures are off"
                    );
                    None
                }
            };
            Ok(Sampler {
                aggregator: Aggregator::new(clk_tck, boot, now_unix_nanos()),
                settings,
                host: host_name(),
                send_paths: cli.send_worktree_paths,
                users: Users {
                    own: uid,
                    all: cli.all_users,
                },
                run_worktrees: RunWorktrees::default(),
                discovery,
                disks,
                last_incomplete_log: None,
                last_walk_failure_log: None,
            })
        }

        /// One sample, with every worktree in use looked up and walked
        /// before it is sent, all within the walk's time budget.
        fn sample_once(&mut self) -> Request {
            let deadline = Instant::now() + DEFAULT_BUDGET.time;
            let (sample, in_use) = self.read(LookupWait::Every(deadline));
            let usage = match &mut self.disks {
                Some(disks) => disks.measure_all(&in_use, deadline),
                None => Vec::new(),
            };
            self.request(&sample, &usage)
        }

        /// [`Self::read_from`] `/proc`.
        fn read(&mut self, wait: LookupWait) -> (Sample, InUse) {
            let users = self.users;
            self.read_from(
                || reader::read_processes(users, reader::pids()),
                |pid| fs::read_link(format!("/proc/{pid}/cwd")).ok(),
                wait,
            )
        }

        /// Account one sample of `processes`, and find its worktrees from
        /// each run-attributed process's working directory, `cwd_of` its
        /// pid. How long the sample waits for lookups is `wait`'s: a lookup
        /// still outstanding then is no worktree for a run seen for the
        /// first time, which keeps none (§5.4 of the design); the sample
        /// waits for nothing else.
        fn read_from(
            &mut self,
            processes: impl FnOnce() -> Vec<ProcessSample>,
            cwd_of: impl Fn(u32) -> Option<PathBuf>,
            wait: LookupWait,
        ) -> (Sample, InUse) {
            // The sample's time is taken once the processes are read, so no
            // process it holds started after it.
            let processes = processes();
            let sample = self.aggregator.step(now_unix_nanos(), processes);
            let cwds = working_directories(&sample, cwd_of);
            let (members, in_use) = match &mut self.discovery {
                Some(discovery) => {
                    let all: BTreeSet<PathBuf> = cwds.values().cloned().collect();
                    match wait {
                        LookupWait::Every(until) => discovery.look_up_all(&all, until),
                        LookupWait::FirstSight { cap } => {
                            let wanted: BTreeSet<PathBuf> = sample
                                .runs
                                .iter()
                                .filter(|run| !self.run_worktrees.is_decided(&run.key()))
                                .flat_map(|run| &run.members)
                                .filter_map(|pid| cwds.get(pid).cloned())
                                .collect();
                            // The allowance starts once the processes and
                            // their directories are read, which on a busy
                            // host can itself take longer than it.
                            let until = Instant::now() + FIRST_SIGHT_WAIT;
                            let until = cap.map_or(until, |cap| until.min(cap));
                            discovery.update(&all, &wanted, until);
                        }
                    }
                    worktrees(&cwds, discovery, self.send_paths)
                }
                None => (HashMap::new(), InUse::new()),
            };
            self.run_worktrees
                .update(&sample, |pid| members.get(&pid).cloned());
            (sample, in_use)
        }

        fn request(&mut self, sample: &Sample, usage: &[WorktreeUsage]) -> Request {
            self.log_incomplete();
            self.log_walk_failures();
            let request = build_request(
                sample,
                self.run_worktrees.get(),
                usage,
                &self.settings,
                self.host.as_deref(),
            );
            self.run_worktrees.forget_ended(sample);
            request
        }

        /// Say, at most once a minute, that a walk stopped at its limits.
        /// Never the worktree's path: only its id.
        fn log_incomplete(&mut self) {
            let Some(disks) = &mut self.disks else {
                return;
            };
            let incomplete = disks.take_incomplete();
            let Some(id) = incomplete.first() else {
                return;
            };
            if self
                .last_incomplete_log
                .is_none_or(|at| at.elapsed() >= FAILURE_LOG_EVERY)
            {
                self.last_incomplete_log = Some(Instant::now());
                eprintln!(
                    "dot-agent-deck-collector: the walk of worktree {id} stopped at the collector's limits; its disk figure is a lower bound (deck.worktree.disk.complete=false)"
                );
            }
        }

        /// Say, at most once a minute, that a walk failed and its worktree's
        /// figure is withheld. Never the worktree's path, nor the error's
        /// text: only its id and the kind of error.
        fn log_walk_failures(&mut self) {
            let Some(disks) = &mut self.disks else {
                return;
            };
            let failed = disks.take_failed();
            let Some(failure) = failed.first() else {
                return;
            };
            if self
                .last_walk_failure_log
                .is_none_or(|at| at.elapsed() >= FAILURE_LOG_EVERY)
            {
                self.last_walk_failure_log = Some(Instant::now());
                eprintln!(
                    "dot-agent-deck-collector: the walk of worktree {} failed ({}); its disk figure is withheld until a walk succeeds",
                    failure.worktree_id, failure.kind
                );
            }
        }
    }

    impl Sampling for Sampler {
        fn sample(&mut self) -> Request {
            let (sample, in_use) = self.read(LookupWait::FirstSight { cap: None });
            let usage = match &mut self.disks {
                Some(disks) => disks.update(&in_use),
                None => Vec::new(),
            };
            self.request(&sample, &usage)
        }

        fn sample_final(&mut self, walks_until: Instant) -> Request {
            let (sample, in_use) = self.read(LookupWait::FirstSight {
                cap: Some(walks_until),
            });
            let usage = match &mut self.disks {
                Some(disks) => {
                    disks.update(&in_use);
                    disks.settle(&in_use, walks_until)
                }
                None => Vec::new(),
            };
            self.request(&sample, &usage)
        }
    }

    /// How long a sample waits for its worktree lookups.
    #[derive(Debug, Clone, Copy)]
    enum LookupWait {
        /// The runs seen for the first time wait for their members' lookups,
        /// for [`FIRST_SIGHT_WAIT`] from when the lookups start, and never
        /// past `cap`.
        FirstSight { cap: Option<Instant> },
        /// Every lookup is waited for, until this instant: `--once`.
        Every(Instant),
    }

    /// Each run-attributed process's working directory, by pid, `cwd_of`
    /// its pid. In the collector that reads `/proc/<pid>/cwd`'s link, which
    /// names the directory without looking it up on its filesystem, so it
    /// cannot stall on one that does not answer.
    fn working_directories(
        sample: &Sample,
        cwd_of: impl Fn(u32) -> Option<PathBuf>,
    ) -> HashMap<u32, PathBuf> {
        sample
            .runs
            .iter()
            .flat_map(|run| &run.members)
            .filter_map(|pid| Some((*pid, cwd_of(*pid)?)))
            .collect()
    }

    /// The worktree of each process in `cwds` whose directory's lookup has
    /// answered with one, by pid, and every worktree such a process is in.
    fn worktrees(
        cwds: &HashMap<u32, PathBuf>,
        discovery: &Discovery,
        send_paths: bool,
    ) -> (HashMap<u32, WorktreeRef>, InUse) {
        let mut in_use = InUse::new();
        let mut members = HashMap::new();
        for (pid, cwd) in cwds {
            let Some(Some(toplevel)) = discovery.toplevel(cwd) else {
                continue;
            };
            let worktree = in_use
                .entry(toplevel.clone())
                .or_insert_with_key(|toplevel| WorktreeRef {
                    id: worktree_id(toplevel),
                    path: send_paths.then(|| toplevel.to_string_lossy().into_owned()),
                });
            members.insert(*pid, worktree.clone());
        }
        (members, in_use)
    }

    /// How a request fared.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Delivery {
        Delivered,
        /// Accepted, but the backend reported data points it did not take
        /// (OTLP `partial_success`). Not retried.
        PartiallyRejected,
        Failed,
    }

    enum Sink {
        Stdout,
        Send(Sender),
    }

    impl Sink {
        fn describe(&self) -> String {
            match self {
                Sink::Stdout => "printing to standard output".to_string(),
                Sink::Send(sender) => format!("sending to {}", sender.redacted),
            }
        }

        /// Print or send one request, a send ending by `until` if given.
        /// `always_log` reports a failure even within a minute of the last.
        async fn emit(
            &mut self,
            request: &Request,
            always_log: bool,
            until: Option<Instant>,
        ) -> Delivery {
            match self {
                Sink::Stdout => {
                    let mut line = encode_json(request);
                    line.push(b'\n');
                    let mut out = std::io::stdout().lock();
                    if out.write_all(&line).and_then(|()| out.flush()).is_ok() {
                        Delivery::Delivered
                    } else {
                        Delivery::Failed
                    }
                }
                Sink::Send(sender) => sender.send(request, always_log, until).await,
            }
        }
    }

    /// The loop's exports: where they go, and the ended runs' last reports
    /// held for a later one.
    struct Exporter {
        sink: Sink,
        pending: PendingTerminals,
    }

    impl Exporter {
        /// Export `request`, ending by `until` if given: the ended runs'
        /// last reports that earlier requests failed to deliver go along,
        /// and this request's are held if it fails too. A partially rejected
        /// request is not retried, so it is not held either.
        async fn export(mut self, mut request: Request, until: Option<Instant>) -> Self {
            self.pending.attach(&mut request);
            if self.sink.emit(&request, false, until).await == Delivery::Failed {
                let dropped = self.pending.failed(&request);
                if dropped > 0 {
                    eprintln!(
                        "dot-agent-deck-collector: dropped the final figures of {dropped} ended runs held for a later export"
                    );
                }
            } else {
                self.pending.delivered();
            }
            self
        }
    }

    struct Sender {
        client: reqwest::Client,
        endpoint: String,
        redacted: String,
        json: bool,
        timeout: Duration,
        /// Retries end within this long of the first attempt: the sampling
        /// interval, so a struggling backend cannot pile up samples.
        retry_window: Duration,
        last_failure_log: Option<Instant>,
    }

    impl Sender {
        fn new(export: Export, retry_window: Duration) -> Result<Self, reqwest::Error> {
            let mut headers = reqwest::header::HeaderMap::new();
            for (key, value) in &export.headers {
                match (
                    reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                    reqwest::header::HeaderValue::from_str(value),
                ) {
                    (Ok(name), Ok(value)) => {
                        headers.insert(name, value);
                    }
                    // Never echo the header: its value is often a credential.
                    _ => eprintln!(
                        "dot-agent-deck-collector: skipped an OTLP header that is not a valid HTTP header"
                    ),
                }
            }
            // No redirect is followed, so the headers reach the configured
            // endpoint and nothing else.
            let client = reqwest::Client::builder()
                .timeout(export.timeout)
                .redirect(reqwest::redirect::Policy::none())
                .default_headers(headers)
                .build()?;
            Ok(Sender {
                client,
                redacted: redacted_endpoint(&export.endpoint),
                endpoint: export.endpoint,
                json: export.protocol == Protocol::HttpJson,
                timeout: export.timeout,
                retry_window,
                last_failure_log: None,
            })
        }

        /// Send `request`, ending by `until` if given, and log a failure
        /// (`always_log`: even within a minute of the last).
        async fn send(
            &mut self,
            request: &Request,
            always_log: bool,
            until: Option<Instant>,
        ) -> Delivery {
            let (delivery, report) = self.deliver(request, until).await;
            let Some(report) = report else {
                return delivery;
            };
            let due = self
                .last_failure_log
                .is_none_or(|at| at.elapsed() >= FAILURE_LOG_EVERY);
            if always_log || due {
                self.last_failure_log = Some(Instant::now());
                eprintln!(
                    "dot-agent-deck-collector: export to {}{report}",
                    self.redacted
                );
            }
            delivery
        }

        /// Send `request`, retrying a transient failure (HTTP 429, 502, 503
        /// or 504, or the backend unreachable or slow) with jittered backoff,
        /// honouring `Retry-After`. The whole send — every attempt, the waits
        /// between them and reading the answer — ends within the export
        /// timeout and the sampling interval, whichever is shorter, and by
        /// `until` when given ([`Attempts`]). Any other answer is final.
        /// Returns how it fared and, unless it was plainly delivered, what
        /// to log.
        async fn deliver(
            &self,
            request: &Request,
            until: Option<Instant>,
        ) -> (Delivery, Option<String>) {
            let (body, content_type) = if self.json {
                (encode_json(request), "application/json")
            } else {
                (encode_protobuf(request), "application/x-protobuf")
            };
            let mut attempts =
                Attempts::new(Instant::now(), self.timeout, self.retry_window).until(until);
            let mut failure = "timed out".to_string();
            let (delivery, report) = loop {
                let Some(remaining) = attempts.remaining(Instant::now()) else {
                    break (Delivery::Failed, format!(" failed: {failure}"));
                };
                let result = self
                    .client
                    .post(&self.endpoint)
                    .timeout(remaining)
                    .header(reqwest::header::CONTENT_TYPE, content_type)
                    .body(body.clone())
                    .send()
                    .await;
                let asked = match result {
                    Ok(response) if response.status().is_success() => {
                        let within = attempts.remaining(Instant::now()).unwrap_or_default();
                        let body = read_body(response, MAX_RESPONSE_BODY, within).await;
                        return answered(body, self.json, request.data_points());
                    }
                    Ok(response) if is_retryable_status(response.status().as_u16()) => {
                        failure = format!("HTTP {}", response.status().as_u16());
                        retry_after(&response)
                    }
                    Ok(response) => {
                        break (
                            Delivery::Failed,
                            format!(" failed: HTTP {}", response.status().as_u16()),
                        );
                    }
                    Err(e) if e.is_timeout() => {
                        failure = "timed out".to_string();
                        None
                    }
                    Err(e) if e.is_connect() => {
                        failure = "connection failed".to_string();
                        None
                    }
                    Err(_) => break (Delivery::Failed, " failed: request failed".to_string()),
                };
                let Some(wait) = attempts.next_wait(Instant::now(), asked, jittered) else {
                    break (Delivery::Failed, format!(" failed: {failure}"));
                };
                tokio::time::sleep(wait).await;
            };
            (delivery, Some(report))
        }
    }

    /// How a send fared from its 2xx answer's `body`, and, unless it was
    /// plainly delivered, what to log. `json`: the protocol the answer is
    /// in; `data_points`: how many the request carried.
    fn answered(body: Body, json: bool, data_points: usize) -> (Delivery, Option<String>) {
        // An answer that does not arrive whole in time, or is too large to
        // read whole, may have rejected points nobody will know about, so it
        // is a failure: `--once` fails and the ended runs' reports stay
        // held. It is not retried, since the backend may have kept the
        // request.
        let failed = |why: &str| {
            (
                Delivery::Failed,
                Some(format!(" failed: the backend's answer {why}")),
            )
        };
        let body = match body {
            Body::Read(body) => body,
            Body::TimedOut => return failed("did not arrive in time"),
            Body::Failed => return failed("could not be read"),
            Body::TooLarge => return failed("was too large to read"),
        };
        let partial = if json {
            decode_metrics_partial_success_json(&body)
        } else {
            decode_partial_success_protobuf(&body)
        };
        // The backend's own message is never logged: it can echo the
        // configured headers' values.
        match partial {
            Some(p) if p.rejected > 0 => (
                Delivery::PartiallyRejected,
                Some(format!(
                    ": the backend rejected {} of the request's {data_points} data points",
                    p.rejected
                )),
            ),
            Some(p) if !p.error_message.is_empty() => (
                Delivery::Delivered,
                Some(": the backend accepted the request with a warning".to_string()),
            ),
            _ => (Delivery::Delivered, None),
        }
    }

    /// The deadline and the waits of one send, kept apart from the clock
    /// and the network so that they are decided from given instants.
    #[derive(Debug)]
    struct Attempts {
        deadline: Instant,
        backoff: Duration,
    }

    impl Attempts {
        /// A send that starts at `start` ends within `timeout` and
        /// `interval`, whichever is shorter.
        fn new(start: Instant, timeout: Duration, interval: Duration) -> Self {
            let deadline = start
                .checked_add(timeout.min(interval))
                .unwrap_or_else(|| start + Duration::from_secs(86_400));
            Attempts {
                deadline,
                backoff: RETRY_INITIAL_BACKOFF,
            }
        }

        /// The same, ending by `cap` when it is earlier: the shutdown's
        /// deadline.
        fn until(mut self, cap: Option<Instant>) -> Self {
            if let Some(cap) = cap {
                self.deadline = self.deadline.min(cap);
            }
            self
        }

        /// What is left of the deadline at `now`; `None` once nothing is.
        fn remaining(&self, now: Instant) -> Option<Duration> {
            Some(self.deadline.saturating_duration_since(now)).filter(|left| !left.is_zero())
        }

        /// The wait before the next attempt, from `now`: the backoff as
        /// `jitter` scales it, lengthened, never shortened, by the
        /// `Retry-After` the backend `asked` for. `None` when that wait
        /// would not end before the deadline, so it is not started. The
        /// backoff then doubles, up to `RETRY_MAX_BACKOFF`.
        fn next_wait(
            &mut self,
            now: Instant,
            asked: Option<Duration>,
            jitter: impl FnOnce(Duration) -> Duration,
        ) -> Option<Duration> {
            let backoff = jitter(self.backoff);
            let wait = asked.map_or(backoff, |asked| asked.max(backoff));
            if wait >= self.deadline.saturating_duration_since(now) {
                return None;
            }
            self.backoff = (self.backoff * 2).min(RETRY_MAX_BACKOFF);
            Some(wait)
        }
    }

    /// How reading a 2xx answer's body ended.
    #[derive(Debug, PartialEq, Eq)]
    enum Body {
        /// The whole body, its end seen: an empty one is a body too.
        Read(Vec<u8>),
        /// More than the bytes the collector keeps arrived.
        TooLarge,
        /// The body's end did not arrive in time.
        TimedOut,
        /// The connection failed while the body was read.
        Failed,
    }

    /// What one read of a body yields. `Data` is the chunk as the
    /// transport handed it over, not yet copied anywhere.
    #[derive(Debug)]
    enum Chunk {
        Data(Bytes),
        End,
        TimedOut,
        Failed,
    }

    /// A body's chunks, in the order they arrive: a response's, or a
    /// test's script.
    trait Chunks {
        fn next_chunk(&mut self) -> impl Future<Output = Chunk> + Send;
    }

    impl Chunks for reqwest::Response {
        async fn next_chunk(&mut self) -> Chunk {
            match self.chunk().await {
                Ok(Some(chunk)) => Chunk::Data(chunk),
                Ok(None) => Chunk::End,
                Err(e) if e.is_timeout() => Chunk::TimedOut,
                Err(_) => Chunk::Failed,
            }
        }
    }

    /// The whole body `chunks` yields, read within `within`, when it is at
    /// most `max` bytes. A body is whole once its end is seen, so one that
    /// fills `max` exactly is read on until its end arrives, and fails when
    /// that end stalls or breaks; one that goes past `max`, inside a chunk
    /// or with a later one, is `TooLarge`. The body accumulated here never
    /// holds more than `max` bytes: each chunk's size is checked before any
    /// of it is copied in. The chunk itself is the transport's, buffered
    /// however the transport reads, and is not bounded by `max`. Unlike the
    /// deck exporter's `bounded_body`, a body that fails or does not arrive
    /// in time is reported as such, not read as an empty one.
    async fn read_body(mut chunks: impl Chunks, max: usize, within: Duration) -> Body {
        let read = async move {
            let mut body = Vec::new();
            loop {
                match chunks.next_chunk().await {
                    Chunk::Data(chunk) if chunk.len() > max - body.len() => return Body::TooLarge,
                    Chunk::Data(chunk) => body.extend_from_slice(&chunk),
                    Chunk::End => return Body::Read(body),
                    Chunk::TimedOut => return Body::TimedOut,
                    Chunk::Failed => return Body::Failed,
                }
            }
        };
        tokio::time::timeout(within, read)
            .await
            .unwrap_or(Body::TimedOut)
    }

    #[cfg(test)]
    mod tests {
        use dot_agent_deck::telemetry::collector::config::Temporality;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio::sync::mpsc;

        use super::*;

        /// The jitter at its largest, so a wait is the backoff itself.
        fn unjittered(backoff: Duration) -> Duration {
            backoff
        }

        /// U3/V3: one send ends within the export timeout and the sampling
        /// interval, whichever is shorter.
        #[test]
        fn the_deadline_is_the_shorter_of_timeout_and_interval() {
            let start = Instant::now();
            let s = Duration::from_secs;
            let by_interval = Attempts::new(start, s(10), s(2));
            assert_eq!(by_interval.remaining(start), Some(s(2)));
            assert_eq!(by_interval.remaining(start + s(1)), Some(s(1)));
            assert_eq!(by_interval.remaining(start + s(2)), None);
            assert_eq!(by_interval.remaining(start + s(3)), None);
            let by_timeout = Attempts::new(start, s(1), s(15));
            assert_eq!(by_timeout.remaining(start), Some(s(1)));
            assert_eq!(by_timeout.remaining(start + s(1)), None);
        }

        /// U3/V3: a wait that would end at or past the deadline is not
        /// started, and the backoff doubles up to its cap.
        #[test]
        fn a_wait_past_the_deadline_is_not_started() {
            let start = Instant::now();
            let s = Duration::from_secs;
            let mut attempts = Attempts::new(start, s(10), s(2));
            assert_eq!(attempts.next_wait(start, None, unjittered), Some(s(1)));
            // 2 s more from 1 s in ends at 3 s, past the 2 s deadline.
            assert_eq!(attempts.next_wait(start + s(1), None, unjittered), None);
            // Ending exactly at the deadline is not started either.
            assert_eq!(attempts.next_wait(start, None, unjittered), None);
            let mut long = Attempts::new(start, s(600), s(600));
            let waits: Vec<_> = (0..5)
                .map(|_| long.next_wait(start, None, unjittered).unwrap())
                .collect();
            assert_eq!(waits, [s(1), s(2), s(4), s(5), s(5)]);
            // The jitter applies to the backoff.
            let mut jittered = Attempts::new(start, s(600), s(600));
            assert_eq!(
                jittered.next_wait(start, None, |b| b / 2),
                Some(Duration::from_millis(500))
            );
        }

        /// U3/V3: `Retry-After` can lengthen a wait, never shorten it, and
        /// a wait it lengthens past the deadline is not started.
        #[test]
        fn retry_after_only_lengthens_a_wait() {
            let start = Instant::now();
            let s = Duration::from_secs;
            let mut attempts = Attempts::new(start, s(60), s(60));
            assert_eq!(
                attempts.next_wait(start, Some(s(0)), unjittered),
                Some(s(1))
            );
            assert_eq!(
                attempts.next_wait(start, Some(s(1)), unjittered),
                Some(s(2))
            );
            assert_eq!(
                attempts.next_wait(start, Some(s(30)), unjittered),
                Some(s(30))
            );
            assert_eq!(attempts.next_wait(start, Some(s(60)), unjittered), None);
            assert_eq!(
                attempts.next_wait(start + s(31), Some(s(30)), unjittered),
                None
            );
        }

        /// A body that yields these chunks, in order, and then nothing
        /// more: a body whose end never arrives.
        struct Script(std::collections::VecDeque<Chunk>);

        impl Chunks for Script {
            async fn next_chunk(&mut self) -> Chunk {
                match self.0.pop_front() {
                    Some(chunk) => chunk,
                    None => std::future::pending().await,
                }
            }
        }

        /// Far longer than any body below takes to read: a read that ends
        /// within it ended on its chunks, not on the time.
        const GENEROUS: Duration = Duration::from_secs(30);
        /// For a body whose end never arrives: nothing has to happen within
        /// it, so it can be short.
        const STALL: Duration = Duration::from_millis(20);
        const CAP: usize = 8;

        async fn read(chunks: impl IntoIterator<Item = Chunk>, within: Duration) -> Body {
            read_body(Script(chunks.into_iter().collect()), CAP, within).await
        }

        fn data(bytes: &[u8]) -> Chunk {
            Chunk::Data(Bytes::copy_from_slice(bytes))
        }

        /// V1/W1: a body is read whole once its end arrives, an empty one
        /// and one that fills the cap exactly included.
        #[tokio::test]
        async fn a_body_is_read_whole_once_its_end_arrives() {
            let whole = read([data(b"abc"), data(b""), data(b"de"), Chunk::End], GENEROUS);
            assert_eq!(whole.await, Body::Read(b"abcde".to_vec()));
            assert_eq!(read([Chunk::End], GENEROUS).await, Body::Read(vec![]));
            let full = read([data(b"abcd"), data(b"efgh"), Chunk::End], GENEROUS);
            assert_eq!(full.await, Body::Read(b"abcdefgh".to_vec()));
        }

        /// W1: a body that fills the cap and then stalls or breaks before
        /// its end is not read whole, and neither is one that stops short of
        /// the cap.
        #[tokio::test]
        async fn a_body_that_stalls_or_breaks_at_the_cap_is_not_read() {
            assert_eq!(read([data(b"abcdefgh")], STALL).await, Body::TimedOut);
            let broken = read([data(b"abcdefgh"), Chunk::Failed], GENEROUS);
            assert_eq!(broken.await, Body::Failed);
            let timed_out = read([data(b"abcdefgh"), Chunk::TimedOut], GENEROUS);
            assert_eq!(timed_out.await, Body::TimedOut);
            assert_eq!(read([data(b"abc")], STALL).await, Body::TimedOut);
            assert_eq!(
                read([data(b"abc"), Chunk::Failed], GENEROUS).await,
                Body::Failed
            );
        }

        /// W1: a body that goes past the cap is too large, whether it does
        /// so inside one chunk or with a chunk after the cap is full, and
        /// whatever follows.
        #[tokio::test]
        async fn a_body_past_the_cap_is_too_large() {
            assert_eq!(read([data(b"abcdefghi")], GENEROUS).await, Body::TooLarge);
            let across = read([data(b"abcdefg"), data(b"hi"), Chunk::End], GENEROUS);
            assert_eq!(across.await, Body::TooLarge);
            let after = read([data(b"abcdefgh"), data(b"i")], GENEROUS);
            assert_eq!(after.await, Body::TooLarge);
        }

        const REJECTION: &str = "{\"partialSuccess\":{\"rejectedDataPoints\":\"1\"}}";

        /// V1/W1: how each way a 2xx answer's body ends decides the send.
        #[test]
        fn a_send_fares_as_its_answer_reads() {
            let failed = |why: &str| {
                (
                    Delivery::Failed,
                    Some(format!(" failed: the backend's answer {why}")),
                )
            };
            assert_eq!(
                answered(Body::Read(vec![]), true, 3),
                (Delivery::Delivered, None)
            );
            assert_eq!(
                answered(Body::Read(REJECTION.into()), true, 3),
                (
                    Delivery::PartiallyRejected,
                    Some(": the backend rejected 1 of the request's 3 data points".to_string())
                )
            );
            let warned = answered(
                Body::Read(b"{\"partialSuccess\":{\"errorMessage\":\"slow down\"}}".to_vec()),
                true,
                3,
            );
            assert_eq!(
                warned,
                (
                    Delivery::Delivered,
                    Some(": the backend accepted the request with a warning".to_string())
                )
            );
            assert_eq!(
                answered(Body::TimedOut, true, 3),
                failed("did not arrive in time")
            );
            assert_eq!(answered(Body::Failed, true, 3), failed("could not be read"));
            assert_eq!(
                answered(Body::TooLarge, true, 3),
                failed("was too large to read")
            );
        }

        /// W1: a partial rejection too large to read whole fails the send
        /// rather than counting as delivered.
        #[tokio::test]
        async fn an_oversized_partial_rejection_is_not_delivered() {
            let mut answer = REJECTION.trim_end_matches("}}").to_string();
            answer.push_str(",\"errorMessage\":\"");
            answer.push_str(&"x".repeat(MAX_RESPONSE_BODY));
            answer.push_str("\"}}");
            let body = read_body(
                Script([data(answer.as_bytes()), Chunk::End].into()),
                MAX_RESPONSE_BODY,
                GENEROUS,
            )
            .await;
            assert_eq!(body, Body::TooLarge);
            assert_eq!(answered(body, true, 1).0, Delivery::Failed);
        }

        /// What a loopback backend does on one connection, after reading
        /// the whole request.
        #[derive(Clone)]
        enum Answer {
            /// Write nothing and keep the connection open.
            Hold,
            /// Write this, then keep the connection open.
            WriteThenHold(Vec<u8>),
            /// Write this, then close the connection.
            WriteThenClose(Vec<u8>),
        }

        /// Read one HTTP request: its head, then the `Content-Length` bytes
        /// of its body, which it returns. `None` when the client closed or
        /// the connection failed before the request ended, or its head was
        /// not one this reads: a sender that gave up early is not this
        /// backend's failure.
        async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<Vec<u8>> {
            let mut seen = Vec::new();
            let mut buf = [0u8; 4096];
            let head_end = loop {
                match stream.read(&mut buf).await {
                    Ok(n) if n > 0 => seen.extend_from_slice(&buf[..n]),
                    _ => return None,
                }
                if let Some(at) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let head = String::from_utf8_lossy(&seen[..head_end]).to_ascii_lowercase();
            let length = match head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map(|v| v.trim().parse::<usize>())
            {
                None => 0,
                Some(Ok(length)) => length,
                Some(Err(_)) => return None,
            };
            while seen.len() < head_end + length {
                match stream.read(&mut buf).await {
                    Ok(n) if n > 0 => seen.extend_from_slice(&buf[..n]),
                    _ => return None,
                }
            }
            Some(seen[head_end..head_end + length].to_vec())
        }

        /// A backend on loopback that gives its one connection `answer`.
        /// Returns its endpoint and a channel that yields once it has
        /// accepted the connection and read the whole request, which it does
        /// before writing any of `answer`. A request that never arrives
        /// whole gets no answer, and the channel never yields.
        async fn backend(answer: Answer) -> (String, mpsc::Receiver<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/v1/metrics", listener.local_addr().unwrap());
            let (ready, requested) = mpsc::channel(1);
            tokio::spawn(async move {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                if read_request(&mut stream).await.is_none() {
                    return;
                }
                let _ = ready.send(()).await;
                let hold = match answer {
                    Answer::Hold => true,
                    Answer::WriteThenHold(bytes) => {
                        let _ = stream.write_all(&bytes).await;
                        true
                    }
                    Answer::WriteThenClose(bytes) => {
                        let _ = stream.write_all(&bytes).await;
                        false
                    }
                };
                if hold {
                    tokio::time::sleep(Duration::from_secs(600)).await;
                }
            });
            (endpoint, requested)
        }

        fn sender(endpoint: String, timeout: Duration, interval: Duration) -> Sender {
            let export = Export {
                endpoint,
                headers: Vec::new(),
                protocol: Protocol::HttpJson,
                timeout,
            };
            Sender::new(export, interval).unwrap()
        }

        /// Far past every deadline below, and far short of the export
        /// timeout: a send that ends within it was bounded by its deadline.
        const BOUND: Duration = Duration::from_secs(120);
        const EXPORT_TIMEOUT: Duration = Duration::from_secs(600);

        /// Send an empty request to `answer`'s backend with an `interval`
        /// deadline, within [`BOUND`]. Returns how the send fared, and
        /// whether the backend had read the whole request by the time it
        /// ended: it reads it before writing any answer, so a send that
        /// read an answer finds it had.
        async fn send_to(answer: Answer, interval: Duration) -> ((Delivery, Option<String>), bool) {
            let (endpoint, mut requested) = backend(answer).await;
            let sender = sender(endpoint, EXPORT_TIMEOUT, interval);
            let fared =
                tokio::time::timeout(BOUND, sender.deliver(&Request { resources: vec![] }, None))
                    .await
                    .expect("the send outlived its deadline");
            (fared, requested.try_recv().is_ok())
        }

        /// [`send_to`] for a test whose outcome comes from the answer's body,
        /// so the backend must have had the request.
        async fn deliver_to(answer: Answer, interval: Duration) -> (Delivery, Option<String>) {
            let (fared, requested) = send_to(answer, interval).await;
            assert!(requested, "the backend did not get the request: {fared:?}");
            fared
        }

        /// A backend on loopback that answers every request with an empty
        /// 200, on as many connections as it is given, and yields each
        /// request's body.
        async fn recording_backend() -> (String, mpsc::UnboundedReceiver<Vec<u8>>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/v1/metrics", listener.local_addr().unwrap());
            let (bodies, received) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let bodies = bodies.clone();
                    tokio::spawn(async move {
                        while let Some(body) = read_request(&mut stream).await {
                            let _ = bodies.send(body);
                            let answer = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
                            if stream.write_all(answer).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            });
            (endpoint, received)
        }

        const CLK_TCK: u64 = 100;
        const BOOT: u64 = 1_700_000_000;
        const RUN: &str = "scripted-run";

        fn settings(temporality: Temporality) -> Settings {
            Settings {
                service_name: "dot-agent-deck-collector".into(),
                resource_pairs: Vec::new(),
                temporality,
                unknown_temporality: None,
                label_uids: false,
            }
        }

        /// One process of the run `run_id`, having used `ticks` of CPU.
        fn process(pid: u32, run_id: &str, ticks: u64) -> ProcessSample {
            use dot_agent_deck::telemetry::collector::aggregate::Environ;
            use dot_agent_deck::telemetry::collector::identity::RunIdentity;
            use dot_agent_deck::telemetry::collector::procfs::ProcStat;
            ProcessSample {
                pid,
                uid: 1000,
                stat: ProcStat {
                    ppid: 1,
                    flags: 0,
                    utime: ticks,
                    stime: 0,
                    cutime: 0,
                    cstime: 0,
                    starttime: 0,
                },
                environ: Environ::Read(Some(RunIdentity {
                    run_id: run_id.into(),
                    attributes: Vec::new(),
                })),
                pss: None,
            }
        }

        /// A stand-in sampler: a real [`Aggregator`] and real requests, over
        /// one process of [`RUN`] that has used one more second of CPU at
        /// each sample, with no `/proc` read and nothing to wait for.
        struct Scripted {
            aggregator: Aggregator,
            settings: Settings,
            ticks: u64,
            time: u64,
        }

        impl Scripted {
            fn new(temporality: Temporality) -> Self {
                let started = BOOT * 1_000_000_000;
                Scripted {
                    aggregator: Aggregator::new(CLK_TCK, BOOT, started),
                    settings: settings(temporality),
                    ticks: 0,
                    time: started,
                }
            }

            fn next(&mut self) -> Request {
                self.ticks += CLK_TCK;
                self.time += 1_000_000_000;
                let sample = self
                    .aggregator
                    .step(self.time, vec![process(4242, RUN, self.ticks)]);
                build_request(&sample, &HashMap::new(), &[], &self.settings, None)
            }
        }

        impl Sampling for Scripted {
            fn sample(&mut self) -> Request {
                self.next()
            }

            fn sample_final(&mut self, _walks_until: Instant) -> Request {
                self.next()
            }
        }

        fn exporter_to(endpoint: String) -> Exporter {
            Exporter {
                sink: Sink::Send(sender(endpoint, EXPORT_TIMEOUT, EXPORT_TIMEOUT)),
                pending: PendingTerminals::default(),
            }
        }

        /// Each `deck.run.cpu.time` point of [`RUN`] in an OTLP/JSON body:
        /// its temporality and value.
        fn run_cpu_points(body: &[u8]) -> Vec<(i64, f64)> {
            let request: serde_json::Value = serde_json::from_slice(body).unwrap();
            let mut points = Vec::new();
            for resource in request["resourceMetrics"].as_array().unwrap() {
                let ours = resource["resource"]["attributes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| a["key"] == "deck.run.id" && a["value"]["stringValue"] == RUN);
                if !ours {
                    continue;
                }
                for scope in resource["scopeMetrics"].as_array().unwrap() {
                    for metric in scope["metrics"].as_array().unwrap() {
                        if metric["name"] != "deck.run.cpu.time" {
                            continue;
                        }
                        let sum = &metric["sum"];
                        let temporality = sum["aggregationTemporality"].as_i64().unwrap();
                        for point in sum["dataPoints"].as_array().unwrap() {
                            points.push((temporality, point["asDouble"].as_f64().unwrap()));
                        }
                    }
                }
            }
            points
        }

        /// N2: under delta temporality, a sample in progress when the stop
        /// comes is exported along with the final one, so the run's delta
        /// points still add up to all the CPU it used: none of its intervals
        /// is left out.
        #[tokio::test]
        async fn a_sample_in_progress_at_the_stop_is_exported_too() {
            let (endpoint, mut received) = recording_backend().await;
            let exporter = exporter_to(endpoint);
            let mut sampler = Scripted::new(Temporality::Delta);
            let first = sampler.sample();
            let exporter = exporter.export(first, None).await;
            let sampling = tokio::task::spawn_blocking(move || {
                let request = sampler.sample();
                (sampler, request)
            });
            let budget = ShutdownBudget {
                walks: Duration::ZERO,
                total: GENEROUS,
            };
            let work = InProgress::Sampling(sampling, exporter);
            tokio::time::timeout(BOUND, shutdown(Instant::now(), work, budget))
                .await
                .expect("the shutdown outlived its bound");
            let mut points = Vec::new();
            while let Ok(body) = received.try_recv() {
                points.extend(run_cpu_points(&body));
            }
            assert_eq!(points.len(), 3, "{points:?}");
            assert!(points.iter().all(|(temporality, _)| *temporality == 1));
            let total: f64 = points.iter().map(|(_, value)| value).sum();
            assert!((total - 3.0).abs() < 1e-9, "{points:?}");
        }

        /// N4: the final export ends at the shutdown's deadline when the
        /// backend takes the request and never answers, however long the
        /// export timeout and the interval allow.
        #[tokio::test]
        async fn the_final_export_ends_within_the_shutdown_budget() {
            let (endpoint, mut requested) = backend(Answer::Hold).await;
            let budget = ShutdownBudget {
                walks: Duration::from_millis(100),
                total: Duration::from_secs(2),
            };
            let work = InProgress::Nothing(
                Scripted::new(Temporality::Cumulative),
                exporter_to(endpoint),
            );
            let stopped = Instant::now();
            tokio::time::timeout(BOUND, shutdown(stopped, work, budget))
                .await
                .expect("the shutdown outlived its bound");
            let took = stopped.elapsed();
            assert!(took < budget.total + Duration::from_secs(1), "{took:?}");
            assert!(
                requested.try_recv().is_ok(),
                "the final request was never sent"
            );
        }

        /// N4, with a real signal: a SIGTERM that arrives while a regular
        /// export waits on a backend that never answers is received then,
        /// and the collector's loop ends within the shutdown budget of it,
        /// not when the export would have given up.
        #[tokio::test]
        async fn a_signal_during_an_export_ends_the_loop_within_the_budget() {
            let (endpoint, mut requested) = backend(Answer::Hold).await;
            let budget = ShutdownBudget {
                walks: Duration::from_millis(100),
                total: Duration::from_secs(2),
            };
            let stop = stop_signal().unwrap();
            let signalled = tokio::spawn(async move {
                requested
                    .recv()
                    .await
                    .expect("the backend never got the first request");
                let at = Instant::now();
                // SAFETY: `kill` has no memory-safety preconditions; the
                // SIGTERM handler is installed, so it does not end the test.
                assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGTERM) }, 0);
                at
            });
            let looped = run_loop(
                EXPORT_TIMEOUT,
                Scripted::new(Temporality::Cumulative),
                exporter_to(endpoint),
                stop,
                budget,
            );
            tokio::time::timeout(BOUND, looped)
                .await
                .expect("the loop outlived its bound");
            let took = signalled.await.unwrap().elapsed();
            assert!(took < budget.total + Duration::from_secs(1), "{took:?}");
        }

        /// N4: the shutdown's deadline cuts a send's own, never lengthens it.
        #[test]
        fn a_cap_only_shortens_a_sends_deadline() {
            let start = Instant::now();
            let s = Duration::from_secs;
            let capped = Attempts::new(start, s(10), s(15)).until(Some(start + s(3)));
            assert_eq!(capped.remaining(start), Some(s(3)));
            let later = Attempts::new(start, s(10), s(15)).until(Some(start + s(30)));
            assert_eq!(later.remaining(start), Some(s(10)));
            let none = Attempts::new(start, s(10), s(15)).until(None);
            assert_eq!(none.remaining(start), Some(s(10)));
        }

        /// N3: the first-sight allowance starts once the processes are read,
        /// so a read slower than the allowance still leaves a lookup that
        /// answers at once time to decide a new run's worktree.
        #[test]
        fn the_first_sight_wait_starts_after_a_slow_process_read() {
            let tmp = tempfile::tempdir().unwrap();
            fs::create_dir(tmp.path().join(".git")).unwrap();
            let cli = Cli::parse_from(["dot-agent-deck-collector", "--disk-interval", "0"]);
            let mut sampler = Sampler::new(&cli, settings(Temporality::Cumulative)).unwrap();
            let cwd = tmp.path().to_path_buf();
            let (sample, _) = sampler.read_from(
                || {
                    std::thread::sleep(FIRST_SIGHT_WAIT + Duration::from_millis(500));
                    vec![process(4242, RUN, CLK_TCK)]
                },
                |_| Some(cwd.clone()),
                LookupWait::FirstSight { cap: None },
            );
            let expected = worktree_id(&fs::canonicalize(tmp.path()).unwrap());
            let run = sample.runs[0].key();
            let decided = sampler.run_worktrees.get().get(&run).map(|w| w.id.clone());
            assert_eq!(decided, Some(expected));
        }

        /// A sample is timed once its processes are read: a run that
        /// started during a slow read is no later than the sample, so its
        /// first CPU interval is not inverted.
        #[test]
        fn a_run_started_during_a_slow_read_is_not_after_the_sample() {
            let cli = Cli::parse_from(["dot-agent-deck-collector", "--disk-interval", "0"]);
            let mut sampler = Sampler::new(&cli, settings(Temporality::Cumulative)).unwrap();
            sampler.aggregator = Aggregator::new(CLK_TCK, BOOT, now_unix_nanos());
            let started = std::cell::Cell::new(0);
            let (sample, _) = sampler.read_from(
                || {
                    std::thread::sleep(Duration::from_millis(100));
                    let now = now_unix_nanos();
                    started.set(now);
                    std::thread::sleep(Duration::from_millis(100));
                    let mut p = process(4242, RUN, CLK_TCK);
                    p.stat.starttime = (now - BOOT * 1_000_000_000) * CLK_TCK / 1_000_000_000;
                    vec![p]
                },
                |_| None,
                LookupWait::FirstSight { cap: None },
            );
            let start = sample.runs[0].figures.start_unix_nanos;
            assert!(start <= sample.time_unix_nanos, "{start} > {sample:#?}");
            assert!(
                sample.time_unix_nanos >= started.get(),
                "the sample was timed before its processes were read: {sample:#?}"
            );
        }

        /// A 200 answer with `body`, declaring `length` bytes of it.
        fn ok(length: usize, body: &[u8]) -> Vec<u8> {
            let mut answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {length}\r\n\r\n"
            )
            .into_bytes();
            answer.extend_from_slice(body);
            answer
        }

        /// U3/V3 smoke test: a backend that takes the request and never
        /// answers holds the send for the sampling interval, not for the
        /// longer export timeout. Whatever stage the one-second deadline
        /// ends it in, connecting, sending or awaiting the answer, it fails
        /// as timed out, so whether the backend got the request is not
        /// asked.
        #[tokio::test]
        async fn an_unanswered_send_ends_at_its_deadline() {
            let ((delivery, report), _) = send_to(Answer::Hold, Duration::from_secs(1)).await;
            assert_eq!(delivery, Delivery::Failed);
            assert_eq!(report.as_deref(), Some(" failed: timed out"));
        }

        /// V1/W1: a 2xx answer whose connection closes before its body
        /// ends, part-way through or with exactly the bytes the collector
        /// keeps, is not delivered.
        #[tokio::test]
        async fn a_success_whose_body_is_cut_short_is_not_delivered() {
            let cut = ok(200, b"{\"partialSuccess\":");
            let at_cap = ok(MAX_RESPONSE_BODY + 1, &vec![b' '; MAX_RESPONSE_BODY]);
            for answer in [cut, at_cap] {
                let (delivery, report) = deliver_to(Answer::WriteThenClose(answer), GENEROUS).await;
                assert_eq!(delivery, Delivery::Failed);
                assert_eq!(
                    report.as_deref(),
                    Some(" failed: the backend's answer could not be read")
                );
            }
        }

        /// V1: a 2xx answer with an empty body is delivered, and a partial
        /// rejection that arrives whole is reported.
        #[tokio::test]
        async fn whole_answers_are_delivered_or_reported() {
            let empty = Answer::WriteThenHold(ok(0, b""));
            assert_eq!(
                deliver_to(empty, GENEROUS).await,
                (Delivery::Delivered, None)
            );
            let rejected = Answer::WriteThenHold(ok(REJECTION.len(), REJECTION.as_bytes()));
            let (delivery, _) = deliver_to(rejected, GENEROUS).await;
            assert_eq!(delivery, Delivery::PartiallyRejected);
        }
    }
}
