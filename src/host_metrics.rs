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
