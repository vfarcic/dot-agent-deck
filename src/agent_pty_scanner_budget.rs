//! PRD #1666 part 3, round 5-wake (plan §4.6, §9.1): the release scanner's
//! budget, measured on the real push path. Not a gate: it prints the figures
//! the release records' `parser_cost` is verified from, and is run by hand,
//! in the shipped (release) profile:
//!
//! ```text
//! cargo test --release --lib scanner_budget -- --ignored --nocapture --test-threads 1
//! ```
//!
//! What is measured, as plan §9.1 defines it:
//!
//! - **added lock hold**: each recorded trace is replayed through
//!   [`AgentBus::push`] at its recorded `read` sizes on two buses, one with the
//!   release scanner and one without, and the hold of push `i` without the
//!   scanner is subtracted from the hold of push `i` with it, run by run. p99
//!   is the nearest rank over every push of every run. The scanner's own wall
//!   time inside each push is reported beside it as a cross-check.
//! - **CPU per generation**: `CLOCK_THREAD_CPUTIME_ID` around the scanner's
//!   calls only, summed over one replay, and over a composite trace that fills
//!   the whole 256 KiB window.
//! - **eight panes at once**: eight threads replay eight traces into eight
//!   buses together; the machine's core count and load are printed, since
//!   contention there is the bus lock's, not the scanner's CPU.
//! - **the pathological traces**: 256 KiB of erase-display alternating with
//!   rows that hit both header anchors at 512 × 512, and fourteen more of one
//!   whole-grid or many-row operation repeated (Q25), pushed 8192 bytes at a
//!   time; each maximum per-push figure is printed as a number, with the
//!   work limit that dropped the scanner.
//! - **the work-unit calibration** ([`scanner_budget_calibration`]): every
//!   trace priced in [`crate::vt_work`]'s units with no limit, against its
//!   CPU time, which is where the scanner's and the evidence parser's work
//!   limits come from.
//! - resizes are timed on their own and charged to no push.

use super::*;

const RUNS: usize = 15;

pub(super) struct Trace {
    pub(super) name: String,
    pub(super) profile: &'static crate::doorbell_screen::ScreenProfile,
    pub(super) bytes: Vec<u8>,
    pub(super) read_lens: Vec<usize>,
}

/// Every recorded launch trace under `tests/fixtures/*/r5wake`, with its
/// recorded `read` sizes.
pub(super) fn traces() -> Vec<Trace> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut out = Vec::new();
    for (dir, profile) in [
        ("codex-inbox/r5wake", &crate::doorbell_screen::CODEX_PROFILE),
        (
            "claude-inbox/r5wake",
            &crate::doorbell_screen::CLAUDE_PROFILE,
        ),
    ] {
        let mut entries: Vec<_> = std::fs::read_dir(root.join(dir))
            .expect("fixture dir")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "bin"))
            .collect();
        entries.sort();
        for bin in entries {
            let meta: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(bin.with_extension("json")).expect("trace json"),
            )
            .expect("trace json parses");
            let Some(lens) = meta["read_lens"].as_array() else {
                continue;
            };
            let read_lens: Vec<usize> = lens
                .iter()
                .map(|l| l.as_u64().expect("read length") as usize)
                .collect();
            let bytes = std::fs::read(&bin).expect("trace bytes");
            assert_eq!(
                read_lens.iter().sum::<usize>(),
                bytes.len(),
                "{bin:?}: the read sizes cover the trace"
            );
            out.push(Trace {
                name: bin.file_stem().unwrap().to_string_lossy().into_owned(),
                profile,
                bytes,
                read_lens,
            });
        }
    }
    out
}

pub(super) fn chunks<'a>(
    bytes: &'a [u8],
    lens: &'a [usize],
) -> impl Iterator<Item = &'a [u8]> + 'a {
    let mut at = 0;
    lens.iter().map(move |&n| {
        let chunk = &bytes[at..at + n];
        at += n;
        chunk
    })
}

/// One replay: per push, the lock hold and the scanner's wall time; the
/// scanner's CPU summed.
fn replay(bus: &AgentBus, trace: &[u8], lens: &[usize]) -> (Vec<u64>, Vec<u64>, u64) {
    let mut holds = Vec::with_capacity(lens.len());
    let mut scanner = Vec::with_capacity(lens.len());
    let mut cpu = 0;
    for chunk in chunks(trace, lens) {
        let cost = bus.push_timed(chunk.to_vec());
        holds.push(cost.hold_ns);
        if let Some((wall, c)) = cost.scanner {
            scanner.push(wall);
            cpu += c;
        }
    }
    (holds, scanner, cpu)
}

fn nearest_rank(sorted: &[i64], pct: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((pct / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn us(ns: i64) -> String {
    format!("{:.1}", ns as f64 / 1_000.0)
}

fn load_and_cores() -> String {
    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    format!(
        "cores={} loadavg={}",
        std::thread::available_parallelism().map_or(0, |n| n.get()),
        load.split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join(" ")
    )
}

struct Row {
    added: Vec<i64>,
    scanner: Vec<i64>,
    cpu_max: u64,
    pushes: usize,
    /// The most work units the scanner did in one replay, and whether any
    /// replay reached a work limit.
    work_max: u64,
    over: bool,
}

fn measure(trace: &Trace, rows: u16, cols: u16) -> Row {
    let mut added = Vec::new();
    let mut scanner_all = Vec::new();
    let mut cpu_max = 0;
    let mut work_max = 0;
    let mut over = false;
    for run in 0..RUNS {
        let with = AgentBus::profiled(Some(trace.profile), rows, cols);
        let without = AgentBus::profiled_without_scanner(Some(trace.profile), rows, cols);
        // Alternate which goes first so neither always meets a warm cache.
        let ((on, scanner, cpu), (off, _, _)) = if run % 2 == 0 {
            let a = replay(&with, &trace.bytes, &trace.read_lens);
            let b = replay(&without, &trace.bytes, &trace.read_lens);
            (a, b)
        } else {
            let b = replay(&without, &trace.bytes, &trace.read_lens);
            let a = replay(&with, &trace.bytes, &trace.read_lens);
            (a, b)
        };
        added.extend(on.iter().zip(&off).map(|(a, b)| *a as i64 - *b as i64));
        scanner_all.extend(scanner.iter().map(|&n| n as i64));
        cpu_max = cpu_max.max(cpu);
        let cost = with.state.lock().unwrap().release.cost();
        work_max = work_max.max(cost.work);
        over |= cost.over.is_some();
    }
    added.sort_unstable();
    scanner_all.sort_unstable();
    Row {
        pushes: trace.read_lens.len(),
        added,
        scanner: scanner_all,
        cpu_max,
        work_max,
        over,
    }
}

fn print_row(label: &str, row: &Row) {
    println!(
        "{label:<58} pushes={:>4} runs={RUNS} added_hold_us p99={:>7} max={:>7} | scanner_us p99={:>7} max={:>7} | cpu_ms/gen max={:.2} | work max={} over={}",
        row.pushes,
        us(nearest_rank(&row.added, 99.0)),
        us(*row.added.last().unwrap_or(&0)),
        us(nearest_rank(&row.scanner, 99.0)),
        us(*row.scanner.last().unwrap_or(&0)),
        row.cpu_max as f64 / 1e6,
        row.work_max,
        row.over,
    );
}

/// Scenario: Not a gate. Replay every recorded launch trace through the real push path with and without the release scanner, at 50 × 200 and 80 × 300, eight panes at once, over a full 256 KiB window and on a pathological trace at 512 × 512, and print the added lock hold, the scanner's own time and its CPU per generation against plan §4.6's limits.
#[test]
#[ignore = "measurement, run by hand in the release profile (plan §9.1)"]
fn scanner_budget_measurement() {
    let traces = traces();
    println!("release scanner budget, {}", load_and_cores());
    println!(
        "limits (plan §4.6): added hold <= 1000 us p99 and <= 10000 us max per push; cpu <= 50 ms per generation"
    );
    let mut all_added = Vec::new();
    let mut worst_cpu = 0u64;
    for (rows, cols) in [(50u16, 200u16), (80, 300)] {
        for trace in &traces {
            let row = measure(trace, rows, cols);
            print_row(&format!("{} @{rows}x{cols}", trace.name), &row);
            all_added.extend(row.added.iter().copied());
            worst_cpu = worst_cpu.max(row.cpu_max);
        }
    }
    all_added.sort_unstable();
    println!(
        "ALL measured traces: added_hold_us p99={} max={} (n={}), worst cpu_ms/gen={:.2}",
        us(nearest_rank(&all_added, 99.0)),
        us(*all_added.last().unwrap_or(&0)),
        all_added.len(),
        worst_cpu as f64 / 1e6
    );

    // A full window: traces concatenated, then repeated, to 256 KiB, pushed
    // at the recorded sizes.
    let window = crate::release_scanner::RELEASE_WINDOW_BYTES as usize;
    let mut bytes = Vec::new();
    let mut lens = Vec::new();
    'fill: loop {
        for trace in &traces {
            for chunk in chunks(&trace.bytes, &trace.read_lens) {
                let take = chunk.len().min(window - bytes.len());
                bytes.extend_from_slice(&chunk[..take]);
                lens.push(take);
                if bytes.len() == window {
                    break 'fill;
                }
            }
        }
    }
    for (rows, cols) in [(50u16, 200u16), (80, 300)] {
        let full = Trace {
            name: "full-window-composite".into(),
            profile: &crate::doorbell_screen::CLAUDE_PROFILE,
            bytes: bytes.clone(),
            read_lens: lens.clone(),
        };
        let row = measure(&full, rows, cols);
        print_row(&format!("full 256 KiB window @{rows}x{cols}"), &row);
    }

    // Eight panes at once.
    println!("eight panes at once: {}", load_and_cores());
    let mut added8 = Vec::new();
    let mut scanner8 = Vec::new();
    for run in 0..RUNS {
        let picked: Vec<&Trace> = traces.iter().cycle().skip(run).take(8).collect();
        let with = std::thread::scope(|scope| {
            let handles: Vec<_> = picked
                .iter()
                .map(|t| {
                    scope.spawn(move || {
                        let bus = AgentBus::profiled(Some(t.profile), 50, 200);
                        let replayed = replay(&bus, &t.bytes, &t.read_lens);
                        let over = bus.state.lock().unwrap().release.cost().over;
                        assert_eq!(over, None, "{} reached a work limit", t.name);
                        replayed
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });
        let without = std::thread::scope(|scope| {
            let handles: Vec<_> = picked
                .iter()
                .map(|t| {
                    scope.spawn(move || {
                        let bus = AgentBus::profiled_without_scanner(Some(t.profile), 50, 200);
                        replay(&bus, &t.bytes, &t.read_lens)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });
        for ((on, sc, _), (off, _, _)) in with.iter().zip(&without) {
            added8.extend(on.iter().zip(off).map(|(a, b)| *a as i64 - *b as i64));
            scanner8.extend(sc.iter().map(|&n| n as i64));
        }
    }
    added8.sort_unstable();
    scanner8.sort_unstable();
    println!(
        "eight panes @50x200: added_hold_us p99={} max={} (n={}) | scanner_us p99={} max={}",
        us(nearest_rank(&added8, 99.0)),
        us(*added8.last().unwrap_or(&0)),
        added8.len(),
        us(nearest_rank(&scanner8, 99.0)),
        us(*scanner8.last().unwrap_or(&0)),
    );

    // Resizes, charged to no push.
    let mut resize_max = 0u64;
    for _ in 0..RUNS {
        let bus = AgentBus::profiled(Some(&crate::doorbell_screen::CLAUDE_PROFILE), 50, 200);
        for chunk in chunks(&traces[0].bytes, &traces[0].read_lens) {
            bus.push_timed(chunk.to_vec());
        }
        for (r, c) in [(80u16, 300u16), (50, 200), (512, 512), (24, 80)] {
            let _ = bus.resize_and_clear(r, c, || Ok::<(), ()>(()));
        }
        resize_max = resize_max.max(bus.state.lock().unwrap().release.cost().resize_ns_max);
    }
    println!(
        "resize of the scanner's grid: max {} us",
        us(resize_max as i64)
    );

    // The pathological traces (Q25), each through the real push path with
    // its budget in: the scanner is dropped at the first byte over a limit,
    // so the added hold and the CPU are bounded however long the trace runs.
    for (name, rows, cols, patho) in pathological() {
        let lens: Vec<usize> = patho.chunks(8192).map(<[u8]>::len).collect();
        let mut added_p = Vec::new();
        let mut scanner_p = Vec::new();
        let mut cpu_p = 0;
        let mut dropped = None;
        for run in 0..3 {
            let with = AgentBus::profiled(None, rows, cols);
            let without = AgentBus::profiled_without_scanner(None, rows, cols);
            let ((on, sc, cpu), (off, _, _)) = if run % 2 == 0 {
                let a = replay(&with, &patho, &lens);
                (a, replay(&without, &patho, &lens))
            } else {
                let b = replay(&without, &patho, &lens);
                (replay(&with, &patho, &lens), b)
            };
            added_p.extend(on.iter().zip(&off).map(|(a, b)| *a as i64 - *b as i64));
            scanner_p.extend(sc.iter().map(|&n| n as i64));
            cpu_p = cpu_p.max(cpu);
            let state = with.state.lock().unwrap();
            dropped = Some((state.release.closed(), state.release.cost().over));
        }
        added_p.sort_unstable();
        scanner_p.sort_unstable();
        println!(
            "pathological {name} @{rows}x{cols}: added_hold_us max={} | scanner_us max={} | cpu_ms/gen={:.2} | window {:?}",
            us(*added_p.last().unwrap_or(&0)),
            us(*scanner_p.last().unwrap_or(&0)),
            cpu_p as f64 / 1e6,
            dropped,
        );
    }

    // The evidence parser's share (the other half of `parser_cost`): a
    // profiled bus against an unprofiled one, both without the scanner.
    let evidence = |profile: &'static crate::doorbell_screen::ScreenProfile,
                    bytes: &[u8],
                    lens: &[usize],
                    rows: u16,
                    cols: u16,
                    runs: usize| {
        let mut added = Vec::new();
        for run in 0..runs {
            let with = AgentBus::profiled_without_scanner(Some(profile), rows, cols);
            let without = AgentBus::profiled_without_scanner(None, rows, cols);
            let (on, off) = if run % 2 == 0 {
                let a = replay(&with, bytes, lens).0;
                (a, replay(&without, bytes, lens).0)
            } else {
                let b = replay(&without, bytes, lens).0;
                (replay(&with, bytes, lens).0, b)
            };
            added.extend(on.iter().zip(&off).map(|(a, b)| *a as i64 - *b as i64));
        }
        added.sort_unstable();
        added
    };
    let mut ev_all = Vec::new();
    for (rows, cols) in [(50u16, 200u16), (80, 300)] {
        for trace in &traces {
            ev_all.extend(evidence(
                trace.profile,
                &trace.bytes,
                &trace.read_lens,
                rows,
                cols,
                RUNS,
            ));
        }
    }
    ev_all.sort_unstable();
    println!(
        "evidence parser, ALL measured traces @50x200+80x300: added_hold_us p99={} max={} (n={})",
        us(nearest_rank(&ev_all, 99.0)),
        us(*ev_all.last().unwrap_or(&0)),
        ev_all.len()
    );
    for (name, rows, cols, patho) in pathological() {
        let lens: Vec<usize> = patho.chunks(8192).map(<[u8]>::len).collect();
        let ev_p = evidence(
            &crate::doorbell_screen::CLAUDE_PROFILE,
            &patho,
            &lens,
            rows,
            cols,
            3,
        );
        let bus = AgentBus::profiled_without_scanner(
            Some(&crate::doorbell_screen::CLAUDE_PROFILE),
            rows,
            cols,
        );
        replay(&bus, &patho, &lens);
        println!(
            "evidence parser, pathological {name} @{rows}x{cols}: added_hold_us max={} | status {}",
            us(*ev_p.last().unwrap_or(&0)),
            bus.evidence().0.as_str(),
        );
    }
}

/// The pathological traces Q25's budgets are calibrated on, each 256 KiB of
/// one whole-grid or many-row operation repeated, at the geometry that makes
/// it most expensive within the cell budget.
pub(super) fn pathological() -> Vec<(&'static str, u16, u16, Vec<u8>)> {
    let window = crate::release_scanner::RELEASE_WINDOW_BYTES as usize;
    let fill = |unit: &[u8]| {
        let mut bytes = Vec::with_capacity(window + unit.len());
        while bytes.len() < window {
            bytes.extend_from_slice(unit);
        }
        bytes.truncate(window);
        bytes
    };
    let mut erase = Vec::new();
    let mut i = 0u32;
    while erase.len() < window {
        erase.extend_from_slice(b"\x1b[2J");
        erase.extend_from_slice(
            format!(
                "\x1b[2;12HClaude Code v{}.\x1b[2;6HOpenAI Codex (v0.{i}\r\n",
                i % 10
            )
            .as_bytes(),
        );
        i += 1;
    }
    erase.truncate(window);
    vec![
        ("erase-display and anchor rows", 512, 512, erase),
        ("line feeds at the bottom", 4096, 64, fill(b"\n")),
        ("reverse index at the top", 4096, 64, fill(b"\x1bM")),
        ("scroll up 4096", 4096, 64, fill(b"\x1b[4096S")),
        ("insert lines 4096", 4096, 64, fill(b"\x1b[4096L")),
        (
            "repeat 65535 in one column",
            4096,
            64,
            fill(b"x\x1b[65535b"),
        ),
        (
            "alternate screen x8",
            512,
            512,
            fill(b"\x1b[?1049;1049;1049;1049;1049;1049;1049;1049h"),
        ),
        ("full reset", 512, 512, fill(b"\x1bc")),
        ("erase line", 512, 512, fill(b"\x1b[K")),
        (
            "erase line, cursor at the start",
            512,
            512,
            fill(b"\r\x1b[K"),
        ),
        ("erase characters 512", 512, 512, fill(b"\r\x1b[512X")),
        ("delete characters 512", 512, 512, fill(b"\r\x1b[512P")),
        ("insert characters 512", 512, 512, fill(b"\r\x1b[512@")),
        ("erase display below", 512, 512, fill(b"\x1b[H\x1b[J")),
        ("prints at the row end", 4096, 64, fill(b"\x1b[4096;64Hx")),
    ]
}

/// Work units and CPU for `bytes` fed one byte at a time to a metered parser
/// with no limit, as the scanner and the evidence parser feed theirs; the
/// most units any `chunk`-byte push takes, and the most CPU nanoseconds.
fn units_and_cpu(bytes: &[u8], rows: u16, cols: u16, chunk: usize) -> (u64, u64, u64, u64) {
    let mut parser = crate::vt_work::MeteredParser::new(rows, cols, 0);
    let mut push_units_max = 0;
    let mut push_cpu_max = 0;
    let mut cpu = 0;
    for push in bytes.chunks(chunk) {
        let before_units = parser.spent();
        let before = crate::release_scanner::thread_cpu_ns().unwrap_or(0);
        for &b in push {
            parser.feed_byte(b, u64::MAX);
        }
        let took = crate::release_scanner::thread_cpu_ns()
            .unwrap_or(0)
            .saturating_sub(before);
        cpu += took;
        push_cpu_max = push_cpu_max.max(took);
        push_units_max = push_units_max.max(parser.spent() - before_units);
    }
    (parser.spent(), cpu, push_units_max, push_cpu_max)
}

/// Scenario: Not a gate. Price every recorded launch trace and each pathological trace in Q25's work units, fed byte by byte with no limit, and print the units per generation, the most units in one push and the CPU nanoseconds each unit cost, so the scanner's and the evidence parser's work limits can be set from measured figures.
#[test]
#[ignore = "measurement, run by hand in the release profile (Q25)"]
fn scanner_budget_calibration() {
    println!("work-unit calibration, {}", load_and_cores());
    println!(
        "limits: scanner total {} units, scanner and evidence per push {} units",
        crate::release_scanner::SCANNER_WORK_TOTAL,
        crate::release_scanner::SCANNER_WORK_PER_PUSH
    );
    let mut launch_total_max = 0u64;
    let mut launch_push_max = 0u64;
    let mut launch_ns_per_unit_max = 0f64;
    for (rows, cols) in [(50u16, 200u16), (80, 300)] {
        for trace in traces() {
            let mut best = (u64::MAX, u64::MAX);
            let mut units = 0;
            let mut push_max = 0;
            for _ in 0..5 {
                let mut parser = crate::vt_work::MeteredParser::new(rows, cols, 0);
                let mut cpu = 0;
                let mut pm = 0;
                for chunk in chunks(&trace.bytes, &trace.read_lens) {
                    let before_units = parser.spent();
                    let before = crate::release_scanner::thread_cpu_ns().unwrap_or(0);
                    for &b in chunk {
                        parser.feed_byte(b, u64::MAX);
                    }
                    cpu += crate::release_scanner::thread_cpu_ns()
                        .unwrap_or(0)
                        .saturating_sub(before);
                    pm = pm.max(parser.spent() - before_units);
                }
                units = parser.spent();
                push_max = pm;
                best = best.min((cpu, 0));
            }
            let ns_per_unit = best.0 as f64 / units.max(1) as f64;
            launch_total_max = launch_total_max.max(units);
            launch_push_max = launch_push_max.max(push_max);
            launch_ns_per_unit_max = launch_ns_per_unit_max.max(ns_per_unit);
            println!(
                "{:<48} @{rows}x{cols}: units={units:>10} push_units_max={push_max:>9} cpu_ms={:.2} ns/unit={ns_per_unit:.3}",
                trace.name,
                best.0 as f64 / 1e6
            );
        }
    }
    println!(
        "LAUNCHES: most units per generation {launch_total_max} ({:.1}% of the total limit), most units in one push {launch_push_max} ({:.1}% of the per-push limit), worst ns/unit {launch_ns_per_unit_max:.3}",
        100.0 * launch_total_max as f64 / crate::release_scanner::SCANNER_WORK_TOTAL as f64,
        100.0 * launch_push_max as f64 / crate::release_scanner::SCANNER_WORK_PER_PUSH as f64,
    );
    let mut worst = 0f64;
    for (name, rows, cols, bytes) in pathological() {
        // Priced to the end with no limit, at the reader's 8192-byte pushes.
        let (units, cpu, push_units, push_cpu) = units_and_cpu(&bytes, rows, cols, 8192);
        let ns_per_unit = cpu as f64 / units.max(1) as f64;
        let push_ns_per_unit = push_cpu as f64 / push_units.max(1) as f64;
        worst = worst.max(ns_per_unit).max(push_ns_per_unit);
        println!(
            "PATHOLOGICAL {name:<34} @{rows}x{cols}: units={units:>13} cpu_ms={:>8.1} ns/unit={ns_per_unit:.3} | push units max={push_units} cpu_ms max={:.2}",
            cpu as f64 / 1e6,
            push_cpu as f64 / 1e6,
        );
    }
    let worst = worst.max(launch_ns_per_unit_max);
    println!(
        "WORST ns/unit {worst:.3}: per-push limit = {:.2} ms, total limit = {:.1} ms at that cost",
        crate::release_scanner::SCANNER_WORK_PER_PUSH as f64 * worst / 1e6,
        crate::release_scanner::SCANNER_WORK_TOTAL as f64 * worst / 1e6,
    );
}
