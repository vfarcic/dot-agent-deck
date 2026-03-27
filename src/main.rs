use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tokio::sync::RwLock;

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

use dot_agent_deck::config::{socket_path, DashboardConfig};
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
    /// Get or set configuration values
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum HooksAction {
    /// Install hooks into ~/.claude/settings.json
    Install,
    /// Remove hooks from ~/.claude/settings.json
    Uninstall,
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
        None | Some(Commands::Dashboard) => {
            if let Some(exit_code) = maybe_exec_zellij() {
                return exit_code;
            }
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

    let config = dot_agent_deck::config::DashboardConfig::load();
    let pane_controller = dot_agent_deck::pane::detect_multiplexer();
    let tui_state = state.clone();
    let tui_result =
        tokio::task::spawn_blocking(move || run_tui(tui_state, pane_controller, config)).await;

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

/// If not already inside Zellij, launch Zellij with a layout that runs the dashboard.
/// Returns `Some(ExitCode)` if we should exit (either launched Zellij or hit an error).
/// Returns `None` if we're already inside Zellij and should proceed normally.
fn maybe_exec_zellij() -> Option<ExitCode> {
    if std::env::var("ZELLIJ").is_ok() {
        return None;
    }

    // Check if zellij is available
    if std::process::Command::new("zellij")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("Warning: Zellij not found on PATH. Running dashboard without pane control.");
        eprintln!("Install Zellij for full functionality:");
        eprintln!("  brew install zellij");
        eprintln!("  cargo install zellij");
        eprintln!("  https://zellij.dev/documentation/installation");
        // Fall through to run_dashboard() with NoopController
        return None;
    }

    // Create a private per-process runtime directory for launcher files.
    let runtime_dir = std::env::temp_dir().join(format!("dot-agent-deck-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&runtime_dir) {
        eprintln!("Failed to create runtime directory: {e}");
        return Some(ExitCode::FAILURE);
    }

    let self_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "dot-agent-deck".into());

    // Write a shell wrapper that Zellij will start as the pane "shell".
    // This is more reliable than layout `command` which Zellij sometimes ignores.
    let shell_script = format!("#!/bin/sh\nexec \"{self_path}\"\n");
    let shell_path = runtime_dir.join("shell.sh");
    let shell_path_str = shell_path.display().to_string();
    if let Err(e) = std::fs::write(&shell_path, &shell_script) {
        eprintln!("Failed to write shell script: {e}");
        return Some(ExitCode::FAILURE);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&shell_path, std::fs::Permissions::from_mode(0o755));
    }

    // Layout: two-column split — dashboard (1/3) + stacked agent panes (2/3).
    // Swap layouts define how additional panes are arranged (stacked on the right).
    let layout = format!(
        r#"layout {{
    default_tab_template {{
        children
    }}
    tab {{
        pane borderless=true command="{shell_path_str}"
    }}

    swap_tiled_layout name="dashboard" {{
        tab max_panes=1 {{
            pane borderless=true
        }}
        tab split_direction="vertical" max_panes=2 {{
            pane borderless=true size="33%"
            pane size="67%"
        }}
        tab split_direction="vertical" {{
            pane borderless=true size="33%"
            pane stacked=true size="67%" {{
                children
            }}
        }}
    }}
}}
"#
    );

    let layout_path = runtime_dir.join("layout.kdl");
    let layout_path_str = layout_path.display().to_string();
    if let Err(e) = std::fs::write(&layout_path, layout) {
        eprintln!("Failed to write layout file: {e}");
        return Some(ExitCode::FAILURE);
    }

    // Config: suppress Zellij UI chrome and the welcome/tips popup.
    let config = r#"simplified_ui true
pane_frames true
show_release_notes false
disable_session_metadata true
plugins {
    tab-bar location="zellij:tab-bar"
    status-bar location="zellij:status-bar"
    strider location="zellij:strider"
    compact-bar location="zellij:compact-bar"
    session-manager location="zellij:session-manager"
    configuration location="zellij:configuration"
    plugin-manager location="zellij:plugin-manager"
}
load_plugins {
}
keybinds clear-defaults=true {
    normal {
        bind "Alt h" "Alt Left" "Alt d" { MoveFocus "Left"; }
        bind "Alt j" "Alt Down"  { MoveFocus "Down"; }
        bind "Alt k" "Alt Up"    { MoveFocus "Up"; }
        bind "Alt w" { CloseFocus; }
        bind "Alt q" { Quit; }
    }
}
"#
    .to_string();

    let config_dir = runtime_dir.join("zellij-config");
    let config_dir_str = config_dir.display().to_string();
    let _ = std::fs::create_dir_all(&config_dir);
    let config_path = config_dir.join("config.kdl");
    if let Err(e) = std::fs::write(&config_path, &config) {
        eprintln!("Failed to write config file: {e}");
        return Some(ExitCode::FAILURE);
    }

    // Check if a session already exists and attach to it instead of destroying it.
    let session_name = "dot-agent-deck";
    if let Ok(output) = std::process::Command::new("zellij")
        .args(["list-sessions", "--short"])
        .output()
    {
        let sessions = String::from_utf8_lossy(&output.stdout);
        if sessions.lines().any(|line| line.trim() == session_name) {
            let err = std::process::Command::new("zellij")
                .args(["attach", session_name])
                .exec();
            eprintln!("Failed to attach to zellij session: {err}");
            return Some(ExitCode::FAILURE);
        }
    }

    let err = std::process::Command::new("zellij")
        .args([
            "--session",
            session_name,
            "--new-session-with-layout",
            &layout_path_str,
            "--config-dir",
            &config_dir_str,
        ])
        .exec();

    // exec() only returns on error
    eprintln!("Failed to exec zellij: {err}");
    Some(ExitCode::FAILURE)
}
