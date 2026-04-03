use std::path::Path;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{error, info, warn};

use crate::error::DaemonError;
use crate::event::{AgentEvent, EventType};
use crate::state::{PermissionResponders, SharedState};

pub async fn run_daemon(
    socket_path: &Path,
    state: SharedState,
    responders: PermissionResponders,
) -> Result<(), DaemonError> {
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
                let responders = responders.clone();
                tokio::spawn(async move {
                    let (read_half, write_half) = stream.into_split();
                    let reader = BufReader::new(read_half);
                    let mut lines = reader.lines();
                    let mut write_half = Some(write_half);

                    while let Ok(Some(line)) = lines.next_line().await {
                        match serde_json::from_str::<AgentEvent>(&line) {
                            Ok(event) => {
                                info!(
                                    session_id = %event.session_id,
                                    event_type = ?event.event_type,
                                    "Received event"
                                );

                                let is_permission =
                                    event.event_type == EventType::PermissionRequest;
                                let tool_use_id = event.metadata.get("tool_use_id").cloned();

                                state.write().await.apply_event(event);

                                if is_permission {
                                    if let Some(tui_id) = tool_use_id
                                        && let Some(writer) = write_half.take()
                                    {
                                        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
                                        {
                                            let mut map = responders.lock().unwrap();
                                            map.insert(tui_id.clone(), tx);
                                        }
                                        tokio::spawn(async move {
                                            let decision = match tokio::time::timeout(
                                                std::time::Duration::from_secs(600),
                                                rx,
                                            )
                                            .await
                                            {
                                                Ok(Ok(d)) => d,
                                                Ok(Err(_)) => {
                                                    warn!(
                                                        "Permission responder dropped for {tui_id}"
                                                    );
                                                    return;
                                                }
                                                Err(_) => {
                                                    warn!("Permission timeout for {tui_id}");
                                                    return;
                                                }
                                            };
                                            let response =
                                                format!("{{\"decision\":\"{decision}\"}}\n");
                                            let mut writer = writer;
                                            let _ = writer.write_all(response.as_bytes()).await;
                                            let _ = writer.flush().await;
                                        });
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("Malformed event: {e} — input: {line}");
                            }
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
