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
//!   Both reject `kind = "kubernetes"` (Phase 3 not yet supported).
//! - **bridge** — [`ConnectBridge`] owns the listener task + the ssh `Child`
//!   and cleans up the socket file on drop. Exposed as a struct so the TUI
//!   loop runs *while* the bridge is alive, and dropping the bridge tears
//!   down both halves.
//! - **CLI handler** — wires lookup/picker + bridge + env-var setup
//!   (`DOT_AGENT_DECK_VIA_DAEMON=1`, `DOT_AGENT_DECK_ATTACH_SOCKET=<bridge>`)
//!   to the existing TUI body.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use thiserror::Error;
use tokio::net::UnixListener;
use tokio::process::{Child, Command as TokioCommand};
use tokio::task::JoinHandle;

use crate::remote::{RemoteConfigError, RemoteEntry, RemotesFile, SshTarget, SystemSshExecutor};

/// Marker `kind` for entries the user added with `--type=kubernetes`. M2.4
/// rejects these explicitly so the message clearly says "Phase 3" instead of
/// surfacing a generic ssh failure deeper in the bridge.
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
        "Remote '{name}' is type 'kubernetes'; kubernetes remotes are not yet supported (Phase 3)."
    )]
    KubernetesNotYetSupported { name: String },
    #[error(
        "No remotes configured. Run `dot-agent-deck remote add <name> --type=ssh <host>` to add one."
    )]
    NoRemotesConfigured,
    #[error("Invalid selection after {attempts} attempts; aborting.")]
    PickerGaveUp { attempts: usize },
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
            "  {idx}) {:<12} (kubernetes)   [Phase 3 — not yet connectable]\n",
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
///   prompting. Kubernetes-only registry still routes through the
///   Phase-3 rejection.
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

/// Start the bridge: bind the socket, spawn ssh, kick off the relay task.
/// Returns the [`ConnectBridge`] for the caller to keep alive while the TUI
/// runs. Caller is responsible for setting `DOT_AGENT_DECK_ATTACH_SOCKET` to
/// `bridge.socket_path()` so the TUI dials the bridge instead of a local
/// daemon.
pub async fn start_bridge(
    target: &SshTarget,
    socket_path: PathBuf,
) -> Result<ConnectBridge, RemoteConnectError> {
    // Stale socket from a prior process that crashed without unlinking would
    // make `bind` return EADDRINUSE. The pid suffix already minimizes the
    // chance, but cleaning up defensively costs nothing.
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;

    let mut cmd = build_tokio_ssh_command(target, "dot-agent-deck daemon attach");
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
    fn connect_lookup_kubernetes_type_phase3_error() {
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
        assert!(
            msg.to_lowercase().contains("phase 3"),
            "msg should mention Phase 3: {msg}"
        );
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
