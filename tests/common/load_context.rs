//! Issue #701: make a load-induced failure say so itself.
//!
//! A test that fails because the machine was starved looks exactly like a test
//! that fails because the code regressed: the same assertion, the same panic,
//! the same red line in nextest's summary. Telling the two apart used to cost a
//! separate measurement round — an `origin/main` control and several isolated
//! reruns — which is the cost #701 recorded. This module moves that measurement
//! into the failing run itself.
//!
//! [`arm`] takes a baseline reading the first time a test process touches the
//! harness. When the process panics, the redacting panic hook in `mod.rs`
//! appends [`failure_report`]: how the machine behaved over *this test's own
//! window*, and a verdict on whether that is enough to explain a failure. The
//! same reading goes to stderr once a minute from a background thread, so a test
//! that nextest kills at its `slow-timeout` — which never reaches a panic — still
//! leaves a reading behind in the output nextest stores for a timed-out test.
//!
//! **What is measured, and why these signals.** On Linux the headline is Pressure
//! Stall Information (`/proc/pressure/{cpu,io,memory}`). Its `total` field is a
//! cumulative count of microseconds during which tasks were stalled, so the
//! difference between two readings divided by the wall time between them is the
//! exact share of *this* window spent stalled — an integral, not a sample, so a
//! burst between two readings is not missed. `cpu some` is the share of time at
//! least one runnable task waited for a CPU; `io full` and `memory full` are the
//! share during which every non-idle task was stalled at once, which is what
//! #863's `io full avg300=65.95` looked like. The one-minute load average per CPU
//! is reported beside them because it is what earlier issues quoted and it works
//! on macOS too; and a count of live agent and deck processes names the
//! co-tenant load #701 measured (81 `claude`, 161 `dot-agent-deck`) that
//! outlives the run that started it.
//!
//! **What it does not do.** It changes no wait, no timeout and no assertion: a
//! starved run still fails, and a verdict of `STARVED` is a label on that
//! failure, not an excuse for it. The readings are machine-wide, so they cannot
//! say *which* process caused the contention, only that it was there. And a
//! `QUIET` verdict says load is not the explanation to reach for first; it does
//! not prove the failure is a regression — #701's own `shell_activity_005` case
//! failed on a quiet box through a readiness race, which is exactly the case a
//! `QUIET` verdict is meant to send straight to the code instead of to a rerun.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// One machine-wide reading. Every field is `Option` because each source can be
/// absent independently: PSI needs Linux 4.20+ with `CONFIG_PSI` and can be
/// switched off at boot (`psi=0`), a container can hide `/proc/pressure`, and
/// only Linux and macOS publish a load average cheaply.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Reading {
    pub(crate) cpu_some_us: Option<u64>,
    pub(crate) io_full_us: Option<u64>,
    pub(crate) memory_full_us: Option<u64>,
    pub(crate) load1: Option<f64>,
}

/// Share of a window spent stalled, as a fraction in `0.0..=1.0`, per PSI source.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Stall {
    pub(crate) cpu_some: Option<f64>,
    pub(crate) io_full: Option<f64>,
    pub(crate) memory_full: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Stalled enough that a timing-sensitive test failing here is expected.
    Starved,
    /// Measurably busy, not starved.
    Contended,
    /// No signal above the contended thresholds.
    Quiet,
    /// Nothing could be measured on this host.
    Unmeasured,
}

/// `cpu some` at or above this share of the window reads as STARVED. Calibrated
/// on the 16-core dev box #701 was measured on: solo runs beside other agents'
/// ordinary work measured 1.5-13%, and every run under 48 or 96 deliberate
/// busy-loops (3 or 6 per CPU) measured 85-97%. The PR for #701 has the table.
pub(crate) const STARVED_CPU_SOME: f64 = 0.50;
/// `io full` / `memory full` at or above this share reads as STARVED: for that
/// share of the window NOTHING runnable was making progress.
pub(crate) const STARVED_FULL: f64 = 0.20;
/// Where PSI is unreadable (macOS, a kernel without it), a load average at or
/// above this many runnable tasks per CPU reads as STARVED. See [`classify`]
/// for why it does not vote where PSI is readable.
pub(crate) const STARVED_LOAD_PER_CPU: f64 = 2.0;
/// The CONTENDED counterparts of the three thresholds above.
pub(crate) const CONTENDED_CPU_SOME: f64 = 0.20;
pub(crate) const CONTENDED_FULL: f64 = 0.05;
pub(crate) const CONTENDED_LOAD_PER_CPU: f64 = 1.0;

/// How often the background thread writes a reading to stderr. Matches the
/// 60 s `slow-timeout` period in `.config/nextest.toml`, so a test nextest kills
/// has left one reading for each full minute it ran before the kill.
const HEARTBEAT: Duration = Duration::from_secs(60);

/// The process names counted as co-tenant load. Exact `comm` matches, which on
/// Linux is the executable's basename truncated to 15 bytes.
const AGENT_COMMS: [&str; 5] = ["claude", "opencode", "codex", "devin", "dot-agent-deck"];

struct Baseline {
    at: Instant,
    reading: Reading,
}

static BASELINE: OnceLock<Baseline> = OnceLock::new();

/// Record the baseline and start the heartbeat.
///
/// **One baseline per PROCESS, which is one per test under nextest** — the
/// runner every alias in `.cargo/config.toml` and every CI job uses, and which
/// runs each test in a process of its own. Under plain `cargo test` several
/// tests share a process and a later test's window then starts at the first
/// one's harness call; the report prints its window length for that reason,
/// so a window much longer than the failing test ran reads as what it is. Idempotent and cheap after the
/// first call. Called from the harness's panic-hook installer, which a `TuiDeck`
/// launch, every harness temp-dir allocation (`harness_temp_root`) and the
/// agent preflights and importers all reach — so a test that uses none of
/// those gets neither the hook nor this report.
pub(crate) fn arm() {
    let mut first = false;
    BASELINE.get_or_init(|| {
        first = true;
        Baseline {
            at: Instant::now(),
            reading: read_now(),
        }
    });
    if first && read_now() != Reading::default() {
        // A failure to spawn only loses the heartbeat; the panic-time report
        // does not depend on it.
        let _ = std::thread::Builder::new()
            .name("load-context".to_string())
            .stack_size(64 * 1024)
            .spawn(|| {
                loop {
                    std::thread::sleep(HEARTBEAT);
                    eprintln!("[load-context] {}", one_line_summary());
                }
            });
    }
}

/// The multi-line block the panic hook appends. Never panics: it runs inside
/// the panic hook, where a second panic aborts the process and loses the
/// original message.
pub(crate) fn failure_report() -> String {
    let Some(base) = BASELINE.get() else {
        return render_report(None, Duration::ZERO, Stall::default(), None, None, &[]);
    };
    let now = read_now();
    let window = base.at.elapsed();
    render_report(
        Some(cpus()),
        window,
        stall_between(&base.reading, &now, window),
        base.reading.load1,
        now.load1,
        &agent_process_counts(),
    )
}

/// The heartbeat's single line.
fn one_line_summary() -> String {
    let Some(base) = BASELINE.get() else {
        return "not armed".to_string();
    };
    let now = read_now();
    let window = base.at.elapsed();
    let stall = stall_between(&base.reading, &now, window);
    let cpus = cpus();
    let verdict = classify(
        &stall,
        peak_load_per_cpu(base.reading.load1, now.load1, cpus),
    );
    format!(
        "+{:.0}s {} cpu-some={} io-full={} mem-full={} load1={} on {cpus} CPUs",
        window.as_secs_f64(),
        verdict_word(verdict),
        percent(stall.cpu_some),
        percent(stall.io_full),
        percent(stall.memory_full),
        now.load1
            .map(|l| format!("{l:.1}"))
            .unwrap_or_else(|| "n/a".to_string()),
    )
}

fn cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

pub(crate) fn peak_load_per_cpu(start: Option<f64>, now: Option<f64>, cpus: usize) -> Option<f64> {
    let peak = match (start, now) {
        (Some(a), Some(b)) => a.max(b),
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => return None,
    };
    (peak.is_finite() && cpus > 0).then(|| peak / cpus as f64)
}

/// Stalled share of `window` for each PSI source present in both readings.
/// A counter that went backwards (it should not, but a reading is not worth a
/// panic) or a zero-length window yields `None` for that source.
pub(crate) fn stall_between(start: &Reading, end: &Reading, window: Duration) -> Stall {
    let window_us = window.as_micros();
    let share = |a: Option<u64>, b: Option<u64>| -> Option<f64> {
        let (a, b) = (a?, b?);
        if window_us == 0 || b < a {
            return None;
        }
        Some(((b - a) as f64 / window_us as f64).min(1.0))
    };
    Stall {
        cpu_some: share(start.cpu_some_us, end.cpu_some_us),
        io_full: share(start.io_full_us, end.io_full_us),
        memory_full: share(start.memory_full_us, end.memory_full_us),
    }
}

/// The verdict for a window.
///
/// **PSI decides wherever it is complete; the load average fills gaps.**
/// Measured while calibrating this on the dev box: with other agents' ordinary
/// work running, the 1-minute load average sat at 21-57 on 16 CPUs while PSI
/// `cpu some` over the same windows was 1.5-13%, and the tests passed at their
/// usual solo times. Linux counts tasks in uninterruptible sleep into the load
/// average, so a disk-bound box inflates it without starving anything of CPU.
/// PSI measures the stall itself, so when all three PSI sources are readable
/// the load average is shown in the report but does not vote.
///
/// When any PSI source is missing — none on macOS, or only some of the three
/// on a kernel or container that hides the rest — the load average votes
/// alongside whatever PSI is present, because it is the only remaining signal
/// for the resource that went unmeasured (it counts both CPU-runnable and
/// I/O-blocked tasks). The worst vote wins either way: any one signal over its
/// STARVED threshold is enough, because each independently means runnable work
/// sat waiting.
pub(crate) fn classify(stall: &Stall, load_per_cpu: Option<f64>) -> Verdict {
    let over = |v: Option<f64>, t: f64| v.is_some_and(|v| v >= t);
    let fulls = [stall.io_full, stall.memory_full];
    let psi = [stall.cpu_some, stall.io_full, stall.memory_full];
    let psi_vote = if !psi.iter().any(Option::is_some) {
        None
    } else if over(stall.cpu_some, STARVED_CPU_SOME) || fulls.iter().any(|&f| over(f, STARVED_FULL))
    {
        Some(Verdict::Starved)
    } else if over(stall.cpu_some, CONTENDED_CPU_SOME)
        || fulls.iter().any(|&f| over(f, CONTENDED_FULL))
    {
        Some(Verdict::Contended)
    } else {
        Some(Verdict::Quiet)
    };
    let load_vote = if psi.iter().all(Option::is_some) {
        None
    } else {
        match load_per_cpu {
            Some(l) if l >= STARVED_LOAD_PER_CPU => Some(Verdict::Starved),
            Some(l) if l >= CONTENDED_LOAD_PER_CPU => Some(Verdict::Contended),
            Some(_) => Some(Verdict::Quiet),
            None => None,
        }
    };
    let severity = |v: Verdict| match v {
        Verdict::Starved => 3,
        Verdict::Contended => 2,
        Verdict::Quiet => 1,
        Verdict::Unmeasured => 0,
    };
    [psi_vote, load_vote]
        .into_iter()
        .flatten()
        .max_by_key(|v| severity(*v))
        .unwrap_or(Verdict::Unmeasured)
}

fn verdict_word(v: Verdict) -> &'static str {
    match v {
        Verdict::Starved => "STARVED",
        Verdict::Contended => "CONTENDED",
        Verdict::Quiet => "QUIET",
        Verdict::Unmeasured => "UNMEASURED",
    }
}

fn percent(v: Option<f64>) -> String {
    v.map(|v| format!("{:.1}%", v * 100.0))
        .unwrap_or_else(|| "n/a".to_string())
}

/// Pure rendering, split out so every verdict's wording is unit-testable
/// without a panic or a loaded machine. `cpus` is `None` when the baseline was
/// never taken.
pub(crate) fn render_report(
    cpus: Option<usize>,
    window: Duration,
    stall: Stall,
    load_start: Option<f64>,
    load_now: Option<f64>,
    agents: &[(&str, usize)],
) -> String {
    let mut out = String::from("\n---- load context (issue #701) ----\n");
    let Some(cpus) = cpus else {
        out.push_str(
            "verdict: UNMEASURED — the harness took no baseline in this process, so there is no \
             window to measure.\n",
        );
        return out;
    };
    let verdict = classify(&stall, peak_load_per_cpu(load_start, load_now, cpus));
    let advice = match verdict {
        Verdict::Starved => {
            "the machine was starved while this test ran. A timing-sensitive failure here is \
             expected and is NOT evidence of a regression: rerun this test alone (CLAUDE.md \
             rule 6) before investigating the code."
        }
        Verdict::Contended => {
            "the machine was busy but not starved. Load may have contributed; a solo rerun \
             (CLAUDE.md rule 6) separates the two."
        }
        Verdict::Quiet => {
            "the machine was not measurably contended while this test ran, so load is not the \
             explanation to reach for first — read the failure as a real signal."
        }
        Verdict::Unmeasured => "no load signal is available on this host.",
    };
    out.push_str(&format!("verdict: {} — {advice}\n", verdict_word(verdict)));
    out.push_str(&format!(
        "window:  {:.1}s, from this process's first harness call to the panic\n",
        window.as_secs_f64()
    ));
    out.push_str(&format!(
        "stall:   cpu some {} / io full {} / memory full {} of the window (PSI; STARVED at \
         cpu >= {:.0}% or io/memory >= {:.0}%)\n",
        percent(stall.cpu_some),
        percent(stall.io_full),
        percent(stall.memory_full),
        STARVED_CPU_SOME * 100.0,
        STARVED_FULL * 100.0,
    ));
    let fmt_load = |l: Option<f64>| {
        l.map(|l| format!("{l:.2}"))
            .unwrap_or_else(|| "n/a".to_string())
    };
    out.push_str(&format!(
        "load:    1-min average {} at start, {} now, on {cpus} CPUs (votes only where a PSI source \
         is missing; STARVED at {:.0} per CPU)\n",
        fmt_load(load_start),
        fmt_load(load_now),
        STARVED_LOAD_PER_CPU,
    ));
    if !agents.is_empty() {
        let listed: Vec<String> = agents.iter().map(|(n, c)| format!("{c} {n}")).collect();
        out.push_str(&format!(
            "alive:   {} processes on this machine, this test's own included\n",
            listed.join(", ")
        ));
    }
    out
}

/// The `total=` field of the `some` or `full` line of a PSI file.
pub(crate) fn parse_psi_total(text: &str, kind: &str) -> Option<u64> {
    text.lines()
        .find(|line| line.split_whitespace().next() == Some(kind))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("total="))?
        .parse()
        .ok()
}

/// The `avg10=` field of the `some` or `full` line of a PSI file, as a fraction
/// in `0.0..=1.0` rather than the percentage the kernel prints.
///
/// The ten-second average rather than a `total=` delta, because its consumer —
/// `load_scaled` in `mod.rs` — sizes a wait at the moment the wait starts and
/// has no earlier reading of its own to take a difference against.
pub(crate) fn parse_psi_avg10(text: &str, kind: &str) -> Option<f64> {
    let percent: f64 = text
        .lines()
        .find(|line| line.split_whitespace().next() == Some(kind))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse()
        .ok()?;
    (percent.is_finite() && percent >= 0.0).then(|| (percent / 100.0).min(1.0))
}

/// Issue #709 follow-up: the larger of the `io full` and `memory full` ten-second
/// averages — the share of the last ten seconds during which NOTHING runnable on
/// the machine made progress — or `None` where neither file is readable.
///
/// `cpu some` is deliberately not folded in: CPU contention is what the load
/// average already measures for `load_scaled`, while an I/O stall is precisely
/// what it does not. `delegate_012` failed with an empty snapshot at load 15 on
/// 16 CPUs (a factor of 1.0) while `io full` sat at 68% of its window.
#[cfg(target_os = "linux")]
pub(crate) fn full_stall_share() -> Option<f64> {
    let psi = |file: &str| {
        std::fs::read_to_string(format!("/proc/pressure/{file}"))
            .ok()
            .and_then(|text| parse_psi_avg10(&text, "full"))
    };
    match (psi("io"), psi("memory")) {
        (Some(io), Some(memory)) => Some(io.max(memory)),
        (one, other) => one.or(other),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn full_stall_share() -> Option<f64> {
    None
}

#[cfg(target_os = "linux")]
fn read_now() -> Reading {
    let psi = |file: &str, kind: &str| {
        std::fs::read_to_string(format!("/proc/pressure/{file}"))
            .ok()
            .and_then(|text| parse_psi_total(&text, kind))
    };
    Reading {
        cpu_some_us: psi("cpu", "some"),
        io_full_us: psi("io", "full"),
        memory_full_us: psi("memory", "full"),
        load1: super::one_minute_load_average(),
    }
}

#[cfg(not(target_os = "linux"))]
fn read_now() -> Reading {
    Reading {
        load1: super::one_minute_load_average(),
        ..Reading::default()
    }
}

/// Live processes per name in [`AGENT_COMMS`], omitting names with none. Linux
/// only; elsewhere the line is left out of the report rather than guessed at.
#[cfg(target_os = "linux")]
fn agent_process_counts() -> Vec<(&'static str, usize)> {
    let mut counts = [0usize; AGENT_COMMS.len()];
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        if let Some(i) = AGENT_COMMS.iter().position(|c| *c == comm.trim_end()) {
            counts[i] += 1;
        }
    }
    AGENT_COMMS
        .iter()
        .zip(counts)
        .filter(|(_, c)| *c > 0)
        .map(|(n, c)| (*n, c))
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn agent_process_counts() -> Vec<(&'static str, usize)> {
    Vec::new()
}
