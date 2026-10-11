//! Measurements of the machine this process runs on — PRD #1258.
//!
//! **The one load measurement in the tree.** `machine_load_per_cpu` used to
//! exist twice, in `src/test_budget.rs` and in `tests/common/mod.rs`, and
//! issue #1245 had to teach each copy macOS separately. Both now call this
//! module, so a fix to how the load is read lands once. Keep it that way: a
//! third caller imports from here rather than reading `/proc/loadavg` itself.
//!
//! Every function here answers `None` where the platform does not publish the
//! figure cheaply. An unmeasurable figure is absent, never zero, so a consumer
//! can tell "idle" from "unknown".
//!
//! **The daemon's host sample** (PRD #1258 M1). [`HostMetricsCache`] is what the
//! daemon answers [`crate::daemon_protocol::AttachRequest::HostMetrics`] from:
//! disk free/total for three named roles, load per CPU, CPU count and memory,
//! sampled **on demand** — there is no timer — and reused for
//! [`HOST_METRICS_MAX_AGE`]. Each reply states the sample's age. The reply names
//! roles, never paths, so a client learns nothing about the host's layout.
//! The reply's shape, [`HostMetrics`], and the bounds a client holds it to
//! live in [`crate::daemon_protocol`], so a client names the reply without
//! reaching into this module.

use crate::daemon_protocol::{
    DiskUsage, HostMetrics, ROLE_TEMP_ROOT, ROLE_WORKING_ROOT, ROLE_WORKTREE_PARENT,
};

/// The 1-minute load average, or `None` where this platform does not publish
/// one cheaply.
///
/// Linux reads `/proc/loadavg` and macOS calls `getloadavg(3)`; every other
/// target answers `None`.
///
/// **macOS used to be `None` as well, and that was a flake** (issue #1244).
/// The reason given was that the `libc` crate did not expose `getloadavg` for
/// Apple targets. It does: `libc` declares it in `unix/bsd/mod.rs`, which
/// `apple` sits under. So `build-macos` — a 3-core runner under a full
/// `cargo nextest run` — got flat, unscaled wait ceilings, and
/// `idle_worker_010` failed there at 8.248 s against an 8 s ceiling (PR #1238,
/// run 35747708815): issue #709's starvation shape, on the one platform #709
/// could not scale.
#[cfg(target_os = "linux")]
pub fn one_minute_load_average() -> Option<f64> {
    let raw = std::fs::read_to_string("/proc/loadavg").ok()?;
    raw.split_whitespace().next()?.parse().ok()
}

/// See the Linux variant for the contract.
#[cfg(target_os = "macos")]
pub fn one_minute_load_average() -> Option<f64> {
    let mut sample = [0.0_f64; 1];
    // SAFETY: `getloadavg` writes at most `nelem` (1) doubles into a buffer we
    // own and have sized to 1, and returns how many it wrote, or -1 on failure.
    let written = unsafe { libc::getloadavg(sample.as_mut_ptr(), 1) };
    (written == 1).then_some(sample[0])
}

/// See the Linux variant for the contract.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn one_minute_load_average() -> Option<f64> {
    None
}

/// The 1-minute load average divided by the number of CPUs this process may
/// use, or `None` where either cannot be measured or the reading is not finite.
///
/// The test harness's and the lib tests' `load_scaled` turn this into a wait
/// multiplier; where it is `None` they apply no multiplier at all, because an
/// unmeasurable load is not evidence of a loaded machine.
pub fn machine_load_per_cpu() -> Option<f64> {
    let one_minute = one_minute_load_average()?;
    let cpus = std::thread::available_parallelism().ok()?.get() as f64;
    if !one_minute.is_finite() || cpus <= 0.0 {
        return None;
    }
    Some(one_minute / cpus)
}

/// How long one host sample is reused before a request takes a new one.
///
/// Sampling is a `statvfs` per watched role, a load read and a memory read —
/// cheap, but a client polling while its overlay is open would otherwise pay
/// for it on every redraw, and several clients would each pay separately. Two
/// seconds keeps the numbers fresher than anyone reads them while bounding the
/// cost to one sample per window however many clients ask. Every reply carries
/// the age, so a client never has to guess how stale a cache hit is.
pub const HOST_METRICS_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(2);

/// The longest a request waits for a sample being taken before it answers
/// without one.
///
/// A sample is normally a few hundred microseconds, but a `statvfs` on a hung
/// NFS or FUSE mount does not return, and no timeout cancels a syscall already
/// running. One second sits under the desktop's two-second reply timeout, so a
/// request that gives up here still reaches its client as an answer.
pub const HOST_METRICS_SAMPLE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// A sample being taken: resolves to the sample, or `None` if taking it failed.
type SampleFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<HostMetrics>> + Send>>;

/// Starts one sample. Production runs [`sample_host`] on a detached thread
/// ([`detached_sampler`]); tests inject one they can hold open.
type Sampler = std::sync::Arc<dyn Fn() -> SampleFuture + Send + Sync>;

/// A [`Sampler`] that runs `sample` on a thread of its own, detached, and
/// resolves when that thread reports.
///
/// **Not Tokio's blocking pool, on purpose** (PR #1672 review). The daemon's
/// runtime is `#[tokio::main]`, and dropping a runtime joins every blocking
/// job it spawned, with no deadline. A `statvfs` stuck on a hung NFS or FUSE
/// mount would then hold the daemon's exit — a stop, an idle shutdown or a
/// supervised restart — after every agent had already been drained. A detached
/// thread is joined by nobody: the process exits around it. The cache stays
/// single-flight either way, so a mount that never answers holds this one
/// thread and no more. A `sample` that panics drops its sender, which reads
/// as a failed sample, as a panicked blocking job did.
fn detached_sampler(sample: std::sync::Arc<dyn Fn() -> HostMetrics + Send + Sync>) -> Sampler {
    std::sync::Arc::new(move || -> SampleFuture {
        let sample = std::sync::Arc::clone(&sample);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("host-metrics-sample".into())
            .spawn(move || {
                let _ = tx.send(sample());
            });
        Box::pin(async move {
            // Dropping the handle detaches the thread.
            spawned.ok()?;
            rx.await.ok()
        })
    })
}

/// The daemon's one host sample, reused for [`HOST_METRICS_MAX_AGE`].
///
/// **Sampling is single-flight, decided on the async side** (PRD #1258 audit
/// A1). The blocking work is a `statvfs` per role, and a hung filesystem makes
/// that call never return. So:
///
/// - a request inside the max age is answered from the cache with no blocking
///   job at all;
/// - a request that finds the sample stale starts **one** refresh, unless one
///   is already in flight, and that is the only place blocking work is spawned;
/// - a request that finds a refresh already in flight answers at once from the
///   last sample, stating its true age, which may be past the max age;
/// - a request with no sample to fall back on waits for the refresh for at most
///   [`HOST_METRICS_SAMPLE_WAIT`] and then answers without one.
///
/// A refresh that never finishes therefore holds one blocking thread and no
/// more, however many requests arrive, and every request still gets an answer
/// in bounded time. The lock guards two fields and is never held across an
/// `await`, a spawn or a syscall.
///
/// Ages are measured on Tokio's clock, not `std`'s, so a paused-clock test can
/// sequence a cache hit and an expiry without sleeping (#1237's pattern). One
/// cache per attach server, so every connection to a daemon shares it.
pub struct HostMetricsCache {
    state: std::sync::Mutex<CacheState>,
    sampler: Sampler,
    wait: std::time::Duration,
}

#[derive(Default)]
struct CacheState {
    /// The last sample taken, and when it landed.
    sample: Option<(tokio::time::Instant, HostMetrics)>,
    /// Becomes `true` when the refresh in flight has finished, either way.
    in_flight: Option<tokio::sync::watch::Receiver<bool>>,
}

impl CacheState {
    /// The last sample as of `now`, with its age, however old it is.
    fn aged(&self, now: tokio::time::Instant) -> Option<HostMetrics> {
        self.sample.as_ref().map(|(taken, metrics)| HostMetrics {
            sample_age_ms: u64::try_from(now.saturating_duration_since(*taken).as_millis())
                .unwrap_or(u64::MAX),
            ..metrics.clone()
        })
    }

    fn fresh(&self, now: tokio::time::Instant) -> Option<HostMetrics> {
        let (taken, _) = self.sample.as_ref()?;
        (now.saturating_duration_since(*taken) <= HOST_METRICS_MAX_AGE)
            .then(|| self.aged(now))
            .flatten()
    }
}

impl std::fmt::Debug for HostMetricsCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostMetricsCache")
            .field("wait", &self.wait)
            .finish_non_exhaustive()
    }
}

impl Default for HostMetricsCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Clears the in-flight marker and wakes the waiters when a refresh ends,
/// including by panic or by its task being dropped, so a failed refresh never
/// leaves the cache unable to start the next one.
struct RefreshDone {
    cache: std::sync::Arc<HostMetricsCache>,
    done: tokio::sync::watch::Sender<bool>,
}

impl Drop for RefreshDone {
    fn drop(&mut self) {
        self.cache.lock().in_flight = None;
        let _ = self.done.send(true);
    }
}

impl HostMetricsCache {
    pub fn new() -> Self {
        Self::with_sampler(
            detached_sampler(std::sync::Arc::new(sample_host)),
            HOST_METRICS_SAMPLE_WAIT,
        )
    }

    fn with_sampler(sampler: Sampler, wait: std::time::Duration) -> Self {
        Self {
            state: std::sync::Mutex::default(),
            sampler,
            wait,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The host sample with its age: the cached one while it is inside
    /// [`HOST_METRICS_MAX_AGE`], else a fresh one, else — while a refresh is in
    /// flight or has failed — the last one at its true age. `None` only when no
    /// sample has ever been taken and none arrived within
    /// [`HOST_METRICS_SAMPLE_WAIT`]. See the type's doc for why.
    pub async fn read(self: &std::sync::Arc<Self>) -> Option<HostMetrics> {
        let started = tokio::time::Instant::now();
        let (mut done, start_refresh) = {
            let mut state = self.lock();
            if let Some(hit) = state.fresh(started) {
                return Some(hit);
            }
            match &state.in_flight {
                Some(in_flight) => {
                    if let Some(last) = state.aged(started) {
                        return Some(last);
                    }
                    (in_flight.clone(), None)
                }
                None => {
                    let (tx, rx) = tokio::sync::watch::channel(false);
                    state.in_flight = Some(rx.clone());
                    (rx, Some(tx))
                }
            }
        };
        if let Some(tx) = start_refresh {
            let guard = RefreshDone {
                cache: std::sync::Arc::clone(self),
                done: tx,
            };
            let sample = (self.sampler)();
            tokio::spawn(async move {
                if let Some(fresh) = sample.await {
                    // Aged from when it landed: a sample that took long to
                    // take describes the host closer to its end than its start.
                    guard.cache.lock().sample = Some((
                        tokio::time::Instant::now(),
                        HostMetrics {
                            sample_age_ms: 0,
                            ..fresh
                        },
                    ));
                }
                drop(guard);
            });
        }
        // An `Err` here is the refresh's sender dropped without a send, which
        // the guard rules out; either way the answer is what the cache holds.
        let _ = tokio::time::timeout(self.wait, done.wait_for(|finished| *finished)).await;
        self.lock().aged(tokio::time::Instant::now())
    }
}

/// Take one sample of this host, now. Blocking: a `statvfs` per role and two
/// small file reads. Callers go through [`HostMetricsCache`], which runs it on
/// a detached thread, one at a time.
pub fn sample_host() -> HostMetrics {
    #[cfg(feature = "e2e")]
    if let Some(fixed) = e2e_fixed_sample() {
        return fixed;
    }
    let (memory_used_bytes, memory_available_bytes) = read_memory();
    HostMetrics {
        disks: watched_roles()
            .into_iter()
            .map(|(role, path)| {
                let (free_bytes, total_bytes) = path
                    .as_deref()
                    .and_then(nearest_existing_ancestor)
                    .map_or((None, None), |p| disk_usage(&p));
                DiskUsage {
                    role: role.to_string(),
                    free_bytes,
                    total_bytes,
                }
            })
            .collect(),
        load_per_cpu: machine_load_per_cpu(),
        cpu_count: std::thread::available_parallelism()
            .ok()
            .and_then(|n| u32::try_from(n.get()).ok()),
        memory_used_bytes,
        memory_available_bytes,
        sampled_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_millis()).ok())
            .unwrap_or(0),
        sample_age_ms: 0,
    }
}

/// e2e seam for the docs screenshots: with the `e2e` feature, a sample given
/// as JSON in `DOT_AGENT_DECK_E2E_HOST_SAMPLE` replaces this host's, so the
/// `host-metrics` capture shows the same figures on every machine. Gated on
/// the feature rather than only on the variable, for the reason
/// `effective_current_exe` is (`src/platform/paths.rs`): a build without the
/// `e2e` feature — which is what a normally shipped release is — has no way to
/// be told to report a host it is not on. The release profile is not the gate:
/// `cargo build --release --features e2e` includes this seam.
#[cfg(feature = "e2e")]
fn e2e_fixed_sample() -> Option<HostMetrics> {
    serde_json::from_str(&std::env::var("DOT_AGENT_DECK_E2E_HOST_SAMPLE").ok()?).ok()
}

/// The three watched roles and the path each resolves to on this host.
///
/// - **`working_root`** is the daemon's startup cwd
///   ([`crate::project_resolve::daemon_startup_cwd`]): captured once by
///   `run_daemon_with`, so the answer does not move if the process's cwd does,
///   and it is the directory the deck was started from. That is often the
///   checkout its agents work in, but not necessarily: an agent's cwd is its
///   own, and a dispatch worktree follows the directory of the agent that
///   dispatched it. A daemon is not otherwise told a "project root": agent
///   cwds are per pane and come and go, and a configured `working_dir` belongs
///   to a schedule. A server started without `run_daemon_with` (a test harness)
///   never captured one, so it falls back to the process's current cwd.
/// - **`worktree_parent`** is that root's parent, where the
///   `../<repo>-dispatch-*` worktrees land (`CLAUDE.md` rule 14) when they are
///   dispatched from that root.
/// - **`temp_root`** is the e2e harness's base: `DAD_E2E_TMPDIR` when set and
///   non-empty, else `/var/tmp/dad-e2e-<uid>` on Unix — the same two rungs
///   [`crate::config_write_guard::default_test_roots`] names, read the same way.
///   The harness validates a moved base before using it; that is its concern,
///   and measuring the filesystem under it needs no such trust. Elsewhere it is
///   the OS temp dir, the harness's last rung.
///
/// A path that does not exist (yet) is measured at its nearest existing
/// ancestor, which is the filesystem a write there would land on.
fn watched_roles() -> [(&'static str, Option<std::path::PathBuf>); 3] {
    let working_root = crate::project_resolve::daemon_startup_cwd()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    let worktree_parent = working_root
        .as_deref()
        .map(|root| root.parent().unwrap_or(root).to_path_buf());
    [
        (ROLE_WORKING_ROOT, working_root),
        (ROLE_WORKTREE_PARENT, worktree_parent),
        (ROLE_TEMP_ROOT, Some(e2e_temp_root())),
    ]
}

fn e2e_temp_root() -> std::path::PathBuf {
    if let Some(moved) = std::env::var_os("DAD_E2E_TMPDIR").filter(|v| !v.is_empty()) {
        return moved.into();
    }
    #[cfg(unix)]
    {
        std::path::PathBuf::from("/var/tmp")
            .join(format!("dad-e2e-{}", crate::platform::paths::current_uid()))
    }
    #[cfg(not(unix))]
    {
        std::env::temp_dir()
    }
}

/// `path` itself if it exists, else its closest ancestor that does.
fn nearest_existing_ancestor(path: &std::path::Path) -> Option<std::path::PathBuf> {
    path.ancestors()
        .find(|candidate| !candidate.as_os_str().is_empty() && candidate.exists())
        .map(std::path::Path::to_path_buf)
}

/// `(free, total)` bytes of the filesystem holding `path`, via `statvfs(3)`.
// The field widths differ by platform (`fsblkcnt_t` is 32-bit on macOS).
#[allow(clippy::unnecessary_cast)]
#[cfg(unix)]
fn disk_usage(path: &std::path::Path) -> (Option<u64>, Option<u64>) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return (None, None);
    };
    // SAFETY: `statvfs` reads the NUL-terminated path we own and writes one
    // `struct statvfs` into the zeroed buffer we own; it returns 0 on success.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return (None, None);
    }
    disk_figures(
        stat.f_frsize as u64,
        stat.f_bavail as u64,
        stat.f_blocks as u64,
    )
}

/// `(free, total)` bytes from `statvfs`'s fragment size, available blocks and
/// total blocks.
#[cfg(any(unix, test))]
fn disk_figures(fragment: u64, available: u64, blocks: u64) -> (Option<u64>, Option<u64>) {
    let total = blocks.checked_mul(fragment).filter(|&t| t > 0);
    // Free is only a reading beside a total: with a zero fragment size both
    // products are zero, and a zero free figure there would be the "zero where
    // it is unknown" this module exists to avoid. An overflowed one is not a
    // reading either.
    let free = total.and(available.checked_mul(fragment));
    (free, total)
}

#[cfg(not(unix))]
fn disk_usage(_path: &std::path::Path) -> (Option<u64>, Option<u64>) {
    (None, None)
}

#[cfg(target_os = "linux")]
fn read_memory() -> (Option<u64>, Option<u64>) {
    read_memory_from(std::path::Path::new("/proc/meminfo"))
}

#[cfg(not(target_os = "linux"))]
fn read_memory() -> (Option<u64>, Option<u64>) {
    (None, None)
}

/// `(used, available)` bytes from a Linux `meminfo` file: `MemTotal -
/// MemAvailable`, and `MemAvailable`. Each is `None` when its inputs cannot be
/// read, independently — an unreadable `MemTotal` leaves `MemAvailable` intact
/// and makes `used` absent rather than zero.
#[cfg(target_os = "linux")]
pub(crate) fn read_memory_from(meminfo: &std::path::Path) -> (Option<u64>, Option<u64>) {
    let Ok(raw) = std::fs::read_to_string(meminfo) else {
        return (None, None);
    };
    let field = |name: &str| -> Option<u64> {
        let rest = raw
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))?;
        let mut parts = rest.split_whitespace();
        let kib: u64 = parts.next()?.parse().ok()?;
        match parts.next() {
            Some("kB") => kib.checked_mul(1024),
            _ => None,
        }
    };
    let total = field("MemTotal");
    let available = field("MemAvailable");
    let used = total
        .zip(available)
        .and_then(|(total, available)| total.checked_sub(available));
    (used, available)
}

// macOS only: see the test's own comment for why Linux has no counterpart.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// Issue #1244: macOS used to return `None` here, leaving every load-scaled
    /// ceiling unscaled on `build-macos`. `build-macos` runs this, so a
    /// regression is red there rather than a flake.
    ///
    /// macOS only, deliberately. On Linux a `None` is legitimate — a container
    /// or chroot with no readable `/proc/loadavg` is exactly the unmeasurable
    /// case the callers tolerate — and `getloadavg` has no such dependency, so
    /// only here is `None` a defect rather than an environment.
    #[test]
    fn the_load_is_measurable_on_macos() {
        let load = machine_load_per_cpu();
        assert!(
            load.is_some_and(|l| l.is_finite() && l >= 0.0),
            "machine_load_per_cpu() must measure the load here, got {load:?}"
        );
    }
}

#[cfg(test)]
mod single_flight_tests {
    use super::*;
    use spec::spec;
    use std::sync::{Arc, Mutex};
    use tokio::sync::{mpsc, oneshot};

    type Gate = Arc<Mutex<Option<oneshot::Receiver<HostMetrics>>>>;

    /// A cache whose sampler stands in for the blocking job: each start is
    /// reported on the returned channel, and the sample resolves only when the
    /// test sends through the gate it installed — or never, while the test
    /// holds the sender.
    fn stalled_cache() -> (Arc<HostMetricsCache>, mpsc::UnboundedReceiver<()>, Gate) {
        let (started_tx, started_rx) = mpsc::unbounded_channel();
        let gate: Gate = Arc::default();
        let sampler_gate = Arc::clone(&gate);
        let sampler: Sampler = Arc::new(move || -> SampleFuture {
            started_tx.send(()).unwrap();
            let gate = sampler_gate.lock().unwrap().take();
            Box::pin(async move { gate?.await.ok() })
        });
        (
            Arc::new(HostMetricsCache::with_sampler(
                sampler,
                HOST_METRICS_SAMPLE_WAIT,
            )),
            started_rx,
            gate,
        )
    }

    fn hold(gate: &Gate) -> oneshot::Sender<HostMetrics> {
        let (tx, rx) = oneshot::channel();
        *gate.lock().unwrap() = Some(rx);
        tx
    }

    fn sample(sampled_at_ms: u64) -> HostMetrics {
        HostMetrics {
            disks: Vec::new(),
            load_per_cpu: Some(0.5),
            cpu_count: Some(4),
            memory_used_bytes: None,
            memory_available_bytes: None,
            sampled_at_ms,
            sample_age_ms: 0,
        }
    }

    fn starts(started: &mut mpsc::UnboundedReceiver<()>) -> usize {
        std::iter::from_fn(|| started.try_recv().ok()).count()
    }

    /// Scenario: Eight requests reach a cold cache whose one sample never
    /// finishes; exactly one sample is started and every request answers
    /// without a sample once the bounded wait passes. After a sample lands and
    /// expires, eight more requests during a second stuck sample start one more
    /// and all answer with the last sample at its true age, while a fresh cache
    /// hit starts none.
    #[spec("protocol/host-metrics/005")]
    #[tokio::test(start_paused = true)]
    async fn protocol_host_metrics_005_stuck_sample_spawns_one_job_and_requests_return() {
        let (cache, mut started, gate) = stalled_cache();

        // Cold and stuck: one sample, and every request answers within the wait.
        let stuck = hold(&gate);
        let before = tokio::time::Instant::now();
        let mut requests = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            requests.spawn(async move { cache.read().await });
        }
        while let Some(reply) = requests.join_next().await {
            assert_eq!(reply.unwrap(), None, "no sample exists to answer with");
        }
        assert_eq!(starts(&mut started), 1, "one sample for eight requests");
        assert_eq!(
            tokio::time::Instant::now() - before,
            HOST_METRICS_SAMPLE_WAIT,
            "every request answered once the bounded wait passed"
        );

        // The stuck sample finally lands; the request waiting on it gets it.
        let waiting = {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move { cache.read().await })
        };
        tokio::task::yield_now().await;
        stuck.send(sample(1)).unwrap();
        let landed = waiting.await.unwrap().expect("the landed sample");
        assert_eq!((landed.sampled_at_ms, landed.sample_age_ms), (1, 0));
        assert_eq!(starts(&mut started), 0, "joining the refresh started none");

        // A cache hit inside the max age is served without starting a sample.
        assert_eq!(cache.read().await.map(|m| m.sampled_at_ms), Some(1));
        assert_eq!(starts(&mut started), 0, "a warm hit starts no sample");

        // Expired, and the refresh sticks: one start, and every other request
        // answers at once with the last sample at its true age.
        let expired = HOST_METRICS_MAX_AGE + std::time::Duration::from_millis(1);
        tokio::time::advance(expired).await;
        let _stuck_again = hold(&gate);
        let initiator = {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move { cache.read().await })
        };
        started.recv().await.expect("the refresh started");
        let joined_at = tokio::time::Instant::now();
        for _ in 0..7 {
            let last = cache.read().await.expect("the last sample");
            assert_eq!(last.sampled_at_ms, 1);
            assert_eq!(last.sample_age_ms, expired.as_millis() as u64);
        }
        assert_eq!(
            tokio::time::Instant::now(),
            joined_at,
            "requests during a refresh do not wait for it"
        );
        let last = initiator.await.unwrap().expect("the last sample");
        assert_eq!(last.sampled_at_ms, 1);
        assert_eq!(
            last.sample_age_ms,
            (expired + HOST_METRICS_SAMPLE_WAIT).as_millis() as u64,
            "the initiator gave up after the bounded wait and said how old its answer is"
        );
        assert_eq!(
            starts(&mut started),
            0,
            "one sample for eight more requests"
        );
    }

    /// One scripted sampler start: what the cache's next refresh does.
    type Script = Box<dyn FnOnce() -> SampleFuture + Send>;

    /// A cache that runs the queued scripts in order, one per sampler start,
    /// reporting each start on the returned channel. Its wait is an hour, so a
    /// request that returns at all was released by the refresh ending, not by
    /// giving up.
    fn scripted_cache() -> (
        Arc<HostMetricsCache>,
        mpsc::UnboundedReceiver<()>,
        Arc<Mutex<std::collections::VecDeque<Script>>>,
    ) {
        let (started_tx, started_rx) = mpsc::unbounded_channel();
        let scripts: Arc<Mutex<std::collections::VecDeque<Script>>> = Arc::default();
        let queue = Arc::clone(&scripts);
        let sampler: Sampler = Arc::new(move || -> SampleFuture {
            started_tx.send(()).unwrap();
            let next = queue
                .lock()
                .unwrap()
                .pop_front()
                .expect("a sampler start the test did not script");
            next()
        });
        let cache = HostMetricsCache::with_sampler(sampler, std::time::Duration::from_secs(3600));
        (Arc::new(cache), started_rx, scripts)
    }

    fn current_thread() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime")
    }

    /// Bounds a hang in a broken build; no assertion is made about how long a
    /// release takes.
    async fn released<T>(handle: tokio::task::JoinHandle<T>, who: &str) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(30), handle)
            .await
            .unwrap_or_else(|_| panic!("{who} was never released"))
            .unwrap_or_else(|e| panic!("{who} failed: {e}"))
    }

    /// The next sampler start, or a panic naming it: a marker left set means no
    /// start ever comes, and the test should fail rather than hang.
    async fn next_start(started: &mut mpsc::UnboundedReceiver<()>) {
        tokio::time::timeout(std::time::Duration::from_secs(30), started.recv())
            .await
            .expect("the in-flight marker was left set: no refresh started")
            .expect("the sampler is alive");
    }

    /// Scenario: A refresh whose sample panics, one whose task is dropped with
    /// the runtime it ran on, and one whose sampler panics before returning a
    /// sample each release every request waiting on it at once and leave the
    /// cache free to start the next refresh, which then answers with a real
    /// sample.
    #[spec("protocol/host-metrics/010")]
    #[test]
    fn protocol_host_metrics_010_a_refresh_that_panics_or_is_dropped_releases_waiters() {
        let (cache, mut started, scripts) = scripted_cache();
        let script = |s: Script| scripts.lock().unwrap().push_back(s);

        // 1. The sample panics on the refresh task. On a current-thread runtime
        // each spawned request runs to its wait before the test resumes, so
        // both are waiting on the refresh when the panic fires.
        let (fire, trigger) = oneshot::channel::<()>();
        script(Box::new(move || {
            Box::pin(async move {
                let _ = trigger.await;
                panic!("the sample panicked");
            })
        }));
        let rt = current_thread();
        rt.block_on(async {
            let initiator = tokio::spawn({
                let cache = Arc::clone(&cache);
                async move { cache.read().await }
            });
            next_start(&mut started).await;
            let joiner = tokio::spawn({
                let cache = Arc::clone(&cache);
                async move { cache.read().await }
            });
            tokio::task::yield_now().await;
            fire.send(()).unwrap();
            assert_eq!(released(initiator, "the initiator").await, None);
            assert_eq!(released(joiner, "the joiner").await, None);
            assert_eq!(starts(&mut started), 0, "the joiner started no refresh");
        });

        // 2. The refresh task is dropped: it lives on a runtime that shuts
        // down while a request on another runtime waits on it.
        script(Box::new(|| Box::pin(std::future::pending())));
        let doomed = current_thread();
        doomed.block_on(async {
            tokio::spawn({
                let cache = Arc::clone(&cache);
                async move { cache.read().await }
            });
            next_start(&mut started).await;
        });
        rt.block_on(async {
            let joiner = tokio::spawn({
                let cache = Arc::clone(&cache);
                async move { cache.read().await }
            });
            tokio::task::yield_now().await;
            assert_eq!(starts(&mut started), 0, "the joiner joined the refresh");
            doomed.shutdown_background();
            assert_eq!(released(joiner, "the joiner").await, None);
        });

        // 3. The sampler itself panics, inside the request that started it.
        script(Box::new(|| panic!("the sampler panicked")));
        rt.block_on(async {
            let initiator = tokio::spawn({
                let cache = Arc::clone(&cache);
                async move { cache.read().await }
            });
            let failed = tokio::time::timeout(std::time::Duration::from_secs(30), initiator)
                .await
                .expect("the request that panicked ended");
            assert!(failed.is_err_and(|e| e.is_panic()));
            assert_eq!(starts(&mut started), 1);
        });

        // Each failure left the marker clear: the next request starts a
        // refresh and gets its sample.
        script(Box::new(|| Box::pin(async { Some(sample(7)) })));
        rt.block_on(async {
            let fresh = cache.read().await.expect("the next refresh answered");
            assert_eq!(fresh.sampled_at_ms, 7);
            assert_eq!(
                starts(&mut started),
                1,
                "the next request started one refresh"
            );
        });
    }

    /// Scenario: The production sampler runs a sample that never returns, as a
    /// statvfs on a hung mount would; a request on the daemon's kind of runtime
    /// gives up after its bounded wait, and dropping that runtime then finishes
    /// while the sample is still stuck, so the stuck sample cannot hold up the
    /// daemon's exit.
    #[spec("protocol/host-metrics/011")]
    #[test]
    fn protocol_host_metrics_011_runtime_shutdown_does_not_wait_on_a_stuck_sample() {
        let (entered_tx, entered) = std::sync::mpsc::channel::<()>();
        let (release, released_rx) = std::sync::mpsc::channel::<()>();
        let released_rx = Mutex::new(released_rx);
        let stuck: Arc<dyn Fn() -> HostMetrics + Send + Sync> = Arc::new(move || {
            entered_tx.send(()).unwrap();
            // Returns once the test releases it, or once the sender is dropped
            // by a failing test, so no thread outlives the test binary's work.
            let _ = released_rx.lock().unwrap().recv();
            sample(1)
        });
        let cache = Arc::new(HostMetricsCache::with_sampler(
            detached_sampler(stuck),
            HOST_METRICS_SAMPLE_WAIT,
        ));

        // `#[tokio::main]` builds a multi-thread runtime; this is that kind.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a multi-thread runtime");
        let reply = rt.block_on({
            let cache = Arc::clone(&cache);
            async move { cache.read().await }
        });
        assert_eq!(reply, None, "the request gave up on the stuck sample");
        entered
            .recv()
            .expect("the sample started and is now stuck in its call");

        // Dropping the runtime is what the daemon's exit does. It must finish
        // while the sample is still stuck: nothing has been released yet.
        let (dropped_tx, dropped) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            drop(rt);
            let _ = dropped_tx.send(());
        });
        // Bounds a hang in a broken build; no assertion is made about how long
        // a drop takes.
        let outcome = dropped.recv_timeout(std::time::Duration::from_secs(30));
        release.send(()).unwrap();
        assert!(
            outcome.is_ok(),
            "dropping the runtime waited on the stuck sample: {outcome:?}"
        );
    }

    /// Scenario: A statvfs result with a zero fragment size yields neither a
    /// free nor a total figure, rather than a zero free figure beside an
    /// unknown total; ordinary figures multiply out.
    #[spec("protocol/host-metrics/006")]
    #[test]
    fn protocol_host_metrics_006_zero_fragment_size_leaves_free_absent() {
        assert_eq!(disk_figures(0, 10, 20), (None, None));
        assert_eq!(disk_figures(4096, 0, 20), (Some(0), Some(20 * 4096)));
        assert_eq!(
            disk_figures(4096, 10, 20),
            (Some(10 * 4096), Some(20 * 4096))
        );
        assert_eq!(disk_figures(u64::MAX, 10, 20), (None, None));
    }
}
