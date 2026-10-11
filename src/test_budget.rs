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
///
/// One of the two readings [`load_scaled`] takes; the other is
/// [`machine_stall_share`].
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

/// How much slower than an idle box work progresses when every non-idle task
/// is stalled for `stall_share` of the time: `1 / (1 - stall_share)`, clamped to
/// `1.0..=MAX_LOAD_FACTOR`. An unmeasurable or non-finite share yields 1.0, for
/// [`load_factor`]'s reason. A copy of the harness's `stall_factor`.
fn stall_factor(stall_share: Option<f64>) -> f64 {
    match stall_share {
        Some(share) if share.is_finite() => {
            let progress = 1.0 - share.clamp(0.0, 1.0);
            if progress <= 1.0 / MAX_LOAD_FACTOR {
                MAX_LOAD_FACTOR
            } else {
                (1.0 / progress).clamp(1.0, MAX_LOAD_FACTOR)
            }
        }
        _ => 1.0,
    }
}

/// The larger of the IO and memory `full` stall shares from Pressure Stall
/// Information, as a fraction, or `None` where neither file can be read. A
/// copy of the harness's `machine_stall_share`, which says why `full`, why IO
/// and memory, and why the larger of `avg10` and `avg60`.
fn machine_stall_share() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let read = |file: &str| {
            std::fs::read_to_string(format!("/proc/pressure/{file}"))
                .ok()
                .and_then(|text| parse_psi_full_share(&text))
        };
        match (read("io"), read("memory")) {
            (Some(io), Some(memory)) => Some(io.max(memory)),
            (one, other) => one.or(other),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The larger of the `avg10` and `avg60` fields on the `full` line of a PSI
/// file, as a fraction rather than the percentage the kernel prints.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_psi_full_share(text: &str) -> Option<f64> {
    let line = text
        .lines()
        .find(|line| line.split_whitespace().next() == Some("full"))?;
    let field = |name: &str| -> Option<f64> {
        line.split_whitespace()
            .find_map(|f| f.strip_prefix(name))?
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
    };
    let percent = match (field("avg10="), field("avg60=")) {
        (Some(a), Some(b)) => a.max(b),
        (one, other) => one.or(other)?,
    };
    Some(percent / 100.0)
}

/// The factor [`load_scaled`] applies: the larger of the two readings, so
/// either kind of contention widens a ceiling.
fn contention_factor(load_per_cpu: Option<f64>, stall_share: Option<f64>) -> f64 {
    load_factor(load_per_cpu).max(stall_factor(stall_share))
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
///
/// **Contention is the larger of the load average and the PSI stall share**,
/// as in the harness. This copy used to read the load average alone, and PRD
/// #1631 measured what that misses: `first_write_within_bounds_only_the_time_not_spent_on_the_draft`
/// failed at a load of ~33 on 16 CPUs (a factor of ~2) beside `io full` at
/// 75% (a factor of 4).
pub(crate) fn load_scaled(base: Duration) -> Duration {
    base.mul_f64(contention_factor(
        machine_load_per_cpu(),
        machine_stall_share(),
    ))
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
    fn a_stall_share_widens_by_the_progress_it_leaves() {
        assert_eq!(stall_factor(None), 1.0);
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(stall_factor(Some(bad)), 1.0, "{bad} is not a stall share");
        }
        assert_eq!(stall_factor(Some(0.0)), 1.0);
        assert_eq!(stall_factor(Some(0.5)), 2.0);
        assert_eq!(stall_factor(Some(0.75)), 4.0, "PRD #1631's io full 75%");
        assert_eq!(stall_factor(Some(0.928)), MAX_LOAD_FACTOR);
        assert_eq!(stall_factor(Some(-0.3)), 1.0, "never shrinks");
    }

    #[test]
    fn contention_is_the_larger_of_load_and_stall() {
        assert_eq!(contention_factor(Some(33.0 / 16.0), Some(0.75)), 4.0);
        assert_eq!(contention_factor(Some(2.75), Some(0.0)), 2.75);
        assert_eq!(contention_factor(None, None), 1.0);
        assert_eq!(contention_factor(Some(1000.0), Some(1.0)), MAX_LOAD_FACTOR);
    }

    #[test]
    fn the_psi_full_line_is_read_as_a_fraction() {
        let io = "some avg10=82.47 avg60=76.65 avg300=67.06 total=1\n\
                  full avg10=74.69 avg60=70.26 avg300=61.17 total=1\n";
        assert_eq!(parse_psi_full_share(io), Some(0.7469));
        let lull = "full avg10=10.00 avg60=72.00 avg300=1.00 total=1\n";
        assert_eq!(parse_psi_full_share(lull), Some(0.72));
        assert_eq!(parse_psi_full_share("some avg10=9.00 avg60=9.00\n"), None);
        assert_eq!(parse_psi_full_share("full avg10=x avg60=y\n"), None);
        assert_eq!(parse_psi_full_share(""), None);
    }

    #[test]
    fn scaling_is_the_factor_applied_to_the_base() {
        let base = Duration::from_secs(3);
        let scaled = load_scaled(base);
        assert!(scaled >= base, "a ceiling is never narrowed");
        assert!(
            scaled <= base.mul_f64(MAX_LOAD_FACTOR),
            "and never past the cap"
        );
    }
}
