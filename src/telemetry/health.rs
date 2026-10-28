//! Issue #1981 M8: a periodic `daemon.health` span, the daemon's own health
//! over one interval, through the same trace pipeline as the runs.
//!
//! Each span covers the interval since the last one and says:
//!
//! - `deck.daemon.memory.rss`: the daemon's resident memory in bytes, read
//!   from `/proc/self/status` (Linux only; absent elsewhere);
//! - `deck.daemon.runs.open`: the runs the tracker holds open or prepared,
//!   one per agent PTY the deck started;
//! - `deck.daemon.hook_latency.count`, and when it is not zero `.p50_ms`,
//!   `.p95_ms` and `.max_ms`: how long the interval's hook events took from
//!   the hook process stamping them to telemetry reading them off the
//!   broadcast. Only events an agent's hooks produced count: one the daemon
//!   made itself ([`crate::event::AgentEvent::is_daemon_synthetic`]) would
//!   read about zero. An empty interval leaves the quantiles out rather than
//!   sending zeros, so an average over time is not dragged down by quiet
//!   intervals;
//! - `deck.telemetry.spans.dropped`: every span the export queue refused
//!   since the daemon started, health spans included.
//!
//! The interval is [`DEFAULT_INTERVAL`], or [`INTERVAL_ENV`] milliseconds
//! when the daemon's environment sets it to a whole number above zero.

use std::time::Duration;

use super::config::EnvMap;
use super::span::{Attribute, SpanRecord, new_span_id, new_trace_id};
use super::{AttrKey, SPAN_DAEMON_HEALTH};

/// How often a `daemon.health` span is sent.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// The daemon's environment variable overriding [`DEFAULT_INTERVAL`], in
/// milliseconds.
pub const INTERVAL_ENV: &str = "DOT_AGENT_DECK_TELEMETRY_HEALTH_INTERVAL_MS";

/// Latency samples one interval keeps. Past it the count still counts and the
/// maximum still moves; the quantiles are read from the samples kept.
pub const MAX_SAMPLES: usize = 4096;

/// The health interval `env` asks for: [`INTERVAL_ENV`] when it is a whole
/// number of milliseconds above zero, else [`DEFAULT_INTERVAL`].
pub fn interval(env: &EnvMap) -> Duration {
    env.get(INTERVAL_ENV)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_INTERVAL, Duration::from_millis)
}

/// One interval's hook latencies, from its start.
#[derive(Debug)]
pub struct Window {
    start: u64,
    count: u64,
    max_ms: u64,
    samples: Vec<u64>,
}

impl Window {
    pub fn new(start: u64) -> Self {
        Window {
            start,
            count: 0,
            max_ms: 0,
            samples: Vec::new(),
        }
    }

    /// A producer's hook event stamped at `stamped` (Unix nanoseconds) was
    /// read at `now`. A stamp after `now` reads as no delay.
    pub fn record(&mut self, stamped: u64, now: u64) {
        let ms = now.saturating_sub(stamped) / 1_000_000;
        self.count += 1;
        self.max_ms = self.max_ms.max(ms);
        if self.samples.len() < MAX_SAMPLES {
            self.samples.push(ms);
        }
    }

    /// End this interval at `now` with the measures taken then, giving its
    /// span, and start the next one.
    pub fn close(&mut self, now: u64, runs_open: usize, dropped: u64) -> SpanRecord {
        let window = std::mem::replace(self, Window::new(now));
        window.span(now, runs_open, rss_bytes(), dropped)
    }

    fn span(mut self, now: u64, runs_open: usize, rss: Option<u64>, dropped: u64) -> SpanRecord {
        let mut attributes = Vec::with_capacity(7);
        if let Some(rss) = rss {
            attributes.push(Attribute::int(AttrKey::DaemonMemoryRss, int(rss)));
        }
        attributes.push(Attribute::int(
            AttrKey::DaemonRunsOpen,
            int(u64::try_from(runs_open).unwrap_or(u64::MAX)),
        ));
        attributes.push(Attribute::int(AttrKey::HookLatencyCount, int(self.count)));
        if self.count > 0 {
            self.samples.sort_unstable();
            attributes.push(Attribute::int(
                AttrKey::HookLatencyP50,
                int(quantile(&self.samples, 50)),
            ));
            attributes.push(Attribute::int(
                AttrKey::HookLatencyP95,
                int(quantile(&self.samples, 95)),
            ));
            attributes.push(Attribute::int(AttrKey::HookLatencyMax, int(self.max_ms)));
        }
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

fn int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// This process's resident memory in bytes, where the OS says.
#[cfg(target_os = "linux")]
fn rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    rss_from_status(&status)
}

#[cfg(not(target_os = "linux"))]
fn rss_bytes() -> Option<u64> {
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

    #[test]
    fn an_empty_interval_counts_zero_and_omits_the_quantiles() {
        let span = Window::new(10).span(20, 2, Some(4096), 3);
        assert_eq!(span.name, SPAN_DAEMON_HEALTH);
        assert_eq!((span.start_unix_nanos, span.end_unix_nanos), (10, 20));
        assert_eq!(
            attr(&span, AttrKey::HookLatencyCount),
            Some(&AttrValue::Int(0))
        );
        for key in [
            AttrKey::HookLatencyP50,
            AttrKey::HookLatencyP95,
            AttrKey::HookLatencyMax,
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
        let span = window.close(2_000 * ms, 0, 0);
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
        let next = window.close(3_000 * ms, 0, 0);
        assert_eq!(next.start_unix_nanos, 2_000 * ms);
        assert_eq!(
            attr(&next, AttrKey::HookLatencyCount),
            Some(&AttrValue::Int(0))
        );
        assert_eq!(attr(&next, AttrKey::HookLatencyP50), None);
    }

    #[test]
    fn a_single_sample_is_every_quantile() {
        let mut window = Window::new(0);
        window.record(0, 1_500_000_000);
        let span = window.close(2_000_000_000, 1, 0);
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
        assert!(rss_bytes().is_some_and(|b| b > 0));
    }
}
