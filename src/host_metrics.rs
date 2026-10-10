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

/// The daemon's one host sample, reused for [`HOST_METRICS_MAX_AGE`].
///
/// Ages are measured on Tokio's clock, not `std`'s, so a paused-clock test can
/// sequence a cache hit and an expiry without sleeping (#1237's pattern). One
/// cache per attach server, so every connection to a daemon shares it.
#[derive(Debug, Default)]
pub struct HostMetricsCache {
    sample: std::sync::Mutex<Option<(tokio::time::Instant, HostMetrics)>>,
}

impl HostMetricsCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached sample with its age as of `now`, or a fresh one if the cache
    /// is empty or older than [`HOST_METRICS_MAX_AGE`]. `now` is the caller's
    /// so it can be read on the async side, where Tokio's clock lives, while
    /// this runs on a blocking thread.
    ///
    /// The lock is held while sampling, so concurrent requests on a cold cache
    /// take one sample between them rather than one each.
    pub fn read_at(&self, now: tokio::time::Instant) -> HostMetrics {
        self.read_with(now, sample_host)
    }

    fn read_with(
        &self,
        now: tokio::time::Instant,
        sample: impl FnOnce() -> HostMetrics,
    ) -> HostMetrics {
        let mut cached = self.sample.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((taken, metrics)) = cached.as_ref() {
            let age = now.saturating_duration_since(*taken);
            if age <= HOST_METRICS_MAX_AGE {
                return HostMetrics {
                    sample_age_ms: u64::try_from(age.as_millis()).unwrap_or(u64::MAX),
                    ..metrics.clone()
                };
            }
        }
        let fresh = HostMetrics {
            sample_age_ms: 0,
            ..sample()
        };
        *cached = Some((now, fresh.clone()));
        fresh
    }
}

/// Take one sample of this host, now. Blocking: a `statvfs` per role and two
/// small file reads. Callers go through [`HostMetricsCache`].
pub fn sample_host() -> HostMetrics {
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
    // The field widths differ by platform (`fsblkcnt_t` is 32-bit on macOS).
    #[allow(clippy::unnecessary_cast)]
    let (fragment, available, blocks) = (
        stat.f_frsize as u64,
        stat.f_bavail as u64,
        stat.f_blocks as u64,
    );
    let total = blocks.checked_mul(fragment).filter(|&t| t > 0);
    // A free figure without a total to bound it is still a reading, but one
    // that overflowed is not.
    let free = available.checked_mul(fragment);
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
