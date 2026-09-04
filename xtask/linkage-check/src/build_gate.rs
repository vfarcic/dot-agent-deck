//! Issue #863: `scripts/build-gate.sh` and `scripts/link-gate.sh`, the
//! machine-wide bound on concurrent linking.
//!
//! Two concurrent workspace builds from separate dispatch worktrees drove PSI
//! `io full avg300=65.95` — for two-thirds of a five-minute window every
//! runnable task on the box was blocked on disk — with `dm-0` at 100% and `ld
//! invoked oom-killer`. The gate is what bounds that. Its whole value is in
//! runtime behaviour, so a compile-time gate proves nothing about it: exactly
//! the shape of `clean_tmp.rs`, whose deletion-safety properties are runtime
//! assertions and nothing else.
//!
//! What has to hold is a bound AND a non-obstruction, and the second is the
//! reason for most of the tests below. A gate that fails a build, or hangs
//! one, is worse than the storm it prevents — so every rung of the degradation
//! ladder gets its own test: no `flock`, an unwritable pool directory, a
//! disabled or malformed slot count, a wait budget that expires. Each must run
//! the command anyway.
//!
//! Unix only. `flock(2)`, a POSIX shell, and the process semantics the
//! stale-lock test turns on are all Unix; the `linker` key these scripts hang
//! off is set for the Linux target triple alone, so Windows and macOS never
//! reach them.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

fn build_gate() -> PathBuf {
    repo_root().join("scripts/build-gate.sh")
}

fn link_gate() -> PathBuf {
    repo_root().join("scripts/link-gate.sh")
}

/// Same shape as `pin_lockstep`'s probe: say so loudly rather than failing a
/// contributor's unrelated change on a missing interpreter.
fn bash_present() -> bool {
    Command::new("bash")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn flock_present() -> bool {
    Command::new("flock")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A worker that stamps its entry and exit into one shared file, so a reader
/// can reconstruct how many ran at the same moment. Appends under `O_APPEND`,
/// which is atomic for writes this small, so the trace needs no locking of its
/// own — locking the observer would defeat the thing being observed.
fn write_worker(dir: &Path, hold: &str) -> PathBuf {
    let worker = dir.join("worker.sh");
    fs::write(
        &worker,
        format!(
            "#!/usr/bin/env bash\n\
             trace=\"$1\"\n\
             echo \"+ $(date +%s%N)\" >> \"$trace\"\n\
             sleep {hold}\n\
             echo \"- $(date +%s%N)\" >> \"$trace\"\n"
        ),
    )
    .expect("write the worker script");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755))
        .expect("make the worker executable");
    worker
}

/// The high-water mark of simultaneous workers, replayed from a trace file.
fn peak_concurrency(trace: &Path) -> usize {
    let body = fs::read_to_string(trace).unwrap_or_default();
    let mut events: Vec<(u128, i32)> = body
        .lines()
        .filter_map(|line| {
            let (sign, stamp) = line.split_once(' ')?;
            let at: u128 = stamp.trim().parse().ok()?;
            // A close at the same nanosecond as an open must be counted first,
            // so a boundary tie can never invent an overlap that did not
            // happen. `-1` sorting before `1` does that.
            Some((at, if sign == "+" { 1 } else { -1 }))
        })
        .collect();
    events.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let (mut live, mut peak) = (0i32, 0i32);
    for (_, delta) in events {
        live += delta;
        peak = peak.max(live);
    }
    peak.max(0) as usize
}

fn started(trace: &Path) -> usize {
    fs::read_to_string(trace)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with('+'))
        .count()
}

/// Launch `count` gated workers at once and wait for all of them, returning the
/// trace path. `pool_dir` is the semaphore's home; giving each test its own
/// keeps them from contending with each other, or with a real build running on
/// the same machine.
fn run_gated_burst(dir: &Path, pool_dir: &Path, jobs: &str, count: usize, hold: &str) -> PathBuf {
    let worker = write_worker(dir, hold);
    let trace = dir.join("trace");
    fs::write(&trace, "").expect("create the trace file");

    let children: Vec<_> = (0..count)
        .map(|_| {
            Command::new("bash")
                .arg(build_gate())
                .args(["--pool", "t", "--jobs", jobs, "--"])
                .arg(&worker)
                .arg(&trace)
                .env("DAD_BUILD_GATE_DIR", pool_dir)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn a gated worker")
        })
        .collect();
    for mut c in children {
        c.wait().expect("wait for a gated worker");
    }
    trace
}

/// The bound itself: more work than slots must never exceed the slots.
#[test]
fn gate_holds_concurrency_at_the_slot_count() {
    if !bash_present() || !flock_present() {
        eprintln!("SKIP: the build gate needs `bash` and `flock` on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let pool = tempfile::tempdir().expect("pool dir");
    let trace = run_gated_burst(dir.path(), pool.path(), "3", 12, "0.3");

    assert_eq!(
        started(&trace),
        12,
        "every job must run — a gate that drops work is worse than no gate"
    );
    let peak = peak_concurrency(&trace);
    assert!(
        peak <= 3,
        "12 jobs against 3 slots peaked at {peak} concurrent; the bound did not hold"
    );
    assert!(
        peak > 1,
        "peaked at {peak}: the gate serialised to one at a time when 3 slots were configured"
    );
}

/// A single slot is a legitimate configured value and must genuinely serialise
/// — the one case where "one at a time" is correct rather than a bug.
#[test]
fn one_slot_serialises() {
    if !bash_present() || !flock_present() {
        eprintln!("SKIP: needs `bash` and `flock` on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let pool = tempfile::tempdir().expect("pool dir");
    let trace = run_gated_burst(dir.path(), pool.path(), "1", 5, "0.2");
    assert_eq!(started(&trace), 5, "every job must still run");
    assert_eq!(
        peak_concurrency(&trace),
        1,
        "`--jobs 1` must mean one at a time"
    );
}

/// The gate is transparent to the command's exit status, so a failing build
/// still reads as a failing build.
#[test]
fn exit_status_passes_through() {
    if !bash_present() || !flock_present() {
        eprintln!("SKIP: needs `bash` and `flock` on PATH");
        return;
    }
    let pool = tempfile::tempdir().expect("pool dir");
    let out = Command::new("bash")
        .arg(build_gate())
        .args(["--pool", "t", "--jobs", "2", "--", "bash", "-c", "exit 7"])
        .env("DAD_BUILD_GATE_DIR", pool.path())
        .output()
        .expect("run the gate");
    assert_eq!(
        out.status.code(),
        Some(7),
        "the gate must report the command's own status, got: {}",
        combined(&out)
    );
}

/// The kill switch. `DAD_LINK_JOBS=0` reaches the gate as `--jobs 0`, and a
/// contributor who sets it is entitled to the unbounded behaviour they had
/// before — so this asserts the bound is really gone, not merely widened.
#[test]
fn zero_slots_runs_ungated() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let pool = tempfile::tempdir().expect("pool dir");
    let trace = run_gated_burst(dir.path(), pool.path(), "0", 8, "0.3");
    assert_eq!(started(&trace), 8);
    assert!(
        peak_concurrency(&trace) > 3,
        "`--jobs 0` must disable the gate, but the burst never ran wide"
    );
}

/// A typo in the slot count is a misconfiguration, not a reason to stop
/// building: it says so on stderr and runs anyway.
#[test]
fn non_numeric_slot_count_warns_and_runs_ungated() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let pool = tempfile::tempdir().expect("pool dir");
    let out = Command::new("bash")
        .arg(build_gate())
        .args([
            "--pool", "t", "--jobs", "lots", "--", "bash", "-c", "echo ran",
        ])
        .env("DAD_BUILD_GATE_DIR", pool.path())
        .output()
        .expect("run the gate");
    let text = combined(&out);
    assert!(out.status.success(), "must still run the command: {text}");
    assert!(text.contains("ran"), "the command did not run: {text}");
    assert!(
        text.contains("not a number"),
        "a malformed slot count must be reported: {text}"
    );
}

/// A pool directory that cannot be created — a read-only or full filesystem,
/// a `noexec`/`nosuid` mount, a stale `DAD_BUILD_GATE_DIR` — degrades to an
/// ungated run rather than a failed one.
#[test]
fn unwritable_pool_directory_runs_ungated() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("SKIP: root ignores the directory permissions this test relies on");
        return;
    }
    let parent = tempfile::tempdir().expect("pool parent");
    fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o555))
        .expect("make the pool parent read-only");
    let out = Command::new("bash")
        .arg(build_gate())
        .args(["--pool", "t", "--jobs", "2", "--", "bash", "-c", "echo ran"])
        .env("DAD_BUILD_GATE_DIR", parent.path().join("nested"))
        .output()
        .expect("run the gate");
    // Restore before the assertion so the TempDir can always clean itself up.
    let _ = fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o755));
    let text = combined(&out);
    assert!(out.status.success(), "must still run the command: {text}");
    assert!(text.contains("ran"), "the command did not run: {text}");
}

/// No `flock` on PATH is the one dependency the gate has, and it is not
/// universal — a slim container, a BSD, a stripped image. The build must not
/// notice.
#[test]
fn missing_flock_runs_ungated() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).expect("create the stub bin dir");
    // A PATH holding everything the script uses EXCEPT flock, so the test
    // proves the flock probe is what degraded and not some other missing tool.
    for tool in [
        "bash", "date", "awk", "id", "mkdir", "nproc", "sleep", "echo",
    ] {
        if let Ok(found) = which(tool) {
            let _ = std::os::unix::fs::symlink(found, bin.join(tool));
        }
    }
    let pool = tempfile::tempdir().expect("pool dir");
    let out = Command::new("bash")
        .arg(build_gate())
        .args(["--pool", "t", "--jobs", "2", "--", "bash", "-c", "echo ran"])
        .env("PATH", &bin)
        .env("DAD_BUILD_GATE_DIR", pool.path())
        .output()
        .expect("run the gate");
    let text = combined(&out);
    assert!(out.status.success(), "must still run the command: {text}");
    assert!(text.contains("ran"), "the command did not run: {text}");
    assert!(
        !pool.path().join("t").exists(),
        "with no flock the gate must not have started building a pool"
    );
}

fn which(tool: &str) -> Result<PathBuf, ()> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .output()
        .map_err(|_| ())?;
    if !out.status.success() {
        return Err(());
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() {
        Err(())
    } else {
        Ok(PathBuf::from(p))
    }
}

/// The bound on how long the gate may ever delay a build. A pool wedged by some
/// future bug — or simply by work that outlives the budget — must cost a delay
/// and a warning, never a hang and never a failure. This is what keeps a failed
/// acquisition from turning into a failed dispatch.
#[test]
fn expired_wait_budget_runs_ungated_rather_than_waiting_forever() {
    if !bash_present() || !flock_present() {
        eprintln!("SKIP: needs `bash` and `flock` on PATH");
        return;
    }
    let pool = tempfile::tempdir().expect("pool dir");
    // Occupy the single slot for far longer than the waiter's budget.
    let mut holder = Command::new("bash")
        .arg(build_gate())
        .args(["--pool", "t", "--jobs", "1", "--", "sleep", "60"])
        .env("DAD_BUILD_GATE_DIR", pool.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the slot holder");
    // Let the holder actually take the slot before the waiter starts.
    std::thread::sleep(std::time::Duration::from_millis(800));

    let began = std::time::Instant::now();
    let out = Command::new("bash")
        .arg(build_gate())
        .args([
            "--pool", "t", "--jobs", "1", "--wait", "2", "--", "bash", "-c", "echo ran",
        ])
        .env("DAD_BUILD_GATE_DIR", pool.path())
        .output()
        .expect("run the waiter");
    let waited = began.elapsed();

    let _ = holder.kill();
    let _ = holder.wait();
    kill_pool_holders(pool.path());

    let text = combined(&out);
    assert!(out.status.success(), "the waiter must not fail: {text}");
    assert!(
        text.contains("ran"),
        "the waiter never ran its command: {text}"
    );
    assert!(
        text.contains("running ungated"),
        "the waiter must say it gave up on a slot: {text}"
    );
    assert!(
        waited < std::time::Duration::from_secs(45),
        "a 2s budget waited {waited:?}; the budget is what bounds the hang"
    );
}

/// The property that makes a lock FILE the wrong tool and `flock(2)` the right
/// one: there is no stale-lock state to recover, because the kernel drops the
/// lock when the last descriptor closes — SIGKILL and OOM-kill included. The
/// storm this gate exists for ends in an OOM kill, so this is the failure mode
/// most likely to be exercised in anger.
#[test]
fn a_sigkilled_holder_leaves_no_stale_slot() {
    if !bash_present() || !flock_present() {
        eprintln!("SKIP: needs `bash` and `flock` on PATH");
        return;
    }
    let pool = tempfile::tempdir().expect("pool dir");
    let mut holder = Command::new("bash")
        .arg(build_gate())
        .args(["--pool", "t", "--jobs", "1", "--", "sleep", "60"])
        .env("DAD_BUILD_GATE_DIR", pool.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the slot holder");
    std::thread::sleep(std::time::Duration::from_millis(800));

    // SIGKILL the whole holder tree. The lock lives on an open descriptor that
    // `flock` passes down to its child, so the slot is only free once every
    // process holding a copy is gone — which is the correct semantics, and the
    // reason this kills the descendants rather than just the one pid.
    let _ = holder.kill();
    let _ = holder.wait();
    kill_pool_holders(pool.path());

    let began = std::time::Instant::now();
    let out = Command::new("bash")
        .arg(build_gate())
        .args([
            "--pool", "t", "--jobs", "1", "--wait", "20", "--", "bash", "-c", "echo ran",
        ])
        .env("DAD_BUILD_GATE_DIR", pool.path())
        .output()
        .expect("run the successor");
    let waited = began.elapsed();
    let text = combined(&out);

    assert!(out.status.success(), "the successor must run: {text}");
    assert!(text.contains("ran"), "the successor never ran: {text}");
    assert!(
        !text.contains("running ungated"),
        "the successor fell through the wait budget instead of taking a freed \
         slot — the killed holder left the slot locked: {text}"
    );
    assert!(
        waited < std::time::Duration::from_secs(10),
        "took {waited:?} to reclaim a slot whose holder was killed"
    );
}

/// SIGKILL every process still holding a descriptor on one of this pool's slot
/// files. `holder.kill()` reaches only the `bash` that fronts the gate; `flock`
/// and the command it launched are separate processes that inherited the
/// locked descriptor.
fn kill_pool_holders(pool: &Path) {
    let Ok(procs) = fs::read_dir("/proc") else {
        return;
    };
    let needle = pool.to_string_lossy().to_string();
    for entry in procs.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let holds = fds.flatten().any(|fd| {
            fs::read_link(fd.path())
                .map(|t| t.to_string_lossy().contains(&needle))
                .unwrap_or(false)
        });
        if holds {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
}

/// A caller's own bug — no command, or an unknown flag — is the one thing the
/// gate reports rather than papers over. It is caught the first time the
/// wrapper runs, so it cannot reach a contributor as a silent no-op.
#[test]
fn caller_errors_are_reported() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    for args in [
        // No command at all.
        vec!["--pool", "t", "--jobs", "2"],
        // No pool.
        vec!["--jobs", "2", "--", "true"],
        // An option nobody defined.
        vec!["--nonsense", "--", "true"],
        // A pool name that would climb out of the gate directory.
        vec!["--pool", "../../etc", "--jobs", "2", "--", "true"],
    ] {
        let out = Command::new("bash")
            .arg(build_gate())
            .args(&args)
            .output()
            .expect("run the gate");
        assert!(
            !out.status.success(),
            "`{}` should have been rejected: {}",
            args.join(" "),
            combined(&out)
        );
    }
}

// ---------------------------------------------------------------------------
// The linker seam
// ---------------------------------------------------------------------------

/// A stand-in linker that records the argv it was handed.
fn write_recorder(dir: &Path) -> PathBuf {
    let rec = dir.join("recorder.sh");
    fs::write(
        &rec,
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > \"$RECORD_TO\"\nexit 0\n",
    )
    .expect("write the recorder");
    fs::set_permissions(&rec, fs::Permissions::from_mode(0o755)).expect("chmod the recorder");
    rec
}

/// rustc's argv must reach the real linker byte for byte. A wrapper that
/// reorders, drops or re-quotes an argument produces link failures that look
/// like compiler bugs, so this pins transparency rather than trusting it.
#[test]
fn link_gate_forwards_rustc_argv_verbatim() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let pool = tempfile::tempdir().expect("pool dir");
    let rec = write_recorder(dir.path());
    let record_to = dir.path().join("argv");
    // Shapes rustc really emits, including one with a space and one empty.
    let args = [
        "-Wl,--as-needed",
        "-o",
        "/tmp/a b/target/debug/deps/test-1234",
        "-nodefaultlibs",
        "",
        "-Wl,-Bstatic",
    ];
    let out = Command::new("bash")
        .arg(link_gate())
        .args(args)
        .env("DAD_LINKER", &rec)
        .env("RECORD_TO", &record_to)
        .env("DAD_BUILD_GATE_DIR", pool.path())
        .output()
        .expect("run the link gate");
    assert!(out.status.success(), "link gate failed: {}", combined(&out));

    let seen = fs::read_to_string(&record_to).expect("the recorder wrote nothing");
    let seen: Vec<&str> = seen
        .strip_suffix('\n')
        .unwrap_or(&seen)
        .split('\n')
        .collect();
    assert_eq!(
        seen,
        args.to_vec(),
        "the linker's argv was not forwarded verbatim"
    );
}

/// A partial checkout, or a `noexec` mount, must not turn every link into a
/// hard error. With the semaphore unreachable the seam still links.
#[test]
fn link_gate_links_anyway_when_the_semaphore_is_missing() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    // A copy of the seam with NO build-gate.sh beside it.
    let lone = dir.path().join("link-gate.sh");
    fs::copy(link_gate(), &lone).expect("copy the link gate");
    fs::set_permissions(&lone, fs::Permissions::from_mode(0o755)).expect("chmod the copy");
    let rec = write_recorder(dir.path());
    let record_to = dir.path().join("argv");

    let out = Command::new("bash")
        .arg(&lone)
        .arg("-o")
        .arg("/dev/null")
        .env("DAD_LINKER", &rec)
        .env("RECORD_TO", &record_to)
        .output()
        .expect("run the orphaned link gate");
    assert!(
        out.status.success(),
        "a missing semaphore must not fail the link: {}",
        combined(&out)
    );
    assert_eq!(
        fs::read_to_string(&record_to).expect("the recorder wrote nothing"),
        "-o\n/dev/null\n",
        "the real linker was not reached"
    );
}

/// The computed default has to be a usable number on whatever machine it lands
/// on, not a value tuned to the 16-core box in the report. The gate pre-creates
/// one slot file per configured slot, so the pool directory is a direct readout
/// of what the seam decided.
#[test]
fn link_gate_default_slot_count_is_sane_for_this_machine() {
    if !bash_present() || !flock_present() {
        eprintln!("SKIP: needs `bash` and `flock` on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let pool = tempfile::tempdir().expect("pool dir");
    let rec = write_recorder(dir.path());

    let out = Command::new("bash")
        .arg(link_gate())
        .arg("-o")
        .arg("/dev/null")
        .env("DAD_LINKER", &rec)
        .env("RECORD_TO", dir.path().join("argv"))
        .env("DAD_BUILD_GATE_DIR", pool.path())
        .env_remove("DAD_LINK_JOBS")
        .output()
        .expect("run the link gate");
    assert!(out.status.success(), "link gate failed: {}", combined(&out));

    let slots = fs::read_dir(pool.path().join("link"))
        .expect("the link pool was never created")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("slot."))
        .count();
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    assert!(
        slots >= 1,
        "the default resolved to {slots} slots; zero would read as 'disabled'"
    );
    assert!(
        slots <= cpus,
        "the default resolved to {slots} slots on a {cpus}-core machine — more \
         linkers than cores cannot help and is what the clamp exists to stop"
    );
}

/// The pool's whole value is that every build on the box finds the same one, so
/// the default path must not vary per process. `TMPDIR` does — this
/// repository's own e2e harness relocates it — and deriving the pool from it
/// would hand two agents two private pools that each look like they are
/// working while bounding nothing.
#[test]
fn default_pool_location_ignores_tmpdir() {
    if !bash_present() || !flock_present() {
        eprintln!("SKIP: needs `bash` and `flock` on PATH");
        return;
    }
    let fake_tmp = tempfile::tempdir().expect("a TMPDIR to be ignored");
    let out = Command::new("bash")
        .arg(build_gate())
        .args(["--pool", "tmpdirprobe", "--jobs", "2", "--", "true"])
        .env("TMPDIR", fake_tmp.path())
        .env_remove("DAD_BUILD_GATE_DIR")
        .output()
        .expect("run the gate");
    assert!(out.status.success(), "gate failed: {}", combined(&out));
    assert!(
        !fake_tmp.path().join("tmpdirprobe").exists(),
        "the pool followed $TMPDIR, so two processes with different TMPDIRs \
         would each get a private pool and bound nothing"
    );
    // Cleaned up by hand: this one deliberately lands in the real, shared pool
    // directory, which no TempDir owns.
    let _ = fs::remove_dir_all(
        PathBuf::from("/tmp")
            .join(format!("dad-build-gate-{}", unsafe { libc::geteuid() }))
            .join("tmpdirprobe"),
    );
}

/// A slot count far above any plausible core count is not a bound, and
/// honouring it literally would create that many files before discovering as
/// much.
#[test]
fn absurd_slot_count_runs_ungated_without_creating_files() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let pool = tempfile::tempdir().expect("pool dir");
    let out = Command::new("bash")
        .arg(build_gate())
        .args([
            "--pool", "t", "--jobs", "100000", "--", "bash", "-c", "echo ran",
        ])
        .env("DAD_BUILD_GATE_DIR", pool.path())
        .output()
        .expect("run the gate");
    let text = combined(&out);
    assert!(out.status.success(), "must still run the command: {text}");
    assert!(text.contains("ran"), "the command did not run: {text}");
    assert!(
        !pool.path().join("t").exists(),
        "100000 slot files were created before the gate noticed the count was \
         not a bound"
    );
}

/// The wiring itself. Everything above tests scripts that nothing would invoke
/// if `.cargo/config.toml` stopped naming them — the same reason
/// `pin_lockstep` runs its script against the real repository.
#[test]
fn cargo_config_routes_linux_links_through_the_gate() {
    let cfg = fs::read_to_string(repo_root().join(".cargo/config.toml"))
        .expect("read .cargo/config.toml");
    assert!(
        cfg.contains(r#"linker = "./scripts/link-gate.sh""#),
        "`.cargo/config.toml` no longer routes the Linux target's linker \
         through scripts/link-gate.sh, so the gate is wired to nothing"
    );
    assert!(
        cfg.contains("[target.x86_64-unknown-linux-gnu]"),
        "the linker key must stay under the Linux triple — Windows and macOS \
         have no shell to run a .sh linker wrapper, and both are required checks"
    );
    for script in [build_gate(), link_gate()] {
        let mode = fs::metadata(&script)
            .unwrap_or_else(|e| panic!("{} is missing: {e}", script.display()))
            .permissions()
            .mode();
        assert!(
            mode & 0o111 != 0,
            "{} is not executable; cargo would fail every link on Linux",
            script.display()
        );
    }
}
