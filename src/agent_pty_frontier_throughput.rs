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
}

fn spawn(argv: &[&str]) -> Pty {
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
            Frontier::setup(bus, "7", pty.master.as_raw_fd(), &[])
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
        Reader::Frontier => Frontier::setup(&bus, "7", pty.master.as_raw_fd(), &[]),
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
