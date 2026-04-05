use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tokio::sync::RwLock;

use dot_agent_deck::config::{DashboardConfig, socket_path};
use dot_agent_deck::daemon::run_daemon;
use dot_agent_deck::hook::handle_hook;
use dot_agent_deck::hooks_manage;
use dot_agent_deck::state::{AppState, new_permission_responders};
use dot_agent_deck::theme::Theme;
use dot_agent_deck::ui::run_tui;

#[derive(Parser)]
#[command(name = "dot-agent-deck", about = "AI agent session dashboard", version = env!("DAD_VERSION"))]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
enum CliAgent {
    #[default]
    ClaudeCode,
    Opencode,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the dashboard (default when no subcommand)
    Dashboard {
        /// Color theme: auto-detect, force light, or force dark
        #[arg(long, value_enum)]
        theme: Option<Theme>,
    },
    /// Handle an agent hook event (reads stdin, sends to socket)
    Hook {
        /// Agent type
        #[arg(long, value_enum, default_value_t = CliAgent::ClaudeCode)]
        agent: CliAgent,
    },
    /// Manage hook installation
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// Get or set configuration values
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum HooksAction {
    /// Install hooks for an agent
    Install {
        /// Agent type
        #[arg(long, value_enum, default_value_t = CliAgent::ClaudeCode)]
        agent: CliAgent,
    },
    /// Remove hooks for an agent
    Uninstall {
        /// Agent type
        #[arg(long, value_enum, default_value_t = CliAgent::ClaudeCode)]
        agent: CliAgent,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Get a configuration value
    Get {
        /// Configuration key (e.g., default_command)
        key: String,
    },
    /// Set a configuration value
    Set {
        /// Configuration key (e.g., default_command)
        key: String,
        /// Value to set
        value: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        None | Some(Commands::Dashboard { theme: None }) => {
            run_dashboard(None);
            ExitCode::SUCCESS
        }
        Some(Commands::Dashboard { theme: Some(t) }) => {
            run_dashboard(Some(t));
            ExitCode::SUCCESS
        }
        Some(Commands::Hook { agent }) => {
            let agent_str = match agent {
                CliAgent::ClaudeCode => "claude-code",
                CliAgent::Opencode => "opencode",
            };
            handle_hook(agent_str)
        }
        Some(Commands::Hooks { action }) => {
            match action {
                HooksAction::Install { agent } => match agent {
                    CliAgent::Opencode => {
                        if let Err(e) = dot_agent_deck::opencode_manage::install() {
                            eprintln!("Failed to install OpenCode plugin: {e}");
                            return ExitCode::FAILURE;
                        }
                    }
                    CliAgent::ClaudeCode => hooks_manage::install(),
                },
                HooksAction::Uninstall { agent } => match agent {
                    CliAgent::Opencode => {
                        if let Err(e) = dot_agent_deck::opencode_manage::uninstall() {
                            eprintln!("Failed to uninstall OpenCode plugin: {e}");
                            return ExitCode::FAILURE;
                        }
                    }
                    CliAgent::ClaudeCode => hooks_manage::uninstall(),
                },
            }
            ExitCode::SUCCESS
        }
        Some(Commands::Config { action }) => match action {
            ConfigAction::Get { key } => {
                let config = DashboardConfig::load();
                match config.get_field(&key) {
                    Ok(value) => {
                        println!("{value}");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        ExitCode::FAILURE
                    }
                }
            }
            ConfigAction::Set { key, value } => {
                let mut config = DashboardConfig::load();
                if let Err(e) = config.set_field(&key, &value) {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
                if let Err(e) = config.save() {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
                ExitCode::SUCCESS
            }
        },
    }
}

#[tokio::main]
async fn run_dashboard(cli_theme: Option<Theme>) {
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
    let responders = new_permission_responders();
    let path = socket_path();

    let daemon_state = state.clone();
    let daemon_responders = responders.clone();
    let daemon_path = path.clone();
    let daemon_handle = tokio::spawn(async move {
        if let Err(e) = run_daemon(&daemon_path, daemon_state, daemon_responders).await {
            eprintln!("Daemon error: {e}");
        }
    });

    let version_state = state.clone();
    tokio::spawn(async move {
        if let Some(latest) = dot_agent_deck::version::check_for_update().await {
            version_state.write().await.update_available = Some(latest);
        }
    });

    let config = dot_agent_deck::config::DashboardConfig::load();
    let effective_theme = cli_theme.unwrap_or(config.theme);
    // Detect terminal theme *before* raw mode / alternate screen takes over.
    let palette = dot_agent_deck::theme::resolve_palette(effective_theme);
    let pane_controller = dot_agent_deck::pane::detect_multiplexer();
    let tui_state = state.clone();
    let tui_responders = responders.clone();
    let tui_result = tokio::task::spawn_blocking(move || {
        run_tui(tui_state, pane_controller, config, tui_responders, palette)
    })
    .await;

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
