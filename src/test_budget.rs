//! Load-scaled wait ceilings for the lib target's own `#[cfg(test)]` tests —
//! issue #709's helper, on this side of the wall.
//!
//! `tests/common/mod.rs` carries `load_scaled`, `load_factor` and the
//! measurement behind them, but **the lib target does not link that file** —
//! the same wall [`crate::test_isolation`] documents, and for the same reason it
//! cannot share a constant with the harness. So a unit test in `src/` reaching
//! for a contention-proof ceiling had nothing to call and wrote a flat
//! `Duration::from_secs(N)` instead. That is precisely the pre-#709
//! configuration, and it fails the way #709 measured: a ceiling sized for an
//! idle box, on something that must HAPPEN, read as a wrong verdict once the
//! box is contended.
//!
//! **Measured here, not assumed.** Both `shell_activity_monitor` tests in
//! `daemon.rs` failed together in 1 of 10 `cargo test-fast` runs on a 16-core
//! box, and each failed at its own first deadline plus overhead — 3.076 s
//! against a flat `from_secs(3)`, and 5.089 s against a flat `from_secs(5)`.
//! Neither is a behavioural failure: both wait on a real `/bin/sh` starting a
//! **`python3` interpreter** which then forks, `setsid`s and `execv`s a `sleep`,
//! after which the monitor still has to sample the process table. That whole
//! chain is a boot, and a boot is what #709 is about.
//!
//! Kept deliberately small: the harness's version carries the full rationale and
//! is the one to read. This is the minimum needed to stop a lib-side test
//! asserting against an idle box's schedule.

use std::time::Duration;

/// The largest factor [`load_scaled`] will multiply a base by. Mirrors
/// `tests/common/mod.rs`'s `MAX_LOAD_FACTOR`; see there for why it is a clamp
/// rather than "whatever the load average says".
const MAX_LOAD_FACTOR: f64 = 6.0;

/// The 1-minute load average per CPU, or `None` where this platform does not
/// publish one cheaply. Linux (`/proc/loadavg`) and macOS (`getloadavg(3)`),
/// exactly as the harness's twin, which records why macOS joined; elsewhere
/// `None`, because an unmeasurable load must widen nothing.
fn machine_load_per_cpu() -> Option<f64> {
    let one_minute = one_minute_load_average()?;
    let cpus = std::thread::available_parallelism().ok()?.get() as f64;
    if !one_minute.is_finite() || cpus <= 0.0 {
        return None;
    }
    Some(one_minute / cpus)
}

#[cfg(target_os = "linux")]
fn one_minute_load_average() -> Option<f64> {
    let raw = std::fs::read_to_string("/proc/loadavg").ok()?;
    raw.split_whitespace().next()?.parse().ok()
}

#[cfg(target_os = "macos")]
fn one_minute_load_average() -> Option<f64> {
    let mut sample = [0.0_f64; 1];
    // SAFETY: `getloadavg` writes at most `nelem` (1) doubles into a buffer we
    // own and have sized to 1, and returns how many it wrote, or -1 on failure.
    let written = unsafe { libc::getloadavg(sample.as_mut_ptr(), 1) };
    (written == 1).then_some(sample[0])
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn one_minute_load_average() -> Option<f64> {
    None
}

/// The multiplier [`load_scaled`] applies, split out so the unmeasurable branch
/// is testable on Linux and macOS — where [`machine_load_per_cpu`] does not
/// return `None` in practice.
///
/// **An unmeasurable load yields 1.0, not [`MAX_LOAD_FACTOR`].** `None` is the
/// absence of a measurement, not evidence of contention, and the harness's twin
/// records what treating it as maximal cost: every `load_scaled` wait multiplied
/// by 6 on every macOS run, idle or not. A non-finite input is likewise
/// unmeasurable, so this is total and cannot hand `mul_f64` a `NaN` to panic on.
fn load_factor(load_per_cpu: Option<f64>) -> f64 {
    match load_per_cpu {
        Some(load) if load.is_finite() => load.clamp(1.0, MAX_LOAD_FACTOR),
        _ => 1.0,
    }
}

/// Widen a wait ceiling in proportion to how contended the machine is, so a fast
/// box still fails fast and a loaded one still passes.
///
/// Apply this ONLY to a ceiling on something that must HAPPEN, never to a
/// negative window in which something must NOT happen. The waits it feeds return
/// the moment their condition holds, so a wider ceiling costs nothing on the
/// happy path and is paid only where the alternative was a wrong verdict; a
/// negative window is always paid in full and its length is part of the
/// assertion.
pub(crate) fn load_scaled(base: Duration) -> Duration {
    base.mul_f64(load_factor(machine_load_per_cpu()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unmeasurable_load_widens_nothing() {
        assert_eq!(load_factor(None), 1.0);
        assert_eq!(load_factor(Some(f64::NAN)), 1.0);
        assert_eq!(load_factor(Some(f64::INFINITY)), 1.0);
    }

    #[test]
    fn an_idle_box_keeps_the_base_and_a_loaded_one_is_clamped() {
        assert_eq!(
            load_factor(Some(0.1)),
            1.0,
            "below 1.0 never shrinks a base"
        );
        assert_eq!(
            load_factor(Some(2.75)),
            2.75,
            "#709's measured load passes through"
        );
        assert_eq!(
            load_factor(Some(99.0)),
            MAX_LOAD_FACTOR,
            "clamped, not unbounded"
        );
    }

    /// Issue #1244: macOS used to return `None` here, leaving every lib-side
    /// ceiling unscaled on `build-macos`. macOS only, because a Linux box with
    /// no readable `/proc/loadavg` returning `None` is correct — see the
    /// harness twin's test.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_load_is_measurable_on_macos() {
        let load = machine_load_per_cpu();
        assert!(
            load.is_some_and(|l| l.is_finite() && l >= 0.0),
            "machine_load_per_cpu() must measure the load here, got {load:?}"
        );
    }

    #[test]
    fn scaling_is_the_factor_applied_to_the_base() {
        let base = Duration::from_secs(3);
        assert_eq!(
            load_scaled(base),
            base.mul_f64(load_factor(machine_load_per_cpu()))
        );
        assert!(load_scaled(base) >= base, "a ceiling is never narrowed");
    }
}
