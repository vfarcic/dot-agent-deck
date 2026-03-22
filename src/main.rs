use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::info;

use dot_agent_deck::config::socket_path;
use dot_agent_deck::daemon::run_daemon;
use dot_agent_deck::state::AppState;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("dot_agent_deck=info".parse().unwrap()),
        )
        .init();

    let state = Arc::new(RwLock::new(AppState::default()));
    let path = socket_path();

    info!("Starting dot-agent-deck daemon");

    let daemon_state = state.clone();
    let daemon_path = path.clone();
    let daemon_handle = tokio::spawn(async move {
        if let Err(e) = run_daemon(&daemon_path, daemon_state).await {
            tracing::error!("Daemon error: {e}");
        }
    });

    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for ctrl-c");

    info!("Shutting down...");
    daemon_handle.abort();

    // Clean up socket file
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }

    info!("Goodbye.");
}
