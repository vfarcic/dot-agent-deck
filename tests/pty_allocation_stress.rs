#![cfg(unix)]

//! Temporary, opt-in diagnostic for #1884, never a normal-tier workload.
//! No deck daemon, agent, credentials or shared state is used. The allocation,
//! reader/writer duplication and owned teardown match portable-pty's deck path;
//! this deliberately excludes child spawning and the deck's background reader.

use portable_pty::{NativePtySystem, PtySize, PtySystem};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};

#[derive(Default, Debug, Serialize, Deserialize)]
struct Counts {
    attempts: usize,
    opened: usize,
    errors: BTreeMap<String, usize>,
}

impl Counts {
    fn error(&mut self, stage: &str, error: impl std::fmt::Display) {
        *self.errors.entry(format!("{stage}: {error}")).or_default() += 1;
    }

    fn merge(&mut self, other: Self) {
        self.attempts += other.attempts;
        self.opened += other.opened;
        for (error, count) in other.errors {
            *self.errors.entry(error).or_default() += count;
        }
    }
}

fn allocation_loop(iterations: usize) -> Counts {
    let system = NativePtySystem::default();
    let mut counts = Counts::default();
    for iteration in 0..iterations {
        counts.attempts += 1;
        let pair = match system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(pair) => pair,
            Err(error) => {
                counts.error("openpty", error);
                continue;
            }
        };
        counts.opened += 1;
        if let Err(error) = pair.master.get_size() {
            counts.error("get_size", error);
        }
        let writer = pair.master.take_writer();
        let reader = pair.master.try_clone_reader();
        if let Err(error) = &writer {
            counts.error("take_writer", error);
        }
        if let Err(error) = &reader {
            counts.error("try_clone_reader", error);
        }
        // Both orders occur in real teardown: the reader may outlive the
        // registry's master and writer. No read can block this diagnostic.
        if iteration % 2 == 0 {
            drop(pair.slave);
            drop(writer);
            drop(pair.master);
            drop(reader);
        } else {
            drop(reader);
            drop(writer);
            drop(pair);
        }
    }
    counts
}

fn threads(workers: usize, iterations: usize, churn: bool) -> Counts {
    let start = Arc::new(Barrier::new(workers + usize::from(churn)));
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let churner = churn.then(|| {
            let start = start.clone();
            let stop = &stop;
            scope.spawn(move || {
                start.wait();
                let mut cycles = 0;
                while !stop.load(Ordering::Acquire) {
                    let files: Vec<_> = (0..16)
                        .map(|_| File::open("/dev/null").expect("unrelated fd churn"))
                        .collect();
                    drop(files);
                    cycles += 1;
                }
                cycles
            })
        });
        let jobs: Vec<_> = (0..workers)
            .map(|_| {
                let start = start.clone();
                scope.spawn(move || {
                    start.wait();
                    allocation_loop(iterations)
                })
            })
            .collect();
        // Join without unwrapping until the churn thread has been stopped,
        // including when an allocation worker panics.
        let outcomes: Vec<_> = jobs.into_iter().map(|job| job.join()).collect();
        stop.store(true, Ordering::Release);
        if let Some(churner) = churner {
            println!("PTY_STRESS_CHURN_CYCLES={}", churner.join().unwrap());
        }
        let mut counts = Counts::default();
        for outcome in outcomes {
            counts.merge(outcome.unwrap());
        }
        counts
    })
}

fn child(mode: &str, iterations: usize) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "pty_allocation_stress_worker", "--nocapture"])
        .env("DAD_PTY_STRESS_WORKER", mode)
        .env("DAD_PTY_STRESS_ITERATIONS", iterations.to_string())
        .env("OTEL_SDK_DISABLED", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the isolated diagnostic test process")
}

fn finish(child: std::process::Child) -> Counts {
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "diagnostic child failed: {output:?}"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    println!("{stdout}");
    let result = stdout
        .lines()
        .find_map(|line| line.split_once("PTY_STRESS_RESULT=").map(|(_, json)| json))
        .expect("child must return its measured counts");
    serde_json::from_str(result).unwrap()
}

fn positive_control(iterations: usize) -> Counts {
    let mut counts = Counts::default();
    for _ in 0..iterations {
        // Create a known stale number, without leaving a Rust owner behind.
        // The child process has no other descriptor-allocating thread.
        let old = File::open("/dev/null").unwrap().into_raw_fd();
        // SAFETY: ownership was transferred above, and this is its first close.
        assert_eq!(unsafe { libc::close(old) }, 0);
        // This is the first stage of Apple's openpty implementation. Pause
        // before grantpt so a second thread can close the recycled number.
        // SAFETY: posix_openpt returns a fresh descriptor, owned here as raw.
        let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        assert!(master >= 0, "positive-control master allocation failed");
        assert_eq!(master, old, "positive control requires observed fd reuse");
        std::thread::spawn(move || {
            // SAFETY: deliberate stale-number close, synchronized with its
            // sole current owner. There is no Rust owner and no competing fd
            // allocation; the master is not used until this thread joins.
            assert_eq!(unsafe { libc::close(old) }, 0);
        })
        .join()
        .unwrap();
        counts.attempts += 1;
        // SAFETY: grantpt on a closed number returns an errno, not UB. Do not
        // close master again: the positive control already consumed its fd.
        assert_eq!(unsafe { libc::grantpt(master) }, -1);
        let error = std::io::Error::last_os_error();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
        counts.error("injected_stale_close/grantpt", error);
    }
    counts
}

fn iterations() -> usize {
    std::env::var("DAD_PTY_STRESS_ITERATIONS")
        .ok()
        .map(|value| value.parse().unwrap())
        .unwrap_or(25_000)
}

/// Scenario: only when explicitly requested, open and tear down PTYs from concurrent threads, with and without unrelated descriptor churn, and from separate processes. An isolated stale-fd double-close control checks the expected EBADF signature; each mode prints its own allocation and error counts.
#[test]
fn pty_allocation_stress() {
    if std::env::var("DAD_PTY_STRESS").as_deref() != Ok("1") {
        return;
    }
    let workers = 8;
    let iterations = iterations();
    println!("PTY_STRESS_CONFIG workers={workers} iterations={iterations}");
    for (mode, churn) in [("threads", false), ("threads-fd-churn", true)] {
        println!(
            "PTY_STRESS_MODE {mode} {:?}",
            threads(workers, iterations, churn)
        );
    }
    println!(
        "PTY_STRESS_MODE positive-control {:?}",
        finish(child("positive", 1000))
    );
    // Spawn all processes before waiting, so they really allocate concurrently.
    let children: Vec<_> = (0..workers)
        .map(|_| child("allocate", iterations))
        .collect();
    let mut counts = Counts::default();
    for process in children {
        counts.merge(finish(process));
    }
    println!("PTY_STRESS_MODE processes {counts:?}");
}

/// Scenario: an explicitly launched diagnostic subprocess allocates and closes PTYs, or deliberately closes one recycled master descriptor before grantpt. It prints structured counts for the parent and never starts a deck daemon or an agent.
#[test]
fn pty_allocation_stress_worker() {
    let Ok(mode) = std::env::var("DAD_PTY_STRESS_WORKER") else {
        return;
    };
    // Prove unrelated descriptors remain usable after the positive control.
    let sentinel = File::open("/dev/null").unwrap();
    let counts = match mode.as_str() {
        "allocate" => allocation_loop(iterations()),
        "positive" => positive_control(iterations()),
        _ => panic!("unknown diagnostic mode"),
    };
    // SAFETY: sentinel remains owned and open throughout this subprocess.
    assert_ne!(
        unsafe { libc::fcntl(sentinel.as_raw_fd(), libc::F_GETFD) },
        -1
    );
    println!(
        "PTY_STRESS_RESULT={}",
        serde_json::to_string(&counts).unwrap()
    );
}
