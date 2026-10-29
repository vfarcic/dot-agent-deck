//! PRD #1666 part 3 (Q35-14b's review, Q84): an installed generation's
//! observer (`src/composer_probe/observe.rs`) on real processes, its own
//! thread running: a temporary instance whose helper copy is a copy of
//! `/bin/sh` that blocks reading a FIFO, so a "helper" is a live process
//! running the helper copy and never `exec`s anything else; connections
//! registered by a real pidfd standing in for `SO_PEERPIDFD`; markers
//! written into `inv/G/` as a helper writes them; and an agent `A` launched
//! as the deck launches a direct one, so the permission settles and the
//! child scan runs. Every process a test starts is killed by its captured
//! handle or pid.

#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::fs::{File, Permissions};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use dot_agent_deck::composer_probe::Permission;
use dot_agent_deck::composer_probe::epoch::GenerationInput;
use dot_agent_deck::composer_probe::install::{Originals, PaneClass, ProbeAgent, install};
use dot_agent_deck::composer_probe::instance::{
    DaemonId, Instance, InstanceReader, open_root, splittable,
};
use dot_agent_deck::composer_probe::observe::{
    Bell, ControlOp, DRAIN_RETRIES, E2_AFTER, Observer, SCAN_PERIOD, Spawned,
};
use dot_agent_deck::composer_probe::record::{Decision, NotConsumed, RecordKey, Settlement};
use dot_agent_deck::doorbell_activation::{
    EditorProbe, EditorSelection, ExportShape, KeymapSource, LatchedRelease, Measurement,
    PasteToggles,
};
use dot_agent_deck::event::{
    AgentType, ComposerClaim, ComposerDecision, ComposerInvocationRequest,
};
use semver::Version;
use spec::spec;

const G: u64 = 7;

/// The release the latch reports, which `RELEASE_RECORDS` holds for Claude.
fn release() -> Version {
    Version::new(2, 1, 310)
}

/// An editor-probe record for that release, as the `e2e` build assumes one.
fn assumed() -> &'static [(AgentType, Version, EditorProbe<'static>)] {
    static ASSUMED: OnceLock<Vec<(AgentType, Version, EditorProbe<'static>)>> = OnceLock::new();
    ASSUMED.get_or_init(|| {
        vec![(
            AgentType::ClaudeCode,
            release(),
            EditorProbe {
                export: ExportShape::ClaudeTmpdir,
                selection: EditorSelection::ClaudeCode,
                paste_toggles: PasteToggles {
                    disables: 1,
                    enables: 1,
                },
                keymap_sources: &[KeymapSource::ClaudeKeybindings],
                keymap_loaded_at_start: Measurement::Verified(()),
                inert_candidates: &[],
                pasted_bel_is_text: Measurement::Absent,
            },
        )]
    })
}

fn tempdir() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("observer")
        .rand_bytes(8)
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    assert!(splittable(dir.path()), "{}", dir.path().display());
    std::fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).unwrap();
    assert!(
        open_root(dir.path()).is_ok(),
        "{} passes through a directory another user could change: point TMPDIR at one that doesn't",
        dir.path().display()
    );
    dir
}

fn start_of(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let after = &stat[stat.rfind(')').unwrap() + 1..];
    after.split_whitespace().nth(19).unwrap().parse().unwrap()
}

fn key_of(pid: u32) -> RecordKey {
    RecordKey {
        pid,
        start: start_of(pid),
    }
}

fn pidfd(pid: u32) -> OwnedFd {
    // SAFETY: `pidfd_open(pid, 0)`; a non-negative result is a new
    // descriptor we own.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    assert!(fd >= 0, "pidfd_open({pid})");
    // SAFETY: just opened, owned by nothing else.
    unsafe { OwnedFd::from_raw_fd(fd as i32) }
}

fn alive(child: &mut Child) -> bool {
    child.try_wait().unwrap().is_none()
}

fn wait_for(budget: Duration, mut done: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + budget;
    while Instant::now() < until {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

fn request(decision: ComposerDecision) -> ComposerInvocationRequest {
    ComposerInvocationRequest {
        pane_id: "p".into(),
        token: None,
        generation: G,
        decision,
        claim: Some(ComposerClaim::Owned),
        seg_seen: Some(0),
        native_seen: Some(0),
        argv: vec!["/tmp/claude-1000/claude-prompt-1.md".into()],
    }
}

/// A temporary instance with generation `G` installed for a direct Claude
/// and its observer running.
struct Harness {
    dir: tempfile::TempDir,
    instance: Instance,
    observer: Arc<Observer>,
    /// Every process the test started, killed when the harness goes.
    started: Vec<Child>,
}

impl Harness {
    /// `agent` gives the launch from the helper copy's path: `A`'s argv,
    /// executed by the test as the deck executes a direct command, or none.
    fn start(agent: impl FnOnce(&Path) -> Option<Vec<OsString>>) -> Harness {
        let dir = tempdir();
        let daemon = DaemonId {
            pid: std::process::id(),
            start: start_of(std::process::id()),
            boot: "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0".to_string(),
            nonce: "00112233445566778899aabbccddeeff".to_string(),
        };
        let instance = Instance::create(
            &dir.path().join("state").join("editor"),
            daemon,
            &mut File::open("/bin/sh").unwrap(),
        )
        .unwrap();
        let mut installation = install(
            &instance,
            PaneClass::Direct(ProbeAgent::Claude),
            &G.to_string(),
            &Originals::default(),
        )
        .unwrap();
        let mut started = Vec::new();
        let agent = agent(instance.helper());
        let child = agent.as_ref().map(|launch| {
            let a = Command::new(&launch[0])
                .args(&launch[1..])
                .stdin(Stdio::null())
                .spawn()
                .unwrap();
            let pid = a.id();
            started.push(a);
            assert!(wait_for(Duration::from_secs(5), || std::fs::read(format!(
                "/proc/{pid}/cmdline"
            ))
            .is_ok_and(|c| c.starts_with(launch[0].as_encoded_bytes()))));
            (pid, start_of(pid))
        });
        installation.set_launch(agent.unwrap_or_default());
        let bell: Arc<OnceLock<Arc<Bell>>> = Arc::new(OnceLock::new());
        let observer = Observer::start(
            installation,
            Spawned {
                agent_id: G.to_string(),
                agent: Some(AgentType::ClaudeCode),
                env: vec![],
                child,
                input: Arc::new(GenerationInput::default()),
                latch: Box::new(|_| LatchedRelease::Known(release())),
                assumed: assumed(),
            },
            &bell,
        )
        .expect("observer");
        Harness {
            dir,
            instance,
            observer,
            started,
        }
    }

    fn inv(&self) -> PathBuf {
        self.instance.dir().join("inv").join(G.to_string())
    }

    fn fifo(&self, name: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        path
    }

    /// A live process running `program` as `sh -c 'read x < fifo'`: blocked
    /// in a builtin, never `exec`ing anything else.
    fn blocked(&mut self, program: &Path, name: &str) -> u32 {
        let fifo = self.fifo(name);
        let child = Command::new(program)
            .args(["-c", "read x < \"$0\""])
            .arg(&fifo)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        self.started.push(child);
        let program = std::fs::canonicalize(program).unwrap();
        assert!(wait_for(Duration::from_secs(5), || std::fs::read_link(
            format!("/proc/{pid}/exe")
        )
        .is_ok_and(|exe| exe == program)));
        pid
    }

    fn helper(&mut self, name: &str) -> u32 {
        let program = self.instance.helper().to_path_buf();
        self.blocked(&program, name)
    }

    fn process(&mut self, pid: u32) -> &mut Child {
        self.started.iter_mut().find(|c| c.id() == pid).unwrap()
    }

    fn marker(&self, pid: u32, decision: &str) {
        let name = format!("{pid}-{}-{decision}-0-0", start_of(pid));
        let path = self.inv().join(name);
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.observer.stop();
        for child in &mut self.started {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Scenario: Start an observer, connect two live helper copies by their pidfds and have one declare `Armed` and the other `Unarmed`, then write armed markers naming an unrelated live process and a third helper copy that never connected. Past E2's bound only the connected `Armed` helper is killed, by `SIGKILL`; the unrelated process, the marker-named helper and the editor-bearing helper all live on, though each marker's decision was recorded.
#[spec("inbox/probe/072")]
#[test]
fn probe_072_e2_signals_only_a_helper_the_daemon_attested() {
    let mut h = Harness::start(|_| None);
    let armed = h.helper("armed");
    let editor = h.helper("editor");
    let named = h.helper("named");
    let stranger = h.blocked(Path::new("/bin/sh"), "stranger");

    let armed_conn = h.observer.connect(Ok(Some(pidfd(armed)))).expect("helper");
    let editor_conn = h.observer.connect(Ok(Some(pidfd(editor)))).expect("helper");
    assert!(
        h.observer.connect(Ok(Some(pidfd(stranger)))).is_none(),
        "not the helper copy: no capability"
    );
    h.observer
        .invocation(&armed_conn, &request(ComposerDecision::Armed));
    h.observer
        .invocation(&editor_conn, &request(ComposerDecision::Unarmed));
    h.marker(stranger, "armed");
    h.marker(named, "armed");
    let input = h.observer.input();
    assert!(
        wait_for(Duration::from_secs(5), || [stranger, named].iter().all(
            |p| {
                input
                    .record(key_of(*p))
                    .is_some_and(|r| r.decision == Some(Decision::Armed))
            }
        )),
        "each marker's decision is recorded"
    );

    assert!(
        wait_for(E2_AFTER + Duration::from_secs(5), || !alive(
            h.process(armed)
        )),
        "the attested armed helper is killed past its bound"
    );
    let status = h.process(armed).wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    // Every other one is past its bound by now too.
    std::thread::sleep(Duration::from_secs(1));
    for pid in [stranger, named, editor] {
        assert!(alive(h.process(pid)), "{pid} must not be signalled");
    }
    drop((armed_conn, editor_conn));
}

/// Scenario: A helper's `Unarmed` invocation is recorded and its connection closed, then `inv/G/` is filled past what a pass reads and the helper killed: its exit is observed but the record isn't drained while the read overflows, however long it waits; once the extra names go it drains and is pruned. The same with `inv/G/` moved away (a failed read) instead: not drained until it is back.
#[spec("inbox/probe/072")]
#[test]
fn probe_072_an_overflowed_or_failed_marker_read_is_no_drain() {
    let mut h = Harness::start(|_| None);
    let input = h.observer.input();

    // Overflow.
    let helper = h.helper("overflow");
    let key = key_of(helper);
    let conn = h.observer.connect(Ok(Some(pidfd(helper)))).unwrap();
    h.observer
        .invocation(&conn, &request(ComposerDecision::Unarmed));
    drop(conn);
    let junk: Vec<PathBuf> = (0..300)
        .map(|i| h.inv().join(format!("junk-{i}")))
        .collect();
    for path in &junk {
        std::fs::write(path, b"").unwrap();
    }
    h.process(helper).kill().unwrap();
    h.process(helper).wait().unwrap();
    assert!(wait_for(Duration::from_secs(5), || input
        .record(key)
        .is_some_and(|r| r.exit_observed.is_some())));
    std::thread::sleep(SCAN_PERIOD * 20);
    let r = input.record(key).expect("kept while the read overflows");
    assert!(!r.drained, "an overflowed read is no drain");
    for path in &junk {
        std::fs::remove_file(path).unwrap();
    }
    assert!(
        wait_for(Duration::from_secs(5), || input
            .record(key)
            .is_none_or(|r| r.drained)),
        "drained once the read is complete"
    );
    let fact = h.observer.exit_fact(key).unwrap();
    assert_eq!(fact.decision, Some(Decision::Unarmed));

    // A failed read.
    let helper = h.helper("failed");
    let key = key_of(helper);
    let conn = h.observer.connect(Ok(Some(pidfd(helper)))).unwrap();
    h.observer
        .invocation(&conn, &request(ComposerDecision::Unarmed));
    drop(conn);
    let away = h.inv().with_extension("away");
    std::fs::rename(h.inv(), &away).unwrap();
    h.process(helper).kill().unwrap();
    h.process(helper).wait().unwrap();
    assert!(wait_for(Duration::from_secs(5), || input
        .record(key)
        .is_some_and(|r| r.exit_observed.is_some())));
    std::thread::sleep(SCAN_PERIOD * 20);
    assert!(
        !input.record(key).expect("kept").drained,
        "a failed read is no drain"
    );
    std::fs::rename(&away, h.inv()).unwrap();
    assert!(wait_for(Duration::from_secs(5), || input
        .record(key)
        .is_none_or(|r| r.drained)));
}

/// Scenario: With `inv/G/` kept past what a pass reads, a killed helper's drain is retried until its bound and then given up: the record is drained but kept, its markers not taken as deleted, and it is still there after the extra names go.
#[spec("inbox/probe/072")]
#[test]
fn probe_072_a_drain_still_unread_after_its_retries_is_given_up_and_kept() {
    let mut h = Harness::start(|_| None);
    let input = h.observer.input();
    let helper = h.helper("bounded");
    let key = key_of(helper);
    let conn = h.observer.connect(Ok(Some(pidfd(helper)))).unwrap();
    h.observer
        .invocation(&conn, &request(ComposerDecision::Unarmed));
    drop(conn);
    let junk: Vec<PathBuf> = (0..300)
        .map(|i| h.inv().join(format!("junk-{i}")))
        .collect();
    for path in &junk {
        std::fs::write(path, b"").unwrap();
    }
    h.process(helper).kill().unwrap();
    h.process(helper).wait().unwrap();
    assert!(
        wait_for(SCAN_PERIOD * (DRAIN_RETRIES + 100), || input
            .record(key)
            .is_some_and(|r| r.drained)),
        "given up after its retries"
    );
    for path in &junk {
        std::fs::remove_file(path).unwrap();
    }
    std::thread::sleep(SCAN_PERIOD * 20);
    assert!(
        input.record(key).is_some(),
        "an uncertain drain infers no deletion"
    );
}

/// Scenario: Launch `A` as the deck launches a direct Claude, with two helper copies as its children, and race four control publications against the observer's own settlement: the permission settles `Armable` once, `A` stays bound and `gen-G` names it. The scan records both helpers; each gets a registered connection and is killed. The connection carrying a decision still in flight keeps the drain waiting past the read deadline until that `Unarmed` decision is applied and the connection closes, so the exit settles `NotConsumed(Unarmed)`; the control connection, with nothing in flight, closes at its deadline and its helper's exit settles with no decision.
#[spec("inbox/probe/072")]
#[test]
fn probe_072_a_decision_in_flight_at_the_exit_is_applied_before_the_drain() {
    let bin = tempdir();
    let agent_exe = bin.path().join("claude");
    std::fs::copy("/bin/sh", &agent_exe).unwrap();
    std::fs::set_permissions(&agent_exe, Permissions::from_mode(0o700)).unwrap();
    let fifos = bin.path().join("fifos");
    std::fs::create_dir(&fifos).unwrap();
    for name in ["in-flight", "quiet"] {
        let c = std::ffi::CString::new(fifos.join(name).as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }
    let h = Harness::start(|helper| {
        Some(vec![
            agent_exe.clone().into(),
            "-c".into(),
            "\"$1\" -c 'read x < \"$0\"' \"$2\" & \"$1\" -c 'read x < \"$0\"' \"$3\" & wait".into(),
            "a".into(),
            helper.into(),
            fifos.join("in-flight").into(),
            fifos.join("quiet").into(),
        ])
    });

    let racers: Vec<_> = (0..4)
        .map(|_| {
            let observer = Arc::clone(&h.observer);
            std::thread::spawn(move || observer.control(G, ControlOp::Publish))
        })
        .collect();
    for racer in racers {
        assert_eq!(racer.join().unwrap(), Ok(1), "published once, idempotently");
    }
    assert_eq!(h.observer.permission(), Permission::Armable);
    let a = h.started[0].id();
    let bound = h.observer.bound_agent().expect("A stays bound");
    assert_eq!(bound.pid, a);
    let reader = InstanceReader::open(h.instance.dir()).unwrap();
    assert_eq!(reader.identity(&G.to_string()), Some(bound));

    // Both helpers, as `A`'s children, recorded by the scan.
    let helper_id = std::fs::read_link(h.instance.helper()).unwrap_or(h.instance.helper().into());
    let mut helpers = Vec::new();
    assert!(wait_for(Duration::from_secs(10), || {
        helpers = std::fs::read_to_string(format!("/proc/{a}/task/{a}/children"))
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|p| p.parse::<u32>().ok())
            .filter(|p| std::fs::read_link(format!("/proc/{p}/exe")).is_ok_and(|e| e == helper_id))
            .collect();
        helpers.len() == 2
    }));
    let input = h.observer.input();
    assert!(wait_for(Duration::from_secs(5), || helpers
        .iter()
        .all(|p| input.record(key_of(*p)).is_some())));
    let (in_flight, quiet) = (helpers[0], helpers[1]);
    let (in_flight_key, quiet_key) = (key_of(in_flight), key_of(quiet));
    let busy = h.observer.connect(Ok(Some(pidfd(in_flight)))).unwrap();
    let idle = h.observer.connect(Ok(Some(pidfd(quiet)))).unwrap();

    for pid in [in_flight, quiet] {
        // SAFETY: a pid this test captured, of a process alive until now.
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) }, 0);
    }
    assert!(wait_for(Duration::from_secs(5), || [
        in_flight_key,
        quiet_key
    ]
    .iter()
    .all(|k| input
        .record(*k)
        .is_some_and(|r| r.exit_observed.is_some()))));
    // Each connection learns its helper's exit and its read deadline passes.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), busy.read_deadline())
            .await
            .expect("read deadline");
        tokio::time::timeout(Duration::from_secs(2), idle.read_deadline())
            .await
            .expect("read deadline");
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !input.record(in_flight_key).unwrap().drained,
        "no timer drains an open connection"
    );

    // The control: nothing pending at its deadline, so its reader closes it.
    drop(idle);
    assert!(wait_for(Duration::from_secs(5), || h
        .observer
        .exit_fact(quiet_key)
        .is_some()));
    let quiet_fact = h.observer.exit_fact(quiet_key).unwrap();
    assert_eq!(quiet_fact.decision, None);
    assert_eq!(
        quiet_fact.settlement,
        Settlement::NotConsumed(NotConsumed::Unproven)
    );
    assert!(!input.record(in_flight_key).unwrap().drained);

    // The decision in flight is applied, then its connection closes.
    h.observer
        .invocation(&busy, &request(ComposerDecision::Unarmed));
    drop(busy);
    assert!(wait_for(Duration::from_secs(5), || h
        .observer
        .exit_fact(in_flight_key)
        .is_some()));
    let fact = h.observer.exit_fact(in_flight_key).unwrap();
    assert_eq!(fact.decision, Some(Decision::Unarmed));
    assert_eq!(
        fact.settlement,
        Settlement::NotConsumed(NotConsumed::Unarmed)
    );
}
