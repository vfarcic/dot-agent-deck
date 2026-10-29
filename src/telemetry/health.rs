//! Issue #1981 M8: a periodic `daemon.health` span, the daemon's own health
//! over one interval, through the same trace pipeline as the runs.
//!
//! Each span covers the interval since the last one and says:
//!
//! - `deck.daemon.memory.rss`: the daemon's resident memory in bytes, read
//!   from `/proc/self/status` on the blocking pool as the interval ends
//!   (Linux only; absent elsewhere);
//! - `deck.daemon.runs.open`: the runs the tracker holds open or prepared,
//!   one per agent PTY the deck started;
//! - `deck.daemon.hook_latency.count`, and when it is not zero `.p50_ms`,
//!   `.p95_ms` and `.max_ms`: how long the interval's hook events took from
//!   the hook process stamping them to telemetry reading them off the
//!   broadcast. Only events an agent's hooks produced count: one the daemon
//!   made itself ([`crate::event::AgentEvent::is_daemon_synthetic`]) would
//!   read about zero. The stamp is the producer's own clock, so this is best
//!   effort; a stamp after the read counts as no delay. The count and the
//!   maximum cover every event; past [`MAX_SAMPLES`] events the quantiles
//!   are estimated from a uniform sample of the whole interval. An empty
//!   interval leaves the quantiles out rather than sending zeros, so an
//!   average over time is not dragged down by quiet intervals;
//! - `deck.daemon.attach_latency.count`, and when it is not zero `.p50_ms`,
//!   `.p95_ms` and `.max_ms`: how long the interval's attaches took, from the
//!   daemon taking a client's request to stream a pane to it having written
//!   the pane's current screen, the snapshot replay, to the client. Sampled
//!   and summarised as the hook latencies are;
//! - `deck.telemetry.spans.dropped`: every span the export queue refused
//!   since the daemon started, health spans included.
//!
//! The interval is [`DEFAULT_INTERVAL`], or [`INTERVAL_ENV`] milliseconds
//! when the daemon's environment sets it to a whole number above zero, raised
//! to [`MIN_INTERVAL`].

use std::time::Duration;

use super::config::EnvMap;
use super::span::{Attribute, SpanRecord, new_span_id, new_trace_id};
use super::{AttrKey, SPAN_DAEMON_HEALTH};

/// How often a `daemon.health` span is sent.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// The daemon's environment variable overriding [`DEFAULT_INTERVAL`], in
/// milliseconds.
pub const INTERVAL_ENV: &str = "DOT_AGENT_DECK_TELEMETRY_HEALTH_INTERVAL_MS";

/// The shortest interval [`INTERVAL_ENV`] can ask for; a shorter one is
/// raised to it.
pub const MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Latency samples one interval keeps, per measure: a uniform sample of every
/// event in it (Algorithm R), so the quantiles describe the whole interval
/// however many events it had. The count and the maximum are exact.
pub const MAX_SAMPLES: usize = 4096;

/// The health interval `env` asks for: [`INTERVAL_ENV`] when it is a whole
/// number of milliseconds above zero, at least [`MIN_INTERVAL`], else
/// [`DEFAULT_INTERVAL`].
pub fn interval(env: &EnvMap) -> Duration {
    env.get(INTERVAL_ENV)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_INTERVAL, |ms| {
            Duration::from_millis(ms).max(MIN_INTERVAL)
        })
}

/// One interval's latencies of one kind: how many, the slowest, and a
/// uniform sample of them.
#[derive(Debug, Default)]
struct Latencies {
    count: u64,
    max_ms: u64,
    samples: Vec<u64>,
}

impl Latencies {
    /// One more latency of `ms` milliseconds, `rng` choosing which kept
    /// sample it replaces once [`MAX_SAMPLES`] are kept.
    fn record(&mut self, ms: u64, rng: &mut XorShift) {
        self.count += 1;
        self.max_ms = self.max_ms.max(ms);
        if self.samples.len() < MAX_SAMPLES {
            self.samples.push(ms);
            return;
        }
        // Algorithm R: the `count`th event replaces a kept sample with
        // probability MAX_SAMPLES / count, so every event of the interval is
        // equally likely to be kept.
        let slot = rng.next() % self.count;
        if let Some(kept) = usize::try_from(slot)
            .ok()
            .and_then(|i| self.samples.get_mut(i))
        {
            *kept = ms;
        }
    }

    /// The count, and when it is not zero the median, 95th percentile and
    /// maximum, under the four keys given.
    fn attributes(mut self, [count, p50, p95, max]: [AttrKey; 4]) -> Vec<Attribute> {
        let mut attributes = vec![Attribute::int(count, int(self.count))];
        if self.count > 0 {
            self.samples.sort_unstable();
            attributes.push(Attribute::int(p50, int(quantile(&self.samples, 50))));
            attributes.push(Attribute::int(p95, int(quantile(&self.samples, 95))));
            attributes.push(Attribute::int(max, int(self.max_ms)));
        }
        attributes
    }
}

/// One interval's hook and attach latencies, from its start.
#[derive(Debug)]
pub struct Window {
    start: u64,
    hooks: Latencies,
    attaches: Latencies,
    /// Chooses which sample a later event replaces.
    rng: XorShift,
}

impl Window {
    pub fn new(start: u64) -> Self {
        Self::seeded(start, random_seed())
    }

    fn seeded(start: u64, seed: u64) -> Self {
        Window {
            start,
            hooks: Latencies::default(),
            attaches: Latencies::default(),
            rng: XorShift::new(seed),
        }
    }

    /// A producer's hook event stamped at `stamped` (Unix nanoseconds) was
    /// read at `now`. A stamp after `now` reads as no delay.
    pub fn record(&mut self, stamped: u64, now: u64) {
        let ms = now.saturating_sub(stamped) / 1_000_000;
        self.hooks.record(ms, &mut self.rng);
    }

    /// Issue #1981 M8: an attach took `took`, from the daemon taking the
    /// request to it having written the pane's screen to the client.
    pub fn record_attach(&mut self, took: Duration) {
        let ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
        self.attaches.record(ms, &mut self.rng);
    }

    /// End this interval at `now` with the measures taken then (`rss` from
    /// [`rss_bytes`]), giving its span, and start the next one.
    pub fn close(
        &mut self,
        now: u64,
        runs_open: usize,
        rss: Option<u64>,
        dropped: u64,
    ) -> SpanRecord {
        self.cut(now).span(now, runs_open, rss, dropped)
    }

    /// End this interval at `now` and start the next one, giving the ended
    /// one, whose span [`Self::span`] makes once the measures taken at its
    /// end are in. What is recorded after the cut goes to the next interval,
    /// however long those measures take.
    pub fn cut(&mut self, now: u64) -> Window {
        let next = Window::seeded(now, self.rng.next());
        std::mem::replace(self, next)
    }

    /// This ended interval's span, ending at `now`.
    pub fn span(self, now: u64, runs_open: usize, rss: Option<u64>, dropped: u64) -> SpanRecord {
        let mut attributes = Vec::with_capacity(11);
        if let Some(rss) = rss {
            attributes.push(Attribute::int(AttrKey::DaemonMemoryRss, int(rss)));
        }
        attributes.push(Attribute::int(
            AttrKey::DaemonRunsOpen,
            int(u64::try_from(runs_open).unwrap_or(u64::MAX)),
        ));
        attributes.extend(self.hooks.attributes([
            AttrKey::HookLatencyCount,
            AttrKey::HookLatencyP50,
            AttrKey::HookLatencyP95,
            AttrKey::HookLatencyMax,
        ]));
        attributes.extend(self.attaches.attributes([
            AttrKey::AttachLatencyCount,
            AttrKey::AttachLatencyP50,
            AttrKey::AttachLatencyP95,
            AttrKey::AttachLatencyMax,
        ]));
        attributes.push(Attribute::int(AttrKey::SpansDropped, int(dropped)));
        SpanRecord {
            trace_id: new_trace_id(),
            span_id: new_span_id(),
            parent_span_id: None,
            name: SPAN_DAEMON_HEALTH,
            start_unix_nanos: self.start,
            end_unix_nanos: now.max(self.start),
            attributes,
            events: Vec::new(),
        }
    }
}

/// The nearest-rank `pct`th percentile of `sorted`, which is not empty.
fn quantile(sorted: &[u64], pct: usize) -> u64 {
    let rank = (sorted.len() * pct).div_ceil(100).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

/// xorshift64*: enough to pick reservoir slots, not for anything secret.
#[derive(Debug)]
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        // Zero is the one state it never leaves.
        XorShift(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn random_seed() -> u64 {
    let mut seed = [0u8; 8];
    // A failed read leaves a fixed seed, which still samples uniformly.
    let _ = getrandom::fill(&mut seed);
    u64::from_le_bytes(seed)
}

fn int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// This process's resident memory in bytes, where the OS says, read on the
/// blocking pool: the caller is the telemetry ingest task, which must not
/// wait on a file read on an async worker.
pub async fn rss_bytes() -> Option<u64> {
    off_worker(read_rss).await
}

/// `read`, run on the blocking pool; `None` if it panicked.
async fn off_worker(read: fn() -> Option<u64>) -> Option<u64> {
    tokio::task::spawn_blocking(read).await.ok().flatten()
}

#[cfg(target_os = "linux")]
fn read_rss() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    rss_from_status(&status)
}

#[cfg(not(target_os = "linux"))]
fn read_rss() -> Option<u64> {
    None
}

/// `VmRSS` from a `/proc/<pid>/status` text, in bytes.
#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
fn rss_from_status(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let mut parts = line["VmRSS:".len()..].split_whitespace();
    let value: u64 = parts.next()?.parse().ok()?;
    match parts.next() {
        Some("kB") => value.checked_mul(1024),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::span::AttrValue;

    fn attr(span: &SpanRecord, key: AttrKey) -> Option<&AttrValue> {
        span.attributes
            .iter()
            .find(|a| a.key == key)
            .map(|a| &a.value)
    }

    #[test]
    fn the_interval_comes_from_the_environment_or_defaults() {
        let env = |v: &str| EnvMap::from([(INTERVAL_ENV.to_string(), v.to_string())]);
        assert_eq!(interval(&EnvMap::new()), DEFAULT_INTERVAL);
        assert_eq!(interval(&env("250")), Duration::from_millis(250));
        assert_eq!(interval(&env("0")), DEFAULT_INTERVAL);
        assert_eq!(interval(&env("soon")), DEFAULT_INTERVAL);
    }

    /// Issue #1981 M8 review: an interval shorter than [`MIN_INTERVAL`] is
    /// raised to it, so the daemon cannot be made to sample in a busy loop.
    #[test]
    fn a_too_short_interval_is_raised_to_the_minimum() {
        let env = |v: &str| EnvMap::from([(INTERVAL_ENV.to_string(), v.to_string())]);
        assert_eq!(MIN_INTERVAL, Duration::from_millis(100));
        assert_eq!(interval(&env("1")), MIN_INTERVAL);
        assert_eq!(interval(&env("99")), MIN_INTERVAL);
        assert_eq!(interval(&env("100")), MIN_INTERVAL);
        assert_eq!(interval(&env("101")), Duration::from_millis(101));
    }

    /// Issue #1981 M8 review: a burst early in an interval does not fix its
    /// quantiles. 4,096 events with no delay and then 8,192 a second late
    /// are two thirds late, so the median and the 95th percentile are the
    /// late ones; the count and the maximum stay exact.
    #[test]
    fn quantiles_sample_the_whole_interval_not_its_start() {
        let ms = 1_000_000;
        for seed in [1, 7, 0xDEAD_BEEF, u64::MAX] {
            let mut window = Window::seeded(0, seed);
            for _ in 0..MAX_SAMPLES {
                window.record(10 * ms, 10 * ms);
            }
            for _ in 0..2 * MAX_SAMPLES {
                window.record(10 * ms, 1_010 * ms);
            }
            assert_eq!(window.hooks.samples.len(), MAX_SAMPLES);
            let late = window.hooks.samples.iter().filter(|s| **s == 1000).count();
            // Two thirds of the kept samples, give or take sampling noise.
            assert!(
                (2_400..=3_050).contains(&late),
                "seed {seed}: {late} of {MAX_SAMPLES} kept samples are late"
            );
            let span = window.close(2_000 * ms, 0, None, 0);
            assert_eq!(
                attr(&span, AttrKey::HookLatencyCount),
                Some(&AttrValue::Int(3 * MAX_SAMPLES as i64))
            );
            for key in [
                AttrKey::HookLatencyP50,
                AttrKey::HookLatencyP95,
                AttrKey::HookLatencyMax,
            ] {
                assert_eq!(attr(&span, key), Some(&AttrValue::Int(1000)), "seed {seed}");
            }
        }
    }

    #[test]
    fn an_empty_interval_counts_zero_and_omits_the_quantiles() {
        let span = Window::new(10).span(20, 2, Some(4096), 3);
        assert_eq!(span.name, SPAN_DAEMON_HEALTH);
        assert_eq!((span.start_unix_nanos, span.end_unix_nanos), (10, 20));
        assert_eq!(
            attr(&span, AttrKey::HookLatencyCount),
            Some(&AttrValue::Int(0))
        );
        assert_eq!(
            attr(&span, AttrKey::AttachLatencyCount),
            Some(&AttrValue::Int(0))
        );
        for key in [
            AttrKey::HookLatencyP50,
            AttrKey::HookLatencyP95,
            AttrKey::HookLatencyMax,
            AttrKey::AttachLatencyP50,
            AttrKey::AttachLatencyP95,
            AttrKey::AttachLatencyMax,
        ] {
            assert_eq!(attr(&span, key), None);
        }
        assert_eq!(
            attr(&span, AttrKey::DaemonRunsOpen),
            Some(&AttrValue::Int(2))
        );
        assert_eq!(
            attr(&span, AttrKey::DaemonMemoryRss),
            Some(&AttrValue::Int(4096))
        );
        assert_eq!(attr(&span, AttrKey::SpansDropped), Some(&AttrValue::Int(3)));
        assert!(span.parent_span_id.is_none());
    }

    #[test]
    fn samples_give_ordered_quantiles_and_each_interval_starts_empty() {
        let ms = 1_000_000;
        let mut window = Window::new(0);
        for delay in 1..=100 {
            window.record(1_000 * ms, (1_000 + delay) * ms);
        }
        // A stamp from the future is no delay, not a negative one.
        window.record(5_000 * ms, 4_000 * ms);
        let span = window.close(2_000 * ms, 0, None, 0);
        assert_eq!(
            attr(&span, AttrKey::HookLatencyCount),
            Some(&AttrValue::Int(101))
        );
        assert_eq!(
            attr(&span, AttrKey::HookLatencyP50),
            Some(&AttrValue::Int(50))
        );
        assert_eq!(
            attr(&span, AttrKey::HookLatencyP95),
            Some(&AttrValue::Int(95))
        );
        assert_eq!(
            attr(&span, AttrKey::HookLatencyMax),
            Some(&AttrValue::Int(100))
        );
        let next = window.close(3_000 * ms, 0, None, 0);
        assert_eq!(next.start_unix_nanos, 2_000 * ms);
        assert_eq!(
            attr(&next, AttrKey::HookLatencyCount),
            Some(&AttrValue::Int(0))
        );
        assert_eq!(attr(&next, AttrKey::HookLatencyP50), None);
    }

    /// Issue #1981 M8: attaches are summarised apart from hook events, in
    /// whole milliseconds, and each interval starts with none.
    #[test]
    fn attach_latencies_are_summarised_apart_from_hooks() {
        let mut window = Window::new(0);
        for ms in 1..=100 {
            window.record_attach(Duration::from_millis(ms));
        }
        window.record_attach(Duration::from_micros(999));
        let span = window.close(2_000_000_000, 0, None, 0);
        assert_eq!(
            attr(&span, AttrKey::AttachLatencyCount),
            Some(&AttrValue::Int(101))
        );
        assert_eq!(
            attr(&span, AttrKey::AttachLatencyP50),
            Some(&AttrValue::Int(50))
        );
        assert_eq!(
            attr(&span, AttrKey::AttachLatencyP95),
            Some(&AttrValue::Int(95))
        );
        assert_eq!(
            attr(&span, AttrKey::AttachLatencyMax),
            Some(&AttrValue::Int(100))
        );
        assert_eq!(
            attr(&span, AttrKey::HookLatencyCount),
            Some(&AttrValue::Int(0)),
            "an attach is not a hook event"
        );
        let next = window.close(3_000_000_000, 0, None, 0);
        assert_eq!(
            attr(&next, AttrKey::AttachLatencyCount),
            Some(&AttrValue::Int(0))
        );
        assert_eq!(attr(&next, AttrKey::AttachLatencyMax), None);
    }

    /// Qodo review on PR #2020: an attach that finishes while the ended
    /// interval's memory is being read belongs to the next interval, not to
    /// a span that ends before it.
    #[test]
    fn an_attach_after_the_cut_counts_in_the_next_interval() {
        let mut window = Window::new(0);
        let ended = window.cut(1_000);
        window.record_attach(Duration::from_millis(5));
        let span = ended.span(1_000, 0, Some(1), 0);
        assert_eq!(
            attr(&span, AttrKey::AttachLatencyCount),
            Some(&AttrValue::Int(0))
        );
        let next = window.close(2_000, 0, None, 0);
        assert_eq!(next.start_unix_nanos, 1_000);
        assert_eq!(
            attr(&next, AttrKey::AttachLatencyCount),
            Some(&AttrValue::Int(1))
        );
    }

    #[test]
    fn a_single_sample_is_every_quantile() {
        let mut window = Window::new(0);
        window.record(0, 1_500_000_000);
        let span = window.close(2_000_000_000, 1, None, 0);
        for key in [
            AttrKey::HookLatencyP50,
            AttrKey::HookLatencyP95,
            AttrKey::HookLatencyMax,
        ] {
            assert_eq!(attr(&span, key), Some(&AttrValue::Int(1500)));
        }
    }

    #[test]
    fn rss_is_read_in_bytes_from_the_status_text() {
        let status = "Name:\tdeck\nVmPeak:\t  9000 kB\nVmRSS:\t  1234 kB\nThreads:\t4\n";
        assert_eq!(rss_from_status(status), Some(1234 * 1024));
        assert_eq!(rss_from_status("Name:\tdeck\n"), None);
        #[cfg(target_os = "linux")]
        assert!(read_rss().is_some_and(|b| b > 0));
    }

    /// Qodo review on PR #2010: the read runs on the blocking pool, not on
    /// the async worker the ingest task is polled on, and its answer comes
    /// back to the caller.
    #[tokio::test(flavor = "current_thread")]
    async fn rss_is_read_off_the_async_worker() {
        static READ_ON: std::sync::Mutex<Option<std::thread::ThreadId>> =
            std::sync::Mutex::new(None);
        fn read() -> Option<u64> {
            *READ_ON.lock().unwrap() = Some(std::thread::current().id());
            Some(7)
        }
        assert_eq!(off_worker(read).await, Some(7));
        let read_on = READ_ON.lock().unwrap().expect("the read ran");
        assert_ne!(read_on, std::thread::current().id());
        #[cfg(target_os = "linux")]
        assert!(rss_bytes().await.is_some_and(|b| b > 0));
    }
}
