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

/// The release a bound deck's latch reports, the one the e2e stand-ins draw
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

/// How a [`Deck`] starts: whether its observer binds `A`, whether its
/// agent's `HOME` is the deck's own, whether its release is latched from
/// the start, and the bound agent's own environment when it isn't the
/// spawn's (names to paths under the deck's directory).
struct DeckStart {
    bound: bool,
    home: bool,
    latched: bool,
    agent_env: Option<Vec<(String, String)>>,
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
    /// A bound deck's release latch reports the release once this is set,
    /// and nothing before, so the permission waits (set at the start
    /// unless the deck was started with its latch held back).
    latched: Arc<std::sync::atomic::AtomicBool>,
}

impl Deck {
    async fn start() -> Deck {
        Deck::start_with_bound(false).await
    }

    /// A deck whose observer binds `A` to this test process (Q87): its
    /// launch is the process's own argv, its release latched with an
    /// editor-probe record, and its `TMPDIR` the deck's directory, so the
    /// permission settles `Armable` and a liability can be published.
    async fn start_bound() -> Deck {
        Deck::start_with_bound(true).await
    }

    /// [`Deck::start_bound`] whose agent's `HOME` is the deck's own
    /// `home` directory, holding an empty Claude `keybindings.json`
    /// ([`Deck::keybindings`]), so the generation's keymap is read from a
    /// file the test owns rather than from the user's own.
    async fn start_bound_at_home() -> Deck {
        Deck::start_with_home(true, true, true).await
    }

    /// [`Deck::start_bound_at_home`] whose bound agent's own environment
    /// is `agent_env` (a name to a path under the deck's directory), not
    /// the `HOME` its spawn gave it: an agent started through a command
    /// that set its own (`HOME=/elsewhere claude`).
    async fn start_bound_at_home_as(agent_env: &[(&str, &str)]) -> Deck {
        Deck::start_with(DeckStart {
            bound: true,
            home: true,
            latched: true,
            agent_env: Some(
                agent_env
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
        })
        .await
    }

    /// [`Deck::start_bound_at_home`] whose release latch reports nothing
    /// until [`Deck::latch`], so its permission stays pending until then.
    async fn start_bound_at_home_latched_later() -> Deck {
        Deck::start_with_home(true, true, false).await
    }

    /// Let a deck started with its latch held back see its release.
    fn latch(&self) {
        self.latched
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    async fn start_with_bound(bound: bool) -> Deck {
        Deck::start_with_home(bound, false, true).await
    }

    /// The Claude `keybindings.json` of a deck started at home.
    fn keybindings(&self) -> PathBuf {
        self.dir.path().join("home/.claude/keybindings.json")
    }

    async fn start_with_home(bound: bool, home: bool, latched: bool) -> Deck {
        Deck::start_with(DeckStart {
            bound,
            home,
            latched,
            agent_env: None,
        })
        .await
    }

    async fn start_with(start: DeckStart) -> Deck {
        let DeckStart {
            bound,
            home,
            latched,
            agent_env,
        } = start;
        let latched = Arc::new(std::sync::atomic::AtomicBool::new(latched));
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
            if home {
                let claude = dir.path().join("home/.claude");
                std::fs::create_dir_all(&claude).unwrap();
                std::fs::write(claude.join("keybindings.json"), "{}").unwrap();
                // What the spawn path reads before the child exists
                // (A-20d-1b), with the agent's own `HOME`, in the working
                // directory the bound agent (this process) runs in: Claude
                // Code's settings-file `env` sources are read from it, and
                // another directory holds as changed (cxfix2 MEDIUM-A).
                //
                // Not hermetic (the revcxfix2 review's NIT-4): that working
                // directory is the checkout, so the deck reads its committed
                // `.claude/settings.json`, any untracked
                // `.claude/settings.local.json`, and `/etc/claude-code`. A
                // local file there that the deck holds on (an `env` naming
                // `VISUAL`, `EDITOR`, `HOME` or `CLAUDE_CONFIG_DIR`, such a
                // member name anywhere, a `__proto__`, or a document it
                // can't parse) turns the tests that read the keymap red with
                // `Keymap(Editor)` or `Keymap(Unmeasured)`: move it aside to
                // run them. The bound agent's cwd is this process's own, and
                // the managed path is fixed in the product, so a seam for
                // either would be a production change made for a test.
                let home_dir = dir.path().join("home");
                let cwd = std::env::current_dir().unwrap();
                installation.set_keymap_baseline(
                    crate::composer_probe::keymap::KeymapBaseline::at_spawn(
                        true,
                        false,
                        |name| (name == "HOME").then(|| home_dir.clone().into_os_string()),
                        &[],
                        &cwd,
                    ),
                );
            }
            let mut env = vec![(
                "TMPDIR".to_string(),
                dir.path().to_str().unwrap().to_string(),
            )];
            if home {
                env.push((
                    "HOME".to_string(),
                    dir.path().join("home").to_str().unwrap().to_string(),
                ));
            }
            // The bound agent is this test process, whose environment is
            // the runner's: what the observer reads as the agent's is the
            // spawn's own, or the one the test names under the deck's
            // directory.
            let mut environ: Vec<(std::ffi::OsString, std::ffi::OsString)> = match &agent_env {
                Some(agent_env) => agent_env
                    .iter()
                    .map(|(k, v)| (k.into(), dir.path().join(v).into_os_string()))
                    .collect(),
                None => env.iter().map(|(k, v)| (k.into(), v.into())).collect(),
            };
            // The installation's `VISUAL` and `EDITOR`, as a directly
            // spawned agent inherits them: the editor its Ctrl+G opens is
            // the helper (the revcxact review's HIGH-1).
            for name in ["VISUAL", "EDITOR"] {
                environ.push((name.into(), instance.helper().as_os_str().to_os_string()));
            }
            installation.set_bound_environ_for_test(environ);
            Spawned {
                agent_id: id.clone(),
                agent: Some(AgentType::ClaudeCode),
                env,
                child: Some((me, start_of(me))),
                input: Arc::new(GenerationInput::default()),
                latch: Box::new({
                    let latched = Arc::clone(&latched);
                    move |_| {
                        if latched.load(std::sync::atomic::Ordering::SeqCst) {
                            LatchedRelease::Known(release())
                        } else {
                            LatchedRelease::Unseen
                        }
                    }
                }),
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
            latched,
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

/// A whole `editor_done` report line of exactly `bytes` bytes before its
/// newline: the JSON padded with spaces, which the parse trims.
fn report_line(bytes: usize) -> Vec<u8> {
    let json = b"{\"report\":\"editor_done\",\"tree\":\"clean\"}";
    let mut line = json.to_vec();
    line.resize(bytes, b' ');
    line.push(b'\n');
    line
}

/// Scenario: Two helper copies each register their connection on the real hook socket and declare `Unarmed`; one sends a whole report line exactly at the report cap, the other a line one byte over it, and both are killed. The report at the cap is applied to the record of the helper the socket's peer names and that exit settles with it, while the line over the cap is refused, so that helper's exit drains only to the uncertain outcome with no report (Q84-2: a report applied through the real registered socket, and the cap that socket read enforces).
#[spec("inbox/probe/074")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_074_a_report_at_the_cap_is_applied_from_its_registered_connection() {
    use crate::composer_probe::observe::REPORT_BYTES;
    let mut deck = Deck::start().await;

    let (at_cap, mut peers) = deck.helper("at-cap", 1).await;
    let at_cap_key = key_of(at_cap);
    let mut at_cap_peer = peers.pop().unwrap();
    let (over, mut peers) = deck.helper("over-cap", 1).await;
    let over_key = key_of(over);
    let mut over_peer = peers.pop().unwrap();
    for (peer, key) in [(&mut at_cap_peer, at_cap_key), (&mut over_peer, over_key)] {
        deck.register(peer).await;
        deck.unarmed(peer, key).await;
    }

    at_cap_peer.send(&report_line(REPORT_BYTES)).await;
    over_peer.send(&report_line(REPORT_BYTES + 1)).await;
    deck.kill(at_cap);
    deck.kill(over);
    for peer in [&mut at_cap_peer, &mut over_peer] {
        tokio::time::timeout(SETTLE, peer.closed())
            .await
            .expect("closed");
    }

    assert!(
        wait_for(SETTLE, || deck.settled(at_cap_key)).await,
        "a report at the cap is applied and the exit settles"
    );
    let fact = deck.observer.exit_fact(at_cap_key).unwrap();
    assert_eq!(fact.report, Report::Done(Tree::Clean));
    assert_eq!(fact.decision, Some(Decision::Unarmed));

    assert!(
        wait_for(SETTLE, || deck.uncertain(over_key)).await,
        "a line over the cap is refused and the exit drains uncertain"
    );
    assert_eq!(
        deck.observer.exit_fact(over_key).unwrap().report,
        Report::None,
        "the refused line is no report"
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

/// Scenario: A bound deck's scan records a live helper copy `Owned` with no decision, the observer's passes are held, and the helper's report arrives on its own registered connection before any decision: it is kept and changes neither the missing decision nor the pending writer scope. In one variant the helper's `unarmed` marker is read by the next pass, so the decision comes after the report and opens the scope as an unchecked return; in the other no decision ever comes. The helper is killed while passes are held and the pass after the go drains it: with no decision the scope ends open as unconfirmed, and with the late decision and a clean report it stays an unchecked return.
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
        // Born after the publication, past its start ticks' granularity: the
        // boot clock has moved into a later tick than the publication's.
        let published = crate::composer_probe::master_holders::boot_ticks().unwrap();
        assert!(
            wait_for(SETTLE, || {
                crate::composer_probe::master_holders::boot_ticks().unwrap() > published
            })
            .await,
            "the boot clock moves a tick"
        );
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

/// Scenario: Record a live helper copy as the child scan does before it can attribute it, so its completion is pending and a native Ctrl+G accepted then waits on a paused quiet clock; write an `armed` marker for it and park the observer's real marker path after it took its stamp, before either lock, while admission reads the hazard FIFO nine seconds later. Released, the marker attributes the helper as not the composer's, so the clock runs again from the read's time, not the marker's earlier stamp: the entry stays through ten seconds of quiet time and leaves only after them, and the record keeps the stamp taken before the park as its first `Armed` sighting.
#[spec("inbox/probe/052")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_052_a_markers_early_stamp_resumes_the_quiet_clock_no_earlier_than_a_racing_read() {
    use crate::composer_probe::helper::marker_name;
    use crate::composer_probe::instance::InstanceReader;
    use crate::composer_probe::observe::scan_observation;
    use crate::composer_probe::query_ledger::EntryView;
    use crate::composer_probe::record::Completion;
    use crate::composer_probe::stamp_park::{StampPoint, park_at};
    let mut deck = Deck::start().await;
    let input = deck.observer.input();
    let (helper, _) = deck.helper("early-stamp", 0).await;
    let key = key_of(helper);

    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    input
        .record_observe(
            &scan_observation(deck.generation, key, Attribution::Pending, t0),
            t0,
        )
        .unwrap();
    assert_eq!(input.record(key).unwrap().completion, Completion::Pending);
    let seq = input.accept_native(t0).expect("a native number");
    assert_eq!(
        input.hazards(at(1)).0,
        vec![EntryView::Quiet { seq, paused: true }],
        "the clock is paused while the record is pending"
    );

    let (park, reached, go) = park_at(&[StampPoint::MarkerStamped]);
    deck.observer.park_stamps_for_test(park);
    InstanceReader::open(deck.instance.dir())
        .unwrap()
        .create_marker(
            &deck.generation.to_string(),
            &marker_name(key.pid, key.start, ComposerDecision::Armed, None),
        )
        .unwrap();
    assert_eq!(
        tokio::task::block_in_place(|| reached.recv_timeout(SETTLE)),
        Ok(StampPoint::MarkerStamped),
        "the next pass reads the marker and parks after its stamp"
    );
    let parked = Instant::now();
    assert_eq!(
        input.record(key).unwrap().decision,
        None,
        "stamped, not yet observed"
    );
    assert!(
        !input.hazards_clear(at(9)),
        "admission's read: the entry is still paused"
    );
    std::thread::sleep(Duration::from_millis(2));
    go.send(()).unwrap();

    assert!(
        wait_for(SETTLE, || input
            .record(key)
            .is_some_and(|r| r.decision == Some(Decision::Armed)))
        .await,
        "the released marker is observed"
    );
    let record = input.record(key).unwrap();
    assert_eq!(
        record.completion,
        Completion::NotRequired,
        "the marker's attribution names the helper not the composer's: {record:?}"
    );
    let stamp = record.first_armed_seen.expect("an Armed sighting");
    assert!(
        stamp <= parked,
        "the stamp ({stamp:?}) is taken before the park ({parked:?}), not under a lock after it"
    );
    // Resumed at 9 s: ten seconds of quiet time are reached at 19 s, and
    // the tie still counts as quiet.
    assert_eq!(
        input.hazards(at(18)).0,
        vec![EntryView::Quiet { seq, paused: false }],
        "the paused interval between the marker's stamp and the read isn't counted"
    );
    assert!(!input.hazards_clear(at(19)), "the tie goes to the hazard");
    assert!(input.hazards_clear(at(20)));
    deck.kill(helper);
    deck.stop().await;
}

/// Drive one probe on `deck`'s generation to an `Armed` empty read and its
/// settlement through the generation's public steps: a probe segment opened,
/// its liability raised and its byte written, a failure first (so that only
/// an empty settlement makes the next attempt due at once), then the
/// daemon's read of the export and the helper's `Armed` invocation on IPC,
/// matched to the segment by the number it read. `between` runs after the
/// probe was scheduled and before the read reaches the settlement.
fn empty_read_settled(deck: &Deck, pid: u32, between: impl FnOnce()) {
    use crate::composer_probe::record::{Channel, Claim, Observation};
    let input = deck.observer.input();
    let now = Instant::now();
    let (number, _) = input.segment_open_probe(false, now).unwrap();
    input.liability_raise(deck.generation, now).unwrap();
    input.segment_written(number, 0, now, now).unwrap();
    input.probe_failed(now);
    between();
    let key = RecordKey { pid, start: 1 };
    input.note_read(key, true);
    input
        .record_observe(
            &Observation {
                generation: deck.generation,
                key,
                channel: Channel::Ipc,
                attribution: Attribution::Owned,
                claim: Some(Claim::Owned),
                decision: Some(Decision::Armed),
                seg_seen: Some(number),
                native_seen: None,
                birth_floor: now,
            },
            now,
        )
        .unwrap();
}

/// `(due now, failures in a row)` of `deck`'s generation's probe schedule.
fn schedule(deck: &Deck) -> (bool, u32) {
    let (due, failures, _) = deck
        .observer
        .input()
        .advance_schedule_for_e2e(Duration::ZERO, Instant::now())
        .expect("a live schedule");
    (due, failures)
}

/// Scenario: A bound deck whose agent reads its `keybindings.json` from a `HOME` the test owns; its permission settles with the file empty. A probe is scheduled, after one failed attempt, and its helper decides `Armed` with the daemon reading the export empty. When the binding file changes after the probe was scheduled and before the read's settlement, the read is not applied: the settlement counts a second failure, the next attempt waits its backoff, and the keymap reads `Changed`. With the file left alone (the control) the same settlement applies the read: the failures reset and the next attempt is due at once (A-20d-1a, O-20.7b).
#[spec("inbox/probe/014")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_014_a_binding_changed_before_an_empty_reads_settlement_withholds_the_read() {
    use crate::composer_probe::keymap::Hold;
    for changed in [false, true] {
        let deck = Deck::start_bound_at_home().await;
        let observer = Arc::clone(&deck.observer);
        assert!(
            wait_for(SETTLE, || observer.permission() == Permission::Armable).await,
            "the bound deck's permission settles Armable"
        );
        assert_eq!(
            tokio::task::spawn_blocking({
                let observer = Arc::clone(&observer);
                move || observer.keymap()
            })
            .await
            .unwrap(),
            Ok(()),
            "the deck's own empty keybindings.json allows (a CLAUDE_CONFIG_DIR in the test's \
             environment would hold it)"
        );
        let path = deck.keybindings();
        empty_read_settled(&deck, 4545, || {
            if changed {
                std::fs::write(&path, r#"{"bindings": []}"#).unwrap();
            }
        });
        if changed {
            assert!(
                wait_for(SETTLE, || schedule(&deck) == (false, 2)).await,
                "the read is withheld and counted a failed check: {:?}",
                schedule(&deck)
            );
            let keymap = tokio::task::spawn_blocking({
                let observer = Arc::clone(&observer);
                move || observer.keymap()
            })
            .await
            .unwrap();
            assert_eq!(keymap, Err(Hold::Changed));
        } else {
            assert!(
                wait_for(SETTLE, || schedule(&deck) == (true, 0)).await,
                "the read is applied: {:?}",
                schedule(&deck)
            );
        }
        deck.stop().await;
    }
}

/// A bound deck at home whose permission settled `Armable` and whose
/// keymap allows, with its observer parked at its next keymap re-read: the
/// park's `reached` and `go`.
async fn parked_at_the_reread(
    deck: &Deck,
) -> (
    std::sync::mpsc::Receiver<crate::composer_probe::stamp_park::StampPoint>,
    std::sync::mpsc::Sender<()>,
) {
    use crate::composer_probe::stamp_park::{StampPoint, park_at};
    let observer = Arc::clone(&deck.observer);
    assert!(
        wait_for(SETTLE, || observer.permission() == Permission::Armable).await,
        "the bound deck's permission settles Armable"
    );
    let (park, reached, go) = park_at(&[StampPoint::KeymapRecheck]);
    deck.observer.park_stamps_for_test(park);
    (reached, go)
}

/// Scenario: A bound deck at home; its observer is parked inside its keymap re-read, after it read which empty reads it settles and before it reads the file. A probe's `Armed` empty read settles and parks; while the observer is parked there the binding file changes; then the observer goes on. Its read is the one taken after the change, so the empty read is withheld and counted a failed check (A-20d-1c: a change inside the re-read's window is seen).
#[spec("inbox/probe/014")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_014_a_binding_changed_inside_the_reread_window_withholds_the_read() {
    use crate::composer_probe::stamp_park::StampPoint;
    let deck = Deck::start_bound_at_home().await;
    let (reached, go) = parked_at_the_reread(&deck).await;
    empty_read_settled(&deck, 4646, || {});
    let at = tokio::task::spawn_blocking(move || reached.recv_timeout(SETTLE))
        .await
        .unwrap()
        .expect("the observer reaches its re-read");
    assert_eq!(at, StampPoint::KeymapRecheck);
    assert!(deck.observer.input().settling(), "parked, not applied");
    assert_eq!(schedule(&deck), (false, 1));
    std::fs::write(deck.keybindings(), r#"{"bindings": []}"#).unwrap();
    go.send(()).unwrap();
    assert!(
        wait_for(SETTLE, || schedule(&deck) == (false, 2)).await,
        "the read taken after the change withholds it: {:?}",
        schedule(&deck)
    );
    deck.stop().await;
}

/// Scenario: A bound deck at home; a probe's `Armed` empty read parks and the observer stops inside its keymap re-read, with the binding file unchanged. The generation retires there; then the observer goes on, reads a keymap that allows, and hands the verdict back. Nothing is applied to the retired generation: its schedule keeps the failure it had and the read never reads as settled empty (A-20d-1c: ownership across the I/O).
#[spec("inbox/probe/014")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_014_a_generation_retired_inside_the_reread_window_applies_nothing() {
    let deck = Deck::start_bound_at_home().await;
    let (reached, go) = parked_at_the_reread(&deck).await;
    empty_read_settled(&deck, 4747, || {});
    tokio::task::spawn_blocking(move || reached.recv_timeout(SETTLE))
        .await
        .unwrap()
        .expect("the observer reaches its re-read");
    let input = deck.observer.input();
    let before = deck.observer.passes_for_e2e().finished;
    input.retire();
    go.send(()).unwrap();
    assert!(
        wait_for(SETTLE, || {
            let passes = deck.observer.passes_for_e2e();
            !passes.running && passes.finished > before
        })
        .await,
        "the parked pass finishes"
    );
    assert_eq!(
        schedule(&deck),
        (false, 1),
        "the allowing verdict applied nothing to the retired generation"
    );
    deck.stop().await;
}

/// Scenario: A bound deck at home, its spawn's `HOME` the deck's own home with an empty `keybindings.json`, while the agent it bound runs with another environment, as one started by `HOME=/elsewhere claude` or `CLAUDE_CONFIG_DIR=/x claude` does. With the agent's `HOME` naming a home whose `keybindings.json` moves Ctrl+G, the keymap holds as changed; with the agent's `CLAUDE_CONFIG_DIR` set, as unmeasured. The deck reads the files the agent's own environment names, not its spawn's (the reva21 review's U1-1).
#[spec("inbox/probe/014")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_014_the_bound_agents_own_environment_picks_its_key_bindings() {
    use crate::composer_probe::keymap::Hold;
    let cases: [(&[(&str, &str)], Hold); 2] = [
        (&[("HOME", "elsewhere")], Hold::Changed),
        (
            &[("HOME", "home"), ("CLAUDE_CONFIG_DIR", "config")],
            Hold::Unmeasured,
        ),
    ];
    for (agent_env, held) in cases {
        let deck = Deck::start_bound_at_home_as(agent_env).await;
        let claude = deck.dir.path().join("elsewhere/.claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("keybindings.json"),
            r#"{"bindings": [{"context": "Chat", "bindings": {"ctrl+g": "chat:submit"}}]}"#,
        )
        .unwrap();
        let observer = Arc::clone(&deck.observer);
        assert!(
            wait_for(SETTLE, || observer.permission() == Permission::Armable).await,
            "the bound deck's permission settles Armable"
        );
        let keymap = tokio::task::spawn_blocking(move || observer.keymap())
            .await
            .unwrap();
        assert_eq!(keymap, Err(held), "{agent_env:?}");
        deck.stop().await;
    }
}

/// A bound deck at home whose release latch was held back and is now let
/// go, with its observer parked inside the settlement that computed
/// `Armable`: it holds the settle claim, its keymap evidence not set and the
/// permission not published. The park tells `waiting` when a probe
/// request's settlement finds the claim held, and the observer goes on at
/// `go`.
async fn parked_in_the_settlement(
    deck: &Deck,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    use crate::composer_probe::stamp_park::StampPoint;
    let (settling, settled_at) = std::sync::mpsc::channel();
    let (waiting_tx, waiting) = std::sync::mpsc::channel();
    let (go, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    deck.observer.park_stamps_for_test(move |at| match at {
        StampPoint::SettleEvidence => {
            let _ = settling.send(());
            let _ = go_rx.lock().unwrap().recv();
        }
        StampPoint::SettleWaiting => {
            let _ = waiting_tx.send(());
        }
        _ => {}
    });
    deck.latch();
    tokio::task::spawn_blocking(move || settled_at.recv_timeout(SETTLE))
        .await
        .unwrap()
        .expect("the observer settles once the release is seen");
    assert_eq!(
        deck.observer.permission(),
        Permission::Pending,
        "parked before it publishes"
    );
    (waiting, go)
}

/// Scenario: A bound deck at home, its release held back so its permission waits; the release is let go and the observer is parked inside its own settlement, after it computed `Armable` and before it set the keymap evidence and published. A probe request's settlement then runs, finds the observer holding the claim, and the observer is let go only once the request is waiting for it. The request's settlement returns after the observer's, so the keymap read after it allows and the permission reads `Armable`; a settlement that returned at once would read `Unmeasured` with the permission still pending, and the probe would be refused (Q116b, the race `probe_029` caught on GitHub's runners).
#[spec("inbox/probe/059")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_059_a_requests_settlement_waits_for_the_one_in_flight() {
    let deck = Deck::start_bound_at_home_latched_later().await;
    let (waiting, go) = parked_in_the_settlement(&deck).await;
    let release = go.clone();
    // Let the observer go once the request waits for it; never, if it
    // doesn't wait.
    let releaser = std::thread::spawn(move || {
        if waiting.recv_timeout(SETTLE).is_ok() {
            let _ = release.send(());
        }
    });
    let request = tokio::task::spawn_blocking({
        let observer = Arc::clone(&deck.observer);
        move || {
            observer.settle();
            (observer.permission(), observer.keymap())
        }
    });
    let read = tokio::time::timeout(SETTLE, request).await;
    let _ = go.send(());
    releaser.join().unwrap();
    let read = read.expect("the request's settlement returns").unwrap();
    assert_eq!(
        read,
        (Permission::Armable, Ok(())),
        "the request read after the settlement in flight, not before it"
    );
    deck.stop().await;
}

/// Scenario: As above, but the observer parked inside its settlement is never let go while the probe request's settlement waits. The request's settlement gives up after its bound, well inside the test's budget, with the permission still pending: it waits for a settlement in flight, not forever (Q116b).
#[spec("inbox/probe/059")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_059_a_requests_settlement_waits_no_longer_than_its_bound() {
    let deck = Deck::start_bound_at_home_latched_later().await;
    let (waiting, go) = parked_in_the_settlement(&deck).await;
    let started = Instant::now();
    let request = tokio::task::spawn_blocking({
        let observer = Arc::clone(&deck.observer);
        move || {
            observer.settle();
            observer.permission()
        }
    });
    let read = tokio::time::timeout(SETTLE, request).await;
    let elapsed = started.elapsed();
    let _ = go.send(());
    assert_eq!(
        read.expect("the request's settlement gives up").unwrap(),
        Permission::Pending
    );
    assert!(
        waiting.try_recv().is_ok(),
        "the request found the claim held"
    );
    assert!(
        elapsed >= crate::composer_probe::observe::SETTLE_WAIT,
        "it waited its bound: {elapsed:?}"
    );
    deck.stop().await;
}

/// Scenario: A bound deck at home is spawned with an empty `keybindings.json`, its release latch held back so its permission waits. The binding file changes after the spawn and before the deck sees the agent's release; then the release is latched and the permission settles `Armable`. The keymap reads `Changed`: the read the generation is compared with is the one taken before its spawn, which the agent loaded, not one taken at settlement. Left unchanged (the control), the keymap allows (A-20d-1b, the pre-spawn baseline).
#[spec("inbox/probe/014")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_014_a_binding_changed_between_the_spawn_and_the_permission_holds() {
    use crate::composer_probe::keymap::Hold;
    for changed in [false, true] {
        let deck = Deck::start_bound_at_home_latched_later().await;
        let observer = Arc::clone(&deck.observer);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            observer.permission(),
            Permission::Pending,
            "no release seen yet"
        );
        if changed {
            std::fs::write(deck.keybindings(), r#"{"bindings": []}"#).unwrap();
        }
        deck.latch();
        assert!(
            wait_for(SETTLE, || observer.permission() == Permission::Armable).await,
            "the permission settles once the release is seen"
        );
        let keymap = tokio::task::spawn_blocking({
            let observer = Arc::clone(&observer);
            move || observer.keymap()
        })
        .await
        .unwrap();
        assert_eq!(
            keymap,
            if changed { Err(Hold::Changed) } else { Ok(()) },
            "changed: {changed}"
        );
        deck.stop().await;
    }
}

/// Scenario: Start a direct Codex generation whose export directory the deck refused before its spawn (a `~/.codex/editor` its group can write to, Q35-27). Its observer is inert for that reason from its start, and a settlement afterwards leaves it so, with no discovery run and nothing refused, so no check is ever armed; the doorbell's hold for its touched box is `cannot_check`, where another inert generation's is `cannot_type`; and the registry hands the doorbell the refused directory and its cause for the card. A generation with no such refusal has none.
#[spec("inbox/probe/091")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_091_a_refused_codex_export_directory_holds_its_generation_with_the_detail() {
    use crate::composer_probe::InertCause;
    use crate::inbox_display::{HoldCause, HoldDetail, WakeHold};
    let registry = Arc::new(AgentPtyRegistry::new());
    let id = registry
        .spawn_agent(SpawnOptions {
            command: Some("/bin/sh"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn shell agent");
    let dir = tempfile::Builder::new()
        .prefix("composer-export-hold")
        .rand_bytes(8)
        .tempdir_in(std::env::temp_dir())
        .unwrap();
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
    let detail = HoldDetail::new(
        HoldCause::WritableDirectory,
        Some("/home/me/.codex/editor"),
        None,
    );
    let mut installation = install(
        &instance,
        PaneClass::Direct(ProbeAgent::Codex),
        &id,
        &Originals::default(),
    )
    .unwrap();
    assert_eq!(installation.export_hold(), None, "install prepares nothing");
    installation.set_export_hold_for_test(detail.clone());
    installation.set_launch(Vec::new());
    let spawned = Spawned {
        agent_id: id.clone(),
        agent: Some(AgentType::Codex),
        env: vec![],
        child: None,
        input: Arc::new(GenerationInput::default()),
        latch: Box::new(|_| LatchedRelease::Unseen),
        assumed: &[],
    };
    let observer = Observer::start(installation, spawned, &Arc::new(std::sync::OnceLock::new()))
        .expect("observer");
    registry.set_composer_observer_for_test(&id, Arc::clone(&observer));
    assert_eq!(
        observer.permission(),
        Permission::Inert(InertCause::ExportDirectory),
        "inert from its start"
    );
    assert_eq!(observer.export_hold(), Some(&detail));
    // Q35-22's discovery settles only a pending generation: a settlement
    // now neither moves the permission nor runs discovery, which, with no
    // pin taken for this generation, would refuse it `executable not
    // resolved`.
    observer.settle();
    assert_eq!(
        observer.binding_status_for_e2e(),
        (Permission::Inert(InertCause::ExportDirectory), None),
        "a settlement leaves the export hold, and discovery never ran"
    );
    assert_eq!(
        registry.doorbell_unchecked_hold(PANE, &id, Some(observer.permission())),
        WakeHold::CannotCheck
    );
    assert_ne!(
        registry.doorbell_unchecked_hold(PANE, &id, Some(Permission::Inert(InertCause::Unbound))),
        WakeHold::CannotCheck,
        "the control: another inert generation isn't held for its box"
    );
    assert_eq!(registry.doorbell_hold_detail(PANE, &id), Some(detail));
    assert_eq!(
        registry.doorbell_hold_detail("another-pane", &id),
        None,
        "only in the generation's own pane"
    );
    observer.stop();
}
