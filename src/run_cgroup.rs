//! Issue #1897: start each agent run in a cgroup of its own, so per-run CPU
//! and memory count work that leaves the run's process tree.
//!
//! The deck's resource collector (PRD #1859) groups processes by the
//! `deck.run.id` they inherit in their environment. That misses short-lived
//! work that detaches from a run's tree, and all of a detached process that
//! also clears its environment. A cgroup holds every process forked inside it
//! whatever it does with its parent or its environment, and its `cpu.stat`
//! and `memory.current` are the run's totals. The collector reads them when a
//! run has one (`telemetry::collector::cgroup`), and a stock scraper that
//! groups by cgroup can read them with no deck code at all.
//!
//! **Only in a subtree systemd delegated to the daemon.** A process may create
//! cgroups only below one its manager handed it: the cgroup of a unit with
//! `Delegate=yes`. `dot-agent-deck daemon install` writes its user unit with
//! that line. A daemon started any other way runs in a cgroup it does not
//! own — a lazy-spawned daemon lives in the login session's
//! `session-<n>.scope`, which is root's — so it starts agents exactly as
//! before and the collector keeps grouping by environment. The decision and
//! the measurements behind it are in `docs/develop/run-cgroups.md`.
//!
//! **Ownership is not delegation.** Under the user manager every unit's
//! cgroup is owned by the user, delegated or not, and systemd sets no marker
//! on it, so [`init`] asks systemd for the unit's `Delegate` property instead
//! of testing whether it can write there. Writing into a cgroup systemd still
//! manages would fight systemd's own changes to it.
//!
//! **Teardown is unchanged.** Every cgroup this module makes is inside the
//! daemon's own unit cgroup, so stopping or restarting the unit kills
//! everything in it exactly as before, the run cgroups included, and systemd
//! removes the subtree. Nothing here signals an agent or the daemon: the one
//! process it kills is its own `systemctl show`, when that does not answer.
//!
//! **It never fails a start.** Every step that can go wrong — no cgroup v2, no
//! delegation, a write the kernel refuses — leaves the agent where it would
//! have been without this module, and says why in the log.
//!
//! Layout, under the unit's cgroup `<unit>`:
//!
//! ```text
//! <unit>/dot-agent-deck-daemon   the daemon itself (cgroup v2 forbids
//!                                processes in a cgroup that hands
//!                                controllers to children)
//! <unit>/run-<deck.run.id>       one agent run, telemetry on
//! <unit>/agent-<pid>             one agent run, telemetry off
//! ```

/// The leaf the daemon moves itself into.
pub const DAEMON_LEAF: &str = "dot-agent-deck-daemon";

/// The prefix of a run's cgroup, followed by its `deck.run.id`. The collector
/// matches a run to its cgroup by this name.
pub const RUN_PREFIX: &str = "run-";

/// The prefix of an agent's cgroup when no run was started for it (telemetry
/// off), followed by the agent's pid.
pub const AGENT_PREFIX: &str = "agent-";

/// Set to `0` to keep every agent in the daemon's own cgroup even where the
/// subtree is delegated.
pub const RUN_CGROUPS_ENV: &str = "DOT_AGENT_DECK_RUN_CGROUPS";

/// Which systemd instance manages a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    /// A user's manager (`systemctl --user`).
    User,
    /// The system manager.
    System,
}

/// The daemon's unit, as its cgroup path names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitCgroup {
    /// The unit's cgroup, relative to the cgroup v2 mount, with a leading `/`.
    pub path: String,
    /// The unit's name, the last component of `path`.
    pub unit: String,
    pub manager: Manager,
}

/// The cgroup v2 path in a `/proc/<pid>/cgroup`, when the process is on a
/// pure cgroup v2 hierarchy: the file is then a single `0::<path>` line. A
/// cgroup v1 or hybrid host lists v1 hierarchies as well, and gets `None`.
pub fn unified_path(proc_cgroup: &str) -> Option<&str> {
    let mut lines = proc_cgroup.lines().filter(|l| !l.is_empty());
    let path = lines.next()?.strip_prefix("0::")?;
    (lines.next().is_none() && path.starts_with('/')).then_some(path)
}

/// The unit whose subtree a daemon in cgroup `path` would use: the cgroup
/// itself, or its parent when the daemon already sits in [`DAEMON_LEAF`]
/// (a daemon started by an earlier one there). `None` when that cgroup is not
/// a systemd service or scope.
pub fn unit_cgroup(path: &str) -> Option<UnitCgroup> {
    let path = path
        .strip_suffix(&format!("/{DAEMON_LEAF}"))
        .unwrap_or(path);
    let unit = path.rsplit('/').next()?;
    if !(unit.ends_with(".service") || unit.ends_with(".scope")) {
        return None;
    }
    let under_user_manager = path.split('/').any(|c| {
        c.strip_prefix("user@")
            .and_then(|c| c.strip_suffix(".service"))
            .is_some_and(|uid| !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit()))
    });
    Some(UnitCgroup {
        path: path.to_string(),
        unit: unit.to_string(),
        manager: if under_user_manager {
            Manager::User
        } else {
            Manager::System
        },
    })
}

/// Whether `systemctl show -p Delegate --value`'s output says the unit is
/// delegated.
pub fn delegated(show_output: &str) -> bool {
    show_output.trim() == "yes"
}

/// The `cgroup.subtree_control` line enabling those of `memory` and `pids`
/// that `cgroup.controllers` lists, or `None` when it lists neither.
///
/// Never `cpu`: CPU time is in every cgroup's `cpu.stat` without it, and
/// enabling it would give each agent's cgroup an equal `cpu.weight`, so a
/// busy machine would share CPU between runs instead of between threads as
/// it did before (Qodo on PR #2475). `memory` and `pids` set no limit by
/// default; they only count.
pub fn controllers_to_enable(available: &str) -> Option<String> {
    let have: Vec<&str> = available.split_whitespace().collect();
    let wanted: Vec<String> = ["memory", "pids"]
        .iter()
        .filter(|c| have.contains(c))
        .map(|c| format!("+{c}"))
        .collect();
    (!wanted.is_empty()).then(|| wanted.join(" "))
}

/// The cgroup name for an agent: `run-<id>` for a run id as the deck mints
/// them, `agent-<pid>` otherwise.
pub fn run_cgroup_name(run_id: Option<&str>, pid: u32) -> String {
    match run_id.filter(|id| crate::telemetry::launch::is_run_id(id)) {
        Some(id) => format!("{RUN_PREFIX}{id}"),
        None => format!("{AGENT_PREFIX}{pid}"),
    }
}

/// Whether a name in the daemon's subtree is one this module made for an
/// agent, and may therefore remove once it is empty.
pub fn is_agent_cgroup(name: &str) -> bool {
    let rest = |prefix: &str| name.strip_prefix(prefix).filter(|r| !r.is_empty());
    rest(RUN_PREFIX).is_some_and(crate::telemetry::launch::is_run_id)
        || rest(AGENT_PREFIX).is_some_and(|r| r.bytes().all(|b| b.is_ascii_digit()))
}

/// `populated` in a `cgroup.events`: whether the cgroup or one below it holds
/// a live process.
pub fn populated(events: &str) -> Option<bool> {
    events.lines().find_map(|l| match l.split_once(' ')? {
        ("populated", v) => Some(v.trim() == "1"),
        _ => None,
    })
}

/// `(ppid, session id)` from a `/proc/<pid>/stat`.
pub fn ppid_and_session(stat: &str) -> Option<(u32, u32)> {
    // The command name is in parentheses and may hold spaces or `)`; the
    // fields after the last `)` are `state ppid pgrp session …`.
    let mut fields = stat.get(stat.rfind(')')? + 1..)?.split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?.parse().ok()?;
    let _pgrp = fields.next()?;
    let session = fields.next()?.parse().ok()?;
    Some((ppid, session))
}

/// Whether the process described by `pid`, read through `stat_of`, belongs to
/// the agent `child`: it is in `child`'s session (portable-pty starts every
/// agent as a session leader) or `child` is among its ancestors. Bounded, so
/// a cycle of reused pids cannot loop.
pub fn belongs_to(pid: u32, child: u32, stat_of: impl Fn(u32) -> Option<(u32, u32)>) -> bool {
    let mut current = pid;
    for _ in 0..64 {
        let Some((ppid, session)) = stat_of(current) else {
            return false;
        };
        if current == child || session == child {
            return true;
        }
        if ppid <= 1 {
            return false;
        }
        current = ppid;
    }
    false
}

#[cfg(target_os = "linux")]
pub use linux::{init, place};

/// No cgroups here: every agent starts as it always did.
#[cfg(not(target_os = "linux"))]
pub fn init() {}

/// No cgroups here: every agent starts as it always did.
#[cfg(not(target_os = "linux"))]
pub fn place(_child_pid: u32, _run_id: Option<&str>) {}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashSet;
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use super::*;

    const CGROUP_MOUNT: &str = "/sys/fs/cgroup";

    /// How long `systemctl show` may take before the daemon gives up on it.
    const SHOW_TIMEOUT: Duration = Duration::from_secs(3);

    /// How often empty agent cgroups are looked for. One is removed when two
    /// sweeps in a row found it empty, so it outlives its last process by one
    /// to two of these: longer than the collector's 30-second maximum
    /// cadence, so its final counters are read before it goes.
    const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

    /// The delegated subtree this daemon starts agents in.
    #[derive(Debug)]
    struct RunCgroups {
        /// The unit's cgroup, absolute.
        base: PathBuf,
        /// [`DAEMON_LEAF`] under it.
        leaf: PathBuf,
    }

    static RUN_CGROUPS: OnceLock<Option<RunCgroups>> = OnceLock::new();

    /// Decide, once, at `daemon serve` start, whether this daemon starts
    /// agents in cgroups of their own, and set the subtree up if so. Call it
    /// before any agent is started. A daemon that never calls it — every
    /// in-process test daemon — places nothing.
    pub fn init() {
        RUN_CGROUPS.get_or_init(|| match set_up() {
            Ok(cgroups) => {
                tracing::info!(
                    base = %cgroups.base.display(),
                    "each agent run starts in its own cgroup (issue #1897)"
                );
                Some(cgroups)
            }
            Err(reason) => {
                tracing::info!(
                    "agent runs share the daemon's cgroup: {reason}; the collector groups them by environment (issue #1897)"
                );
                None
            }
        });
    }

    fn set_up() -> Result<RunCgroups, String> {
        if std::env::var(RUN_CGROUPS_ENV).is_ok_and(|v| v.trim() == "0") {
            return Err(format!("{RUN_CGROUPS_ENV}=0"));
        }
        let own = std::fs::read_to_string("/proc/self/cgroup")
            .map_err(|e| format!("cannot read /proc/self/cgroup ({e})"))?;
        let path = unified_path(&own).ok_or("the host is not on a pure cgroup v2 hierarchy")?;
        let unit = unit_cgroup(path).ok_or_else(|| {
            format!("the daemon's cgroup {path} is not a systemd service or scope")
        })?;
        let show = systemctl_show_delegate(&unit)?;
        if !delegated(&show) {
            return Err(format!(
                "systemd has not delegated {} (Delegate=no)",
                unit.unit
            ));
        }
        let base = Path::new(CGROUP_MOUNT).join(unit.path.trim_start_matches('/'));
        let leaf = base.join(DAEMON_LEAF);
        mkdir(&leaf).map_err(|e| format!("cannot create {} ({e})", leaf.display()))?;
        write(&leaf.join("cgroup.procs"), "0")
            .map_err(|e| format!("cannot move the daemon into {} ({e})", leaf.display()))?;
        // Without the memory controller a run's cgroup still counts its CPU
        // (`cpu.stat` is in every cgroup), so a refusal here is not fatal.
        // The kernel answers `EBUSY` while any other process is left in the
        // unit's cgroup, such as one the login shell that gave the daemon
        // its `PATH` started and left behind, so it is tried again for a
        // moment here and then by every sweep until it succeeds.
        let mut enabled = false;
        for attempt in 0..10 {
            match enable_controllers(&base) {
                Ok(()) => {
                    enabled = true;
                    break;
                }
                Err(e) if attempt == 9 => tracing::warn!(
                    base = %base.display(),
                    "cannot enable the memory and pids controllers for agent cgroups yet ({e}); \
                     their CPU is counted, their memory only once this succeeds, tried again every minute"
                ),
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        let cgroups = RunCgroups { base, leaf };
        let base = cgroups.base.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("run-cgroup-sweep".into())
            .spawn(move || sweep_forever(&base, enabled))
        {
            tracing::warn!(
                "cannot start the agent cgroup sweep ({e}); empty ones stay until the unit stops"
            );
        }
        Ok(cgroups)
    }

    /// Hand `memory` and `pids`, those `base` has, to its children. On
    /// a refusal, the error names the processes still in `base`.
    fn enable_controllers(base: &Path) -> Result<(), String> {
        let available =
            std::fs::read_to_string(base.join("cgroup.controllers")).unwrap_or_default();
        let Some(line) = controllers_to_enable(&available) else {
            return Ok(());
        };
        write(&base.join("cgroup.subtree_control"), &line).map_err(|e| {
            let left: Vec<String> = std::fs::read_to_string(base.join("cgroup.procs"))
                .unwrap_or_default()
                .lines()
                .take(5)
                .map(|pid| {
                    let comm = std::fs::read_to_string(format!("/proc/{}/comm", pid.trim()))
                        .unwrap_or_default();
                    format!("{} ({})", pid.trim(), comm.trim())
                })
                .collect();
            if left.is_empty() {
                e.to_string()
            } else {
                format!("{e}; still in the unit's cgroup: {}", left.join(", "))
            }
        })
    }

    /// `systemctl [--user] show -p Delegate --value <unit>`, bounded by
    /// [`SHOW_TIMEOUT`].
    fn systemctl_show_delegate(unit: &UnitCgroup) -> Result<String, String> {
        let mut cmd = Command::new("systemctl");
        if unit.manager == Manager::User {
            cmd.arg("--user");
        }
        cmd.args(["show", "-p", "Delegate", "--value", "--", &unit.unit])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot run systemctl ({e})"))?;
        let deadline = Instant::now() + SHOW_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => break,
                Ok(Some(status)) => return Err(format!("systemctl show exited with {status}")),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("systemctl show did not answer in time".into());
                }
                Err(e) => return Err(format!("cannot wait for systemctl ({e})")),
            }
        }
        let mut out = String::new();
        if let Some(mut stdout) = child.stdout.take() {
            stdout
                .read_to_string(&mut out)
                .map_err(|e| format!("cannot read systemctl's answer ({e})"))?;
        }
        Ok(out)
    }

    /// Move the agent `child_pid` just started into a cgroup of its own:
    /// `run-<run_id>`, or `agent-<pid>` when it has no run. A no-op when
    /// [`init`] found no delegated subtree. Never fails: a refusal leaves the
    /// agent in the daemon's cgroup and is logged.
    ///
    /// The agent is moved after it started, because portable-pty gives the
    /// spawn no hook before `exec`. Whatever it forked before the move stays
    /// behind, so the daemon's leaf is then searched for processes in the
    /// agent's session or below it, and those are moved too. A process that
    /// left the agent's session and was reparented in that same window, a few
    /// microseconds, is the one this can miss.
    pub fn place(child_pid: u32, run_id: Option<&str>) {
        let Some(Some(cgroups)) = RUN_CGROUPS.get() else {
            return;
        };
        let dir = cgroups.base.join(run_cgroup_name(run_id, child_pid));
        if let Err(e) = mkdir(&dir) {
            tracing::warn!(
                pid = child_pid,
                "cannot create {} ({e}); the agent shares the daemon's cgroup",
                dir.display()
            );
            return;
        }
        let procs = dir.join("cgroup.procs");
        if let Err(e) = write(&procs, &child_pid.to_string()) {
            // The agent may already have exited, which is no failure.
            if Path::new(&format!("/proc/{child_pid}")).exists() {
                tracing::warn!(
                    pid = child_pid,
                    "cannot move the agent into {} ({e}); it shares the daemon's cgroup",
                    dir.display()
                );
            }
            let _ = std::fs::remove_dir(&dir);
            return;
        }
        let own = std::process::id();
        for _ in 0..3 {
            let mut moved = false;
            let leftover =
                std::fs::read_to_string(cgroups.leaf.join("cgroup.procs")).unwrap_or_default();
            for pid in leftover
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
            {
                if pid != own && belongs_to(pid, child_pid, stat_of) {
                    moved |= write(&procs, &pid.to_string()).is_ok();
                }
            }
            if !moved {
                break;
            }
        }
    }

    fn stat_of(pid: u32) -> Option<(u32, u32)> {
        ppid_and_session(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
    }

    fn mkdir(dir: &Path) -> std::io::Result<()> {
        match std::fs::create_dir(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            other => other,
        }
    }

    /// One `write(2)` of `value`: a cgroup interface file takes a whole
    /// value per write.
    fn write(file: &Path, value: &str) -> std::io::Result<()> {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .write(true)
            .open(file)?
            .write_all(value.as_bytes())
    }

    /// Remove agent cgroups that two sweeps in a row found empty, and enable
    /// the controllers until that has succeeded.
    fn sweep_forever(base: &Path, mut enabled: bool) {
        let mut empty_last_time = HashSet::new();
        loop {
            std::thread::sleep(SWEEP_INTERVAL);
            if !enabled && enable_controllers(base).is_ok() {
                enabled = true;
                tracing::info!(
                    base = %base.display(),
                    "the memory and pids controllers are enabled for agent cgroups"
                );
            }
            empty_last_time = sweep(base, &empty_last_time, |dir| {
                // `rmdir` refuses a cgroup that became populated meanwhile.
                let _ = std::fs::remove_dir(dir);
            });
        }
    }

    /// One sweep of `base`: `remove` each agent cgroup that is empty now and
    /// was in `empty_last_time`, and answer the empty ones it left.
    fn sweep(
        base: &Path,
        empty_last_time: &HashSet<String>,
        remove: impl Fn(&Path),
    ) -> HashSet<String> {
        let mut empty_now = HashSet::new();
        let Ok(entries) = std::fs::read_dir(base) else {
            return empty_now;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !is_agent_cgroup(&name) {
                continue;
            }
            let events =
                std::fs::read_to_string(entry.path().join("cgroup.events")).unwrap_or_default();
            if populated(&events) != Some(false) {
                continue;
            }
            if empty_last_time.contains(&name) {
                remove(&entry.path());
            } else {
                empty_now.insert(name);
            }
        }
        empty_now
    }

    #[cfg(test)]
    mod tests {
        use std::cell::RefCell;

        use super::*;

        #[test]
        fn run_cgroups_001_a_sweep_removes_only_what_two_sweeps_found_empty() {
            // Plain directories stand in for cgroups, each with the
            // `cgroup.events` the kernel would show.
            let base = tempfile::tempdir().unwrap();
            let run = format!("{RUN_PREFIX}{}", "a".repeat(32));
            let busy = format!("{AGENT_PREFIX}42");
            let later = format!("{AGENT_PREFIX}43");
            let events = |name: &str, populated: &str| {
                let dir = base.path().join(name);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("cgroup.events"),
                    format!("populated {populated}\nfrozen 0\n"),
                )
                .unwrap();
            };
            events(&run, "0");
            events(&busy, "1");
            events(DAEMON_LEAF, "0");
            let removed = RefCell::new(Vec::new());
            let remove = |dir: &Path| {
                removed
                    .borrow_mut()
                    .push(dir.file_name().unwrap().to_string_lossy().into_owned())
            };
            let first = sweep(base.path(), &HashSet::new(), remove);
            assert_eq!(first, HashSet::from([run.clone()]));
            assert!(
                removed.borrow().is_empty(),
                "one empty sweep removes nothing"
            );
            // One that empties only now waits for the next sweep.
            events(&later, "0");
            let second = sweep(base.path(), &first, remove);
            assert_eq!(*removed.borrow(), vec![run.clone()]);
            assert!(second.contains(&later) && !second.contains(&busy));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_cgroups_002_only_a_pure_cgroup_v2_path_is_read() {
        assert_eq!(
            unified_path("0::/user.slice/user-1000.slice/session-8.scope\n"),
            Some("/user.slice/user-1000.slice/session-8.scope")
        );
        // Hybrid: v1 hierarchies listed next to the unified one.
        assert_eq!(unified_path("12:pids:/user.slice\n0::/user.slice\n"), None);
        assert_eq!(unified_path("1:name=systemd:/x\n"), None);
        assert_eq!(unified_path(""), None);
        assert_eq!(unified_path("0::relative\n"), None);
    }

    #[test]
    fn run_cgroups_003_the_unit_and_its_manager_are_read_from_the_path() {
        let user = "/user.slice/user-1000.slice/user@1000.service/app.slice/dot-agent-deck.service";
        assert_eq!(
            unit_cgroup(user),
            Some(UnitCgroup {
                path: user.into(),
                unit: "dot-agent-deck.service".into(),
                manager: Manager::User,
            })
        );
        // A daemon an earlier one started in the leaf uses the same unit.
        assert_eq!(
            unit_cgroup(&format!("{user}/{DAEMON_LEAF}")).unwrap().path,
            user
        );
        assert_eq!(
            unit_cgroup("/system.slice/dot-agent-deck.service")
                .unwrap()
                .manager,
            Manager::System
        );
        // A login session's scope is a unit too; whether it is delegated is
        // systemd's answer, not the path's.
        assert_eq!(
            unit_cgroup("/user.slice/user-1000.slice/session-8.scope")
                .unwrap()
                .unit,
            "session-8.scope"
        );
        // An agent's own cgroup, or the root, is no unit: a daemon started
        // inside another deck's agent places nothing.
        assert_eq!(unit_cgroup(&format!("{user}/run-{}", "a".repeat(32))), None);
        assert_eq!(unit_cgroup("/"), None);
        assert_eq!(
            unit_cgroup("/user.slice/user@x.service/a.service")
                .unwrap()
                .manager,
            Manager::System,
            "only a numeric user@<uid>.service is the user manager"
        );
    }

    #[test]
    fn run_cgroups_004_delegation_is_systemds_yes_alone() {
        assert!(delegated("yes\n"));
        assert!(!delegated("no\n"));
        assert!(!delegated(""));
        assert!(!delegated("yes no\n"));
    }

    #[test]
    fn run_cgroups_005_only_available_controllers_are_enabled() {
        assert_eq!(
            controllers_to_enable("cpu memory pids\n").as_deref(),
            Some("+memory +pids"),
            "cpu would change how runs share the CPU"
        );
        assert_eq!(controllers_to_enable("cpu io\n"), None);
        assert_eq!(
            controllers_to_enable("memory io\n").as_deref(),
            Some("+memory")
        );
        assert_eq!(controllers_to_enable("io\n"), None);
        assert_eq!(controllers_to_enable(""), None);
    }

    #[test]
    fn run_cgroups_006_names_and_which_ones_a_sweep_may_remove() {
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(run_cgroup_name(Some(id), 7), format!("run-{id}"));
        assert_eq!(run_cgroup_name(Some(""), 7), "agent-7");
        assert_eq!(run_cgroup_name(Some("../escape"), 7), "agent-7");
        assert_eq!(run_cgroup_name(None, 7), "agent-7");
        assert!(is_agent_cgroup(&format!("run-{id}")));
        assert!(is_agent_cgroup("agent-7"));
        for other in [
            DAEMON_LEAF,
            "run-",
            "agent-",
            "agent-x",
            "run-short",
            "cgroup.procs",
        ] {
            assert!(!is_agent_cgroup(other), "{other}");
        }
    }

    #[test]
    fn run_cgroups_007_cgroup_events_and_proc_stat_are_parsed() {
        assert_eq!(populated("populated 1\nfrozen 0\n"), Some(true));
        assert_eq!(populated("populated 0\nfrozen 0\n"), Some(false));
        assert_eq!(populated("frozen 0\n"), None);
        // A command name with spaces and a `)` in it.
        assert_eq!(
            ppid_and_session("4242 (a) b (c) S 4000 4242 4100 0 -1"),
            Some((4000, 4100))
        );
        assert_eq!(ppid_and_session("garbage"), None);
    }

    #[test]
    fn run_cgroups_008_a_process_belongs_to_the_agent_by_session_or_ancestry() {
        // pid -> (ppid, session)
        let table = |pid: u32| match pid {
            100 => Some((1, 100)),   // the agent, a session leader
            101 => Some((100, 100)), // its child
            102 => Some((101, 102)), // a grandchild that left the session
            103 => Some((1, 100)),   // reparented to init, still in the session
            200 => Some((1, 200)),   // another agent
            300 => Some((50, 50)),   // the daemon's own helper
            50 => Some((1, 50)),
            _ => None,
        };
        for pid in [100, 101, 102, 103] {
            assert!(belongs_to(pid, 100, table), "{pid}");
        }
        for pid in [200, 300, 999] {
            assert!(!belongs_to(pid, 100, table), "{pid}");
        }
        // A chain longer than any real ancestry ends.
        assert!(!belongs_to(1, 100, |p| Some((p + 2, p))));
    }
}
