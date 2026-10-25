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
        MAX_RESPONSE_BODY, RETRY_INITIAL_BACKOFF, RETRY_MAX_BACKOFF, is_retryable_status, jittered,
        retry_after,
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

        /// Send `request`, and log a failure (`always_log`: even within a
        /// minute of the last).
        async fn send(&mut self, request: &Request, always_log: bool) -> Delivery {
            let (delivery, report) = self.deliver(request).await;
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
        /// timeout and the sampling interval, whichever is shorter
        /// ([`Attempts`]). Any other answer is final. Returns how it fared
        /// and, unless it was plainly delivered, what to log.
        async fn deliver(&self, request: &Request) -> (Delivery, Option<String>) {
            let (body, content_type) = if self.json {
                (encode_json(request), "application/json")
            } else {
                (encode_protobuf(request), "application/x-protobuf")
            };
            let mut attempts = Attempts::new(Instant::now(), self.timeout, self.retry_window);
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
                        // An answer that does not arrive whole in time may
                        // have rejected points nobody will know about, so it
                        // is a failure: `--once` fails and the ended runs'
                        // reports stay held. It is not retried, since the
                        // backend may have kept the request.
                        let body = match read_body(response, MAX_RESPONSE_BODY, within).await {
                            Body::Read(body) => body,
                            Body::TimedOut => {
                                break (
                                    Delivery::Failed,
                                    " failed: the backend's answer did not arrive in time"
                                        .to_string(),
                                );
                            }
                            Body::Failed => {
                                break (
                                    Delivery::Failed,
                                    " failed: the backend's answer could not be read".to_string(),
                                );
                            }
                        };
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
                            _ => return (Delivery::Delivered, None),
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
                let Some(wait) = attempts.next_wait(Instant::now(), asked, jittered) else {
                    break (Delivery::Failed, format!(" failed: {failure}"));
                };
                tokio::time::sleep(wait).await;
            };
            (delivery, Some(report))
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
        /// The body, or its first `max` bytes: an empty one is a body too.
        Read(Vec<u8>),
        TimedOut,
        /// The connection failed while the body was read.
        Failed,
    }

    /// At most `max` bytes of `response`'s body, read within `within`.
    /// Unlike the deck exporter's `bounded_body`, a body that fails or does
    /// not arrive in time is reported as such, not read as an empty one.
    async fn read_body(mut response: reqwest::Response, max: usize, within: Duration) -> Body {
        let read = async move {
            let mut body = Vec::new();
            while body.len() < max {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        let room = max - body.len();
                        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
                    }
                    Ok(None) => break,
                    Err(e) if e.is_timeout() => return Body::TimedOut,
                    Err(_) => return Body::Failed,
                }
            }
            Body::Read(body)
        };
        tokio::time::timeout(within, read)
            .await
            .unwrap_or(Body::TimedOut)
    }

    #[cfg(test)]
    mod tests {
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

        /// What a loopback backend does on one connection, after reading
        /// the whole request.
        #[derive(Clone, Copy)]
        enum Answer {
            /// Write nothing and keep the connection open.
            Hold,
            /// Write this, then keep the connection open.
            WriteThenHold(&'static str),
            /// Write this, then close the connection.
            WriteThenClose(&'static str),
        }

        /// Read one HTTP request: its head, then the `Content-Length` bytes
        /// of its body.
        async fn read_request(stream: &mut tokio::net::TcpStream) {
            let mut seen = Vec::new();
            let mut buf = [0u8; 4096];
            let head_end = loop {
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0, "the client closed before its request ended");
                seen.extend_from_slice(&buf[..n]);
                if let Some(at) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let head = String::from_utf8_lossy(&seen[..head_end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |v| v.trim().parse().unwrap());
            while seen.len() < head_end + length {
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0, "the client closed before its request ended");
                seen.extend_from_slice(&buf[..n]);
            }
        }

        /// A backend on loopback that gives its one connection `answer`.
        /// Returns its endpoint and a channel that yields once it has
        /// accepted the connection and read the whole request.
        async fn backend(answer: Answer) -> (String, mpsc::Receiver<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/v1/metrics", listener.local_addr().unwrap());
            let (ready, requested) = mpsc::channel(1);
            tokio::spawn(async move {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                read_request(&mut stream).await;
                let _ = ready.send(()).await;
                let hold = match answer {
                    Answer::Hold => true,
                    Answer::WriteThenHold(bytes) => {
                        let _ = stream.write_all(bytes.as_bytes()).await;
                        true
                    }
                    Answer::WriteThenClose(bytes) => {
                        let _ = stream.write_all(bytes.as_bytes()).await;
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
        const BOUND: Duration = Duration::from_secs(30);
        const EXPORT_TIMEOUT: Duration = Duration::from_secs(600);

        /// Send an empty request to `answer`'s backend with a one-second
        /// interval, after the backend has the request, within [`BOUND`].
        async fn deliver_to(answer: Answer) -> (Delivery, Option<String>) {
            let (endpoint, mut requested) = backend(answer).await;
            let sender = sender(endpoint, EXPORT_TIMEOUT, Duration::from_secs(1));
            let send =
                tokio::spawn(async move { sender.deliver(&Request { resources: vec![] }).await });
            tokio::time::timeout(BOUND, requested.recv())
                .await
                .expect("the backend did not get the request")
                .expect("the backend stopped");
            tokio::time::timeout(BOUND, send)
                .await
                .expect("the send outlived its deadline")
                .unwrap()
        }

        /// U3/V3 smoke test: a backend that takes the request and never
        /// answers holds the send for the sampling interval, not for the
        /// longer export timeout.
        #[tokio::test]
        async fn an_unanswered_send_ends_at_its_deadline() {
            let (delivery, report) = deliver_to(Answer::Hold).await;
            assert_eq!(delivery, Delivery::Failed);
            assert_eq!(report.as_deref(), Some(" failed: timed out"));
        }

        /// V1: a 2xx answer whose body stalls, here part-way through a
        /// partial rejection, is not delivered, and ends at the deadline.
        #[tokio::test]
        async fn a_success_whose_body_stalls_is_not_delivered() {
            const STALLED: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 200\r\n\r\n{\"partialSuccess\":{\"rejectedDataPoints\":\"1\"";
            let (delivery, report) = deliver_to(Answer::WriteThenHold(STALLED)).await;
            assert_eq!(delivery, Delivery::Failed);
            assert_eq!(
                report.as_deref(),
                Some(" failed: the backend's answer did not arrive in time")
            );
        }

        /// V1: a 2xx answer whose connection fails mid-body is not
        /// delivered, while a 2xx with an empty body is.
        #[tokio::test]
        async fn a_success_whose_body_fails_is_not_delivered() {
            const CUT: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 200\r\n\r\n{\"partialSuccess\":";
            let (delivery, report) = deliver_to(Answer::WriteThenClose(CUT)).await;
            assert_eq!(delivery, Delivery::Failed);
            assert_eq!(
                report.as_deref(),
                Some(" failed: the backend's answer could not be read")
            );

            const EMPTY: &str = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
            let (delivery, report) = deliver_to(Answer::WriteThenHold(EMPTY)).await;
            assert_eq!((delivery, report), (Delivery::Delivered, None));
        }

        /// V1: a partial rejection that arrives whole within the deadline is
        /// still reported.
        #[tokio::test]
        async fn a_partial_rejection_in_time_is_reported() {
            const REJECTED: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 45\r\n\r\n{\"partialSuccess\":{\"rejectedDataPoints\":\"1\"}}";
            let (delivery, _) = deliver_to(Answer::WriteThenHold(REJECTED)).await;
            assert_eq!(delivery, Delivery::PartiallyRejected);
        }
    }
}
