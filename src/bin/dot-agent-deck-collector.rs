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
    use std::collections::HashMap;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use clap::{Parser, Subcommand};
    use dot_agent_deck::telemetry::collector::aggregate::{Aggregator, Sample};
    use dot_agent_deck::telemetry::collector::config::{Export, Settings};
    use dot_agent_deck::telemetry::collector::disk::{
        self, DEFAULT_BUDGET, canonical_toplevel, git_toplevel, worktree_id,
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
        MAX_RESPONSE_BODY, RETRY_INITIAL_BACKOFF, RETRY_MAX_BACKOFF, bounded_body,
        is_retryable_status, jittered, retry_after,
    };
    use dot_agent_deck::telemetry::otlp::{
        decode_metrics_partial_success_json, decode_partial_success_protobuf,
    };
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
        runtime.block_on(run(cli, sampler, sink))
    }

    async fn run(cli: Cli, mut sampler: Sampler, mut sink: Sink) -> ExitCode {
        if cli.once {
            let request = sampler.sample_once();
            return if sink.emit(&request, true).await == Delivery::Delivered {
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
        let mut pending = PendingTerminals::default();
        let mut ticker = tokio::time::interval(Duration::from_secs(cli.interval));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let request = sampler.sample();
                    sink.emit_holding(request, &mut pending).await;
                }
                _ = terminate.recv() => break,
                _ = interrupt.recv() => break,
            }
        }
        let request = sampler.sample();
        sink.emit_holding(request, &mut pending).await;
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
        /// The collector's own effective user, the only one it collects
        /// without `--all-users`.
        users: Users,
        run_worktrees: RunWorktrees,
        disks: Option<Walks>,
        last_incomplete_log: Option<Instant>,
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
                disks,
                last_incomplete_log: None,
            })
        }

        /// One sample, with the worktrees in use walked in the background.
        fn sample(&mut self) -> Request {
            let (sample, in_use) = self.read();
            let usage = match &mut self.disks {
                Some(disks) => disks.update(&in_use),
                None => Vec::new(),
            };
            self.request(&sample, &usage)
        }

        /// One sample, with every worktree in use walked before it is sent.
        fn sample_once(&mut self) -> Request {
            let (sample, in_use) = self.read();
            let usage = match &mut self.disks {
                Some(disks) => disks.measure_all(&in_use, Instant::now() + DEFAULT_BUDGET.time),
                None => Vec::new(),
            };
            self.request(&sample, &usage)
        }

        fn read(&mut self) -> (Sample, InUse) {
            let processes = reader::read_processes(&self.aggregator, self.users, reader::pids());
            let sample = self.aggregator.step(now_unix_nanos(), processes);
            let (members, in_use) = worktrees(&sample, self.send_paths);
            self.run_worktrees
                .update(&sample, |pid| members.get(&pid).cloned());
            (sample, in_use)
        }

        fn request(&mut self, sample: &Sample, usage: &[WorktreeUsage]) -> Request {
            self.log_incomplete();
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
    }

    /// The worktree of each run-attributed process that is in one, by pid,
    /// and every worktree such a process is in.
    fn worktrees(sample: &Sample, send_paths: bool) -> (HashMap<u32, WorktreeRef>, InUse) {
        let mut toplevels: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
        let mut in_use = InUse::new();
        let mut members = HashMap::new();
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
                members.insert(*pid, worktree.clone());
            }
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

        /// Print or send one request.
        /// `always_log` reports a failure even within a minute of the last.
        async fn emit(&mut self, request: &Request, always_log: bool) -> Delivery {
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
                Sink::Send(sender) => sender.send(request, always_log).await,
            }
        }

        /// [`Self::emit`] in the loop: the ended runs' last reports that
        /// earlier requests failed to deliver go along, and this request's
        /// are held if it fails too. A partially rejected request is not
        /// retried, so it is not held either.
        async fn emit_holding(&mut self, mut request: Request, pending: &mut PendingTerminals) {
            pending.attach(&mut request);
            if self.emit(&request, false).await == Delivery::Failed {
                let dropped = pending.failed(&request);
                if dropped > 0 {
                    eprintln!(
                        "dot-agent-deck-collector: dropped the final figures of {dropped} ended runs held for a later export"
                    );
                }
            } else {
                pending.delivered();
            }
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

        /// Send `request`, retrying a transient failure (HTTP 429, 502, 503
        /// or 504, or the backend unreachable or slow) with jittered backoff,
        /// honouring `Retry-After`. The whole send — every attempt, the waits
        /// between them and reading the answer — ends within the export
        /// timeout and the sampling interval, whichever is shorter. Any other
        /// answer is final.
        async fn send(&mut self, request: &Request, always_log: bool) -> Delivery {
            let (body, content_type) = if self.json {
                (encode_json(request), "application/json")
            } else {
                (encode_protobuf(request), "application/x-protobuf")
            };
            let start = Instant::now();
            let deadline = start
                .checked_add(self.timeout.min(self.retry_window))
                .unwrap_or_else(|| start + Duration::from_secs(86_400));
            let mut backoff = RETRY_INITIAL_BACKOFF;
            let mut failure = "timed out".to_string();
            let (delivery, report) = loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break (Delivery::Failed, format!(" failed: {failure}"));
                }
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
                        // Delivered whatever happens next; an answer that
                        // does not arrive in time reports no rejection.
                        let body = tokio::time::timeout(
                            deadline.saturating_duration_since(Instant::now()),
                            bounded_body(response, MAX_RESPONSE_BODY),
                        )
                        .await
                        .unwrap_or_default();
                        let partial = if self.json {
                            decode_metrics_partial_success_json(&body)
                        } else {
                            decode_partial_success_protobuf(&body)
                        };
                        // The backend's own message is never logged: it can
                        // echo the configured headers' values.
                        match partial {
                            Some(p) if p.rejected > 0 => {
                                break (
                                    Delivery::PartiallyRejected,
                                    format!(
                                        ": the backend rejected {} of the request's {} data points",
                                        p.rejected,
                                        request.data_points()
                                    ),
                                );
                            }
                            Some(p) if !p.error_message.is_empty() => {
                                break (
                                    Delivery::Delivered,
                                    ": the backend accepted the request with a warning".to_string(),
                                );
                            }
                            _ => return Delivery::Delivered,
                        }
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
                // A `Retry-After` can lengthen the wait, never shorten it
                // below the backoff.
                let backoff_wait = jittered(backoff);
                let wait = asked.map_or(backoff_wait, |asked| asked.max(backoff_wait));
                if wait >= deadline.saturating_duration_since(Instant::now()) {
                    break (Delivery::Failed, format!(" failed: {failure}"));
                }
                tokio::time::sleep(wait).await;
                backoff = (backoff * 2).min(RETRY_MAX_BACKOFF);
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
    }

    #[cfg(test)]
    mod tests {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        use super::*;

        /// A backend on loopback that answers its `n`-th connection (from 0)
        /// with `answers[n]`, or holds the connection open and never answers
        /// past the end of `answers`. Returns its endpoint and how many
        /// connections it took.
        async fn backend(answers: &'static [&'static str]) -> (String, Arc<AtomicUsize>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/v1/metrics", listener.local_addr().unwrap());
            let taken = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&taken);
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(async move {
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf).await;
                        match answers.get(n) {
                            Some(answer) => {
                                let _ = stream.write_all(answer.as_bytes()).await;
                                let _ = stream.shutdown().await;
                            }
                            None => tokio::time::sleep(Duration::from_secs(60)).await,
                        }
                    });
                }
            });
            (endpoint, taken)
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

        const UNAVAILABLE: &str =
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

        /// U3: a backend that never answers holds the send for the sampling
        /// interval, not for the longer export timeout.
        #[tokio::test]
        async fn a_slow_first_attempt_ends_within_the_interval() {
            let (endpoint, taken) = backend(&[]).await;
            let mut sender = sender(
                endpoint,
                Duration::from_secs(10),
                Duration::from_millis(500),
            );
            let start = Instant::now();
            let delivery = sender.send(&Request { resources: vec![] }, false).await;
            let took = start.elapsed();
            assert_eq!(delivery, Delivery::Failed);
            assert_eq!(taken.load(Ordering::SeqCst), 1);
            assert!(took >= Duration::from_millis(450), "{took:?}");
            assert!(took < Duration::from_secs(2), "{took:?}");
        }

        /// U3: a retry after a transient answer gets only what is left of
        /// the interval, not a fresh export timeout.
        #[tokio::test]
        async fn a_slow_retry_ends_within_the_interval() {
            let (endpoint, taken) = backend(&[UNAVAILABLE]).await;
            // The first backoff is at most a second, so the retry starts
            // inside this interval.
            let mut sender = sender(endpoint, Duration::from_secs(10), Duration::from_secs(2));
            let start = Instant::now();
            let delivery = sender.send(&Request { resources: vec![] }, false).await;
            let took = start.elapsed();
            assert_eq!(delivery, Delivery::Failed);
            assert_eq!(taken.load(Ordering::SeqCst), 2, "the retry was made");
            assert!(took >= Duration::from_millis(1900), "{took:?}");
            assert!(took < Duration::from_secs(4), "{took:?}");
        }
    }
}
