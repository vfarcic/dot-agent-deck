//! Issue #1897: a run's CPU and memory read from the cgroup the deck started
//! it in, where it has one ([`crate::run_cgroup`]).
//!
//! The environment grouping in [`super::aggregate`] counts what stays in a
//! run's process tree, and misses short-lived work that detaches from it. A
//! run's cgroup holds every process forked inside it, so its `cpu.stat` and
//! `memory.current` are the run's whole use. They are sent as metrics of
//! their own (`deck.run.cgroup.*`) next to the environment-grouped ones,
//! which stay what they were: a run with no cgroup, because its daemon has no
//! delegated subtree, has only those.
//!
//! A run is matched to its cgroup through its own processes: the cgroup one
//! of its members is in must be named `run-<its deck.run.id>`. So a cgroup is
//! read only for a run that has a live member in it, and a run that ended is
//! not read again; what its detached processes use after that is not sent.

use std::collections::HashMap;

use super::aggregate::{RunKey, Sample};
use super::config::Temporality;
use super::otlp::{Metric, MetricData, NumberPoint, NumberValue, Request};
use super::{keys, metrics};
use crate::run_cgroup::RUN_PREFIX;

/// What one read of a run's cgroup found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgroupReading {
    /// `cpu.stat`'s `usage_usec`: CPU used in the cgroup since it was made.
    pub cpu_usec: u64,
    /// `memory.current`, when the memory controller is enabled for it.
    pub memory_current: Option<u64>,
    /// `memory.stat`'s `anon`, likewise.
    pub memory_anon: Option<u64>,
}

/// A run's cgroup figures in one sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CgroupPoint {
    pub reading: CgroupReading,
    /// Cumulative CPU in seconds.
    pub cpu_seconds: f64,
    /// CPU since the previous sample that read this cgroup, in seconds.
    pub cpu_seconds_delta: f64,
    /// That previous sample, if any.
    pub previous_unix_nanos: Option<u64>,
}

/// `usage_usec` in a `cpu.stat`.
pub fn cpu_usage_usec(cpu_stat: &str) -> Option<u64> {
    stat_field(cpu_stat, "usage_usec")
}

/// `anon` in a `memory.stat`.
pub fn memory_anon(memory_stat: &str) -> Option<u64> {
    stat_field(memory_stat, "anon")
}

fn stat_field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (k, v) = line.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok()).flatten()
    })
}

/// Whether cgroup `path` (from `/proc/<pid>/cgroup`) is the cgroup the deck
/// made for run `run_id`.
pub fn is_run_cgroup(path: &str, run_id: &str) -> bool {
    path.rsplit('/')
        .next()
        .and_then(|leaf| leaf.strip_prefix(RUN_PREFIX))
        .is_some_and(|id| !id.is_empty() && id == run_id)
}

/// The cgroup figures carried from one sample to the next.
#[derive(Debug, Default)]
pub struct RunCgroups {
    last: HashMap<RunKey, (u64, u64)>,
}

impl RunCgroups {
    /// Read the cgroup of each live run in `sample` through `read`, given the
    /// run's id and its members, and answer the points to send. A run whose
    /// cgroup was not found this sample keeps its last reading for the next
    /// delta. Ended runs are forgotten.
    pub fn update(
        &mut self,
        sample: &Sample,
        mut read: impl FnMut(&str, &[u32]) -> Option<CgroupReading>,
    ) -> HashMap<RunKey, CgroupPoint> {
        let mut points = HashMap::new();
        for run in &sample.runs {
            let key = run.key();
            if run.ended {
                self.last.remove(&key);
                continue;
            }
            let Some(reading) = read(&run.identity.run_id, &run.members) else {
                continue;
            };
            let previous = self.last.get(&key).copied();
            let delta_usec = match previous {
                // A cgroup's usage only grows; a smaller one is a different
                // cgroup under the same name, counted from its start.
                Some((usec, _)) if reading.cpu_usec >= usec => reading.cpu_usec - usec,
                _ => reading.cpu_usec,
            };
            self.last
                .insert(key.clone(), (reading.cpu_usec, sample.time_unix_nanos));
            points.insert(
                key,
                CgroupPoint {
                    reading,
                    cpu_seconds: reading.cpu_usec as f64 / 1e6,
                    cpu_seconds_delta: delta_usec as f64 / 1e6,
                    previous_unix_nanos: previous.map(|(_, at)| at),
                },
            );
        }
        points
    }
}

/// Add each run's cgroup metrics to its resource in `request`, which
/// [`super::otlp::build_request`] built from `sample`: one resource per run,
/// in `sample.runs`' order, first.
pub fn add_to_request(
    request: &mut Request,
    sample: &Sample,
    points: &HashMap<RunKey, CgroupPoint>,
    temporality: Temporality,
) {
    for (run, resource) in sample.runs.iter().zip(request.resources.iter_mut()) {
        debug_assert!(
            resource
                .attributes
                .iter()
                .any(|(k, v)| k == keys::RUN_ID && *v == run.identity.run_id),
            "build_request's resources start with the runs, in order"
        );
        if let Some(point) = points.get(&run.key()) {
            resource.metrics.extend(cgroup_metrics(
                point,
                run.figures.start_unix_nanos,
                sample.time_unix_nanos,
                temporality,
            ));
        }
    }
}

fn cgroup_metrics(
    point: &CgroupPoint,
    run_start_unix_nanos: u64,
    time: u64,
    temporality: Temporality,
) -> Vec<Metric> {
    let (start, cpu) = match temporality {
        Temporality::Cumulative => (run_start_unix_nanos, point.cpu_seconds),
        Temporality::Delta => (
            point.previous_unix_nanos.unwrap_or(run_start_unix_nanos),
            point.cpu_seconds_delta,
        ),
    };
    let mut out = vec![Metric {
        name: metrics::CGROUP_CPU_TIME,
        unit: "s",
        description: "CPU time used by every process in the run's cgroup",
        data: MetricData::Sum {
            temporality,
            monotonic: true,
            points: vec![NumberPoint {
                start_unix_nanos: Some(start),
                time_unix_nanos: time,
                value: NumberValue::Double(cpu),
                attributes: Vec::new(),
            }],
        },
    }];
    let memory: Vec<NumberPoint> = [
        ("current", point.reading.memory_current),
        ("anon", point.reading.memory_anon),
    ]
    .into_iter()
    .filter_map(|(kind, bytes)| {
        Some(NumberPoint {
            start_unix_nanos: None,
            time_unix_nanos: time,
            value: NumberValue::Int(i64::try_from(bytes?).unwrap_or(i64::MAX)),
            attributes: vec![(keys::MEMORY_KIND.to_string(), kind.to_string())],
        })
    })
    .collect();
    if !memory.is_empty() {
        out.push(Metric {
            name: metrics::CGROUP_MEMORY,
            unit: "By",
            description: "Memory charged to the run's cgroup",
            data: MetricData::Gauge { points: memory },
        });
    }
    out
}

/// Read the cgroup of run `run_id` through its `members`, oldest first: the
/// first member found in a cgroup named for the run. At most a few members
/// are tried, since every process of a run that has a cgroup is in it.
#[cfg(target_os = "linux")]
pub fn read(run_id: &str, members: &[u32]) -> Option<CgroupReading> {
    use std::fs;
    for pid in members.iter().take(4) {
        let Ok(own) = fs::read_to_string(format!("/proc/{pid}/cgroup")) else {
            continue;
        };
        let path = crate::run_cgroup::unified_path(&own)?;
        if !is_run_cgroup(path, run_id) {
            continue;
        }
        let dir = std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
        let cpu_usec = cpu_usage_usec(&fs::read_to_string(dir.join("cpu.stat")).ok()?)?;
        let memory_current = fs::read_to_string(dir.join("memory.current"))
            .ok()
            .and_then(|t| t.trim().parse().ok());
        let memory_anon = fs::read_to_string(dir.join("memory.stat"))
            .ok()
            .and_then(|t| memory_anon(&t));
        return Some(CgroupReading {
            cpu_usec,
            memory_current,
            memory_anon,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::collector::aggregate::{Figures, RunFigures};
    use crate::telemetry::collector::identity::RunIdentity;
    use crate::telemetry::collector::procfs::Pss;

    const RUN: &str = "0123456789abcdef0123456789abcdef";

    fn run(id: &str, ended: bool) -> RunFigures {
        RunFigures {
            identity: RunIdentity {
                run_id: id.into(),
                attributes: Vec::new(),
            },
            uid: 1000,
            figures: Figures {
                cpu_seconds: 1.0,
                cpu_seconds_delta: 1.0,
                start_unix_nanos: 5,
                previous_unix_nanos: None,
                pss: Pss::default(),
                processes: 1,
            },
            ended,
            members: if ended { Vec::new() } else { vec![42] },
        }
    }

    fn sample(time: u64, runs: Vec<RunFigures>) -> Sample {
        Sample {
            time_unix_nanos: time,
            runs,
            unattributed: run("x", false).figures,
        }
    }

    fn reading(cpu_usec: u64) -> CgroupReading {
        CgroupReading {
            cpu_usec,
            memory_current: Some(4096),
            memory_anon: None,
        }
    }

    #[test]
    fn run_cgroups_101_cgroup_files_are_parsed() {
        let cpu = "usage_usec 3037673\nuser_usec 2000000\nsystem_usec 1037673\n";
        assert_eq!(cpu_usage_usec(cpu), Some(3_037_673));
        assert_eq!(cpu_usage_usec("user_usec 5\n"), None);
        assert_eq!(
            memory_anon("anon 8192\nfile 4096\nanon_thp 0\n"),
            Some(8192)
        );
        assert_eq!(memory_anon("file 1\n"), None);
    }

    #[test]
    fn run_cgroups_102_only_the_cgroup_named_for_the_run_is_its_own() {
        let base = "/user.slice/user-1000.slice/user@1000.service/app.slice/dot-agent-deck.service";
        assert!(is_run_cgroup(&format!("{base}/run-{RUN}"), RUN));
        assert!(!is_run_cgroup(&format!("{base}/run-{RUN}"), "other"));
        assert!(!is_run_cgroup(
            &format!("{base}/dot-agent-deck-daemon"),
            RUN
        ));
        assert!(!is_run_cgroup(&format!("{base}/agent-42"), RUN));
        assert!(!is_run_cgroup(&format!("{base}/run-"), ""));
    }

    #[test]
    fn run_cgroups_103_cpu_is_cumulative_and_its_delta_is_since_the_last_read() {
        let mut cgroups = RunCgroups::default();
        let first = cgroups.update(&sample(10, vec![run(RUN, false)]), |_, _| {
            Some(reading(2_000_000))
        });
        let key = run(RUN, false).key();
        assert_eq!(first[&key].cpu_seconds, 2.0);
        assert_eq!(first[&key].cpu_seconds_delta, 2.0);
        assert_eq!(first[&key].previous_unix_nanos, None);
        // Not found this sample: no point, and the last reading stays.
        assert!(
            cgroups
                .update(&sample(20, vec![run(RUN, false)]), |_, _| None)
                .is_empty()
        );
        let third = cgroups.update(&sample(30, vec![run(RUN, false)]), |_, _| {
            Some(reading(5_500_000))
        });
        assert_eq!(third[&key].cpu_seconds, 5.5);
        assert_eq!(third[&key].cpu_seconds_delta, 3.5);
        assert_eq!(third[&key].previous_unix_nanos, Some(10));
        // An ended run is not read, and is forgotten.
        let ended = cgroups.update(&sample(40, vec![run(RUN, true)]), |_, _| panic!("read"));
        assert!(ended.is_empty());
        let again = cgroups.update(&sample(50, vec![run(RUN, false)]), |_, _| {
            Some(reading(1_000_000))
        });
        assert_eq!(again[&key].cpu_seconds_delta, 1.0);
    }

    #[test]
    fn run_cgroups_104_the_metrics_join_the_runs_own_resource() {
        use crate::telemetry::collector::config::Settings;
        use crate::telemetry::collector::otlp::build_request;
        let other = "fedcba9876543210fedcba9876543210";
        let sample = sample(30, vec![run(RUN, false), run(other, false)]);
        let settings = Settings {
            service_name: "dot-agent-deck-collector".into(),
            resource_pairs: Vec::new(),
            temporality: Temporality::Cumulative,
            unknown_temporality: None,
            label_uids: false,
        };
        let mut request = build_request(&sample, &HashMap::new(), &[], &settings, None);
        let before: Vec<usize> = request.resources.iter().map(|r| r.metrics.len()).collect();
        let points =
            RunCgroups::default().update(&sample, |id, _| (id == RUN).then(|| reading(1_500_000)));
        add_to_request(&mut request, &sample, &points, Temporality::Cumulative);
        let names = |i: usize| -> Vec<&str> {
            request.resources[i].metrics[before[i]..]
                .iter()
                .map(|m| m.name)
                .collect()
        };
        assert_eq!(names(0), [metrics::CGROUP_CPU_TIME, metrics::CGROUP_MEMORY]);
        assert!(
            names(1).is_empty(),
            "a run with no cgroup keeps only its own metrics"
        );
        let MetricData::Sum { points, .. } = &request.resources[0].metrics[before[0]].data else {
            panic!("cpu is a sum");
        };
        assert_eq!(points[0].value, NumberValue::Double(1.5));
        assert_eq!(points[0].start_unix_nanos, Some(5), "from the run's start");
        let MetricData::Gauge { points } = &request.resources[0].metrics[before[0] + 1].data else {
            panic!("memory is a gauge");
        };
        assert_eq!(
            points.len(),
            1,
            "no anon point when memory.stat was not read"
        );
        assert_eq!(
            points[0].attributes,
            [(keys::MEMORY_KIND.to_string(), "current".to_string())]
        );
    }
}
