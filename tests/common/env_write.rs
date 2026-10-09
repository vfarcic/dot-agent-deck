//! Issue #1516: the check an in-process environment write in these tests makes
//! before it writes.
//!
//! `std::env::set_var` and `remove_var` race every thread that reads the
//! environment at the same moment, and libc code reads it without std's lock.
//! A Tokio runtime's worker and blocking threads run the deck's own code, which
//! reads its configuration knobs from the environment, so a test must not write
//! the environment while one exists. `delegate_prompt_injection.rs` did, three
//! times (`orchestration/delegate/011`, `/012` and `/025` re-pointed the
//! readiness buffer inside `block_on`), and so did `idle_worker_detector.rs`'s
//! `scheduler/idle-worker/003`. A test that needs to change a knob mid-run now
//! does it through `dot_agent_deck::env_override`, which is behind a lock.
//!
//! This makes the rule a runtime refusal rather than a comment: every
//! env-writing guard in those files calls [`assert_no_tokio_runtime`] first.
//! What it cannot see: a runtime built with a custom thread name (no test in
//! this tree names its runtime threads); a worker started an instant before the
//! check, which still carries its creator's name until it renames itself; and,
//! on a host without `/proc`, anything but the in-runtime half.
//!
//! **The thread scan runs only when this process is this test's alone**, which
//! is what nextest's process-per-test mode guarantees and what every gate here
//! uses ([`owns_the_process`]). Under plain `cargo test` a sibling test's
//! runtime shares the process, so the scan would refuse a write over threads
//! this test does not own; that sibling race is issue #245's, and only the
//! in-runtime half applies there.

/// Panic, naming `site`, if the calling thread is inside a Tokio runtime or (on
/// Linux) if any Tokio runtime thread is still alive in this process after
/// [`EXITING_THREAD_GRACE`].
///
/// A runtime's threads are joined when it drops (its blocking pool joins every
/// thread it started), so after the drop none of them runs user code again. A
/// joined thread can still be listed in `/proc/self/task` for a moment, though:
/// the kernel wakes the joiner when the thread releases its memory map, before
/// it removes the thread from the group. Measured here: the guards of
/// `delegate/025` and `idle-worker/001`, which drop after their runtime, saw one
/// or two such threads on a loaded box. So the scan waits for the listed
/// threads to disappear, and refuses only one that outlives the grace.
pub fn assert_no_tokio_runtime(site: &str) {
    assert!(
        tokio::runtime::Handle::try_current().is_err(),
        "{site}: writes the process environment from inside a Tokio runtime, \
         where the runtime's own threads read it concurrently (issue #1516). \
         Write it before the runtime is built, or change the knob through \
         `dot_agent_deck::env_override`"
    );
    if !owns_the_process() {
        return;
    }
    let deadline = std::time::Instant::now() + EXITING_THREAD_GRACE;
    let mut live = tokio_runtime_threads();
    while !live.is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
        live = tokio_runtime_threads();
    }
    assert!(
        live.is_empty(),
        "{site}: writes the process environment while Tokio runtime threads \
         exist ({}), and they read it concurrently (issue #1516). Write it \
         before the runtime is built or after it has dropped, or change the knob \
         through `dot_agent_deck::env_override`",
        live.join(", ")
    );
}

/// Whether this test runs in a process of its own, so that every thread in it
/// belongs to this test: nextest sets `NEXTEST_EXECUTION_MODE=process-per-test`
/// in each test process it starts. Plain `cargo test` sets nothing and runs the
/// whole binary's tests as threads of one process.
pub fn owns_the_process() -> bool {
    std::env::var_os("NEXTEST_EXECUTION_MODE").is_some_and(|mode| mode == "process-per-test")
}

/// How long [`assert_no_tokio_runtime`] waits for a listed runtime thread to
/// finish leaving. An exiting thread is gone within milliseconds; the margin is
/// for a starved box. A runtime that is genuinely alive keeps its threads, so it
/// is refused after this long rather than missed. A write that finds no runtime
/// thread listed waits for nothing.
pub const EXITING_THREAD_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The `tid:name` of every thread in this process that a Tokio runtime started,
/// recognised by Tokio's default thread name: `tokio-rt-worker` in the Tokio this
/// tree pins, `tokio-runtime-worker` (truncated by the kernel to 15 bytes) in
/// older releases. Empty where `/proc` is unavailable.
/// `env_write_refuses_while_runtime_threads_exist_and_not_after_the_runtime_drops`
/// fails if a Tokio upgrade renames them again.
pub fn tokio_runtime_threads() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
            return Vec::new();
        };
        let mut live: Vec<String> = tasks
            .filter_map(Result::ok)
            .filter_map(|task| {
                let name = std::fs::read_to_string(task.path().join("comm")).ok()?;
                let name = name.trim_end();
                (name.starts_with("tokio-rt-") || name.starts_with("tokio-runtime"))
                    .then(|| format!("{}:{name}", task.file_name().to_string_lossy()))
            })
            .collect();
        live.sort();
        live
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}
