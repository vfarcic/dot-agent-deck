use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tokio::sync::RwLock;

use dot_agent_deck::config::socket_path;
use dot_agent_deck::daemon::run_daemon;
use dot_agent_deck::hook::handle_hook;
use dot_agent_deck::hooks_manage;
use dot_agent_deck::state::AppState;
use dot_agent_deck::ui::run_tui;

#[derive(Parser)]
#[command(name = "dot-agent-deck", about = "AI agent session dashboard")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the dashboard (default when no subcommand)
    Dashboard,
    /// Handle a Claude Code hook event (reads stdin, sends to socket)
    Hook,
    /// Manage hook installation
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
}

#[derive(Subcommand)]
enum HooksAction {
    /// Install hooks into ~/.claude/settings.json
    Install,
    /// Remove hooks from ~/.claude/settings.json
    Uninstall,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        None | Some(Commands::Dashboard) => {
            run_dashboard();
            ExitCode::SUCCESS
        }
        Some(Commands::Hook) => handle_hook(),
        Some(Commands::Hooks { action }) => {
            match action {
                HooksAction::Install => hooks_manage::install(),
                HooksAction::Uninstall => hooks_manage::uninstall(),
            }
            ExitCode::SUCCESS
        }
    }
}

#[tokio::main]
async fn run_dashboard() {
    // Optional file-based logging when DOT_AGENT_DECK_LOG is set
    if std::env::var("DOT_AGENT_DECK_LOG").is_ok() {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("dot_agent_deck=info".parse().unwrap()),
            )
            .with_writer(std::io::stderr)
            .init();
    }

    let state = Arc::new(RwLock::new(AppState::default()));
    let path = socket_path();

    let daemon_state = state.clone();
    let daemon_path = path.clone();
    let daemon_handle = tokio::spawn(async move {
        if let Err(e) = run_daemon(&daemon_path, daemon_state).await {
            eprintln!("Daemon error: {e}");
        }
    });

    let pane_controller = dot_agent_deck::pane::detect_multiplexer();
    let tui_state = state.clone();
    let tui_result =
        tokio::task::spawn_blocking(move || run_tui(tui_state, pane_controller)).await;

    // TUI exited — clean up
    daemon_handle.abort();

    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }

    if let Err(e) = tui_result {
        eprintln!("TUI task error: {e}");
    } else if let Ok(Err(e)) = tui_result {
        eprintln!("TUI error: {e}");
    }
}
