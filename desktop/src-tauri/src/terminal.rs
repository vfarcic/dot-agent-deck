use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use dot_agent_deck::daemon_protocol::{
    KIND_DETACH, KIND_GEOMETRY, KIND_STREAM_END, KIND_STREAM_IN, KIND_STREAM_OUT,
    KIND_STREAM_REJECT, parse_geometry_frame, read_frame, write_frame,
};
use dot_agent_deck::platform::ipc::IpcWriteHalf;
use tauri::ipc::{Channel, Response};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Mutex as AsyncMutex;

use crate::daemon_bridge::trusted_daemon;
use crate::dto::{
    TerminalAttachResult, TerminalState, TerminalStateEvent, safe_message, validate_agent_id,
    validate_dimensions, validate_terminal_input,
};

#[derive(Clone)]
struct TerminalSession {
    agent_id: String,
    channel_id: u32,
    generation: u64,
    writer: Arc<AsyncMutex<IpcWriteHalf>>,
    /// PRD #882 — the viewer token this session's attach was given, sent back on
    /// every resize so the request updates THIS tile's constraint. Two tiles can
    /// show the same agent, so the token is what tells them apart — the process
    /// identity behind the connection cannot.
    viewer: Option<String>,
}

pub(crate) struct DesktopState {
    sessions: Mutex<HashMap<String, TerminalSession>>,
    attach_gate: AsyncMutex<()>,
    next_generation: AtomicU64,
    pub(crate) watcher_started: AtomicBool,
}

impl Default for DesktopState {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            attach_gate: AsyncMutex::new(()),
            next_generation: AtomicU64::new(1),
            watcher_started: AtomicBool::new(false),
        }
    }
}

impl DesktopState {
    fn sessions(&self) -> Result<MutexGuard<'_, HashMap<String, TerminalSession>>, String> {
        self.sessions
            .lock()
            .map_err(|_| "desktop terminal session registry lock was poisoned".to_string())
    }

    pub(crate) fn start_watcher_once(&self) -> bool {
        !self.watcher_started.swap(true, Ordering::AcqRel)
    }

    fn insert_unique_session(
        &self,
        session_id: String,
        session: TerminalSession,
    ) -> Result<(), String> {
        let mut sessions = self.sessions()?;
        sessions.retain(|_, existing| existing.agent_id != session.agent_id);
        sessions.insert(session_id, session);
        Ok(())
    }
}

fn session_id(generation: u64) -> String {
    format!("terminal-{generation:016x}")
}

fn emit_terminal_state(app: &AppHandle, event: TerminalStateEvent) {
    let _ = app.emit("desktop://terminal-state", event);
}

/// PRD #882 — tell the frontend the daemon changed this agent's applied
/// geometry, so the tile can reshape its xterm grid to match.
fn emit_terminal_geometry(app: &AppHandle, event: crate::dto::TerminalGeometryEvent) {
    let _ = app.emit("desktop://terminal-geometry", event);
}

fn rejection_notice(reason: &[u8]) -> Vec<u8> {
    let reason = safe_message(String::from_utf8_lossy(reason));
    let reason = reason
        .chars()
        .map(|character| {
            if matches!(character, '\n' | '\t') {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let reason = if reason.trim().is_empty() {
        "daemon refused terminal input"
    } else {
        reason.trim()
    };
    format!("\r\n[agent-deck] terminal input rejected: {reason}\r\n").into_bytes()
}

pub(crate) async fn attach(
    app: &AppHandle,
    state: &DesktopState,
    agent_id: String,
    on_output: Channel<Response>,
    // PRD #882 — the geometry this tile can draw the agent at, measured by
    // `FitAddon` in the webview. Declaring it registers the tile as a viewer, so
    // the daemon sizes the agent to the smallest pane among every client
    // watching it and tells this tile whenever that changes.
    viewport: Option<(u16, u16)>,
) -> Result<TerminalAttachResult, String> {
    validate_agent_id(&agent_id)?;
    let _attach_guard = state.attach_gate.lock().await;
    let channel_id = on_output.id();
    if let Some((session_id, session)) = state
        .sessions()?
        .iter()
        .find(|(_, session)| session.agent_id == agent_id && session.channel_id == channel_id)
    {
        return Ok(TerminalAttachResult {
            session_id: session_id.clone(),
            agent_id,
            generation: session.generation,
            reused: true,
            // A reused session keeps whatever geometry it already has; the
            // frontend's grid is already sized to it and no attach happened.
            applied_rows: None,
            applied_cols: None,
        });
    }
    detach_agent(state, &agent_id).await;
    let daemon = trusted_daemon().await?;
    daemon.require_compatible()?;
    // PRD #882: a half-measured tile (one axis zero) declares nothing rather
    // than a geometry it does not mean — under a smallest-wins policy a bogus
    // constraint would shrink the agent for every other client too.
    let viewport = viewport.filter(|(rows, cols)| *rows > 0 && *cols > 0);
    // PRD #882: participate in the policy either way. The tile very often
    // attaches BEFORE the webview has measured it — the shown-set effect drives
    // the attach and `FitAddon` runs a frame later — and a tile that attached as
    // a non-participant would send its first resize with no viewer token, which
    // the daemon applies as an unattributed override across every other client.
    // Registering now and contributing a constraint on the first resize is the
    // difference between joining the policy and overriding it.
    let connection = match viewport {
        Some(viewport) => {
            daemon
                .client
                .attach_as_viewer(&agent_id, Some(viewport))
                .await
        }
        None => daemon.client.attach_pending_viewport(&agent_id).await,
    }
    .map_err(|error| safe_message(error.to_string()))?;
    let viewer = connection.viewer().map(|v| v.to_string());
    let applied = connection.applied();
    let (mut reader, writer) = connection.into_split();
    let generation = state.next_generation.fetch_add(1, Ordering::Relaxed);
    let session_id = session_id(generation);
    state.insert_unique_session(
        session_id.clone(),
        TerminalSession {
            agent_id: agent_id.clone(),
            channel_id,
            generation,
            writer: Arc::new(AsyncMutex::new(writer)),
            viewer,
        },
    )?;

    emit_terminal_state(
        app,
        TerminalStateEvent {
            session_id: session_id.clone(),
            agent_id: agent_id.clone(),
            generation,
            state: TerminalState::Attached,
            message: None,
        },
    );

    let app_for_stream = app.clone();
    let session_for_stream = session_id.clone();
    let agent_for_stream = agent_id.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match read_frame(&mut reader).await {
                Ok(Some((KIND_STREAM_OUT, data))) => {
                    if let Err(error) = on_output.send(Response::new(data)) {
                        emit_terminal_state(
                            &app_for_stream,
                            TerminalStateEvent {
                                session_id: session_for_stream.clone(),
                                agent_id: agent_for_stream.clone(),
                                generation,
                                state: TerminalState::Error,
                                message: Some(safe_message(format!(
                                    "terminal output channel closed: {error}"
                                ))),
                            },
                        );
                        break;
                    }
                }
                Ok(Some((KIND_STREAM_END, reason))) => {
                    emit_terminal_state(
                        &app_for_stream,
                        TerminalStateEvent {
                            session_id: session_for_stream.clone(),
                            agent_id: agent_for_stream.clone(),
                            generation,
                            state: TerminalState::End,
                            message: (!reason.is_empty())
                                .then(|| safe_message(String::from_utf8_lossy(&reason))),
                        },
                    );
                    break;
                }
                // PRD #882: the daemon applied a new geometry for this agent.
                // Non-terminal, like a rejection — the stream stays open and
                // output keeps flowing; only the grid changes.
                Ok(Some((KIND_GEOMETRY, payload))) => {
                    if let Some((rows, cols)) = parse_geometry_frame(&payload) {
                        emit_terminal_geometry(
                            &app_for_stream,
                            crate::dto::TerminalGeometryEvent {
                                session_id: session_for_stream.clone(),
                                agent_id: agent_for_stream.clone(),
                                generation,
                                rows,
                                cols,
                            },
                        );
                    }
                }
                Ok(Some((KIND_STREAM_REJECT, reason))) => {
                    // Protocol v6 rejections are explicitly non-terminal: the
                    // daemon refused one input frame because the target is no
                    // longer writable, but the attachment remains useful for
                    // output. Surface the reason in-band without tearing down
                    // the session or changing the frontend lifecycle contract.
                    if let Err(error) = on_output.send(Response::new(rejection_notice(&reason))) {
                        emit_terminal_state(
                            &app_for_stream,
                            TerminalStateEvent {
                                session_id: session_for_stream.clone(),
                                agent_id: agent_for_stream.clone(),
                                generation,
                                state: TerminalState::Error,
                                message: Some(safe_message(format!(
                                    "terminal output channel closed: {error}"
                                ))),
                            },
                        );
                        break;
                    }
                }
                Ok(None) => {
                    emit_terminal_state(
                        &app_for_stream,
                        TerminalStateEvent {
                            session_id: session_for_stream.clone(),
                            agent_id: agent_for_stream.clone(),
                            generation,
                            state: TerminalState::End,
                            message: None,
                        },
                    );
                    break;
                }
                Ok(Some((kind, _))) => {
                    emit_terminal_state(
                        &app_for_stream,
                        TerminalStateEvent {
                            session_id: session_for_stream.clone(),
                            agent_id: agent_for_stream.clone(),
                            generation,
                            state: TerminalState::Error,
                            message: Some(format!(
                                "unexpected daemon terminal frame kind 0x{kind:02x}"
                            )),
                        },
                    );
                    break;
                }
                Err(error) => {
                    emit_terminal_state(
                        &app_for_stream,
                        TerminalStateEvent {
                            session_id: session_for_stream.clone(),
                            agent_id: agent_for_stream.clone(),
                            generation,
                            state: TerminalState::Error,
                            message: Some(safe_message(error.to_string())),
                        },
                    );
                    break;
                }
            }
        }

        let state = app_for_stream.state::<DesktopState>();
        if let Ok(mut sessions) = state.sessions()
            && sessions
                .get(&session_for_stream)
                .is_some_and(|session| session.generation == generation)
        {
            sessions.remove(&session_for_stream);
        }
    });

    Ok(TerminalAttachResult {
        session_id,
        agent_id,
        generation,
        reused: false,
        // PRD #882: the geometry in force at attach time, resolved under the
        // same daemon lock as the scrollback replay that is about to arrive on
        // the channel. The frontend sizes its grid from this before writing
        // those bytes, so the replay is parsed at the geometry it was written
        // at rather than at whatever the tile happened to measure.
        applied_rows: applied.map(|(rows, _)| rows),
        applied_cols: applied.map(|(_, cols)| cols),
    })
}

pub(crate) async fn write(
    state: &DesktopState,
    session_id: &str,
    data: &[u8],
) -> Result<(), String> {
    validate_terminal_input(data)?;
    let writer = state
        .sessions()?
        .get(session_id)
        .map(|session| Arc::clone(&session.writer))
        .ok_or_else(|| format!("terminal session not found: {}", safe_message(session_id)))?;
    let mut writer = writer.lock().await;
    write_frame(&mut *writer, KIND_STREAM_IN, data)
        .await
        .map_err(|error| safe_message(error.to_string()))
}

pub(crate) async fn resize(
    state: &DesktopState,
    session_id: &str,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let (rows, cols) = validate_dimensions(rows, cols)?;
    let (agent_id, viewer) = state
        .sessions()?
        .get(session_id)
        .map(|session| (session.agent_id.clone(), session.viewer.clone()))
        .ok_or_else(|| format!("terminal session not found: {}", safe_message(session_id)))?;
    let daemon = trusted_daemon().await?;
    daemon.require_compatible()?;
    // PRD #882: name this tile's viewer so the request updates its constraint
    // rather than overriding every other client's. The daemon answers with what
    // it actually applied; the frontend learns that number from the
    // `desktop://terminal-geometry` event the daemon pushes to every viewer,
    // including this one, so nothing is returned here.
    daemon
        .client
        .resize_agent_as_viewer(&agent_id, rows, cols, viewer.as_deref())
        .await
        .map(|_| ())
        .map_err(|error| safe_message(error.to_string()))
}

pub(crate) async fn detach(state: &DesktopState, session_id: &str) -> Result<bool, String> {
    let session = state.sessions()?.remove(session_id);
    let Some(session) = session else {
        return Ok(false);
    };
    let mut writer = session.writer.lock().await;
    let _ = write_frame(&mut *writer, KIND_DETACH, &[]).await;
    Ok(true)
}

pub(crate) async fn detach_agent(state: &DesktopState, agent_id: &str) {
    let session_ids = match state.sessions() {
        Ok(sessions) => sessions
            .iter()
            .filter(|(_, session)| session.agent_id == agent_id)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>(),
        Err(_) => return,
    };
    for session_id in session_ids {
        let _ = detach(state, &session_id).await;
    }
}

pub(crate) async fn detach_all(state: &DesktopState) {
    let session_ids = match state.sessions() {
        Ok(sessions) => sessions.keys().cloned().collect::<Vec<_>>(),
        Err(_) => return,
    };
    for session_id in session_ids {
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            detach(state, &session_id),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_session_ids_are_stable_and_distinct() {
        assert_eq!(session_id(1), "terminal-0000000000000001");
        assert_ne!(session_id(1), session_id(2));
    }

    #[test]
    fn stream_rejection_notice_is_bounded_sanitized_and_non_ansi() {
        let notice = rejection_notice(b"history-only\n\x1b[31m");
        let notice = String::from_utf8(notice).unwrap();
        assert_eq!(
            notice,
            "\r\n[agent-deck] terminal input rejected: history-only [31m\r\n"
        );
        assert!(!notice.contains('\u{1b}'));
    }

    #[tokio::test]
    async fn detach_is_idempotent_for_unknown_session() {
        let state = DesktopState::default();
        assert!(!detach(&state, "terminal-missing").await.unwrap());
        assert!(!detach(&state, "terminal-missing").await.unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registry_keeps_only_one_session_per_agent() {
        fn fixture_session(agent_id: &str, generation: u64) -> TerminalSession {
            let (stream, _peer) = tokio::net::UnixStream::pair().unwrap();
            let (_, writer) = stream.into_split();
            TerminalSession {
                agent_id: agent_id.into(),
                channel_id: generation as u32,
                generation,
                writer: Arc::new(AsyncMutex::new(writer)),
                // Test seam: no attach happened, so there is no viewer token.
                viewer: None,
            }
        }

        let state = DesktopState::default();
        state
            .insert_unique_session("terminal-1".into(), fixture_session("agent-a", 1))
            .unwrap();
        state
            .insert_unique_session("terminal-2".into(), fixture_session("agent-a", 2))
            .unwrap();

        let sessions = state.sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains_key("terminal-2"));
    }
}
