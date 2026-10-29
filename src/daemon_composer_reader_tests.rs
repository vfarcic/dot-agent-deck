//! PRD #1666 part 3 (Q85): the hook loop's reading of a registered composer
//! connection, on a real hook socket served by [`run_hook_loop`]. An agent
//! spawned by the registry carries an installed generation's observer over a
//! temporary instance whose helper copy is a copy of `/bin/sh`; a "helper" is
//! that copy blocked reading a FIFO, and it `connect`s, before its `exec`, a
//! socket the test created, so the daemon's `SO_PEERPIDFD` names the helper
//! while the test keeps the descriptor: once the helper is killed, the test
//! is the foreign process still holding its connection. Every process a test
//! starts is killed by its captured handle.

use super::*;
use crate::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use crate::composer_probe::epoch::GenerationInput;
use crate::composer_probe::install::{Originals, PaneClass, ProbeAgent, install};
use crate::composer_probe::instance::{DaemonId, Instance, open_root, splittable};
use crate::composer_probe::observe::{
    CONNECTIONS_PER_GENERATION, Observer, READ_DEADLINE, Spawned,
};
use crate::composer_probe::record::{Decision, RecordKey, Report, Tree};
use crate::event::{
    AgentType, ComposerClaim, ComposerDecision, ComposerInvocationReply, ComposerInvocationRequest,
    HookCapabilitiesReply, HookCapabilitiesRequest,
};
use spec::spec;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
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
        installation.set_launch(Vec::new());
        let observer = Observer::start(
            installation,
            Spawned {
                agent_id: id.clone(),
                agent: Some(AgentType::ClaudeCode),
                env: vec![],
                child: None,
                input: Arc::new(GenerationInput::default()),
                latch: Box::new(|_| crate::doorbell_activation::LatchedRelease::Unseen),
                assumed: &[],
            },
            &Arc::new(std::sync::OnceLock::new()),
        )
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

    fn invocation(&self, decision: ComposerDecision) -> String {
        let request = DaemonMessage::ComposerInvocation(ComposerInvocationRequest {
            pane_id: PANE.to_string(),
            token: Some(self.token.clone()),
            generation: self.generation,
            decision,
            claim: Some(ComposerClaim::Owned),
            seg_seen: Some(0),
            native_seen: Some(0),
            argv: vec!["/tmp/claude-1000/claude-prompt-1.md".into()],
        });
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

    async fn stop(mut self) {
        self.observer.stop();
        for child in &mut self.started {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.handle.abort();
        let _ = (&mut self.handle).await;
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

/// Scenario: A helper copy declares `Unarmed` and is killed without sending anything more, its connection held open by the test: the connection closes at its read deadline, and the exit settles in the ordinary way, its record pruned and its exit facts kept with no report.
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
    let exited = Instant::now();
    tokio::time::timeout(SETTLE, peer.closed())
        .await
        .expect("closed at its read deadline");
    assert!(
        exited.elapsed() >= READ_DEADLINE,
        "not before the deadline: {:?}",
        exited.elapsed()
    );
    assert!(
        wait_for(SETTLE, || deck.settled(key)).await,
        "a quiet connection's exit settles"
    );
    let fact = deck.observer.exit_fact(key).unwrap();
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
