//! PRD #1666 part 3, round 5-wake: the wrapper's side of the input-mode
//! evidence, `inbox/doorbell/054` (the producer) and `inbox/doorbell/052`'s
//! carrier half (the per-identity verdict and the exchange).

use super::*;
use spec::spec;
use std::collections::VecDeque;
use std::io::{BufRead, Read, Write};
use std::time::{Duration, Instant};

const MS: u64 = 1_000_000;

const WRAPPER: Observer = Observer {
    pid: 4242,
    start: 1_000,
};

/// A clock that returns `reads` in turn, then fails.
fn clock_of(reads: Vec<Option<u64>>) -> impl FnMut() -> Option<u64> {
    let mut reads = reads.into_iter();
    move || reads.next().flatten()
}

/// Scenario: The producer's clock reads t0, the inner PTY reports both bits clear, and the clock has moved on by more than the age bound when it is read again, as for a wrapper starved in the middle of its read. The producer discards the sample as a wide sample instead of reporting it, and the same observation offered to the daemon's store anyway is refused.
#[spec("inbox/doorbell/054")]
#[test]
fn doorbell_054_a_starved_read_is_discarded_and_would_be_refused() {
    let t0 = 10_000 * MS;
    let mut watch = InputModeWatch::default();
    let mut clock = clock_of(vec![Some(t0), Some(t0 + MODE_MAX_AGE_NS + MS)]);
    let Tick::WideSample(sample) = watch.tick(&mut clock, || Some((false, false))) else {
        panic!("a starved read must be discarded");
    };
    let mut store = InputModeStore::default();
    assert_eq!(
        store.accept(WRAPPER, sample, t0 + MODE_MAX_AGE_NS + 2 * MS),
        Err(Refusal::WideSample)
    );
    assert_eq!(store.latest(), None);
}

/// Scenario: The producer's read takes just under the age bound: it reports, and the daemon's store takes the report. Used one millisecond past the bound from the first stamp, the evidence is stale although the second stamp is only two milliseconds old; the settle anchor is the second stamp.
#[spec("inbox/doorbell/054")]
#[test]
fn doorbell_054_age_at_use_counts_from_the_stamp_before_the_read() {
    let t0 = 10_000 * MS;
    let done = t0 + MODE_MAX_AGE_NS - MS;
    let mut watch = InputModeWatch::default();
    let mut clock = clock_of(vec![Some(t0), Some(done)]);
    let Tick::Report(sample) = watch.tick(&mut clock, || Some((false, false))) else {
        panic!("a read inside the bound is reported");
    };
    assert_eq!(
        (sample.sampled_mono_ns, sample.sample_done_mono_ns),
        (t0, done)
    );
    let mut store = InputModeStore::default();
    store.accept(WRAPPER, sample, done + MS).unwrap();
    assert!(store.usable(t0 + MODE_MAX_AGE_NS).is_ok());
    assert_eq!(
        store.usable(t0 + MODE_MAX_AGE_NS + MS),
        Err(ModeClause::Stale)
    );
    assert_eq!(store.anchor(), Some(done), "the anchor is the later stamp");
}

/// Scenario: After one good report is stored, the producer ticks with the clock failing before the read, the `tcgetattr` failing, and the clock failing after the read. None of those ticks produces a report, and the stored evidence goes stale on its own stamp.
#[spec("inbox/doorbell/054")]
#[test]
fn doorbell_054_a_failed_read_reports_nothing_and_the_evidence_ages_out() {
    let t0 = 10_000 * MS;
    let mut watch = InputModeWatch::default();
    let mut store = InputModeStore::default();
    let mut clock = clock_of(vec![Some(t0), Some(t0 + MS)]);
    let Tick::Report(first) = watch.tick(&mut clock, || Some((false, false))) else {
        panic!("the first sample is reported");
    };
    store.accept(WRAPPER, first, t0 + 2 * MS).unwrap();
    let later = t0 + 2 * MODE_REASSERT.as_nanos() as u64;
    let mut failing = clock_of(vec![None]);
    assert_eq!(
        watch.tick(&mut failing, || Some((false, false))),
        Tick::ClockFailed
    );
    let mut clock = clock_of(vec![Some(later)]);
    assert_eq!(watch.tick(&mut clock, || None), Tick::TermiosFailed);
    let mut clock = clock_of(vec![Some(later), None]);
    assert_eq!(
        watch.tick(&mut clock, || Some((false, false))),
        Tick::ClockFailed
    );
    assert_eq!(store.latest(), Some(first));
    assert_eq!(
        store.usable(t0 + MODE_MAX_AGE_NS + MS),
        Err(ModeClause::Stale)
    );
}

/// Scenario: The producer ticks every 50 ms with the bits unchanged for over a second, then sees echo turn on. It reports the first sample, stays quiet until the re-assertion interval has passed and then reports again, and reports the change on the tick that sees it, with sequence numbers that only grow.
#[spec("inbox/doorbell/054")]
#[test]
fn doorbell_054_reports_on_change_and_on_reassertion() {
    let t0 = 10_000 * MS;
    let mut watch = InputModeWatch::default();
    let mut reports = Vec::new();
    for i in 0..30u64 {
        let at = t0 + i * 50 * MS;
        let echo = i >= 25;
        let mut clock = clock_of(vec![Some(at), Some(at + MS)]);
        if let Tick::Report(sample) = watch.tick(&mut clock, || Some((false, echo))) {
            reports.push((i, sample));
        }
    }
    let ticks: Vec<u64> = reports.iter().map(|(i, _)| *i).collect();
    assert_eq!(ticks, vec![0, 20, 25], "first, re-asserted at 1 s, changed");
    assert!(reports.windows(2).all(|w| w[0].1.seq < w[1].1.seq));
    assert!(reports[2].1.echo);
}

/// Scenario: Encode the wrapper's probe line and its `input_mode` report line, and decode each as an agent event, as a daemon that predates both would. Neither decodes, so an older daemon cannot take either for a status report.
#[spec("inbox/doorbell/052")]
#[test]
fn doorbell_052_neither_line_reads_as_an_agent_event() {
    let target = ReportTarget {
        socket: PathBuf::from("/nonexistent"),
        pane_id: "pane-x".to_string(),
        token: Some("tok".to_string()),
    };
    let sample = ModeSample {
        seq: 1,
        icanon: false,
        echo: false,
        sampled_mono_ns: 1,
        sample_done_mono_ns: 2,
    };
    for line in [target.probe_line(), target.report_line(&sample)] {
        assert!(
            serde_json::from_str::<crate::event::AgentEvent>(&line).is_err(),
            "{line}"
        );
        assert!(serde_json::from_str::<crate::event::DaemonMessage>(&line).is_ok());
    }
}

/// One step a [`Scripted`] connection's reads take.
enum Step {
    Data(Vec<u8>),
    Eof,
    Fail(std::io::ErrorKind),
}

/// A connection whose reads follow a script: once it runs out, every read
/// times out, as a peer that keeps the connection open and says nothing.
struct Scripted {
    reads: VecDeque<Step>,
    written: Vec<u8>,
    writes_fail: bool,
    bound_fails: bool,
    /// How many bounds succeed before every later one fails, when set.
    bounds_left: Option<usize>,
}

impl Scripted {
    fn new(reads: Vec<Step>) -> Self {
        Self {
            reads: reads.into(),
            written: Vec::new(),
            writes_fail: false,
            bound_fails: false,
            bounds_left: None,
        }
    }

    fn answering(line: &str) -> Self {
        Self::new(vec![Step::Data(format!("{line}\n").into_bytes())])
    }

    fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.written)
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Read for Scripted {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.reads.front_mut() {
            None => Err(std::io::ErrorKind::WouldBlock.into()),
            Some(Step::Eof) => Ok(0),
            Some(Step::Data(data)) => {
                let n = buf.len().min(data.len());
                buf[..n].copy_from_slice(&data[..n]);
                data.drain(..n);
                if data.is_empty() {
                    self.reads.pop_front();
                }
                Ok(n)
            }
            Some(Step::Fail(kind)) => {
                let kind = *kind;
                self.reads.pop_front();
                Err(kind.into())
            }
        }
    }
}

impl BoundedConn for Scripted {
    fn bound(&mut self, _left: Duration) -> std::io::Result<()> {
        if self.bound_fails {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        match &mut self.bounds_left {
            Some(0) => Err(std::io::ErrorKind::InvalidInput.into()),
            Some(left) => {
                *left -= 1;
                Ok(())
            }
            None => Ok(()),
        }
    }
}

impl Write for Scripted {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.writes_fail {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

const PROBE: &str = r#"{"message_type":"hook_capabilities","pane_id":"p"}"#;
const REPORT: &str = r#"{"message_type":"input_mode","pane_id":"p"}"#;
const SUPPORTED: &str = r#"{"input_mode_report":1}"#;

fn id(pid: u32, start: u64) -> Option<DaemonIdentity> {
    Some(DaemonIdentity { pid, start })
}

/// A whole send's deadline from now.
fn soon() -> Instant {
    Instant::now() + MODE_SEND_TIMEOUT
}

/// Scenario: Carry a report on scripted connections to daemons the wrapper has not met: one answers `input_mode_report: 1`, others answer without the field, with a refusal, with a malformed line, or not at all while the connection stays open. Only the first gets the report, on the same connection after the probe; each of the others gets the probe alone and is remembered as unsupported, and a later connection to it carries nothing at all; the supported one's next connection carries the report with no probe.
#[spec("inbox/doorbell/052")]
#[test]
fn doorbell_052_the_probes_answer_decides_and_is_remembered() {
    let mut cache = VerdictCache::default();
    let mut conn = Scripted::answering(SUPPORTED);
    let (outcome, _) = exchange(&mut conn, id(1, 1), &mut cache, PROBE, REPORT, soon());
    assert!(matches!(outcome, Exchange::Sent { probed: true, .. }));
    assert_eq!(conn.lines(), vec![PROBE, REPORT]);
    let mut conn = Scripted::new(vec![]);
    let (outcome, _) = exchange(&mut conn, id(1, 1), &mut cache, PROBE, REPORT, soon());
    assert!(matches!(outcome, Exchange::Sent { probed: false, .. }));
    assert_eq!(conn.lines(), vec![REPORT], "no second probe");

    for (pid, answer) in [
        (2, Some(r#"{"task_scoped_completion":1}"#)),
        (
            3,
            Some(r#"{"input_mode_report":1,"code":"unknown_token","message":"no"}"#),
        ),
        (4, Some("not json")),
        (5, Some(r#"{"input_mode_report":2}"#)),
        (6, None),
    ] {
        let mut conn = match answer {
            Some(line) => Scripted::answering(line),
            None => Scripted::new(vec![]),
        };
        let (outcome, _) = exchange(&mut conn, id(pid, 1), &mut cache, PROBE, REPORT, soon());
        assert_eq!(outcome, Exchange::Withheld { probed: true }, "daemon {pid}");
        assert_eq!(conn.lines(), vec![PROBE], "daemon {pid}");
        let mut again = Scripted::new(vec![]);
        let (outcome, _) = exchange(&mut again, id(pid, 1), &mut cache, PROBE, REPORT, soon());
        assert_eq!(outcome, Exchange::Withheld { probed: false });
        assert!(again.written.is_empty(), "daemon {pid}: nothing written");
    }
}

/// Scenario: A probe's connection ends before an answer, by end of file, by a reset, and by a failed write; and a connection's daemon cannot be identified. None of those records a verdict, so the next connection to each daemon probes again; the unidentified one writes nothing at all.
#[spec("inbox/doorbell/052")]
#[test]
fn doorbell_052_an_ended_connection_records_no_verdict() {
    let mut cache = VerdictCache::default();
    let ended = |cache: &mut VerdictCache, mut conn: Scripted, pid| {
        let (outcome, _) = exchange(&mut conn, id(pid, 1), cache, PROBE, REPORT, soon());
        outcome
    };
    assert_eq!(
        ended(&mut cache, Scripted::new(vec![Step::Eof]), 1),
        Exchange::Ended { why: "eof" }
    );
    assert_eq!(
        ended(
            &mut cache,
            Scripted::new(vec![Step::Fail(std::io::ErrorKind::ConnectionReset)]),
            2
        ),
        Exchange::Ended { why: "reset" }
    );
    let mut failing = Scripted::new(vec![]);
    failing.writes_fail = true;
    assert_eq!(
        ended(&mut cache, failing, 3),
        Exchange::Ended {
            why: "probe_write_failed"
        }
    );
    assert!(cache.is_empty(), "no verdict recorded");
    for pid in 1..=3 {
        let mut conn = Scripted::answering(SUPPORTED);
        let (outcome, _) = exchange(&mut conn, id(pid, 1), &mut cache, PROBE, REPORT, soon());
        assert!(
            matches!(outcome, Exchange::Sent { probed: true, .. }),
            "daemon {pid} is probed again"
        );
    }
    let mut conn = Scripted::answering(SUPPORTED);
    let (outcome, _) = exchange(&mut conn, None, &mut cache, PROBE, REPORT, soon());
    assert_eq!(outcome, Exchange::IdentityUnreadable);
    assert!(conn.written.is_empty());
}

/// Scenario: A connection whose reads and writes cannot be bounded, a probe answer cut off by the deadline after its first bytes, and silence after a probe written with less than the silence window of the deadline left. None of them records a verdict, and the unbounded one writes nothing; silence with the whole window left records Unsupported (the control).
#[spec("inbox/doorbell/052")]
#[test]
fn doorbell_052_a_cut_off_or_unbounded_exchange_records_no_verdict() {
    let mut cache = VerdictCache::default();
    let mut unbounded = Scripted::answering(SUPPORTED);
    unbounded.bound_fails = true;
    let (outcome, _) = exchange(&mut unbounded, id(1, 1), &mut cache, PROBE, REPORT, soon());
    assert_eq!(outcome, Exchange::Unbounded);
    assert!(unbounded.written.is_empty());

    let mut cut = Scripted::new(vec![Step::Data(br#"{"input_mode_rep"#.to_vec())]);
    let (outcome, _) = exchange(&mut cut, id(2, 1), &mut cache, PROBE, REPORT, soon());
    assert_eq!(outcome, Exchange::Ended { why: "deadline" });
    assert_eq!(cut.lines(), vec![PROBE], "the probe alone");

    let late = Instant::now() + MODE_PROBE_SILENCE / 2;
    let mut silent = Scripted::new(vec![]);
    let (outcome, _) = exchange(&mut silent, id(3, 1), &mut cache, PROBE, REPORT, late);
    assert_eq!(outcome, Exchange::Ended { why: "deadline" });
    assert!(cache.is_empty(), "no verdict recorded");

    let mut silent = Scripted::new(vec![]);
    let (outcome, _) = exchange(&mut silent, id(3, 1), &mut cache, PROBE, REPORT, soon());
    assert_eq!(outcome, Exchange::Withheld { probed: true });
}

/// Scenario: A daemon answers the probe `input_mode_report: 1`, and then the connection's bound fails: once before the report's write, once before the acknowledgement's read. The first writes the probe alone and ends unbounded, the second writes the probe and the report and reads no acknowledgement; both keep the Supported verdict the answer recorded, so the daemon's next connection carries the report with no probe.
#[spec("inbox/doorbell/052")]
#[test]
fn doorbell_052_a_bound_failing_after_the_answer_keeps_the_verdict() {
    // The probe's write takes one bound and its answer one per byte read.
    let answered = 1 + SUPPORTED.len() + 1;
    for (pid, bounds, outcome, lines) in [
        (1, answered, Exchange::Unbounded, vec![PROBE]),
        (
            2,
            answered + 1,
            Exchange::Sent {
                probed: true,
                ack: None,
            },
            vec![PROBE, REPORT],
        ),
    ] {
        let mut cache = VerdictCache::default();
        let mut conn = Scripted::new(vec![
            Step::Data(format!("{SUPPORTED}\n").into_bytes()),
            Step::Data(b"{}\n".to_vec()),
        ]);
        conn.bounds_left = Some(bounds);
        let (got, _) = exchange(&mut conn, id(pid, 1), &mut cache, PROBE, REPORT, soon());
        assert_eq!(got, outcome, "daemon {pid}");
        assert_eq!(conn.lines(), lines, "daemon {pid}");
        assert_eq!(
            cache.get(DaemonIdentity { pid, start: 1 }),
            Some(Verdict::Supported),
            "daemon {pid}"
        );
        let mut next = Scripted::new(vec![]);
        let (got, _) = exchange(&mut next, id(pid, 1), &mut cache, PROBE, REPORT, soon());
        assert!(
            matches!(got, Exchange::Sent { probed: false, .. }),
            "daemon {pid}: {got:?}"
        );
        assert_eq!(next.lines(), vec![REPORT], "daemon {pid}");
    }
    // A bound that fails on the cached path writes nothing and keeps it too.
    let mut cache = VerdictCache::default();
    cache.record(DaemonIdentity { pid: 3, start: 1 }, Verdict::Supported);
    let mut conn = Scripted::new(vec![]);
    conn.bound_fails = true;
    let (got, _) = exchange(&mut conn, id(3, 1), &mut cache, PROBE, REPORT, soon());
    assert_eq!(got, Exchange::Unbounded);
    assert!(conn.written.is_empty());
    assert_eq!(
        cache.get(DaemonIdentity { pid: 3, start: 1 }),
        Some(Verdict::Supported)
    );
}

/// Scenario: The wrapper meets daemon A, then B, then A again while A is still remembered, then a daemon with A's pid and a new start time, then enough others to fill its memory. A is not probed again on its return; A's pid with a new start time is a daemon of its own and is probed; the ninth identity evicts the least recently used one, which is probed again when it comes back.
#[spec("inbox/doorbell/052")]
#[test]
fn doorbell_052_verdicts_are_kept_per_identity() {
    let mut cache = VerdictCache::default();
    let probed = |cache: &mut VerdictCache, identity| {
        let mut conn = Scripted::answering(SUPPORTED);
        let (outcome, evicted) = exchange(&mut conn, identity, cache, PROBE, REPORT, soon());
        let Exchange::Sent { probed, .. } = outcome else {
            panic!("{outcome:?}");
        };
        (probed, evicted)
    };
    assert!(probed(&mut cache, id(10, 1)).0, "A");
    assert!(probed(&mut cache, id(11, 1)).0, "B");
    assert!(!probed(&mut cache, id(10, 1)).0, "A again, still cached");
    assert!(probed(&mut cache, id(10, 2)).0, "A's pid, a new start");
    // Cached now, least recently used first: B, A, A'. Fill to eight.
    for pid in 20..25 {
        assert!(probed(&mut cache, id(pid, 1)).0);
    }
    assert_eq!(cache.len(), VERDICT_CACHE_LEN);
    let (was_probed, evicted) = probed(&mut cache, id(30, 1));
    assert!(was_probed);
    assert_eq!(evicted, id(11, 1), "B was the least recently used");
    assert!(probed(&mut cache, id(11, 1)).0, "B is probed on its return");
    assert!(!probed(&mut cache, id(30, 1)).0);
}

const FAKE_DAEMON: &str = "DAD_TEST_FAKE_INPUT_MODE_DAEMON";
const FAKE_DAEMON_TEST: &str = "input_mode::tests::fake_daemon_process";

/// A fake daemon for 052: where it listens, where it logs every line it
/// reads, and how it answers (`supported`, `no_field`, `silent`, `old`).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct FakeDaemon {
    sock: PathBuf,
    log: PathBuf,
    answer: String,
}

/// Not a check of its own: without [`FAKE_DAEMON`] it returns at once. 052
/// starts this test binary with it set to run a fake daemon in a process of
/// its own, so that each fake daemon has a real identity.
#[test]
fn fake_daemon_process() {
    let Ok(spec) = std::env::var(FAKE_DAEMON) else {
        return;
    };
    let spec: FakeDaemon = serde_json::from_str(&spec).expect("the fake daemon's spec");
    let listener = std::os::unix::net::UnixListener::bind(&spec.sock).expect("bind");
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let spec = spec.clone();
        std::thread::spawn(move || {
            let mut writer = conn.try_clone().expect("clone");
            for line in std::io::BufReader::new(conn).lines() {
                let Ok(line) = line else { return };
                let mut log = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&spec.log)
                    .expect("the log");
                let _ = log.write_all(format!("{line}\n").as_bytes());
                let kind = serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .and_then(|v| v["message_type"].as_str().map(str::to_string));
                let answer = match (spec.answer.as_str(), kind.as_deref()) {
                    ("supported", Some("hook_capabilities")) => Some(SUPPORTED.to_string()),
                    ("supported", Some("input_mode")) => {
                        serde_json::to_string(&crate::event::SignalAck::accepted()).ok()
                    }
                    ("no_field", Some("hook_capabilities")) => {
                        Some(r#"{"task_scoped_completion":1}"#.to_string())
                    }
                    // `silent` answers nothing; `old`, like a daemon from
                    // before the probe, logs the line as malformed and reads
                    // on.
                    _ => None,
                };
                if let Some(answer) = answer {
                    let _ = writer.write_all(format!("{answer}\n").as_bytes());
                }
            }
        });
    }
}

/// A fake daemon process, killed by its own handle when dropped.
struct FakeDaemonProcess {
    child: std::process::Child,
    log: PathBuf,
    name: String,
    answer: String,
}

impl FakeDaemonProcess {
    fn start(dir: &std::path::Path, sock: &std::path::Path, name: &str, answer: &str) -> Self {
        let _ = std::fs::remove_file(sock);
        let log = dir.join(format!("{name}.log"));
        let spec = FakeDaemon {
            sock: sock.to_path_buf(),
            log: log.clone(),
            answer: answer.to_string(),
        };
        let child = std::process::Command::new(std::env::current_exe().expect("the test binary"))
            .args([
                "--exact",
                FAKE_DAEMON_TEST,
                "--nocapture",
                "--test-threads=1",
            ])
            .env(FAKE_DAEMON, serde_json::to_string(&spec).unwrap())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start a fake daemon");
        let deadline = Instant::now() + Duration::from_secs(20);
        // Bound, by its own process: a socket file another process left there
        // was removed above.
        while !sock.exists() {
            assert!(
                Instant::now() < deadline,
                "fake daemon {name} never listened"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        Self {
            child,
            log,
            name: name.to_string(),
            answer: answer.to_string(),
        }
    }

    /// How many probe lines and report lines it has read.
    fn lines(&self) -> (usize, usize) {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        let count = |kind: &str| {
            text.lines()
                .filter(|line| line.contains(&format!(r#""message_type":"{kind}""#)))
                .count()
        };
        (count("hook_capabilities"), count("input_mode"))
    }

    fn stop(mut self) -> (usize, usize) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.lines()
    }
}

impl Drop for FakeDaemonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A fresh raw sample numbered `seq`.
fn fresh(seq: u64) -> ModeSample {
    let at = monotonic_ns().unwrap();
    ModeSample {
        seq,
        icanon: false,
        echo: false,
        // The done stamp is the clock read itself: a later read can equal it
        // (macOS's monotonic clock ticks in microseconds), never precede it.
        sampled_mono_ns: at.saturating_sub(1),
        sample_done_mono_ns: at,
    }
}

/// Post a fresh raw sample to `sender` every 50 ms for `span`, numbering
/// from `*seq`.
fn feed(sender: &ModeSender, seq: &mut u64, span: Duration) {
    let until = Instant::now() + span;
    while Instant::now() < until {
        *seq += 1;
        sender.post(fresh(*seq));
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl FakeDaemonProcess {
    /// Whether it has read the report numbered `seq`.
    fn reported(&self, seq: u64) -> bool {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .any(|line| {
                line.contains(r#""message_type":"input_mode""#)
                    && line.contains(&format!(r#""seq":{seq},"#))
            })
    }
}

/// Scenario: A wrapper's real sender posts fresh samples to one socket path while the daemon behind it is replaced, each a separate process: A, which serves the reports; then B1, which answers the probe without the field; B2, which never answers; B3, which reads lines as a daemon from before the probe would; then C, which serves them. At each replacement the sender is held at its turn with one report in hand and a newer one waiting in its slot, the old daemon is stopped, the new one started, and then the sender is let go. A and C each read exactly one probe and then a stream of reports, C's including the one held across its replacement; each B reads exactly one probe and no report, the held one included, however many ticks pass, so A's verdict never reaches them and theirs never reaches C.
#[spec("inbox/doorbell/052")]
#[test]
fn doorbell_052_a_replaced_daemon_is_probed_afresh() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("hook.sock");
    let sender = ModeSender::spawn(
        ReportTarget {
            socket: sock.clone(),
            pane_id: "pane-carrier".to_string(),
            token: Some("token".to_string()),
        },
        IdentityLookups::PLATFORM,
    );
    let mut seq = 0;
    let mut current = FakeDaemonProcess::start(dir.path(), &sock, "a", "supported");
    feed(&sender, &mut seq, Duration::from_millis(1200));
    for (name, answer) in [
        ("b1", "no_field"),
        ("b2", "silent"),
        ("b3", "old"),
        ("c", "supported"),
    ] {
        let (at, go) = sender.hold_next_turn();
        seq += 1;
        sender.post(fresh(seq));
        let held = at
            .recv_timeout(Duration::from_secs(10))
            .expect("the sender reaches its turn");
        seq += 1;
        sender.post(fresh(seq));
        let previous_name = current.name.clone();
        let previous_answer = current.answer.clone();
        let (probes, reports) = current.stop();
        match previous_answer.as_str() {
            "supported" => {
                assert_eq!(probes, 1, "{previous_name}: one probe");
                assert!(reports >= 3, "{previous_name}: reports flow ({reports})");
            }
            _ => assert_eq!(
                (probes, reports),
                (1, 0),
                "{previous_name} ({previous_answer})"
            ),
        }
        current = FakeDaemonProcess::start(dir.path(), &sock, name, answer);
        go.send(()).expect("let the sender go");
        feed(&sender, &mut seq, Duration::from_millis(1800));
        if answer == "supported" {
            assert!(
                current.reported(held),
                "{name}: the report held across the replacement"
            );
        } else {
            assert!(!current.reported(held), "{name}: the held report");
        }
    }
    let (probes, reports) = current.stop();
    assert_eq!(probes, 1, "C: one probe for many connections");
    assert!(reports >= 3, "C: reports flow ({reports})");
    sender.close();
}

/// Bind a listener at `path` that runs `serve` on a thread per connection.
fn endpoint(
    path: &std::path::Path,
    serve: impl Fn(std::os::unix::net::UnixStream) + Send + Sync + 'static,
) {
    let listener = std::os::unix::net::UnixListener::bind(path).expect("bind");
    let serve = std::sync::Arc::new(serve);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { return };
            let serve = std::sync::Arc::clone(&serve);
            std::thread::spawn(move || serve(conn));
        }
    });
}

/// Read one line from `conn`, byte by byte.
fn read_one_line(conn: &mut std::os::unix::net::UnixStream) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match conn.read(&mut byte) {
            Ok(0) | Err(_) => return None,
            Ok(_) if byte[0] == b'\n' => return Some(String::from_utf8_lossy(&line).into()),
            Ok(_) => line.push(byte[0]),
        }
    }
}

/// Write `text` to `conn` one byte every `every`, stopping at a failed write.
fn drip(conn: &mut std::os::unix::net::UnixStream, text: &str, every: Duration) {
    for byte in text.bytes() {
        std::thread::sleep(every);
        if conn.write_all(&[byte]).is_err() {
            return;
        }
    }
}

fn target(socket: &std::path::Path) -> ReportTarget {
    ReportTarget {
        socket: socket.to_path_buf(),
        pane_id: "pane-bounded".to_string(),
        token: Some("token".to_string()),
    }
}

/// What [`ReportTarget::send`] returns.
type SendResult = Option<(Exchange, Option<DaemonIdentity>, Option<DaemonIdentity>)>;

/// `target.send` on its own thread: how long it took and what it returned,
/// or `None` if it had not returned after ten seconds.
fn timed_send(target: ReportTarget, lookups: IdentityLookups) -> Option<(Duration, SendResult)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        let mut cache = VerdictCache::default();
        let sent = target.send(&fresh(1), &mut cache, &lookups);
        let _ = tx.send((started.elapsed(), sent));
    });
    rx.recv_timeout(Duration::from_secs(10)).ok()
}

/// How long past [`MODE_SEND_TIMEOUT`] a bounded send may take here: the
/// scheduling of a loaded test machine, not the protocol.
const SLACK: Duration = Duration::from_millis(700);

/// Scenario: Send a report to a listener whose accept queue is full and that never accepts, to a daemon that drips its probe answer one byte every 100 ms, and to one that answers the probe at once and drips its acknowledgement. Each send returns within the send timeout (plus scheduling slack), instead of parking in the connect or reading a byte at a time for as long as the peer keeps dripping: no daemon for the full queue, no verdict for the cut-off answer, and the report sent with no acknowledgement for the dripped one.
#[spec("inbox/doorbell/052")]
#[test]
fn doorbell_052_one_deadline_bounds_the_whole_send() {
    let saturated = crate::platform::ipc::SaturatedListener::new();
    let (took, sent) = timed_send(target(&saturated.path), IdentityLookups::PLATFORM)
        .expect("the send parked in connect");
    assert!(sent.is_none(), "{sent:?}");
    assert!(took < MODE_SEND_TIMEOUT + SLACK, "{took:?}");

    let dir = tempfile::tempdir().unwrap();
    let answer = dir.path().join("answer.sock");
    endpoint(&answer, |mut conn| {
        let _ = read_one_line(&mut conn);
        drip(
            &mut conn,
            &format!("{SUPPORTED}\n"),
            Duration::from_millis(100),
        );
    });
    let (took, sent) =
        timed_send(target(&answer), IdentityLookups::PLATFORM).expect("the send never returned");
    let (outcome, identity, _) = sent.expect("connected");
    assert!(identity.is_some());
    assert_eq!(outcome, Exchange::Ended { why: "deadline" });
    assert!(took < MODE_SEND_TIMEOUT + SLACK, "{took:?}");

    let ack = dir.path().join("ack.sock");
    endpoint(&ack, |mut conn| {
        let _ = read_one_line(&mut conn);
        let _ = conn.write_all(format!("{SUPPORTED}\n").as_bytes());
        let _ = read_one_line(&mut conn);
        drip(
            &mut conn,
            &format!(
                "{}\n",
                serde_json::to_string(&crate::event::SignalAck::accepted()).unwrap()
            ),
            Duration::from_millis(100),
        );
    });
    let (took, sent) =
        timed_send(target(&ack), IdentityLookups::PLATFORM).expect("the send never returned");
    let (outcome, _, _) = sent.expect("connected");
    assert_eq!(
        outcome,
        Exchange::Sent {
            probed: true,
            ack: None
        }
    );
    assert!(took < MODE_SEND_TIMEOUT + SLACK, "{took:?}");
}

/// The platform connect after a pause that leaves less than
/// [`MODE_PROBE_SILENCE`] of the send's deadline.
fn slow_connect(
    path: &std::path::Path,
    budget: Duration,
) -> std::io::Result<std::os::unix::net::UnixStream> {
    let pause = MODE_SEND_TIMEOUT - MODE_PROBE_SILENCE + Duration::from_millis(100);
    std::thread::sleep(pause);
    (IdentityLookups::PLATFORM.connect)(path, budget.saturating_sub(pause))
}

/// Scenario: A daemon reads each probe and stays silent with the connection open; slow connects leave less than the silence window, and a delayed identity lookup also exhausts the deadline before the probe can be written. Both paths end within the send timeout (plus scheduling slack), record no verdict and try again, with completed probes reaching the daemon; a send with the whole silence window records Unsupported (the control). A daemon that serves reports but answers its probe only after the deadline is also recorded Unsupported: the window prevents judging too early, not misjudging a slow live daemon.
#[spec("inbox/doorbell/052")]
#[test]
fn doorbell_052_silence_after_a_slow_connect_is_asked_again() {
    let dir = tempfile::tempdir().unwrap();
    let silent = dir.path().join("silent.sock");
    let probes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = std::sync::Arc::clone(&probes);
    endpoint(&silent, move |mut conn| {
        if read_one_line(&mut conn).is_some() {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        // Silent, the connection open until the wrapper closes it.
        let _ = read_one_line(&mut conn);
    });
    let slow = IdentityLookups {
        connect: slow_connect,
        ..IdentityLookups::PLATFORM
    };
    let mut cache = VerdictCache::default();
    let mut completed_probes = 0;
    for round in 1..=2 {
        let started = Instant::now();
        let sent = target(&silent).send(&fresh(round), &mut cache, &slow);
        let took = started.elapsed();
        let (outcome, identity, _) = sent.expect("connected");
        assert!(identity.is_some());
        // Only 150 ms remains after slow_connect. On a loaded runner it
        // can expire during identification or before the probe's write;
        // exchange calls that probe_write_failed even without a socket error.
        match outcome {
            Exchange::Ended { why: "deadline" } => completed_probes += 1,
            Exchange::Ended {
                why: "probe_write_failed",
            } => {}
            other => panic!("round {round}: {other:?}"),
        }
        assert!(took < MODE_SEND_TIMEOUT + SLACK, "round {round}: {took:?}");
        assert!(cache.is_empty(), "round {round}: no verdict recorded");
    }

    // Force that scheduling case on every platform. The peer stays open,
    // isolating deadline exhaustion from a broken pipe or reset.
    let starved = IdentityLookups {
        connect: slow_connect,
        started: |pid| {
            std::thread::sleep(MODE_PROBE_SILENCE);
            (IdentityLookups::PLATFORM.started)(pid)
        },
        ..IdentityLookups::PLATFORM
    };
    for round in 3..=4 {
        let started = Instant::now();
        let (outcome, identity, _) = target(&silent)
            .send(&fresh(round), &mut cache, &starved)
            .expect("connected");
        assert!(identity.is_some());
        assert_eq!(
            outcome,
            Exchange::Ended {
                why: "probe_write_failed"
            },
            "round {round}"
        );
        assert!(started.elapsed() < MODE_SEND_TIMEOUT + SLACK);
        assert!(cache.is_empty(), "round {round}: no verdict recorded");
    }

    let sent = target(&silent).send(&fresh(5), &mut cache, &IdentityLookups::PLATFORM);
    let (outcome, _, _) = sent.expect("connected");
    assert_eq!(outcome, Exchange::Withheld { probed: true });
    completed_probes += 1;
    let deadline = Instant::now() + Duration::from_secs(5);
    while probes.load(std::sync::atomic::Ordering::SeqCst) < completed_probes
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        probes.load(std::sync::atomic::Ordering::SeqCst) >= completed_probes,
        "every completed probe reached the peer"
    );

    let late = dir.path().join("late.sock");
    endpoint(&late, |mut conn| {
        let _ = read_one_line(&mut conn);
        std::thread::sleep(MODE_SEND_TIMEOUT + Duration::from_millis(200));
        let _ = conn.write_all(format!("{SUPPORTED}\n").as_bytes());
    });
    let mut cache = VerdictCache::default();
    let sent = target(&late).send(&fresh(6), &mut cache, &IdentityLookups::PLATFORM);
    let (outcome, identity, _) = sent.expect("connected");
    assert_eq!(outcome, Exchange::Withheld { probed: true });
    assert_eq!(
        cache.get(identity.expect("identified")),
        Some(Verdict::Unsupported),
        "a slow live daemon is recorded Unsupported"
    );
}

/// Scenario: A listener records every byte it reads. A send whose peer credentials read as another user writes nothing to it, not even the probe that carries the pane's token; the same send with the peer read as this user writes the probe (the control).
#[spec("inbox/doorbell/052")]
#[test]
fn doorbell_052_a_foreign_peer_gets_no_byte() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hook.sock");
    let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    endpoint(&path, {
        let received = std::sync::Arc::clone(&received);
        move |mut conn| {
            let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 4096];
            while let Ok(n) = conn.read(&mut buf) {
                if n == 0 {
                    return;
                }
                received.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        }
    });
    let foreign = IdentityLookups {
        peer_uid: |_| Some(crate::platform::paths::current_uid().wrapping_add(1)),
        ..IdentityLookups::PLATFORM
    };
    let (_, sent) = timed_send(target(&path), foreign).expect("the send never returned");
    assert_eq!(
        sent.map(|(outcome, _, _)| outcome),
        Some(Exchange::ForeignPeer)
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        received.lock().unwrap().is_empty(),
        "bytes reached a foreign peer: {:?}",
        String::from_utf8_lossy(&received.lock().unwrap())
    );
    let unreadable = IdentityLookups {
        peer_uid: |_| None,
        ..IdentityLookups::PLATFORM
    };
    let (_, sent) = timed_send(target(&path), unreadable).expect("the send never returned");
    assert_eq!(
        sent.map(|(outcome, _, _)| outcome),
        Some(Exchange::ForeignPeer)
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(received.lock().unwrap().is_empty());

    let _ = timed_send(target(&path), IdentityLookups::PLATFORM).expect("the send never returned");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !String::from_utf8_lossy(&received.lock().unwrap()).contains("hook_capabilities") {
        assert!(
            Instant::now() < deadline,
            "the control's probe never arrived"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Scenario: A wrapper's real sender reports to a daemon that answers its probe and then drips every acknowledgement a byte every 100 ms; then that daemon's socket is replaced by one that acknowledges at once. Reports reach the replacement within a few send timeouts, so no dripping connection holds the sender. Closed while it waits on a dripped acknowledgement, the sender's thread ends within one send timeout.
#[spec("inbox/doorbell/052")]
#[test]
fn doorbell_052_a_dripping_daemon_does_not_hold_the_sender() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("hook.sock");
    let ack_line = format!(
        "{}\n",
        serde_json::to_string(&crate::event::SignalAck::accepted()).unwrap()
    );
    let dripping = |path: &std::path::Path| {
        let reports = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        endpoint(path, {
            let ack_line = ack_line.clone();
            let reports = std::sync::Arc::clone(&reports);
            move |mut conn| {
                while let Some(line) = read_one_line(&mut conn) {
                    if line.contains("hook_capabilities") {
                        let _ = conn.write_all(format!("{SUPPORTED}\n").as_bytes());
                    } else {
                        reports.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        drip(&mut conn, &ack_line, Duration::from_millis(100));
                    }
                }
            }
        });
        reports
    };
    let reports_at_a = dripping(&sock);
    let sender = ModeSender::spawn(target(&sock), IdentityLookups::PLATFORM);
    let mut seq = 0;
    feed(&sender, &mut seq, Duration::from_millis(1500));
    assert!(reports_at_a.load(std::sync::atomic::Ordering::SeqCst) >= 1);

    std::fs::remove_file(&sock).unwrap();
    let reports_at_b = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    endpoint(&sock, {
        let reports_at_b = std::sync::Arc::clone(&reports_at_b);
        let ack_line = ack_line.clone();
        move |mut conn| {
            while let Some(line) = read_one_line(&mut conn) {
                if line.contains("hook_capabilities") {
                    let _ = conn.write_all(format!("{SUPPORTED}\n").as_bytes());
                } else {
                    reports_at_b.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = conn.write_all(ack_line.as_bytes());
                }
            }
        }
    });
    feed(&sender, &mut seq, 4 * MODE_SEND_TIMEOUT);
    assert!(
        reports_at_b.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "no report reached the replacement"
    );
    sender.close();

    let drip_sock = dir.path().join("drip.sock");
    let reports = dripping(&drip_sock);
    let sender = ModeSender::spawn(target(&drip_sock), IdentityLookups::PLATFORM);
    seq += 1;
    sender.post(fresh(seq));
    let deadline = Instant::now() + Duration::from_secs(5);
    while reports.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the report never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    sender.close();
    let deadline = Instant::now() + MODE_SEND_TIMEOUT + SLACK;
    while !sender.ended() {
        assert!(
            Instant::now() < deadline,
            "the sender's thread outlived its send"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
