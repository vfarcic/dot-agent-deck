//! Attribution and accounting: which run each process belongs to, and how
//! much CPU and memory each run used, sample by sample.
//!
//! **Attribution.** A process whose own environment names a run belongs to
//! it, and stays in it when it later execs with an environment that names
//! none; when it execs with one that names another run, it moves there from
//! that sample on, and what it was credited before stays with the run it
//! left. One whose environment names none takes its parent's run (a test
//! harness or a tool that replaced its environment) and keeps it when its
//! parent exits; a later sample in which its own environment names a run (it
//! exec'd with a new one) moves it there. One with neither is unattributed,
//! and is looked at again every sample. Its environment is read at every
//! sample, which is what notices an exec.
//!
//! **Users.** A run is keyed by its id *and* the user its processes run as
//! ([`RunKey`]): a process takes a run from its own environment under its
//! own user, and inherits only from a parent of the same user. So a process
//! of another user that names a run id, by accident or to pass its use off
//! as another user's, is a separate run, however its id is written.
//!
//! **CPU.** Each process's `total` is `utime + stime + cutime + cstime`, so a
//! child it reaped is in it. A process is credited its whole `total` the
//! first time it is seen and its increase afterwards. A process that vanished
//! since the last sample was reaped by an ancestor whose children time now
//! holds all of it, part of which was already credited while it was alive: so
//! its last-seen `total` is taken back from the nearest surviving ancestor
//! (walking up through ancestors that vanished too), up to that ancestor's
//! increase in children time. What a process used after its last sample and
//! was reaped by a process outside the visible set is lost; the measurements
//! in PRD #1859 bound how much.

use std::collections::{BTreeMap, HashMap};

use super::identity::RunIdentity;
use super::procfs::{ProcStat, Pss};

/// How far a vanished process's last-known ancestry is followed.
const MAX_ANCESTRY: usize = 64;

/// What the reader learned about one process's environment this sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Environ {
    /// Read: the run it names, if any.
    Read(Option<RunIdentity>),
    /// Not readable: a process of the collector's own user that made itself
    /// non-dumpable, whose environment and memory figures only root may
    /// read. It names no run of its own: it keeps a run it was already
    /// attributed to (sticky, by its own environment or its parent's), and
    /// otherwise can take only its parent's.
    Unreadable,
}

/// One visible process in a sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSample {
    pub pid: u32,
    /// The effective user the process runs as (`/proc/<pid>/status`).
    pub uid: u32,
    pub stat: ProcStat,
    pub environ: Environ,
    /// `None` when `smaps_rollup` could not be read.
    pub pss: Option<Pss>,
}

/// What one run, or the unattributed bucket, used.
#[derive(Debug, Clone, PartialEq)]
pub struct Figures {
    /// Cumulative CPU since the series started, in seconds.
    pub cpu_seconds: f64,
    /// CPU credited in this sample alone, in seconds.
    pub cpu_seconds_delta: f64,
    /// When the cumulative series started (Unix nanoseconds).
    pub start_unix_nanos: u64,
    /// The previous sample that reported this series, if any.
    pub previous_unix_nanos: Option<u64>,
    pub pss: Pss,
    pub processes: u64,
}

/// What a run is aggregated under: its id and its processes' user.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunKey {
    pub run_id: String,
    pub uid: u32,
}

/// One run's figures in a sample.
#[derive(Debug, Clone, PartialEq)]
pub struct RunFigures {
    pub identity: RunIdentity,
    /// The user its processes run as.
    pub uid: u32,
    pub figures: Figures,
    /// No live process this sample: the run's last report.
    pub ended: bool,
    /// Its live processes' pids, oldest first: what the worktree of the run
    /// is read from.
    pub members: Vec<u32>,
}

impl RunFigures {
    pub fn key(&self) -> RunKey {
        RunKey {
            run_id: self.identity.run_id.clone(),
            uid: self.uid,
        }
    }
}

/// The result of one sample.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub time_unix_nanos: u64,
    /// By run id, then user.
    pub runs: Vec<RunFigures>,
    pub unattributed: Figures,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Attribution {
    Run(RunKey),
    Unattributed,
}

/// `(pid, starttime)`: a reused pid is a different process.
type Key = (u32, u64);

#[derive(Debug, Clone)]
struct Tracked {
    ppid: u32,
    uid: u32,
    total: u64,
    children: u64,
    attribution: Attribution,
    /// Attributed by its own environment: an environment that names no run
    /// later does not take it out of that run.
    own: bool,
}

#[derive(Debug, Clone)]
struct Series {
    ticks: u64,
    start_unix_nanos: u64,
    last_unix_nanos: Option<u64>,
}

impl Series {
    fn new(start_unix_nanos: u64) -> Self {
        Series {
            ticks: 0,
            start_unix_nanos,
            last_unix_nanos: None,
        }
    }
}

#[derive(Debug, Clone)]
struct RunState {
    identity: RunIdentity,
    series: Series,
}

/// The state carried from one sample to the next.
#[derive(Debug)]
pub struct Aggregator {
    clk_tck: u64,
    boot_unix_secs: u64,
    tracked: HashMap<Key, Tracked>,
    runs: BTreeMap<RunKey, RunState>,
    unattributed: Series,
}

impl Aggregator {
    /// `clk_tck` is `sysconf(_SC_CLK_TCK)`, `boot_unix_secs` `/proc/stat`'s
    /// `btime`, and `started_unix_nanos` when the collector started, which is
    /// where the unattributed series starts.
    pub fn new(clk_tck: u64, boot_unix_secs: u64, started_unix_nanos: u64) -> Self {
        Aggregator {
            clk_tck: clk_tck.max(1),
            boot_unix_secs,
            tracked: HashMap::new(),
            runs: BTreeMap::new(),
            unattributed: Series::new(started_unix_nanos),
        }
    }

    fn ticks_to_seconds(&self, ticks: u64) -> f64 {
        ticks as f64 / self.clk_tck as f64
    }

    /// When a process that started `starttime` ticks after boot started.
    fn start_unix_nanos(&self, starttime: u64) -> u64 {
        let since_boot = u128::from(starttime) * 1_000_000_000 / u128::from(self.clk_tck);
        let boot = u128::from(self.boot_unix_secs) * 1_000_000_000;
        u64::try_from(boot + since_boot).unwrap_or(u64::MAX)
    }

    /// Account one sample of every visible process, taken at
    /// `time_unix_nanos`.
    pub fn step(&mut self, time_unix_nanos: u64, mut processes: Vec<ProcessSample>) -> Sample {
        // Oldest first, so a parent is attributed before its children.
        processes.sort_by_key(|p| (p.stat.starttime, p.pid));

        let mut current: HashMap<Key, Tracked> = HashMap::with_capacity(processes.len());
        let mut by_pid: HashMap<u32, Key> = HashMap::with_capacity(processes.len());
        let mut new_runs: HashMap<RunKey, u64> = HashMap::new();
        for p in &processes {
            let key = (p.pid, p.stat.starttime);
            let prev = self.tracked.get(&key);
            let own_run = match &p.environ {
                Environ::Read(Some(identity)) => Some(identity),
                _ => None,
            };
            // What it was attributed to stays only while it runs as the
            // same user (a setuid exec changes that).
            let prev = prev.filter(|t| t.uid == p.uid);
            let (attribution, own) = match (prev, own_run) {
                // Its own environment names a run: that run's, wherever it
                // was before. A process that exec'd into another run's
                // environment moves; its earlier credit stays where it was.
                (_, Some(identity)) => {
                    let run = RunKey {
                        run_id: identity.run_id.clone(),
                        uid: p.uid,
                    };
                    if !self.runs.contains_key(&run) {
                        self.runs.insert(
                            run.clone(),
                            RunState {
                                identity: identity.clone(),
                                series: Series::new(0),
                            },
                        );
                        new_runs.insert(run.clone(), u64::MAX);
                    }
                    (Attribution::Run(run), true)
                }
                // It names none now (it exec'd with a cleared environment,
                // or made itself unreadable): it stays where it was.
                (Some(t), None) if t.own => (t.attribution.clone(), true),
                (Some(t), None) if matches!(t.attribution, Attribution::Run(_)) => {
                    (t.attribution.clone(), false)
                }
                _ => {
                    let parent = by_pid
                        .get(&p.stat.ppid)
                        .filter(|pk| pk.1 <= key.1)
                        .and_then(|pk| current.get(pk))
                        .filter(|parent| parent.uid == p.uid);
                    let attribution = parent
                        .map(|t| t.attribution.clone())
                        .unwrap_or(Attribution::Unattributed);
                    (attribution, false)
                }
            };
            if let Attribution::Run(run) = &attribution
                && let Some(start) = new_runs.get_mut(run)
            {
                *start = (*start).min(p.stat.starttime);
            }
            current.insert(
                key,
                Tracked {
                    ppid: p.stat.ppid,
                    uid: p.uid,
                    total: p.stat.total_ticks(),
                    children: p.stat.children_ticks(),
                    attribution,
                    own,
                },
            );
            by_pid.insert(p.pid, key);
        }
        for (run, starttime) in new_runs {
            // Never after the sample: a series cannot start after its first
            // point, whatever the clocks say.
            let start = self.start_unix_nanos(starttime).min(time_unix_nanos);
            if let Some(run) = self.runs.get_mut(&run) {
                run.series.start_unix_nanos = start;
            }
        }

        // Credit each process its increase, or its whole total when new.
        let mut credit: HashMap<Key, i128> = HashMap::with_capacity(current.len());
        let mut budget: HashMap<Key, u64> = HashMap::with_capacity(current.len());
        for (key, now) in &current {
            match self.tracked.get(key) {
                Some(prev) => {
                    credit.insert(*key, i128::from(now.total.saturating_sub(prev.total)));
                    budget.insert(*key, now.children.saturating_sub(prev.children));
                }
                None => {
                    credit.insert(*key, i128::from(now.total));
                    budget.insert(*key, 0);
                }
            }
        }
        // Take back what a vanished process was already credited from the
        // ancestor that reaped it.
        let prev_by_pid: HashMap<u32, Key> = self.tracked.keys().map(|k| (k.0, *k)).collect();
        for (key, gone) in &self.tracked {
            if current.contains_key(key) {
                continue;
            }
            let mut node = gone;
            let mut reaper = None;
            for _ in 0..MAX_ANCESTRY {
                if let Some(pk) = by_pid.get(&node.ppid)
                    && self.tracked.contains_key(pk)
                {
                    reaper = Some(*pk);
                    break;
                }
                match prev_by_pid.get(&node.ppid) {
                    Some(vk) if !current.contains_key(vk) => node = &self.tracked[vk],
                    _ => break,
                }
            }
            if let Some(pk) = reaper
                && let Some(left) = budget.get_mut(&pk)
            {
                let taken = gone.total.min(*left);
                *left -= taken;
                *credit.get_mut(&pk).expect("every current key has a credit") -= i128::from(taken);
            }
        }

        // Sum per run and for the unattributed bucket.
        let mut run_ticks: HashMap<&RunKey, u64> = HashMap::new();
        let mut unattributed_ticks = 0u64;
        for (key, c) in &credit {
            let ticks = u64::try_from((*c).max(0)).unwrap_or(u64::MAX);
            match &current[key].attribution {
                Attribution::Run(run) => *run_ticks.entry(run).or_default() += ticks,
                Attribution::Unattributed => unattributed_ticks += ticks,
            }
        }
        let mut run_pss: HashMap<&RunKey, (Pss, Vec<u32>)> = HashMap::new();
        let mut unattributed_pss = Pss::default();
        let mut unattributed_processes = 0u64;
        for p in &processes {
            let pss = p.pss.unwrap_or_default();
            match &current[&(p.pid, p.stat.starttime)].attribution {
                Attribution::Run(run) => {
                    let entry = run_pss.entry(run).or_default();
                    entry.0.add(&pss);
                    entry.1.push(p.pid);
                }
                Attribution::Unattributed => {
                    unattributed_pss.add(&pss);
                    unattributed_processes += 1;
                }
            }
        }

        let mut runs = Vec::with_capacity(self.runs.len());
        let mut ended = Vec::new();
        for (key, run) in &mut self.runs {
            let ticks = run_ticks.get(key).copied().unwrap_or(0);
            let (pss, members) = run_pss.remove(key).unwrap_or_default();
            let previous = run.series.last_unix_nanos;
            run.series.ticks += ticks;
            run.series.last_unix_nanos = Some(time_unix_nanos);
            if members.is_empty() {
                ended.push(key.clone());
            }
            runs.push(RunFigures {
                identity: run.identity.clone(),
                uid: key.uid,
                figures: Figures {
                    cpu_seconds: run.series.ticks as f64 / self.clk_tck as f64,
                    cpu_seconds_delta: ticks as f64 / self.clk_tck as f64,
                    start_unix_nanos: run.series.start_unix_nanos,
                    previous_unix_nanos: previous,
                    pss,
                    processes: members.len() as u64,
                },
                ended: members.is_empty(),
                members,
            });
        }
        for key in ended {
            self.runs.remove(&key);
        }

        let previous = self.unattributed.last_unix_nanos;
        self.unattributed.ticks += unattributed_ticks;
        self.unattributed.last_unix_nanos = Some(time_unix_nanos);
        let unattributed = Figures {
            cpu_seconds: self.ticks_to_seconds(self.unattributed.ticks),
            cpu_seconds_delta: self.ticks_to_seconds(unattributed_ticks),
            start_unix_nanos: self.unattributed.start_unix_nanos,
            previous_unix_nanos: previous,
            pss: unattributed_pss,
            processes: unattributed_processes,
        };

        self.tracked = current;
        Sample {
            time_unix_nanos,
            runs,
            unattributed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLK: u64 = 100;
    const BOOT: u64 = 1_790_000_000;
    const STARTED: u64 = 1_790_000_500_000_000_000;

    fn identity(run_id: &str) -> RunIdentity {
        RunIdentity {
            run_id: run_id.into(),
            attributes: vec![("deck.pane.id".into(), format!("pane-{run_id}"))],
        }
    }

    /// A process: own CPU `(utime + stime)` and reaped children's CPU, both
    /// in ticks, started `start` ticks after boot.
    #[derive(Clone)]
    struct P {
        pid: u32,
        ppid: u32,
        start: u64,
        own: u64,
        children: u64,
        run: Option<&'static str>,
        pss: Option<Pss>,
        uid: u32,
    }

    const UID: u32 = 1000;

    fn p(pid: u32, ppid: u32, start: u64, run: Option<&'static str>) -> P {
        P {
            pid,
            ppid,
            start,
            own: 0,
            children: 0,
            run,
            pss: None,
            uid: UID,
        }
    }

    impl P {
        fn cpu(mut self, own: u64, children: u64) -> Self {
            self.own = own;
            self.children = children;
            self
        }
        fn uid(mut self, uid: u32) -> Self {
            self.uid = uid;
            self
        }
        fn pss(mut self, anon: u64, file: u64, shmem: u64) -> Self {
            self.pss = Some(Pss { anon, file, shmem });
            self
        }
        fn sample(&self) -> ProcessSample {
            let environ = Environ::Read(self.run.map(identity));
            ProcessSample {
                pid: self.pid,
                uid: self.uid,
                stat: ProcStat {
                    ppid: self.ppid,
                    flags: 0,
                    utime: self.own,
                    stime: 0,
                    cutime: self.children,
                    cstime: 0,
                    starttime: self.start,
                },
                environ,
                pss: self.pss,
            }
        }
    }

    fn step(agg: &mut Aggregator, t: u64, procs: &[P]) -> Sample {
        let samples = procs.iter().map(P::sample).collect();
        agg.step(STARTED + t * 1_000_000_000, samples)
    }

    fn run<'a>(sample: &'a Sample, run_id: &str) -> &'a RunFigures {
        sample
            .runs
            .iter()
            .find(|r| r.identity.run_id == run_id)
            .unwrap_or_else(|| panic!("no run {run_id} in {sample:#?}"))
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn first_sight_credits_everything_since_start() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        let s = step(
            &mut agg,
            0,
            &[
                p(10, 1, 500, Some("r1")).cpu(250, 50),
                p(11, 10, 600, Some("r1")).cpu(100, 0),
            ],
        );
        let r = run(&s, "r1");
        assert!(approx(r.figures.cpu_seconds, 4.0));
        assert!(approx(r.figures.cpu_seconds_delta, 4.0));
        assert_eq!(r.figures.processes, 2);
        assert_eq!(r.members, vec![10, 11]);
        assert!(!r.ended);
        // The series starts when the run's oldest process started: 5 s after boot.
        assert_eq!(
            r.figures.start_unix_nanos,
            BOOT * 1_000_000_000 + 5_000_000_000
        );
        assert_eq!(r.figures.previous_unix_nanos, None);
        assert_eq!(r.identity, identity("r1"));
    }

    #[test]
    fn later_samples_credit_only_the_increase() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        step(&mut agg, 0, &[p(10, 1, 500, Some("r1")).cpu(100, 0)]);
        let s = step(&mut agg, 15, &[p(10, 1, 500, Some("r1")).cpu(160, 0)]);
        let r = run(&s, "r1");
        assert!(approx(r.figures.cpu_seconds, 1.6));
        assert!(approx(r.figures.cpu_seconds_delta, 0.6));
        assert_eq!(r.figures.previous_unix_nanos, Some(STARTED));
    }

    #[test]
    fn a_child_seen_alive_then_reaped_is_counted_once() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        // The child has used 1 s when first seen, and 3 s by the time its
        // parent reaps it: the parent's children time grows by 3 s.
        step(
            &mut agg,
            0,
            &[
                p(10, 1, 500, Some("r1")).cpu(10, 0),
                p(11, 10, 600, Some("r1")).cpu(100, 0),
            ],
        );
        let s = step(&mut agg, 15, &[p(10, 1, 500, Some("r1")).cpu(10, 300)]);
        assert!(approx(run(&s, "r1").figures.cpu_seconds, 3.1));
    }

    #[test]
    fn a_chain_that_vanished_together_is_taken_back_from_the_survivor() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        // shell -> timeout -> yes; timeout and yes exit in the same interval,
        // so the shell's children time holds both.
        step(
            &mut agg,
            0,
            &[
                p(10, 1, 500, Some("r1")).cpu(5, 0),
                p(11, 10, 600, Some("r1")).cpu(1, 0),
                p(12, 11, 601, Some("r1")).cpu(200, 0),
            ],
        );
        let s = step(&mut agg, 15, &[p(10, 1, 500, Some("r1")).cpu(5, 401)]);
        // 2.06 s first seen; then the shell's reaped 4.01 s, of which 2.01 s
        // was already credited.
        assert!(approx(run(&s, "r1").figures.cpu_seconds, 4.06));
    }

    #[test]
    fn the_take_back_never_exceeds_what_the_reaper_gained() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        // The child was reparented elsewhere and reaped there: its parent's
        // children time does not grow, so nothing is taken back from it.
        step(
            &mut agg,
            0,
            &[
                p(10, 1, 500, Some("r1")).cpu(50, 0),
                p(11, 10, 600, Some("r1")).cpu(100, 0),
            ],
        );
        let s = step(&mut agg, 15, &[p(10, 1, 500, Some("r1")).cpu(70, 0)]);
        assert!(approx(run(&s, "r1").figures.cpu_seconds, 1.7));
    }

    #[test]
    fn a_double_forked_process_keeps_its_run() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        let s = step(
            &mut agg,
            0,
            &[
                p(10, 1, 500, Some("r1")).cpu(1, 0),
                // Reparented to pid 1, which the collector cannot see.
                p(20, 1, 700, Some("r1")).cpu(300, 0),
            ],
        );
        let r = run(&s, "r1");
        assert_eq!(r.figures.processes, 2);
        assert!(approx(r.figures.cpu_seconds, 3.01));
    }

    #[test]
    fn a_cleared_environment_takes_its_parents_run_and_keeps_it() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        let s = step(
            &mut agg,
            0,
            &[
                p(10, 1, 500, Some("r1")).cpu(1, 0),
                p(11, 10, 600, None).cpu(100, 0),
                p(12, 11, 700, None).cpu(100, 0),
            ],
        );
        assert_eq!(run(&s, "r1").figures.processes, 3);
        assert_eq!(s.unattributed.processes, 0);
        // The middle process exits and 12 is reparented to pid 1: it stays.
        let s = step(
            &mut agg,
            15,
            &[
                p(10, 1, 500, Some("r1")).cpu(1, 0),
                p(12, 1, 700, None).cpu(150, 0),
            ],
        );
        assert_eq!(run(&s, "r1").members, vec![10, 12]);
        assert_eq!(s.unattributed.processes, 0);
    }

    #[test]
    fn a_cleared_environment_outside_any_run_is_unattributed() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        let s = step(
            &mut agg,
            0,
            &[
                p(10, 1, 500, Some("r1")).cpu(1, 0),
                p(30, 1, 800, None).cpu(400, 0).pss(1024, 2048, 0),
            ],
        );
        assert_eq!(s.unattributed.processes, 1);
        assert!(approx(s.unattributed.cpu_seconds, 4.0));
        assert_eq!(s.unattributed.pss.anon, 1024);
        assert_eq!(s.unattributed.start_unix_nanos, STARTED);
        assert_eq!(run(&s, "r1").figures.processes, 1);
    }

    #[test]
    fn an_unattributed_process_that_execs_into_a_run_moves_to_it() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        let s = step(&mut agg, 0, &[p(10, 1, 500, None).cpu(100, 0)]);
        assert_eq!(s.unattributed.processes, 1);
        assert!(s.runs.is_empty());
        let s = step(&mut agg, 15, &[p(10, 1, 500, Some("r1")).cpu(150, 0)]);
        // The earlier second stays where it was credited.
        assert!(approx(s.unattributed.cpu_seconds, 1.0));
        assert!(approx(run(&s, "r1").figures.cpu_seconds, 0.5));
    }

    #[test]
    fn an_own_environment_is_sticky_through_one_that_names_no_run() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        step(&mut agg, 0, &[p(10, 1, 500, Some("r1"))]);
        // It exec'd with a cleared environment: it is still the run's.
        let s = step(&mut agg, 15, &[p(10, 1, 500, None).cpu(10, 0)]);
        assert_eq!(run(&s, "r1").figures.processes, 1);
        // And so is a child it starts with no environment of its own.
        let s = step(
            &mut agg,
            30,
            &[p(10, 1, 500, None).cpu(10, 0), p(11, 10, 600, None)],
        );
        assert_eq!(run(&s, "r1").members, vec![10, 11]);
    }

    /// Finding 2: a process that keeps its pid but execs with another run's
    /// environment moves to that run from that sample on; what it was
    /// credited before stays with the run it left, which ends when it was
    /// that run's last process.
    #[test]
    fn a_process_that_execs_into_another_run_moves_to_it() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        let s = step(&mut agg, 0, &[p(10, 1, 500, Some("a")).cpu(100, 0)]);
        assert!(approx(run(&s, "a").figures.cpu_seconds, 1.0));
        let s = step(&mut agg, 15, &[p(10, 1, 500, Some("b")).cpu(150, 0)]);
        let a = run(&s, "a");
        assert!(a.ended, "{a:?}");
        assert!(approx(a.figures.cpu_seconds, 1.0));
        let b = run(&s, "b");
        assert_eq!(b.members, vec![10]);
        // Only what it used since the last sample, not its whole total.
        assert!(approx(b.figures.cpu_seconds, 0.5));
        // It stays in b, and its child with no environment of its own
        // follows it.
        let s = step(
            &mut agg,
            30,
            &[p(10, 1, 500, None).cpu(170, 0), p(11, 10, 600, None)],
        );
        assert_eq!(run(&s, "b").members, vec![10, 11]);
        assert!(approx(run(&s, "b").figures.cpu_seconds, 0.7));
        assert!(s.runs.iter().all(|r| r.identity.run_id != "a"));
    }

    #[test]
    fn a_reused_pid_is_a_new_process() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        step(&mut agg, 0, &[p(10, 1, 500, Some("r1")).cpu(500, 0)]);
        let s = step(&mut agg, 15, &[p(10, 1, 900, Some("r2")).cpu(30, 0)]);
        assert!(approx(run(&s, "r2").figures.cpu_seconds, 0.3));
        assert!(run(&s, "r1").ended);
    }

    #[test]
    fn a_child_does_not_inherit_from_a_younger_process_with_its_parents_pid() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        // pid 10 started after 11 did, so it cannot be 11's parent.
        let s = step(
            &mut agg,
            0,
            &[p(10, 1, 900, Some("r1")), p(11, 10, 600, None)],
        );
        assert_eq!(s.unattributed.processes, 1);
        assert_eq!(run(&s, "r1").figures.processes, 1);
    }

    #[test]
    fn memory_is_summed_per_run_by_kind() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        let s = step(
            &mut agg,
            0,
            &[
                p(10, 1, 500, Some("r1")).pss(100, 10, 1),
                p(11, 10, 600, Some("r1")).pss(200, 20, 2),
                p(12, 10, 601, Some("r1")),
                p(20, 1, 700, Some("r2")).pss(5, 0, 0),
            ],
        );
        assert_eq!(
            run(&s, "r1").figures.pss,
            Pss {
                anon: 300,
                file: 30,
                shmem: 3
            }
        );
        assert_eq!(run(&s, "r1").figures.processes, 3);
        assert_eq!(run(&s, "r2").figures.pss.anon, 5);
    }

    #[test]
    fn a_run_with_no_live_process_reports_once_more_then_is_forgotten() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        step(
            &mut agg,
            0,
            &[
                p(10, 1, 500, Some("r1")).cpu(100, 0).pss(1, 1, 1),
                p(20, 1, 600, Some("r2")).cpu(1, 0),
            ],
        );
        let s = step(&mut agg, 15, &[p(20, 1, 600, Some("r2")).cpu(2, 0)]);
        let ended = run(&s, "r1");
        assert!(ended.ended);
        assert_eq!(ended.figures.processes, 0);
        assert_eq!(ended.figures.pss, Pss::default());
        assert!(approx(ended.figures.cpu_seconds, 1.0));
        assert!(approx(ended.figures.cpu_seconds_delta, 0.0));
        assert_eq!(
            ended.figures.start_unix_nanos,
            BOOT * 1_000_000_000 + 5_000_000_000
        );
        let s = step(&mut agg, 30, &[p(20, 1, 600, Some("r2")).cpu(3, 0)]);
        assert_eq!(s.runs.len(), 1);
        assert_eq!(s.runs[0].identity.run_id, "r2");
    }

    /// A run whose oldest process started after the sample's time (a slow
    /// read, or a clock that stepped back) starts its series at the sample,
    /// never after it.
    #[test]
    fn a_series_never_starts_after_its_first_sample() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        let at = BOOT * 1_000_000_000 + 5_000_000_000;
        // Started 6 s after boot, sampled 5 s after boot.
        let s = agg.step(at, vec![p(10, 1, 600, Some("r1")).cpu(1, 0).sample()]);
        assert_eq!(run(&s, "r1").figures.start_unix_nanos, at);
    }

    #[test]
    fn runs_are_reported_in_run_id_order_and_the_series_start_is_stable() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        let procs = [
            p(20, 1, 700, Some("b")).cpu(1, 0),
            p(10, 1, 500, Some("a")).cpu(1, 0),
        ];
        let first = step(&mut agg, 0, &procs);
        let ids: Vec<_> = first
            .runs
            .iter()
            .map(|r| r.identity.run_id.as_str())
            .collect();
        assert_eq!(ids, ["a", "b"]);
        // A process older than the run's first one, appearing later, does not
        // move the start of a series already reported.
        let mut later = procs.to_vec();
        later.push(p(30, 1, 100, Some("a")));
        let second = step(&mut agg, 15, &later);
        assert_eq!(
            run(&second, "a").figures.start_unix_nanos,
            run(&first, "a").figures.start_unix_nanos
        );
    }

    /// Another user's process naming the same run id is a separate run, and
    /// a child never inherits a run across users.
    #[test]
    fn runs_of_different_users_never_share_a_key() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        let s = step(
            &mut agg,
            0,
            &[
                p(10, 1, 500, Some("r1")).cpu(100, 0),
                p(11, 10, 600, None).cpu(50, 0).uid(0),
                p(20, 1, 700, Some("r1")).cpu(300, 0).uid(2000),
            ],
        );
        let mine: Vec<_> = s.runs.iter().filter(|r| r.uid == UID).collect();
        let theirs: Vec<_> = s.runs.iter().filter(|r| r.uid == 2000).collect();
        assert_eq!(s.runs.len(), 2, "{s:#?}");
        assert_eq!(mine[0].members, vec![10]);
        assert!(approx(mine[0].figures.cpu_seconds, 1.0));
        assert_eq!(theirs[0].members, vec![20]);
        assert!(approx(theirs[0].figures.cpu_seconds, 3.0));
        assert_eq!(mine[0].key().run_id, theirs[0].key().run_id);
        assert_ne!(mine[0].key(), theirs[0].key());
        // The root-owned child of a run process inherits nothing.
        assert_eq!(s.unattributed.processes, 1);
        // A process that changes user loses its sticky run.
        let s = step(
            &mut agg,
            15,
            &[
                p(10, 1, 500, None).cpu(110, 0).uid(0),
                p(20, 1, 700, Some("r1")).cpu(300, 0).uid(2000),
            ],
        );
        assert!(s.runs.iter().any(|r| r.uid == UID && r.ended));
        assert_eq!(s.unattributed.processes, 1);
    }

    #[test]
    fn counters_going_backwards_credit_nothing() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        step(&mut agg, 0, &[p(10, 1, 500, Some("r1")).cpu(100, 50)]);
        let s = step(&mut agg, 15, &[p(10, 1, 500, Some("r1")).cpu(90, 40)]);
        assert!(approx(run(&s, "r1").figures.cpu_seconds, 1.5));
    }

    #[test]
    fn the_unattributed_bucket_is_always_reported() {
        let mut agg = Aggregator::new(CLK, BOOT, STARTED);
        let s = agg.step(STARTED, Vec::new());
        assert_eq!(s.unattributed.processes, 0);
        assert!(approx(s.unattributed.cpu_seconds, 0.0));
        assert!(s.runs.is_empty());
    }
}
