#![cfg(all(feature = "e2e", unix))]

//! Lane-1 reproduction coverage for PRD #1223's report that agents started
//! through the desktop's daemon verbs do not appear correctly in the TUI.
//! The desktop GUI itself has no driver tier, so these scenarios drive the
//! exact attach requests its actions issue and then observe the real TUI in a
//! PTY. Each starts with the equivalent TUI-native launch as a control.

mod common;

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use common::{DaemonProc, TuiDeck};
use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_PANE_ID, TabMembership};
use dot_agent_deck::daemon_client::{DaemonClient, EventSubscription, StartAgentOptions};
use dot_agent_deck::daemon_protocol::{AttachRequest, AttachResponse, KIND_EVENT, KIND_RESP};
use dot_agent_deck::event::{AgentType, BroadcastMsg};
use spec::spec;

const PLAIN_LABEL: &str = "desktop-visible-agent";
const SECOND_PLAIN_LABEL: &str = "second-desktop-agent";
const THIRD_PLAIN_LABEL: &str = "third-desktop-agent";
const TWO_CLIENT_LABEL: &str = "two-client-desktop-agent";
const LAZY_SPAWN_LABEL: &str = "lazy-spawn-desktop-agent";
const REFETCH_LABEL: &str = "refetch-desktop-agent";
const PLAIN_COMMAND: &str = "sleep 600";
const ORCHESTRATION_NAME: &str = "desktop-visibility-team";
const ORCHESTRATION_TITLE: &str = "Desktop prepared run";
const ORCHESTRATION_ID: &str = "desktop-visibility-orchestration";
const ORCHESTRATION_ROLES: [&str; 3] = ["coordinator", "builder", "reviewer"];
const ORCHESTRATION_CONFIG: &str = include_str!("fixtures/desktop-visibility/.dot-agent-deck.toml");

struct WireFrame {
    kind: u8,
    payload: Vec<u8>,
}

fn read_wire_frame(stream: &mut UnixStream) -> io::Result<Option<WireFrame>> {
    let mut kind = [0_u8; 1];
    match stream.read_exact(&mut kind) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let mut len = [0_u8; 4];
    stream.read_exact(&mut len)?;
    let mut payload = vec![0_u8; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut payload)?;
    Ok(Some(WireFrame {
        kind: kind[0],
        payload,
    }))
}

fn write_wire_frame(stream: &mut UnixStream, frame: &WireFrame) -> io::Result<()> {
    stream.write_all(&[frame.kind])?;
    stream.write_all(&(frame.payload.len() as u32).to_be_bytes())?;
    stream.write_all(&frame.payload)
}

fn relay_after_first_response(
    mut downstream: UnixStream,
    mut upstream: UnixStream,
) -> io::Result<()> {
    let mut downstream_reader = downstream.try_clone()?;
    let mut upstream_writer = upstream.try_clone()?;
    let inbound = std::thread::spawn(move || {
        let result = io::copy(&mut downstream_reader, &mut upstream_writer);
        let _ = upstream_writer.shutdown(Shutdown::Write);
        result
    });
    let outbound = io::copy(&mut upstream, &mut downstream);
    let _ = downstream.shutdown(Shutdown::Both);
    let _ = upstream.shutdown(Shutdown::Both);
    let _ = inbound.join();
    outbound.map(|_| ())
}

fn relay_subscription(
    mut downstream: UnixStream,
    mut upstream: UnixStream,
    event_tx: Sender<Vec<u8>>,
) -> io::Result<()> {
    let mut downstream_reader = downstream.try_clone()?;
    let mut upstream_writer = upstream.try_clone()?;
    let inbound = std::thread::spawn(move || {
        let result = io::copy(&mut downstream_reader, &mut upstream_writer);
        let _ = upstream_writer.shutdown(Shutdown::Write);
        result
    });
    while let Some(frame) = read_wire_frame(&mut upstream)? {
        write_wire_frame(&mut downstream, &frame)?;
        if frame.kind == KIND_EVENT {
            let _ = event_tx.send(frame.payload);
        }
    }
    let _ = downstream.shutdown(Shutdown::Both);
    let _ = upstream.shutdown(Shutdown::Both);
    let _ = inbound.join();
    Ok(())
}

fn proxy_connection(
    mut downstream: UnixStream,
    upstream_path: &Path,
    list_gate_claimed: &AtomicBool,
    list_snapshot_tx: &Sender<Option<usize>>,
    release_list_rx: &Mutex<Receiver<()>>,
    subscription_tx: &Sender<()>,
    event_tx: &Sender<Vec<u8>>,
) -> io::Result<()> {
    let Some(request_frame) = read_wire_frame(&mut downstream)? else {
        return Ok(());
    };
    let request: AttachRequest = serde_json::from_slice(&request_frame.payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut upstream = UnixStream::connect(upstream_path)?;
    write_wire_frame(&mut upstream, &request_frame)?;
    let response_frame = read_wire_frame(&mut upstream)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "daemon closed before its first response",
        )
    })?;
    if response_frame.kind != KIND_RESP {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "daemon's first response used frame kind {:#x}, expected {KIND_RESP:#x}",
                response_frame.kind
            ),
        ));
    }

    match request {
        AttachRequest::ListAgents if !list_gate_claimed.swap(true, Ordering::SeqCst) => {
            let response: AttachResponse = serde_json::from_slice(&response_frame.payload)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let _ = list_snapshot_tx.send(response.agent_records.as_ref().map(Vec::len));
            let _ = release_list_rx
                .lock()
                .expect("attach-race release receiver")
                .recv_timeout(Duration::from_secs(10));
            write_wire_frame(&mut downstream, &response_frame)?;
            relay_after_first_response(downstream, upstream)
        }
        AttachRequest::SubscribeEvents => {
            write_wire_frame(&mut downstream, &response_frame)?;
            let _ = subscription_tx.send(());
            relay_subscription(downstream, upstream, event_tx.clone())
        }
        _ => {
            write_wire_frame(&mut downstream, &response_frame)?;
            relay_after_first_response(downstream, upstream)
        }
    }
}

/// Transparent attach-socket proxy that freezes startup hydration after the
/// daemon has captured its `ListAgents` snapshot. The event stream remains
/// live, so the test can put a card-surface event ahead of the empty hydration
/// response on the TUI side without a sleep or scheduler-timing assumption.
struct AttachRaceGate {
    path: PathBuf,
    list_snapshot_rx: Receiver<Option<usize>>,
    subscription_rx: Receiver<()>,
    event_rx: Receiver<Vec<u8>>,
    release_list_tx: Option<Sender<()>>,
    stopping: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    handlers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    errors: Arc<Mutex<Vec<String>>>,
    _tempdir: tempfile::TempDir,
}

impl AttachRaceGate {
    fn new(upstream_path: &Path) -> Self {
        let tempdir = common::harness_tempdir().expect("create attach-race proxy tempdir");
        let path = tempdir.path().join("attach-race.sock");
        let listener = UnixListener::bind(&path).expect("bind attach-race proxy");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("make attach-race proxy owner-only");
        let upstream_path = upstream_path.to_path_buf();
        let stopping = Arc::new(AtomicBool::new(false));
        let stopping_for_thread = Arc::clone(&stopping);
        let handlers = Arc::new(Mutex::new(Vec::new()));
        let handlers_for_thread = Arc::clone(&handlers);
        let errors = Arc::new(Mutex::new(Vec::new()));
        let errors_for_thread = Arc::clone(&errors);
        let list_gate_claimed = Arc::new(AtomicBool::new(false));
        let (list_snapshot_tx, list_snapshot_rx) = mpsc::channel();
        let (release_list_tx, release_list_rx) = mpsc::channel();
        let release_list_rx = Arc::new(Mutex::new(release_list_rx));
        let (subscription_tx, subscription_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();

        let accept_thread = std::thread::spawn(move || {
            while let Ok((downstream, _)) = listener.accept() {
                if stopping_for_thread.load(Ordering::SeqCst) {
                    break;
                }
                let upstream_path = upstream_path.clone();
                let list_gate_claimed = Arc::clone(&list_gate_claimed);
                let list_snapshot_tx = list_snapshot_tx.clone();
                let release_list_rx = Arc::clone(&release_list_rx);
                let subscription_tx = subscription_tx.clone();
                let event_tx = event_tx.clone();
                let errors = Arc::clone(&errors_for_thread);
                let handler = std::thread::spawn(move || {
                    if let Err(error) = proxy_connection(
                        downstream,
                        &upstream_path,
                        &list_gate_claimed,
                        &list_snapshot_tx,
                        &release_list_rx,
                        &subscription_tx,
                        &event_tx,
                    ) {
                        errors
                            .lock()
                            .expect("attach-race errors")
                            .push(error.to_string());
                    }
                });
                handlers_for_thread
                    .lock()
                    .expect("attach-race handlers")
                    .push(handler);
            }
        });

        Self {
            path,
            list_snapshot_rx,
            subscription_rx,
            event_rx,
            release_list_tx: Some(release_list_tx),
            stopping,
            accept_thread: Some(accept_thread),
            handlers,
            errors,
            _tempdir: tempdir,
        }
    }

    fn wait_for_empty_snapshot(&self, attempt: usize) {
        let count = self
            .list_snapshot_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|error| {
                panic!(
                    "attach-race attempt {attempt}: TUI hydration did not capture ListAgents: \
                     {error}. Proxy errors: {:#?}",
                    self.errors.lock().expect("attach-race errors")
                )
            });
        assert_eq!(
            count,
            Some(0),
            "attach-race attempt {attempt}: hydration must capture an explicitly empty typed \
             registry snapshot"
        );
    }

    fn wait_for_subscription(&self, attempt: usize) {
        self.subscription_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|error| {
                panic!(
                    "attach-race attempt {attempt}: TUI event subscription was not active while \
                     hydration was held: {error}. Proxy errors: {:#?}",
                    self.errors.lock().expect("attach-race errors")
                )
            });
    }

    fn wait_for_surface_event(&self, attempt: usize, pane_id: &str) {
        let payload = self
            .event_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|error| {
                panic!(
                    "attach-race attempt {attempt}: no card-surface event crossed the TUI's live \
                     subscription before hydration was released: {error}. Proxy errors: {:#?}",
                    self.errors.lock().expect("attach-race errors")
                )
            });
        let broadcast: BroadcastMsg = serde_json::from_slice(&payload)
            .expect("attach-race KIND_EVENT payload is a BroadcastMsg");
        let BroadcastMsg::Event(event) = broadcast else {
            panic!("attach-race attempt {attempt}: expected a card-surface AgentEvent");
        };
        assert_eq!(
            event.pane_id.as_deref(),
            Some(pane_id),
            "attach-race attempt {attempt}: the event crossing before hydration must belong to \
             the desktop-started pane"
        );
        assert!(
            event.is_card_surface_session_start(),
            "attach-race attempt {attempt}: the event crossing before hydration must be the \
             daemon-authored card-surface SessionStart"
        );
    }

    fn release_hydration(&mut self) {
        self.release_list_tx
            .take()
            .expect("attach-race hydration released once")
            .send(())
            .expect("release attach-race ListAgents response");
    }
}

impl Drop for AttachRaceGate {
    fn drop(&mut self) {
        self.release_list_tx.take();
        self.stopping.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.path);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        for handle in self
            .handlers
            .lock()
            .expect("attach-race handlers")
            .drain(..)
        {
            let _ = handle.join();
        }
    }
}

/// Launch the real TUI binary in a PTY against an already-running daemon.
/// No key is sent by this helper: callers can distinguish startup hydration
/// from a card that appears only after selection or another interaction.
fn launch_tui_against(daemon: &DaemonProc) -> TuiDeck {
    launch_tui_against_sockets(&daemon.attach_socket, &daemon.hook_socket)
}

fn launch_tui_against_sockets(attach_socket: &Path, hook_socket: &Path) -> TuiDeck {
    TuiDeck::builder()
        .with_pty_size(120, 40)
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            attach_socket.to_string_lossy().to_string(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            hook_socket.to_string_lossy().to_string(),
        )
        .launch_with_fixture("minimal")
}

/// Use the real TUI new-agent form to start a named plain pane. This is the
/// control for the desktop's direct `StartAgent` request.
fn start_plain_from_tui(deck: &TuiDeck) {
    deck.wait_for_string("No active agents");
    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // confirm the fixture cwd
    deck.wait_for_string("No mode");
    deck.send_keys(b"\r"); // Mode -> Name
    deck.send_keys(PLAIN_LABEL.as_bytes());
    deck.send_keys(b"\r"); // Name -> Command
    deck.send_keys(PLAIN_COMMAND.as_bytes());
    deck.send_keys(b"\r"); // submit
}

/// Use the real TUI new-agent form to launch the fixture's orchestration. This
/// is the control for the desktop's PrepareWorkflow + StartPreparedAgent loop.
fn start_orchestration_from_tui(deck: &TuiDeck) {
    deck.wait_for_string("No active agents");
    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // confirm the fixture cwd
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C"); // select the fixture's only orchestration
    deck.send_keys(b"\r"); // Mode -> Name
    deck.send_keys(b"\r"); // keep the default title and submit
}

fn canonical_string(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|error| panic!("canonicalize {}: {error}", path.display()))
        .to_string_lossy()
        .into_owned()
}

/// Deterministic representative of `mint_desktop_pane_id()`'s
/// `desktop-{nonce:016x}-{sequence}` wire format.
fn desktop_pane_id(sequence: usize) -> String {
    format!("desktop-0123456789abcdef-{sequence}")
}

fn missing_roles(grid: &str) -> Vec<&'static str> {
    ORCHESTRATION_ROLES
        .iter()
        .copied()
        .filter(|role| !grid.contains(role))
        .collect()
}

/// The single-tab dashboard has a session-count header, one bordered card per
/// plain agent, and the command-mode dashboard controls along the bottom.
fn plain_dashboard_shows(grid: &str, labels: &[&str]) -> bool {
    grid.lines().next() == Some(format!(" dot-agent-deck — {} agent(s)", labels.len()).as_str())
        && labels.iter().all(|label| grid.contains(label))
        && grid.matches('┌').count() + grid.matches('┏').count() == labels.len()
        && grid.matches('└').count() + grid.matches('┗').count() == labels.len()
        && grid.matches("Launch an agent to get started").count() == labels.len()
        && grid
            .lines()
            .any(|line| line.starts_with(" COMMAND  [Back to Pane Ctrl+D]"))
        && grid.contains("[Filter /] [Rename r] [Generate g] [Schedules s]")
}

/// Send the plain `StartAgent` shape built by the desktop action. The explicit
/// type is inferred from the command exactly as `start_agent_action` does.
fn start_plain_from_desktop(daemon: &DaemonProc, cwd: String, pane_id: &str, display_name: &str) {
    let response = daemon
        .send_attach_request(&AttachRequest::StartAgent {
            command: Some(PLAIN_COMMAND.into()),
            cwd: Some(cwd),
            rows: 24,
            cols: 80,
            env: vec![(DOT_AGENT_DECK_PANE_ID.into(), pane_id.into())],
            display_name: Some(display_name.into()),
            tab_membership: None,
            agent_type: AgentType::from_command(Some(PLAIN_COMMAND)),
            seed: None,
            authoring_kind: None,
        })
        .expect("desktop-shaped StartAgent over the attach socket");
    assert!(
        response.ok,
        "desktop-shaped StartAgent must succeed before visibility can be observed: {:?}",
        response.error
    );
}

/// A desktop-shaped client that uses the production `DaemonClient`: one
/// long-lived event subscription plus short-lived command connections from the
/// same handle, matching the desktop watcher and action paths.
struct DesktopAttachClient {
    runtime: tokio::runtime::Runtime,
    client: DaemonClient,
    events: EventSubscription,
}

impl DesktopAttachClient {
    fn connect(attach_socket: &Path) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build desktop attach-client runtime");
        let client = DaemonClient::new(attach_socket.to_path_buf());
        let capabilities = runtime
            .block_on(client.capabilities())
            .expect("desktop client handshake");
        assert!(
            capabilities.is_advertised(),
            "the branch daemon must advertise capabilities to the branch desktop client"
        );
        let events = runtime
            .block_on(client.subscribe_events())
            .expect("desktop client SubscribeEvents");
        Self {
            runtime,
            client,
            events,
        }
    }

    fn start_plain(&self, cwd: String, pane_id: &str, display_name: &str) -> String {
        self.runtime
            .block_on(self.client.start_agent(StartAgentOptions {
                command: Some(PLAIN_COMMAND.into()),
                cwd: Some(cwd),
                display_name: Some(display_name.into()),
                rows: 24,
                cols: 80,
                env: vec![(DOT_AGENT_DECK_PANE_ID.into(), pane_id.into())],
                tab_membership: None,
                agent_type: AgentType::from_command(Some(PLAIN_COMMAND)),
                seed: None,
            }))
            .expect("desktop client's production StartAgent path")
    }

    fn list_agents(&self) -> Vec<dot_agent_deck::agent_pty::AgentRecord> {
        self.runtime
            .block_on(self.client.list_agents())
            .expect("desktop client's post-start ListAgents refetch")
    }

    fn wait_for_surface_event(&mut self, pane_id: &str) {
        self.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match self
                        .events
                        .next_event()
                        .await
                        .expect("read desktop client's subscribed event")
                    {
                        Some(BroadcastMsg::Event(event))
                            if event.pane_id.as_deref() == Some(pane_id)
                                && event.is_card_surface_session_start() =>
                        {
                            return;
                        }
                        Some(_) => {}
                        None => panic!(
                            "desktop client's SubscribeEvents stream ended before the card-surface event for {pane_id:?}"
                        ),
                    }
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "desktop client's own event subscription did not receive the card-surface event for {pane_id:?}"
                )
            });
        });
    }
}

fn run_mid_attach_start_attempt(attempt: usize) {
    let daemon = common::spawn_daemon_serve(None, "0");
    assert!(
        daemon.agent_records().is_empty(),
        "attach-race attempt {attempt}: the daemon must be empty before the TUI begins attaching"
    );
    let mut gate = AttachRaceGate::new(&daemon.attach_socket);
    let cwd = common::harness_tempdir().expect("create attach-race desktop-selected cwd");
    let pane_id = desktop_pane_id(100 + attempt);
    let label = format!("desktop-attach-race-{attempt}");
    let deck = launch_tui_against_sockets(&gate.path, &daemon.hook_socket);

    // The daemon has answered ListAgents with an empty typed snapshot, but the
    // proxy withholds it from startup hydration. In parallel the TUI's live
    // event subscriber has completed its own handshake.
    gate.wait_for_empty_snapshot(attempt);
    gate.wait_for_subscription(attempt);
    start_plain_from_desktop(&daemon, canonical_string(cwd.path()), &pane_id, &label);
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(10));
    assert_eq!(
        records.len(),
        1,
        "attach-race attempt {attempt}: the desktop start must reach the daemon registry"
    );

    // Prove the exact card-surface event crossed the subscription before the
    // stale empty hydration response is allowed to reach the TUI.
    gate.wait_for_surface_event(attempt, &pane_id);
    gate.release_hydration();

    assert!(
        common::wait_until(Duration::from_secs(10), || {
            deck.snapshot_grid().contains(&label)
        }),
        "attach-race attempt {attempt}: the desktop-started card-surface event crossed the \
         TUI subscription BEFORE its previously-captured empty ListAgents hydration response, \
         but {label:?} was not visible after startup settled. If the grid is empty, hydration \
         overwrote the event-populated session map. Records: {records:#?}\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    drop(deck);
    drop(gate);
    drop(daemon);
}

/// Drive the desktop's empty-task preparation and configured-role loop. Its
/// dimensions are the desktop orchestration defaults (32×120), distinct from
/// the plain action's 24×80 defaults.
fn start_orchestration_from_desktop(daemon: &DaemonProc, project_path: &str) {
    let response = daemon
        .send_attach_request(&AttachRequest::PrepareWorkflow {
            path: project_path.into(),
            orchestration: ORCHESTRATION_NAME.into(),
            task: String::new(),
            config_revision: None,
        })
        .expect("desktop-shaped PrepareWorkflow over the attach socket");
    assert!(
        response.ok,
        "desktop-shaped PrepareWorkflow must succeed before roles can start: {:?}",
        response.error
    );
    let prepared = response
        .workflow_prepared
        .expect("successful PrepareWorkflow must return its roles and token");
    assert_eq!(
        prepared.roles.len(),
        ORCHESTRATION_ROLES.len(),
        "the fixture must prepare the same three roles as the TUI control"
    );

    for (role_index, role) in prepared.roles.iter().enumerate() {
        let pane_id = desktop_pane_id(role_index);
        let response = daemon
            .send_attach_request(&AttachRequest::StartPreparedAgent {
                prep_token: prepared.token.clone(),
                command: None,
                cwd: Some(project_path.into()),
                rows: 32,
                cols: 120,
                env: vec![(DOT_AGENT_DECK_PANE_ID.into(), pane_id)],
                display_name: Some(role.name.clone()),
                tab_membership: Some(TabMembership::Orchestration {
                    name: ORCHESTRATION_NAME.into(),
                    role_index,
                    role_name: role.name.clone(),
                    is_start_role: role.start,
                    orchestration_cwd: Some(project_path.into()),
                    display_title: Some(ORCHESTRATION_TITLE.into()),
                    orchestration_id: Some(ORCHESTRATION_ID.into()),
                }),
                agent_type: None,
                seed: None,
                use_configured_command: true,
            })
            .expect("desktop-shaped StartPreparedAgent over the attach socket");
        assert!(
            response.ok,
            "desktop-shaped start for role {:?} must succeed: {:?}",
            role.name, response.error
        );
    }
}

/// Send the same single-agent stop request as the desktop's
/// `stop_agent_action`, and require the daemon to confirm it.
fn stop_agent_from_desktop(daemon: &DaemonProc, agent_id: &str) {
    let response = daemon
        .send_attach_request(&AttachRequest::StopAgent {
            id: agent_id.into(),
        })
        .expect("desktop-shaped StopAgent over the attach socket");
    assert!(
        response.ok,
        "desktop-shaped StopAgent for {agent_id:?} must succeed: {:?}",
        response.error
    );
}

/// Mirror `stop_orchestration_action`: issue one independent `StopAgent` per
/// role concurrently, then require every stop to have been confirmed.
fn stop_orchestration_from_desktop(daemon: &DaemonProc, role_agents: &[(String, String)]) {
    let attach_socket = daemon.attach_socket.clone();
    let outcomes = std::thread::scope(|scope| {
        role_agents
            .iter()
            .map(|(role, agent_id)| {
                let role = role.clone();
                let agent_id = agent_id.clone();
                let attach_socket = attach_socket.clone();
                scope.spawn(move || {
                    let response = common::attach_request_on(
                        &attach_socket,
                        &AttachRequest::StopAgent {
                            id: agent_id.clone(),
                        },
                    );
                    (role, agent_id, response)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("desktop role-stop thread"))
            .collect::<Vec<_>>()
    });

    for (role, agent_id, response) in outcomes {
        let response = response.unwrap_or_else(|error| {
            panic!("desktop-shaped StopAgent for role {role:?} ({agent_id}) failed: {error}")
        });
        assert!(
            response.ok,
            "desktop-shaped StopAgent for role {role:?} ({agent_id}) must succeed: {:?}",
            response.error
        );
    }
}

fn write_orchestration_project() -> tempfile::TempDir {
    let project = common::harness_tempdir().expect("create desktop project");
    std::fs::write(
        project.path().join(".dot-agent-deck.toml"),
        ORCHESTRATION_CONFIG,
    )
    .expect("write the desktop orchestration fixture");
    project
}

/// Scenario: First start a named plain pane through the real TUI form and
/// confirm its dashboard card renders. Then send the desktop-shaped
/// `StartAgent` request to a daemon with no TUI attached, attach a fresh real
/// TUI to that daemon, and require the same named card to appear on the
/// dashboard without sending any key or selection input.
#[spec("newagent/visibility/001")]
#[test]
fn visibility_001_desktop_started_plain_agent_appears_without_selection() {
    // Control: the equivalent TUI-native launch is visibly represented.
    let control = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("minimal");
    start_plain_from_tui(&control);
    control.wait_until_grid("TUI-started plain agent is visible", |grid| {
        grid.contains(PLAIN_LABEL)
    });
    drop(control);

    // Reproduction: the desktop sends StartAgent before this TUI exists.
    let daemon = common::spawn_daemon_serve(None, "0");
    let cwd = common::harness_tempdir().expect("create desktop-selected cwd");
    let canonical_cwd = canonical_string(cwd.path());
    start_plain_from_desktop(&daemon, canonical_cwd, &desktop_pane_id(0), PLAIN_LABEL);
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(10));
    assert_eq!(
        records[0].display_name.as_deref(),
        Some(PLAIN_LABEL),
        "precondition: the daemon registry must carry the desktop display name"
    );

    let deck = launch_tui_against(&daemon);
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            deck.snapshot_grid().contains(PLAIN_LABEL)
        }),
        "a fresh TUI attached after the desktop-shaped StartAgent must show the named agent on \
         its dashboard WITHOUT any keypress or selection, but {PLAIN_LABEL:?} never appeared.\n\
         Final grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Keep a real TUI on its empty dashboard and start a desktop-shaped
/// agent; it must render as a dashboard card before selection, and a click must
/// select that card without changing views. In a second untouched attachment,
/// start two cards and then a third, requiring the dashboard to retain all
/// three bordered cards, its three-session header, and its command-mode footer.
#[spec("newagent/visibility/001")]
#[test]
fn visibility_001_desktop_started_plain_agent_surfaces_into_attached_dashboard() {
    // Control: a TUI-native start creates its card synchronously, even for the
    // same hookless stand-in used below.
    let control = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("minimal");
    start_plain_from_tui(&control);
    control.wait_until_grid("TUI-started plain agent is visible", |grid| {
        grid.contains(PLAIN_LABEL)
    });
    drop(control);

    let daemon = common::spawn_daemon_serve(None, "0");
    let deck = launch_tui_against(&daemon);
    deck.wait_for_string("No active agents. Press Ctrl+n to create an agent.");
    assert!(
        daemon.agent_records().is_empty(),
        "precondition: the attached TUI's empty dashboard must correspond to a daemon with zero agents"
    );
    let cwd = common::harness_tempdir().expect("create desktop-selected cwd");
    let canonical_cwd = canonical_string(cwd.path());
    start_plain_from_desktop(
        &daemon,
        canonical_cwd.clone(),
        &desktop_pane_id(0),
        PLAIN_LABEL,
    );
    let first_records = daemon.wait_for_agent_count(1, Duration::from_secs(10));

    deck.wait_until_grid("first desktop start stays on one-card dashboard", |grid| {
        plain_dashboard_shows(grid, &[PLAIN_LABEL])
    });
    assert_eq!(
        first_records[0].display_name.as_deref(),
        Some(PLAIN_LABEL),
        "the first dashboard card must represent the desktop-started agent"
    );

    let (label_col, label_row) = deck.wait_for_in_grid(PLAIN_LABEL);
    deck.click(label_col, label_row);
    deck.wait_until_grid("selecting the first card keeps the dashboard", |grid| {
        plain_dashboard_shows(grid, &[PLAIN_LABEL]) && grid.contains('▸')
    });
    drop(deck);
    drop(daemon);

    // The third start reaches a TUI that has never selected or focused a card.
    let daemon = common::spawn_daemon_serve(None, "0");
    let deck = launch_tui_against(&daemon);
    deck.wait_for_string("No active agents. Press Ctrl+n to create an agent.");
    start_plain_from_desktop(
        &daemon,
        canonical_cwd.clone(),
        &desktop_pane_id(0),
        PLAIN_LABEL,
    );
    start_plain_from_desktop(
        &daemon,
        canonical_cwd,
        &desktop_pane_id(1),
        SECOND_PLAIN_LABEL,
    );
    let second_records = daemon.wait_for_agent_count(2, Duration::from_secs(10));
    deck.wait_until_grid("two desktop cards share the dashboard", |grid| {
        plain_dashboard_shows(grid, &[PLAIN_LABEL, SECOND_PLAIN_LABEL])
    });
    assert_eq!(second_records.len(), 2, "both starts must be registered");

    start_plain_from_desktop(
        &daemon,
        canonical_string(cwd.path()),
        &desktop_pane_id(2),
        THIRD_PLAIN_LABEL,
    );
    let third_records = daemon.wait_for_agent_count(3, Duration::from_secs(10));
    deck.wait_until_grid(
        "third desktop card joins the two existing dashboard cards",
        |grid| plain_dashboard_shows(grid, &[PLAIN_LABEL, SECOND_PLAIN_LABEL, THIRD_PLAIN_LABEL]),
    );
    assert_eq!(
        third_records.len(),
        3,
        "all three starts must be registered"
    );
}

/// Scenario: Attach a real TUI to an already-running empty daemon, then attach a
/// second production `DaemonClient` with its own live event subscription. Start
/// the first agent through that second client and require both subscribers to
/// observe it without any TUI input.
#[spec("newagent/visibility/001")]
#[test]
fn visibility_001_second_subscriber_start_reaches_attached_tui() {
    let daemon = common::spawn_daemon_serve(None, "0");
    let deck = launch_tui_against(&daemon);
    deck.wait_for_string("No active agents. Press Ctrl+n to create an agent.");

    let mut desktop = DesktopAttachClient::connect(&daemon.attach_socket);
    let cwd = common::harness_tempdir().expect("create two-client desktop-selected cwd");
    let pane_id = desktop_pane_id(200);
    let agent_id = desktop.start_plain(canonical_string(cwd.path()), &pane_id, TWO_CLIENT_LABEL);
    desktop.wait_for_surface_event(&pane_id);
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(10));

    assert_eq!(
        records[0].id, agent_id,
        "the agent observed through the daemon registry must be the one the second client started"
    );
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            deck.snapshot_grid().contains(TWO_CLIENT_LABEL)
        }),
        "the daemon broadcast the first start to the desktop client's own subscription, but the \
         already-attached TUI subscriber did not render {TWO_CLIENT_LABEL:?} without input. \
         Records: {records:#?}\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Let a real TUI lazily spawn its isolated daemon, then attach a
/// second production client with its own event subscription and start the first
/// desktop-shaped agent. The untouched spawning TUI must render that agent.
#[spec("newagent/visibility/001")]
#[test]
fn visibility_001_tui_spawner_receives_second_client_first_start() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents. Press Ctrl+n to create an agent.");

    let mut desktop = DesktopAttachClient::connect(deck.attach_socket_path());
    let cwd = common::harness_tempdir().expect("create lazy-spawn desktop-selected cwd");
    let pane_id = desktop_pane_id(201);
    let agent_id = desktop.start_plain(canonical_string(cwd.path()), &pane_id, LAZY_SPAWN_LABEL);
    desktop.wait_for_surface_event(&pane_id);
    let records = desktop.list_agents();

    assert_eq!(
        records.len(),
        1,
        "the TUI-spawned daemon must register exactly the second client's first agent"
    );
    assert_eq!(
        records[0].id, agent_id,
        "the lazy daemon's first registry record must be the second client's start"
    );
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            deck.snapshot_grid().contains(LAZY_SPAWN_LABEL)
        }),
        "the TUI lazily spawned the daemon and the desktop client's own subscription received \
         the first card-surface event, but the untouched spawning TUI did not render \
         {LAZY_SPAWN_LABEL:?}. Records: {records:#?}\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: After a TUI lazily spawns the daemon and a second subscribed client
/// starts its first agent, immediately perform the desktop's post-start
/// `ListAgents` refetch before consuming the queued event. The refetch ordering
/// must not keep the untouched TUI on its empty dashboard.
#[spec("newagent/visibility/001")]
#[test]
fn visibility_001_desktop_refetch_after_first_start_keeps_tui_visible() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents. Press Ctrl+n to create an agent.");

    let mut desktop = DesktopAttachClient::connect(deck.attach_socket_path());
    let cwd = common::harness_tempdir().expect("create post-start-refetch desktop-selected cwd");
    let pane_id = desktop_pane_id(202);
    let agent_id = desktop.start_plain(canonical_string(cwd.path()), &pane_id, REFETCH_LABEL);

    // The production action fetches the target-deck snapshot immediately after
    // StartAgent answers, while its independent watcher consumes the broadcast.
    // Fetch first here so the ordering is deterministic and maximally strict.
    let records = desktop.list_agents();
    desktop.wait_for_surface_event(&pane_id);

    assert_eq!(
        records.len(),
        1,
        "the immediate desktop refetch must include its accepted first start"
    );
    assert_eq!(
        records[0].id, agent_id,
        "the immediate desktop refetch must return the agent StartAgent just accepted"
    );
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            deck.snapshot_grid().contains(REFETCH_LABEL)
        }),
        "the desktop client's immediate post-start ListAgents refetch and its subscribed \
         card-surface event both completed, but the untouched TUI did not render \
         {REFETCH_LABEL:?}. Records: {records:#?}\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: On three fresh real-TUI launches, freeze startup after the daemon
/// captures an empty hydration snapshot, then deliver a desktop-shaped start
/// through the already-live event subscription before releasing that snapshot.
/// The first agent must remain visible without selection after startup settles.
#[spec("newagent/visibility/001")]
#[test]
fn visibility_001_desktop_started_plain_agent_survives_mid_attach_hydration() {
    run_mid_attach_start_attempt(1);
    run_mid_attach_start_attempt(2);
    run_mid_attach_start_attempt(3);
}

/// Scenario: First launch a three-role orchestration through the real TUI form
/// and confirm it creates a separate tab with every role visible. Then drive
/// the desktop's empty-task `PrepareWorkflow` plus one configured
/// `StartPreparedAgent` per role against a headless daemon, attach a fresh real
/// TUI without sending input, and require the named orchestration tab to be
/// rebuilt with all three role cards.
#[spec("newagent/visibility/002")]
#[test]
fn visibility_002_desktop_prepared_orchestration_rebuilds_its_tab_with_every_role() {
    // Control: the TUI's own role loop creates one orchestration tab carrying
    // all three configured roles.
    let control = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("desktop-visibility");
    start_orchestration_from_tui(&control);
    control.wait_until_grid("TUI-started orchestration tab carries every role", |grid| {
        grid.lines()
            .next()
            .is_some_and(|tabs| tabs.contains("Dashboard") && tabs.contains('│'))
            && missing_roles(grid).is_empty()
    });
    drop(control);

    // Reproduction: prepare and start every role through the desktop's daemon
    // sequence while there is no TUI attached.
    let daemon = common::spawn_daemon_serve(None, "0");
    let project = write_orchestration_project();
    let project_path = canonical_string(project.path());
    start_orchestration_from_desktop(&daemon, &project_path);

    let records = daemon.wait_for_agent_count(ORCHESTRATION_ROLES.len(), Duration::from_secs(10));
    let registered_roles: Vec<_> = records
        .iter()
        .map(|record| {
            (
                record.display_name.clone(),
                record.agent_type.clone(),
                record.tab_membership.clone(),
                record.pane_id_env.clone(),
            )
        })
        .collect();

    let deck = launch_tui_against(&daemon);
    assert!(
        common::wait_until(Duration::from_secs(15), || {
            let grid = deck.snapshot_grid();
            grid.lines().next().is_some_and(|tabs| {
                tabs.contains("Dashboard") && tabs.contains(ORCHESTRATION_TITLE)
            }) && missing_roles(&grid).is_empty()
        }),
        "a fresh TUI attached after the desktop's prepared-role loop must rebuild a distinct \
         tab titled {ORCHESTRATION_TITLE:?} with EVERY role visible. Missing roles: {:?}.\n\
         Daemon role metadata (display name, agent type, membership, pane id): {registered_roles:#?}\n\
         Final grid:\n{}",
        missing_roles(&deck.snapshot_grid()),
        deck.snapshot_grid()
    );
}

/// Scenario: Keep a real TUI attached to an empty daemon, then launch the
/// desktop's empty-task prepared orchestration. The titled tab must appear
/// without a reconnect, and switching into it must show all three role-named
/// cards with their declared agent types.
#[spec("newagent/visibility/002")]
#[test]
fn visibility_002_desktop_prepared_orchestration_surfaces_into_attached_tui_as_own_tab() {
    // Control: the TUI's own StartAgent role loop builds the separate tab.
    let control = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("desktop-visibility");
    start_orchestration_from_tui(&control);
    control.wait_until_grid("TUI-started orchestration tab carries every role", |grid| {
        grid.lines()
            .next()
            .is_some_and(|tabs| tabs.contains("Dashboard") && tabs.contains('│'))
            && missing_roles(grid).is_empty()
    });
    drop(control);

    let daemon = common::spawn_daemon_serve(None, "0");
    let deck = launch_tui_against(&daemon);
    deck.wait_for_string("No active agents");
    let project = write_orchestration_project();
    let project_path = canonical_string(project.path());
    start_orchestration_from_desktop(&daemon, &project_path);
    let records = daemon.wait_for_agent_count(ORCHESTRATION_ROLES.len(), Duration::from_secs(10));

    deck.wait_until_grid("desktop-started orchestration tab appears", |grid| {
        grid.lines()
            .next()
            .is_some_and(|tabs| tabs.contains("Dashboard") && tabs.contains(ORCHESTRATION_TITLE))
    });
    deck.send_bytes(b"\x1b[C"); // Right -> next tab -> Desktop prepared run

    assert!(
        common::wait_until(Duration::from_secs(10), || {
            let grid = deck.snapshot_grid();
            grid.lines().next().is_some_and(|tabs| {
                tabs.contains("Dashboard") && tabs.contains(ORCHESTRATION_TITLE)
            }) && grid.contains("3 agent(s)")
                && grid.contains("ClaudeCode · coordinator")
                && grid.contains("OpenCode · builder")
                && grid.contains("Pi · reviewer")
        }),
        "the already-attached TUI created tab {ORCHESTRATION_TITLE:?}, but switching into it \
         did not show exactly three sessions with the role-named ClaudeCode coordinator, \
         OpenCode builder, and Pi reviewer cards. Role metadata: {records:#?}\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Start the same named plain agent through the desktop-shaped
/// request twice against already-attached TUIs. A confirmed TUI-native close is
/// the control; a desktop-shaped `StopAgent` must remove the second card from
/// the live dashboard just as completely, without any TUI input or reconnect.
#[spec("newagent/visibility/003")]
#[test]
fn visibility_003_desktop_stop_removes_plain_agent_from_attached_dashboard() {
    // Control: stop a desktop-started card through the TUI's own confirmed
    // Ctrl+W path. This proves the card and pane can be removed normally.
    let control_daemon = common::spawn_daemon_serve(None, "0");
    let control_deck = launch_tui_against(&control_daemon);
    control_deck.wait_for_string("No active agents");
    let control_cwd = common::harness_tempdir().expect("create control cwd");
    start_plain_from_desktop(
        &control_daemon,
        canonical_string(control_cwd.path()),
        &desktop_pane_id(0),
        PLAIN_LABEL,
    );
    control_deck.wait_for_string(PLAIN_LABEL);
    let (label_col, label_row) = control_deck.wait_for_in_grid(PLAIN_LABEL);
    control_deck.click(label_col, label_row);
    control_deck.send_keys(b"\x17"); // Ctrl+W -> close confirmation
    control_deck.wait_for_string("Close selected agent?");
    control_deck.send_keys(b"\x1b[B"); // Down -> Close
    control_deck.send_keys(b"\r");
    control_deck.wait_until_grid("TUI-native stop removes the desktop-started card", |grid| {
        grid.contains("No active agents") && !grid.contains(PLAIN_LABEL)
    });
    assert!(
        common::wait_until(Duration::from_secs(10), || control_daemon
            .agent_records()
            .is_empty()),
        "control: the TUI-native close must empty the daemon registry"
    );
    drop(control_deck);
    drop(control_daemon);

    // Reproduction: the TUI stays untouched after the desktop-shaped stop.
    let daemon = common::spawn_daemon_serve(None, "0");
    let deck = launch_tui_against(&daemon);
    deck.wait_for_string("No active agents");
    let cwd = common::harness_tempdir().expect("create desktop-selected cwd");
    start_plain_from_desktop(
        &daemon,
        canonical_string(cwd.path()),
        &desktop_pane_id(0),
        PLAIN_LABEL,
    );
    deck.wait_for_string(PLAIN_LABEL);
    let record = daemon
        .wait_for_agent_count(1, Duration::from_secs(10))
        .into_iter()
        .next()
        .expect("desktop-started plain agent in daemon registry");

    // Subscribe only after the start-side surface has landed, so this buffer is
    // diagnostic evidence about the stop rather than the synthetic start.
    let stop_events = daemon.subscribe_events();
    stop_agent_from_desktop(&daemon, &record.id);
    let registry_empty = common::wait_until(Duration::from_secs(10), || {
        daemon.agent_records().is_empty()
    });
    let card_disappeared = common::wait_until(Duration::from_secs(15), || {
        let grid = deck.snapshot_grid();
        grid.contains("No active agents") && !grid.contains(PLAIN_LABEL)
    });
    let final_grid = deck.snapshot_grid();
    let published_events = stop_events.snapshot();

    assert!(
        registry_empty,
        "desktop-shaped StopAgent was accepted but the daemon registry did not empty. \
         Records: {:#?}",
        daemon.agent_records()
    );
    assert!(
        card_disappeared,
        "the desktop-shaped StopAgent emptied the daemon registry, but the already-attached \
         TUI did not remove {PLAIN_LABEL:?} WITHOUT a keypress, reconnect, or manual refresh. \
         AgentEvents published after stop: {published_events:#?}\nFinal grid:\n{final_grid}"
    );
}

/// Scenario: Start the same prepared three-role orchestration twice through
/// the desktop-shaped request sequence. A confirmed TUI-native tab close is the
/// control; concurrently stopping every role the desktop's way must remove the
/// second now-empty tab without TUI input or reconnect.
#[spec("newagent/visibility/004")]
#[test]
fn visibility_004_desktop_close_removes_orchestration_tab_from_attached_tui() {
    // Control: close the desktop-created orchestration through the TUI. The
    // native path stops all roles concurrently and removes a clean tab.
    let control_daemon = common::spawn_daemon_serve(None, "0");
    let control_deck = launch_tui_against(&control_daemon);
    control_deck.wait_for_string("No active agents");
    let control_project = write_orchestration_project();
    let control_project_path = canonical_string(control_project.path());
    start_orchestration_from_desktop(&control_daemon, &control_project_path);
    control_deck.wait_until_grid("control orchestration tab appears", |grid| {
        grid.lines()
            .next()
            .is_some_and(|tabs| tabs.contains(ORCHESTRATION_TITLE))
    });
    control_deck.send_keys(b"\x1b[C"); // Right -> Desktop prepared run
    control_deck.wait_for_string("3 agent(s)");
    control_deck.send_keys(b"\x17"); // Ctrl+W -> whole-tab confirmation
    control_deck.wait_for_string("Close this tab and all its agents?");
    control_deck.send_keys(b"\x1b[B"); // Down -> Close
    control_deck.send_keys(b"\r");
    control_deck.wait_until_grid("TUI-native close removes the orchestration tab", |grid| {
        grid.contains("No active agents") && !grid.contains(ORCHESTRATION_TITLE)
    });
    assert!(
        common::wait_until(Duration::from_secs(10), || control_daemon
            .agent_records()
            .is_empty()),
        "control: the TUI-native whole-tab close must empty the daemon registry"
    );
    drop(control_deck);
    drop(control_daemon);

    // Reproduction: close every listed role concurrently, exactly as the
    // desktop action does, while leaving the attached TUI untouched.
    let daemon = common::spawn_daemon_serve(None, "0");
    let deck = launch_tui_against(&daemon);
    deck.wait_for_string("No active agents");
    let project = write_orchestration_project();
    let project_path = canonical_string(project.path());
    start_orchestration_from_desktop(&daemon, &project_path);
    deck.wait_until_grid("desktop-started orchestration tab appears", |grid| {
        grid.lines()
            .next()
            .is_some_and(|tabs| tabs.contains(ORCHESTRATION_TITLE))
    });
    let records = daemon.wait_for_agent_count(ORCHESTRATION_ROLES.len(), Duration::from_secs(10));
    assert_eq!(
        records.len(),
        ORCHESTRATION_ROLES.len(),
        "precondition: every desktop-started role must be registered"
    );
    let role_agents: Vec<_> = records
        .iter()
        .map(|record| {
            (
                record
                    .display_name
                    .clone()
                    .unwrap_or_else(|| "<unnamed role>".into()),
                record.id.clone(),
            )
        })
        .collect();

    let stop_events = daemon.subscribe_events();
    stop_orchestration_from_desktop(&daemon, &role_agents);
    let registry_empty = common::wait_until(Duration::from_secs(10), || {
        daemon.agent_records().is_empty()
    });
    let tab_disappeared = common::wait_until(Duration::from_secs(15), || {
        let grid = deck.snapshot_grid();
        grid.contains("No active agents") && !grid.contains(ORCHESTRATION_TITLE)
    });
    let final_grid = deck.snapshot_grid();
    let published_events = stop_events.snapshot();

    assert!(
        registry_empty,
        "desktop-shaped orchestration close returned but the daemon registry did not empty. \
         Role agents: {role_agents:#?}; records: {:#?}",
        daemon.agent_records()
    );
    assert!(
        tab_disappeared,
        "concurrent desktop-shaped StopAgent requests emptied every role from the daemon, but \
         the already-attached TUI did not remove the now-empty {ORCHESTRATION_TITLE:?} tab \
         WITHOUT a keypress, reconnect, or manual refresh. Role agents: {role_agents:#?}; \
         AgentEvents published after stops: {published_events:#?}\nFinal grid:\n{final_grid}"
    );
}
