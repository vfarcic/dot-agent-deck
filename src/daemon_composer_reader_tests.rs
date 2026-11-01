//! PRD #1666 part 3 (Q85): the hook loop's reading of a registered composer
//! connection, on a real hook socket served by [`run_hook_loop`]. An agent
//! spawned by the registry carries an installed generation's observer over a
//! temporary instance whose helper copy is a copy of `/bin/sh`; a "helper" is
//! that copy blocked reading a FIFO, and it `connect`s, before its `exec`, a
//! socket the test created, so the daemon's `SO_PEERPIDFD` names the helper
//! while the test keeps the descriptor: once the helper is killed, the test
//! is the foreign process still holding its connection. Every process a test
//! starts is killed by its captured handle, when the test ends or panics
//! ([`Deck`]'s drop), and the observer stopped with it. A bound deck
//! ([`Deck::start_bound`], Q87) binds `A` to the test process itself, from
//! its own argv, so the helpers it starts are `A`'s children, which the
//! child scan records `Owned` when their last argument is a Claude export.

use super::*;
use crate::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use crate::composer_probe::Permission;
use crate::composer_probe::epoch::GenerationInput;
use crate::composer_probe::install::{Originals, PaneClass, ProbeAgent, install};
use crate::composer_probe::instance::{DaemonId, Instance, open_root, splittable};
use crate::composer_probe::observe::{
    CONNECTIONS_PER_GENERATION, ControlOp, HeldForTest, LOST, LOST_PER_GENERATION, Observer,
    READ_DEADLINE, Spawned,
};
use crate::composer_probe::record::{
    Attribution, Decision, NotConsumed, RecordKey, Report, Settlement, Tree,
};
use crate::doorbell_activation::{
    EditorProbe, EditorSelection, ExportShape, KeymapSource, LatchedRelease, Measurement,
    PasteToggles,
};
use crate::event::{
    AgentType, ComposerClaim, ComposerDecision, ComposerInvocationReply, ComposerInvocationRequest,
    HookCapabilitiesReply, HookCapabilitiesRequest,
};
use spec::spec;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

const PANE: &str = "composer-reader-pane";

/// How long a test waits for something that, done right, takes a few
/// observer passes.
const SETTLE: Duration = Duration::from_secs(5);

/// The release a bound deck's latch reports, which `RELEASE_RECORDS` holds
/// for Claude.
fn release() -> semver::Version {
    semver::Version::new(2, 1, 310)
}

/// An editor-probe record for that release, as the `e2e` build assumes one.
fn assumed() -> &'static [(AgentType, semver::Version, EditorProbe<'static>)] {
    static ASSUMED: std::sync::OnceLock<Vec<(AgentType, semver::Version, EditorProbe<'static>)>> =
        std::sync::OnceLock::new();
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

async fn wait_for(budget: Duration, mut done: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + budget;
    while Instant::now() < until {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    done()
}

/// One composer connection, held by the test.
struct Peer {
    reader: tokio::io::BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Peer {
    async fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn reply(&mut self) -> String {
        let mut line = String::new();
        tokio::time::timeout(SETTLE, self.reader.read_line(&mut line))
            .await
            .expect("the daemon answers")
            .unwrap();
        line
    }

    /// Resolves when the daemon has closed the connection: EOF, or a reset
    /// when it closed with bytes of ours still unread.
    async fn closed(&mut self) {
        let mut sink = Vec::new();
        let _ = self.reader.read_to_end(&mut sink).await;
    }
}

/// A hook loop serving one agent whose generation's observer runs over a
/// temporary instance.
struct Deck {
    dir: tempfile::TempDir,
    sock: PathBuf,
    registry: Arc<AgentPtyRegistry>,
    observer: Arc<Observer>,
    instance: Instance,
    generation: u64,
    token: String,
    handle: tokio::task::JoinHandle<Result<(), DaemonError>>,
    started: Vec<Child>,
}

impl Deck {
    async fn start() -> Deck {
        Deck::start_with(false).await
    }

    /// A deck whose observer binds `A` to this test process (Q87): its
    /// launch is the process's own argv, its release latched with an
    /// editor-probe record, and its `TMPDIR` the deck's directory, so the
    /// permission settles `Armable` and a liability can be published.
    async fn start_bound() -> Deck {
        Deck::start_with(true).await
    }

    async fn start_with(bound: bool) -> Deck {
        let registry = Arc::new(AgentPtyRegistry::new());
        let id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn shell agent");
        let token = registry.hook_token_of(&id).expect("a hook token");
        let generation: u64 = id.parse().expect("a numeric agent id");

        let dir = tempfile::Builder::new()
            .prefix("composer-reader")
            .rand_bytes(8)
            .tempdir_in(std::env::temp_dir())
            .unwrap();
        assert!(splittable(dir.path()), "{}", dir.path().display());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            open_root(dir.path()).is_ok(),
            "{} passes through a directory another user could change: point TMPDIR at one \
             that doesn't",
            dir.path().display()
        );
        let me = std::process::id();
        let instance = Instance::create(
            &dir.path().join("state").join("editor"),
            DaemonId {
                pid: me,
                start: start_of(me),
                boot: "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0".to_string(),
                nonce: "00112233445566778899aabbccddeeff".to_string(),
            },
            &mut std::fs::File::open("/bin/sh").unwrap(),
        )
        .unwrap();
        let mut installation = install(
            &instance,
            PaneClass::Direct(ProbeAgent::Claude),
            &id,
            &Originals::default(),
        )
        .unwrap();
        let spawned = if bound {
            let mut argv = std::fs::read(format!("/proc/{me}/cmdline")).unwrap();
            if argv.last() == Some(&0) {
                argv.pop();
            }
            let launch: Vec<std::ffi::OsString> = argv
                .split(|b| *b == 0)
                .map(|arg| std::ffi::OsStr::from_bytes(arg).to_os_string())
                .collect();
            assert!(
                std::path::Path::new(&launch[0]).is_absolute(),
                "the test binary runs by an absolute path: {launch:?}"
            );
            installation.set_launch(launch);
            Spawned {
                agent_id: id.clone(),
                agent: Some(AgentType::ClaudeCode),
                env: vec![(
                    "TMPDIR".to_string(),
                    dir.path().to_str().unwrap().to_string(),
                )],
                child: Some((me, start_of(me))),
                input: Arc::new(GenerationInput::default()),
                latch: Box::new(|_| LatchedRelease::Known(release())),
                assumed: assumed(),
            }
        } else {
            installation.set_launch(Vec::new());
            Spawned {
                agent_id: id.clone(),
                agent: Some(AgentType::ClaudeCode),
                env: vec![],
                child: None,
                input: Arc::new(GenerationInput::default()),
                latch: Box::new(|_| LatchedRelease::Unseen),
                assumed: &[],
            }
        };
        let observer =
            Observer::start(installation, spawned, &Arc::new(std::sync::OnceLock::new()))
                .expect("observer");
        registry.set_composer_observer_for_test(&id, Arc::clone(&observer));

        let sock = dir.path().join("hook.sock");
        let listener = IpcListener::from_tokio_listener(
            tokio::net::UnixListener::bind(&sock).expect("bind hook socket"),
        );
        let state: SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        state.write().await.register_pane(PANE.to_string());
        let (event_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let handle = tokio::spawn({
            let registry = Arc::clone(&registry);
            async move {
                run_hook_loop(
                    listener,
                    state,
                    event_tx,
                    registry,
                    Arc::new(Notify::new()),
                    crate::issue_dispatch_run::new_worktree_registry(),
                )
                .await
            }
        });
        Deck {
            dir,
            sock,
            registry,
            observer,
            instance,
            generation,
            token,
            handle,
            started: Vec::new(),
        }
    }

    /// A live helper copy blocked reading a FIFO, with `connections`
    /// connections to the hook socket that it made itself before its
    /// `exec` and that the test holds.
    async fn helper(&mut self, name: &str, connections: usize) -> (u32, Vec<Peer>) {
        self.helper_with(name, connections, false).await
    }

    /// [`Deck::helper`] whose last argument is a Claude export under the
    /// deck's `TMPDIR`, so a bound deck's scan attributes it `Owned`.
    async fn owned_helper(&mut self, name: &str, connections: usize) -> (u32, Vec<Peer>) {
        self.helper_with(name, connections, true).await
    }

    async fn helper_with(
        &mut self,
        name: &str,
        connections: usize,
        export: bool,
    ) -> (u32, Vec<Peer>) {
        let fifo = self.dir.path().join(name);
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let sockets: Vec<OwnedFd> = (0..connections)
            .map(|_| {
                // SAFETY: `socket(2)`; a non-negative result is a new
                // descriptor we own.
                let fd = unsafe {
                    libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0)
                };
                assert!(fd >= 0);
                // SAFETY: just opened, owned by nothing else.
                unsafe { OwnedFd::from_raw_fd(fd) }
            })
            .collect();
        let raw: Vec<i32> = sockets.iter().map(AsRawFd::as_raw_fd).collect();
        // SAFETY: all-zero is a valid `sockaddr_un`.
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let path = self.sock.as_os_str().as_encoded_bytes();
        assert!(path.len() < addr.sun_path.len());
        for (dst, src) in addr.sun_path.iter_mut().zip(path) {
            *dst = *src as libc::c_char;
        }
        let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        let mut command = Command::new(self.instance.helper());
        command
            .args(["-c", "read x < \"$0\""])
            .arg(&fifo)
            .stdin(Stdio::null());
        if export {
            // SAFETY: `getuid(2)` has no failure mode.
            let uid = unsafe { libc::getuid() };
            command.arg(
                self.dir
                    .path()
                    .join(format!("claude-{uid}"))
                    .join(format!("claude-prompt-{name}.md")),
            );
        }
        // SAFETY: `connect(2)` is async-signal-safe, and the closure only
        // reads what it owns. The descriptors are the parent's, inherited
        // across the fork and closed by the `exec`: the connection is the
        // child's by `SO_PEERPIDFD`, and the parent's to read and write.
        unsafe {
            command.pre_exec(move || {
                for fd in &raw {
                    let rc = libc::connect(*fd, (&raw const addr).cast::<libc::sockaddr>(), len);
                    if rc != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let pid = child.id();
        self.started.push(child);
        let program = std::fs::canonicalize(self.instance.helper()).unwrap();
        assert!(
            wait_for(SETTLE, || std::fs::read_link(format!("/proc/{pid}/exe"))
                .is_ok_and(|exe| exe == program))
            .await
        );
        let peers = sockets
            .into_iter()
            .map(|fd| {
                let std = std::os::unix::net::UnixStream::from(fd);
                std.set_nonblocking(true).unwrap();
                let (read, writer) = tokio::net::UnixStream::from_std(std).unwrap().into_split();
                Peer {
                    reader: tokio::io::BufReader::new(read),
                    writer,
                }
            })
            .collect();
        (pid, peers)
    }

    /// Ask the capability question on `peer`: the composer probe is
    /// offered, so the connection is registered.
    async fn register(&self, peer: &mut Peer) {
        let ask = DaemonMessage::HookCapabilities(HookCapabilitiesRequest {
            pane_id: PANE.to_string(),
            token: Some(self.token.clone()),
        });
        peer.send(format!("{}\n", serde_json::to_string(&ask).unwrap()).as_bytes())
            .await;
        let reply: HookCapabilitiesReply = serde_json::from_str(&peer.reply().await).unwrap();
        assert!(reply.composer_probe.is_some(), "{reply:?}");
    }

    fn request(&self, decision: ComposerDecision) -> ComposerInvocationRequest {
        ComposerInvocationRequest {
            pane_id: PANE.to_string(),
            token: Some(self.token.clone()),
            generation: self.generation,
            decision,
            claim: Some(ComposerClaim::Owned),
            seg_seen: Some(0),
            native_seen: Some(0),
            argv: vec!["/tmp/claude-1000/claude-prompt-1.md".into()],
        }
    }

    fn invocation(&self, decision: ComposerDecision) -> String {
        let request = DaemonMessage::ComposerInvocation(self.request(decision));
        format!("{}\n", serde_json::to_string(&request).unwrap())
    }

    /// An `Unarmed` invocation on `peer`, answered: its record made and its
    /// connection now reading the report.
    async fn unarmed(&self, peer: &mut Peer, key: RecordKey) {
        peer.send(self.invocation(ComposerDecision::Unarmed).as_bytes())
            .await;
        let reply: ComposerInvocationReply = serde_json::from_str(&peer.reply().await).unwrap();
        assert!(reply.outcome.is_some(), "{reply:?}");
        let input = self.observer.input();
        assert_eq!(
            input.record(key).and_then(|r| r.decision),
            Some(Decision::Unarmed)
        );
    }

    fn kill(&mut self, pid: u32) {
        let child = self.started.iter_mut().find(|c| c.id() == pid).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// Whether `key`'s exit drained to the uncertain outcome: drained, and
    /// kept rather than pruned, while its exit facts are there either way.
    fn uncertain(&self, key: RecordKey) -> bool {
        self.observer.input().record(key).is_some_and(|r| r.drained)
    }

    /// Whether `key`'s exit drained to the ordinary settlement: its record
    /// pruned, its exit facts kept.
    fn settled(&self, key: RecordKey) -> bool {
        self.observer.input().record(key).is_none() && self.observer.exit_fact(key).is_some()
    }

    /// `key`'s record once the bound deck's scan attributed it `Owned`,
    /// with no decision.
    async fn scanned_owned(&self, key: RecordKey) {
        let input = self.observer.input();
        assert!(
            wait_for(SETTLE, || input.record(key).is_some()).await,
            "the scan records the helper"
        );
        let record = input.record(key).unwrap();
        assert_eq!(record.attribution, Attribution::Owned, "{record:?}");
        assert_eq!(record.decision, None);
    }

    /// Ends the hook loop; [`Deck`]'s drop does the rest.
    async fn stop(mut self) {
        self.handle.abort();
        let _ = (&mut self.handle).await;
    }
}

impl Drop for Deck {
    /// Panic-safe (Q87): the observer stopped, every process the test
    /// started killed and reaped, the hook loop ended and the agent gone,
    /// whether the test passed or not.
    fn drop(&mut self) {
        self.observer.stop();
        for child in &mut self.started {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.handle.abort();
        self.registry.shutdown_all();
    }
}

/// Scenario: A helper copy asks the daemon's hook socket for the composer capability, so its connection is registered, and the generation's other seven connections are taken, so an eighth is refused. The helper is killed with no record, while the test, still holding its descriptor, sends a short line every 50 ms: the daemon closes the connection within its read deadline of the exit, whatever keeps arriving, and the freed slot admits a connection again.
#[spec("inbox/probe/074")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_074_a_holder_sending_lines_cannot_keep_a_departed_connection() {
    let mut deck = Deck::start().await;
    let (helper, mut peers) = deck.helper("holder", 1).await;
    let mut peer = peers.pop().unwrap();
    deck.register(&mut peer).await;
    let (other, _) = deck.helper("other", 0).await;
    let taken: Vec<_> = (1..CONNECTIONS_PER_GENERATION)
        .map(|_| {
            deck.observer
                .connect(Ok(Some(pidfd(other))))
                .expect("within the generation's budget")
        })
        .collect();
    assert!(
        deck.observer.connect(Ok(Some(pidfd(other)))).is_none(),
        "the generation's budget is full"
    );

    deck.kill(helper);
    let exited = Instant::now();
    let Peer {
        mut reader,
        mut writer,
    } = peer;
    let holder = tokio::spawn(async move {
        loop {
            if writer
                .write_all(b"{\"not\":\"a message\"}\n")
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    let mut sink = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), reader.read_to_end(&mut sink))
        .await
        .expect("the daemon closes the departed connection however long the holder sends")
        .ok();
    let open_for = exited.elapsed();
    assert!(
        open_for < READ_DEADLINE + Duration::from_secs(1),
        "closed {open_for:?} after the exit: the read deadline, plus the observer's pass"
    );
    assert!(
        wait_for(SETTLE, || deck
            .observer
            .connect(Ok(Some(pidfd(other))))
            .is_some())
        .await,
        "the departed connection's slot is given back"
    );
    holder.abort();
    drop(taken);
    deck.stop().await;
}

/// Scenario: A helper copy opens two registered connections and declares `Unarmed` on the first, which then waits for its report; on the second it sends only the first half of another invocation line, and is killed. The daemon never completes or reparses that half line: the second connection ends at its read deadline holding it, and the helper's exit drains only to the uncertain outcome, its record kept, never to the ordinary settlement.
#[spec("inbox/probe/074")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_074_a_line_cut_at_the_deadline_drains_uncertain() {
    let mut deck = Deck::start().await;
    let (helper, mut peers) = deck.helper("cut", 2).await;
    let key = key_of(helper);
    let mut cut = peers.pop().unwrap();
    let mut first = peers.pop().unwrap();
    deck.register(&mut first).await;
    deck.register(&mut cut).await;
    deck.unarmed(&mut first, key).await;
    let line = deck.invocation(ComposerDecision::Unarmed);
    cut.send(&line.as_bytes()[..line.len() / 2]).await;

    deck.kill(helper);
    tokio::time::timeout(SETTLE, cut.closed())
        .await
        .expect("closed at its read deadline");
    tokio::time::timeout(SETTLE, first.closed())
        .await
        .expect("closed at its read deadline");
    assert!(
        wait_for(SETTLE, || deck.uncertain(key)).await,
        "a connection that ended holding part of a line drains uncertain"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(deck.uncertain(key), "kept, never pruned as settled");
    let fact = deck.observer.exit_fact(key).unwrap();
    assert_eq!(fact.decision, Some(Decision::Unarmed));
    // The rest of the line, sent after the connection ended, reaches
    // nothing.
    let _ = cut
        .writer
        .write_all(&line.as_bytes()[line.len() / 2..])
        .await;
    deck.stop().await;
}

/// Scenario: A helper copy declares `Unarmed` and then sends its whole `editor_done` report before it is killed: the report is applied before the connection closes, and the exit settles in the ordinary way with the report in its exit facts. The same with only half of the report sent: the connection ends at its read deadline holding the half line, the report isn't applied, and the exit drains only to the uncertain outcome.
#[spec("inbox/probe/074")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_074_a_report_whole_is_applied_and_one_cut_drains_uncertain() {
    let mut deck = Deck::start().await;
    let report = b"{\"report\":\"editor_done\",\"tree\":\"clean\"}\n";

    let (whole, mut peers) = deck.helper("whole", 1).await;
    let whole_key = key_of(whole);
    let mut peer = peers.pop().unwrap();
    deck.register(&mut peer).await;
    deck.unarmed(&mut peer, whole_key).await;
    peer.send(report).await;
    deck.kill(whole);
    tokio::time::timeout(SETTLE, peer.closed())
        .await
        .expect("closed");
    assert!(
        wait_for(SETTLE, || deck.settled(whole_key)).await,
        "a report read whole is applied and the exit settles"
    );
    let fact = deck.observer.exit_fact(whole_key).unwrap();
    assert_eq!(fact.report, Report::Done(Tree::Clean));
    assert_eq!(fact.decision, Some(Decision::Unarmed));

    let (cut, mut peers) = deck.helper("cut", 1).await;
    let cut_key = key_of(cut);
    let mut peer = peers.pop().unwrap();
    deck.register(&mut peer).await;
    deck.unarmed(&mut peer, cut_key).await;
    peer.send(&report[..report.len() / 2]).await;
    deck.kill(cut);
    tokio::time::timeout(SETTLE, peer.closed())
        .await
        .expect("closed at its read deadline");
    assert!(
        wait_for(SETTLE, || deck.uncertain(cut_key)).await,
        "a report cut at the deadline drains uncertain"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(deck.uncertain(cut_key), "kept, never pruned as settled");
    assert_eq!(
        deck.observer.exit_fact(cut_key).unwrap().report,
        Report::None
    );
    deck.stop().await;
}

/// Scenario: A helper copy declares `Unarmed` and is killed without sending anything more, its connection held open by the test: the connection closes no sooner than the read deadline after the instant the observer recorded the exit, and the exit settles in the ordinary way, its record pruned and its exit facts kept with no report.
#[spec("inbox/probe/074")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_074_a_quiet_connection_closes_at_its_deadline_and_settles() {
    let mut deck = Deck::start().await;
    let (helper, mut peers) = deck.helper("quiet", 1).await;
    let key = key_of(helper);
    let mut peer = peers.pop().unwrap();
    deck.register(&mut peer).await;
    deck.unarmed(&mut peer, key).await;
    deck.kill(helper);
    tokio::time::timeout(SETTLE, peer.closed())
        .await
        .expect("closed at its read deadline");
    let closed = Instant::now();
    assert!(
        wait_for(SETTLE, || deck.settled(key)).await,
        "a quiet connection's exit settles"
    );
    let fact = deck.observer.exit_fact(key).unwrap();
    // The observer marks the departure after it records the exit, so the
    // deadline is no sooner than this.
    assert!(
        closed >= fact.exit_observed + READ_DEADLINE,
        "closed {:?} after the observed exit, before the read deadline",
        closed.saturating_duration_since(fact.exit_observed)
    );
    assert_eq!(fact.report, Report::None);
    assert_eq!(fact.decision, Some(Decision::Unarmed));
    deck.stop().await;
}

/// Scenario: A helper copy declares `Unarmed`, sends half of its `editor_done` report and closes its end of the connection, so the daemon reads the half line to the end and can't parse it; the helper is then killed. The report isn't applied, and the exit drains only to the uncertain outcome, never the ordinary settlement.
#[spec("inbox/probe/074")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_074_a_report_ended_unparsed_drains_uncertain() {
    let mut deck = Deck::start().await;
    let report = b"{\"report\":\"editor_done\",\"tree\":\"clean\"}\n";
    let (helper, mut peers) = deck.helper("ended", 1).await;
    let key = key_of(helper);
    let mut peer = peers.pop().unwrap();
    deck.register(&mut peer).await;
    deck.unarmed(&mut peer, key).await;
    peer.send(&report[..report.len() / 2]).await;
    peer.writer.shutdown().await.unwrap();
    tokio::time::timeout(SETTLE, peer.closed())
        .await
        .expect("closed at the end");
    deck.kill(helper);
    assert!(
        wait_for(SETTLE, || deck.uncertain(key)).await,
        "a report the daemon couldn't parse drains uncertain"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(deck.uncertain(key), "kept, never pruned as settled");
    assert_eq!(deck.observer.exit_fact(key).unwrap().report, Report::None);
    deck.stop().await;
}

/// Scenario: A helper copy declares `Unarmed` on one registered connection and, on a second, sends more than the hook socket's line cap with no newline: the daemon refuses the line and drops that connection. Once the helper is killed its exit drains only to the uncertain outcome, since what the refused line said is unknown.
#[spec("inbox/probe/074")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_074_a_refused_line_drains_uncertain() {
    let mut deck = Deck::start().await;
    let (helper, mut peers) = deck.helper("refused", 2).await;
    let key = key_of(helper);
    let mut long = peers.pop().unwrap();
    let mut first = peers.pop().unwrap();
    deck.register(&mut first).await;
    deck.register(&mut long).await;
    deck.unarmed(&mut first, key).await;
    let Peer {
        reader: mut long_reader,
        writer: mut long_writer,
    } = long;
    let writer = tokio::spawn(async move {
        let chunk = vec![b'x'; 64 << 10];
        let mut sent = 0;
        while sent <= crate::bounded_read::MAX_HOOK_LINE_BYTES {
            if long_writer.write_all(&chunk).await.is_err() {
                return;
            }
            sent += chunk.len();
        }
    });
    let mut sink = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), long_reader.read_to_end(&mut sink))
        .await
        .expect("the over-long line's connection is dropped")
        .ok();
    let _ = writer.await;
    deck.kill(helper);
    assert!(
        wait_for(SETTLE, || deck.uncertain(key)).await,
        "a connection that refused a line drains uncertain"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(deck.uncertain(key), "kept, never pruned as settled");
    deck.stop().await;
}

/// Scenario: A live helper copy with no record gets two connections registered by its pidfd: the first is admitted watched, by its own hold on the helper, and the second shares that hold. The first ends with bytes unread and both close; the helper still lives, so across the observer's passes its lost mark is kept, and a third connection shares the mark's hold. The `Unarmed` invocation made on it creates the record, which inherits the earlier loss: once the helper is killed its exit drains only to the uncertain outcome. The control helper, with the same steps and no loss, settles in the ordinary way.
#[spec("inbox/probe/074")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_074_a_loss_before_its_record_is_kept_and_inherited() {
    let mut deck = Deck::start().await;
    let (helper, _) = deck.helper("lost", 0).await;
    let key = key_of(helper);
    let first = deck
        .observer
        .connect(Ok(Some(pidfd(helper))))
        .expect("admitted");
    assert_eq!(
        first.held_for_test(),
        HeldForTest {
            peer: Some(true),
            lost: None,
            pin: None
        },
        "admitted watched, by its own hold"
    );
    let second = deck
        .observer
        .connect(Ok(Some(pidfd(helper))))
        .expect("admitted");
    assert_eq!(
        second.held_for_test().peer,
        Some(true),
        "one hold per helper"
    );
    first.lose();
    assert_eq!(
        second.held_for_test().lost,
        Some(true),
        "the mark keeps that hold"
    );
    drop(first);
    drop(second);
    // Past the pass each close rings and an idle sweep: alive, so kept.
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    assert!(
        deck.observer.lost_mark_for_test(key),
        "a live helper's loss is never forgotten"
    );
    let third = deck
        .observer
        .connect(Ok(Some(pidfd(helper))))
        .expect("admitted");
    assert_eq!(
        third.held_for_test(),
        HeldForTest {
            peer: Some(true),
            lost: Some(true),
            pin: None
        },
        "a later connection shares the mark's hold"
    );
    let observer = Arc::clone(&deck.observer);
    let request = deck.request(ComposerDecision::Unarmed);
    let third = tokio::task::spawn_blocking(move || {
        observer.invocation(&third, &request);
        third
    })
    .await
    .unwrap();
    assert_eq!(
        deck.observer.input().record(key).and_then(|r| r.decision),
        Some(Decision::Unarmed)
    );
    assert_eq!(
        third.held_for_test().pin,
        Some(true),
        "its pin shares it too"
    );
    drop(third);
    deck.kill(helper);
    assert!(
        wait_for(SETTLE, || deck.uncertain(key)).await,
        "the record made after the loss inherits it"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(deck.uncertain(key), "kept, never pruned as settled");

    let (control, _) = deck.helper("control", 0).await;
    let control_key = key_of(control);
    let conn = deck
        .observer
        .connect(Ok(Some(pidfd(control))))
        .expect("admitted");
    let observer = Arc::clone(&deck.observer);
    let request = deck.request(ComposerDecision::Unarmed);
    tokio::task::spawn_blocking(move || {
        observer.invocation(&conn, &request);
    })
    .await
    .unwrap();
    deck.kill(control);
    assert!(
        wait_for(SETTLE, || deck.settled(control_key)).await,
        "with no loss the exit settles"
    );
    deck.stop().await;
}

/// Scenario: With `A` bound and nothing published, more live helper copies than a generation keeps lost marks for each get a connection that ends with bytes unread: the marks and their pool stop at the bound and the generation's loss overflow latches. Killed, the helpers' marks go and give their units back, and the overflow stays. A liability is then published, and a helper copy born after it, recorded `Owned` with no decision by the scan, is killed with no loss of its own: its exit drains only to the uncertain outcome and the liability stays, never consumed by the exit alone.
#[spec("inbox/probe/074")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_074_lost_marks_are_bounded_and_overflow_drains_uncertain() {
    let mut deck = Deck::start_bound().await;
    assert!(
        wait_for(SETTLE, || deck.observer.permission() == Permission::Armable).await,
        "{:?}",
        deck.observer.permission()
    );
    let mut cycled = Vec::new();
    for i in 0..=LOST_PER_GENERATION {
        let (pid, _) = deck.helper(&format!("lost-{i}"), 0).await;
        let conn = deck
            .observer
            .connect(Ok(Some(pidfd(pid))))
            .expect("admitted");
        conn.lose();
        drop(conn);
        cycled.push(pid);
    }
    assert_eq!(
        deck.observer.lost_for_test(),
        (LOST_PER_GENERATION as usize, LOST_PER_GENERATION, true),
        "bounded, and the loss past the bound latched"
    );
    let input = deck.observer.input();
    assert!(
        cycled
            .iter()
            .all(|pid| input.record(key_of(*pid)).is_none())
    );
    for pid in cycled {
        deck.kill(pid);
    }
    assert!(
        wait_for(SETTLE, || deck.observer.lost_for_test() == (0, 0, true)).await,
        "exited with no record, their marks go; the overflow stays: {:?}",
        deck.observer.lost_for_test()
    );

    assert_eq!(
        deck.observer.control(deck.generation, ControlOp::Publish),
        Ok(1)
    );
    // Born after the publication, past its start ticks' granularity.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (helper, _) = deck.owned_helper("after", 0).await;
    let key = key_of(helper);
    deck.scanned_owned(key).await;
    assert!(!deck.observer.lost_mark_for_test(key));
    deck.kill(helper);
    assert!(
        wait_for(SETTLE, || deck.uncertain(key)).await,
        "an overflowed generation drains uncertain"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let record = input.record(key).expect("kept");
    assert_eq!(
        record.settlement,
        Settlement::NotConsumed(NotConsumed::Unproven)
    );
    assert_eq!(
        input.liability_count(),
        Some(1),
        "never consumed by the exit alone"
    );
    deck.stop().await;
}

/// Scenario: With `A` bound and a liability published, a helper copy born after it is recorded `Owned` with no decision by the scan; on its registered connection it sends half of an invocation line and closes its end, so the daemon reads the half line to the end and can't parse it. Once the helper is killed its exit drains only to the uncertain outcome and the liability stays. The control helper, born after the same publication, closes its registered connection with nothing sent: its exit drains in the ordinary way and consumes the liability.
#[spec("inbox/probe/074")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_074_half_an_invocation_ended_by_the_close_drains_uncertain() {
    let mut deck = Deck::start_bound().await;
    assert!(
        wait_for(SETTLE, || deck.observer.permission() == Permission::Armable).await,
        "{:?}",
        deck.observer.permission()
    );
    assert_eq!(
        deck.observer.control(deck.generation, ControlOp::Publish),
        Ok(1)
    );
    let input = deck.observer.input();
    // Born after the publication, past its start ticks' granularity.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (helper, mut peers) = deck.owned_helper("half", 1).await;
    let key = key_of(helper);
    let mut peer = peers.pop().unwrap();
    deck.scanned_owned(key).await;
    deck.register(&mut peer).await;
    let line = deck.invocation(ComposerDecision::Unarmed);
    peer.send(&line.as_bytes()[..line.len() / 2]).await;
    peer.writer.shutdown().await.unwrap();
    tokio::time::timeout(SETTLE, peer.closed())
        .await
        .expect("closed at the end");
    deck.kill(helper);
    assert!(
        wait_for(SETTLE, || deck.uncertain(key)).await,
        "half an invocation read to the end drains uncertain"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let record = input.record(key).expect("kept");
    assert_eq!(record.decision, None);
    assert_eq!(
        record.settlement,
        Settlement::NotConsumed(NotConsumed::Unproven)
    );
    assert_eq!(
        input.liability_count(),
        Some(1),
        "never consumed by the exit alone"
    );

    let (clean, mut peers) = deck.owned_helper("clean", 1).await;
    let clean_key = key_of(clean);
    let mut peer = peers.pop().unwrap();
    deck.scanned_owned(clean_key).await;
    deck.register(&mut peer).await;
    peer.writer.shutdown().await.unwrap();
    tokio::time::timeout(SETTLE, peer.closed())
        .await
        .expect("closed at the end");
    deck.kill(clean);
    assert!(
        wait_for(SETTLE, || deck
            .observer
            .exit_fact(clean_key)
            .is_some_and(|f| f.settlement == Settlement::Consumed))
        .await,
        "a clean close settles by the exit and consumes: {:?}",
        input.record(clean_key)
    );
    assert_eq!(input.liability_count(), Some(0));
    deck.stop().await;
}

/// Where a test parks the observer's drain of one record (Q91): the drain
/// blocks there, between its snapshot and its settlement, until released.
/// Released when dropped too, so a failing test never leaves the observer's
/// thread parked.
#[derive(Default)]
struct DrainGate {
    /// Parked, released.
    state: std::sync::Mutex<(bool, bool)>,
    changed: std::sync::Condvar,
}

impl DrainGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, (bool, bool)> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The observer's side: wait here until released.
    fn park(&self) {
        let mut state = self.lock();
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn parked(&self) -> bool {
        self.lock().0
    }

    fn release(&self) {
        self.lock().1 = true;
        self.changed.notify_all();
    }
}

/// Releases its gate when dropped.
struct Released(Arc<DrainGate>);

impl Drop for Released {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Units of the daemon's total lost pool a test took, given back when
/// dropped. The pool is a process static: nextest runs each test in its own
/// process, so no other test sees it full.
struct TakenFromTotal(u32);

impl Drop for TakenFromTotal {
    fn drop(&mut self) {
        for _ in 0..self.0 {
            LOST.give();
        }
    }
}

/// What latches the generation's loss overflow while a drain is parked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Latch {
    Nothing,
    /// The generation's lost pool is full.
    Generation,
    /// The daemon's total lost pool is full.
    Total,
}

/// A decisionless `Owned` helper born after a liability's publication is
/// killed, and its drain parked between its snapshot and its settlement
/// (Q91); `latch` then latches the generation's loss overflow, or doesn't,
/// through another helper's connection ending with bytes unread, and the
/// drain is released. With the overflow latched the exit drains only to
/// the uncertain outcome and the liability stays; without it, it settles
/// and consumes.
async fn overflow_while_a_drain_is_parked(latch: Latch) {
    let mut deck = Deck::start_bound().await;
    assert!(
        wait_for(SETTLE, || deck.observer.permission() == Permission::Armable).await,
        "{:?}",
        deck.observer.permission()
    );
    // Before the publication, so no scan records them: live helpers holding
    // every lost mark the generation keeps, and one more whose connection
    // stays open to lose its bytes later.
    if latch == Latch::Generation {
        for i in 0..LOST_PER_GENERATION {
            let (pid, _) = deck.helper(&format!("marked-{i}"), 0).await;
            let conn = deck
                .observer
                .connect(Ok(Some(pidfd(pid))))
                .expect("admitted");
            conn.lose();
        }
        assert_eq!(
            deck.observer.lost_for_test(),
            (LOST_PER_GENERATION as usize, LOST_PER_GENERATION, false),
            "the generation's pool full, and nothing latched yet"
        );
    }
    let (loser, _) = deck.helper("loser", 0).await;
    let late = deck
        .observer
        .connect(Ok(Some(pidfd(loser))))
        .expect("admitted");

    assert_eq!(
        deck.observer.control(deck.generation, ControlOp::Publish),
        Ok(1)
    );
    let input = deck.observer.input();
    // Born after the publication, past its start ticks' granularity.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (helper, _) = deck.owned_helper("parked", 0).await;
    let key = key_of(helper);
    deck.scanned_owned(key).await;
    let gate = Arc::new(DrainGate::default());
    let _released = Released(Arc::clone(&gate));
    deck.observer.park_drains_for_test({
        let gate = Arc::clone(&gate);
        move |drained| {
            if drained == key {
                gate.park();
            }
        }
    });
    deck.kill(helper);
    assert!(
        wait_for(SETTLE, || gate.parked()).await,
        "the drain reaches its settlement"
    );
    assert!(
        !deck.observer.lost_for_test().2,
        "nothing latched when the drain took its snapshot"
    );
    match latch {
        Latch::Nothing => {}
        Latch::Generation => late.lose(),
        Latch::Total => {
            let mut taken = TakenFromTotal(0);
            while LOST.take() {
                taken.0 += 1;
            }
            late.lose();
            assert_eq!(
                deck.observer.lost_for_test().1,
                0,
                "the generation's unit given back when the total refused"
            );
        }
    }
    assert_eq!(
        deck.observer.lost_for_test().2,
        latch != Latch::Nothing,
        "{latch:?}"
    );
    gate.release();

    if latch == Latch::Nothing {
        assert!(
            wait_for(SETTLE, || deck
                .observer
                .exit_fact(key)
                .is_some_and(|f| f.settlement == Settlement::Consumed))
            .await,
            "with no overflow the parked drain settles and consumes: {:?}",
            input.record(key)
        );
        assert_eq!(input.liability_count(), Some(0));
    } else {
        assert!(
            wait_for(SETTLE, || deck.uncertain(key)).await,
            "an overflow latched before the settlement drains uncertain: {:?}",
            input.record(key)
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let record = input.record(key).expect("kept");
        assert_eq!(
            record.settlement,
            Settlement::NotConsumed(NotConsumed::Unproven)
        );
        assert_eq!(
            input.liability_count(),
            Some(1),
            "never consumed by the exit alone"
        );
    }
    drop(late);
    deck.stop().await;
}

/// Scenario: With `A` bound, the generation's lost pool filled by live helpers' marks and a liability published, a decisionless `Owned` helper born after the publication is killed and its drain is held between its snapshot and its settlement. Meanwhile another helper's connection ends with bytes unread, finds no lost mark left and latches the generation's loss overflow; released, the drain settles only to the uncertain outcome and the liability stays.
#[spec("inbox/probe/074")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_074_overflow_latched_by_the_generation_pool_during_a_drain_holds_it_uncertain() {
    overflow_while_a_drain_is_parked(Latch::Generation).await;
}

/// Scenario: With `A` bound and a liability published, a decisionless `Owned` helper born after the publication is killed and its drain is held between its snapshot and its settlement. Meanwhile the daemon's total lost pool is taken whole, as other generations' marks would, and another helper's connection ends with bytes unread: the total refuses it a mark, the generation's unit comes back, and its loss overflow latches. Released, the drain settles only to the uncertain outcome and the liability stays.
#[spec("inbox/probe/074")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_074_overflow_latched_by_the_total_pool_during_a_drain_holds_it_uncertain() {
    overflow_while_a_drain_is_parked(Latch::Total).await;
}

/// Scenario: The control for the two above: the same decisionless `Owned` helper's drain is held between its snapshot and its settlement and released with nothing lost meanwhile; it settles by its exit and consumes the liability.
#[spec("inbox/probe/074")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_074_a_drain_held_with_no_overflow_settles_and_consumes() {
    overflow_while_a_drain_is_parked(Latch::Nothing).await;
}

/// Scenario: With `A` bound and nothing published, so no scan runs, a live helper copy with no record gets a connection registered by its pidfd, held by its own hold. A liability is then published, and the child scan records the helper and pins it: the pin is the connection's hold, not a second one.
#[spec("inbox/probe/074")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_074_a_scan_after_a_connection_pins_its_hold() {
    let mut deck = Deck::start_bound().await;
    assert!(
        wait_for(SETTLE, || deck.observer.permission() == Permission::Armable).await,
        "{:?}",
        deck.observer.permission()
    );
    let (helper, _) = deck.owned_helper("connected", 0).await;
    let key = key_of(helper);
    let conn = deck
        .observer
        .connect(Ok(Some(pidfd(helper))))
        .expect("admitted");
    assert_eq!(
        conn.held_for_test(),
        HeldForTest {
            peer: Some(true),
            lost: None,
            pin: None
        },
        "admitted watched, by its own hold, before any scan"
    );
    assert_eq!(
        deck.observer.control(deck.generation, ControlOp::Publish),
        Ok(1)
    );
    let input = deck.observer.input();
    assert!(
        wait_for(SETTLE, || input.record(key).is_some()
            && conn.held_for_test().pin.is_some())
        .await,
        "the scan records and pins the helper: {:?}",
        input.record(key)
    );
    assert_eq!(
        conn.held_for_test().pin,
        Some(true),
        "the scan's pin is the connection's hold"
    );
    drop(conn);
    deck.kill(helper);
    deck.stop().await;
}

/// Hold the observer's passes and wait, bounded, for the hold's
/// acknowledgement: no pass running, so none runs until the go.
async fn hold_passes(observer: &Arc<Observer>) -> u64 {
    let observer = Arc::clone(observer);
    let passes = tokio::task::spawn_blocking(move || observer.hold_passes_for_e2e(true, SETTLE))
        .await
        .unwrap()
        .expect("no pass still running at the hold's bound");
    assert!(passes.held && !passes.running, "{passes:?}");
    passes.finished
}

/// Scenario: A bound deck's scan records a live helper copy `Owned` with no decision, and the observer's passes are then held with no pass running. The helper's report arrives on its own registered connection before any decision: it is kept and changes neither the missing decision nor the pending writer scope. In one variant the helper's `unarmed` marker is then read by the next pass, so its decision comes after its report and opens the scope as an unchecked return; in the other no decision ever comes. The helper is killed while passes are held, so its exit is first seen by the pass after the go, which drains it: with no decision the scope ends open as unconfirmed; with the late decision and a clean report it stays an unchecked return.
#[spec("inbox/probe/057")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_057_a_report_before_the_decision_then_the_exit_drain() {
    use crate::composer_probe::helper::marker_name;
    use crate::composer_probe::record::{Cause, ScopeState};
    use crate::event::{EditorReport, EditorTree};
    for later_decision in [false, true] {
        let mut deck = Deck::start_bound().await;
        assert!(
            wait_for(SETTLE, || deck.observer.permission() == Permission::Armable).await,
            "{:?}",
            deck.observer.permission()
        );
        assert_eq!(
            deck.observer.control(deck.generation, ControlOp::Publish),
            Ok(1)
        );
        let input = deck.observer.input();
        // Born after the publication, past its start ticks' granularity.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (helper, _) = deck.owned_helper("early-report", 0).await;
        let key = key_of(helper);
        deck.scanned_owned(key).await;
        assert_eq!(
            input.writer_scope().map(|s| s.state()),
            Some(ScopeState::Pending),
            "an undecided record"
        );

        let held = hold_passes(&deck.observer).await;
        let conn = deck
            .observer
            .connect(Ok(Some(pidfd(helper))))
            .expect("admitted");
        deck.observer.report(
            &conn,
            EditorReport::EditorDone {
                tree: EditorTree::Clean,
            },
        );
        let record = input.record(key).expect("kept");
        assert_eq!(
            record.report,
            Report::Done(Tree::Clean),
            "the report is kept"
        );
        assert_eq!(record.decision, None, "the report is no decision");
        assert_eq!(
            input.writer_scope().map(|s| s.state()),
            Some(ScopeState::Pending),
            "the report before a decision clears nothing"
        );

        let mut quiet = held;
        if later_decision {
            let marker = marker_name(key.pid, key.start, ComposerDecision::Unarmed, None);
            crate::composer_probe::instance::InstanceReader::open(deck.instance.dir())
                .unwrap()
                .create_marker(&deck.generation.to_string(), &marker)
                .unwrap();
            assert_eq!(
                deck.observer.passes_for_e2e().finished,
                held,
                "no pass while held"
            );
            assert_eq!(input.record(key).unwrap().decision, None);
            deck.observer.hold_passes_for_e2e(false, SETTLE).unwrap();
            assert!(
                wait_for(SETTLE, || input
                    .record(key)
                    .is_some_and(|r| r.decision == Some(Decision::Unarmed)))
                .await,
                "the next pass reads the marker's decision"
            );
            assert_eq!(
                input.writer_scope().map(|s| s.state()),
                Some(ScopeState::Open(Cause::EditorReturnUnchecked)),
                "a decision after the report opens the scope as at any decision"
            );
            quiet = hold_passes(&deck.observer).await;
        }

        drop(conn);
        deck.kill(helper);
        assert_eq!(
            deck.observer.passes_for_e2e().finished,
            quiet,
            "no pass while held"
        );
        assert!(
            input.record(key).is_some_and(|r| !r.drained),
            "not drained while held"
        );
        deck.observer.hold_passes_for_e2e(false, SETTLE).unwrap();
        assert!(
            wait_for(SETTLE, || deck.observer.exit_fact(key).is_some()).await,
            "the pass after the go sees the exit and drains it"
        );
        let fact = deck.observer.exit_fact(key).unwrap();
        assert_eq!(fact.report, Report::Done(Tree::Clean));
        let (decision, cause) = if later_decision {
            (Some(Decision::Unarmed), Cause::EditorReturnUnchecked)
        } else {
            (None, Cause::EditorUnconfirmed)
        };
        assert_eq!(fact.decision, decision);
        assert_eq!(
            input.writer_scope().map(|s| s.state()),
            Some(ScopeState::Open(cause)),
            "later_decision={later_decision}: the scope after the drain"
        );
        deck.stop().await;
    }
}
