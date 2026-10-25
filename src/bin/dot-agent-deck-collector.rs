//! `dot-agent-deck-collector`: PRD #1859's per-machine collector. It runs
//! outside the deck, reads `/proc`, and sends each agent run's CPU and memory
//! and each worktree's disk use as OTLP metrics that join the deck's run
//! traces on `deck.run.id`. Linux only.
//!
//! The pure-data core is `dot_agent_deck::telemetry::collector`; this binary
//! holds the command line, the `/proc` reader, the worktree walks and the
//! sampling-and-export loop.

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
    use std::collections::{BTreeMap, HashMap};
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::sync::mpsc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use clap::{Parser, Subcommand};
    use dot_agent_deck::telemetry::collector::aggregate::{
        Aggregator, Environ, ProcessSample, Sample,
    };
    use dot_agent_deck::telemetry::collector::config::{Export, Settings};
    use dot_agent_deck::telemetry::collector::disk::{
        canonical_toplevel, disk_usage, git_toplevel, worktree_id,
    };
    use dot_agent_deck::telemetry::collector::identity::run_identity;
    use dot_agent_deck::telemetry::collector::otlp::{
        Request, WorktreeRef, WorktreeUsage, build_request, encode_json, encode_protobuf,
    };
    use dot_agent_deck::telemetry::collector::procfs::{ProcStat, Pss, parse_boot_time};
    use dot_agent_deck::telemetry::config::{Protocol, process_env, redacted_endpoint};
    use dot_agent_deck::telemetry::resource::host_name;

    /// A failed export is logged at most this often.
    const FAILURE_LOG_EVERY: Duration = Duration::from_secs(60);

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
        let settings = Settings::from_env(&env);
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
            Some(export) => match Sender::new(export) {
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
        runtime.block_on(run(cli, sampler, sink))
    }

    async fn run(cli: Cli, mut sampler: Sampler, mut sink: Sink) -> ExitCode {
        if cli.once {
            let request = sampler.sample_once();
            return if sink.emit(&request, true).await {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut terminate), Ok(mut interrupt)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            eprintln!("dot-agent-deck-collector: cannot install signal handlers");
            return ExitCode::FAILURE;
        };
        eprintln!(
            "dot-agent-deck-collector: sampling every {}s, {}",
            cli.interval,
            sink.describe()
        );
        let mut ticker = tokio::time::interval(Duration::from_secs(cli.interval));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let request = sampler.sample();
                    sink.emit(&request, false).await;
                }
                _ = terminate.recv() => break,
                _ = interrupt.recv() => break,
            }
        }
        let request = sampler.sample();
        sink.emit(&request, false).await;
        ExitCode::SUCCESS
    }

    fn now_unix_nanos() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }

    /// Reads `/proc`, accounts each sample and keeps the worktree walks.
    struct Sampler {
        aggregator: Aggregator,
        settings: Settings,
        host: Option<String>,
        send_paths: bool,
        disks: Option<Disks>,
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
            Ok(Sampler {
                aggregator: Aggregator::new(clk_tck, boot, now_unix_nanos()),
                settings,
                host: host_name(),
                send_paths: cli.send_worktree_paths,
                disks: (cli.disk_interval > 0)
                    .then(|| Disks::new(Duration::from_secs(cli.disk_interval))),
            })
        }

        /// One sample, with the worktrees in use walked on background threads.
        fn sample(&mut self) -> Request {
            let (sample, run_worktrees, in_use) = self.read();
            let usage = match &mut self.disks {
                Some(disks) => disks.update(&in_use),
                None => Vec::new(),
            };
            self.request(&sample, &run_worktrees, &usage)
        }

        /// One sample, with every worktree in use walked before it is sent.
        fn sample_once(&mut self) -> Request {
            let (sample, run_worktrees, in_use) = self.read();
            let usage = match &self.disks {
                Some(_) => walk_all(&in_use),
                None => Vec::new(),
            };
            self.request(&sample, &run_worktrees, &usage)
        }

        fn read(&mut self) -> (Sample, HashMap<String, WorktreeRef>, InUse) {
            let processes = read_processes(&self.aggregator);
            let sample = self.aggregator.step(now_unix_nanos(), processes);
            let (run_worktrees, in_use) = worktrees(&sample, self.send_paths);
            (sample, run_worktrees, in_use)
        }

        fn request(
            &self,
            sample: &Sample,
            run_worktrees: &HashMap<String, WorktreeRef>,
            usage: &[WorktreeUsage],
        ) -> Request {
            build_request(
                sample,
                run_worktrees,
                usage,
                &self.settings,
                self.host.as_deref(),
            )
        }
    }

    /// Every visible process, read in the order that keeps a child's reaping
    /// mid-sample least likely to be counted twice: every `stat` first, then
    /// the environments that are not settled yet, then `smaps_rollup`.
    fn read_processes(aggregator: &Aggregator) -> Vec<ProcessSample> {
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut stats = Vec::new();
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if let Ok(text) = fs::read_to_string(format!("/proc/{pid}/stat"))
                && let Some(stat) = ProcStat::parse(&text)
                && !stat.is_kernel_thread()
            {
                stats.push((pid, stat));
            }
        }
        let mut visible = Vec::with_capacity(stats.len());
        for (pid, stat) in stats {
            let environ = if aggregator.environ_settled(pid, stat.starttime) {
                Environ::NotRead
            } else {
                // A process whose environment cannot be read (another
                // user's) is not visible, and counted nowhere.
                match fs::read(format!("/proc/{pid}/environ")) {
                    Ok(bytes) => Environ::Read(run_identity(&bytes)),
                    Err(_) => continue,
                }
            };
            visible.push((pid, stat, environ));
        }
        visible
            .into_iter()
            .map(|(pid, stat, environ)| ProcessSample {
                pid,
                stat,
                environ,
                pss: fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
                    .ok()
                    .and_then(|text| Pss::parse_smaps_rollup(&text)),
            })
            .collect()
    }

    /// The worktrees in use, by canonical toplevel.
    type InUse = BTreeMap<PathBuf, WorktreeRef>;

    /// Each live run's worktree (that of its oldest process that has one),
    /// and every worktree a run-attributed process is in.
    fn worktrees(sample: &Sample, send_paths: bool) -> (HashMap<String, WorktreeRef>, InUse) {
        let mut toplevels: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
        let mut in_use = InUse::new();
        let mut run_worktrees = HashMap::new();
        for run in &sample.runs {
            for pid in &run.members {
                let Ok(cwd) = fs::read_link(format!("/proc/{pid}/cwd")) else {
                    continue;
                };
                let toplevel = toplevels
                    .entry(cwd)
                    .or_insert_with_key(|cwd| {
                        git_toplevel(cwd).and_then(|dir| fs::canonicalize(dir).ok())
                    })
                    .clone();
                let Some(toplevel) = toplevel else {
                    continue;
                };
                let worktree = in_use
                    .entry(toplevel)
                    .or_insert_with_key(|toplevel| WorktreeRef {
                        id: worktree_id(toplevel),
                        path: send_paths.then(|| toplevel.to_string_lossy().into_owned()),
                    });
                run_worktrees
                    .entry(run.identity.run_id.clone())
                    .or_insert_with(|| worktree.clone());
            }
        }
        (run_worktrees, in_use)
    }

    fn measure(toplevel: &Path, worktree: &WorktreeRef) -> Option<WorktreeUsage> {
        let bytes = disk_usage(toplevel).ok()?;
        Some(WorktreeUsage {
            worktree: worktree.clone(),
            bytes,
            measured_unix_nanos: now_unix_nanos(),
        })
    }

    /// Walk every worktree in use at once, each on its own thread.
    fn walk_all(in_use: &InUse) -> Vec<WorktreeUsage> {
        std::thread::scope(|scope| {
            let walks: Vec<_> = in_use
                .iter()
                .map(|(toplevel, worktree)| scope.spawn(move || measure(toplevel, worktree)))
                .collect();
            walks
                .into_iter()
                .filter_map(|walk| walk.join().ok().flatten())
                .collect()
        })
    }

    #[derive(Default)]
    struct DiskState {
        last: Option<WorktreeUsage>,
        started: Option<Instant>,
        walking: bool,
    }

    /// The background walks: each worktree in use is walked at most once per
    /// interval, and a request carries its most recent completed walk.
    struct Disks {
        interval: Duration,
        state: HashMap<PathBuf, DiskState>,
        done_tx: mpsc::Sender<(PathBuf, Option<WorktreeUsage>)>,
        done_rx: mpsc::Receiver<(PathBuf, Option<WorktreeUsage>)>,
    }

    impl Disks {
        fn new(interval: Duration) -> Self {
            let (done_tx, done_rx) = mpsc::channel();
            Disks {
                interval,
                state: HashMap::new(),
                done_tx,
                done_rx,
            }
        }

        fn update(&mut self, in_use: &InUse) -> Vec<WorktreeUsage> {
            while let Ok((toplevel, usage)) = self.done_rx.try_recv() {
                if let Some(state) = self.state.get_mut(&toplevel) {
                    state.walking = false;
                    if usage.is_some() {
                        state.last = usage;
                    }
                }
            }
            // A worktree no run uses any more is forgotten; a walk still
            // running for it finishes into nothing.
            self.state
                .retain(|toplevel, _| in_use.contains_key(toplevel));
            let mut out = Vec::new();
            for (toplevel, worktree) in in_use {
                let state = self.state.entry(toplevel.clone()).or_default();
                let due = state
                    .started
                    .is_none_or(|started| started.elapsed() >= self.interval);
                if !state.walking && due {
                    state.walking = true;
                    state.started = Some(Instant::now());
                    let done = self.done_tx.clone();
                    let toplevel = toplevel.clone();
                    let worktree = worktree.clone();
                    let spawned = std::thread::Builder::new()
                        .name("worktree-walk".into())
                        .spawn(move || {
                            let usage = measure(&toplevel, &worktree);
                            let _ = done.send((toplevel, usage));
                        });
                    if spawned.is_err() {
                        state.walking = false;
                    }
                }
                if let Some(last) = &state.last {
                    // The id or path may be stated differently this sample
                    // (paths switched on): send what this sample names.
                    out.push(WorktreeUsage {
                        worktree: worktree.clone(),
                        ..last.clone()
                    });
                }
            }
            out
        }
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

        /// Print or send one request; `false` when it was not delivered.
        /// `always_log` reports a failure even within a minute of the last.
        async fn emit(&mut self, request: &Request, always_log: bool) -> bool {
            match self {
                Sink::Stdout => {
                    let mut line = encode_json(request);
                    line.push(b'\n');
                    let mut out = std::io::stdout().lock();
                    out.write_all(&line).and_then(|()| out.flush()).is_ok()
                }
                Sink::Send(sender) => sender.send(request, always_log).await,
            }
        }
    }

    struct Sender {
        client: reqwest::Client,
        endpoint: String,
        redacted: String,
        json: bool,
        last_failure_log: Option<Instant>,
    }

    impl Sender {
        fn new(export: Export) -> Result<Self, reqwest::Error> {
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
                last_failure_log: None,
            })
        }

        async fn send(&mut self, request: &Request, always_log: bool) -> bool {
            let (body, content_type) = if self.json {
                (encode_json(request), "application/json")
            } else {
                (encode_protobuf(request), "application/x-protobuf")
            };
            let failure = match self
                .client
                .post(&self.endpoint)
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(body)
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => return true,
                Ok(response) => format!("HTTP {}", response.status().as_u16()),
                Err(e) if e.is_timeout() => "timed out".to_string(),
                Err(e) if e.is_connect() => "connection failed".to_string(),
                Err(_) => "request failed".to_string(),
            };
            let due = self
                .last_failure_log
                .is_none_or(|at| at.elapsed() >= FAILURE_LOG_EVERY);
            if always_log || due {
                self.last_failure_log = Some(Instant::now());
                eprintln!(
                    "dot-agent-deck-collector: export to {} failed: {failure}",
                    self.redacted
                );
            }
            false
        }
    }
}
