use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use dot_agent_deck::daemon_protocol::{
    KIND_DETACH, KIND_STREAM_END, KIND_STREAM_IN, KIND_STREAM_OUT, KIND_STREAM_REJECT, read_frame,
    write_frame,
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
        });
    }
    detach_agent(state, &agent_id).await;
    let daemon = trusted_daemon().await?;
    daemon.require_compatible()?;
    let connection = daemon
        .client
        .attach(&agent_id)
        .await
        .map_err(|error| safe_message(error.to_string()))?;
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
    let agent_id = state
        .sessions()?
        .get(session_id)
        .map(|session| session.agent_id.clone())
        .ok_or_else(|| format!("terminal session not found: {}", safe_message(session_id)))?;
    let daemon = trusted_daemon().await?;
    daemon.require_compatible()?;
    daemon
        .client
        .resize_agent(&agent_id, rows, cols)
        .await
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
