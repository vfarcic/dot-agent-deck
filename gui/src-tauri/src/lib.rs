//! Thin Tauri shell over [`dad_gui_core`] (PRD #176 M1.2 + M1.3).
//!
//! All the testable logic — socket discovery, connect, the `Hello` version
//! negotiation, agent listing, the attach stream, and resize coalescing — lives
//! in `dad-gui-core` and is exercised by the Rust gates. This shell only does
//! the things that *require* a window and a webview:
//!
//! - boot a connection attempt on startup and emit the resulting
//!   [`ConnectionState`] to the webview as a `connection-state` event (M1.2);
//! - expose `connection_state` / `reconnect` so the frontend can render
//!   "connected + negotiated version" or a connect/retry affordance (M1.2);
//! - **M1.3**: expose `list_agents` and an embedded-terminal surface —
//!   `attach` opens a [`dad_gui_core::AgentStream`], pumps each `KIND_STREAM_OUT`
//!   chunk to the webview as a base64 `terminal-output` event for xterm.js, and
//!   wires `terminal_input` (keystrokes → `KIND_STREAM_IN`) and
//!   `terminal_resize` (coalesced, latest-wins → daemon `Resize`) back.
//!
//! NOTE: this crate is workspace-`exclude`d and needs the system WebKitGTK dev
//! libraries to compile; it is not part of `cargo fmt`/`clippy`/`test-fast`.

use std::path::PathBuf;
use std::sync::Mutex;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use dad_gui_core::{
    AgentRecord, ConnectionState, ResizeHandle, TabMembership, attach_socket_path, attach_stream,
    connect_or_autostart, list_agents, resize_channel, run_resize_worker,
};
use serde::Serialize;
use tauri::async_runtime::JoinHandle;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;

/// The client build id advertised in the `Hello` handshake. Informational only
/// — the daemon logs it but never rejects on it.
fn client_build_version() -> String {
    format!("dad-gui {}", env!("CARGO_PKG_VERSION"))
}

/// One attached agent's live wiring. Dropping it tears the terminal down: the
/// `input_tx` sender drops (the write-pump ends and the socket write half
/// closes — the daemon sees an implicit detach), the `resize` handle drops (the
/// resize worker ends), and the spawned tasks are aborted so the read-pump
/// stops waiting on the socket.
struct Attachment {
    agent_id: String,
    /// Keystroke bytes → write-pump → `KIND_STREAM_IN`.
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Coalesced resize producer (single-slot, latest-wins).
    resize: ResizeHandle,
    /// read-pump, write-pump, resize-worker — aborted on teardown.
    tasks: Vec<JoinHandle<()>>,
}

impl Attachment {
    /// Abort the spawned tasks. Channels/handles drop with the struct.
    fn teardown(self) {
        for t in self.tasks {
            t.abort();
        }
    }
}

/// Shared shell state: the latest connection snapshot (so a late-loading
/// webview can pull it) plus the current single embedded-terminal attachment
/// (M1.3 is one pane; M2.1 generalizes to many).
#[derive(Default)]
struct AppState {
    last: Mutex<Option<ConnectionState>>,
    attachment: Mutex<Option<Attachment>>,
}

impl AppState {
    fn set_state(&self, state: ConnectionState) {
        *self.last.lock().expect("AppState mutex poisoned") = Some(state);
    }

    fn get_state(&self) -> ConnectionState {
        self.last
            .lock()
            .expect("AppState mutex poisoned")
            .clone()
            .unwrap_or(ConnectionState::Connecting)
    }

    /// Install a new attachment, tearing down any previous one.
    fn set_attachment(&self, next: Attachment) {
        let mut guard = self.attachment.lock().expect("attachment mutex poisoned");
        if let Some(old) = guard.take() {
            old.teardown();
        }
        *guard = Some(next);
    }

    /// Tear down the current attachment, if any.
    fn clear_attachment(&self) {
        if let Some(old) = self
            .attachment
            .lock()
            .expect("attachment mutex poisoned")
            .take()
        {
            old.teardown();
        }
    }

    /// Clone the keystroke sender for the agent the webview thinks is focused,
    /// but only if it matches the live attachment (guards a stale command from
    /// a just-detached pane).
    fn input_sender(&self, agent_id: &str) -> Option<mpsc::UnboundedSender<Vec<u8>>> {
        let guard = self.attachment.lock().expect("attachment mutex poisoned");
        guard
            .as_ref()
            .filter(|a| a.agent_id == agent_id)
            .map(|a| a.input_tx.clone())
    }

    /// Clone the resize handle for the focused agent (same staleness guard as
    /// [`input_sender`]).
    fn resize_handle(&self, agent_id: &str) -> Option<ResizeHandle> {
        let guard = self.attachment.lock().expect("attachment mutex poisoned");
        guard
            .as_ref()
            .filter(|a| a.agent_id == agent_id)
            .map(|a| a.resize.clone())
    }
}

/// Emit `state` to the webview and remember it as the latest snapshot.
fn publish(app: &AppHandle, state: ConnectionState) {
    app.state::<AppState>().set_state(state.clone());
    if let Err(e) = app.emit("connection-state", &state) {
        tracing::warn!(error = %e, "failed to emit connection-state");
    }
}

/// Auto-start the daemon if needed, connect + `Hello` version negotiation, then
/// drop the probe connection (M1.3 streams agent output over its own per-agent
/// attach connections, so the Hello socket is only used to confirm
/// reachability + version). Launching the GUI with no daemon running brings the
/// daemon up here — mirroring the TUI's always-external bootstrap (PRD #93) —
/// rather than asking the user to start it. Any failure publishes the matching
/// [`ConnectionState`] so the frontend shows a connect/retry affordance with a
/// clear reason (binary not found, spawn failed, socket never appeared).
async fn bootstrap_connection(app: AppHandle) {
    publish(&app, ConnectionState::Connecting);
    let socket = attach_socket_path();
    match connect_or_autostart(&socket, Some(client_build_version())).await {
        Ok(conn) => publish(&app, conn.state()),
        Err(err) => {
            tracing::info!(error = %err, "daemon connect/auto-start failed");
            publish(&app, ConnectionState::from_connect_error(&err));
        }
    }
}

/// Which navigation bucket an agent falls into — mirrors the TUI's
/// Mode-vs-Orchestration tab split (PRD #176 M2.1). Serializes lowercase
/// (`"mode"` / `"orchestration"` / `"dashboard"`) for the webview to group on.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum Bucket {
    Mode,
    Orchestration,
    Dashboard,
}

/// A listed agent, projected to just what the webview's pane picker needs:
/// the id/label plus the tab-bucketing fields so the chrome can rebuild the
/// TUI's Mode/Orchestration tab structure from `AgentRecord.tab_membership`.
#[derive(Debug, Clone, Serialize)]
struct AgentSummary {
    id: String,
    /// `display_name` when the daemon has one, else the id (so there is always
    /// a label to show), mirroring the TUI's hydration fallback.
    label: String,
    /// Mode / Orchestration / Dashboard navigation bucket.
    bucket: Bucket,
    /// The mode or orchestration tab name; `None` for dashboard panes.
    #[serde(skip_serializing_if = "Option::is_none")]
    tab_name: Option<String>,
    /// Orchestration role position, so the webview can order role panes within
    /// a tab the way the TUI does; `None` outside an orchestration.
    #[serde(skip_serializing_if = "Option::is_none")]
    role_index: Option<usize>,
    /// Orchestration role name (e.g. `coder`), when the daemon carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    role_name: Option<String>,
}

impl From<AgentRecord> for AgentSummary {
    fn from(r: AgentRecord) -> Self {
        let label = r.display_name.unwrap_or_else(|| r.id.clone());
        let (bucket, tab_name, role_index, role_name) = match r.tab_membership {
            None => (Bucket::Dashboard, None, None, None),
            Some(TabMembership::Mode { name }) => (Bucket::Mode, Some(name), None, None),
            Some(TabMembership::Orchestration {
                name,
                role_index,
                role_name,
                ..
            }) => {
                let role = (!role_name.is_empty()).then_some(role_name);
                (Bucket::Orchestration, Some(name), Some(role_index), role)
            }
        };
        AgentSummary {
            id: r.id,
            label,
            bucket,
            tab_name,
            role_index,
            role_name,
        }
    }
}

/// Return the latest known connection state (for a webview that loaded after
/// the initial event).
#[tauri::command]
fn connection_state(state: State<'_, AppState>) -> ConnectionState {
    state.get_state()
}

/// Retry the connection on demand (the frontend's "Retry" button).
#[tauri::command]
async fn reconnect(app: AppHandle) {
    bootstrap_connection(app).await;
}

/// M1.3: list the agents the daemon is managing so the webview can pick one to
/// attach a terminal to.
#[tauri::command]
async fn agents() -> Result<Vec<AgentSummary>, String> {
    let socket = attach_socket_path();
    list_agents(&socket)
        .await
        .map(|records| records.into_iter().map(AgentSummary::from).collect())
        .map_err(|e| e.to_string())
}

/// M1.3: attach a live embedded terminal to `agent_id`. Opens an attach stream,
/// then spawns three tasks: a read-pump (daemon `KIND_STREAM_OUT` → base64
/// `terminal-output` events the webview feeds to xterm.js), a write-pump
/// (queued keystrokes → `KIND_STREAM_IN`), and the coalescing resize worker.
/// Any previous attachment is torn down first (M1.3 is a single pane).
#[tauri::command]
async fn attach(app: AppHandle, agent_id: String) -> Result<(), String> {
    let socket = attach_socket_path();
    let stream = attach_stream(&socket, &agent_id)
        .await
        .map_err(|e| e.to_string())?;
    let (mut reader, mut writer) = stream.into_split();

    // Write-pump: drain queued keystrokes and forward as KIND_STREAM_IN. Ends
    // when the sender drops (attachment torn down) or a socket write fails.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let write_task = tauri::async_runtime::spawn(async move {
        while let Some(bytes) = input_rx.recv().await {
            if let Err(e) = writer.write_input(&bytes).await {
                tracing::warn!(error = %e, "terminal write-pump ended");
                break;
            }
        }
    });

    // Resize worker: coalesced single-slot, latest-wins (mirrors the TUI).
    let (resize, resize_rx) = resize_channel();
    let resize_task = tauri::async_runtime::spawn(run_resize_worker(
        resize_rx,
        socket.clone(),
        agent_id.clone(),
    ));

    // Read-pump: stream daemon output to the webview as base64 events. On
    // stream end, emit `terminal-exit` and clear the attachment.
    let read_app = app.clone();
    let read_agent = agent_id.clone();
    let read_task = tauri::async_runtime::spawn(async move {
        loop {
            match reader.next_output().await {
                Ok(Some(bytes)) => {
                    let payload = TerminalOutput {
                        agent_id: read_agent.clone(),
                        data: BASE64.encode(&bytes),
                    };
                    if let Err(e) = read_app.emit("terminal-output", &payload) {
                        tracing::warn!(error = %e, "failed to emit terminal-output");
                        break;
                    }
                }
                Ok(None) => break, // STREAM_END / clean EOF
                Err(e) => {
                    tracing::warn!(error = %e, "terminal read-pump errored");
                    break;
                }
            }
        }
        let _ = read_app.emit(
            "terminal-exit",
            &TerminalExit {
                agent_id: read_agent.clone(),
            },
        );
        // Best-effort: drop the attachment if it's still this agent's.
        let state = read_app.state::<AppState>();
        let still_ours = {
            let guard = state.attachment.lock().expect("attachment mutex poisoned");
            guard.as_ref().is_some_and(|a| a.agent_id == read_agent)
        };
        if still_ours {
            state.clear_attachment();
        }
    });

    app.state::<AppState>().set_attachment(Attachment {
        agent_id,
        input_tx,
        resize,
        tasks: vec![read_task, write_task, resize_task],
    });
    Ok(())
}

/// M1.3: forward base64-encoded keystroke bytes from the focused xterm.js
/// terminal to the daemon as `KIND_STREAM_IN`. Base64 keeps arbitrary control
/// bytes byte-exact across the IPC boundary.
#[tauri::command]
fn terminal_input(
    state: State<'_, AppState>,
    agent_id: String,
    data: String,
) -> Result<(), String> {
    let bytes = BASE64.decode(&data).map_err(|e| e.to_string())?;
    match state.input_sender(&agent_id) {
        Some(tx) => tx
            .send(bytes)
            .map_err(|_| "terminal write-pump has ended".to_string()),
        None => Err(format!("no live attachment for agent {agent_id}")),
    }
}

/// M1.3: record a webview-side terminal resize. Coalesced single-slot — only
/// the latest size reaches the daemon (the resize worker drains it).
#[tauri::command]
fn terminal_resize(state: State<'_, AppState>, agent_id: String, rows: u16, cols: u16) {
    if let Some(handle) = state.resize_handle(&agent_id) {
        handle.resize(rows, cols);
    }
}

/// M1.3: explicitly detach the current terminal (leave the agent running).
#[tauri::command]
fn detach(state: State<'_, AppState>) {
    state.clear_attachment();
}

/// `terminal-output` event payload: one chunk of agent PTY bytes, base64.
#[derive(Debug, Clone, Serialize)]
struct TerminalOutput {
    agent_id: String,
    data: String,
}

/// `terminal-exit` event payload: the attach stream for `agent_id` ended.
#[derive(Debug, Clone, Serialize)]
struct TerminalExit {
    agent_id: String,
}

/// Resolve the daemon socket path once at startup (exposed for logging/tests).
fn socket_path_for_log() -> PathBuf {
    attach_socket_path()
}

/// Build and run the Tauri application.
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    tracing::info!(socket = %socket_path_for_log().display(), "dot-agent-deck GUI starting");

    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            connection_state,
            reconnect,
            agents,
            attach,
            terminal_input,
            terminal_resize,
            detach
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(bootstrap_connection(handle));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the dot-agent-deck GUI");
}
