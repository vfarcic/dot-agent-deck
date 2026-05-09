//! PRD #76 M2.4 — `dot-agent-deck connect [name]`.
//!
//! Public-facing subcommand that attaches a local TUI viewer to a remote
//! daemon. Architecture (see `prds/76-remote-agent-environments.md` line 227
//! and `.dot-agent-deck/m2.4-context.md`):
//!
//! ```text
//!  TUI (in-proc) <—UnixStream—> bridge socket
//!                                    │
//!                                    │ byte-relay (tokio::io::copy x2)
//!                                    ▼
//!                          ssh stdin/stdout (Child)
//!                                    │
//!                                    │ ssh transport
//!                                    ▼
//!                          remote `dot-agent-deck daemon attach`
//! ```
//!
//! Three pieces here, each independently testable:
//! - **lookup / picker** — pure registry logic. `lookup_remote` resolves a
//!   name; `pick_remote` runs the numbered prompt when `<NAME>` is omitted.
//!   Both reject `kind = "kubernetes"` (planned in PRD #80, not yet supported).
//! - **bridge** — [`ConnectBridge`] owns the listener task + the ssh `Child`
//!   and cleans up the socket file on drop. Exposed as a struct so the TUI
//!   loop runs *while* the bridge is alive, and dropping the bridge tears
//!   down both halves.
//! - **CLI handler** — wires lookup/picker + bridge + env-var setup
//!   (`DOT_AGENT_DECK_VIA_DAEMON=1`, `DOT_AGENT_DECK_ATTACH_SOCKET=<bridge>`)
//!   to the existing TUI body.

use std::io::{BufRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
use tokio::process::{Child, Command as TokioCommand};
use tokio::task::JoinHandle;

use crate::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_REQ, KIND_RESP, read_frame, write_frame,
};
use crate::remote::{RemoteConfigError, RemoteEntry, RemotesFile, SshTarget, SystemSshExecutor};

/// Mode for the bridge socket: owner-only read/write. The bridge carries the
/// full streaming attach protocol for the *remote* daemon, so a
/// world-connectable bridge socket would let any local user hijack the
/// remote daemon's control channel. Mirrors `daemon::SOCKET_MODE`.
const BRIDGE_SOCKET_MODE: u32 = 0o600;

/// Marker `kind` for entries the user added with `--type=kubernetes`. M2.4
/// rejects these explicitly so the message clearly points the user at PRD #80
/// instead of surfacing a generic ssh failure deeper in the bridge.
const KIND_KUBERNETES: &str = "kubernetes";

/// Maximum invalid attempts on the picker prompt before bailing. Three is the
/// usual ergonomic choice for a numeric prompt — the user gets two retries
/// after the first miss before we conclude they're not paying attention.
const PICKER_MAX_RETRIES: usize = 3;

#[derive(Debug, Error)]
pub enum RemoteConnectError {
    #[error(
        "No remote named '{name}'. Run `dot-agent-deck remote list` to see configured remotes."
    )]
    UnknownName { name: String },
    #[error(
        "Remote '{name}' is type 'kubernetes'; kubernetes remotes are not yet supported (planned in PRD #80)."
    )]
    KubernetesNotYetSupported { name: String },
    #[error("No remotes configured. Run `dot-agent-deck remote add <name> <host>` to add one.")]
    NoRemotesConfigured,
    #[error("Invalid selection after {attempts} attempts; aborting.")]
    PickerGaveUp { attempts: usize },
    /// Connect-blocking: ssh failed to establish a session with the remote.
    /// Could be network (DNS, refused, timeout, route), auth (publickey,
    /// password denied), or host-key trust. The captured `detail` is ssh's
    /// stderr verbatim — surface it to the user so they can disambiguate.
    #[error(
        "Cannot reach remote '{name}' ({host}): {detail}\n\
         Check network connectivity, your ssh config, or run `ssh {host}` once manually to confirm."
    )]
    HostUnreachable {
        name: String,
        host: String,
        detail: String,
    },
    /// Connect-blocking: ssh reached the host, but the daemon binary on the
    /// other end is missing, broken, or speaks a wire format we don't
    /// understand. Most common cause is a stale or unfinished install — the
    /// suggested fix is `remote upgrade <name>`. Version-mismatch is rolled
    /// into this variant intentionally: per `Out of Scope`/Phase 4, we don't
    /// surface a separate "version skew" UX in M2.6.
    #[error(
        "Daemon on remote '{name}' is not responding: {detail}\n\
         Run `dot-agent-deck remote upgrade {name}` to (re)install the daemon binary on the remote."
    )]
    DaemonUnavailable { name: String, detail: String },
    #[error(transparent)]
    Registry(#[from] RemoteConfigError),
    #[error("Bridge I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Resolve a remote by name from the registry at `path`. Errors with
/// `UnknownName` if the name is missing or `KubernetesNotYetSupported` if
/// the entry's kind is `kubernetes`.
pub fn lookup_remote(name: &str, path: &Path) -> Result<RemoteEntry, RemoteConnectError> {
    let registry = RemotesFile::load(path)?;
    let entry = registry
        .remotes
        .into_iter()
        .find(|r| r.name == name)
        .ok_or_else(|| RemoteConnectError::UnknownName {
            name: name.to_string(),
        })?;
    if entry.kind == KIND_KUBERNETES {
        return Err(RemoteConnectError::KubernetesNotYetSupported { name: entry.name });
    }
    Ok(entry)
}

/// Render one picker row. Kubernetes entries are listed but tagged so the
/// user knows they can't pick one yet.
fn format_picker_row(idx: usize, entry: &RemoteEntry) -> String {
    if entry.kind == KIND_KUBERNETES {
        format!(
            "  {idx}) {:<12} (kubernetes)   [PRD #80 — not yet connectable]\n",
            entry.name
        )
    } else {
        format!("  {idx}) {:<12} (ssh, {})\n", entry.name, entry.host)
    }
}

/// Run the env picker.
///
/// - 0 entries: error with the empty-state hint (this is a hard "can't
///   proceed" — distinct from `remote list`'s ambient empty state).
/// - 1 entry: auto-pick. Print "Connecting to <name>..." and return without
///   prompting. Kubernetes-only registry still routes through the PRD #80
///   rejection.
/// - 2+ entries: numbered prompt. Up to [`PICKER_MAX_RETRIES`] invalid
///   attempts before giving up.
///
/// Generic over `BufRead` / `Write` so tests can inject fake I/O.
pub fn pick_remote<R: BufRead, W: Write>(
    path: &Path,
    input: &mut R,
    output: &mut W,
) -> Result<RemoteEntry, RemoteConnectError> {
    let registry = RemotesFile::load(path)?;
    if registry.remotes.is_empty() {
        return Err(RemoteConnectError::NoRemotesConfigured);
    }
    if registry.remotes.len() == 1 {
        let entry = registry.remotes.into_iter().next().expect("len==1");
        if entry.kind == KIND_KUBERNETES {
            return Err(RemoteConnectError::KubernetesNotYetSupported { name: entry.name });
        }
        writeln!(output, "Connecting to {}...", entry.name)?;
        return Ok(entry);
    }

    writeln!(output, "Select a remote:")?;
    for (i, entry) in registry.remotes.iter().enumerate() {
        write!(output, "{}", format_picker_row(i + 1, entry))?;
    }

    let mut attempts = 0usize;
    loop {
        write!(output, "> ")?;
        output.flush()?;
        let mut line = String::new();
        let n = input.read_line(&mut line)?;
        // EOF without a valid pick is the same failure mode as an invalid
        // entry — bail rather than spin forever.
        if n == 0 {
            return Err(RemoteConnectError::PickerGaveUp {
                attempts: attempts + 1,
            });
        }
        let trimmed = line.trim();
        match trimmed.parse::<usize>() {
            Ok(n) if (1..=registry.remotes.len()).contains(&n) => {
                let entry = registry
                    .remotes
                    .into_iter()
                    .nth(n - 1)
                    .expect("bounds checked");
                if entry.kind == KIND_KUBERNETES {
                    return Err(RemoteConnectError::KubernetesNotYetSupported { name: entry.name });
                }
                return Ok(entry);
            }
            _ => {
                attempts += 1;
                if attempts >= PICKER_MAX_RETRIES {
                    return Err(RemoteConnectError::PickerGaveUp { attempts });
                }
                writeln!(
                    output,
                    "Please enter a number between 1 and {}.",
                    registry.remotes.len()
                )?;
            }
        }
    }
}

/// Build the bridge socket path. Uses `$XDG_RUNTIME_DIR` when set
/// (preferred — owned by the user, auto-cleaned on logout), else `$TMPDIR`,
/// else `/tmp`. The pid suffix avoids collisions with concurrent
/// `connect` invocations from the same user.
pub fn bridge_socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("TMPDIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join(format!(
        "dot-agent-deck-connect-{}.sock",
        std::process::id()
    ))
}

/// Owns the bridge socket, the listener+relay task, and the ssh `Child`. The
/// listener accepts exactly one connection (the TUI dials this once via the
/// M1.3 stream-backed-pane path) and then byte-relays both directions until
/// either side closes. Drop unlinks the socket file; the ssh child exits via
/// `kill_on_drop(true)` set on spawn.
pub struct ConnectBridge {
    socket_path: PathBuf,
    _listener_task: JoinHandle<()>,
    _ssh: Child,
}

impl ConnectBridge {
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for ConnectBridge {
    fn drop(&mut self) {
        // Unlink best-effort. If the listener task is still running it'll be
        // dropped with this struct (its handle is owned here); the ssh child
        // is killed via the kill_on_drop flag on its `Command`.
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Build a `tokio::process::Command` for the ssh exec from an [`SshTarget`].
/// We intentionally re-walk the args off `SystemSshExecutor::build_command`
/// (which produces a `std::process::Command`) so the security defaults
/// (`BatchMode=yes`, no `StrictHostKeyChecking=no`, key/port handling) stay
/// in one place.
fn build_tokio_ssh_command(target: &SshTarget, remote_command: &str) -> TokioCommand {
    let std_cmd = SystemSshExecutor::build_command(target, remote_command);
    TokioCommand::from(std_cmd)
}

// ---------------------------------------------------------------------------
// M2.6 — probe-then-bridge (failure-mode-aware connect)
// ---------------------------------------------------------------------------

/// Result of running ssh + a single `list-agents` request against the remote
/// before bringing up the persistent bridge. Captured as raw bytes/strings so
/// the classification step can be a pure function tested without any ssh
/// subprocess in the loop.
#[derive(Debug, Default)]
pub struct ProbeOutcome {
    /// ssh process exit status. `None` if the process was killed by signal
    /// or never spawned.
    pub exit_status: Option<i32>,
    /// stderr captured from the ssh subprocess. Carries both transport-side
    /// errors (ssh's own diagnostics on exit 255) and remote-shell errors
    /// (e.g. `bash: dot-agent-deck: command not found` when the daemon
    /// binary is missing on the far side).
    pub stderr: String,
    /// Successfully parsed RESP frame, if one came back. `None` if no RESP
    /// arrived (ssh died before answering, EOF mid-frame, malformed JSON).
    pub response: Option<AttachResponse>,
}

/// Pure classification: turn a probe outcome into either the agent list or a
/// typed connect-blocking error. Pulled out so unit tests can exercise every
/// failure mode without a real ssh subprocess. The real `probe_remote`
/// orchestrates the subprocess and then calls into here.
fn classify_probe_outcome(
    name: &str,
    target: &SshTarget,
    outcome: ProbeOutcome,
) -> Result<Vec<String>, RemoteConnectError> {
    // 1. The daemon answered. A successful RESP is the happy path; an
    //    error RESP means the wire format works but the operation failed
    //    (almost always version skew per the PRD's Out-of-Scope), so
    //    surface `DaemonUnavailable` with the daemon's own error string.
    if let Some(resp) = outcome.response {
        if resp.ok {
            return Ok(resp.agents.unwrap_or_default());
        }
        return Err(RemoteConnectError::DaemonUnavailable {
            name: name.to_string(),
            detail: resp
                .error
                .unwrap_or_else(|| "daemon returned an error response".to_string()),
        });
    }

    // 2. No RESP. Inspect stderr + exit status.
    let stderr = outcome.stderr.trim();
    let lower = stderr.to_ascii_lowercase();

    // "Daemon binary missing" — POSIX shells emit `command not found` (or
    // `not found` on ksh/sh) and exit 127. Check this BEFORE the generic
    // transport patterns because both branches end up routing `Permission
    // denied`-like substrings; the binary-missing case is more specific.
    let daemon_missing = lower.contains("command not found")
        || lower.contains(": not found")
        || lower.contains("no such file or directory")
        || lower.contains("executable file not found")
        || lower.contains("cannot execute")
        || outcome.exit_status == Some(127);

    if daemon_missing {
        return Err(RemoteConnectError::DaemonUnavailable {
            name: name.to_string(),
            detail: if stderr.is_empty() {
                format!(
                    "ssh exited {} with no stderr — daemon binary likely missing on the remote",
                    outcome
                        .exit_status
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "(signalled)".into())
                )
            } else {
                stderr.to_string()
            },
        });
    }

    // Transport / auth / host-key — anything ssh itself emits on exit 255,
    // plus host-key trust prompts that fail under BatchMode.
    let transport_failure = lower.contains("connection refused")
        || lower.contains("network is unreachable")
        || lower.contains("no route to host")
        || lower.contains("could not resolve hostname")
        || lower.contains("connection timed out")
        || lower.contains("permission denied")
        || lower.contains("publickey")
        || lower.contains("host key verification failed")
        || lower.contains("remote host identification has changed")
        || outcome.exit_status == Some(255);

    if transport_failure {
        return Err(RemoteConnectError::HostUnreachable {
            name: name.to_string(),
            host: target.user_host(),
            detail: if stderr.is_empty() {
                "ssh exited 255 with no diagnostic".to_string()
            } else {
                stderr.to_string()
            },
        });
    }

    // Default: ssh exited with an unrecognized signature. Treat as
    // DaemonUnavailable rather than HostUnreachable — if ssh had failed at
    // the transport layer it would have said so, so the most likely cause
    // is the daemon binary itself misbehaving.
    Err(RemoteConnectError::DaemonUnavailable {
        name: name.to_string(),
        detail: if stderr.is_empty() {
            format!(
                "ssh exited {} with no stderr and no daemon response",
                outcome
                    .exit_status
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "(signalled)".into())
            )
        } else {
            stderr.to_string()
        },
    })
}

/// Probe the remote daemon: spawn a one-shot `ssh ... dot-agent-deck daemon
/// attach`, send a single `list-agents` request, capture the response and
/// stderr, then classify the outcome. On success returns the agent IDs (the
/// caller uses an empty list to print the M2.6 empty-state hint); on failure
/// returns a typed `HostUnreachable` or `DaemonUnavailable` error.
///
/// Architecture B from the M2.6 plan: today's `daemon_protocol::handle_connection`
/// reads exactly ONE request frame per connection and then closes, so a
/// "probe + handoff" on the same ssh stdio is not viable without a protocol
/// change. Two ssh invocations cost an extra login (~sub-second on a warm
/// link, free with persistent control sockets) but keep the protocol
/// semantics clean.
pub async fn probe_remote(
    target: &SshTarget,
    name: &str,
) -> Result<Vec<String>, RemoteConnectError> {
    let outcome = run_probe_subprocess(target).await;
    classify_probe_outcome(name, target, outcome)
}

/// Production probe: spawn ssh with stdin/stdout/stderr piped, write the
/// list-agents REQ, read one RESP, drain stderr concurrently, wait for ssh
/// to exit, and return the captured outcome. No classification here — that's
/// `classify_probe_outcome`'s job.
async fn run_probe_subprocess(target: &SshTarget) -> ProbeOutcome {
    let mut cmd = build_tokio_ssh_command(target, "~/.local/bin/dot-agent-deck daemon attach");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ProbeOutcome {
                exit_status: None,
                stderr: format!("failed to spawn ssh: {e}"),
                response: None,
            };
        }
    };

    let mut stdin = child
        .stdin
        .take()
        .expect("Stdio::piped() guarantees stdin handle");
    let mut stdout = child
        .stdout
        .take()
        .expect("Stdio::piped() guarantees stdout handle");
    let mut stderr = child
        .stderr
        .take()
        .expect("Stdio::piped() guarantees stderr handle");

    // Drain stderr concurrently so a chatty ssh can't fill the pipe buffer
    // and deadlock our send/recv path. The task ends naturally on EOF when
    // ssh exits and stderr closes.
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf).await;
        buf
    });

    // Send the list-agents request. The remote `dot-agent-deck daemon attach`
    // bridge forwards this to the local daemon's attach socket, which replies
    // with one RESP frame and closes the connection (per
    // `daemon_protocol::handle_connection`).
    let payload = serde_json::to_vec(&AttachRequest::ListAgents).expect("serialize ListAgents");
    let send_ok = write_frame(&mut stdin, KIND_REQ, &payload).await.is_ok();
    // Drop stdin so the bridge doesn't sit waiting for more frames after the
    // RESP closes the daemon side.
    drop(stdin);

    let response = if send_ok {
        match read_frame(&mut stdout).await {
            Ok(Some((KIND_RESP, payload))) => {
                serde_json::from_slice::<AttachResponse>(&payload).ok()
            }
            _ => None,
        }
    } else {
        None
    };

    let status = child.wait().await.ok().and_then(|s| s.code());
    let stderr_text = stderr_task.await.unwrap_or_default();

    ProbeOutcome {
        exit_status: status,
        stderr: stderr_text,
        response,
    }
}

/// Start the bridge: bind the socket, spawn ssh, kick off the relay task.
/// Returns the [`ConnectBridge`] for the caller to keep alive while the TUI
/// runs. Caller is responsible for setting `DOT_AGENT_DECK_ATTACH_SOCKET` to
/// `bridge.socket_path()` so the TUI dials the bridge instead of a local
/// daemon.
///
/// M2.6: callers SHOULD run [`probe_remote`] before this so a connect-time
/// failure surfaces as a typed error before the TUI launches. This function
/// itself does not classify ssh transport failures — by the time it returns,
/// the relay task is already alive and any ssh exit shows up async.
pub async fn start_bridge(
    target: &SshTarget,
    socket_path: PathBuf,
) -> Result<ConnectBridge, RemoteConnectError> {
    // Stale socket from a prior process that crashed without unlinking would
    // make `bind` return EADDRINUSE. The pid suffix already minimizes the
    // chance, but cleaning up defensively costs nothing.
    let _ = std::fs::remove_file(&socket_path);
    // Reuse the daemon's TOCTOU-safe umask-before-bind helper so the bridge
    // socket inode is created at 0o600 directly. Without this the socket
    // inherits the user's umask — typically world-connectable — and any
    // local user could hijack the remote daemon's control channel through
    // the bridge.
    let listener = crate::daemon::bind_socket(&socket_path)?;
    // Defense in depth: `bind_socket` already created the inode at 0o600 via
    // umask, but restating the mode here covers any future code path that
    // bypasses `bind_socket`. Mirrors `run_daemon_with` and
    // `daemon_protocol::bind_attach_listener`.
    std::fs::set_permissions(
        &socket_path,
        std::fs::Permissions::from_mode(BRIDGE_SOCKET_MODE),
    )?;

    let mut cmd = build_tokio_ssh_command(target, "~/.local/bin/dot-agent-deck daemon attach");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let stdin = child
        .stdin
        .take()
        .expect("Stdio::piped() guarantees stdin handle");
    let stdout = child
        .stdout
        .take()
        .expect("Stdio::piped() guarantees stdout handle");

    let listener_task = tokio::spawn(run_relay(listener, stdin, stdout));

    Ok(ConnectBridge {
        socket_path,
        _listener_task: listener_task,
        _ssh: child,
    })
}

/// Listener+relay task: accept one connection from the TUI, then byte-copy
/// both directions until one side closes.
async fn run_relay(
    listener: UnixListener,
    ssh_stdin: tokio::process::ChildStdin,
    ssh_stdout: tokio::process::ChildStdout,
) {
    let stream = match listener.accept().await {
        Ok((s, _)) => s,
        Err(_) => return,
    };
    relay_once(stream, ssh_stdin, ssh_stdout).await;
}

/// One-shot byte relay between an accepted UnixStream and a pair of ssh
/// child handles. Same `tokio::pin! + tokio::select!` shape as
/// `daemon_attach::run_daemon_attach` — when either half completes (EOF,
/// error, cancellation), Tokio drops the other.
///
/// Generic-ish via concrete types so this is callable from the unit test
/// with a `tokio::io::DuplexStream` standing in for ssh.
async fn relay_once<R, W>(stream: tokio::net::UnixStream, mut ssh_stdin: W, mut ssh_stdout: R)
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let (mut sock_r, mut sock_w) = stream.into_split();
    let inbound = async {
        let _ = tokio::io::copy(&mut sock_r, &mut ssh_stdin).await;
    };
    let outbound = async {
        let _ = tokio::io::copy(&mut ssh_stdout, &mut sock_w).await;
    };
    tokio::pin!(inbound, outbound);
    tokio::select! {
        _ = &mut inbound => {},
        _ = &mut outbound => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn entry(name: &str, kind: &str, host: &str) -> RemoteEntry {
        RemoteEntry {
            name: name.to_string(),
            kind: kind.to_string(),
            host: host.to_string(),
            port: 22,
            key: None,
            version: "0.24.5".to_string(),
            added_at: "2026-05-09T01:00:00+00:00".to_string(),
            upgraded_at: None,
        }
    }

    fn write_registry(dir: &tempfile::TempDir, entries: Vec<RemoteEntry>) -> PathBuf {
        let path = dir.path().join("remotes.toml");
        let file = RemotesFile { remotes: entries };
        file.save(&path).unwrap();
        path
    }

    // ----- lookup -----

    #[test]
    fn connect_lookup_unknown_name_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_registry(
            &dir,
            vec![
                entry("hetzner-1", "ssh", "viktor@hetzner-1.example.com"),
                entry("lab", "ssh", "lab.local"),
            ],
        );
        let err = lookup_remote("missing", &path).expect_err("unknown name must error");
        match &err {
            RemoteConnectError::UnknownName { name } => assert_eq!(name, "missing"),
            other => panic!("unexpected error: {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("missing"), "msg should name the remote: {msg}");
        assert!(
            msg.contains("dot-agent-deck remote list"),
            "msg should hint at `remote list`: {msg}"
        );
    }

    #[test]
    fn connect_lookup_kubernetes_type_routes_to_prd_80() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_registry(
            &dir,
            vec![entry("k3s-prod", "kubernetes", "k3s-prod.example.com")],
        );
        let err = lookup_remote("k3s-prod", &path).expect_err("kubernetes type must error");
        match &err {
            RemoteConnectError::KubernetesNotYetSupported { name } => {
                assert_eq!(name, "k3s-prod");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("PRD #80"), "msg should mention PRD #80: {msg}");
    }

    // ----- picker -----

    #[test]
    fn connect_picker_empty_registry_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml"); // does not exist
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let err =
            pick_remote(&path, &mut input, &mut output).expect_err("empty registry must error");
        assert!(matches!(err, RemoteConnectError::NoRemotesConfigured));
        let msg = err.to_string();
        assert!(
            msg.contains("No remotes configured"),
            "msg should give the empty-state hint: {msg}"
        );
        assert!(
            msg.contains("dot-agent-deck remote add"),
            "msg should point at `remote add`: {msg}"
        );
    }

    #[test]
    fn connect_picker_single_entry_auto_picks() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_registry(&dir, vec![entry("only", "ssh", "only.example.com")]);
        // Empty stdin: the picker MUST NOT consume from input when there's
        // only one entry — there's nothing to choose. Constructing the
        // cursor with no bytes means a hypothetical read_line would return 0
        // (EOF) and surface as PickerGaveUp; since we expect Ok, we know
        // read_line was never called.
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let chosen = pick_remote(&path, &mut input, &mut output).expect("auto-pick");
        assert_eq!(chosen.name, "only");
        let stdout = String::from_utf8(output).unwrap();
        assert!(
            stdout.contains("Connecting to only..."),
            "single-entry path should announce the connection: {stdout}"
        );
        assert!(
            !stdout.contains("Select a remote"),
            "single-entry path must not print the picker header: {stdout}"
        );
    }

    #[test]
    fn connect_picker_invalid_input_reprompts() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_registry(
            &dir,
            vec![
                entry("hetzner-1", "ssh", "viktor@hetzner-1.example.com"),
                entry("lab", "ssh", "lab.local"),
            ],
        );
        // First two lines are invalid; third line picks #2. The picker must
        // accept #2 after reprompting (proves retry/reprompt logic works).
        let mut input = Cursor::new(b"abc\n99\n2\n".to_vec());
        let mut output = Vec::<u8>::new();
        let chosen = pick_remote(&path, &mut input, &mut output).expect("third try succeeds");
        assert_eq!(chosen.name, "lab");
        let stdout = String::from_utf8(output).unwrap();
        // Should have re-prompted at least twice with the bounds hint.
        let reprompt_count = stdout.matches("Please enter a number").count();
        assert!(
            reprompt_count >= 2,
            "expected >=2 reprompts after two bad inputs, got {reprompt_count} in:\n{stdout}"
        );
    }

    #[test]
    fn connect_picker_max_retries_bails() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_registry(
            &dir,
            vec![
                entry("hetzner-1", "ssh", "h.example.com"),
                entry("lab", "ssh", "lab.local"),
            ],
        );
        let mut input = Cursor::new(b"abc\nxyz\nfoo\n".to_vec());
        let mut output = Vec::<u8>::new();
        let err =
            pick_remote(&path, &mut input, &mut output).expect_err("3 invalid inputs must bail");
        match err {
            RemoteConnectError::PickerGaveUp { attempts } => {
                assert_eq!(attempts, PICKER_MAX_RETRIES);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    // ----- M2.6 probe classification -----

    fn ssh_target() -> SshTarget {
        SshTarget {
            host: "remote.example.com".to_string(),
            user: Some("viktor".to_string()),
            port: 22,
            key: None,
        }
    }

    #[test]
    fn classify_probe_success_returns_agent_list() {
        // Happy path: daemon answered with ok=true and an agent list.
        let outcome = ProbeOutcome {
            exit_status: Some(0),
            stderr: String::new(),
            response: Some(AttachResponse {
                ok: true,
                error: None,
                agents: Some(vec!["alpha".to_string(), "beta".to_string()]),
                id: None,
            }),
        };
        let agents =
            classify_probe_outcome("hetzner-1", &ssh_target(), outcome).expect("happy path Ok");
        assert_eq!(agents, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn classify_probe_success_empty_list_is_ok_for_caller_hint() {
        // The "no running agents" case is NOT an error — the bridge launches,
        // the TUI launches, and main.rs prints the empty-state hint based on
        // an empty Vec. This test pins that contract.
        let outcome = ProbeOutcome {
            exit_status: Some(0),
            stderr: String::new(),
            response: Some(AttachResponse {
                ok: true,
                error: None,
                agents: Some(vec![]),
                id: None,
            }),
        };
        let agents =
            classify_probe_outcome("hetzner-1", &ssh_target(), outcome).expect("empty list is Ok");
        assert!(
            agents.is_empty(),
            "empty agent list must propagate so caller can print the hint"
        );
    }

    #[test]
    fn classify_probe_daemon_error_response_is_daemon_unavailable() {
        // Wire format works, but daemon returned ok=false (most often a
        // version-skew / unsupported request). Surface as DaemonUnavailable
        // with the daemon's own error string in the detail.
        let outcome = ProbeOutcome {
            exit_status: Some(0),
            stderr: String::new(),
            response: Some(AttachResponse {
                ok: false,
                error: Some("unknown request kind".to_string()),
                agents: None,
                id: None,
            }),
        };
        let err = classify_probe_outcome("hetzner-1", &ssh_target(), outcome)
            .expect_err("daemon error must error");
        match &err {
            RemoteConnectError::DaemonUnavailable { name, detail } => {
                assert_eq!(name, "hetzner-1");
                assert!(
                    detail.contains("unknown request kind"),
                    "detail should carry daemon error verbatim: {detail}"
                );
            }
            other => panic!("expected DaemonUnavailable, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("remote upgrade hetzner-1"),
            "msg should hint at `remote upgrade <name>`: {msg}"
        );
    }

    #[test]
    fn classify_probe_command_not_found_is_daemon_unavailable() {
        // Daemon binary missing on the remote: posix shell prints "command
        // not found" and exits 127. Surface as DaemonUnavailable with the
        // shell error verbatim and the `remote upgrade` hint visible in the
        // Display impl.
        let outcome = ProbeOutcome {
            exit_status: Some(127),
            stderr: "bash: dot-agent-deck: command not found\n".to_string(),
            response: None,
        };
        let err = classify_probe_outcome("hetzner-1", &ssh_target(), outcome)
            .expect_err("command-not-found must error");
        match &err {
            RemoteConnectError::DaemonUnavailable { name, detail } => {
                assert_eq!(name, "hetzner-1");
                assert!(
                    detail.contains("command not found"),
                    "detail should carry shell stderr verbatim: {detail}"
                );
            }
            other => panic!("expected DaemonUnavailable, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("remote upgrade hetzner-1"),
            "msg should hint at `remote upgrade <name>`: {msg}"
        );
    }

    #[test]
    fn classify_probe_exit_127_no_stderr_is_daemon_unavailable() {
        // Some shell configurations swallow stderr on `command not found`
        // (e.g. nologin shell, restrictive sshd config). Exit 127 alone is
        // enough to classify as daemon-missing.
        let outcome = ProbeOutcome {
            exit_status: Some(127),
            stderr: String::new(),
            response: None,
        };
        let err = classify_probe_outcome("hetzner-1", &ssh_target(), outcome)
            .expect_err("exit 127 must error");
        match err {
            RemoteConnectError::DaemonUnavailable { detail, .. } => {
                assert!(
                    detail.contains("daemon binary likely missing"),
                    "synthetic detail should explain the exit code: {detail}"
                );
            }
            other => panic!("expected DaemonUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn classify_probe_connection_refused_is_host_unreachable() {
        // ssh's own diagnostic on a refused connection. Exit 255 + stderr
        // with "Connection refused".
        let outcome = ProbeOutcome {
            exit_status: Some(255),
            stderr: "ssh: connect to host remote.example.com port 22: Connection refused\n"
                .to_string(),
            response: None,
        };
        let err = classify_probe_outcome("hetzner-1", &ssh_target(), outcome)
            .expect_err("connection refused must error");
        match &err {
            RemoteConnectError::HostUnreachable { name, host, detail } => {
                assert_eq!(name, "hetzner-1");
                assert_eq!(host, "viktor@remote.example.com");
                assert!(
                    detail.to_lowercase().contains("connection refused"),
                    "detail should carry ssh stderr verbatim: {detail}"
                );
            }
            other => panic!("expected HostUnreachable, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("Check network connectivity"),
            "msg should hint at network/ssh config: {msg}"
        );
    }

    #[test]
    fn classify_probe_permission_denied_is_host_unreachable() {
        // Auth failure under BatchMode=yes — ssh exits 255 with "Permission
        // denied (publickey)". Treat as HostUnreachable: the daemon may be
        // perfectly fine, but we can't get there to find out.
        let outcome = ProbeOutcome {
            exit_status: Some(255),
            stderr: "viktor@remote.example.com: Permission denied (publickey).\n".to_string(),
            response: None,
        };
        let err = classify_probe_outcome("hetzner-1", &ssh_target(), outcome)
            .expect_err("permission denied must error");
        match err {
            RemoteConnectError::HostUnreachable { detail, .. } => {
                assert!(
                    detail.to_lowercase().contains("permission denied"),
                    "detail should carry ssh stderr verbatim: {detail}"
                );
            }
            other => panic!("expected HostUnreachable, got {other:?}"),
        }
    }

    #[test]
    fn classify_probe_host_key_failure_is_host_unreachable() {
        let outcome = ProbeOutcome {
            exit_status: Some(255),
            stderr: "Host key verification failed.\n".to_string(),
            response: None,
        };
        let err = classify_probe_outcome("hetzner-1", &ssh_target(), outcome)
            .expect_err("host-key failure must error");
        assert!(
            matches!(err, RemoteConnectError::HostUnreachable { .. }),
            "host-key failure must classify as HostUnreachable, got {err:?}"
        );
    }

    #[test]
    fn classify_probe_unknown_failure_defaults_to_daemon_unavailable() {
        // ssh exited cleanly (0) but we got no RESP and no recognizable
        // diagnostic. Most likely a daemon misbehaviour (printed nothing,
        // closed stdout). Default to DaemonUnavailable so the user gets the
        // `remote upgrade` hint.
        let outcome = ProbeOutcome {
            exit_status: Some(0),
            stderr: String::new(),
            response: None,
        };
        let err = classify_probe_outcome("hetzner-1", &ssh_target(), outcome)
            .expect_err("unrecognized failure must error");
        assert!(
            matches!(err, RemoteConnectError::DaemonUnavailable { .. }),
            "default fallback must be DaemonUnavailable, got {err:?}"
        );
    }

    // ----- bridge byte-relay -----

    #[tokio::test]
    async fn bridge_relays_bytes_both_directions() {
        // In-process fake "ssh process": a pair of DuplexStreams stand in
        // for ssh's stdin/stdout. We feed bytes into one side and expect
        // them out the other, going through the bridge socket.
        let (fake_ssh_in_test_side, ssh_stdin) = tokio::io::duplex(64 * 1024);
        let (ssh_stdout, fake_ssh_out_test_side) = tokio::io::duplex(64 * 1024);

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("bridge.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        // Spawn the relay; it will accept exactly once.
        let relay = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap().0;
            relay_once(stream, ssh_stdin, ssh_stdout).await;
        });

        // Dial the bridge as the TUI would.
        let mut client = tokio::net::UnixStream::connect(&socket_path).await.unwrap();

        // viewer → remote: write to client, expect on fake_ssh_in_test_side.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        client.write_all(b"hello").await.unwrap();
        client.flush().await.unwrap();
        let mut got = [0u8; 5];
        let mut fake_in = fake_ssh_in_test_side;
        fake_in.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"hello");

        // remote → viewer: write to fake_ssh_out_test_side, expect on client.
        let mut fake_out = fake_ssh_out_test_side;
        fake_out.write_all(b"world").await.unwrap();
        fake_out.flush().await.unwrap();
        let mut got2 = [0u8; 5];
        client.read_exact(&mut got2).await.unwrap();
        assert_eq!(&got2, b"world");

        // Tear down so the relay returns.
        drop(client);
        drop(fake_in);
        drop(fake_out);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), relay).await;
    }

    #[tokio::test]
    async fn bridge_socket_inode_mode_is_0o600() {
        // The bridge carries the FULL streaming attach protocol for the
        // *remote* daemon (list-agents, start-agent, attach-stream). A
        // world-connectable bridge socket would let any local user hijack
        // that control channel, so the inode must be created at 0o600.
        // Mirrors `daemon::tests::socket_is_0600_immediately_after_bind`.
        //
        // We can't bring up real ssh in a unit test, so we drive
        // `start_bridge` with a target that ssh will spawn against and
        // immediately tear down. `bind_socket` runs *before* the spawn —
        // by the time `start_bridge` returns Ok, the inode mode is settled
        // regardless of ssh's connect outcome. ssh is killed via
        // `kill_on_drop` when the bridge is dropped at end of test.
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("bridge.sock");
        // Localhost target with no user — ssh just needs to fork
        // successfully; BatchMode=yes makes any actual connect fail fast.
        let target = SshTarget {
            host: "127.0.0.1".to_string(),
            user: None,
            port: 22,
            key: None,
        };

        let bridge = start_bridge(&target, socket_path.clone())
            .await
            .expect("start_bridge should bind the socket");

        let meta = std::fs::metadata(&socket_path).expect("socket file should exist");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, BRIDGE_SOCKET_MODE,
            "expected bridge socket at 0o600, got {mode:o}"
        );

        drop(bridge);
    }

    #[tokio::test]
    async fn bridge_cleanup_removes_socket_file_on_drop() {
        // We can't easily spawn `start_bridge` here without a real ssh
        // binary, but we can test the Drop contract directly: build a
        // ConnectBridge with a stub Child + listener task, drop it, and
        // observe the socket file is gone.
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("bridge.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let task = tokio::spawn(async move {
            // Never accepts; cancelled when the JoinHandle is dropped.
            let _ = listener.accept().await;
        });
        // Spawn a trivial child that exits immediately. `true` exists on
        // every POSIX system and lets us populate the `Child` field
        // without a real ssh invocation. kill_on_drop is harmless on a
        // process that has already exited.
        let mut cmd = TokioCommand::new("true");
        cmd.kill_on_drop(true);
        let child = cmd.spawn().expect("spawn `true`");

        assert!(socket_path.exists(), "precondition: socket file present");

        let bridge = ConnectBridge {
            socket_path: socket_path.clone(),
            _listener_task: task,
            _ssh: child,
        };
        drop(bridge);

        assert!(!socket_path.exists(), "Drop should unlink the socket file");
    }
}
