//! Streaming attach protocol for the daemon (PRD #76, M1.2).
//!
//! # Protocol versioning
//!
//! [`PROTOCOL_VERSION`] is the on-the-wire shape of this module. Bump it when
//! a change would cause an older or newer peer to mis-parse a frame:
//!
//! - **Bump:** new `KIND_*` codes, payload-schema changes that aren't
//!   forward-compatible (renames, type changes, removed fields without a
//!   `#[serde(default)]` shim), new [`AttachRequest`] variants.
//! - **Do NOT bump:** additive optional fields tagged
//!   `#[serde(default, skip_serializing_if = "Option::is_none")]` — those are
//!   forward-compatible by design (older peer ignores the field, newer peer
//!   tolerates its absence).
//!
//! The handshake itself ([`AttachRequest::Hello`]) is enforced only by the
//! laptop-side `connect` flow — single-binary in-process call sites already
//! match versions by construction and don't need the check.
//!
//! # Wire format
//!
//! Length-prefixed binary frames:
//!
//! ```text
//! +-------+--------------------+----------------------+
//! | 1 B   | 4 B (big-endian)   | N bytes              |
//! | kind  | payload length     | payload              |
//! +-------+--------------------+----------------------+
//! ```
//!
//! Justification: PRD line 294 explicitly rules out gRPC / JSON-RPC and
//! "extra build deps". We have `tokio` and `serde_json` already, so control
//! frames carry JSON and stream frames carry raw PTY bytes — no new deps,
//! and the framing is portable to stdio (M2.1). No socket-only assumptions
//! (no fd passing, no `SCM_RIGHTS`).
//!
//! # Frame kinds
//!
//! | Kind            | Direction         | Payload                       |
//! |-----------------|-------------------|-------------------------------|
//! | `KIND_REQ`      | client → server   | JSON [`AttachRequest`]        |
//! | `KIND_RESP`     | server → client   | JSON [`AttachResponse`]       |
//! | `KIND_STREAM_OUT` | server → client | raw PTY bytes                 |
//! | `KIND_STREAM_IN`  | client → server | raw bytes for PTY stdin       |
//! | `KIND_DETACH`     | client → server | empty — detach, leave agent   |
//! | `KIND_STREAM_END` | server → client | optional reason (e.g. lagged) |
//! | `KIND_EVENT`      | server → client | JSON [`crate::event::BroadcastMsg`] (M2.17/M2.19, after a `SubscribeEvents` request) |
//! | `KIND_SHUTDOWN`   | client → server | empty — shut the daemon down (PRD #92 F1) |
//! | `KIND_SHUTDOWN_ACK` | server → client | empty — acknowledges `KIND_SHUTDOWN` before teardown begins (PRD #92 F1 followup) |
//!
//! # Per-connection state machine
//!
//! 1. Client sends a single `KIND_REQ` with one of the [`AttachRequest`]
//!    variants.
//! 2. Server replies with `KIND_RESP` carrying [`AttachResponse`].
//! 3. For non-streaming ops (`list-agents`, `start-agent`, `stop-agent`,
//!    `snapshot`) the server then closes the connection. `snapshot` may
//!    emit one `KIND_STREAM_OUT` frame with the scrollback bytes, followed
//!    by `KIND_STREAM_END` and close.
//! 4. For `attach-stream`, the server immediately follows the OK response
//!    with a single `KIND_STREAM_OUT` carrying the consistent scrollback
//!    snapshot, then enters streaming mode: live PTY bytes flow as
//!    `KIND_STREAM_OUT`, client keystrokes flow as `KIND_STREAM_IN`, and
//!    either side may end via `KIND_DETACH` (client) or `KIND_STREAM_END`
//!    (server, e.g. agent died or subscriber lagged).
//!
//! # Concurrent attach
//!
//! Multiple clients may attach to the same agent. They share a single
//! [`crate::agent_pty::AgentBus`]: each subscriber gets its own broadcast
//! receiver, so PTY output fans out to every attached client. Each client's
//! `KIND_STREAM_IN` is forwarded through a shared writer (under
//! `tokio::sync::Mutex`), so concurrent keystrokes interleave at byte
//! granularity — last writer wins per byte, which matches PRD line 199's
//! "daemon is the single source of truth" model.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWrite};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tracing::{error, info, warn};

pub use crate::agent_pty::TabMembership;
use crate::agent_pty::{AgentPtyRegistry, SpawnOptions};
use crate::agent_pty::{DOT_AGENT_DECK_PANE_ID, is_valid_pane_id_env};
use crate::event::BroadcastMsg;
use crate::pane_input::escape_bytes_for_log;
use crate::state::SharedState;

// PRD #176 M1.1: the wire types (frame codec, `AttachRequest` / `AttachResponse`,
// `RunningAgentsSummary`, the frame kinds, `PROTOCOL_VERSION`, `MAX_FRAME_LEN`)
// moved to the `protocol` crate. Re-exported here so existing
// `crate::daemon_protocol::…` call sites — the CLI, the TUI client, the
// integration tests — compile unchanged. Only the daemon SERVER (below) and the
// build-id-coupled `hello_response` helper still live in the binary.
pub use protocol::{
    AttachRequest, AttachResponse, KIND_DETACH, KIND_EVENT, KIND_REQ, KIND_RESP, KIND_SHUTDOWN,
    KIND_SHUTDOWN_ACK, KIND_STREAM_END, KIND_STREAM_IN, KIND_STREAM_OUT, MAX_FRAME_LEN,
    PROTOCOL_VERSION, RunningAgentsSummary, read_frame, write_frame,
};

/// PRD #176 M1.1: build a `Hello` handshake reply carrying the daemon's
/// compile-time identity. The [`protocol::AttachResponse::hello`] constructor
/// takes `build_version` / `daemon_version` as parameters because the protocol
/// crate has no access to the binary's `env!("DAD_VERSION")` /
/// `env!("DAD_BUILD_ID")`. This helper fills them in from the binary's build
/// environment (honoring the test-only `local_build_id` override), so every
/// daemon-side `Hello` reply is constructed in exactly one place.
pub fn hello_response() -> AttachResponse {
    AttachResponse::hello(
        PROTOCOL_VERSION,
        crate::build_id::local_build_id(),
        env!("DAD_VERSION").to_string(),
    )
}

/// Bounded timeout for a single STREAM_OUT/STREAM_END write to a client. If
/// a client stops draining its socket, the OS send buffer fills and our
/// `write_all` blocks forever — which would also block lag detection (we
/// can't drain the broadcast receiver). With a per-write timeout, a wedged
/// client is dropped within this many seconds instead of pinning the output
/// task. 5s is a generous upper bound for "client can't accept a frame";
/// the client can reattach and replay scrollback.
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Try to write a single frame within `CLIENT_WRITE_TIMEOUT`. Returns
/// `true` on success and `false` if the write timed out or errored — the
/// caller should treat both as "client gone" and bail out.
async fn write_or_timeout<W: AsyncWrite + Unpin>(w: &mut W, kind: u8, payload: &[u8]) -> bool {
    matches!(
        tokio::time::timeout(CLIENT_WRITE_TIMEOUT, write_frame(w, kind, payload)).await,
        Ok(Ok(()))
    )
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Bind the attach socket and return the listener, ready for `serve_attach`.
/// Cleans up any stale socket file before binding. Split from `run_attach_server`
/// so callers (notably tests) can synchronously confirm the listener is ready
/// to accept connections before spawning the async serve loop — this removes
/// the bind/accept readiness race that the old probe-and-retry pattern was
/// papering over.
pub fn bind_attach_listener(path: &Path) -> io::Result<UnixListener> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = crate::daemon::bind_socket(path)?;
    // Defense in depth — the umask-before-bind in `bind_socket` already
    // creates the inode at 0o600; restating the mode here means any future
    // code path that bypasses `bind_socket` still ends up with the right
    // permissions.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Accept-loop half of the attach server. Runs until the listener errors out
/// or the future is dropped. Pairs with `bind_attach_listener`.
///
/// `event_tx` is the daemon-wide `BroadcastMsg` broadcast (PRD #76
/// M2.17 for hook events; extended in M2.19 to also carry delegate
/// signals). It is held here so each accepted connection can call
/// `subscribe()` if the client opens a `SubscribeEvents` stream. The
/// cost of holding a `Sender` with zero subscribers is negligible —
/// `send` only succeeds when at least one `Receiver` exists.
pub async fn serve_attach(
    listener: UnixListener,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
) -> io::Result<()> {
    // Discard counter and use an empty state so callers that don't care
    // about idle shutdown or daemon-side orchestration don't need to
    // construct either. The daemon's main path uses
    // [`serve_attach_with_counter`] with its real state.
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::RwLock;
    let dummy_count = Arc::new(AtomicUsize::new(0));
    let dummy_state: SharedState = Arc::new(RwLock::new(crate::state::AppState::default()));
    // No-counter callers (tests, the local daemon_client fallback) don't drive
    // the scheduler; hand them an empty stand-in so `ReloadSchedules`/`RunNow`
    // resolve against an empty registry rather than needing a real one.
    let dummy_scheduler = Arc::new(crate::scheduler::Scheduler::with_stderr_notifier());
    let dummy_reuse = crate::spawn::new_reuse_registry();
    serve_attach_with_counter(
        listener,
        registry,
        event_tx,
        dummy_count,
        dummy_state,
        None,
        dummy_scheduler,
        dummy_reuse,
    )
    .await
}

/// PRD #93 M1.2 variant of [`serve_attach`] that maintains `client_count`
/// across the lifetime of each accepted connection. The daemon's idle
/// monitor reads this count alongside the PTY registry size to decide when
/// the daemon may exit (both must be zero for the configured idle window).
///
/// The counter is incremented immediately after `accept` returns and
/// decremented in the per-connection task's exit branch (panic or not — the
/// `tokio::spawn` future is wrapped so the decrement always runs).
#[allow(clippy::too_many_arguments)]
pub async fn serve_attach_with_counter(
    listener: UnixListener,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
    client_count: Arc<std::sync::atomic::AtomicUsize>,
    state: SharedState,
    shutdown: Option<Arc<tokio::sync::Notify>>,
    scheduler: Arc<crate::scheduler::Scheduler>,
    reuse_registry: crate::spawn::ReuseRegistry,
) -> io::Result<()> {
    use std::sync::atomic::Ordering;
    use tokio::sync::Notify;
    // PRD #93 round-2 reviewer REV-1: the same Notify the registry uses for
    // spawn/close/exit transitions also fires on every attach-counter
    // transition. The daemon's edge-triggered idle monitor waits on it, so
    // a brief detach+reconnect wakes the monitor before any timer can fire.
    // Cloned once per accepted connection — `notify_one` is cheap and tokio
    // Notify stores a permit if no waiter is registered, so a signal sent
    // between the monitor's loop iterations isn't lost.
    let change_notify: Arc<Notify> = registry.change_notify();
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let registry = registry.clone();
                let event_tx = event_tx.clone();
                let counter = client_count.clone();
                let state = state.clone();
                let notify = change_notify.clone();
                let shutdown = shutdown.clone();
                let scheduler = scheduler.clone();
                let reuse_registry = reuse_registry.clone();
                tokio::spawn(async move {
                    // RAII guard: increments on creation, decrements on drop,
                    // so a `handle_connection` task that panics or is dropped
                    // still releases its slot in the client count. Without
                    // the guard, an unwinding task would leak a slot and
                    // keep the daemon alive past the idle threshold.
                    //
                    // The guard also signals `change_notify` on drop so the
                    // edge-triggered idle monitor wakes immediately on
                    // disconnect (PRD #93 round-2 reviewer REV-1).
                    struct ClientGuard {
                        counter: Arc<std::sync::atomic::AtomicUsize>,
                        notify: Arc<Notify>,
                    }
                    impl Drop for ClientGuard {
                        fn drop(&mut self) {
                            self.counter.fetch_sub(1, Ordering::SeqCst);
                            self.notify.notify_one();
                        }
                    }
                    counter.fetch_add(1, Ordering::SeqCst);
                    // Signal the increment too so the monitor cancels any
                    // pending shutdown timer the moment a fresh client
                    // connects, not after the next decrement.
                    notify.notify_one();
                    let _guard = ClientGuard {
                        counter: counter.clone(),
                        notify: notify.clone(),
                    };
                    if let Err(e) = handle_connection(
                        stream,
                        registry,
                        event_tx,
                        state,
                        shutdown,
                        scheduler,
                        reuse_registry,
                    )
                    .await
                    {
                        warn!("attach protocol connection error: {e}");
                    }
                });
            }
            Err(e) => {
                error!("attach accept failed: {e}");
                return Err(e);
            }
        }
    }
}

/// Bind the attach socket and serve protocol connections forever. Cleans up
/// any stale socket file before binding. Runs until the listener errors out
/// or the future is dropped.
pub async fn run_attach_server(
    path: &Path,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
) -> io::Result<()> {
    let listener = bind_attach_listener(path)?;
    info!("Attach protocol listening on {}", path.display());
    serve_attach(listener, registry, event_tx).await
}

/// PRD #93 M1.2 counter-aware sibling of [`run_attach_server`]. The daemon
/// loop uses this so the idle monitor sees attached-client transitions in
/// real time.
pub async fn run_attach_server_with_counter(
    path: &Path,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
    client_count: Arc<std::sync::atomic::AtomicUsize>,
    state: SharedState,
) -> io::Result<()> {
    let listener = bind_attach_listener(path)?;
    info!("Attach protocol listening on {}", path.display());
    let dummy_scheduler = Arc::new(crate::scheduler::Scheduler::with_stderr_notifier());
    let dummy_reuse = crate::spawn::new_reuse_registry();
    serve_attach_with_counter(
        listener,
        registry,
        event_tx,
        client_count,
        state,
        None,
        dummy_scheduler,
        dummy_reuse,
    )
    .await
}

async fn handle_connection(
    mut stream: UnixStream,
    registry: Arc<AgentPtyRegistry>,
    event_tx: broadcast::Sender<BroadcastMsg>,
    state: SharedState,
    shutdown: Option<Arc<tokio::sync::Notify>>,
    scheduler: Arc<crate::scheduler::Scheduler>,
    reuse_registry: crate::spawn::ReuseRegistry,
) -> io::Result<()> {
    let frame = match read_frame(&mut stream).await? {
        Some(f) => f,
        None => return Ok(()),
    };
    // PRD #92 F1: client → server `KIND_SHUTDOWN` is a header-only frame
    // that means "shut the daemon down now." It comes before any
    // `KIND_REQ`, so handle it before the usual request-decoding path.
    //
    // PRD #92 F1 followup hardening:
    //   (a) Reject any non-empty payload — `KIND_SHUTDOWN` is contractually
    //       header-only, and an attacker (or an upgrade-mismatch peer)
    //       smuggling bytes alongside the kind byte must not be able to
    //       trigger daemon teardown by mistake. Drop the frame silently
    //       (close the connection, do not initiate shutdown).
    //   (b) Send a `KIND_SHUTDOWN_ACK` **before** initiating teardown so
    //       the client can distinguish "daemon acknowledged" from
    //       "old daemon closed the connection on an unknown frame."
    //       Teardown can take ≥3 seconds (SIGTERM grace + SIGKILL), so
    //       the ack must be on the wire first.
    if frame.0 == KIND_SHUTDOWN {
        if !frame.1.is_empty() {
            warn!(
                payload_len = frame.1.len(),
                "KIND_SHUTDOWN rejected — frame is contractually header-only"
            );
            return Ok(());
        }
        info!("KIND_SHUTDOWN received — sending ack and beginning graceful daemon shutdown");
        // Ack first: the client's `send_shutdown` waits up to 1s for this
        // frame and treats absence as a hard error. Writing the ack
        // before kicking off the registry drain keeps the wire ordering
        // honest even if the teardown itself takes the full 3-second
        // SIGTERM grace.
        if let Err(e) = write_frame(&mut stream, KIND_SHUTDOWN_ACK, &[]).await {
            warn!(error = %e, "failed to write KIND_SHUTDOWN_ACK before shutdown — proceeding anyway");
        }
        // Drop the registry's children with a 3-second grace window for
        // SIGTERM to take effect; survivors get SIGKILL via the existing
        // teardown.
        let registry_for_shutdown = registry.clone();
        tokio::task::spawn_blocking(move || {
            registry_for_shutdown.shutdown_all_graceful(Duration::from_secs(3));
        })
        .await
        .ok();
        if let Some(s) = shutdown {
            s.notify_one();
        } else {
            // `serve_attach` (test/harness path) doesn't pass a shutdown
            // notify because tests don't run the production hook loop.
            // The registry was still drained, so the test can assert on
            // that side effect.
            warn!(
                "KIND_SHUTDOWN handled but no daemon-shutdown notify wired (likely a test harness)"
            );
        }
        return Ok(());
    }
    if frame.0 != KIND_REQ {
        let resp = AttachResponse::err(format!("expected REQ frame, got kind 0x{:02x}", frame.0));
        write_resp(&mut stream, &resp).await?;
        return Ok(());
    }
    let req: AttachRequest = match serde_json::from_slice(&frame.1) {
        Ok(r) => r,
        Err(e) => {
            let resp = AttachResponse::err(format!("malformed request: {e}"));
            write_resp(&mut stream, &resp).await?;
            return Ok(());
        }
    };

    match req {
        AttachRequest::ListAgents => {
            let records = registry.agent_records();
            write_resp(&mut stream, &AttachResponse::agent_records(records)).await?;
        }
        AttachRequest::StartAgent {
            command,
            cwd,
            rows,
            cols,
            env,
            display_name,
            tab_membership,
            agent_type,
        } => {
            // PRD #92 F1 followup hardening: refuse to start a new agent
            // while the registry's `shutting_down` latch is set. The
            // latch is flipped at the start of `shutdown_all_graceful`
            // so a race between an in-flight `StartAgent` and a
            // `KIND_SHUTDOWN` cannot spawn a new child the teardown is
            // about to miss. Reply with a clean error rather than
            // letting the spawn race the drain.
            if registry.is_shutting_down() {
                write_resp(
                    &mut stream,
                    &AttachResponse::err("start-agent: daemon is shutting down"),
                )
                .await?;
                return Ok(());
            }
            // Trust boundary: same OS user, same exec capability — see the
            // `AttachRequest::StartAgent` docs. We forward `command`/`cwd`/
            // `env` to the spawn path verbatim. The only check here is a
            // sanity guard against an empty/whitespace-only `command`,
            // which is almost certainly a client bug rather than an
            // attack: it would otherwise resolve to a binary named "" or
            // " " and fail with a confusing OS error. This is *not* an
            // allowlist.
            if let Some(c) = command.as_deref()
                && c.trim().is_empty()
            {
                write_resp(
                    &mut stream,
                    &AttachResponse::err("start-agent: command is empty or whitespace-only"),
                )
                .await?;
                return Ok(());
            }

            // PRD #93 round-5: capture the bits we need to populate the
            // daemon's `AppState` role map BEFORE the spawn (we'll need
            // the pane id from env and the orchestration metadata from
            // tab_membership). The spawn moves `opts`, so we clone what
            // we need first.
            let pane_id_env: Option<String> = env
                .iter()
                .find(|(k, _)| k == DOT_AGENT_DECK_PANE_ID)
                .map(|(_, v)| v.clone())
                .and_then(|v| {
                    if is_valid_pane_id_env(&v) {
                        Some(v)
                    } else {
                        None
                    }
                });
            // Round-11 auditor #C: also pull `orchestration_cwd` out of
            // the membership so the daemon can use it (not StartAgent.cwd)
            // as the disambiguator in `pane_orchestration_map`. This keeps
            // round-9 #2's "workers can have different per-pane cwds"
            // contract intact — pane_cwd_map gets StartAgent.cwd
            // per-pane, but pane_orchestration_map keys on the shared
            // orchestration cwd from the TabMembership.
            let orchestration_meta: Option<(String, String, bool, Option<String>)> =
                tab_membership.as_ref().and_then(|tm| match tm {
                    TabMembership::Orchestration {
                        name,
                        role_name,
                        is_start_role,
                        orchestration_cwd,
                        ..
                    } if !role_name.is_empty() => Some((
                        name.clone(),
                        role_name.clone(),
                        *is_start_role,
                        orchestration_cwd.clone(),
                    )),
                    _ => None,
                });
            let cwd_for_state = cwd.clone();

            let opts = SpawnOptions {
                command: command.as_deref(),
                cwd: cwd.as_deref(),
                display_name: display_name.as_deref(),
                rows,
                cols,
                env,
                tab_membership,
                agent_type,
            };
            match registry.spawn_agent(opts) {
                Ok(id) => {
                    // PRD #93 round-5: populate daemon-side role maps so
                    // `handle_delegate` / `handle_work_done` can resolve
                    // the worker pane and orchestrator pane purely from
                    // daemon state — no TUI round-trip, no broadcast hop.
                    // We do this only for orchestration panes; dashboard
                    // and mode panes don't participate in delegate
                    // dispatch.
                    if let (
                        Some(pane_id),
                        Some((orch_name, role_name, is_start_role, orchestration_cwd)),
                    ) = (pane_id_env.as_deref(), orchestration_meta)
                    {
                        // Round-11 auditor #C: scope the orchestration
                        // identity by `(name, orchestration_cwd)` so
                        // two unnamed orchestrations in different cwds
                        // (`~/a/foo` and `~/b/foo`, both resolving
                        // `name` to "foo") don't collide. The
                        // `orchestration_cwd` is shared across every
                        // role pane in one orchestration tab (round-9
                        // #2: per-pane cwd may diverge, but the
                        // orchestration's identity does not). Older
                        // clients that don't carry the field fall back
                        // to StartAgent.cwd — preserves backwards
                        // compat at the cost of re-opening the
                        // collision; `Some` vs `None` is detectable so
                        // this is documented behavior, not a silent
                        // misroute.
                        let orch_cwd = orchestration_cwd
                            .or_else(|| cwd_for_state.clone())
                            .unwrap_or_default();
                        let mut st = state.write().await;
                        st.register_pane(pane_id.to_string());
                        st.pane_role_map
                            .insert(pane_id.to_string(), role_name.clone());
                        st.pane_orchestration_map
                            .insert(pane_id.to_string(), (orch_name, orch_cwd));
                        if let Some(c) = cwd_for_state.clone() {
                            st.pane_cwd_map.insert(pane_id.to_string(), c);
                        }
                        if is_start_role {
                            st.orchestrator_pane_ids.insert(pane_id.to_string());
                        }
                    }
                    write_resp(&mut stream, &AttachResponse::with_id(id)).await?
                }
                Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
            }
        }
        AttachRequest::StopAgent { id } => {
            // PRD #93 round-5: capture the agent's `pane_id_env` BEFORE
            // close_agent removes the registry entry, so we can clean up
            // the daemon's per-pane role-map entries after a successful
            // close. Without this, a closed pane's role/cwd would linger
            // in the maps and a subsequent `handle_delegate` aimed at
            // that role would still resolve the dead pane.
            let pane_id_env = registry
                .agent_records()
                .into_iter()
                .find(|r| r.id == id)
                .and_then(|r| r.pane_id_env);
            // PRD #92 F8 followup (auditor #1): `close_agent` runs the
            // synchronous SIGTERM-with-grace loop in
            // `terminate_child_with_grace_and_wait`, which calls
            // `std::thread::sleep` for up to 3 s while polling the
            // child's `try_wait`. Calling that from inside the async
            // attach-connection task would block a Tokio worker thread
            // for the duration of the grace window — under load this
            // can starve other connections. Mirror the
            // `KIND_SHUTDOWN` handler's pattern: hop the blocking work
            // onto a `spawn_blocking` pool task, await the
            // `JoinHandle`, and surface a join error as a failed
            // close.
            let registry_for_close = registry.clone();
            let id_for_close = id.clone();
            let close_result =
                tokio::task::spawn_blocking(move || registry_for_close.close_agent(&id_for_close))
                    .await;
            let close_outcome: Result<(), String> = match close_result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(join_err) => {
                    tracing::warn!(
                        error = %join_err,
                        agent_id = %id,
                        "spawn_blocking for close_agent panicked or was cancelled"
                    );
                    Err(format!("close_agent task failed: {join_err}"))
                }
            };
            match close_outcome {
                Ok(()) => {
                    if let Some(pane_id) = pane_id_env {
                        state.write().await.unregister_pane(&pane_id);
                    }
                    write_resp(&mut stream, &AttachResponse::ok()).await?
                }
                Err(msg) => write_resp(&mut stream, &AttachResponse::err(msg)).await?,
            }
        }
        AttachRequest::SetAgentLabel {
            id,
            display_name,
            cwd,
        } => match registry.set_agent_label(&id, display_name, cwd) {
            Ok(()) => write_resp(&mut stream, &AttachResponse::ok()).await?,
            Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
        },
        AttachRequest::Snapshot { id } => match registry.snapshot(&id) {
            Ok(bytes) => {
                write_resp(&mut stream, &AttachResponse::ok()).await?;
                // Mirror the attach-stream / subscribe-events policy: bound the
                // body and STREAM_END writes with `CLIENT_WRITE_TIMEOUT`. A
                // client that opened a `Snapshot` connection and stopped
                // reading after the OK response could otherwise park this task
                // forever on `write_all` (kernel send buffer fills, the write
                // never completes). On timeout, best-effort STREAM_END with a
                // typed reason and return Ok(()) — a stuck client doesn't
                // justify failing the dispatcher task.
                if !bytes.is_empty()
                    && !write_or_timeout(&mut stream, KIND_STREAM_OUT, &bytes).await
                {
                    let _ = write_or_timeout(&mut stream, KIND_STREAM_END, b"timeout").await;
                    return Ok(());
                }
                if !write_or_timeout(&mut stream, KIND_STREAM_END, &[]).await {
                    return Ok(());
                }
            }
            Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
        },
        AttachRequest::AttachStream { id } => {
            handle_attach_stream(stream, registry, id).await?;
        }
        AttachRequest::Resize { id, rows, cols } => match registry.resize(&id, rows, cols) {
            Ok(()) => write_resp(&mut stream, &AttachResponse::ok()).await?,
            Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
        },
        AttachRequest::WriteAndSubmit { pane_id, text } => {
            match registry.write_to_pane_and_submit(&pane_id, &text).await {
                Ok(()) => write_resp(&mut stream, &AttachResponse::ok()).await?,
                Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
            }
        }
        AttachRequest::SubscribeEvents => {
            handle_subscribe_events(stream, event_tx).await?;
        }
        AttachRequest::Hello {
            client_version: _,
            client_build_version,
        } => {
            // PRD #76 M2.21: the daemon never enforces or rejects on
            // `client_version` — we always reply with our own
            // `PROTOCOL_VERSION` and let the caller decide. Centralizing the
            // policy on the client side means a newer client talking to an
            // older daemon (the upgrade-skew direction the daemon can't
            // detect anyway) still gets a sensible mismatch error instead of
            // the daemon rejecting what *would* be its own future shape.
            //
            // PRD #103 M1.2: log the client's build_version when present
            // for post-hoc debugging of mismatch reports. Same server
            // policy — never reject; the laptop decides.
            //
            // `client_build_version` is advisory, not trust-bearing: a
            // hostile or buggy client could embed newlines / ANSI escapes
            // that would corrupt log files or terminal display when an
            // operator tails the log. Pass through `escape_debug` to
            // render any control bytes as printable escapes before
            // formatting. The daemon-side `local_build_id()` is from our
            // own compile-time env and doesn't need the same treatment,
            // but escaping both keeps the log line consistently quoted.
            if let Some(cbv) = client_build_version.as_deref() {
                let daemon_build = crate::build_id::local_build_id();
                let cbv_safe = cbv.escape_debug().to_string();
                let daemon_build_safe = daemon_build.escape_debug().to_string();
                info!(
                    target: "daemon_protocol",
                    "Hello from client build_version=\"{cbv_safe}\" (daemon build_version=\"{daemon_build_safe}\")",
                );
            }
            // PRD #161 M1.1: enumerate the live registry so the reply carries
            // the running-agent summary (count + display names). The Part-A
            // restart prompt and Part-B connect nudge read it to say
            // "N running agents: …" before recycling the daemon. Additive and
            // optional — an older daemon omits it and the client tolerates
            // its absence.
            //
            // PRD #161 FIX 1 test knob: `DOT_AGENT_DECK_TEST_OMIT_RUNNING_AGENTS`
            // makes the reply OMIT `running_agents` (leave it `None`),
            // simulating a pre-#161 daemon so the cross-version None-agents
            // fallback (handshake FIX 1) can be exercised at L2. Gated behind
            // the same `cfg(any(test, debug_assertions))` as
            // `DOT_AGENT_DECK_BUILD_ID_OVERRIDE`, so a shipped release binary
            // compiles the hook out and can never be tricked into hiding its
            // live agents.
            #[cfg(any(test, debug_assertions))]
            let omit_running_agents =
                std::env::var_os("DOT_AGENT_DECK_TEST_OMIT_RUNNING_AGENTS").is_some();
            #[cfg(not(any(test, debug_assertions)))]
            let omit_running_agents = false;
            let mut resp = hello_response();
            if !omit_running_agents {
                let summary = RunningAgentsSummary::from_records(&registry.agent_records());
                resp = resp.with_running_agents(summary);
            }
            write_resp(&mut stream, &resp).await?;
        }
        AttachRequest::ReloadSchedules => {
            // PRD #127 M1.3: re-read the global config and diff/replace the
            // registered task set. A bad entry is surfaced via the notifier
            // and skipped; it never fails the reload. Then wake the idle
            // monitor (via the registry's change_notify) so a reload that
            // dropped the last enabled schedule lets the idle gate fire, and
            // one that added a schedule re-arms the carve-out.
            let loaded = crate::config::LoadedSchedules::load();
            scheduler.report_config_errors(&loaded.errors);
            scheduler.reload_apply(
                &loaded.tasks,
                crate::daemon::schedule_callback_factory(
                    registry.clone(),
                    reuse_registry.clone(),
                    event_tx.clone(),
                ),
            );
            registry.change_notify().notify_one();
            let names = scheduler.registered_names();
            let mut resp = AttachResponse::ok();
            resp.agents = Some(names);
            write_resp(&mut stream, &resp).await?;
        }
        AttachRequest::RunNow { name } => {
            // PRD #127 M1.5: fire the task now (the `schedule run-now` door).
            // Both started and skipped-still-running mean the task IS
            // registered → ok=true (so `wait_for_schedule_registered` and the
            // CLI treat it as success). PRD #127 C5: surface the started-vs-
            // skipped outcome in `agents` so the caller can report it
            // distinctly; an unknown task → ok=false.
            match scheduler.run_now(&name) {
                Ok(started) => {
                    let token = if started { "started" } else { "skipped" };
                    let mut resp = AttachResponse::ok();
                    resp.agents = Some(vec![token.to_string()]);
                    write_resp(&mut stream, &resp).await?
                }
                Err(e) => write_resp(&mut stream, &AttachResponse::err(e.to_string())).await?,
            }
        }
    }
    Ok(())
}

// CodeRabbit Fix C fixup: bound the response write with `CLIENT_WRITE_TIMEOUT`.
// Every dispatch arm calls `write_resp` first; without a timeout, a same-UID
// client that connected and then stopped reading could pin the dispatcher task
// on this initial OK/Err write (kernel send buffer fills, `write_all` never
// completes). On timeout, surface `io::ErrorKind::TimedOut` so existing `?`
// callers propagate up and let the connection drop.
#[doc(hidden)]
pub async fn write_resp<W: AsyncWrite + Unpin>(w: &mut W, resp: &AttachResponse) -> io::Result<()> {
    let payload = serde_json::to_vec(resp).expect("AttachResponse must serialize");
    match tokio::time::timeout(CLIENT_WRITE_TIMEOUT, write_frame(w, KIND_RESP, &payload)).await {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "write_resp: client did not drain RESP within CLIENT_WRITE_TIMEOUT",
        )),
    }
}

/// Long-lived `SubscribeEvents` handler (PRD #76 M2.17). Confirms the
/// subscription with an OK `RESP`, then forwards every hook
/// [`BroadcastMsg::Event`] from the daemon-wide broadcast as a
/// `KIND_EVENT` frame. Each write is bounded by `CLIENT_WRITE_TIMEOUT`
/// so a wedged client can't pin this task forever. A lagged receiver
/// (the client fell further behind than the broadcast capacity) closes
/// the connection with `KIND_STREAM_END` carrying `"lagged"`; the
/// TUI's reconnect path drains a `list_agents` snapshot to recover.
/// Client disconnect is detected by racing a one-byte read against
/// `rx.recv()` so the broadcast `Receiver` is dropped promptly when
/// the client goes away between messages — otherwise the
/// per-connection task and its receiver would leak for the lifetime
/// of the daemon.
///
/// PRD #93 round-5: orchestration signals (delegate / work-done) used
/// to ride this channel via `BroadcastMsg::Delegate` / `WorkDone`,
/// guarded by a replay buffer (`PendingBroadcasts`), a salvage loop on
/// detach, and a test gate to drive the salvage race. All of that is
/// gone — orchestration prompts now flow directly into target PTYs
/// (see [`AppState::handle_delegate`] /
/// [`AppState::handle_work_done`]) and the surviving PTY scrollback
/// makes a separate replay path unnecessary.
async fn handle_subscribe_events(
    stream: UnixStream,
    event_tx: broadcast::Sender<BroadcastMsg>,
) -> io::Result<()> {
    let mut rx = event_tx.subscribe();
    let (mut rd, mut wr) = stream.into_split();
    write_resp(&mut wr, &AttachResponse::ok()).await?;

    loop {
        tokio::select! {
            recv = rx.recv() => {
                match recv {
                    Ok(msg) => {
                        let payload = match serde_json::to_vec(&msg) {
                            Ok(b) => b,
                            Err(e) => {
                                // A BroadcastMsg that can't serialize is a daemon
                                // bug — log and skip rather than tear the
                                // subscription down for every other client.
                                warn!("subscribe-events: skipping unserializable broadcast: {e}");
                                continue;
                            }
                        };
                        if !write_or_timeout(&mut wr, KIND_EVENT, &payload).await {
                            let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"timeout").await;
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Daemon's event_tx dropped — daemon is shutting down.
                        let _ = write_or_timeout(&mut wr, KIND_STREAM_END, &[]).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Client fell behind beyond EVENT_BROADCAST_CAPACITY.
                        // Tear the subscription down with a typed reason so the
                        // client can drop and reconnect.
                        let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"lagged").await;
                        break;
                    }
                }
            }
            // Disconnect detector: the client never writes after the
            // SubscribeEvents request, so any read result here means the
            // socket is gone (EOF / error) or the client is misbehaving.
            // Either way, exit so the receiver drops.
            _ = rd.read_u8() => {
                break;
            }
        }
    }
    Ok(())
}

async fn handle_attach_stream(
    stream: UnixStream,
    registry: Arc<AgentPtyRegistry>,
    id: String,
) -> io::Result<()> {
    let handle = match registry.subscribe(&id) {
        Ok(h) => h,
        Err(e) => {
            let mut s = stream;
            write_resp(&mut s, &AttachResponse::err(e.to_string())).await?;
            return Ok(());
        }
    };

    let (mut rd, mut wr) = stream.into_split();

    // 1. Confirm the attach succeeded.
    write_resp(&mut wr, &AttachResponse::ok()).await?;
    // 2. Replay the consistent scrollback snapshot before live bytes start
    //    flowing. `subscribe()` guarantees no overlap or gap with the bytes
    //    delivered via `rx` below. The write is bounded by
    //    `CLIENT_WRITE_TIMEOUT` for the same reason live STREAM_OUT writes
    //    are: a client wedged at attach time would otherwise pin this task
    //    forever (kernel send buffer fills, `write_all` never completes,
    //    and the output task never even starts so lag detection can't
    //    fire). On timeout, mirror the output-task policy — best-effort
    //    bounded STREAM_END, then drop the writer and bail.
    if !handle.snapshot.is_empty()
        && !write_or_timeout(&mut wr, KIND_STREAM_OUT, &handle.snapshot).await
    {
        let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"timeout").await;
        return Ok(());
    }

    let mut rx = handle.rx;
    let writer = handle.writer;

    // PRD #128 trace-field-symmetry: cache the agent's `pane_id_env`
    // once per attach so the per-frame STREAM_IN trace can emit
    // `pane_id` alongside `agent_id` without re-locking the registry on
    // every frame. `pane_id_env` is fixed for an agent's lifetime, so
    // one lookup is enough. Three states are distinguished — the M1.4
    // cross-path diff needs to tell them apart, not just see an empty
    // string. Angle-bracket sentinels (`<agent-gone>` / `<no-pane>`)
    // can never collide with a real `pane_id_env` value because
    // `is_valid_pane_id_env` rejects `<` and `>`.
    let pane_id: String = match registry.pane_id_env_for_agent(&id) {
        Some(Some(s)) => s,
        Some(None) => "<no-pane>".to_string(),
        None => "<agent-gone>".to_string(),
    };

    // Output task: forward broadcast bytes → STREAM_OUT frames. Owns `wr`
    // for the duration of streaming.
    //
    // Every write goes through `CLIENT_WRITE_TIMEOUT`. Without it, a client
    // that stops draining its socket pins this task on `write_all` (the
    // kernel send buffer fills and the write never completes) — which also
    // suppresses lag detection, since we can't reach the next `rx.recv()`
    // to observe `RecvError::Lagged`. With the timeout, a wedged client is
    // detected within bounded time and the connection is dropped.
    let output_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(bytes) => {
                    if !write_or_timeout(&mut wr, KIND_STREAM_OUT, &bytes).await {
                        // Client wedged or socket error: try one bounded
                        // STREAM_END, then give up. If even STREAM_END
                        // can't get through, dropping `wr` here closes the
                        // socket — the client observes EOF either way.
                        let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"timeout").await;
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // Agent terminated (reader thread saw EOF).
                    let _ = write_or_timeout(&mut wr, KIND_STREAM_END, &[]).await;
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // This subscriber fell behind beyond BROADCAST_CAPACITY.
                    // Better to disconnect than to deliver corrupted ANSI;
                    // the client can reattach and replay scrollback. The
                    // bounded write timeout matters here too: if the client
                    // also wedged its socket, we still need to drop within
                    // a known time rather than block on STREAM_END.
                    let _ = write_or_timeout(&mut wr, KIND_STREAM_END, b"lagged").await;
                    break;
                }
            }
        }
    });

    // Input loop: STREAM_IN bytes are forwarded to the shared PTY writer;
    // DETACH (or unknown frame / EOF) ends the loop.
    loop {
        match read_frame(&mut rd).await {
            Ok(Some((KIND_STREAM_IN, bytes))) => {
                use std::io::Write;
                let mut w = writer.lock().await;
                // PRD #128 (cherry-picked from PR #122): byte-level trace
                // of STREAM_IN frames forwarded to the per-agent PTY
                // writer. Useful for confirming that bytes the TUI
                // queued arrived as distinct frames and that no other
                // path interleaved a write on the same writer mutex
                // between them. Gated by `RUST_LOG=trace`. Emitted
                // INSIDE the writer mutex so trace order matches actual
                // write order. Both `agent_id` and `pane_id` are
                // emitted so the M1.4 diff against the daemon-initiated
                // trace in `AgentPtyRegistry::write_to_pane_internal`
                // can join on either key (`pane_id` is cached once per
                // attach above).
                tracing::trace!(
                    target: "pane_write",
                    source = "stream_in",
                    agent_id = %id,
                    pane_id = %pane_id,
                    payload_len = bytes.len(),
                    payload = %escape_bytes_for_log(&bytes),
                    "STREAM_IN forwarded to PTY writer"
                );
                if w.write_all(&bytes).is_err() {
                    break;
                }
                let _ = w.flush();
                drop(w);
                // PRD #127 M2.2: a STREAM_IN frame is a *user* keystroke —
                // stamp the pane's deliver-on-idle debounce clock so a
                // concurrent scheduled reuse fire queues its prompt instead of
                // interrupting active typing. Keyed by `pane_id_env` (the same
                // key the reuse path delivers to).
                registry.note_user_input(&pane_id);
            }
            Ok(Some((KIND_DETACH, _))) => {
                // Explicit M2.5 detach: client signalled intent to leave the
                // agent running. Plain socket EOF takes the `Ok(None)` arm
                // below and is intentionally *not* counted as a detach —
                // only voluntary detaches bump the registry counter.
                registry.record_detach();
                break;
            }
            Ok(Some((kind, _))) => {
                warn!("unexpected frame kind 0x{kind:02x} on attach stream — closing");
                break;
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    // Stop the output task; aborting is fine because either we already saw
    // STREAM_END and the loop exited on its own, or we're detaching and the
    // client doesn't expect more bytes.
    output_task.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // PRD #176 M1.1: the pure wire-shape round-trip / codec tests moved with
    // the types into the `protocol` crate. The two tests below stay in the
    // binary because they assert on the binary's compile-time identity
    // (`env!("DAD_BUILD_ID")` / `env!("DAD_VERSION")`), which the protocol
    // crate has no access to. They exercise the `hello_response` seam that
    // bridges those build-time values into `protocol::AttachResponse::hello`.

    #[test]
    fn hello_request_serde_round_trip() {
        // PRD #76 M2.21: pin the on-wire JSON shape so a future structural
        // change to the AttachRequest enum trips the test rather than
        // silently breaking the handshake. Mirrors the
        // `kind_event_frame_round_trip` precedent.
        let req = AttachRequest::Hello {
            client_version: PROTOCOL_VERSION,
            client_build_version: Some(env!("DAD_BUILD_ID").to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["op"], "hello");
        assert_eq!(v["client_version"], PROTOCOL_VERSION);
        assert_eq!(v["client_build_version"], env!("DAD_BUILD_ID"));

        let back: AttachRequest = serde_json::from_str(&json).unwrap();
        match back {
            AttachRequest::Hello {
                client_version,
                client_build_version,
            } => {
                assert_eq!(client_version, PROTOCOL_VERSION);
                assert_eq!(client_build_version.as_deref(), Some(env!("DAD_BUILD_ID")));
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn hello_response_serde_round_trip() {
        // PRD #103 M1.1 / PRD #161 M1.1: `hello_response()` must populate
        // `build_version` from the daemon's compiled-in DAD_BUILD_ID and
        // `daemon_version` from DAD_VERSION so the laptop can detect
        // handler-code skew. The exact values are build-time-derived; require
        // they are present and match the binary's env.
        let resp = hello_response();
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["server_version"], PROTOCOL_VERSION);
        let wire_build_version = v["build_version"]
            .as_str()
            .expect("hello_response() must emit build_version on the wire");
        assert!(
            !wire_build_version.is_empty(),
            "build_version must be non-empty"
        );
        assert_eq!(wire_build_version, env!("DAD_BUILD_ID"));
        assert_eq!(v["daemon_version"], env!("DAD_VERSION"));

        let back: AttachResponse = serde_json::from_str(&json).unwrap();
        assert!(back.ok);
        assert_eq!(back.server_version, Some(PROTOCOL_VERSION));
        assert_eq!(back.build_version.as_deref(), Some(env!("DAD_BUILD_ID")));
    }
}
