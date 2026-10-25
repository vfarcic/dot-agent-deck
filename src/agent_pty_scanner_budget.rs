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
//! - **the pathological trace**: 256 KiB of erase-display alternating with
//!   rows that hit both header anchors, at 512 × 512, pushed 8192 bytes at a
//!   time; its maximum per-push figure is printed as a number.
//! - resizes are timed on their own and charged to no push.

use super::*;

const RUNS: usize = 15;

struct Trace {
    name: String,
    profile: &'static crate::doorbell_screen::ScreenProfile,
    bytes: Vec<u8>,
    read_lens: Vec<usize>,
}

fn traces() -> Vec<Trace> {
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

fn chunks<'a>(bytes: &'a [u8], lens: &'a [usize]) -> impl Iterator<Item = &'a [u8]> + 'a {
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
}

fn measure(trace: &Trace, rows: u16, cols: u16) -> Row {
    let mut added = Vec::new();
    let mut scanner_all = Vec::new();
    let mut cpu_max = 0;
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
    }
    added.sort_unstable();
    scanner_all.sort_unstable();
    Row {
        pushes: trace.read_lens.len(),
        added,
        scanner: scanner_all,
        cpu_max,
    }
}

fn print_row(label: &str, row: &Row) {
    println!(
        "{label:<58} pushes={:>4} runs={RUNS} added_hold_us p99={:>7} max={:>7} | scanner_us p99={:>7} max={:>7} | cpu_ms/gen max={:.2}",
        row.pushes,
        us(nearest_rank(&row.added, 99.0)),
        us(*row.added.last().unwrap_or(&0)),
        us(nearest_rank(&row.scanner, 99.0)),
        us(*row.scanner.last().unwrap_or(&0)),
        row.cpu_max as f64 / 1e6,
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
                        replay(&bus, &t.bytes, &t.read_lens)
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
        bus.push_timed(traces[0].bytes.clone());
        for (r, c) in [(80u16, 300u16), (50, 200), (512, 512), (24, 80)] {
            let _ = bus.resize_and_clear(r, c, || Ok::<(), ()>(()));
        }
        resize_max = resize_max.max(bus.state.lock().unwrap().release.cost().resize_ns_max);
    }
    println!(
        "resize of the scanner's grid: max {} us",
        us(resize_max as i64)
    );

    // The pathological trace.
    let mut patho = Vec::new();
    let mut i = 0u32;
    while patho.len() < window {
        patho.extend_from_slice(b"\x1b[2J");
        patho.extend_from_slice(
            format!(
                "\x1b[2;12HClaude Code v{}.\x1b[2;6HOpenAI Codex (v0.{i}\r\n",
                i % 10
            )
            .as_bytes(),
        );
        i += 1;
    }
    patho.truncate(window);
    let lens: Vec<usize> = patho.chunks(8192).map(<[u8]>::len).collect();
    let mut added_p = Vec::new();
    let mut scanner_p = Vec::new();
    let mut cpu_p = 0;
    for run in 0..3 {
        let with = AgentBus::profiled(None, 512, 512);
        let without = AgentBus::profiled_without_scanner(None, 512, 512);
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
    }
    added_p.sort_unstable();
    scanner_p.sort_unstable();
    println!(
        "pathological 256 KiB @512x512 (unprofiled bus): added_hold_us p99={} max={} | scanner_us max={} | cpu_ms/gen={:.1}",
        us(nearest_rank(&added_p, 99.0)),
        us(*added_p.last().unwrap_or(&0)),
        us(*scanner_p.last().unwrap_or(&0)),
        cpu_p as f64 / 1e6
    );

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
    let ev_p = evidence(
        &crate::doorbell_screen::CLAUDE_PROFILE,
        &patho,
        &lens,
        512,
        512,
        3,
    );
    println!(
        "evidence parser, pathological 256 KiB @512x512: added_hold_us p99={} max={}",
        us(nearest_rank(&ev_p, 99.0)),
        us(*ev_p.last().unwrap_or(&0)),
    );
}
