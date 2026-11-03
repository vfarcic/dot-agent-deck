//! PRD #1666 part 3 (Q35-20a): the Linux reader's cost with no frontier
//! asked for, against the blocking loop it replaced. Not a gate: it prints
//! figures, and is run by hand in the shipped (release) profile:
//!
//! ```text
//! cargo test --release --lib frontier_throughput -- --ignored --nocapture --test-threads 1
//! ```
//!
//! Both readers read a real PTY through the same portable-pty reader into a
//! real [`AgentBus`], alternating, [`RUNS`] times each:
//!
//! - **burst**: a child writes [`BURST`] bytes as fast as it can; the figure
//!   is the wall time from spawn to the reader's end, and the read count.
//! - **round trip**: a child in raw mode echoes each byte it is sent; the
//!   figure is the time from writing a byte to the master to its echo
//!   reaching a bus subscriber, over [`TRIPS`] trips, p50 and p99.

use std::io::Write as _;
use std::time::{Duration, Instant};

use super::frontier_reader::Frontier;
use super::*;

const RUNS: usize = 7;
const BURST: usize = 256 * 1024 * 1024;
const TRIPS: usize = 2_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reader {
    /// The blocking loop, as before Q35-20a.
    Plain,
    /// The poll loop, with no request.
    Frontier,
}

struct Pty {
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    opened: Option<u64>,
}

fn spawn(argv: &[&str]) -> Pty {
    let opened = crate::composer_probe::master_holders::boot_ticks();
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(argv[0]);
    command.args(&argv[1..]);
    let child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    Pty {
        master: pair.master,
        child,
        opened,
    }
}

/// Read `pty` to its end into `bus` with `reader`.
fn read(pty: &Pty, bus: &AgentBus, reader: Reader) -> usize {
    let source = pty.master.try_clone_reader().unwrap();
    let mut reads = 0;
    let counted = |_: PushCost| reads += 1;
    match reader {
        Reader::Plain => {
            read_plain(source, bus, counted);
        }
        Reader::Frontier => {
            Frontier::setup(bus, "7", pty.master.as_raw_fd(), pty.opened, &[])
                .expect("frontier setup")
                .run(source, bus, counted);
        }
    }
    reads
}

fn burst(reader: Reader) -> (Duration, usize) {
    let start = Instant::now();
    let mut pty = spawn(&[
        "dd",
        "if=/dev/zero",
        "bs=65536",
        &format!("count={}", BURST / 65536),
        "status=none",
    ]);
    let bus = AgentBus::new();
    let reads = read(&pty, &bus, reader);
    let elapsed = start.elapsed();
    pty.child.wait().unwrap();
    (elapsed, reads)
}

fn round_trips(reader: Reader) -> Vec<Duration> {
    let mut pty = spawn(&["sh", "-c", "stty raw -echo; exec cat"]);
    let bus = Arc::new(AgentBus::new());
    let (_, mut rx) = bus.subscribe();
    let thread_bus = Arc::clone(&bus);
    let source = pty.master.try_clone_reader().unwrap();
    let frontier = match reader {
        Reader::Plain => None,
        Reader::Frontier => Frontier::setup(&bus, "7", pty.master.as_raw_fd(), pty.opened, &[]),
    };
    let thread = std::thread::spawn(move || match frontier {
        None => read_plain(source, &thread_bus, |_| {}),
        Some(frontier) => frontier.run(source, &thread_bus, |_| {}),
    });
    let mut writer = pty.master.take_writer().unwrap();
    // Wait out `stty`: a byte comes back unechoed once `cat` is reading.
    std::thread::sleep(Duration::from_millis(300));
    let mut trips = Vec::with_capacity(TRIPS);
    for _ in 0..TRIPS {
        let start = Instant::now();
        writer.write_all(b"x").unwrap();
        writer.flush().unwrap();
        loop {
            let chunk = rx.blocking_recv().unwrap();
            if chunk.contains(&b'x') {
                break;
            }
        }
        trips.push(start.elapsed());
    }
    pty.child.kill().unwrap();
    drop(writer);
    pty.child.wait().unwrap();
    thread.join().unwrap();
    trips
}

fn nearest_rank(sorted: &[Duration], pct: f64) -> Duration {
    let rank = ((pct / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[test]
#[ignore = "measurement, run by hand in the release profile (Q35-20a)"]
fn frontier_throughput() {
    let mut bursts: Vec<(Reader, Duration, usize)> = Vec::new();
    let mut trips: Vec<(Reader, Vec<Duration>)> = Vec::new();
    for _ in 0..RUNS {
        for reader in [Reader::Plain, Reader::Frontier] {
            let (elapsed, reads) = burst(reader);
            bursts.push((reader, elapsed, reads));
            trips.push((reader, round_trips(reader)));
        }
    }
    println!(
        "cores {:?}, load {}",
        std::thread::available_parallelism().ok(),
        std::fs::read_to_string("/proc/loadavg")
            .unwrap_or_default()
            .trim()
    );
    for reader in [Reader::Plain, Reader::Frontier] {
        let mut times: Vec<Duration> = bursts
            .iter()
            .filter(|(r, ..)| *r == reader)
            .map(|(_, t, _)| *t)
            .collect();
        times.sort();
        let reads: Vec<usize> = bursts
            .iter()
            .filter(|(r, ..)| *r == reader)
            .map(|(.., n)| *n)
            .collect();
        let median = times[times.len() / 2];
        println!(
            "{reader:?} burst {} MiB: median {median:?} ({:.0} MiB/s), min {:?}, max {:?}, reads {reads:?}",
            BURST >> 20,
            (BURST >> 20) as f64 / median.as_secs_f64(),
            times[0],
            times[times.len() - 1],
        );
        let mut all: Vec<Duration> = trips
            .iter()
            .filter(|(r, _)| *r == reader)
            .flat_map(|(_, t)| t.iter().copied())
            .collect();
        all.sort();
        println!(
            "{reader:?} round trip over {} trips: p50 {:?}, p99 {:?}, max {:?}",
            all.len(),
            nearest_rank(&all, 50.0),
            nearest_rank(&all, 99.0),
            all[all.len() - 1],
        );
    }
}

/// Processes started after the master was opened, the way a long-lived
/// generation's census sees every process started since its spawn:
/// `procs` single-threaded sleepers, and `threaded` processes of `threads`
/// tasks each (a Python interpreter whose threads sleep).
fn load(procs: usize, threaded: usize, threads: usize) -> Vec<std::process::Child> {
    let mut children = Vec::new();
    for _ in 0..procs {
        match std::process::Command::new("sleep").arg("900").spawn() {
            Ok(child) => children.push(child),
            Err(e) => {
                println!("load: sleeper spawn failed after {}: {e}", children.len());
                break;
            }
        }
    }
    let script = format!(
        "import threading, time\nfor _ in range({}):\n    threading.Thread(target=time.sleep, args=(900,), daemon=True).start()\ntime.sleep(900)\n",
        threads.saturating_sub(1)
    );
    for _ in 0..threaded {
        match std::process::Command::new("python3")
            .args(["-c", &script])
            .spawn()
        {
            Ok(child) => children.push(child),
            Err(e) => {
                println!("load: threaded spawn failed: {e}");
                break;
            }
        }
    }
    // Let every child reach its sleep and every thread start.
    std::thread::sleep(Duration::from_millis(1500));
    children
}

/// `/proc`'s process and task counts, and the load average.
fn host() -> String {
    let pids: Vec<u32> = std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    let tasks: usize = pids
        .iter()
        .map(|p| std::fs::read_dir(format!("/proc/{p}/task")).map_or(0, |d| d.count()))
        .sum();
    format!(
        "processes {}, tasks {tasks}, cores {:?}, load {}",
        pids.len(),
        std::thread::available_parallelism().ok(),
        std::fs::read_to_string("/proc/loadavg")
            .unwrap_or_default()
            .trim()
    )
}

/// PRD #1666 part 3 (Q35-20a, the census reader-stall budget): what a
/// requested frontier costs on the reader thread of an idle pane, from the
/// request (the wake's ring) to its outcome, and the `/proc` census alone
/// inside it, with more and more processes started after the master. Not a
/// gate: it prints figures, and is run by hand in the release profile:
///
/// ```text
/// cargo test --release --lib frontier_request_latency -- --ignored --nocapture --test-threads 1
/// ```
#[test]
#[ignore = "measurement, run by hand in the release profile (Q35-20a census budget)"]
fn frontier_request_latency() {
    use crate::composer_probe::master_holders::{holders, master_id};
    const REQUESTS: usize = 60;
    let mut pty = spawn(&["sh", "-c", "stty raw -echo; exec cat"]);
    let bus = Arc::new(AgentBus::new());
    let thread_bus = Arc::clone(&bus);
    let source = pty.master.try_clone_reader().unwrap();
    let frontier = Frontier::setup(&bus, "7", pty.master.as_raw_fd(), pty.opened, &[])
        .expect("frontier setup");
    let thread = std::thread::spawn(move || frontier.run(source, &thread_bus, |_| {}));
    let id = master_id(pty.master.as_raw_fd().unwrap()).unwrap();
    let since = pty.opened.unwrap();
    // Wait out `stty`.
    std::thread::sleep(Duration::from_millis(300));
    println!("before any load: {}", host());
    // The last condition is above the dev box this was written on (380
    // processes, 702 tasks, 378 descriptors in the user's own processes):
    // more processes, tasks and same-user descriptors. A sandbox stopped
    // when 1000 sleepers were added, so larger counts are left out.
    let conditions: [(&str, usize, usize, usize); 3] = [
        ("no added process", 0, 0, 0),
        ("400 sleepers", 400, 0, 0),
        ("400 sleepers + 40 processes x 16 threads", 400, 40, 16),
    ];
    let mut loaded = Vec::new();
    let mut added = (0, 0);
    for (label, procs, threaded, threads) in conditions {
        loaded.extend(load(procs - added.0, threaded - added.1, threads));
        added = (procs, threaded);
        let mut census = Vec::new();
        let mut found = String::new();
        for _ in 0..30 {
            let t = Instant::now();
            found = format!("{:?}", holders(&id, since));
            census.push(t.elapsed());
        }
        census.sort();
        let mut path = Vec::new();
        let (mut taken, mut missing) = (0, 0);
        for _ in 0..REQUESTS {
            let t = Instant::now();
            let request = bus.input().request_frontier(7, None);
            let outcome = loop {
                if let Some(outcome) = request.outcome() {
                    break outcome;
                }
                assert!(t.elapsed() < Duration::from_secs(30), "no outcome");
                std::thread::yield_now();
            };
            path.push(t.elapsed());
            match outcome {
                crate::composer_probe::record::FrontierOutcome::Taken { .. } => taken += 1,
                _ => missing += 1,
            }
        }
        path.sort();
        println!("{label}: {}", host());
        println!(
            "  census: p50 {:?}, p90 {:?}, max {:?}; {found}",
            nearest_rank(&census, 50.0),
            nearest_rank(&census, 90.0),
            census[census.len() - 1]
        );
        println!(
            "  requested frontier: p50 {:?}, p90 {:?}, p99 {:?}, max {:?}; taken {taken}, missing {missing}",
            nearest_rank(&path, 50.0),
            nearest_rank(&path, 90.0),
            nearest_rank(&path, 99.0),
            path[path.len() - 1]
        );
    }
    for mut child in loaded {
        let _ = child.kill();
        let _ = child.wait();
    }
    pty.child.kill().unwrap();
    pty.child.wait().unwrap();
    thread.join().unwrap();
}
