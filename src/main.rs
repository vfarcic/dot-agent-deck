use std::io::Write as _;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use tokio::sync::RwLock;

use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_PANE_ID, DOT_AGENT_DECK_VIA_DAEMON};
use dot_agent_deck::config::{DashboardConfig, attach_socket_path, socket_path};
use dot_agent_deck::daemon::{Daemon, run_daemon_with};
use dot_agent_deck::daemon_client::DaemonClient;
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::hook::handle_hook;
use dot_agent_deck::hooks_manage;
use dot_agent_deck::pane::PaneController;
use dot_agent_deck::state::AppState;
use dot_agent_deck::theme::Theme;
use dot_agent_deck::ui::run_tui;

#[derive(Parser)]
#[command(name = "dot-agent-deck", about = "AI agent session dashboard", version = env!("DAD_VERSION"))]
struct Cli {
    /// Restore pane session from last exit
    #[arg(long = "continue")]
    continue_session: bool,

    #[command(subcommand)]
    command: Option<Commands>,

    /// Color theme: auto-detect, force light, or force dark
    #[arg(long, value_enum)]
    theme: Option<Theme>,
}

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
enum CliAgent {
    #[default]
    ClaudeCode,
    Opencode,
}

#[derive(Subcommand)]
enum Commands {
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
    /// Generate ASCII art from session context via LLM
    Ascii {
        /// User prompts / session input context
        #[arg(long)]
        input: String,
        /// Agent response / session output context
        #[arg(long)]
        output: String,
        /// LLM provider (overrides config; e.g., anthropic, openai, ollama)
        #[arg(long)]
        provider: Option<String>,
        /// LLM model (overrides config; e.g., claude-haiku-4-5, gpt-4o-mini)
        #[arg(long)]
        model: Option<String>,
    },
    /// Generate a .dot-agent-deck.toml template in the current or specified directory
    Init {
        /// Target directory (defaults to current directory)
        #[arg(short, long, default_value = ".")]
        path: std::path::PathBuf,
    },
    /// Validate a .dot-agent-deck.toml configuration file
    Validate {
        /// Target directory (defaults to current directory)
        #[arg(short, long, default_value = ".")]
        path: std::path::PathBuf,
    },
    /// Execute a command repeatedly at a fixed interval (like Linux watch)
    Watch {
        /// Refresh interval in seconds
        #[arg(long)]
        interval: u64,
        /// Command to execute
        command: String,
    },
    /// Delegate work to one or more worker roles (orchestrator only)
    Delegate {
        /// Task description with context, file paths, and constraints
        #[arg(long)]
        task: String,
        /// Role name(s) to delegate to (repeatable)
        #[arg(long)]
        to: Vec<String>,
    },
    /// Signal task completion back to the orchestrator
    WorkDone {
        /// Summary of what was accomplished
        #[arg(long)]
        task: String,
        /// Signal that the entire orchestration is complete (orchestrator only)
        #[arg(long)]
        done: bool,
    },
    /// Daemon-side subcommands. Used internally by remote transports — not
    /// part of the everyday user surface.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Manage registered remote agent environments (PRD #76).
    Remote {
        #[command(subcommand)]
        cmd: RemoteCmd,
    },
    /// Attach a local TUI to a remote daemon (PRD #76, M2.4). With no
    /// argument, runs an interactive picker over the configured remotes.
    Connect {
        /// Friendly name from `dot-agent-deck remote list`. If omitted, the
        /// picker runs.
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Bridge stdio to the local daemon's attach socket. ssh execs this on
    /// the remote host so the local TUI can speak the streaming attach
    /// protocol over the ssh pipe (PRD #76, M2.1).
    Attach,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum CliRemoteType {
    Ssh,
    Kubernetes,
}

#[derive(Subcommand)]
enum RemoteCmd {
    /// Register a remote ssh-reachable host as a deck environment.
    Add {
        /// Friendly name for the registry (e.g. hetzner-1). Must be unique.
        name: String,
        /// ssh target: `[user@]host`.
        target: String,
        /// Remote type. Only `ssh` is implemented today; `kubernetes` is
        /// planned in PRD #80.
        #[arg(long = "type", value_enum)]
        kind: CliRemoteType,
        /// ssh port.
        #[arg(long, default_value_t = dot_agent_deck::remote::DEFAULT_SSH_PORT)]
        port: u16,
        /// ssh identity file. Optional; if omitted, ssh's default key search applies.
        #[arg(long)]
        key: Option<std::path::PathBuf>,
        /// Daemon binary version to install on the remote.
        #[arg(long, default_value = env!("DAD_VERSION"))]
        version: String,
        /// Skip binary install. Pre-flight will run `dot-agent-deck --version`
        /// on the remote and require version match.
        #[arg(long = "no-install")]
        no_install: bool,
    },
    /// Print the configured remotes from the local registry. Offline metadata
    /// only — does not probe remote hosts.
    List,
    /// Remove a remote from the local registry. Does not touch the remote
    /// host (the binary and hooks remain installed there until you ssh in
    /// and clean them up explicitly).
    Remove {
        /// Friendly name of the registry entry to remove.
        name: String,
    },
    /// Re-run the binary install flow against an existing entry, then bump
    /// the registry's version field.
    Upgrade {
        /// Friendly name of the registry entry to upgrade.
        name: String,
        /// Target version. Defaults to the local client's version.
        #[arg(long, default_value = env!("DAD_VERSION"))]
        version: String,
        /// Skip binary install. Useful when the user has already swapped the
        /// binary on the remote and just wants the registry's version field
        /// updated.
        #[arg(long = "no-install")]
        no_install: bool,
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
        /// Configuration key (e.g., default_command, idle_art.provider)
        key: String,
    },
    /// Set a configuration value
    Set {
        /// Configuration key (e.g., default_command, idle_art.provider)
        key: String,
        /// Value to set
        value: String,
    },
}

fn main() -> ExitCode {
    let keys_help = dot_agent_deck::config::config_keys_help();
    let cmd = Cli::command().mut_subcommand("config", |c| {
        c.mut_subcommand("get", |g| {
            g.long_about(format!("Get a configuration value\n\n{keys_help}"))
        })
        .mut_subcommand("set", |s| {
            s.long_about(format!("Set a configuration value\n\n{keys_help}"))
        })
    });
    let cli = Cli::from_arg_matches(&cmd.get_matches())
        .expect("clap arg matches should be valid for Cli struct");

    match cli.command {
        None => {
            run_dashboard(cli.theme, cli.continue_session);
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
        Some(Commands::Ascii {
            input,
            output,
            provider,
            model,
        }) => {
            let config = DashboardConfig::load();
            let mut idle_art = config.idle_art;
            if let Some(p) = provider {
                idle_art.provider = p;
            }
            if let Some(m) = model {
                idle_art.model = m;
            }
            match run_ascii(&input, &output, &idle_art) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("Error: {e}");
                    ExitCode::FAILURE
                }
            }
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
        Some(Commands::Init { path }) => dot_agent_deck::init::run_init(&path),
        Some(Commands::Watch { interval, command }) => {
            dot_agent_deck::watch::run_watch(interval, &command)
        }
        Some(Commands::Delegate { task, to }) => {
            let pane_id = match std::env::var(DOT_AGENT_DECK_PANE_ID) {
                Ok(id) => id,
                Err(_) => {
                    eprintln!(
                        "Error: DOT_AGENT_DECK_PANE_ID environment variable not set.\nThis command should be run from within a dot-agent-deck managed pane."
                    );
                    return ExitCode::FAILURE;
                }
            };
            if to.is_empty() {
                eprintln!("Error: at least one --to <role> is required.");
                return ExitCode::FAILURE;
            }
            let signal = dot_agent_deck::event::DelegateSignal {
                pane_id,
                task,
                to,
                timestamp: chrono::Utc::now(),
            };
            let msg = dot_agent_deck::event::DaemonMessage::Delegate(signal);
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!("Failed to serialize delegate signal: {e}");
                    return ExitCode::FAILURE;
                }
            };
            if dot_agent_deck::hook::send_to_socket(&json).is_none() {
                eprintln!("Failed to send delegate signal to daemon socket.");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Some(Commands::WorkDone { task, done }) => {
            let pane_id = match std::env::var(DOT_AGENT_DECK_PANE_ID) {
                Ok(id) => id,
                Err(_) => {
                    eprintln!(
                        "Error: DOT_AGENT_DECK_PANE_ID environment variable not set.\nThis command should be run from within a dot-agent-deck managed pane."
                    );
                    return ExitCode::FAILURE;
                }
            };
            let signal = dot_agent_deck::event::WorkDoneSignal {
                pane_id,
                task,
                done,
                timestamp: chrono::Utc::now(),
            };
            let msg = dot_agent_deck::event::DaemonMessage::WorkDone(signal);
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!("Failed to serialize work-done signal: {e}");
                    return ExitCode::FAILURE;
                }
            };
            if dot_agent_deck::hook::send_to_socket(&json).is_none() {
                eprintln!("Failed to send work-done signal to daemon socket.");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Some(Commands::Daemon { cmd }) => match cmd {
            DaemonCmd::Attach => run_daemon_attach_cli(),
        },
        Some(Commands::Remote { cmd }) => match cmd {
            RemoteCmd::Add {
                name,
                target,
                kind,
                port,
                key,
                version,
                no_install,
            } => {
                let opts = dot_agent_deck::remote::AddOptions {
                    name,
                    remote_type: match kind {
                        CliRemoteType::Ssh => "ssh".to_string(),
                        CliRemoteType::Kubernetes => "kubernetes".to_string(),
                    },
                    target,
                    port,
                    key,
                    version,
                    no_install,
                    release_base: dot_agent_deck::remote::RELEASE_BASE.to_string(),
                };
                let path = dot_agent_deck::remote::default_remotes_path();
                let executor = dot_agent_deck::remote::SystemSshExecutor::new();
                match dot_agent_deck::remote::add(&opts, &executor, &path) {
                    Ok(_) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("{e}");
                        ExitCode::FAILURE
                    }
                }
            }
            RemoteCmd::List => {
                let path = dot_agent_deck::remote::default_remotes_path();
                let mut stdout = std::io::stdout().lock();
                match dot_agent_deck::remote::list(&path, &mut stdout) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("{e}");
                        ExitCode::FAILURE
                    }
                }
            }
            RemoteCmd::Remove { name } => {
                let path = dot_agent_deck::remote::default_remotes_path();
                match dot_agent_deck::remote::remove(&name, &path) {
                    Ok(_) => {
                        println!(
                            "Removed remote '{name}' from local registry. The dot-agent-deck binary on the remote and its hooks are unaffected; if you want to clean those up, ssh in and run `dot-agent-deck hooks uninstall` and `rm ~/.local/bin/dot-agent-deck`."
                        );
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        ExitCode::FAILURE
                    }
                }
            }
            RemoteCmd::Upgrade {
                name,
                version,
                no_install,
            } => {
                let opts = dot_agent_deck::remote::UpgradeOptions {
                    name,
                    version,
                    no_install,
                    release_base: dot_agent_deck::remote::RELEASE_BASE.to_string(),
                };
                let path = dot_agent_deck::remote::default_remotes_path();
                let executor = dot_agent_deck::remote::SystemSshExecutor::new();
                match dot_agent_deck::remote::upgrade(&opts, &executor, &path) {
                    Ok(_) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("{e}");
                        ExitCode::FAILURE
                    }
                }
            }
        },
        Some(Commands::Connect { name }) => run_connect(cli.theme, cli.continue_session, name),
        Some(Commands::Validate { path }) => {
            use dot_agent_deck::config_validation::{has_errors, validate_config};
            use dot_agent_deck::project_config::load_project_config;

            match load_project_config(&path) {
                Ok(None) => {
                    eprintln!("No .dot-agent-deck.toml found in {}", path.display());
                    ExitCode::FAILURE
                }
                Ok(Some(config)) => {
                    let issues = validate_config(&config);
                    if issues.is_empty() {
                        println!("Config is valid.");
                        ExitCode::SUCCESS
                    } else {
                        for issue in &issues {
                            eprintln!("{issue}");
                        }
                        if has_errors(&issues) {
                            ExitCode::FAILURE
                        } else {
                            ExitCode::SUCCESS
                        }
                    }
                }
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

#[tokio::main]
async fn run_dashboard(cli_theme: Option<Theme>, continue_session: bool) {
    init_logging_from_env();
    run_tui_session(cli_theme, continue_session).await;
}

/// Optional file-based logging from `DOT_AGENT_DECK_LOG`. Pulled out of the
/// dashboard entry point so the `connect` subcommand (which builds its own
/// tokio runtime) can call it once before launching the TUI body.
fn init_logging_from_env() {
    if let Ok(log_val) = std::env::var("DOT_AGENT_DECK_LOG") {
        let log_path = if log_val.is_empty() || log_val == "1" {
            "/tmp/dot-agent-deck.log".to_string()
        } else {
            log_val
        };
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(log_file) => {
                tracing_subscriber::fmt()
                    .with_env_filter(
                        tracing_subscriber::EnvFilter::from_default_env()
                            .add_directive("dot_agent_deck=info".parse().unwrap()),
                    )
                    .with_writer(log_file)
                    .with_ansi(false)
                    .init();
            }
            Err(e) => {
                eprintln!("Warning: failed to open log file {log_path}: {e}");
            }
        }
    }
}

/// The TUI body extracted from `run_dashboard` so `connect` can reuse it.
/// Reads `DOT_AGENT_DECK_VIA_DAEMON` + `DOT_AGENT_DECK_ATTACH_SOCKET` to
/// decide whether to spawn a daemon or attach to an existing one (M1.3
/// behavior). The `connect` subcommand sets both env vars before calling
/// this so the TUI dials the bridge socket instead of spawning a daemon.
async fn run_tui_session(cli_theme: Option<Theme>, continue_session: bool) {
    let state = Arc::new(RwLock::new(AppState::default()));
    let path = socket_path();
    let attach_path = attach_socket_path();

    // PRD #76, M1.3: developer-facing flag for stream-backed panes against an
    // already-running daemon. Env (not CLI) so it doesn't show up in --help.
    // M2.4's `connect` subcommand sets this internally so the TUI dials the
    // bridge socket. Truthy values: "1", "true", "yes" (case-insensitive).
    let via_daemon = std::env::var(DOT_AGENT_DECK_VIA_DAEMON)
        .ok()
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);

    let daemon_handle = if via_daemon {
        // The daemon is expected to already be running; skip the spawn.
        // Surface a clear error if its attach socket isn't present.
        let client = DaemonClient::new(attach_path.clone());
        if let Err(e) = client.ensure_socket_exists() {
            eprintln!(
                "remote-deck-local mode: {e}\n\
                 Start the daemon separately before running with DOT_AGENT_DECK_VIA_DAEMON=1."
            );
            return;
        }
        None
    } else {
        let daemon_state = state.clone();
        let daemon_path = path.clone();
        let daemon_attach_path = attach_path.clone();
        Some(tokio::spawn(async move {
            let daemon = Daemon::with_attach(daemon_state, daemon_attach_path);
            if let Err(e) = run_daemon_with(&daemon_path, daemon).await {
                eprintln!("Daemon error: {e}");
            }
        }))
    };

    let version_state = state.clone();
    tokio::spawn(async move {
        if let Some(latest) = dot_agent_deck::version::check_for_update().await {
            version_state.write().await.update_available = Some(latest);
        }
    });

    let config = dot_agent_deck::config::DashboardConfig::load();

    // Auto-install hooks for detected agents (silent, best-effort)
    hooks_manage::auto_install();
    dot_agent_deck::opencode_manage::auto_install();

    let effective_theme = cli_theme.unwrap_or(config.theme);
    // Detect terminal theme *before* raw mode / alternate screen takes over.
    let palette = dot_agent_deck::theme::resolve_palette(effective_theme);
    let pane_controller: Arc<dyn PaneController> = if via_daemon {
        Arc::new(EmbeddedPaneController::with_remote_deck(
            attach_path.clone(),
            tokio::runtime::Handle::current(),
        ))
    } else {
        dot_agent_deck::pane::detect_multiplexer()
    };
    let tui_state = state.clone();
    let tui_result = tokio::task::spawn_blocking(move || {
        run_tui(
            tui_state,
            pane_controller,
            config,
            palette,
            continue_session,
        )
    })
    .await;

    // TUI exited — clean up. In remote-deck-local mode the daemon was not
    // spawned by us, so we don't abort it and we don't unlink its sockets
    // (PRD #76 line 199 — agents survive TUI exit).
    if let Some(handle) = daemon_handle {
        handle.abort();
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        if attach_path.exists() {
            let _ = std::fs::remove_file(&attach_path);
        }
    }

    if let Err(e) = tui_result {
        eprintln!("TUI task error: {e}");
    } else if let Ok(Err(e)) = tui_result {
        eprintln!("TUI error: {e}");
    }
}

/// `dot-agent-deck connect [name]` — PRD #76 M2.4. Resolves the remote (by
/// name or via the picker), spawns an ssh-exec'd `daemon attach` on the
/// remote, plumbs it through a local Unix socket, sets the M1.3 env vars so
/// the in-process TUI dials that socket, then runs the TUI body. Bridge
/// teardown happens automatically via `ConnectBridge::Drop` when this
/// function returns.
#[tokio::main]
async fn run_connect(
    cli_theme: Option<Theme>,
    continue_session: bool,
    name: Option<String>,
) -> ExitCode {
    init_logging_from_env();

    let registry_path = dot_agent_deck::remote::default_remotes_path();

    // 1. Resolve the remote — either by name or via the interactive picker.
    let entry = match name {
        Some(n) => match dot_agent_deck::connect::lookup_remote(&n, &registry_path) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        },
        None => {
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            match dot_agent_deck::connect::pick_remote(&registry_path, &mut input, &mut output) {
                Ok(e) => e,
                Err(e) => {
                    let _ = output.flush();
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    // 2. Compute the bridge socket path up front so we can publish it via
    //    env *before* spawning any tokio task (see step 3).
    let target = entry.ssh_target();
    let socket_path = dot_agent_deck::connect::bridge_socket_path();

    // 2.5. M2.6 — failure-mode-aware probe. Run a one-shot ssh + list-agents
    //      BEFORE bringing up the persistent bridge so connect-time failures
    //      surface as typed errors (HostUnreachable / DaemonUnavailable) with
    //      actionable hints, instead of leaking through the TUI as garbled
    //      output. A successful probe also returns the agent list so we can
    //      print the "no agents running" empty-state hint after the bridge is
    //      up and before the TUI launches.
    let agents = match dot_agent_deck::connect::probe_remote(&target, &entry.name).await {
        Ok(list) => list,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // 3. Set the env vars the TUI's stream-backed-pane path reads
    //    (`DOT_AGENT_DECK_VIA_DAEMON` at src/main.rs ~L567,
    //    `DOT_AGENT_DECK_ATTACH_SOCKET` at src/config.rs:71). Done BEFORE
    //    `start_bridge` so the bridge listener task isn't already alive
    //    when set_var runs.
    //
    // SAFETY: `std::env::set_var` is `unsafe` since Rust 1.89 because
    // concurrent reads on another thread race with the mutation. `#[tokio::main]`
    // has already spun up the runtime's worker threads by this point, so the
    // process is multi-threaded — but the relevant invariant isn't
    // "single-threaded process", it's "no concurrent thread reads these
    // specific env vars while we mutate them". `DOT_AGENT_DECK_VIA_DAEMON`
    // and `DOT_AGENT_DECK_ATTACH_SOCKET` are read only at TUI launch
    // (`run_tui_session` via `attach_socket_path()` in src/config.rs and
    // the inline `env::var` in src/main.rs), and we control the program
    // order: nothing has been spawned yet that touches them. The values
    // also don't depend on anything from `start_bridge` — both are static
    // (`"1"` and the socket path computed above).
    unsafe {
        std::env::set_var(DOT_AGENT_DECK_VIA_DAEMON, "1");
        std::env::set_var("DOT_AGENT_DECK_ATTACH_SOCKET", &socket_path);
    }

    // 4. Bring up the bridge (binds Unix socket + spawns ssh + relay task).
    let bridge = match dot_agent_deck::connect::start_bridge(&target, socket_path.clone()).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Failed to start connect bridge: {e}");
            return ExitCode::FAILURE;
        }
    };

    // 4.5. M2.6 — empty-state hint. If the probe came back with no agents,
    //      drop a single non-blocking line BEFORE launching the TUI so the
    //      user knows to press Ctrl+N. The TUI's existing empty-dashboard
    //      state handles the rest.
    if agents.is_empty() {
        println!(
            "No agents running on '{}'. Press Ctrl+N inside the TUI to start one.",
            entry.name
        );
    }

    // 5. Run the same TUI body the default subcommand uses.
    run_tui_session(cli_theme, continue_session).await;

    // `bridge` drops here: socket file unlinked, ssh child killed via
    // kill_on_drop, listener task aborted with this tokio runtime.
    drop(bridge);
    ExitCode::SUCCESS
}

#[tokio::main]
async fn run_daemon_attach_cli() -> ExitCode {
    let path = attach_socket_path();
    match dot_agent_deck::daemon_attach::run_daemon_attach(
        &path,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn run_ascii(
    input: &str,
    output: &str,
    config: &dot_agent_deck::config::IdleArtConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let result = dot_agent_deck::ascii_art::generate_ascii_art(input, output, config).await?;
    for (i, frame) in result.frames.iter().enumerate() {
        if i > 0 {
            println!("---FRAME---");
        }
        print!("{frame}");
    }
    println!();
    Ok(())
}
