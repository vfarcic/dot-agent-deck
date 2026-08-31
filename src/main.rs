use std::io::Write as _;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use tokio::sync::RwLock;

use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_AGENT_ID, DOT_AGENT_DECK_PANE_ID};
use dot_agent_deck::bounded_read::read_task_input;
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
    /// Devin CLI, likewise Claude-Code-hook-compatible: its native command hooks
    /// shell `dot-agent-deck hook --agent devin` and are ingested by the
    /// [`dot_agent_deck::hook`] `"devin"` arm.
    Devin,
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
            CliAgent::Devin => dot_agent_deck::event::AgentType::Devin,
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
        /// mangle. PATH must be a regular file, not a FIFO or a device; pass
        /// `-` to read a pipe. At most 1 MiB, from a file or from stdin.
        /// Mutually exclusive with --task.
        #[arg(long = "task-file", value_name = "PATH")]
        task_file: Option<String>,
        /// Role name(s) to delegate to (repeatable)
        #[arg(long)]
        to: Vec<String>,
    },
    /// Create a git worktree and start an isolated line of work inside it.
    /// Agent-callable, one step (PRD #220).
    Dispatch {
        /// Short name for the dispatch unit (used for worktree naming).
        /// Omit it only with --list-targets.
        #[arg(required_unless_present = "list_targets")]
        name: Option<String>,
        /// Task description with context, file paths, and constraints.
        /// Mutually exclusive with --task-file.
        #[arg(long, conflicts_with = "task_file")]
        task: Option<String>,
        /// Read the task text verbatim from a file (or `-` for stdin). PATH
        /// must be a regular file, not a FIFO or a device; pass `-` to read a
        /// pipe. At most 1 MiB, from a file or from stdin. Mutually exclusive
        /// with --task.
        #[arg(long = "task-file", value_name = "PATH")]
        task_file: Option<String>,
        /// Start ONE agent, even where this repo defines `[[orchestrations]]`.
        /// Mutually exclusive with --orchestration.
        #[arg(long, conflicts_with = "orchestration")]
        single: bool,
        /// Start a full orchestration by name (`--orchestration review`), or this
        /// repo's DEFAULT one (`--orchestration=` with an empty value) — the block
        /// carrying `default = true`, else the first with roles.
        /// Mutually exclusive with --single.
        ///
        /// The value is REQUIRED rather than optional: with `num_args = 0..=1` clap
        /// consumes the next bare token, so `dispatch --orchestration my-unit
        /// --task "…"` silently bound the UNIT NAME as the orchestration name and
        /// then aborted for a missing positional. Requiring it makes that
        /// invocation unambiguous.
        #[arg(long, value_name = "NAME")]
        orchestration: Option<String>,
        /// Print the spawn targets available in this repo, then exit. Ask the
        /// user which one they want before dispatching.
        ///
        /// Conflicts with every dispatch argument: combined, it used to print the
        /// listing and exit 0 WITHOUT dispatching, so an agent that merged the two
        /// usage lines reported a unit as started that never existed.
        #[arg(
            long,
            conflicts_with_all = ["name", "task", "task_file", "single", "orchestration"]
        )]
        list_targets: bool,
    },
    /// Signal task completion back to the orchestrator
    WorkDone {
        /// Summary of what was accomplished. Mutually exclusive with
        /// --task-file.
        #[arg(long, conflicts_with = "task_file")]
        task: Option<String>,
        /// Read the summary text verbatim from a file (or `-` for stdin). The
        /// shell-safe way to pass a summary containing backticks, quotes,
        /// `$VAR`, or newlines. PATH must be a regular file, not a FIFO or a
        /// device; pass `-` to read a pipe. At most 1 MiB, from a file or from
        /// stdin. Mutually exclusive with --task.
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
    /// Reclaim git worktrees whose PR is merged, whose tree is clean, and
    /// which the deck can prove it created. Never inspects git ancestry for
    /// merge state — squash-merges never enter `main`'s ancestry, and an
    /// ancestor branch with no PR must never be removed. The branch always
    /// survives; only the worktree directory is removed.
    Worktree {
        #[command(subcommand)]
        cmd: WorktreeCmd,
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
    /// Print a read-only snapshot of the daemon's managed agents: pane id,
    /// label, cwd, orchestration role, live status, and active tool. Fork
    /// #47: a CLI consumer of the existing `AttachRequest::ListAgents` — it
    /// never starts, stops, attaches to, resizes, writes to, or subscribes
    /// to any agent, and a missing/unreachable daemon is reported rather
    /// than lazily spawned.
    Status {
        /// Emit a versioned JSON document (`{schema_version, agents}`)
        /// instead of the human table.
        #[arg(long)]
        json: bool,
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
    /// Diagnose a remote's ssh setup: reachability, the deck's install, the
    /// forwards ssh actually resolved, and the remote sshd policy behind them
    /// (PRD #345). Read-only — it never edits ssh config, sshd config, the
    /// registry, or anything on the remote. Exits 0 when the diagnosis is
    /// clear, 1 when a check failed, and 2 when a check could not be
    /// determined.
    Doctor {
        /// Friendly name of the registry entry to diagnose.
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
enum WorktreeCmd {
    /// List every linked worktree with its resolved PR state, cleanliness,
    /// ownership, and gate verdict (remove/ask/keep) with a reason. Read-only
    /// — never removes anything.
    List {
        /// Emit a versioned JSON document (`{schema_version, worktrees}`)
        /// instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// Remove every worktree the gate marks `remove` (deck-owned, merged,
    /// clean) unconditionally. A worktree the deck cannot prove it created is
    /// reported as reclaimable-pending-confirmation and left alone unless
    /// `--yes` is passed. A dirty worktree, an open/closed-unmerged PR, or an
    /// unresolvable PR state always keeps, `--yes` or not. Never deletes the
    /// branch.
    Reclaim {
        /// Authorize removing worktrees the deck did NOT prove it created
        /// (the `ask` verdict), in addition to the ones it did. Has no effect
        /// on worktrees the gate already keeps for another reason.
        #[arg(long)]
        yes: bool,
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
        /// Configuration key (e.g., default_command, bell.on_idle)
        key: String,
    },
    /// Set a configuration value
    Set {
        /// Configuration key (e.g., default_command, bell.on_idle)
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
///
/// Both reads are size-bounded at
/// [`MAX_TASK_BYTES`](dot_agent_deck::bounded_read::MAX_TASK_BYTES) and
/// refused — never
/// truncated — past it, and the path branch additionally requires a **regular
/// file** (issue #328): a FIFO with no writer would otherwise block the CLI
/// forever inside `open`, and a character device such as `/dev/zero` never
/// ends. See [`read_task_input`].
fn resolve_task(
    task: Option<String>,
    task_file: Option<String>,
    stdin: impl std::io::Read,
) -> Result<String, String> {
    match (task, task_file) {
        (Some(t), None) => Ok(t),
        (None, Some(path)) => read_task_input(&path, stdin),
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

/// What `dot-agent-deck delegate` should print and exit with, for one daemon
/// reply. See [`delegate_verdict`].
#[derive(Debug, PartialEq, Eq)]
struct DelegateVerdict {
    /// `true` → `ExitCode::FAILURE`. Under this command's contract that means
    /// "nothing landed", which is what makes a retry safe.
    failed: bool,
    /// Printed to stderr verbatim when set. `None` only for a delegate that
    /// reached every role it named.
    message: Option<String>,
}

/// Parse one line of daemon reply into a [`DelegateResponse`], or `None` when
/// the line is not a delegate reply this build understands.
///
/// PR #466 review: `None` covers BOTH a line that fails to parse and a line that
/// parses but carries no [`DELEGATE_RESPONSE_KIND`] marker. Every field of
/// `DelegateResponse` is `#[serde(default)]`, so without the marker check `{}`
/// and `{"seed":null}` both parse into a pristine "nothing failed" response and
/// the caller reports success it has no evidence for. Callers treat `None` as
/// [`dot_agent_deck::hook::SocketReply::NoReply`] — delivered, unverifiable.
fn parse_delegate_reply(line: &str) -> Option<dot_agent_deck::event::DelegateResponse> {
    serde_json::from_str::<dot_agent_deck::event::DelegateResponse>(line)
        .ok()
        .filter(|r| r.is_delegate_reply())
}

/// Decide what `delegate` reports for a daemon reply it does understand.
///
/// Pure, and separate from the `Delegate` arm, so the contract below is pinned
/// by unit tests in this file — the tier that actually gates a merge. The e2e
/// assertions that cover it live in `tests/e2e_dispatcher_mode.rs`, which CI
/// compiles to nothing (`#![cfg(feature = "e2e")]` + no `--features e2e` in any
/// build job), so a refactor that made the rejection silent again would
/// otherwise pass every gate (PR #466 review).
///
/// Three outcomes, and the middle one is the whole point:
///
/// * `error` — routing failed outright, nothing was dispatched. **Failure.**
/// * `unresolved_roles` with an EMPTY `delivered` — every named role missed.
///   **Failure**, and the message must not assert a cause it has not
///   established: a role can be missing from the toml, BE the sending
///   orchestrator (which `delegate_targets` excludes by design), or have had its
///   worker pane closed. "Check the role names" is right for only the first.
/// * `unresolved_roles` with a NON-EMPTY `delivered` — the delegate HALF landed.
///   **Not a failure**: the task really is in the delivered panes' PTYs and
///   their idle-worker records are armed, so an orchestrator applying the
///   contract "non-zero ⇒ it did not land" would retry and dispatch those panes
///   a second time, arming two records for one pane. The message names both
///   sides so a retry can be aimed at just the roles that missed.
fn delegate_verdict(
    pane_id: &str,
    resp: &dot_agent_deck::event::DelegateResponse,
) -> DelegateVerdict {
    if let Some(error) = resp.error.as_deref() {
        return DelegateVerdict {
            failed: true,
            message: Some(format!(
                "Error: delegate from pane {pane_id} failed: {error}"
            )),
        };
    }
    if resp.unresolved_roles.is_empty() {
        return DelegateVerdict {
            failed: false,
            message: None,
        };
    }
    let unresolved = resp.unresolved_roles.join(", ");
    // The three causes, stated as the three causes rather than as the one that
    // happens to be most common.
    let causes = "(A role reaches no worker when it is absent from \
                  .dot-agent-deck.toml, when it is the delegating orchestrator \
                  itself — an orchestrator cannot delegate to itself — or when \
                  its worker pane has been closed.)";
    if resp.delivered.is_empty() {
        return DelegateVerdict {
            failed: true,
            message: Some(format!(
                "Error: delegate from pane {pane_id} reached no worker for role(s): \
                 {unresolved}. No role in this orchestration received it. {causes}"
            )),
        };
    }
    DelegateVerdict {
        failed: false,
        message: Some(format!(
            "Warning: delegate from pane {pane_id} reached no worker for role(s): \
             {unresolved}. It WAS delivered to: {}. Retry only the roles that \
             missed — re-sending the whole delegate would dispatch the delivered \
             roles a second time. {causes}",
            resp.delivered.join(", ")
        )),
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
                CliAgent::Devin => "devin",
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
            // Kept for the error messages below — the signal is moved into the
            // wire message, and a failure has to name the pane and the roles it
            // could not reach.
            let pane_id_for_report = pane_id.clone();
            let signal_roles = to.clone();
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
            // A REQUEST, not a fire-and-forget send. The daemon is the only place
            // that knows whether this delegate resolved to a worker, and until it
            // answered, `delegate` printed nothing and exited 0 no matter what
            // happened on the other side — so an orchestrator whose delegation was
            // dropped announced that its worker was working and then waited
            // forever for a `work-done` that could not arrive.
            //
            // `send_and_await_reply`, not `request_from_socket`: the latter folds
            // "no daemon" and "old daemon that does not answer this verb" into one
            // `None`, and those must not be reported the same way — the first is a
            // real failure, the second has to stay a success or every delegate
            // against an older daemon reports a phantom error.
            use dot_agent_deck::hook::SocketReply;
            let line = match dot_agent_deck::hook::send_and_await_reply(&json) {
                SocketReply::Unreachable => {
                    eprintln!(
                        "Error: could not reach the dot-agent-deck daemon socket, so the \
                         delegate to {} was NOT delivered.",
                        signal_roles.join(", ")
                    );
                    return ExitCode::FAILURE;
                }
                // Handed to the socket of a daemon that answered nothing
                // readable in `DELEGATE_REPLY_TIMEOUT` — usually one predating
                // this response. Pre-response contract: unverifiable, and the
                // caller must not turn that into a phantom failure. See
                // `SocketReply::NoReply`.
                SocketReply::NoReply => return ExitCode::SUCCESS,
                SocketReply::Line(line) => line,
            };
            let Some(resp) = parse_delegate_reply(&line) else {
                // Same reasoning as `NoReply`: a line we cannot parse — or one
                // that never identifies itself as a delegate reply — is a daemon
                // we do not understand, not a proven failure.
                return ExitCode::SUCCESS;
            };
            let verdict = delegate_verdict(&pane_id_for_report, &resp);
            if let Some(message) = verdict.message {
                eprintln!("{message}");
            }
            if verdict.failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Some(Commands::Dispatch {
            name,
            task,
            task_file,
            single,
            orchestration,
            list_targets,
        }) => {
            let pane_id = match std::env::var(DOT_AGENT_DECK_PANE_ID) {
                Ok(id) => id,
                Err(_) => {
                    eprintln!(
                        "Error: DOT_AGENT_DECK_PANE_ID environment variable not set.\n\
                         This command should be run from within a dot-agent-deck managed pane."
                    );
                    return ExitCode::FAILURE;
                }
            };
            // `--list-targets` is a READ-ONLY daemon round-trip (the `get-seed`
            // pattern): the daemon answers from the PANE's cwd and config, which is
            // the same basis the dispatch itself resolves from. Computing it here
            // from the CLI's own `current_dir()` diverged whenever the agent had
            // `cd`'d, and offered targets the dispatch could not start.
            //
            // Exits after printing. clap's `conflicts_with_all` guarantees no
            // dispatch arguments were supplied, so this cannot silently swallow a
            // real dispatch and still exit 0.
            if list_targets {
                let req = dot_agent_deck::event::DaemonMessage::ListTargets(
                    dot_agent_deck::event::ListTargetsRequest { pane_id },
                );
                let json = match serde_json::to_string(&req) {
                    Ok(j) => j,
                    Err(e) => {
                        eprintln!("Failed to serialize list-targets request: {e}");
                        return ExitCode::FAILURE;
                    }
                };
                match dot_agent_deck::hook::request_from_socket(&json) {
                    Some(line) if !line.trim().is_empty() => {
                        match serde_json::from_str::<dot_agent_deck::event::ListTargetsResponse>(
                            &line,
                        ) {
                            Ok(resp) => {
                                print!("{}", resp.rendered);
                                // A broken config is reported as a FAILURE so the
                                // agent cannot read "no orchestrations here" out of
                                // an error it never noticed.
                                if resp.error.is_some() {
                                    return ExitCode::FAILURE;
                                }
                                ExitCode::SUCCESS
                            }
                            Err(e) => {
                                eprintln!("Failed to parse the daemon's list-targets reply: {e}");
                                ExitCode::FAILURE
                            }
                        }
                    }
                    // No reply: no daemon, or one predating this verb. Say so rather
                    // than printing a confident empty list the caller would act on.
                    _ => {
                        eprintln!(
                            "Error: the daemon did not answer list-targets (not running, or an \
                             older build). Dispatch `--single` to start one agent, or \
                             `--orchestration <name>` if you know the name."
                        );
                        ExitCode::FAILURE
                    }
                }
            } else {
                // `required_unless_present = "list_targets"` guarantees this.
                let Some(name) = name else {
                    eprintln!("Error: a dispatch name is required.");
                    return ExitCode::FAILURE;
                };
                let task_text = match resolve_task(task, task_file, std::io::stdin().lock()) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("Error: {e}");
                        return ExitCode::FAILURE;
                    }
                };
                // clap's `conflicts_with` already rejects both flags together. A bare
                // `--orchestration` arrives as `Some("")` via `default_missing_value`
                // and means "this repo's first (role-bearing) one".
                //
                // The retained name is TRIMMED: an LLM-emitted `--orchestration "review "`
                // otherwise travels to the daemon with its whitespace, fails the exact
                // name comparison, and is refused with "no orchestration named 'review ';
                // available: review" — after a full worktree round trip.
                let shape = match (single, orchestration) {
                    (true, _) => Some(dot_agent_deck::event::DispatchShape::SingleAgent),
                    (false, Some(n)) => Some(dot_agent_deck::event::DispatchShape::Orchestration {
                        name: {
                            let n = n.trim();
                            if n.is_empty() {
                                None
                            } else {
                                Some(n.to_string())
                            }
                        },
                    }),
                    (false, None) => None,
                };
                let signal = dot_agent_deck::event::DispatchSignal {
                    pane_id,
                    name,
                    task: Some(task_text),
                    shape,
                    timestamp: chrono::Utc::now(),
                };
                let msg = dot_agent_deck::event::DaemonMessage::Dispatch(signal);
                let json = match serde_json::to_string(&msg) {
                    Ok(j) => j,
                    Err(e) => {
                        eprintln!("Failed to serialize dispatch signal: {e}");
                        return ExitCode::FAILURE;
                    }
                };
                if dot_agent_deck::hook::send_to_socket(&json).is_none() {
                    eprintln!("Failed to send dispatch signal to daemon socket.");
                    return ExitCode::FAILURE;
                }
                ExitCode::SUCCESS
            }
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
                // PRD #20 §4.2.1: same precedent for Codex — install the deck's
                // `hooks.json` into the active Codex home and record SCOPED,
                // hash-pinned trust for exactly those entries, ONCE at daemon
                // startup. Command-agnostic on purpose: the spawn seam can only
                // detect a `codex` basename, so a launcher (`devbox run codex-big`,
                // `run_codex.sh`) previously got no hooks at all. With the home
                // prepared here, its hook events reach the pane through the
                // inherited `DOT_AGENT_DECK_PANE_ID` regardless of launch method.
                // Runs AFTER the login-shell PATH is applied so codex-presence is
                // detected against the daemon's real PATH. Self-guards on codex
                // being installed and a resolvable home; a no-op otherwise.
                dot_agent_deck::codex_hooks_manage::auto_install_and_trust_at_startup();
                // Same precedent for Devin, which is also a native-hooks agent:
                // merge the deck's hooks into Devin's user config ONCE at daemon
                // startup, command-agnostically, so a headless daemon and a
                // launcher whose basename isn't `devin` are covered too. Runs
                // AFTER the login-shell PATH is applied so devin-presence is
                // detected against the daemon's real PATH. Self-guards on devin
                // being on PATH and a resolvable config dir; a no-op otherwise.
                dot_agent_deck::devin_hooks_manage::auto_install();
                run_daemon_serve_cli()
            }
            DaemonCmd::Hello => run_daemon_hello_cli(),
            DaemonCmd::Stop { force } => run_daemon_stop_cli(force),
            DaemonCmd::Restart { force } => run_daemon_restart_cli(force),
            DaemonCmd::Status { json } => run_daemon_status_cli(json),
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
            RemoteCmd::Doctor { name } => run_remote_doctor(&name),
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
        Some(Commands::Worktree { cmd }) => match cmd {
            WorktreeCmd::List { json } => run_worktree_list_cli(json),
            WorktreeCmd::Reclaim { yes } => run_worktree_reclaim_cli(yes),
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
                    // Issue #308 follow-up: the config failed to PARSE, so the
                    // `toml` error quotes the offending source line verbatim and
                    // this is an untrusted-bytes sink like the issue loop above.
                    // Neutralised at the seam, not here —
                    // `ProjectConfigError`'s `Display` escapes control, C1 and
                    // bidi characters while keeping the error frame's own
                    // newlines, exactly as `ValidationIssue`'s `Display` does
                    // for the single-line case. See both impls for why the
                    // escaping lives there rather than at each `eprintln!`.
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(Commands::Wrap { agent, command }) => {
            // Issue #243: the wrapper LOGS. Until this call it did not — the
            // subcommand went straight into `run_wrap`, so the two `tracing`
            // lines the interface watch emits (which of its two facts fired, the
            // single most useful field diagnostic the readiness mechanism
            // produces) were dropped on the floor by the no-op global
            // subscriber, and `crate::state::dispatch_one_owned`'s claim that
            // the fact is "in the wrapper's log" was false. Diagnosing a Codex
            // delegate that never got its prompt meant reading the wire.
            //
            // Safe in a pane. `init_logging_from_env` installs a subscriber ONLY
            // when `DOT_AGENT_DECK_LOG` is set, and only ever writes to that
            // file — never to stdout or stderr — so a wrapper whose descriptors
            // ARE the agent's terminal cannot paint a log line into it. The
            // daemon fork-execs `dot-agent-deck wrap` without clearing the
            // environment, so an operator who enabled the daemon's log gets the
            // wrapper's half of the story in the same file, correlated by
            // timestamp against the gate lines that read these events.
            init_logging_from_env();
            dot_agent_deck::wrap::run_wrap(agent.as_deref(), &command)
        }
    }
}

/// The deck's project directory for the process-global config reads that have
/// no narrower directory to key off — today just the `[features]` table
/// (issue #577).
///
/// Resolved ONCE here, at the entry point, and handed to
/// `features::init_and_watch` as an explicit directory — the same shape as
/// `examine_worktrees(&cwd)` and `run_reclaim(&cwd, …)` below, and as
/// `load_project_config(dir)` everywhere else. `features_config_path` no
/// longer reaches for the process cwd itself, so nothing downstream of this
/// call silently depends on where the process happens to be running.
///
/// The launch directory is where the search STARTS, not where it ends:
/// `resolve_project_dir` walks up to the nearest ancestor holding a trusted
/// `.dot-agent-deck.toml`, so a deck started at `repo/src` finds `repo`'s
/// flags instead of silently finding none. With no config at or above the
/// launch directory it returns that directory unchanged, which is the
/// pre-#577 path exactly.
fn launch_project_dir() -> std::path::PathBuf {
    let start = std::env::current_dir().unwrap_or_else(|e| {
        // Not fatal: `.` preserves the pre-#577 fallback, and a deck that
        // cannot resolve its own cwd still starts with the flag OFF.
        tracing::warn!(
            "failed to resolve the launch directory ({e}); reading [features] relative to \".\""
        );
        std::path::PathBuf::from(".")
    });
    dot_agent_deck::config::resolve_project_dir(&start)
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
                    .with_env_filter(dot_agent_deck::logging::env_filter_from_env())
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
    // (`DOT_AGENT_DECK_LOG`); it is never printed to the terminal. The project
    // directory is resolved HERE, at the entry point, and passed down (issue
    // #577) — see `launch_project_dir`.
    dot_agent_deck::features::init_and_watch(&launch_project_dir());

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
    // startup; Codex installs its native `hooks.json` AND records scoped,
    // hash-pinned trust for it here too (PRD #20 §4.2.1 — command-agnostic, so a
    // launcher like `devbox run codex-big` that the spawn seam can't detect is
    // still covered); Pi (`Extension`) materializes at spawn-time (see
    // `agent_pty`), so its `startup_auto_install` is `None` and it is skipped.
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
                            // Issue #717: a close left a dispatched worktree on
                            // disk. Queue it for the render loop for the same
                            // reason as the surface above — the status line is
                            // `UiState`, which this task cannot touch.
                            Ok(Some(BroadcastMsg::WorktreeKept(kept))) => {
                                state.write().await.queue_worktree_kept(kept);
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

/// PRD #345: `remote doctor <name>`. Resolves the registry entry FIRST so an
/// unknown name costs zero ssh invocations, then runs the read-only probes and
/// prints one line per check.
///
/// **Three exit codes**, so the outcomes a script has to treat differently are
/// distinguishable:
///
/// - **0** — clear. Every check PASSed, or at most raised an advisory WARN.
/// - **1** — a check FAILed, or the command could not run at all (an unknown
///   registry name, an unreadable registry).
/// - **2** — incomplete: no FAIL, but at least one check is UNKNOWN.
///
/// Both non-zero codes keep the PRD's promise that an UNKNOWN never reads as
/// PASS. Separating them makes the single most common real-world outcome — a
/// healthy tunnel on a host where `sshd -T` needs root you do not have — a
/// stable, scriptable `2` rather than something indistinguishable from a
/// broken tunnel. See [`dot_agent_deck::remote_doctor::Verdict::exit_code`].
fn run_remote_doctor(name: &str) -> ExitCode {
    let registry_path = dot_agent_deck::remote::default_remotes_path();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match dot_agent_deck::remote_doctor::run_doctor(name, &registry_path, &mut out) {
        Ok(verdict) => {
            let _ = out.flush();
            ExitCode::from(verdict.exit_code())
        }
        Err(e) => {
            let _ = out.flush();
            eprintln!("{e}");
            // The diagnosis never started, so there is no verdict to map. `1`
            // rather than `2`: the command itself failed, which is a different
            // thing from a diagnosis that ran and could not see everything.
            ExitCode::FAILURE
        }
    }
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

/// `dot-agent-deck worktree list [--json]`. Pure CLI-subprocess operation
/// over `git`/`gh` in the current directory's repo — no daemon involved, so
/// this is plain synchronous code, no `#[tokio::main]`. Row shaping and the
/// gate itself live in [`dot_agent_deck::worktree_reclaim`]; this wrapper
/// only translates the outcome into stdout/stderr text and an exit code.
fn run_worktree_list_cli(json: bool) -> ExitCode {
    use dot_agent_deck::worktree_reclaim::{
        WorktreeListDocument, examine_worktrees, format_list_human,
    };

    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("worktree list: failed to resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };
    let reports = match examine_worktrees(&cwd) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("worktree list: {e}");
            return ExitCode::FAILURE;
        }
    };

    if json {
        match serde_json::to_string(&WorktreeListDocument::new(reports)) {
            Ok(j) => {
                println!("{j}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("worktree list: failed to serialize JSON: {e}");
                ExitCode::FAILURE
            }
        }
    } else {
        print!("{}", format_list_human(&reports));
        ExitCode::SUCCESS
    }
}

/// `dot-agent-deck worktree reclaim [--yes]`. Removes every worktree the
/// gate marks `remove` (deck-owned, merged PR, clean tree) unconditionally,
/// and — only with `--yes` — also those it marks `ask` (merged and clean,
/// but the deck cannot prove it created them). Without `--yes`, `ask`-verdict
/// worktrees are left alone and reported as a pending decision that leads
/// the output, naming their exact paths and the ready-to-copy `--yes`
/// command. Always exits successfully once it has finished examining and
/// acting on every worktree; only a failure to enumerate worktrees at all
/// (e.g. not a git repo) is reported as failure.
fn run_worktree_reclaim_cli(yes: bool) -> ExitCode {
    use dot_agent_deck::worktree_reclaim::{format_reclaim_human, run_reclaim};

    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("worktree reclaim: failed to resolve current directory: {e}");
            return ExitCode::FAILURE;
        }
    };
    match run_reclaim(&cwd, yes) {
        Ok(outcome) => {
            print!("{}", format_reclaim_human(&outcome));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("worktree reclaim: {e}");
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

/// `dot-agent-deck daemon status [--json]`. Read-only CLI
/// consumer of the existing `AttachRequest::ListAgents`
/// ([`dot_agent_deck::daemon_client::DaemonClient::list_agents`]) — no new
/// attach request type, and therefore no `PROTOCOL_VERSION` bump: this command
/// puts nothing new on the wire, so an older daemon answers a newer CLI's
/// status query exactly as it always did (issue #459 — this rationale used to
/// cite a design note under the gitignored `.dot-agent-deck/`, which no reader
/// of the merged source could open). The `--json` document has its own,
/// separate [`dot_agent_deck::daemon_status::SCHEMA_VERSION`]; that is what
/// moves when the document shape changes. Row
/// shaping lives in [`dot_agent_deck::daemon_status`]; this wrapper only
/// bounds the round trip with [`dot_agent_deck::daemon_status::STATUS_REQUEST_TIMEOUT`]
/// and translates the outcome into stdout/stderr text and an exit code.
///
/// "Unavailable" (no daemon, a transport error, or a timed-out request) is
/// reported as failure — a status query that got no answer learned nothing,
/// unlike `daemon stop`'s idempotent "no daemon running" — but deliberately
/// never with clap's own exit code 2, so a caller can tell "this build
/// doesn't understand the request" apart from "the daemon didn't answer".
/// Never spawns, retries, or otherwise perturbs the daemon it's asking
/// about: a timeout abandons the query rather than looping.
#[tokio::main]
async fn run_daemon_status_cli(json: bool) -> ExitCode {
    use dot_agent_deck::daemon_status::{
        STATUS_REQUEST_TIMEOUT, StatusDocument, build_status_agents, format_human,
    };

    let client = DaemonClient::new(attach_socket_path());
    let records = match tokio::time::timeout(STATUS_REQUEST_TIMEOUT, client.list_agents()).await {
        Ok(Ok(records)) => records,
        Ok(Err(e)) => {
            eprintln!("daemon status: unavailable ({e})");
            return ExitCode::FAILURE;
        }
        Err(_elapsed) => {
            eprintln!(
                "daemon status: unavailable (no response within {}s)",
                STATUS_REQUEST_TIMEOUT.as_secs()
            );
            return ExitCode::FAILURE;
        }
    };

    let agents = build_status_agents(records);
    if json {
        match serde_json::to_string(&StatusDocument::new(agents)) {
            Ok(j) => {
                println!("{j}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("daemon status: failed to serialize JSON: {e}");
                ExitCode::FAILURE
            }
        }
    } else {
        print!("{}", format_human(&agents));
        ExitCode::SUCCESS
    }
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
    // the TUI (the file is the contract; no cross-process sync). The detached
    // spawn in `platform::detach` sets no `current_dir`, so the daemon
    // inherits the launching TUI's directory and the two agree on the file by
    // construction.
    dot_agent_deck::features::init_and_watch(&launch_project_dir());
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use dot_agent_deck::bounded_read::MAX_TASK_BYTES;

    // --- PRD #220: the dispatch shape selector's parsing ---

    fn parse_dispatch(
        args: &[&str],
    ) -> (Option<String>, Option<String>, bool, Option<String>, bool) {
        let mut argv = vec!["dot-agent-deck", "dispatch"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv)
            .expect("dispatch args should parse")
            .command
            .expect("a subcommand")
        {
            Commands::Dispatch {
                name,
                task,
                single,
                orchestration,
                list_targets,
                ..
            } => (name, task, single, orchestration, list_targets),
            // `Commands` deliberately derives no `Debug`, so this cannot print the
            // variant it got — the arm is unreachable anyway, since the argv above
            // always names `dispatch`.
            _ => panic!("expected the Dispatch subcommand"),
        }
    }

    /// `--orchestration` REQUIRES its value, so it can never consume the unit name.
    ///
    /// With `num_args = 0..=1` clap consumed the next bare token, so
    /// `dispatch --orchestration my-unit --task "…"` bound the UNIT NAME as the
    /// orchestration and aborted for a missing positional. A required value makes
    /// both orderings unambiguous.
    #[test]
    fn orchestration_value_is_required_so_it_cannot_eat_the_unit_name() {
        // Flag-first with a name still binds correctly: `probe` is the VALUE, and
        // the missing positional is a real error rather than a silent mis-bind.
        assert!(
            Cli::try_parse_from([
                "dot-agent-deck",
                "dispatch",
                "--orchestration",
                "probe",
                "--task",
                "t",
            ])
            .is_err(),
            "no positional NAME was supplied, so this must be rejected outright"
        );

        // A bare `--orchestration` with nothing after it is now an error, not a
        // silent \"this repo\'s first\".
        assert!(
            Cli::try_parse_from(["dot-agent-deck", "dispatch", "unit", "--orchestration"]).is_err(),
            "--orchestration now requires a value"
        );

        // The explicit empty value is how \"this repo\'s first\" is requested.
        let (name, _, _, orch, _) = parse_dispatch(&["unit", "--orchestration="]);
        assert_eq!(name.as_deref(), Some("unit"));
        assert_eq!(orch.as_deref(), Some(""));

        // And --task is never swallowed.
        let (name, task, _, orch, _) =
            parse_dispatch(&["unit", "--orchestration=review", "--task", "hello"]);
        assert_eq!(name.as_deref(), Some("unit"));
        assert_eq!(task.as_deref(), Some("hello"));
        assert_eq!(orch.as_deref(), Some("review"));
    }

    /// `--list-targets` cannot be combined with dispatch arguments. Combined, the
    /// early branch printed the listing and exited 0 WITHOUT dispatching, so an
    /// agent that merged the seed\'s two usage lines reported a unit as started
    /// that never existed.
    #[test]
    fn list_targets_conflicts_with_every_dispatch_argument() {
        for extra in [
            vec!["unit"],
            vec!["unit", "--task", "t"],
            vec!["--single"],
            vec!["--orchestration=review"],
        ] {
            let mut argv = vec!["dot-agent-deck", "dispatch", "--list-targets"];
            argv.extend(extra.iter().copied());
            assert!(
                Cli::try_parse_from(argv.clone()).is_err(),
                "--list-targets must conflict with {extra:?}"
            );
        }
        // Alone, it parses and needs no name.
        let (name, _, _, _, list) = parse_dispatch(&["--list-targets"]);
        assert!(name.is_none() && list);
    }

    #[test]
    fn dispatch_named_orchestration_and_single_parse_as_expected() {
        let (_, _, single, orch, _) = parse_dispatch(&["unit", "--orchestration=review"]);
        assert!(!single);
        assert_eq!(orch.as_deref(), Some("review"));

        let (_, _, single, orch, _) = parse_dispatch(&["unit", "--single", "--task", "t"]);
        assert!(single);
        assert_eq!(orch, None);
    }

    /// The two shape flags are mutually exclusive, so a caller can never express
    /// an ambiguous choice.
    #[test]
    fn dispatch_rejects_single_and_orchestration_together() {
        assert!(
            Cli::try_parse_from([
                "dot-agent-deck",
                "dispatch",
                "unit",
                "--single",
                "--orchestration=review",
            ])
            .is_err(),
            "--single and --orchestration must conflict"
        );
    }

    /// `--list-targets` is the one form that needs no name; every other form does,
    /// so a missing name can never be read as an empty dispatch name.
    #[test]
    fn dispatch_name_is_required_except_for_list_targets() {
        assert!(
            Cli::try_parse_from(["dot-agent-deck", "dispatch", "--task", "t"]).is_err(),
            "a dispatch with no name and no --list-targets must be rejected"
        );
    }

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

    // ---- Issue #328: both reads are bounded, and a non-regular path is
    // refused rather than opened. The per-shape refusals (FIFO, symlink to a
    // FIFO, character device, endless stream) are unit-tested against the
    // helper in `dot_agent_deck::bounded_read`; what these pin is that
    // `resolve_task` — the seam every `delegate` / `work-done` call goes
    // through — actually routes into it.

    #[test]
    fn task_file_over_the_size_limit_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("huge-task.md");
        std::fs::write(&path, "x".repeat(MAX_TASK_BYTES as usize + 1)).expect("write");

        let err = resolve_task(
            None,
            Some(path.to_str().unwrap().to_string()),
            std::io::empty(),
        )
        .expect_err("a task file over the cap must be refused");
        assert!(
            err.contains("exceeds the") && err.contains("limit"),
            "over-limit error should state the cap: {err}"
        );
    }

    #[test]
    fn task_file_at_the_size_limit_is_still_accepted() {
        // The cap refuses only what is genuinely past it — an input sitting
        // exactly on the boundary is a legitimate task, not a pathological one.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big-task.md");
        let text = "x".repeat(MAX_TASK_BYTES as usize);
        std::fs::write(&path, &text).expect("write");

        let got = resolve_task(
            None,
            Some(path.to_str().unwrap().to_string()),
            std::io::empty(),
        )
        .expect("a task file exactly at the cap must be accepted");
        assert_eq!(got.len(), MAX_TASK_BYTES as usize);
    }

    #[test]
    fn stdin_over_the_size_limit_is_refused() {
        // `-` keeps working (see `task_file_dash_reads_task_verbatim_from_stdin`),
        // but the same cap applies to it.
        let oversized = "x".repeat(MAX_TASK_BYTES as usize + 1);
        let err = resolve_task(None, Some("-".to_string()), oversized.as_bytes())
            .expect_err("oversized stdin must be refused");
        assert!(
            err.contains("task from stdin") && err.contains("exceeds the"),
            "over-limit stdin error should name stdin and the cap: {err}"
        );
    }

    #[test]
    fn task_file_pointing_at_a_non_regular_file_is_refused() {
        // A directory is the portable stand-in for the whole class; the FIFO
        // and character-device cases live in the `bounded_read` unit tests.
        let dir = tempfile::tempdir().expect("tempdir");
        let err = resolve_task(
            None,
            Some(dir.path().to_str().unwrap().to_string()),
            std::io::empty(),
        )
        .expect_err("a non-regular --task-file target must be refused");
        assert!(
            err.contains("--task-file needs a regular file") && err.contains("--task-file -"),
            "refusal should say what is required and point at the stdin alternative: {err}"
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

    // ---- PR #466 review: what `delegate` reports for a daemon reply --------
    //
    // The e2e assertions that cover this (`orchestration/dispatch/001`) live
    // behind `#![cfg(feature = "e2e")]`, and no CI build job passes
    // `--features e2e`, so they compile to nothing where it counts. These pin
    // the same contract in the tier that gates a merge.

    use dot_agent_deck::event::{DELEGATE_RESPONSE_KIND, DelegateResponse};

    fn reply(delivered: &[&str], unresolved: &[&str], error: Option<&str>) -> DelegateResponse {
        DelegateResponse {
            delivered: delivered.iter().map(|s| s.to_string()).collect(),
            unresolved_roles: unresolved.iter().map(|s| s.to_string()).collect(),
            error: error.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn delegate_verdict_reports_a_full_delivery_silently() {
        let v = delegate_verdict("pane-1", &reply(&["coder", "tester"], &[], None));
        assert!(!v.failed, "every named role resolved — this is a success");
        assert_eq!(v.message, None, "a clean delegate prints nothing");
    }

    #[test]
    fn delegate_verdict_fails_a_routing_error() {
        let v = delegate_verdict("xcaller", &reply(&[], &[], Some("no orchestration role")));
        assert!(v.failed, "a routing error means nothing was dispatched");
        let msg = v.message.expect("a routing error must be reported");
        assert!(
            msg.contains("xcaller") && msg.contains("no orchestration role"),
            "the message must name the pane and the daemon's reason: {msg}"
        );
    }

    #[test]
    fn delegate_verdict_fails_when_nothing_landed() {
        let v = delegate_verdict("pane-1", &reply(&[], &["ghost"], None));
        assert!(v.failed, "no role received the task — non-zero is correct");
        let msg = v.message.expect("an unreached delegate must be reported");
        assert!(
            msg.contains("ghost"),
            "the message must name the role that missed: {msg}"
        );
        // The three causes, not the one that happens to be most common: the
        // old message told the user to go check role names in the toml even
        // when the role was sitting there correctly and was simply the
        // orchestrator itself, or had had its worker pane closed.
        assert!(
            msg.contains(".dot-agent-deck.toml")
                && msg.contains("orchestrator cannot delegate to itself")
                && msg.contains("worker pane has been closed"),
            "the message must state all three causes, not assert one: {msg}"
        );
    }

    // THE blocker of the PR #466 review. `--to coder --to tester` with only a
    // `coder` pane really does write the task into the coder's PTY and arm its
    // idle-worker record. Reporting that as a failure invites the orchestrator
    // to retry — under this command's own new contract, non-zero means it did
    // not land — and the coder gets the same task twice, arming two records for
    // one pane.
    #[test]
    fn delegate_verdict_does_not_fail_a_partial_delivery() {
        let v = delegate_verdict("pane-1", &reply(&["coder"], &["tester"], None));
        assert!(
            !v.failed,
            "a delegate that half landed must NOT exit non-zero: a retry would \
             dispatch `coder` a second time"
        );
        let msg = v
            .message
            .expect("a partial delivery must still be reported");
        assert!(
            msg.contains("tester") && msg.contains("coder"),
            "a partial delivery must name BOTH what missed and what landed, or \
             a retry cannot be aimed safely: {msg}"
        );
    }

    #[test]
    fn parse_delegate_reply_requires_the_delegate_marker() {
        let good = serde_json::to_string(&reply(&["coder"], &[], None)).expect("serialize");
        assert!(
            good.contains(DELEGATE_RESPONSE_KIND),
            "the daemon's own reply must carry the marker: {good}"
        );
        let parsed = parse_delegate_reply(&good).expect("a real delegate reply must parse");
        assert_eq!(parsed.delivered, vec!["coder".to_string()]);

        // Every field is `#[serde(default)]`, so each of these DESERIALIZES
        // fine and yields a pristine "nothing failed" response. Accepting one
        // is how the verb whose purpose is answering "did this land?" answers
        // yes when it cannot tell.
        for line in [
            "{}",
            r#"{"seed":null}"#,
            r#"{"kind":"get-seed"}"#,
            "",
            "not json",
        ] {
            assert!(
                parse_delegate_reply(line).is_none(),
                "a reply that does not identify itself as a delegate response \
                 must be treated as unverifiable, not as success: {line}"
            );
        }
    }
}
