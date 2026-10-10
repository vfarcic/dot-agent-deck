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

/// The role of the filesystem holding the deck's working root.
pub const ROLE_WORKING_ROOT: &str = "working_root";
/// The role of the filesystem holding the working root's parent, where sibling
/// worktrees (`../<repo>-dispatch-*`) land.
pub const ROLE_WORKTREE_PARENT: &str = "worktree_parent";
/// The role of the filesystem holding the e2e harness's temp root.
pub const ROLE_TEMP_ROOT: &str = "temp_root";

/// One watched role's filesystem. Either figure is absent when it could not be
/// read; neither is ever reported as zero in its place.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiskUsage {
    /// One of [`ROLE_WORKING_ROOT`], [`ROLE_WORKTREE_PARENT`],
    /// [`ROLE_TEMP_ROOT`]. A string rather than an enum so a client meeting a
    /// role a newer daemon added still decodes the reply.
    pub role: String,
    /// Bytes available to an unprivileged writer (`f_bavail`), which is what a
    /// build can actually use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
}

/// The daemon's answer to `host-metrics`: its own host, as of `sampled_at_ms`.
///
/// Every reading degrades on its own: a field this host cannot read is absent,
/// not zero. Memory is Linux-only today (`/proc/meminfo`); macOS reports it
/// absent rather than reaching for the Mach host-statistics calls, which the
/// `libc` crate marks deprecated in favour of a crate this project does not
/// depend on.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HostMetrics {
    #[serde(default)]
    pub disks: Vec<DiskUsage>,
    /// [`machine_load_per_cpu`]: the 1-minute load average over the CPUs this
    /// process may use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_per_cpu: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_count: Option<u32>,
    /// `MemTotal - MemAvailable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_used_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_available_bytes: Option<u64>,
    /// Wall-clock epoch milliseconds the sample was taken at. Equal across
    /// replies served from one cached sample.
    pub sampled_at_ms: u64,
    /// How old the sample was when this reply was written, in milliseconds.
    pub sample_age_ms: u64,
}

/// The longest a request waits for a sample being taken before it answers
/// without one.
///
/// A sample is normally a few hundred microseconds, but a `statvfs` on a hung
/// NFS or FUSE mount does not return, and no timeout cancels a syscall already
/// running. One second sits under the desktop's two-second reply timeout, so a
/// request that gives up here still reaches its client as an answer.
pub const HOST_METRICS_SAMPLE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// The most disk roles a client accepts in one reply. The daemon names three;
/// the headroom lets a newer daemon add roles without an older client refusing
/// the reply, while a hostile or broken one cannot make a client allocate and
/// render an unbounded list (PRD #1258 audit A2).
pub const MAX_DISK_ROLES: usize = 16;

/// The longest role name, in bytes, a client accepts. The daemon's are under
/// twenty; a role is an identifier, not text.
pub const MAX_ROLE_BYTES: usize = 64;

impl HostMetrics {
    /// Whether a reply is within [`MAX_DISK_ROLES`] and [`MAX_ROLE_BYTES`]. The
    /// client refuses one that is not, rather than handing its renderers work
    /// whose size the peer chose.
    pub fn within_reply_bounds(&self) -> bool {
        self.disks.len() <= MAX_DISK_ROLES
            && self.disks.iter().all(|d| d.role.len() <= MAX_ROLE_BYTES)
    }
}

/// A sample being taken: resolves to the sample, or `None` if taking it failed.
type SampleFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<HostMetrics>> + Send>>;

/// Starts one sample. Production runs [`sample_host`] on Tokio's blocking pool;
/// tests inject one they can hold open.
type Sampler = std::sync::Arc<dyn Fn() -> SampleFuture + Send + Sync>;

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
            std::sync::Arc::new(|| -> SampleFuture {
                Box::pin(async { tokio::task::spawn_blocking(sample_host).await.ok() })
            }),
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
/// Tokio's blocking pool, one at a time.
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
/// `effective_current_exe` is (`src/platform/paths.rs`): a release build has
/// no way to be told to report a host it is not on.
#[cfg(feature = "e2e")]
fn e2e_fixed_sample() -> Option<HostMetrics> {
    serde_json::from_str(&std::env::var("DOT_AGENT_DECK_E2E_HOST_SAMPLE").ok()?).ok()
}

/// The three watched roles and the path each resolves to on this host.
///
/// - **`working_root`** is the daemon's startup cwd
///   ([`crate::project_resolve::daemon_startup_cwd`]): captured once by
///   `run_daemon_with`, so the answer does not move if the process's cwd does,
///   and it is the directory the deck was started from — the checkout its
///   agents work in. A daemon is not otherwise told a "project root": agent
///   cwds are per pane and come and go, and a configured `working_dir` belongs
///   to a schedule. A server started without `run_daemon_with` (a test harness)
///   never captured one, so it falls back to the process's current cwd.
/// - **`worktree_parent`** is that root's parent, where the
///   `../<repo>-dispatch-*` worktrees land (`CLAUDE.md` rule 14).
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
    async fn host_metrics_proto_005_stuck_sample_spawns_one_job_and_requests_return() {
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

    /// Scenario: A statvfs result with a zero fragment size yields neither a
    /// free nor a total figure, rather than a zero free figure beside an
    /// unknown total; ordinary figures multiply out.
    #[spec("protocol/host-metrics/006")]
    #[test]
    fn host_metrics_proto_006_zero_fragment_size_leaves_free_absent() {
        assert_eq!(disk_figures(0, 10, 20), (None, None));
        assert_eq!(disk_figures(4096, 0, 20), (Some(0), Some(20 * 4096)));
        assert_eq!(
            disk_figures(4096, 10, 20),
            (Some(10 * 4096), Some(20 * 4096))
        );
        assert_eq!(disk_figures(u64::MAX, 10, 20), (None, None));
    }
}
