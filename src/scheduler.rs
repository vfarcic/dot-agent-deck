//! Daemon-side scheduler primitive (PRD #127, Phase 1).
//!
//! Pure-data foundation only: cron-expression evaluation, a daemon-side tokio
//! firing loop, skip-if-prior-run-still-active, a manual run-now trigger, and
//! the reload-apply diff that replaces the registered task set from a freshly
//! loaded config. The daemon socket `ReloadSchedules` wiring, the idle
//! carve-out, the spawn primitive, and the UI are separate later tasks and are
//! intentionally NOT implemented here — this module is registered into the
//! crate but kept inert (the daemon does not start the loop yet).
//!
//! **Timezone (PRD #127 Open Q2):** cron expressions are evaluated in **local
//! time** (`chrono::Local`). `0 9 * * MON-FRI` therefore means 09:00 in the
//! host's local zone. A per-schedule `timezone` field is deferred until demand
//! appears (see the PRD).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local, TimeZone, Timelike};
use cron::Schedule as CronSchedule;

use crate::config::ScheduledTask;

// ---------------------------------------------------------------------------
// Notification seam (PRD #126 dependency — soft for development)
// ---------------------------------------------------------------------------

/// Failure-surfacing seam for the scheduler. PRD #126 (agent-driven
/// notifications) is the eventual home for these events; until it lands this
/// trait plus the stderr default impl keep callers from hard-coding a sink.
/// When #126 ships, its channel implements `Notifier` and drops in here with
/// no reshaping of the scheduler callers.
pub trait Notifier: Send + Sync {
    fn notify(&self, event: NotifyEvent);
}

/// A surfaced scheduler event. Kept small and additive: PRD #120 / #126 add
/// variants (spawn errors, mkdir failures) without touching existing arms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyEvent {
    /// A `[[scheduled_tasks]]` entry (or the whole file) failed to load/parse.
    /// `entry` is the array index when the failure is attributable to a single
    /// entry, `None` for a file-level error.
    ConfigError {
        entry: Option<usize>,
        message: String,
    },
    /// A scheduled fire (cron tick or run-now) was skipped because the prior
    /// run for the same task name has not returned yet.
    SkippedStillRunning { task: String },
    /// PRD #127 M2.1: a fire could not create (`mkdir -p`) its `working_dir`.
    /// The fire is abandoned; other tasks keep running.
    WorkingDirError {
        task: String,
        path: String,
        message: String,
    },
    /// PRD #127 M2.1: a fire created/resolved its `working_dir` but the agent
    /// spawn itself failed. The fire is abandoned; other tasks keep running.
    SpawnFailed { task: String, message: String },
}

/// Default [`Notifier`] that logs to stderr. Stand-in until the PRD #126
/// notification channel exists; swapped out by passing a different `Notifier`
/// to [`Scheduler::new`].
#[derive(Debug, Default, Clone)]
pub struct StderrNotifier;

impl Notifier for StderrNotifier {
    fn notify(&self, event: NotifyEvent) {
        match event {
            NotifyEvent::ConfigError { entry, message } => match entry {
                Some(i) => {
                    eprintln!("[scheduler] config error in scheduled_tasks[{i}]: {message}")
                }
                None => eprintln!("[scheduler] config error: {message}"),
            },
            NotifyEvent::SkippedStillRunning { task } => {
                eprintln!("[scheduler] skipped fire of {task:?}: previous run still active");
            }
            NotifyEvent::WorkingDirError {
                task,
                path,
                message,
            } => {
                eprintln!(
                    "[scheduler] task {task:?}: could not create working_dir {path:?}: {message}"
                );
            }
            NotifyEvent::SpawnFailed { task, message } => {
                eprintln!("[scheduler] task {task:?}: spawn failed: {message}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("invalid cron expression {expr:?}: {message}")]
    InvalidCron { expr: String, message: String },
    #[error("no scheduled task named {0:?}")]
    UnknownTask(String),
}

// ---------------------------------------------------------------------------
// Cron parsing / evaluation
// ---------------------------------------------------------------------------

/// Normalize a user-facing cron expression to the form the `cron` crate
/// expects.
///
/// The `cron` crate requires a 6- or 7-field expression
/// (`sec min hour day-of-month month day-of-week [year]`). Users (and the PRD
/// examples) write the familiar 5-field POSIX form (`0 9 * * MON-FRI`), so a
/// 5-field expression gets a `0` seconds field prepended. 6- and 7-field
/// expressions — including the per-second forms the fast unit tests use — pass
/// through unchanged.
fn normalize_cron_expr(expr: &str) -> String {
    let trimmed = expr.trim();
    if trimmed.split_whitespace().count() == 5 {
        format!("0 {trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// Parse a cron expression into a [`CronSchedule`], accepting either the
/// 5-field POSIX form or the 6/7-field form (see [`normalize_cron_expr`]).
pub fn parse_cron(expr: &str) -> Result<CronSchedule, SchedulerError> {
    let normalized = normalize_cron_expr(expr);
    CronSchedule::from_str(&normalized).map_err(|e| SchedulerError::InvalidCron {
        expr: expr.to_string(),
        message: e.to_string(),
    })
}

/// Validation helper reused by the CLI (`schedule add`, M1.5): returns `Ok(())`
/// for a well-formed cron expression, `Err` with a human-readable message
/// otherwise. Thin wrapper over [`parse_cron`] so the CLI and the scheduler
/// share one definition of "valid".
pub fn validate_cron(expr: &str) -> Result<(), String> {
    parse_cron(expr).map(|_| ()).map_err(|e| e.to_string())
}

/// Whether `schedule` fires at the given instant (second granularity). Lets
/// unit tests drive cron evaluation against constructed instants with no
/// wall-clock waits; the production loop uses the same predicate via
/// [`Scheduler::tick_at`].
pub fn fires_at<Z: TimeZone>(schedule: &CronSchedule, instant: DateTime<Z>) -> bool {
    schedule.includes(instant)
}

// ---------------------------------------------------------------------------
// Callbacks and registered tasks
// ---------------------------------------------------------------------------

/// The future returned by a task callback. `'static + Send` so it can be moved
/// onto a `tokio::spawn`.
pub type CallbackFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// A registered task's body. Invoked on each fire (cron tick or run-now). For
/// the static-prompt case (M2.3) this calls `spawn` once; PRD #120 will
/// register a callback that loops over `spawn` N times — without changing this
/// module.
pub type Callback = Arc<dyn Fn() -> CallbackFuture + Send + Sync + 'static>;

/// An opaque handle returned by [`Scheduler::register`]. Carries the task name,
/// which is the reuse-registry key (PRD #127) and the run-now / reload key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskHandle {
    pub name: String,
}

/// The fire-affecting fields of a scheduled task, retained on the registered
/// task so a reload can detect when ANY of them changed — not just the cron.
/// The live callback ([`crate::daemon`]'s `make_schedule_callback`) CLONES
/// these values into the `SpawnRequest` it captures, so a prompt-only edit that
/// the diff misses would keep firing the prompt captured at first registration
/// (PRD #127 stale-prompt bug). Comparing the whole struct forces a callback
/// rebuild on any change. `cron` is kept un-normalized so the comparison
/// matches the value written in `schedules.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FireFields {
    cron: String,
    working_dir: String,
    command: Option<String>,
    prompt: String,
    new_tab_per_fire: bool,
}

impl FireFields {
    /// All fire-affecting fields of a freshly-loaded config entry.
    fn from_task(task: &ScheduledTask) -> Self {
        Self {
            cron: task.cron.clone(),
            working_dir: task.working_dir.clone(),
            command: task.command.clone(),
            prompt: task.prompt.clone(),
            new_tab_per_fire: task.new_tab_per_fire,
        }
    }

    /// Cron-only fields, for [`Scheduler::register`] (test/non-config callers
    /// that carry a cron but no full [`ScheduledTask`]). The remaining fields
    /// default to empty, so a later config-driven reload of the same name
    /// always reads as changed and re-registers — the safe direction.
    fn from_cron(cron_expr: &str) -> Self {
        Self {
            cron: cron_expr.to_string(),
            working_dir: String::new(),
            command: None,
            prompt: String::new(),
            new_tab_per_fire: false,
        }
    }
}

struct RegisteredTask {
    name: String,
    /// All fire-affecting fields (incl. the un-normalized cron), retained so
    /// reload-apply can detect ANY change without re-deriving it.
    fire_fields: FireFields,
    schedule: CronSchedule,
    callback: Callback,
    /// Skip-if-prior-run-still-active flag. Set when a fire starts, cleared
    /// when its callback future completes.
    running: Arc<AtomicBool>,
}

// ---------------------------------------------------------------------------
// Reload diff
// ---------------------------------------------------------------------------

/// What a reload changed about the registered task set. Returned by
/// [`Scheduler::reload_apply`] so the caller (the daemon's future
/// `ReloadSchedules` handler) can log/surface the delta. Pure data.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReloadDiff {
    /// Newly registered task names.
    pub added: Vec<String>,
    /// Task names removed (no longer in the config, or now disabled).
    pub removed: Vec<String>,
    /// Task names whose fire-affecting config changed — cron, prompt,
    /// working_dir, command, or new_tab_per_fire (re-registered in place).
    pub updated: Vec<String>,
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

/// The daemon-side scheduler. Holds the registered tasks and evaluates them
/// against local time. The map is guarded by a plain `std::sync::Mutex` — it is
/// only ever held for brief map operations, never across an `.await`, so it is
/// safe to call from both sync (`run_now`, `reload_apply`) and async (the
/// firing loop) contexts.
pub struct Scheduler {
    tasks: Mutex<HashMap<String, Arc<RegisteredTask>>>,
    notifier: Arc<dyn Notifier>,
}

impl Scheduler {
    /// Construct a scheduler that surfaces failures through `notifier`.
    pub fn new(notifier: Arc<dyn Notifier>) -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            notifier,
        }
    }

    /// Convenience constructor wiring the stderr stand-in notifier (PRD #126
    /// not yet available).
    pub fn with_stderr_notifier() -> Self {
        Self::new(Arc::new(StderrNotifier))
    }

    /// Register a task under `name` firing on `cron_expr`, invoking `callback`
    /// on each fire. Returns a [`TaskHandle`]; replaces any existing task with
    /// the same name. Errors if the cron expression is invalid.
    pub fn register(
        &self,
        name: impl Into<String>,
        cron_expr: &str,
        callback: Callback,
    ) -> Result<TaskHandle, SchedulerError> {
        let name = name.into();
        let task = Arc::new(self.build_task(&name, FireFields::from_cron(cron_expr), callback)?);
        self.tasks
            .lock()
            .expect("scheduler task map poisoned")
            .insert(name.clone(), task);
        Ok(TaskHandle { name })
    }

    fn build_task(
        &self,
        name: &str,
        fire_fields: FireFields,
        callback: Callback,
    ) -> Result<RegisteredTask, SchedulerError> {
        let schedule = parse_cron(&fire_fields.cron)?;
        Ok(RegisteredTask {
            name: name.to_string(),
            fire_fields,
            schedule,
            callback,
            running: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Number of currently-registered tasks.
    pub fn len(&self) -> usize {
        self.tasks
            .lock()
            .expect("scheduler task map poisoned")
            .len()
    }

    /// Whether any task is registered. The daemon's idle carve-out (a later
    /// task) will consult this to decide whether a schedule keeps it alive.
    pub fn is_empty(&self) -> bool {
        self.tasks
            .lock()
            .expect("scheduler task map poisoned")
            .is_empty()
    }

    /// Whether a task with `name` is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.tasks
            .lock()
            .expect("scheduler task map poisoned")
            .contains_key(name)
    }

    /// Names of all currently-registered tasks, sorted. Since the daemon only
    /// registers ENABLED tasks (and `reload_apply` drops disabled ones), this
    /// is the set of live enabled task names — what the `ReloadSchedules`
    /// handler echoes back to the caller.
    pub fn registered_names(&self) -> Vec<String> {
        let tasks = self.tasks.lock().expect("scheduler task map poisoned");
        let mut names: Vec<String> = tasks.keys().cloned().collect();
        names.sort();
        names
    }

    /// Surface a batch of config load/parse errors through the notifier. Used
    /// by the daemon's startup load and `ReloadSchedules` handler so a
    /// malformed entry is reported (never silently swallowed) without the
    /// caller needing access to the notifier directly.
    pub fn report_config_errors(&self, errors: &[crate::config::ScheduleLoadError]) {
        for e in errors {
            self.notifier.notify(NotifyEvent::ConfigError {
                entry: e.entry,
                message: e.message.clone(),
            });
        }
    }

    /// Fire any task whose schedule includes `now` (second granularity),
    /// honoring skip-if-running. Returns the names that actually started. The
    /// production loop ([`Scheduler::run`]) calls this once per second with
    /// `Local::now()`; tests call it with constructed instants, so no
    /// wall-clock wait is needed to exercise firing.
    pub fn tick_at(&self, now: DateTime<Local>) -> Vec<String> {
        let due: Vec<Arc<RegisteredTask>> = {
            let tasks = self.tasks.lock().expect("scheduler task map poisoned");
            tasks
                .values()
                .filter(|t| t.schedule.includes(now))
                .cloned()
                .collect()
        };
        let mut fired = Vec::new();
        for task in due {
            if self.fire(&task) {
                fired.push(task.name.clone());
            }
        }
        fired
    }

    /// Manually fire `name` immediately, honoring the same skip-if-running rule
    /// as a cron tick. Returns `Ok(true)` if the run started, `Ok(false)` if it
    /// was skipped because the prior run is still active, `Err` if no such task.
    pub fn run_now(&self, name: &str) -> Result<bool, SchedulerError> {
        let task = {
            let tasks = self.tasks.lock().expect("scheduler task map poisoned");
            tasks.get(name).cloned()
        };
        match task {
            Some(task) => Ok(self.fire(&task)),
            None => Err(SchedulerError::UnknownTask(name.to_string())),
        }
    }

    /// Start a task's callback if the prior run has finished. Returns whether
    /// the run actually started. Requires a tokio runtime (the callback future
    /// is spawned); the daemon loop and `#[tokio::test]` both provide one.
    fn fire(&self, task: &Arc<RegisteredTask>) -> bool {
        // Atomically claim the running slot: swap returns the *previous* value,
        // so `true` means a prior run is still active → skip and notify.
        if task.running.swap(true, Ordering::SeqCst) {
            self.notifier.notify(NotifyEvent::SkippedStillRunning {
                task: task.name.clone(),
            });
            return false;
        }
        let callback = task.callback.clone();
        let running = task.running.clone();
        tokio::spawn(async move {
            // PRD #127 B2: clear the running flag on scope exit via a drop
            // guard, so a callback PANIC (which would otherwise skip the
            // `store(false)`) can't leave the task permanently "running" and
            // silently skip every future fire for the daemon's lifetime.
            struct RunningGuard(Arc<AtomicBool>);
            impl Drop for RunningGuard {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _guard = RunningGuard(running);
            callback().await;
        });
        true
    }

    /// Replace the registered task set to match `desired` (a freshly-loaded
    /// config), building each task's callback via `make_callback`. Disabled
    /// entries are treated as absent (removed). Returns a [`ReloadDiff`]
    /// describing the delta. Invalid cron expressions in `desired` are surfaced
    /// through the notifier and skipped — a single bad entry never aborts the
    /// reload.
    ///
    /// This is the pure reload-apply logic only. The daemon's socket
    /// `ReloadSchedules` handler (a later task) will load the config and call
    /// this; nothing here touches the socket.
    pub fn reload_apply<F>(&self, desired: &[ScheduledTask], mut make_callback: F) -> ReloadDiff
    where
        F: FnMut(&ScheduledTask) -> Callback,
    {
        let mut diff = ReloadDiff::default();
        let mut tasks = self.tasks.lock().expect("scheduler task map poisoned");

        let desired_enabled: Vec<&ScheduledTask> = desired.iter().filter(|t| t.enabled).collect();
        let desired_names: HashSet<&str> =
            desired_enabled.iter().map(|t| t.name.as_str()).collect();

        // Remove tasks no longer desired (deleted from config or disabled).
        let to_remove: Vec<String> = tasks
            .keys()
            .filter(|name| !desired_names.contains(name.as_str()))
            .cloned()
            .collect();
        for name in to_remove {
            tasks.remove(&name);
            diff.removed.push(name);
        }

        // Add new tasks and re-register ones whose fire-affecting config
        // changed. The guard compares ALL fire-affecting fields (cron, prompt,
        // working_dir, command, new_tab_per_fire), not just the cron — a
        // prompt-only edit must rebuild the callback so the next fire delivers
        // the new prompt rather than the value captured at first registration
        // (PRD #127 stale-prompt bug).
        for task in desired_enabled {
            let fire_fields = FireFields::from_task(task);
            match tasks.get(&task.name) {
                Some(existing) if existing.fire_fields == fire_fields => {
                    // Unchanged: keep the live task (preserves its running flag).
                }
                existing => {
                    // PRD #127 C4: a task that REMAINS present (same name) but
                    // whose cron/fields changed must keep its running flag, or
                    // a reload while it is mid-run resets the flag to false and
                    // the next tick fires it again → a brief double-fire.
                    let preserved_running = existing.map(|e| e.running.clone());
                    let is_update = preserved_running.is_some();
                    match self.build_task(&task.name, fire_fields, make_callback(task)) {
                        Ok(mut built) => {
                            if let Some(running) = preserved_running {
                                built.running = running;
                            }
                            tasks.insert(task.name.clone(), Arc::new(built));
                            if is_update {
                                diff.updated.push(task.name.clone());
                            } else {
                                diff.added.push(task.name.clone());
                            }
                        }
                        Err(e) => {
                            self.notifier.notify(NotifyEvent::ConfigError {
                                entry: None,
                                message: e.to_string(),
                            });
                        }
                    }
                }
            }
        }

        diff
    }

    /// The daemon-side firing loop. Wakes ~twice a second and evaluates EVERY
    /// whole second in `(last_evaluated, now]` against local time, so a late
    /// wake-up (loop stall, brief process suspend) does not silently drop a due
    /// fire — and each second is evaluated exactly once (no double-fire).
    /// Catch-up is capped (see [`catchup_seconds`]) so resuming from a long
    /// sleep doesn't replay hours of ticks at once.
    ///
    /// **DST / local-time tradeoff (PRD #127, accepted):** cron is evaluated in
    /// local time, so at a DST transition a fire may be skipped (spring-forward
    /// hour never occurs) or run twice (fall-back hour repeats). This is the
    /// documented local-time tradeoff — there is intentionally no timezone
    /// handling. See `docs/scheduled-tasks.md`.
    pub async fn run(self: Arc<Self>) {
        let mut last_evaluated: Option<DateTime<Local>> = None;
        loop {
            let now = Local::now();
            let this_second = now.with_nanosecond(0).unwrap_or(now);
            for second in catchup_seconds(last_evaluated, this_second, MAX_CATCHUP_SECONDS) {
                self.tick_at(second);
            }
            last_evaluated = Some(this_second);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// Maximum number of missed whole seconds the firing loop will replay on a
/// single late wake-up (PRD #127 N1). Bounds catch-up after a long stall /
/// suspend so the daemon doesn't fire a backlog of hours at once.
const MAX_CATCHUP_SECONDS: i64 = 60;

/// The whole seconds the firing loop should evaluate this wake-up, given the
/// previously-evaluated second `prev` and the current second `now`:
///
/// - `prev == None` (first tick) → just `[now]`.
/// - `now > prev` → every second in `(prev, now]`, capped to the most recent
///   `max` (a long gap doesn't replay everything).
/// - `now <= prev` (same second, or the clock went backwards) → `[]` — never
///   re-fire a second already evaluated.
///
/// Pure so the late-tick / dedup policy is unit-testable without wall-clock.
fn catchup_seconds(
    prev: Option<DateTime<Local>>,
    now: DateTime<Local>,
    max: i64,
) -> Vec<DateTime<Local>> {
    let Some(prev) = prev else {
        return vec![now];
    };
    let gap = (now - prev).num_seconds();
    if gap <= 0 {
        return Vec::new();
    }
    let first_offset = gap.min(max);
    (0..first_offset)
        .rev()
        .map(|back| now - chrono::Duration::seconds(back))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::Notify;

    fn local_at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    /// A callback that bumps a counter each time it fires.
    fn counting_callback(counter: Arc<AtomicUsize>) -> Callback {
        Arc::new(move || {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
            })
        })
    }

    // scheduler/cron/001 — a known cron fires on a matching instant and not on
    // a non-matching one. Drives the pure evaluator with no wall-clock wait.
    #[test]
    fn cron_fires_on_matching_instant_only() {
        // 09:00 every weekday (5-field POSIX form gets a `0` seconds prefix).
        let schedule = parse_cron("0 9 * * MON-FRI").unwrap();
        // 2026-06-08 is a Monday.
        let monday_0900 = local_at(2026, 6, 8, 9, 0, 0);
        let monday_0901 = local_at(2026, 6, 8, 9, 1, 0);
        // 2026-06-06 is a Saturday.
        let saturday_0900 = local_at(2026, 6, 6, 9, 0, 0);

        assert!(fires_at(&schedule, monday_0900), "should fire Mon 09:00");
        assert!(!fires_at(&schedule, monday_0901), "should not fire 09:01");
        assert!(!fires_at(&schedule, saturday_0900), "should not fire Sat");
    }

    // scheduler/cron/002 — an overrunning callback causes the next fire to be
    // skipped and a skip event to be recorded.
    #[tokio::test]
    async fn skip_if_prior_run_still_active() {
        // Notifier that records the events it receives.
        #[derive(Default)]
        struct RecordingNotifier {
            events: Mutex<Vec<NotifyEvent>>,
        }
        impl Notifier for RecordingNotifier {
            fn notify(&self, event: NotifyEvent) {
                self.events.lock().unwrap().push(event);
            }
        }

        let notifier = Arc::new(RecordingNotifier::default());
        let scheduler = Scheduler::new(notifier.clone());

        // The first run blocks until we release it, so it is still "running"
        // when the second fire is attempted.
        let release = Arc::new(Notify::new());
        let started = Arc::new(Notify::new());
        let fire_count = Arc::new(AtomicUsize::new(0));
        let cb: Callback = {
            let release = release.clone();
            let started = started.clone();
            let fire_count = fire_count.clone();
            Arc::new(move || {
                let release = release.clone();
                let started = started.clone();
                let fire_count = fire_count.clone();
                Box::pin(async move {
                    fire_count.fetch_add(1, Ordering::SeqCst);
                    started.notify_one();
                    release.notified().await;
                })
            })
        };
        scheduler.register("overrun", "* * * * * *", cb).unwrap();

        // First fire starts and parks on `release`.
        assert!(scheduler.run_now("overrun").unwrap(), "first fire starts");
        started.notified().await;

        // Second fire while the first is still active → skipped.
        assert!(
            !scheduler.run_now("overrun").unwrap(),
            "second fire skipped while first still active"
        );

        // A skip event was recorded.
        let events = notifier.events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![NotifyEvent::SkippedStillRunning {
                task: "overrun".to_string()
            }]
        );

        // Only the first callback actually ran.
        assert_eq!(fire_count.load(Ordering::SeqCst), 1);

        // Release the first run; a subsequent fire is allowed again.
        release.notify_one();
        // Give the spawned future a chance to clear the running flag.
        for _ in 0..50 {
            if !scheduler
                .tasks
                .lock()
                .unwrap()
                .get("overrun")
                .unwrap()
                .running
                .load(Ordering::SeqCst)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            scheduler.run_now("overrun").unwrap(),
            "fire allowed again after prior run finished"
        );
    }

    // scheduler/cron/003 — run-now fires immediately without waiting for the
    // next cron tick.
    #[tokio::test]
    async fn run_now_fires_immediately() {
        let scheduler = Scheduler::with_stderr_notifier();
        let counter = Arc::new(AtomicUsize::new(0));
        // A schedule that would not naturally fire for a long time (yearly).
        scheduler
            .register("manual", "0 0 0 1 1 *", counting_callback(counter.clone()))
            .unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert!(scheduler.run_now("manual").unwrap());

        // Wait for the spawned callback to run.
        for _ in 0..50 {
            if counter.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "run-now fired the callback"
        );
    }

    #[test]
    fn run_now_unknown_task_errors() {
        let scheduler = Scheduler::with_stderr_notifier();
        assert!(matches!(
            scheduler.run_now("nope"),
            Err(SchedulerError::UnknownTask(_))
        ));
    }

    // scheduler/cli/001 (validation portion) — the cron validation helper
    // accepts well-formed expressions (5-field and 6-field) and rejects
    // malformed ones.
    #[test]
    fn validate_cron_accepts_and_rejects() {
        assert!(validate_cron("0 9 * * MON-FRI").is_ok(), "5-field POSIX");
        assert!(
            validate_cron("*/30 * * * * *").is_ok(),
            "6-field per-second"
        );
        assert!(validate_cron("0 9 * * *").is_ok(), "5-field daily");

        assert!(validate_cron("not a cron").is_err());
        assert!(validate_cron("99 9 * * *").is_err(), "minute out of range");
        assert!(validate_cron("").is_err());
    }

    #[test]
    fn normalize_prepends_seconds_for_five_field() {
        assert_eq!(normalize_cron_expr("0 9 * * MON-FRI"), "0 0 9 * * MON-FRI");
        assert_eq!(normalize_cron_expr("*/5 * * * * *"), "*/5 * * * * *");
        assert_eq!(normalize_cron_expr("  0 9 * * *  "), "0 0 9 * * *");
    }

    #[test]
    fn reload_apply_diffs_added_removed_updated() {
        let scheduler = Scheduler::with_stderr_notifier();
        let counter = Arc::new(AtomicUsize::new(0));
        let make = |_t: &ScheduledTask| counting_callback(counter.clone());

        let task = |name: &str, cron: &str, enabled: bool| ScheduledTask {
            name: name.to_string(),
            cron: cron.to_string(),
            working_dir: "/tmp".to_string(),
            command: None,
            prompt: "p".to_string(),
            new_tab_per_fire: false,
            enabled,
        };

        // Initial load: two tasks.
        let diff = scheduler.reload_apply(
            &[task("a", "0 9 * * *", true), task("b", "0 10 * * *", true)],
            make,
        );
        assert_eq!(diff.added, vec!["a".to_string(), "b".to_string()]);
        assert!(diff.removed.is_empty() && diff.updated.is_empty());
        assert!(scheduler.contains("a") && scheduler.contains("b"));

        // Reload: "a" cron changed, "b" removed, "c" added, "d" disabled (no-op).
        let diff = scheduler.reload_apply(
            &[
                task("a", "0 11 * * *", true),
                task("c", "0 12 * * *", true),
                task("d", "0 13 * * *", false),
            ],
            make,
        );
        assert_eq!(diff.updated, vec!["a".to_string()]);
        assert_eq!(diff.added, vec!["c".to_string()]);
        assert_eq!(diff.removed, vec!["b".to_string()]);
        assert!(scheduler.contains("a") && scheduler.contains("c"));
        assert!(!scheduler.contains("b") && !scheduler.contains("d"));
    }

    // B2 — a callback that PANICS must not leave the task permanently
    // "running": the drop guard clears the flag so a later fire is allowed.
    #[tokio::test]
    async fn panicking_callback_does_not_permanently_skip() {
        let scheduler = Scheduler::with_stderr_notifier();
        let cb: Callback = Arc::new(|| {
            Box::pin(async {
                panic!("intentional test panic inside scheduled callback");
            })
        });
        scheduler.register("boom", "0 0 1 1 *", cb).unwrap();

        // First fire starts the (panicking) callback.
        assert!(scheduler.run_now("boom").unwrap());

        // After the panic unwinds, the drop guard must have cleared `running`,
        // so a subsequent fire is allowed again (not permanently skipped).
        let mut allowed_again = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if scheduler.run_now("boom").unwrap() {
                allowed_again = true;
                break;
            }
        }
        assert!(
            allowed_again,
            "panicking callback must not leave the task permanently running"
        );
    }

    // C4 — a reload that re-registers a still-present task (cron changed) must
    // PRESERVE its running flag, so a tick during the in-flight run doesn't
    // double-fire.
    #[tokio::test]
    async fn reload_preserves_running_flag_for_present_task() {
        let scheduler = Scheduler::with_stderr_notifier();
        let release = Arc::new(Notify::new());
        let started = Arc::new(Notify::new());
        let cb: Callback = {
            let release = release.clone();
            let started = started.clone();
            Arc::new(move || {
                let release = release.clone();
                let started = started.clone();
                Box::pin(async move {
                    started.notify_one();
                    release.notified().await;
                })
            })
        };
        scheduler.register("x", "0 9 * * *", cb).unwrap();

        // Start a run that parks (running == true).
        assert!(scheduler.run_now("x").unwrap());
        started.notified().await;

        // Reload "x" with a CHANGED cron (re-registered in place).
        let desired = ScheduledTask {
            name: "x".to_string(),
            cron: "0 10 * * *".to_string(),
            working_dir: "/tmp".to_string(),
            command: None,
            prompt: "p".to_string(),
            new_tab_per_fire: false,
            enabled: true,
        };
        let noop: Callback = Arc::new(|| Box::pin(async {}));
        scheduler.reload_apply(&[desired], |_| noop.clone());

        // The running flag survived the re-register → a fresh fire is skipped.
        assert!(
            !scheduler.run_now("x").unwrap(),
            "reload must preserve the running flag of a still-present task"
        );

        // Release the parked run so the test doesn't leak a task.
        release.notify_one();
    }

    // N1 — the firing loop's catch-up evaluates every missed whole second
    // exactly once, caps a long gap, and never re-fires.
    #[test]
    fn catchup_seconds_covers_missed_ticks_without_double_fire() {
        let now = local_at(2026, 6, 8, 9, 0, 10);

        // First tick: just `now`.
        assert_eq!(catchup_seconds(None, now, 60), vec![now]);

        // A 3-second gap evaluates the three missed seconds ending at `now`.
        let prev = local_at(2026, 6, 8, 9, 0, 7);
        assert_eq!(
            catchup_seconds(Some(prev), now, 60),
            vec![
                local_at(2026, 6, 8, 9, 0, 8),
                local_at(2026, 6, 8, 9, 0, 9),
                now,
            ]
        );

        // Same second → nothing (no double-fire).
        assert!(catchup_seconds(Some(now), now, 60).is_empty());

        // Clock went backwards → nothing.
        let future = local_at(2026, 6, 8, 9, 0, 12);
        assert!(catchup_seconds(Some(future), now, 60).is_empty());

        // A huge gap is capped to the most recent `max` seconds.
        let long_ago = local_at(2026, 6, 8, 8, 0, 0);
        let capped = catchup_seconds(Some(long_ago), now, 5);
        assert_eq!(capped.len(), 5);
        assert_eq!(*capped.last().unwrap(), now);
        assert_eq!(capped[0], local_at(2026, 6, 8, 9, 0, 6));
    }
}
