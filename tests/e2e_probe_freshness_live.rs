#![cfg(all(feature = "e2e", feature = "e2e-live", target_os = "linux"))]

//! A measurement, not a contract: how often the writer refuses a probe's
//! screen on a real agent idle at its composer (PRD #1666 part 3, A-20d-7).
//!
//! The writer refuses the screen when output was taken, or the layout moved,
//! between the frame read when the probe was scheduled and the writer's own
//! freshness read just before the arm section (`screen_fresh` in
//! `src/agent_pty_segments.rs`). That refusal reaches the requester as
//! `Admission(Surface)`: the screen passed when scheduled, so at the writer
//! only freshness can have taken it away. A refusal when scheduled has no
//! `Admission(` wrapper and is counted apart.
//!
//! The test runs a recorded Claude Code release (2.1.312 unless
//! `DAD_PROBE_FRESHNESS_RELEASE` names another), in the harness'
//! own HOME, never the host's config, under the `e2e` build's editor-probe assumption,
//! and asks for probes through `e2e-composer-control`'s `probe-request`, the
//! live tick's own scheduling, writer and arm path. Nothing is submitted, so
//! no model turn starts: a probe's Ctrl+G opens the deck's helper, which
//! reads the empty box and returns. Between probes the test waits for the
//! probe's own redraws to stop, then a random 0.5–2.5 s, so a probe lands at
//! a uniformly chosen phase of whatever the idle agent redraws.
//!
//! Conditions, each `PROBES_*` probes: the interval as production has it,
//! then widened with the `e2e` build's `scheduled` barrier, which holds the
//! writer thread after the frame was read and before the freshness read, so
//! the rate is read against a known window too. The idle output cadence is
//! recorded for a minute before any probe, from the pane's attach stream.
//!
//! A last condition is the control: the pane is resized as each hold starts,
//! so every probe must be refused for the screen. Without it a zero would
//! not show that the measurement can see a refusal.
//!
//! The executable is `DAD_PROBE_FRESHNESS_BINARY` when set, else the host's
//! `~/.local/share/claude/versions/<release>`.
//!
//! The Codex twin (U4, after the Q35-22 binding) runs the pinned native
//! Codex 0.164.0 the same way, with no credential: a dummy key and a
//! provider on a closed loopback port, which still draws its composer
//! (nothing is submitted, so nothing asks the provider for a turn).
//!
//! `DAD_PROBE_FRESHNESS_OUT` names a file the summary is appended to as one
//! JSON line; the summary is printed either way.

mod common;

use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP, KIND_STREAM_OUT};
use serde_json::{Value, json};
use spec::spec;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PANE: &str = "probe-freshness";
/// The release measured when `DAD_PROBE_FRESHNESS_RELEASE` names none. The
/// `e2e` build treats it as probe-capable, which it accepts only for a
/// release with a record of its own.
const DEFAULT_RELEASE: &str = "2.1.312";

fn release() -> String {
    std::env::var("DAD_PROBE_FRESHNESS_RELEASE").unwrap_or_else(|_| DEFAULT_RELEASE.to_string())
}
/// The `scheduled` barrier's root, one component under the deck's HOME.
const GATE: &str = "probe-gate";
const BUDGET: Duration = Duration::from_secs(120);
/// How long the idle agent's output is recorded before the first probe.
const IDLE_WATCH: Duration = Duration::from_secs(60);
/// Probes at the interval production has.
const PROBES_NATURAL: usize = 40;
/// Probes at each widened interval, and the widths.
const PROBES_WIDENED: usize = 15;
const WIDENED_MS: [u64; 2] = [100, 1000];
/// The control's probes and hold: the pane is resized as each hold starts.
const PROBES_CONTROL: usize = 5;
const CONTROL_MS: u64 = 1000;
/// The output quiet that counts as the agent idle again after a probe.
const QUIET: Duration = Duration::from_millis(750);

/// The pinned Claude Code release: `DAD_PROBE_FRESHNESS_BINARY`, else the
/// host's own versions directory.
fn claude_release_binary() -> Result<PathBuf, String> {
    let release = release();
    let path = match std::env::var_os("DAD_PROBE_FRESHNESS_BINARY") {
        Some(path) => PathBuf::from(path),
        None => Path::new(&std::env::var_os("HOME").ok_or("no HOME")?)
            .join(".local/share/claude/versions")
            .join(&release),
    };
    if path.is_file() {
        return Ok(path);
    }
    Err(format!(
        "Claude Code {release} is not at the pinned path {}: set DAD_PROBE_FRESHNESS_BINARY \
         to the {release} executable (DAD_PROBE_FRESHNESS_RELEASE names another release)",
        path.display()
    ))
}

/// One attach request with a bound wide enough for a widened probe.
fn request(socket: &Path, req: Value) -> Value {
    let mut s = UnixStream::connect(socket).expect("connect to the attach socket");
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    let bytes = serde_json::to_vec(&req).unwrap();
    s.write_all(&[KIND_REQ]).unwrap();
    s.write_all(&(bytes.len() as u32).to_be_bytes()).unwrap();
    s.write_all(&bytes).unwrap();
    let mut header = [0; 5];
    s.read_exact(&mut header)
        .unwrap_or_else(|e| panic!("attach op {} must answer: {e}", req["op"]));
    assert_eq!(header[0], KIND_RESP);
    let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    let mut body = vec![0; len];
    s.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// The pane's output, each frame's arrival instant and length, read from its
/// attach stream (no geometry: this client resizes nothing).
struct OutputLog {
    frames: Arc<Mutex<Vec<(Instant, usize)>>>,
    stop: Arc<AtomicBool>,
    stream: UnixStream,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl OutputLog {
    fn start(socket: &Path, agent: &str) -> Self {
        use dot_agent_deck::daemon_protocol::AttachRequest;
        let mut stream = UnixStream::connect(socket).expect("connect the output stream");
        let payload = serde_json::to_vec(&AttachRequest::AttachStream {
            id: agent.to_string(),
            rows: None,
            cols: None,
            geometry_updates: false,
            client_id: None,
        })
        .unwrap();
        stream.write_all(&[KIND_REQ]).unwrap();
        stream
            .write_all(&(payload.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(&payload).unwrap();
        let mut reader = stream.try_clone().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (out, halt) = (Arc::clone(&frames), Arc::clone(&stop));
        let thread = std::thread::spawn(move || {
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 65536];
            while !halt.load(Ordering::Relaxed) {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let at = Instant::now();
                        buf.extend_from_slice(&chunk[..n]);
                        while buf.len() >= 5 {
                            let len = u32::from_be_bytes(buf[1..5].try_into().unwrap()) as usize;
                            if buf.len() < 5 + len {
                                break;
                            }
                            if buf[0] == KIND_STREAM_OUT {
                                out.lock().unwrap().push((at, len));
                            }
                            buf.drain(..5 + len);
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => break,
                }
            }
        });
        Self {
            frames,
            stop,
            stream,
            thread: Some(thread),
        }
    }

    fn between(&self, from: Instant, to: Instant) -> Vec<(Instant, usize)> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|(at, _)| *at >= from && *at < to)
            .collect()
    }

    fn last(&self) -> Option<Instant> {
        self.frames.lock().unwrap().last().map(|(at, _)| *at)
    }

    /// Until no output came for `QUIET`, bounded.
    fn wait_quiet(&self, bound: Duration) -> bool {
        common::wait_until(bound, || self.last().is_none_or(|at| at.elapsed() >= QUIET))
    }
}

impl Drop for OutputLog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Let `span` pass, through the harness' bounded poll (Decision 21), to
/// within its 50 ms step.
fn pass(span: Duration) {
    let start = Instant::now();
    common::wait_until(span + Duration::from_secs(5), || start.elapsed() >= span);
}

/// A small generator for the pre-probe wait: no randomness crate needed.
struct Jitter(u64);
impl Jitter {
    fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        Self(seed | 1)
    }
    fn ms(&mut self, from: u64, to: u64) -> Duration {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        Duration::from_millis(from + self.0 % (to - from))
    }
}

struct Run {
    deck: common::TuiDeck,
    agent: String,
    generation: u64,
    kind: &'static str,
    release: String,
}

impl Run {
    fn control(&self, operation: &str) -> Value {
        request(
            self.deck.attach_socket_path(),
            json!({"op":"e2e-composer-control","pane_id":PANE,
                "generation":self.generation,"operation":operation}),
        )
    }

    fn screen(&self) -> String {
        let record =
            request(self.deck.attach_socket_path(), json!({"op":"list-agents"}))["agent_records"]
                .as_array()
                .and_then(|rs| rs.iter().find(|r| r["id"] == self.agent.as_str()).cloned())
                .unwrap_or(Value::Null);
        let rows = record["rows"].as_u64().unwrap_or(40) as u16;
        let cols = record["cols"].as_u64().unwrap_or(140) as u16;
        let mut parser = dot_agent_deck::vt_grid::parser(rows, cols, 0);
        parser.process(&common::pane_snapshot_on(
            self.deck.attach_socket_path(),
            &self.agent,
        ));
        parser.screen().contents()
    }

    fn start(kind: &'static str, deck: common::TuiDeck, command: String) -> Self {
        Self::start_as(kind, "claude_code", release(), deck, command)
    }

    fn start_as(
        kind: &'static str,
        agent_type: &str,
        release: String,
        deck: common::TuiDeck,
        command: String,
    ) -> Self {
        let reply = request(
            deck.attach_socket_path(),
            json!({"op":"start-agent", "command":command,
            "agent_type":agent_type,
            "cwd":deck.workdir(), "rows":40, "cols":140, "display_name":PANE,
            "env":[["DOT_AGENT_DECK_PANE_ID",PANE], ["DISABLE_AUTOUPDATER","1"]]}),
        );
        assert_eq!(reply["ok"], true, "{kind} spawn: {reply}");
        let agent = reply["id"].as_str().unwrap().to_string();
        let generation = agent.parse().expect("a generation is its agent id");
        Run {
            deck,
            agent,
            generation,
            kind,
            release,
        }
    }
}

#[derive(Default)]
struct Tally {
    hold_ms: u64,
    /// The control: the pane resized while the writer is held, so every
    /// probe that reaches the writer must be refused for the screen.
    disturbed: bool,
    asked: usize,
    written: usize,
    /// Refused at the writer for the screen: the measured refusal.
    fresh_refused: usize,
    /// Refused at the writer for anything else.
    writer_other: Vec<String>,
    /// Refused when scheduled, or not due: outside the interval.
    scheduled_refused: Vec<String>,
    /// Probes with any output between the request and its answer.
    output_during: usize,
    /// Each probe's request-to-answer time, in ms.
    round_trip_ms: Vec<u64>,
}

impl Tally {
    fn summary(&self) -> Value {
        let reached = self.written + self.fresh_refused + self.writer_other.len();
        let mut rt = self.round_trip_ms.clone();
        rt.sort_unstable();
        let pct = |p: usize| rt.get((rt.len().saturating_sub(1)) * p / 100).copied();
        json!({
            "hold_ms": self.hold_ms, "disturbed": self.disturbed, "asked": self.asked, "reached_writer": reached,
            "written": self.written, "fresh_refused": self.fresh_refused,
            "rate": if reached > 0 { self.fresh_refused as f64 / reached as f64 } else { f64::NAN },
            "writer_other": self.writer_other, "scheduled_refused": self.scheduled_refused,
            "output_during_request": self.output_during,
            "round_trip_ms": {"p50": pct(50), "p90": pct(90), "max": rt.last()},
        })
    }
}

/// Inter-arrival statistics of output frames in `frames`, over `span`.
fn cadence(frames: &[(Instant, usize)], span: Duration) -> Value {
    let gaps: Vec<u64> = frames
        .windows(2)
        .map(|w| w[1].0.duration_since(w[0].0).as_millis() as u64)
        .collect();
    let mut sorted = gaps.clone();
    sorted.sort_unstable();
    let pct = |p: usize| {
        sorted
            .get((sorted.len().saturating_sub(1)) * p / 100)
            .copied()
    };
    json!({
        "seconds": span.as_secs_f64(), "frames": frames.len(),
        "bytes": frames.iter().map(|(_, n)| n).sum::<usize>(),
        "frames_per_s": frames.len() as f64 / span.as_secs_f64(),
        "gap_ms": {"min": sorted.first(), "p50": pct(50), "p90": pct(90), "max": sorted.last()},
        "lens": frames.iter().map(|(_, n)| *n).take(40).collect::<Vec<_>>(),
    })
}

/// After a probe: its segment ended (the fence open, nothing held), the
/// schedule due again, and the agent's own redraws stopped.
fn settle(run: &Run, out: &OutputLog) {
    let open = common::wait_until(Duration::from_secs(30), || {
        let f = &run.control("fence-status")["composer_fence"];
        f["fenced"] == false && f["held_ops"].as_u64() == Some(0)
    });
    assert!(open, "the probe's segment must end; {}", run.screen());
    if run.control("schedule-status")["composer_schedule"]["due"] != true {
        // A refusal's backoff, or a non-empty read's wait: neither is the
        // interval under measurement.
        let advanced = run.control("schedule-advance:3600000");
        assert_eq!(
            advanced["composer_schedule"]["due"], true,
            "the schedule must be due after an advance: {advanced}"
        );
    }
    assert!(
        out.wait_quiet(Duration::from_secs(30)),
        "the {} pane must go quiet after a probe; {}",
        run.kind,
        run.screen()
    );
}

/// One probe, with the writer held `hold_ms` after the frame was read when
/// that is nonzero, and the pane resized as the hold starts when `disturb`.
fn probe(run: &Run, out: &OutputLog, hold_ms: u64, disturb: bool, tally: &mut Tally) {
    let root = run.deck.home_dir().join(GATE);
    let socket = run.deck.attach_socket_path().to_path_buf();
    let agent = run.agent.clone();
    let rows = if tally.asked.is_multiple_of(2) {
        38
    } else {
        40
    };
    let holder = (hold_ms > 0).then(|| {
        std::fs::write(root.join("phase"), "scheduled").unwrap();
        let root = root.clone();
        std::thread::spawn(move || {
            // `wait_until` polls every 50 ms, so the writer is held from
            // `hold_ms` to `hold_ms` + 50 ms; the round trip records it.
            let arrived = root.join("scheduled.arrived");
            let seen = common::wait_until(Duration::from_secs(20), || arrived.exists());
            if seen && disturb {
                let resized = request(
                    &socket,
                    json!({"op":"resize","id":agent,"rows":rows,"cols":140}),
                );
                assert_eq!(resized["ok"], true, "control resize: {resized}");
            }
            if seen {
                pass(Duration::from_millis(hold_ms));
            }
            std::fs::write(root.join("scheduled.release"), "").unwrap();
            seen
        })
    });
    let t0 = Instant::now();
    let reply = run.control("probe-request");
    let t1 = Instant::now();
    if let Some(h) = holder {
        let _ = h.join();
        for f in [
            "phase",
            "scheduled.arrived",
            "scheduled.release",
            "scheduled.timed-out",
        ] {
            let _ = std::fs::remove_file(root.join(f));
        }
    }
    tally.asked += 1;
    tally
        .round_trip_ms
        .push(t1.duration_since(t0).as_millis() as u64);
    if !out.between(t0, t1).is_empty() {
        tally.output_during += 1;
    }
    let p = &reply["composer_probe"];
    let outcome = p["outcome"].as_str().unwrap_or_default();
    let reason = p["reason"].as_str().unwrap_or_default().to_string();
    match outcome {
        "written" => tally.written += 1,
        "refused" if reason == "Admission(Surface)" => tally.fresh_refused += 1,
        "refused" if reason.starts_with("Admission(") || reason.contains("write failed") => {
            tally.writer_other.push(reason)
        }
        "refused" => tally.scheduled_refused.push(reason),
        _ => tally.scheduled_refused.push(format!("{outcome}: {reply}")),
    }
}

fn measure(run: Run) {
    std::fs::create_dir_all(run.deck.home_dir().join(GATE)).unwrap();
    let out = OutputLog::start(run.deck.attach_socket_path(), &run.agent);
    // The probe path is live on this generation before anything is counted:
    // its permission settles once the header latched.
    let live = common::wait_until(BUDGET, || {
        let r = run.control("schedule-status");
        r["ok"] == true
    });
    assert!(live, "{} must be probe-capable; {}", run.kind, run.screen());
    // And armable: its permission settles once the header latched and the
    // agent is bound. A probe asked before then is refused when scheduled,
    // and each refusal's backoff is advanced past.
    let last = std::cell::RefCell::new(Value::Null);
    let armable = common::wait_until(BUDGET, || {
        if run.control("schedule-status")["composer_schedule"]["due"] != true {
            run.control("schedule-advance:3600000");
        }
        *last.borrow_mut() = run.control("probe-request");
        last.borrow()["composer_probe"]["reason"] != "NotArmable(Pending)"
    });
    assert!(
        armable,
        "{} must become armable: {}; {}",
        run.kind,
        last.borrow(),
        run.screen()
    );
    settle(&run, &out);
    assert!(
        out.wait_quiet(BUDGET),
        "{} must go idle; {}",
        run.kind,
        run.screen()
    );

    let idle_from = Instant::now();
    pass(IDLE_WATCH);
    let idle = cadence(&out.between(idle_from, Instant::now()), IDLE_WATCH);
    eprintln!("[{}] idle cadence: {idle}", run.kind);

    let mut jitter = Jitter::new();
    let mut conditions = Vec::new();
    let mut gaps_before: Vec<(Instant, usize)> = Vec::new();
    let mut gap_time = Duration::ZERO;
    let plan = std::iter::once((0, PROBES_NATURAL, false))
        .chain(WIDENED_MS.iter().map(|ms| (*ms, PROBES_WIDENED, false)))
        .chain(std::iter::once((CONTROL_MS, PROBES_CONTROL, true)));
    for (hold_ms, n, disturbed) in plan {
        let mut tally = Tally {
            hold_ms,
            disturbed,
            ..Default::default()
        };
        for _ in 0..n {
            settle(&run, &out);
            let from = Instant::now();
            pass(jitter.ms(500, 2500));
            gaps_before.extend(out.between(from, Instant::now()));
            gap_time += from.elapsed();
            probe(&run, &out, hold_ms, disturbed, &mut tally);
        }
        settle(&run, &out);
        eprintln!("[{}] {}", run.kind, tally.summary());
        conditions.push(tally);
    }
    let summary = json!({
        "agent": run.kind, "release": run.release,
        // The deck build measured: the library this test links is built
        // from the same tree in the same cargo run as the binary it drives,
        // so a summary names the commit (and `-dirty`) its numbers belong to.
        "build_id": dot_agent_deck::build_id::local_build_id(),
        "idle": idle,
        "between_probes": cadence(&gaps_before, gap_time),
        "conditions": conditions.iter().map(Tally::summary).collect::<Vec<_>>(),
        "load": std::fs::read_to_string("/proc/loadavg").unwrap_or_default().trim(),
        "cpu_pressure": std::fs::read_to_string("/proc/pressure/cpu").unwrap_or_default(),
    });
    eprintln!("PROBE-FRESHNESS {summary}");
    if let Some(path) = std::env::var_os("DAD_PROBE_FRESHNESS_OUT") {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(f, "{summary}").unwrap();
    }
    for t in conditions.iter().filter(|t| t.disturbed) {
        assert!(
            t.fresh_refused > 0,
            "{}: a resize while the writer was held must refuse the screen, \
             or a zero elsewhere measures nothing: {}",
            run.kind,
            t.summary()
        );
    }
    for t in conditions.iter().filter(|t| !t.disturbed) {
        let reached = t.written + t.fresh_refused + t.writer_other.len();
        assert!(
            reached * 2 >= t.asked && t.written > 0,
            "{} at hold {} ms: too few probes reached the writer to measure: {}",
            run.kind,
            t.hold_ms,
            t.summary()
        );
    }
}

fn deck() -> common::TuiDeckBuilder {
    deck_assuming(&format!("claude@{}", release()))
}

fn deck_assuming(assumed: &str) -> common::TuiDeckBuilder {
    common::TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_env("DOT_AGENT_DECK_E2E_ASSUME_EDITOR_PROBE", assumed)
        .with_env("DOT_AGENT_DECK_E2E_PROBE_GATE", GATE)
        .with_env("DOT_AGENT_DECK_LOG", "probe-freshness.log")
        .with_env("RUST_LOG", "info,composer_probe=debug")
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "900")
}

/// Scenario: Start the recorded Claude Code release in a deck pane and leave it idle at its empty composer, record its output for a minute, then ask for probes one at a time, each after the last one's redraws stopped and a random wait, first at the writer's own interval, then with the writer held 100 ms and 1 s after the frame was read, and last held 1 s while the test resizes the pane. The test reports how many probes the writer refused because the screen moved since its frame, and fails unless the resized probes are refused that way.
#[spec("inbox/probe/083")]
#[test]
fn probe_083_claude_idle_screen_freshness_refusal_rate() {
    skip_unless!(common::check_claude_available());
    let binary = match claude_release_binary() {
        Ok(b) => b,
        Err(why) => {
            skip_unless!(Err(why));
            return;
        }
    };
    let deck = deck()
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .launch_with_fixture("minimal");
    assert!(deck.wait_for_grid_string_within("No active agents", BUDGET));
    // The pane's class is read from its command's basename.
    let bin = deck.home_dir().join("pinned");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(&binary, bin.join("claude")).unwrap();
    let command = format!(
        "{} --model claude-haiku-4-5-20251001",
        bin.join("claude").display()
    );
    let run = Run::start("claude", deck, command);
    assert!(
        common::wait_until(BUDGET, || run
            .screen()
            .contains(&format!("Claude Code v{}", release()))),
        "Claude Code {} must start; {}",
        release(),
        run.screen()
    );
    measure(run);
}

/// The pinned native Codex 0.164.0: `DOT_AGENT_DECK_CODEX_0_164_0`, else the
/// package the earlier Codex units unpacked under `/var/tmp`.
fn codex_release_binary() -> Result<PathBuf, String> {
    let path = std::env::var_os("DOT_AGENT_DECK_CODEX_0_164_0")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(
                "/var/tmp/dad-cxhe-codex-0.164.0/x64/package/vendor/x86_64-unknown-linux-musl/bin/codex",
            )
        });
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "the pinned Codex 0.164.0 is not at {}: set DOT_AGENT_DECK_CODEX_0_164_0",
            path.display()
        ))
    }
}

/// Scenario: Start the pinned Codex 0.164.0 in a deck pane with a dummy key and no reachable provider, leave it idle at its empty composer, record its output for a minute, then ask for probes one at a time exactly as the Claude Code run does: at the writer's own interval, held 100 ms and 1 s after the frame was read, and held 1 s while the test resizes the pane. The test reports how many probes the writer refused because the screen moved since its frame, and fails unless the resized probes are refused that way.
#[spec("inbox/probe/083")]
#[test]
fn probe_083_codex_164_idle_screen_freshness_refusal_rate() {
    let binary = match codex_release_binary() {
        Ok(b) => b,
        Err(why) => {
            skip_unless!(Err(why));
            return;
        }
    };
    let deck = deck_assuming("codex@0.164.0").launch_with_fixture("minimal");
    assert!(deck.wait_for_grid_string_within("No active agents", BUDGET));
    // No credential: a dummy key, and a provider on a closed loopback port.
    let codex_home = deck.home_dir().join(".codex");
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::write(
        codex_home.join("auth.json"),
        r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-nollm-dummy-00000000000000000000000000000000"}"#,
    )
    .unwrap();
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            "model = \"nollm-model\"\nmodel_provider = \"nollm\"\ncheck_for_update_on_startup = false\n\n\
             [projects.\"{}\"]\ntrust_level = \"trusted\"\n\n\
             [model_providers.nollm]\nname = \"nollm\"\nbase_url = \"http://127.0.0.1:9/v1\"\n\
             wire_api = \"responses\"\nrequires_openai_auth = true\nrequest_max_retries = 0\n\
             stream_max_retries = 0\n",
            deck.workdir().display()
        ),
    )
    .unwrap();
    // The pane's class is read from its command's basename.
    let bin = deck.home_dir().join("pinned");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(&binary, bin.join("codex")).unwrap();
    trust_codex_hooks(&deck, &bin);
    let command = format!("{}", bin.join("codex").display());
    let run = Run::start_as("codex", "codex", "0.164.0".to_string(), deck, command);
    assert!(
        common::wait_until(BUDGET, || run.screen().contains("OpenAI Codex (v0.164.0)")),
        "Codex 0.164.0 must start; {}",
        run.screen()
    );
    measure(run);
}

/// The deck's Codex hooks installed and trusted in the HOME's `CODEX_HOME`
/// before Codex starts (as `support/codex_inbox_live.rs`'s `prepare` does,
/// with the pinned Codex first on the search path): otherwise the deck's own
/// start-up trust write lands in `config.toml` after the spawn's keymap read,
/// and the generation holds as `Keymap(Changed)` for its whole life (loc10).
fn trust_codex_hooks(deck: &common::TuiDeck, pinned: &Path) {
    let home = deck.home_dir().join(".codex");
    let binary = common::contained_deck_binary();
    let installed = deck.home_dir().join(".local/bin/dot-agent-deck");
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    if !installed.exists() && std::fs::hard_link(binary, &installed).is_err() {
        std::fs::copy(binary, &installed).unwrap();
    }
    let last = std::cell::RefCell::new(String::new());
    let ready = common::wait_until(BUDGET, || {
        let output = std::process::Command::new(binary)
            .args(["hooks", "install", "--agent", "codex"])
            .env_clear()
            .env("HOME", deck.home_dir())
            .env("CODEX_HOME", &home)
            .env("XDG_CONFIG_HOME", deck.home_dir().join(".config"))
            .env(
                "PATH",
                format!(
                    "{}:{}:/usr/bin:/bin",
                    pinned.display(),
                    installed.parent().unwrap().display()
                ),
            )
            .env("DOT_AGENT_DECK_TEST_CONFIG_WRITE", "1")
            .env("DOT_AGENT_DECK_TEST_CONFIG_ROOT", deck.workdir())
            .current_dir(deck.workdir())
            .output()
            .unwrap();
        *last.borrow_mut() = format!("{output:?}");
        output.status.success()
            && std::fs::read_to_string(home.join("config.toml"))
                .is_ok_and(|config| config.matches("trusted_hash").count() >= 3)
    });
    assert!(
        ready,
        "the deck's Codex hooks must be trusted first; {}",
        last.borrow()
    );
}
