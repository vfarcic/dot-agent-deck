use std::io::Write as _;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use tokio::sync::RwLock;

use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_AGENT_ID, DOT_AGENT_DECK_PANE_ID};
use dot_agent_deck::build_version_handshake;
use dot_agent_deck::config::{DashboardConfig, attach_socket_path, socket_path};
use dot_agent_deck::daemon::{Daemon, run_daemon_with};
use dot_agent_deck::daemon_attach::ensure_external_daemon_or_die;
use dot_agent_deck::daemon_client::DaemonClient;
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::hook::handle_hook;
use dot_agent_deck::pane::PaneController;
use dot_agent_deck::state::AppState;
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
    /// PRD #20 W1: Codex ships a Claude-Code-compatible hooks engine, so its
    /// native command hooks shell `dot-agent-deck hook --agent codex`. Ingested
    /// by the [`dot_agent_deck::hook`] `"codex"` arm.
    Codex,
}

impl CliAgent {
    /// Map the CLI-surface agent selector to the registry's typed identity, so
    /// hook install/uninstall dispatch reads the integration STRATEGY from the
    /// agent registry (PRD #20 M2) instead of hardcoding which per-agent module
    /// to call for each variant.
    fn agent_type(self) -> dot_agent_deck::event::AgentType {
        match self {
            CliAgent::ClaudeCode => dot_agent_deck::event::AgentType::ClaudeCode,
            CliAgent::Opencode => dot_agent_deck::event::AgentType::OpenCode,
            CliAgent::Codex => dot_agent_deck::event::AgentType::Codex,
        }
    }
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
        /// Task description with context, file paths, and constraints.
        /// Mutually exclusive with --task-file.
        #[arg(long, conflicts_with = "task_file")]
        task: Option<String>,
        /// Read the task text verbatim from a file (or `-` for stdin). The
        /// shell-safe way to pass a task containing backticks, quotes, `$VAR`,
        /// or newlines, which --task would otherwise let the caller's shell
        /// mangle. Mutually exclusive with --task.
        #[arg(long = "task-file", value_name = "PATH")]
        task_file: Option<String>,
        /// Role name(s) to delegate to (repeatable)
        #[arg(long)]
        to: Vec<String>,
    },
    /// Signal task completion back to the orchestrator
    WorkDone {
        /// Summary of what was accomplished. Mutually exclusive with
        /// --task-file.
        #[arg(long, conflicts_with = "task_file")]
        task: Option<String>,
        /// Read the summary text verbatim from a file (or `-` for stdin). The
        /// shell-safe way to pass a summary containing backticks, quotes,
        /// `$VAR`, or newlines. Mutually exclusive with --task.
        #[arg(long = "task-file", value_name = "PATH")]
        task_file: Option<String>,
        /// Signal that the entire orchestration is complete (orchestrator only)
        #[arg(long)]
        done: bool,
    },
    /// Report an agent lifecycle state so the pane's card status updates
    /// (PRD #201 M1.2). Used by an agent's extension (e.g. the bundled Pi
    /// extension) to drive status with NO hook installed: it rides the
    /// existing raw-`AgentEvent` socket path.
    AgentEvent {
        /// Lifecycle state: one of `running`, `waiting`, `finished`.
        #[arg(long = "type")]
        r#type: String,
    },
    /// Print the seed/prompt the daemon prepared for this pane, then clear it
    /// (PRD #201 native prompt delivery). READ-ONLY: it asks the daemon over
    /// the hook socket for the pane's pending seed and prints it to stdout
    /// (empty output = no seed). The bundled Pi extension shells this on
    /// `session_start` and, if the output is non-empty, delivers it natively
    /// via `pi.sendUserMessage` — so a Pi pane's first prompt no longer needs
    /// PTY keystroke injection. Uses `DOT_AGENT_DECK_PANE_ID` to scope the
    /// request, exactly like `agent-event`.
    GetSeed,
    /// Set up the Pi orchestrator integration (PRD #201). Detects `pi` on
    /// PATH, materializes the bundled orchestrator extension into Pi's global
    /// extension dir, and enables it (Pi auto-discovers the dir). Prints the
    /// one-line install hint and exits non-zero if `pi` is absent.
    Orchestrator {
        #[command(subcommand)]
        cmd: OrchestratorCmd,
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
    /// Manage cron-scheduled prompts (PRD #127). The single validated writer
    /// for the global `~/.config/dot-agent-deck/schedules.toml`: every
    /// mutating subcommand validates the cron, expands `~`/`$VAR`, writes the
    /// global file atomically regardless of cwd, and triggers a live daemon
    /// reload.
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },
    /// Manage the local saved-session snapshot (PRD #89). Auto-restore reads
    /// this on-disk snapshot on every TUI startup; this group is the local
    /// fresh-start escape hatch. A subcommand group (not a bare flag) so future
    /// snapshot operations can be added without changing the surface.
    Snapshot {
        #[command(subcommand)]
        cmd: SnapshotCmd,
    },
    /// Wrap an agent command, passing its stdio through transparently while
    /// tee-ing output through pattern detection into `AgentEvent`s (PRD #20 M6
    /// — the generic stdout-wrapper integration strategy). The child stays
    /// fully interactive; recognised output lines drive the pane's card status,
    /// and the child's exit code becomes the wrapper's exit code. Usage:
    /// `dot-agent-deck wrap [--agent <name>] -- <command> <args...>`.
    Wrap {
        /// Optional agent identity override (a registry basename, e.g.
        /// `claude`). When omitted, the type is inferred from the wrapped
        /// command's binary.
        #[arg(long)]
        agent: Option<String>,
        /// The agent command and its arguments, taken verbatim after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ScheduleAction {
    /// Add a new scheduled task.
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        cron: String,
        #[arg(long = "working-dir")]
        working_dir: String,
        #[arg(long)]
        command: Option<String>,
        #[arg(long)]
        prompt: String,
        // PRD #127 B1: accept an explicit `<true|false>` value (ArgAction::Set),
        // consistent with `update` and what the authoring seed prompt + docs
        // tell the agent to pass. A bare `SetTrue` flag here would reject the
        // value the primary agent-driven path supplies.
        #[arg(long = "new-tab-per-fire", action = clap::ArgAction::Set, default_value_t = false)]
        new_tab_per_fire: bool,
        #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
        enabled: bool,
        // PRD #120: issue-dispatch knobs. When `--repo` is present this `add`
        // authors an ISSUE-DISPATCH task (writes `[scheduled_tasks.issue_dispatch]`,
        // and `--command` is optional — the per-issue command comes from each
        // cloned repo's config). `--repo` is validated as a strict `owner/name`
        // slug.
        #[arg(long)]
        repo: Option<String>,
        #[arg(long = "max-per-run")]
        max_per_run: Option<usize>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        query: Option<String>,
    },
    /// Update fields of an existing task. Rename is forbidden — there is no
    /// name-change flag; `name` selects the task to edit.
    Update {
        #[arg(long)]
        name: String,
        #[arg(long)]
        cron: Option<String>,
        #[arg(long = "working-dir")]
        working_dir: Option<String>,
        #[arg(long)]
        command: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long = "new-tab-per-fire")]
        new_tab_per_fire: Option<bool>,
        #[arg(long)]
        enabled: Option<bool>,
    },
    /// Remove a task definition (does not kill an open tab for it).
    Remove {
        #[arg(long)]
        name: String,
    },
    /// List scheduled tasks with their enabled/disabled state and next-fire.
    List,
    /// Enable a task.
    Enable {
        #[arg(long)]
        name: String,
    },
    /// Disable a task (keeps the definition; stops it firing).
    Disable {
        #[arg(long)]
        name: String,
    },
    /// Fire a task now via the running daemon.
    RunNow {
        #[arg(long)]
        name: String,
    },
    /// Ask the running daemon to re-read the global config.
    Reload,
}

#[derive(Subcommand)]
enum OrchestratorCmd {
    /// Detect `pi`, then materialize + enable the bundled orchestrator
    /// extension in Pi's global extension dir. Idempotent (re-run to refresh a
    /// stale copy). Exits non-zero with the install hint when `pi` is absent.
    Setup,
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Run the daemon as a foreground process, binding the hook-ingestion
    /// and streaming-attach sockets but **not** launching the TUI. Used
    /// internally by lazy-spawn-on-attach (PRD #76, M4.3) and by callers
    /// that want a long-lived daemon to outlive the spawning shell. Not
    /// part of the everyday user surface.
    Serve,
    /// Print the binary's attach-protocol version as JSON. Used by the
    /// laptop-side `connect` flow (PRD #76 M2.21) to detect wire-format skew
    /// across an ssh hop without spawning the remote daemon: the protocol
    /// version is compiled into the binary, so a static print is equivalent
    /// to a Hello round-trip against a running daemon. Output is a JSON
    /// `AttachResponse` carrying `server_version` so the client side can
    /// reuse its existing deserializer.
    Hello,
    /// Stop the local daemon gracefully (SIGTERM, then poll for it to
    /// stop accepting connections). PRD #103 Phase 3 — documented
    /// alternative to `kill -9` after upgrading the binary. Refuses
    /// without `--force` when managed agents are still running.
    Stop {
        /// Terminate even when managed agents are running, and escalate
        /// to SIGKILL if SIGTERM doesn't take effect within the grace
        /// window. Data-loss guard — only pass this when you have
        /// already detached anything you cared about.
        #[arg(long)]
        force: bool,
    },
    /// Stop the local daemon (same flags as `stop`). The next
    /// `dot-agent-deck` invocation lazy-spawns a fresh daemon.
    Restart {
        /// See `stop --force`.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
enum CliRemoteType {
    #[default]
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
        /// Remote type. Defaults to `ssh` (the only transport implemented today);
        /// `kubernetes` is planned in PRD #81.
        #[arg(long = "type", value_enum, default_value_t = CliRemoteType::Ssh)]
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
enum SnapshotCmd {
    /// Delete the local saved-session snapshot. With auto-restore on by
    /// default (PRD #89), this is the one obvious "start fresh" action for the
    /// local deck: the next `dot-agent-deck` startup begins from an empty
    /// dashboard instead of restoring the previous workspace. Registry-only
    /// `remote remove` intentionally does NOT touch this global snapshot.
    Clear,
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

/// Resolve the task/summary text for `delegate` / `work-done` from the mutually
/// exclusive `--task` / `--task-file` inputs.
///
/// `--task-file <path>` reads the text **verbatim** from a file — no shell is
/// involved, so backticks, quotes, `$VAR`, and newlines survive unmangled
/// (the whole point: `--task "…`code`…"` lets the caller's shell
/// command-substitute the backticks before we ever run). `--task-file -` reads
/// stdin instead. clap's `conflicts_with` already rejects passing *both*; this
/// function rejects passing *neither* and surfaces file/stdin read errors.
fn resolve_task(
    task: Option<String>,
    task_file: Option<String>,
    stdin: impl std::io::Read,
) -> Result<String, String> {
    match (task, task_file) {
        (Some(t), None) => Ok(t),
        (None, Some(path)) => read_task_file(&path, stdin),
        // clap `conflicts_with` normally prevents this; kept as a defensive
        // guard so the invariant holds even if the two are ever resolved
        // outside clap parsing.
        (Some(_), Some(_)) => {
            Err("--task and --task-file are mutually exclusive; pass exactly one".to_string())
        }
        (None, None) => Err(
            "provide the task via --task <text> or --task-file <path> (use `-` for stdin)"
                .to_string(),
        ),
    }
}

/// Read task text verbatim from `path`, or from `stdin` when `path` is `-`.
fn read_task_file(path: &str, mut stdin: impl std::io::Read) -> Result<String, String> {
    if path == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut stdin, &mut buf)
            .map_err(|e| format!("failed to read task from stdin: {e}"))?;
        Ok(buf)
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("failed to read task file '{path}': {e}"))
    }
}

fn main() -> ExitCode {
    // PRD #89 M3.4: the `--continue` flag was removed — auto-restore is now the
    // default. Intercept a stale invocation before clap parsing so the user
    // gets a guiding message ("auto-restore is the default; just run
    // `dot-agent-deck`") instead of clap's bare "unexpected argument" error.
    // The exit is non-zero so wrapper scripts still fail loudly until updated.
    // Review-fix F8: also match the `--continue=<value>` form (e.g. a wrapper
    // that passed `--continue=true`) so it keeps the friendly message instead of
    // falling through to clap's generic error.
    if std::env::args().any(|a| a == "--continue" || a.starts_with("--continue=")) {
        eprintln!(
            "error: the `--continue` flag has been removed. Auto-restore is now the default — \
             just run `dot-agent-deck` (no flag) and your previous session is restored \
             automatically."
        );
        return ExitCode::FAILURE;
    }

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
        None => run_dashboard(),
        Some(Commands::Hook { agent }) => {
            let agent_str = match agent {
                CliAgent::ClaudeCode => "claude-code",
                CliAgent::Opencode => "opencode",
                CliAgent::Codex => "codex",
            };
            handle_hook(agent_str)
        }
        Some(Commands::Hooks { action }) => {
            // PRD #20 finding #15: dispatch through the SPEC's own handler rather
            // than a strategy-keyed hardcoded incumbent. Behaviour is unchanged
            // for the two CLI agents — ClaudeCode installs its native hooks,
            // Opencode its plugin — but a FUTURE agent (even one reusing an
            // existing strategy) installs correctly from just its own registry
            // handler, never another agent's module.
            use dot_agent_deck::agent_registry;
            match action {
                HooksAction::Install { agent } => {
                    let spec = agent_registry::spec(&agent.agent_type());
                    match spec.hook_install {
                        Some(install) => {
                            if let Err(e) = install() {
                                eprintln!("Failed to install {} hooks: {e}", spec.label);
                                return ExitCode::FAILURE;
                            }
                        }
                        None => {
                            eprintln!("No hook installer for agent {}", spec.label);
                            return ExitCode::FAILURE;
                        }
                    }
                }
                HooksAction::Uninstall { agent } => {
                    let spec = agent_registry::spec(&agent.agent_type());
                    match spec.hook_uninstall {
                        Some(uninstall) => {
                            if let Err(e) = uninstall() {
                                eprintln!("Failed to uninstall {} hooks: {e}", spec.label);
                                return ExitCode::FAILURE;
                            }
                        }
                        None => {
                            eprintln!("No hook uninstaller for agent {}", spec.label);
                            return ExitCode::FAILURE;
                        }
                    }
                }
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
        Some(Commands::Delegate {
            task,
            task_file,
            to,
        }) => {
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
            let task = match resolve_task(task, task_file, std::io::stdin().lock()) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("Error: {e}");
                    return ExitCode::FAILURE;
                }
            };
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
        Some(Commands::WorkDone {
            task,
            task_file,
            done,
        }) => {
            let pane_id = match std::env::var(DOT_AGENT_DECK_PANE_ID) {
                Ok(id) => id,
                Err(_) => {
                    eprintln!(
                        "Error: DOT_AGENT_DECK_PANE_ID environment variable not set.\nThis command should be run from within a dot-agent-deck managed pane."
                    );
                    return ExitCode::FAILURE;
                }
            };
            let task = match resolve_task(task, task_file, std::io::stdin().lock()) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("Error: {e}");
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
        Some(Commands::AgentEvent { r#type }) => {
            let pane_id = match std::env::var(DOT_AGENT_DECK_PANE_ID) {
                Ok(id) => id,
                Err(_) => {
                    eprintln!(
                        "Error: DOT_AGENT_DECK_PANE_ID environment variable not set.\nThis command should be run from within a dot-agent-deck managed pane."
                    );
                    return ExitCode::FAILURE;
                }
            };
            // Optional — the daemon injects this on spawn (same pattern as the
            // hook path); a pane spawned before agent-id tagging has none.
            let agent_id = std::env::var(DOT_AGENT_DECK_AGENT_ID).ok();
            let event_type = match dot_agent_deck::event::agent_event_type_from_state(&r#type) {
                Some(et) => et,
                None => {
                    eprintln!(
                        "Error: unknown agent-event --type {:?}. Expected one of: running, waiting, finished.",
                        r#type
                    );
                    return ExitCode::FAILURE;
                }
            };
            // Ride the EXISTING raw-`AgentEvent` socket path (zero new wire):
            // a bare AgentEvent with no `message_type` envelope, keyed on a
            // stable session id derived from the pane so repeated events update
            // the same card. The daemon's `run_hook_loop` falls back to
            // `AgentEvent` and `apply_event` drives the status.
            let event = dot_agent_deck::event::AgentEvent {
                session_id: format!("{pane_id}-session"),
                // TODO(companion PRD): derive agent type from the pane instead
                // of hard-coding Pi. Safe today because the daemon's
                // `apply_event` only UPGRADES `None` → a concrete type (never
                // downgrades), so a hard-coded `Pi` from the `agent-event`
                // subcommand can't clobber an already-known type.
                agent_type: dot_agent_deck::event::AgentType::Pi,
                event_type,
                tool_name: None,
                tool_detail: None,
                cwd: None,
                timestamp: chrono::Utc::now(),
                user_prompt: None,
                metadata: Default::default(),
                pane_id: Some(pane_id),
                agent_id,
                agent_version: None,
                schema_version: None,
                live_target: None,
            };
            let json = match serde_json::to_string(&event) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!("Failed to serialize agent-event: {e}");
                    return ExitCode::FAILURE;
                }
            };
            if dot_agent_deck::hook::send_to_socket(&json).is_none() {
                eprintln!("Failed to send agent-event to daemon socket.");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Some(Commands::GetSeed) => {
            let pane_id = match std::env::var(DOT_AGENT_DECK_PANE_ID) {
                Ok(id) => id,
                Err(_) => {
                    eprintln!(
                        "Error: DOT_AGENT_DECK_PANE_ID environment variable not set.\nThis command should be run from within a dot-agent-deck managed pane."
                    );
                    return ExitCode::FAILURE;
                }
            };
            // Ask the daemon (over the hook socket) for the seed it prepared
            // for this pane. READ-ONLY request/response — the one hook-socket
            // verb that reads a reply. A missing daemon / older daemon that
            // doesn't answer → `None` → we print nothing and exit 0, so the
            // extension no-sends and the daemon's PTY-injection safety net
            // still delivers (graceful cross-version degradation, no
            // PROTOCOL_VERSION dependency).
            let req = dot_agent_deck::event::DaemonMessage::GetSeed(
                dot_agent_deck::event::GetSeedRequest { pane_id },
            );
            let json = match serde_json::to_string(&req) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!("Failed to serialize get-seed request: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match dot_agent_deck::hook::request_from_socket(&json) {
                Some(line) if !line.trim().is_empty() => {
                    match serde_json::from_str::<dot_agent_deck::event::GetSeedResponse>(&line) {
                        Ok(resp) => {
                            if let Some(seed) = resp.seed {
                                // Print the seed verbatim (no trailing newline)
                                // so the extension captures exactly the prepared
                                // text. Empty seed → print nothing.
                                print!("{seed}");
                            }
                            ExitCode::SUCCESS
                        }
                        // A reply we can't parse is treated as "no seed": print
                        // nothing, exit 0 — the fallback still covers delivery.
                        Err(_) => ExitCode::SUCCESS,
                    }
                }
                // No reply (no daemon / older daemon / no seed) → no seed.
                _ => ExitCode::SUCCESS,
            }
        }
        Some(Commands::Orchestrator { cmd }) => match cmd {
            // PRD #201 M3.2: thin wrapper — wire real PATH-detection + the real
            // `~/.pi/agent/extensions/dot-agent-deck` dir to the pure
            // `run_setup` core, then render its report to stdout/stderr + exit.
            OrchestratorCmd::Setup => {
                use dot_agent_deck::orchestrator_ext;
                // HOME-unset-safe (matching the auto-materialize path): the
                // strict resolver yields `None` when HOME is unset OR empty.
                // Because this is an EXPLICIT user command it ERRORS (non-zero)
                // rather than silently guessing a `/tmp`/`./` location Pi will
                // never discover — do NOT materialize, do NOT report success.
                match orchestrator_ext::default_extension_dir() {
                    None => {
                        eprintln!(
                            "orchestrator setup: HOME is not set — cannot locate Pi's extension \
                             directory (~/.pi/agent/extensions/dot-agent-deck). Set HOME and \
                             re-run `dot-agent-deck orchestrator setup`."
                        );
                        ExitCode::FAILURE
                    }
                    Some(target_dir) => {
                        let pi_present = orchestrator_ext::pi_on_path();
                        match orchestrator_ext::run_setup(pi_present, &target_dir) {
                            Ok(report) if report.success => {
                                println!("{}", report.message);
                                ExitCode::SUCCESS
                            }
                            Ok(report) => {
                                eprintln!("{}", report.message);
                                ExitCode::FAILURE
                            }
                            Err(e) => {
                                eprintln!(
                                    "orchestrator setup: failed to materialize the Pi extension into {}: {e}",
                                    target_dir.display()
                                );
                                ExitCode::FAILURE
                            }
                        }
                    }
                }
            }
        },
        Some(Commands::Daemon { cmd }) => match cmd {
            DaemonCmd::Serve => {
                // PRD #170 M1.2: capture the login-shell PATH and apply it to
                // the daemon's OWN environment HERE — in the synchronous `main`
                // dispatch, BEFORE `run_daemon_serve_cli` builds its tokio
                // runtime (`#[tokio::main]`) and any worker threads exist. That
                // single-threaded window is the PRD's stated `set_var`
                // soundness condition. This covers BOTH the `daemon serve` path
                // and the lazy-spawned daemon, since the deck lazy-spawns by
                // fork-exec'ing this exact subcommand. Logging is initialized
                // first so the capture result is recorded; `run_daemon_serve_cli`
                // therefore no longer initializes it.
                init_logging_from_env();
                dot_agent_deck::login_shell::apply_login_shell_path();
                // PRD #201: materialize the bundled Pi orchestrator extension ONCE
                // at daemon startup — parity with claude/opencode installing their
                // hooks/plugin at startup. This covers both the lazy-spawned daemon
                // and a headless `daemon serve`, and is command-agnostic (works for
                // `pi`, an absolute path, or a wrapper like `devbox run pi-big`),
                // since it does not look at any spawn command. Runs AFTER the
                // login-shell PATH is applied so pi-presence is detected against the
                // daemon's real PATH. Self-guards on pi being installed; a no-op
                // otherwise. It honors `PI_CODING_AGENT_DIR` (else `~/.pi/agent`),
                // so it lands where pi will look — see `orchestrator_ext`.
                dot_agent_deck::orchestrator_ext::auto_materialize(&[]);
                run_daemon_serve_cli()
            }
            DaemonCmd::Hello => run_daemon_hello_cli(),
            DaemonCmd::Stop { force } => run_daemon_stop_cli(force),
            DaemonCmd::Restart { force } => run_daemon_restart_cli(force),
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
        Some(Commands::Connect { name }) => run_connect(name),
        Some(Commands::Schedule { action }) => run_schedule_cli(action),
        Some(Commands::Snapshot { cmd }) => match cmd {
            // PRD #89 M4.2 — local fresh-start escape hatch. Reuses the same
            // `SavedSession::clear()` the TUI calls at teardown, so it honors
            // the `DOT_AGENT_DECK_SESSION` override and deletes the one global
            // snapshot at `config::session_path()`.
            SnapshotCmd::Clear => match dot_agent_deck::config::SavedSession::clear() {
                Ok(()) => {
                    println!(
                        "Cleared the local saved-session snapshot. The next `dot-agent-deck` startup will begin from an empty dashboard."
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("Failed to clear the saved-session snapshot: {e}");
                    ExitCode::FAILURE
                }
            },
        },
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
        Some(Commands::Wrap { agent, command }) => {
            dot_agent_deck::wrap::run_wrap(agent.as_deref(), &command)
        }
    }
}

#[tokio::main]
async fn run_dashboard() -> ExitCode {
    init_logging_from_env();
    run_tui_session().await
}

/// Optional file-based logging from `DOT_AGENT_DECK_LOG`. Pulled out of the
/// dashboard entry point so the `connect` subcommand (which builds its own
/// tokio runtime) can call it once before launching the TUI body.
///
/// PRD #170 (Auditor-2): this MUST stay synchronous — a plain `std::fs::File`
/// writer, NEVER a `tracing_appender::non_blocking` / worker-thread appender.
/// On the `daemon serve` path it runs immediately before the pre-runtime
/// `apply_login_shell_path` `set_var` (main.rs); a logging thread spawned here
/// would land inside that single-threaded window and break the `set_var`
/// soundness invariant the login-shell PATH capture relies on.
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
/// PRD #93 Phase 2: every fresh `dot-agent-deck` invocation lazy-spawns a
/// per-user daemon on the `attach_socket_path()` Unix socket and
/// attaches to it via the streaming protocol. The legacy in-process
/// daemon path (and its env-var escape hatch) is gone — the daemon is
/// always external.
///
/// Returns `ExitCode::FAILURE` when the external-daemon bootstrap fails
/// (spawn error, start timeout, or trust-check rejection). Successful TUI
/// runs return `ExitCode::SUCCESS` — including TUI-task errors, which are
/// already surfaced to stderr.
async fn run_tui_session() -> ExitCode {
    // PRD #139 M1.2/M1.3: initialize the process-global experimental flag from
    // `.dot-agent-deck.toml` `[features]` (env override wins) and start the
    // live re-read watcher. The startup state is recorded via a single
    // `tracing::info!` line, which surfaces only when file logging is enabled
    // (`DOT_AGENT_DECK_LOG`); it is never printed to the terminal.
    dot_agent_deck::features::init_and_watch();

    let state = Arc::new(RwLock::new(AppState::default()));
    let attach_path = attach_socket_path();

    // If the attach socket is missing, `ensure_external_daemon_or_die`
    // fork-execs `dot-agent-deck daemon serve` detached under
    // flock-serialized contention (so two simultaneous TUIs can't both
    // win the bind — M1.3) and trust-checks any existing socket
    // (uid + 0o600 + is-socket) before the TUI's DaemonClient touches it.
    if let Err(e) = ensure_external_daemon_or_die(&attach_path).await {
        eprintln!(
            "failed to connect to daemon at {}: {e}",
            attach_path.display()
        );
        return ExitCode::FAILURE;
    }
    // PRD #103 Phase 2 / PRD #161 Part A: build-version handshake against
    // the running daemon. Runs unconditionally — including the
    // freshly-spawned case where the build-ids are necessarily equal (PRD
    // M2.3). The cost is one extra Unix-socket round-trip on cold start;
    // the upside is a smoke test of the handshake on every launch, which
    // catches regressions in `ensure_external_daemon_or_die` itself (wrong
    // socket / wrong binary) or in the wire encoding of the `build_version`
    // field.
    //
    // PRD #161 D2 (option A — consent-based always-restart) decides the
    // mismatch path by agents-present + TTY:
    //   - No agents: the daemon is SIGTERM'd silently (`Recovered`); we
    //     fall through and re-spawn a fresh daemon at the current build.
    //   - Agents + TTY: an interactive prompt names the live agents; a
    //     single `s` restarts (`Recovered`, re-spawn), any dismiss key
    //     declines (`ProceedOnExisting`, keep the existing daemon — D4
    //     never-strand).
    //   - Agents + non-TTY: prints the recovery hint to stderr and exits
    //     non-zero (the only non-zero-exit path).
    // Errors are already user-visible inside the helper, so we render no
    // further message here.
    let handshake_outcome =
        match build_version_handshake::ensure_compatible_daemon_or_die(&attach_path).await {
            Ok(outcome) => outcome,
            Err(build_version_handshake::HandshakeError::MismatchAborted) => {
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        };
    // After a `Recovered` outcome the old daemon was just SIGTERM'd; the
    // next attach lazy-spawns a fresh one. Re-run the bootstrap so the
    // socket is back before any client (DaemonClient::list_agents,
    // spawn_event_subscriber, the embedded-pane controller) touches it.
    // On `Match` (compatible) or `ProceedOnExisting` (user declined the
    // restart, keeping the existing daemon) the daemon is already running —
    // re-running the bootstrap would just be wasted I/O.
    if matches!(
        handshake_outcome,
        build_version_handshake::HandshakeOutcome::Recovered
    ) && let Err(e) = ensure_external_daemon_or_die(&attach_path).await
    {
        eprintln!(
            "failed to re-spawn daemon at {} after version-mismatch recovery: {e}",
            attach_path.display()
        );
        return ExitCode::FAILURE;
    }
    // Test-only escape hatch (PRD #103 M4.2): integration tests in
    // tests/build_version_handshake.rs need to exercise the handshake
    // path (including SIGTERM + lazy re-spawn) without entering the
    // full TUI. Setting `DOT_AGENT_DECK_EXIT_AFTER_HANDSHAKE` causes
    // the TUI to exit cleanly here, after the handshake completed and
    // the daemon socket is back up. Production code never sets it; the
    // env-var name is grep-ably explicit so a future audit can confirm.
    if std::env::var_os("DOT_AGENT_DECK_EXIT_AFTER_HANDSHAKE").is_some() {
        return ExitCode::SUCCESS;
    }
    // PRD #76 M2.17: subscribe to the daemon's `AgentEvent` broadcast so
    // the TUI's `AppState` mirrors live agent activity.
    spawn_event_subscriber(attach_path.clone(), state.clone());

    let version_state = state.clone();
    tokio::spawn(async move {
        if let Some(latest) = dot_agent_deck::version::check_for_update().await {
            version_state.write().await.update_available = Some(latest);
        }
    });

    let config = dot_agent_deck::config::DashboardConfig::load();

    // PRD #40: resolve keybindings client-side, *before* entering the
    // alternate screen, so any malformed-config / conflict / unknown-action
    // warnings land on stderr in the normal terminal (and, under a PTY, in
    // the byte stream that precedes the alt-screen switch) where they are
    // actually visible. `run_tui` (via `ratatui::init`) is what flips into
    // the alt-screen, so loading here keeps the warnings ahead of it.
    let keybindings = dot_agent_deck::keybindings::KeybindingConfig::load();

    // Auto-install hooks/plugins for detected agents (silent, best-effort).
    // PRD #20 M2 / R20-010: driven from the agent registry — iterate the shipped
    // agents and run each spec's OWN startup auto-install action. Order is stable
    // (`ALL` order). Dispatching per-spec (rather than mapping the reusable
    // `IntegrationStrategy` enum to a hardcoded incumbent) means a future agent
    // reusing `NativeHooks`/`Plugin` runs ITS OWN installer, not Claude's or
    // OpenCode's. Claude installs native hooks and OpenCode its plugin at
    // startup; Pi (`Extension`) materializes at spawn-time (see `agent_pty`) and
    // Codex (`Wrapper`) has no install step, so their `startup_auto_install` is
    // `None` and they are skipped.
    {
        use dot_agent_deck::agent_registry::ALL;
        for spec in ALL {
            if let Some(install) = spec.startup_auto_install {
                install();
            }
        }
    }

    let pane_controller: Arc<dyn PaneController> = Arc::new(EmbeddedPaneController::new(
        attach_path.clone(),
        tokio::runtime::Handle::current(),
    ));
    let tui_state = state.clone();
    let tui_result = tokio::task::spawn_blocking(move || {
        run_tui(tui_state, pane_controller, config, keybindings)
    })
    .await;

    // TUI exited — clean up. The daemon was fork-execed detached by
    // ensure_external_daemon_or_die (setsid'd into its own session) so
    // it is intentionally outside this process tree: we do not abort
    // the daemon and do not unlink its sockets. Agents must survive
    // TUI exit (PRD #76 line 199).

    if let Err(e) = tui_result {
        eprintln!("TUI task error: {e}");
    } else if let Ok(Err(e)) = tui_result {
        eprintln!("TUI error: {e}");
    }
    ExitCode::SUCCESS
}

/// PRD #76 M2.17 (hook events) / M2.19 (delegate signals): open a
/// long-lived `SubscribeEvents` connection against the daemon and
/// route each [`BroadcastMsg::Event`] into the TUI's `AppState` via
/// `apply_event`.
///
/// PRD #93 round-5: the delegate / work-done variants used to ride this
/// channel too — the daemon couldn't dispatch them locally and the TUI
/// re-ran the role-validation guards. The daemon now owns dispatch end
/// to end (writes the prompt directly into the target pane's PTY), so
/// only hook events flow through here.
///
/// Reconnects with a small backoff on transport errors so a daemon
/// restart or a `KIND_STREAM_END "lagged"` tear-down recovers
/// automatically.
fn spawn_event_subscriber(
    attach_path: std::path::PathBuf,
    state: dot_agent_deck::state::SharedState,
) {
    use dot_agent_deck::event::BroadcastMsg;

    tokio::spawn(async move {
        // Backoff parameters tuned for "daemon briefly unavailable" rather
        // than long outages: a fresh-daemon ready window is sub-second, so
        // a 500ms initial delay catches most transient cases, and we cap
        // at 5s so a stuck daemon doesn't burn CPU on reconnect attempts.
        let mut delay = std::time::Duration::from_millis(500);
        let max_delay = std::time::Duration::from_secs(5);
        let client = DaemonClient::new(attach_path);
        loop {
            match client.subscribe_events().await {
                Ok(mut sub) => {
                    // Reset backoff on a successful subscribe.
                    delay = std::time::Duration::from_millis(500);
                    loop {
                        match sub.next_event().await {
                            Ok(Some(BroadcastMsg::Event(event))) => {
                                state.write().await.apply_event(event);
                            }
                            // PRD #120: a daemon-spawned orchestration (issue
                            // dispatch). Queue it for the render loop, which owns
                            // the TabManager + pane controller and builds the
                            // live tab. The subscriber task can't touch those.
                            Ok(Some(BroadcastMsg::OrchestrationSurface(surface))) => {
                                state.write().await.queue_orchestration_surface(surface);
                            }
                            Ok(None) => break,
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "subscribe_events: stream error, reconnecting"
                                );
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        "subscribe_events: subscribe failed, retrying"
                    );
                }
            }
            tokio::time::sleep(delay).await;
            delay = std::cmp::min(delay * 2, max_delay);
        }
    });
}

/// `dot-agent-deck connect [name]` — PRD #76 M2.9.
///
/// Resolves the remote (via lookup or picker), probes the remote
/// `dot-agent-deck` for reachability + version sanity, then exec's
/// `ssh -t` to run the deck TUI on the remote in M2.8 external-daemon
/// mode. The laptop process blocks until ssh exits and propagates the
/// exit code.
fn run_connect(name: Option<String>) -> ExitCode {
    let registry_path = dot_agent_deck::remote::default_remotes_path();

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

    let local_version = env!("DAD_VERSION");
    match dot_agent_deck::connect::run_connect_default(&entry, &registry_path, local_version) {
        Ok(0) => ExitCode::SUCCESS,
        // ExitCode::from(u8) is the closest we can get to "propagate ssh's
        // exit code." Codes outside 0..=255 saturate to 255, which is also
        // the value ssh itself uses for its own transport errors — that
        // collision is harmless because we already classified those as
        // typed RemoteConnectError before reaching the spawn.
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// `dot-agent-deck daemon hello` — PRD #76 M2.21 protocol-version handshake.
/// Prints a JSON-encoded [`dot_agent_deck::daemon_protocol::AttachResponse`]
/// carrying `server_version = PROTOCOL_VERSION` (and, per PRD #103 M1.3,
/// `build_version = env!("DAD_BUILD_ID")`) and exits.
///
/// Used by the laptop-side `connect` flow over ssh: the remote binary's
/// compiled-in `PROTOCOL_VERSION` is what its daemon would speak, so a static
/// print here is equivalent to a Hello round-trip against a running daemon —
/// and avoids lazy-spawning the daemon just to answer a version probe.
///
/// The wire shape mirrors what the daemon dispatcher returns for an
/// [`dot_agent_deck::daemon_protocol::AttachRequest::Hello`] in the
/// in-process attach path, so the client-side deserializer is the same in
/// both flows. Keep this helper in lockstep with that dispatcher arm and
/// with `AttachResponse::hello` — any divergence silently breaks the
/// handshake.
fn run_daemon_hello_cli() -> ExitCode {
    let resp = dot_agent_deck::daemon_protocol::AttachResponse::hello(
        dot_agent_deck::daemon_protocol::PROTOCOL_VERSION,
    );
    let json = match serde_json::to_string(&resp) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("Failed to serialize hello response: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("{json}");
    ExitCode::SUCCESS
}

/// `dot-agent-deck daemon stop [--force]` — PRD #103 Phase 3 (M3.2).
/// Documented, non-`kill -9` way to recycle the local daemon after a
/// binary upgrade. Idempotent (no-op exit 0 when no daemon is running)
/// and safe-by-default (refuses when managed agents are alive unless
/// `--force` is passed). The recovery flow is in
/// [`dot_agent_deck::daemon_stop::run_daemon_stop`]; this function
/// only translates outcomes into stdout/stderr text and exit codes.
#[tokio::main]
async fn run_daemon_stop_cli(force: bool) -> ExitCode {
    let attach_path = attach_socket_path();
    match dot_agent_deck::daemon_stop::run_daemon_stop(&attach_path, force).await {
        Ok(dot_agent_deck::daemon_stop::StopOutcome::NoDaemonRunning) => {
            println!("no daemon running");
            ExitCode::SUCCESS
        }
        Ok(dot_agent_deck::daemon_stop::StopOutcome::Stopped { pid }) => {
            println!("daemon stopped (pid {pid})");
            ExitCode::SUCCESS
        }
        Ok(dot_agent_deck::daemon_stop::StopOutcome::ForceKilled { pid }) => {
            println!("daemon force-killed via SIGKILL (pid {pid})");
            ExitCode::SUCCESS
        }
        Err(dot_agent_deck::daemon_stop::StopError::LiveAgents { ids }) => {
            eprint!(
                "{}",
                dot_agent_deck::daemon_stop::format_live_agents_refusal(&ids)
            );
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("daemon stop: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `dot-agent-deck daemon restart [--force]` — PRD #103 Phase 3 (M3.3).
/// Thin wrapper over `daemon stop`: the next TUI invocation lazy-spawns
/// a fresh daemon per PRD #93. Shares the same `--force` semantics as
/// `daemon stop`.
#[tokio::main]
async fn run_daemon_restart_cli(force: bool) -> ExitCode {
    let attach_path = attach_socket_path();
    match dot_agent_deck::daemon_stop::run_daemon_restart(&attach_path, force).await {
        Ok(dot_agent_deck::daemon_stop::StopOutcome::NoDaemonRunning) => {
            println!("no daemon running; next invocation will spawn one");
            ExitCode::SUCCESS
        }
        Ok(dot_agent_deck::daemon_stop::StopOutcome::Stopped { pid }) => {
            println!("daemon stopped (pid {pid}); next invocation will spawn a fresh daemon");
            ExitCode::SUCCESS
        }
        Ok(dot_agent_deck::daemon_stop::StopOutcome::ForceKilled { pid }) => {
            println!(
                "daemon force-killed via SIGKILL (pid {pid}); next invocation will spawn a fresh daemon"
            );
            ExitCode::SUCCESS
        }
        Err(dot_agent_deck::daemon_stop::StopError::LiveAgents { ids }) => {
            eprint!(
                "{}",
                dot_agent_deck::daemon_stop::format_live_agents_refusal(&ids)
            );
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("daemon restart: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `dot-agent-deck daemon serve` — PRD #76 M4.3. Runs the daemon (hook
/// ingestion + streaming-attach servers) in the foreground without a TUI.
/// The body mirrors the in-process spawn used by `run_tui_session`
/// (Daemon::with_attach + run_daemon_with) so a remote running this
/// subcommand binds the same two sockets a local TUI would.
///
/// Hook auto-install is skipped here on purpose: `remote add` already runs
/// `hooks install` on the remote, and the on-disk hook scripts only need
/// to be (re)installed when the binary version changes — not every time
/// the daemon starts.
#[tokio::main]
async fn run_daemon_serve_cli() -> ExitCode {
    // NOTE: logging is initialized by the `DaemonCmd::Serve` dispatch arm in
    // `main`, before the login-shell PATH capture and before this runtime is
    // built — so it is intentionally NOT initialized again here (a second
    // `tracing` global-default init would panic).
    // PRD #139 M1.2/M2.1: the daemon reads the experimental flag from the same
    // `.dot-agent-deck.toml` source of truth and watches it independently of
    // the TUI (the file is the contract; no cross-process sync).
    dot_agent_deck::features::init_and_watch();
    let state = Arc::new(RwLock::new(AppState::default()));
    let path = socket_path();
    let attach_path = attach_socket_path();

    let daemon = Daemon::with_attach(state, attach_path.clone());
    if let Err(e) = run_daemon_with(&path, daemon).await {
        eprintln!("Daemon error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// `dot-agent-deck schedule <subcommand>` — PRD #127 M1.5. The single
/// validated writer for the global `schedules.toml`. Mutating subcommands
/// (add/update/remove/enable/disable) load the current file, apply the change
/// through the `schedule_cli` helpers (cron validation + `~`/`$VAR` expansion +
/// rename guard), write the global path atomically regardless of cwd, then
/// trigger a live daemon reload (a daemon that isn't running is fine — the
/// change loads on next `daemon serve`). `run-now` and `reload` send control
/// messages to the daemon; `list` prints the current file.
#[tokio::main]
async fn run_schedule_cli(action: ScheduleAction) -> ExitCode {
    use dot_agent_deck::config::{LoadedSchedules, schedules_path};
    use dot_agent_deck::schedule_cli;

    // Subcommands that purely talk to the daemon (no file write).
    match &action {
        ScheduleAction::RunNow { name } => {
            use dot_agent_deck::daemon_client::RunNowOutcome;
            let client = DaemonClient::new(attach_socket_path());
            return match client.run_now(name).await {
                // PRD #127 C5: report skipped distinctly (still exit 0 — the
                // task is registered and the request succeeded).
                Ok(RunNowOutcome::Started) => {
                    println!("ran {name}");
                    ExitCode::SUCCESS
                }
                Ok(RunNowOutcome::SkippedStillRunning) => {
                    println!("skipped {name}: previous run still active");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("run-now failed: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        ScheduleAction::Reload => {
            let client = DaemonClient::new(attach_socket_path());
            return match client.reload_schedules().await {
                Ok(names) => {
                    println!("reloaded; registered: {}", names.join(", "));
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("reload failed: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        ScheduleAction::List => {
            let loaded = LoadedSchedules::load();
            for err in &loaded.errors {
                eprintln!("warning: skipped malformed entry: {}", err.message);
            }
            print!("{}", schedule_cli::format_list(&loaded.tasks));
            return ExitCode::SUCCESS;
        }
        _ => {}
    }

    // Mutating subcommands: load → apply → atomic write → reload trigger.
    let loaded = LoadedSchedules::load();
    for err in &loaded.errors {
        eprintln!(
            "warning: skipped malformed entry while loading: {}",
            err.message
        );
    }
    let mut tasks = loaded.tasks;

    let apply_result = match action {
        ScheduleAction::Add {
            name,
            cron,
            working_dir,
            command,
            prompt,
            new_tab_per_fire,
            enabled,
            repo,
            max_per_run,
            label,
            query,
        } => {
            // PRD #120: `--repo` turns this into an issue-dispatch `add`. Build
            // the sub-table here (defaulting `max_per_run` to the documented 3
            // when omitted); `schedule_cli::add` validates the slug + relaxes the
            // `--command` requirement.
            use dot_agent_deck::config::{IssueDispatchConfig, default_max_per_run};
            let issue_dispatch = repo.map(|repo| IssueDispatchConfig {
                repo,
                max_per_run: max_per_run.unwrap_or_else(default_max_per_run),
                label,
                query,
            });
            schedule_cli::add(
                &mut tasks,
                schedule_cli::AddArgs {
                    name,
                    cron,
                    working_dir,
                    command,
                    prompt,
                    new_tab_per_fire,
                    enabled,
                    issue_dispatch,
                },
            )
        }
        ScheduleAction::Update {
            name,
            cron,
            working_dir,
            command,
            prompt,
            new_tab_per_fire,
            enabled,
        } => schedule_cli::update(
            &mut tasks,
            schedule_cli::UpdateArgs {
                name,
                cron,
                working_dir,
                command,
                prompt,
                new_tab_per_fire,
                enabled,
            },
        ),
        ScheduleAction::Remove { name } => schedule_cli::remove(&mut tasks, &name),
        ScheduleAction::Enable { name } => schedule_cli::set_enabled(&mut tasks, &name, true),
        ScheduleAction::Disable { name } => schedule_cli::set_enabled(&mut tasks, &name, false),
        // RunNow/Reload/List handled above.
        ScheduleAction::RunNow { .. } | ScheduleAction::Reload | ScheduleAction::List => {
            unreachable!("daemon-only / read-only subcommands handled above")
        }
    };

    if let Err(e) = apply_result {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }

    let path = schedules_path();
    if let Err(e) = schedule_cli::write_atomic(&path, &tasks) {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }

    // Trigger a live reload so a running daemon picks the change up. A daemon
    // that isn't running is not an error — the change loads on next serve.
    let client = DaemonClient::new(attach_socket_path());
    match client.reload_schedules().await {
        Ok(_) => {}
        Err(e) => {
            eprintln!(
                "note: wrote {} but could not reload the daemon ({e}); it will load on next `daemon serve`",
                path.display()
            );
        }
    }
    ExitCode::SUCCESS
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    // PRD #127 B1 — `schedule add --new-tab-per-fire` must accept an explicit
    // `<true|false>` value (ArgAction::Set), matching `update`, the authoring
    // seed prompt, and the docs. A bare SetTrue flag would reject the value.
    fn parse_add_new_tab(value: &str) -> bool {
        let cli = Cli::try_parse_from([
            "dot-agent-deck",
            "schedule",
            "add",
            "--name",
            "t",
            "--cron",
            "0 9 * * *",
            "--working-dir",
            "/tmp",
            "--prompt",
            "p",
            "--new-tab-per-fire",
            value,
        ])
        .expect("schedule add must accept --new-tab-per-fire <true|false>");
        match cli.command {
            Some(Commands::Schedule {
                action:
                    ScheduleAction::Add {
                        new_tab_per_fire, ..
                    },
            }) => new_tab_per_fire,
            _ => panic!("expected `schedule add`"),
        }
    }

    #[test]
    fn schedule_add_new_tab_per_fire_takes_a_value() {
        assert!(parse_add_new_tab("true"));
        assert!(!parse_add_new_tab("false"));
    }

    #[test]
    fn schedule_add_new_tab_per_fire_defaults_false() {
        let cli = Cli::try_parse_from([
            "dot-agent-deck",
            "schedule",
            "add",
            "--name",
            "t",
            "--cron",
            "0 9 * * *",
            "--working-dir",
            "/tmp",
            "--prompt",
            "p",
        ])
        .expect("parse without --new-tab-per-fire");
        match cli.command {
            Some(Commands::Schedule {
                action:
                    ScheduleAction::Add {
                        new_tab_per_fire, ..
                    },
            }) => assert!(!new_tab_per_fire, "default must be false"),
            _ => panic!("expected `schedule add`"),
        }
    }

    // ---- PRD #201: shell-safe `--task-file` for delegate / work-done --------
    //
    // The task text may contain backticks, quotes, `$VAR`, and newlines. Passed
    // as `--task "…"` those are mangled by the caller's shell *before*
    // dot-agent-deck runs; `--task-file` reads the bytes verbatim off disk (or
    // stdin) so they survive. `resolve_task` is the pure seam under both
    // `delegate` and `work-done`, tested directly here.

    // A payload that exercises every character class the shell would otherwise
    // corrupt: backticks (command substitution), single/double quotes, a
    // `$VAR`, an escaped `\`, and multiple lines.
    const TRICKY_TASK: &str =
        "Fix `compute()` in \"src/lib.rs\" for $USER\nsecond 'line' with $HOME & a \\ backslash\n";

    #[test]
    fn task_file_reads_task_verbatim_from_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("task.txt");
        std::fs::write(&path, TRICKY_TASK).expect("write task file");

        // Empty stdin — the file path branch must not touch it.
        let got = resolve_task(
            None,
            Some(path.to_str().unwrap().to_string()),
            std::io::empty(),
        )
        .expect("resolve_task should read the file");
        assert_eq!(
            got, TRICKY_TASK,
            "a task with backticks/quotes/$VAR/newlines must round-trip VERBATIM via --task-file"
        );
    }

    #[test]
    fn task_file_dash_reads_task_verbatim_from_stdin() {
        let got = resolve_task(None, Some("-".to_string()), TRICKY_TASK.as_bytes())
            .expect("resolve_task should read stdin for `-`");
        assert_eq!(
            got, TRICKY_TASK,
            "`--task-file -` must read the task VERBATIM from stdin"
        );
    }

    #[test]
    fn task_plain_string_passes_through() {
        let got = resolve_task(Some("hello".to_string()), None, std::io::empty())
            .expect("plain --task should pass through");
        assert_eq!(got, "hello");
    }

    #[test]
    fn task_file_missing_errors_clearly() {
        let err = resolve_task(
            None,
            Some("/no/such/task-file.txt".to_string()),
            std::io::empty(),
        )
        .expect_err("a missing --task-file must error");
        assert!(
            err.contains("failed to read task file") && err.contains("/no/such/task-file.txt"),
            "missing-file error should name the path: {err}"
        );
    }

    #[test]
    fn task_and_task_file_both_set_is_rejected() {
        // Defensive guard inside resolve_task (clap also rejects this at parse
        // time — see the parse test below).
        let err = resolve_task(
            Some("x".to_string()),
            Some("y".to_string()),
            std::io::empty(),
        )
        .expect_err("--task + --task-file must conflict");
        assert!(
            err.contains("mutually exclusive"),
            "conflict error should be clear: {err}"
        );
    }

    #[test]
    fn task_neither_set_is_rejected() {
        let err = resolve_task(None, None, std::io::empty())
            .expect_err("neither --task nor --task-file must error");
        assert!(
            err.contains("--task") && err.contains("--task-file"),
            "neither-given error should mention both flags: {err}"
        );
    }

    #[test]
    fn delegate_parses_task_file_and_conflicts_with_task() {
        // --task-file parses into `task_file` with `task` empty.
        let cli = Cli::try_parse_from([
            "dot-agent-deck",
            "delegate",
            "--task-file",
            "/tmp/t.txt",
            "--to",
            "coder",
        ])
        .expect("delegate --task-file should parse");
        match cli.command {
            Some(Commands::Delegate {
                task,
                task_file,
                to,
            }) => {
                assert_eq!(task, None);
                assert_eq!(task_file.as_deref(), Some("/tmp/t.txt"));
                assert_eq!(to, vec!["coder".to_string()]);
            }
            _ => panic!("expected `delegate`"),
        }

        // Passing both --task and --task-file is rejected at parse time.
        // (`Cli` isn't `Debug`, so match rather than `expect_err`.)
        let err = match Cli::try_parse_from([
            "dot-agent-deck",
            "delegate",
            "--task",
            "x",
            "--task-file",
            "/tmp/t.txt",
            "--to",
            "coder",
        ]) {
            Ok(_) => panic!("--task + --task-file must conflict at parse time"),
            Err(e) => e,
        };
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "expected a clap ArgumentConflict, got: {err}"
        );
    }

    #[test]
    fn work_done_parses_task_file_and_conflicts_with_task() {
        let cli = Cli::try_parse_from(["dot-agent-deck", "work-done", "--task-file", "-"])
            .expect("work-done --task-file - should parse");
        match cli.command {
            Some(Commands::WorkDone {
                task,
                task_file,
                done,
            }) => {
                assert_eq!(task, None);
                assert_eq!(task_file.as_deref(), Some("-"));
                assert!(!done);
            }
            _ => panic!("expected `work-done`"),
        }

        let err = match Cli::try_parse_from([
            "dot-agent-deck",
            "work-done",
            "--task",
            "x",
            "--task-file",
            "y",
        ]) {
            Ok(_) => panic!("--task + --task-file must conflict at parse time"),
            Err(e) => e,
        };
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "expected a clap ArgumentConflict, got: {err}"
        );
    }
}
