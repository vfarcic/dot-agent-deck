use std::path::Path;

use tokio::io::AsyncBufReadExt;
use tokio::net::UnixListener;
use tracing::{error, info, warn};

use crate::error::DaemonError;
use crate::event::{AgentEvent, DaemonMessage};
use crate::state::SharedState;

pub async fn run_daemon(socket_path: &Path, state: SharedState) -> Result<(), DaemonError> {
    // Clean up stale socket file
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    info!("Daemon listening on {}", socket_path.display());

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    let reader = tokio::io::BufReader::new(stream);
                    let mut lines = reader.lines();

                    while let Ok(Some(line)) = lines.next_line().await {
                        if let Ok(msg) = serde_json::from_str::<DaemonMessage>(&line) {
                            match msg {
                                DaemonMessage::Delegate(signal) => {
                                    info!(
                                        pane_id = %signal.pane_id,
                                        targets = ?signal.to,
                                        "Received delegate signal"
                                    );
                                    state.write().await.handle_delegate(signal);
                                }
                                DaemonMessage::WorkDone(signal) => {
                                    info!(
                                        pane_id = %signal.pane_id,
                                        done = signal.done,
                                        "Received work-done signal"
                                    );
                                    state.write().await.handle_work_done(signal);
                                }
                            }
                        } else if let Ok(event) = serde_json::from_str::<AgentEvent>(&line) {
                            info!(
                                session_id = %event.session_id,
                                event_type = ?event.event_type,
                                "Received event"
                            );
                            state.write().await.apply_event(event);
                        } else {
                            warn!("Malformed event: {line}");
                        }
                    }
                });
            }
            Err(e) => {
                error!("Failed to accept connection: {e}");
            }
        }
    }
}
