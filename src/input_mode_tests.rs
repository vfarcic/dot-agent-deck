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
}

impl Scripted {
    fn new(reads: Vec<Step>) -> Self {
        Self {
            reads: reads.into(),
            written: Vec::new(),
            writes_fail: false,
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

/// Scenario: Carry a report on scripted connections to daemons the wrapper has not met: one answers `input_mode_report: 1`, others answer without the field, with a refusal, with a malformed line, or not at all while the connection stays open. Only the first gets the report, on the same connection after the probe; each of the others gets the probe alone and is remembered as unsupported, and a later connection to it carries nothing at all; the supported one's next connection carries the report with no probe.
#[spec("inbox/doorbell/052")]
#[test]
fn doorbell_052_the_probes_answer_decides_and_is_remembered() {
    let mut cache = VerdictCache::default();
    let mut conn = Scripted::answering(SUPPORTED);
    let (outcome, _) = exchange(&mut conn, id(1, 1), &mut cache, PROBE, REPORT);
    assert!(matches!(outcome, Exchange::Sent { probed: true, .. }));
    assert_eq!(conn.lines(), vec![PROBE, REPORT]);
    let mut conn = Scripted::new(vec![]);
    let (outcome, _) = exchange(&mut conn, id(1, 1), &mut cache, PROBE, REPORT);
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
        let (outcome, _) = exchange(&mut conn, id(pid, 1), &mut cache, PROBE, REPORT);
        assert_eq!(outcome, Exchange::Withheld { probed: true }, "daemon {pid}");
        assert_eq!(conn.lines(), vec![PROBE], "daemon {pid}");
        let mut again = Scripted::new(vec![]);
        let (outcome, _) = exchange(&mut again, id(pid, 1), &mut cache, PROBE, REPORT);
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
        let (outcome, _) = exchange(&mut conn, id(pid, 1), cache, PROBE, REPORT);
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
        let (outcome, _) = exchange(&mut conn, id(pid, 1), &mut cache, PROBE, REPORT);
        assert!(
            matches!(outcome, Exchange::Sent { probed: true, .. }),
            "daemon {pid} is probed again"
        );
    }
    let mut conn = Scripted::answering(SUPPORTED);
    let (outcome, _) = exchange(&mut conn, None, &mut cache, PROBE, REPORT);
    assert_eq!(outcome, Exchange::IdentityUnreadable);
    assert!(conn.written.is_empty());
}

/// Scenario: The wrapper meets daemon A, then B, then A again while A is still remembered, then a daemon with A's pid and a new start time, then enough others to fill its memory. A is not probed again on its return; A's pid with a new start time is a daemon of its own and is probed; the ninth identity evicts the least recently used one, which is probed again when it comes back.
#[spec("inbox/doorbell/052")]
#[test]
fn doorbell_052_verdicts_are_kept_per_identity() {
    let mut cache = VerdictCache::default();
    let probed = |cache: &mut VerdictCache, identity| {
        let mut conn = Scripted::answering(SUPPORTED);
        let (outcome, evicted) = exchange(&mut conn, identity, cache, PROBE, REPORT);
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
        Self { child, log }
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

/// Post a fresh raw sample to `sender` every 50 ms for `span`, numbering
/// from `*seq`.
fn feed(sender: &ModeSender, seq: &mut u64, span: Duration) {
    let until = Instant::now() + span;
    while Instant::now() < until {
        *seq += 1;
        let at = monotonic_ns().unwrap();
        sender.post(ModeSample {
            seq: *seq,
            icanon: false,
            echo: false,
            sampled_mono_ns: at,
            sample_done_mono_ns: at + 1,
        });
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Scenario: A wrapper's real sender posts a fresh sample every 50 ms to one socket path while the daemon behind it is replaced, each a separate process: A, which serves the reports; then B1, which answers the probe without the field; B2, which never answers; B3, which reads lines as a daemon from before the probe would; then C, which serves them. The wrapper keeps posting through every replacement, so a sample is waiting or in flight as each one happens. A and C each read exactly one probe and then a stream of reports; each B reads exactly one probe and no report, however many ticks pass, so A's verdict never reaches them and theirs never reaches C.
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
    let a = FakeDaemonProcess::start(dir.path(), &sock, "a", "supported");
    feed(&sender, &mut seq, Duration::from_millis(1200));
    let (probes, reports) = a.stop();
    assert_eq!(probes, 1, "A: one probe");
    assert!(reports >= 3, "A: reports flow ({reports})");
    for (name, answer) in [("b1", "no_field"), ("b2", "silent"), ("b3", "old")] {
        let b = FakeDaemonProcess::start(dir.path(), &sock, name, answer);
        feed(&sender, &mut seq, Duration::from_millis(1800));
        let (probes, reports) = b.stop();
        assert_eq!((probes, reports), (1, 0), "{name} ({answer})");
    }
    let c = FakeDaemonProcess::start(dir.path(), &sock, "c", "supported");
    feed(&sender, &mut seq, Duration::from_millis(1200));
    let (probes, reports) = c.stop();
    assert_eq!(probes, 1, "C: one probe for many connections");
    assert!(reports >= 3, "C: reports flow ({reports})");
    sender.close();
}
