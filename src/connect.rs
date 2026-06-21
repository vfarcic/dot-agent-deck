//! PRD #76 M2.4 + M2.9 — `dot-agent-deck connect [name]`.
//!
//! After the 2026-05-09 architectural pivot the laptop-side ssh socket
//! bridge was deleted (M2.7); M2.9 reintroduces `connect` as a thin
//! `ssh -t` exec wrapper:
//!
//! 1. **lookup / picker** — `lookup_remote` / `pick_remote` resolve the
//!    registry entry. Kubernetes entries are rejected (planned in PRD #81).
//! 2. **version probe** — one short `ssh <target> dot-agent-deck --version`
//!    classifies host-unreachable, missing-binary, and version-mismatch
//!    outcomes before we hand the terminal over.
//! 3. **exec** — `ssh -t -- <target> env DOT_AGENT_DECK_VIA_DAEMON=1 <install_path>`
//!    runs the deck TUI on the remote with stdio inherited from the user.
//!    The TUI's M2.8 external-daemon mode lazy-spawns the persistent
//!    daemon; the laptop process blocks until ssh exits and propagates the
//!    exit code. The daemon and any agents on the remote keep running
//!    because the daemon is detached.
//!
//! No socket relay or laptop-side daemon ever runs in this flow — that's
//! M2.7's deliberate simplification.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use thiserror::Error;

use crate::daemon_protocol::{AttachResponse, PROTOCOL_VERSION};
use crate::remote::{
    RemoteConfigError, RemoteEntry, RemotesFile, SshError, SshExecutor, SshTarget,
};

/// Marker `kind` for entries the user added with `--type=kubernetes`. M2.4
/// rejects these explicitly so the message clearly points the user at PRD #81
/// instead of surfacing a generic ssh failure deeper in the connect path.
const KIND_KUBERNETES: &str = "kubernetes";

/// Maximum invalid attempts on the picker prompt before bailing. Three is the
/// usual ergonomic choice for a numeric prompt — the user gets two retries
/// after the first miss before we conclude they're not paying attention.
const PICKER_MAX_RETRIES: usize = 3;

/// Absolute install location of `dot-agent-deck` on every registered remote.
/// Hard-coded here for the same reason `remote::install_and_verify` hard-codes
/// it: a non-interactive ssh shell typically doesn't have `~/.local/bin` on
/// PATH, so we always invoke the binary by its absolute path. If the remote
/// install location ever becomes user-configurable, this will become a field
/// on `RemoteEntry`; until then, both `add` and `connect` agree on the same
/// constant.
const REMOTE_INSTALL_PATH: &str = "~/.local/bin/dot-agent-deck";

/// Default cap on the version-probe ssh round-trip. Overridable via
/// `DOT_AGENT_DECK_SSH_PROBE_TIMEOUT_SECS` — useful when a remote is reachable
/// but slow (cold-start VMs, congested networks). The probe is intentionally
/// short: an unresponsive remote should fail-fast rather than wedge `connect`
/// for a TCP timeout. The override is clamped to
/// `[1, PROBE_TIMEOUT_SECS_MAX]` — see [`probe_timeout_secs`].
const PROBE_TIMEOUT_SECS_DEFAULT: u64 = 10;
/// Upper bound on the parsed env var. One hour is well above any realistic
/// cold-start / VPN delay and prevents `Instant::now() + Duration::from_secs(secs)`
/// from panicking on extreme values (e.g. `u64::MAX`). Without this clamp,
/// the panic would unwind between `Command::spawn()` and the polling loop in
/// `remote::run_with_wallclock_kill`, leaking a live ssh child because
/// Rust's `Child` drop does not reap.
const PROBE_TIMEOUT_SECS_MAX: u64 = 3600;
const PROBE_TIMEOUT_ENV: &str = "DOT_AGENT_DECK_SSH_PROBE_TIMEOUT_SECS";

/// Env var the remote TUI reads to choose the M2.8 external-daemon mode.
/// Mirrored from `agent_pty::DOT_AGENT_DECK_VIA_DAEMON`; we don't depend on
/// that module here because connect.rs is built into the laptop CLI surface
/// and shouldn't pull agent-side internals into its dependency graph.
const VIA_DAEMON_ENV: &str = "DOT_AGENT_DECK_VIA_DAEMON";

/// PRD #148: SSH keepalive cadence for the *live* connect session. Once an
/// `ssh -t` session is up ssh otherwise keeps it alive indefinitely with no
/// liveness probing, so a connection killed by laptop sleep is never noticed
/// and the TUI freezes on a dead socket. `ServerAlive*` makes ssh probe the
/// peer over the encrypted channel every `LIVE_KEEPALIVE_INTERVAL_SECS` and
/// drop the session after `LIVE_KEEPALIVE_COUNT_MAX` consecutive unanswered
/// probes, so a dead connection is torn down within roughly
/// `interval × count` (~45s) instead of hanging forever.
///
/// Distinct from the probe path in `remote.rs` (`ServerAliveCountMax=1`, which
/// wants fail-fast): the live interactive session uses a higher count so a
/// brief real network blip (15–30s) doesn't tear down a working session.
const LIVE_KEEPALIVE_INTERVAL_SECS: u64 = 15;
const LIVE_KEEPALIVE_COUNT_MAX: u64 = 3;

/// PRD #148: total number of `ssh -t` spawns `run_connect` will attempt before
/// giving up — the initial connect plus up to `MAX_CONNECT_ATTEMPTS - 1`
/// automatic reconnects after a transport drop (ssh exit 255). Bounded so a
/// genuinely-gone remote surfaces an error and a sane exit code instead of
/// looping forever; with [`RECONNECT_BACKOFF`] this caps the reconnect window
/// at roughly `(MAX_CONNECT_ATTEMPTS - 1) × backoff` plus probe round-trips.
const MAX_CONNECT_ATTEMPTS: usize = 5;

/// PRD #148: fixed delay between reconnect attempts. Gives a just-woken
/// laptop's network a moment to come back (wifi association, DHCP, VPN) before
/// the next re-probe; the probe itself is the reachability gate, this just
/// avoids hammering it the instant ssh reports the drop.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

/// PRD #148: ssh's own exit code for a transport/auth failure (dropped
/// connection, keepalive timeout, refused, auth). ssh passes a clean remote
/// exit through verbatim, so 255 uniquely marks "the transport died, not the
/// user quit" — the signal auto-reconnect keys on. Also returned by
/// `run_connect` when the reconnect budget is exhausted, since a transport
/// failure is ultimately what happened. Mirrors the contract documented at
/// `src/remote.rs` (the exit-255 handling in `SystemSshExecutor::run`).
const SSH_TRANSPORT_FAILURE_EXIT_CODE: i32 = 255;

#[derive(Debug, Error)]
pub enum RemoteConnectError {
    #[error(
        "No remote named '{name}'. Run `dot-agent-deck remote list` to see configured remotes."
    )]
    UnknownName { name: String },
    #[error(
        "Remote '{name}' is type 'kubernetes'; kubernetes remotes are not yet supported (planned in PRD #81)."
    )]
    KubernetesNotYetSupported { name: String },
    #[error("No remotes configured. Run `dot-agent-deck remote add <name> <host>` to add one.")]
    NoRemotesConfigured,
    #[error("Invalid selection after {attempts} attempts; aborting.")]
    PickerGaveUp { attempts: usize },
    /// Couldn't reach the remote at all (network, DNS, refused, timeout).
    /// `detail` carries ssh's stderr verbatim so the user sees what ssh
    /// itself complained about — that's almost always more diagnostic than
    /// any wrapping we could do.
    #[error(
        "Could not reach remote '{name}': {detail}\nCheck your ssh config (`~/.ssh/config`), the host is up, and the network path is open."
    )]
    HostUnreachable { name: String, detail: String },
    /// The remote is reachable but `dot-agent-deck` isn't installed (or
    /// isn't on the absolute install path we expect). Hint at `remote
    /// upgrade` because that re-runs the install pipeline against the same
    /// registry entry.
    #[error(
        "Remote '{name}' is reachable but `dot-agent-deck` was not found at {install_path}. Run `dot-agent-deck remote upgrade {name}` to (re)install."
    )]
    RemoteBinaryMissing { name: String, install_path: String },
    /// Could not even spawn ssh (e.g. ssh binary not on PATH, fork failed).
    /// Distinct from `HostUnreachable` — that one means ssh ran and
    /// reported a transport error, this one means ssh itself never started.
    #[error("Failed to spawn `ssh` for remote '{name}': {source}")]
    SpawnFailed {
        name: String,
        #[source]
        source: std::io::Error,
    },
    /// PRD #76 M2.21: the laptop and the remote speak incompatible
    /// attach-protocol versions. A protocol-version mismatch is fatal: the
    /// wire format isn't backward-compatible and live updates would silently
    /// fail. This is part of the connect *floor* (PRD #161 D3 — "remote too
    /// old to handshake" stays) and is unaffected by the M1.2 removal of the
    /// laptop↔remote version/build *comparison*. `remote`
    /// is `None` when the remote binary is too old to know about the
    /// handshake at all (pre-M2.21 — no `daemon hello` subcommand).
    #[error("Remote '{name}' speaks attach protocol v{remote_str}; laptop speaks v{local}. {upgrade_hint}",
            remote_str = remote.map(|v| v.to_string()).unwrap_or_else(|| "?".to_string()))]
    ProtocolMismatch {
        name: String,
        remote: Option<u32>,
        local: u32,
        upgrade_hint: String,
    },
    /// PRD #76 M2.21: the remote daemon answered the handshake but flagged
    /// `ok: false` in its response. The wire format isn't skewed (a legacy
    /// remote wouldn't reach this branch), so the right user action is to
    /// investigate the remote daemon rather than to upgrade either binary.
    /// `message` carries any `error` string the remote included, when present.
    #[error(
        "Remote '{name}' rejected the protocol handshake{message_suffix}.\nInvestigate the remote daemon (logs, recent restarts, disk space) before retrying.",
        message_suffix = if message.is_empty() { String::new() } else { format!(": {message}") }
    )]
    RemoteHandshakeError { name: String, message: String },
    #[error(transparent)]
    Registry(#[from] RemoteConfigError),
    #[error("I/O error: {0}")]
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
            "  {idx}) {:<12} (kubernetes)   [PRD #81 — not yet connectable]\n",
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
///   prompting. Kubernetes-only registry still routes through the PRD #81
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

// ---------------------------------------------------------------------------
// M2.9 — version probe + ssh -t exec wrapper.
// ---------------------------------------------------------------------------

/// Read the probe-timeout override, falling back to [`PROBE_TIMEOUT_SECS_DEFAULT`].
/// Invalid values fall back silently — a malformed env var should not block
/// `connect` outright.
///
/// Result is clamped to `[1, PROBE_TIMEOUT_SECS_MAX]`:
/// - Lower bound `1`: OpenSSH treats `ConnectTimeout=0` as "no explicit
///   timeout" and `ServerAliveInterval=0` as "keepalives disabled" (so a
///   literal 0 disables the ssh-side guard entirely), AND the laptop-side
///   wallclock kill in `remote::run_with_wallclock_kill` would fire instantly
///   on `Duration::from_secs(0)`. A 1-second floor avoids both fail-open
///   modes; LAN round-trips are well under a second so it's a defensible
///   minimum.
/// - Upper bound `PROBE_TIMEOUT_SECS_MAX` (one hour): prevents
///   `Instant::now() + Duration::from_secs(secs)` from panicking on absurd
///   values (e.g. `u64::MAX`). The clamp closes a self-DoS / ssh-child-leak
///   path: without it, an extreme env var would panic *after* the ssh child
///   was spawned, and `Child::drop` does not reap.
fn probe_timeout_secs() -> u64 {
    std::env::var(PROBE_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(PROBE_TIMEOUT_SECS_DEFAULT)
        .clamp(1, PROBE_TIMEOUT_SECS_MAX)
}

/// Pull the version number out of `dot-agent-deck --version` output.
///
/// Stricter than `remote::parse_version_output` (which is happy with any
/// second whitespace token) because the connect probe uses the parse to
/// distinguish "remote really is dot-agent-deck" from "remote is some
/// other binary at the same path." Requires:
///
/// 1. The first whitespace token to be exactly `dot-agent-deck`.
/// 2. The second token to be at least one digit followed by a dot — a
///    cheap-but-sufficient sanity check that catches "hello world" while
///    accepting both `0.24.5` and `v0.24.5-rc.1`.
fn parse_version_output(stdout: &str) -> Option<String> {
    let mut parts = stdout.split_whitespace();
    let prog = parts.next()?;
    if prog != "dot-agent-deck" {
        return None;
    }
    let version = parts.next()?;
    let stripped = version.strip_prefix('v').unwrap_or(version);
    let mut chars = stripped.chars();
    let first = chars.next()?;
    if !first.is_ascii_digit() {
        return None;
    }
    if !stripped.contains('.') {
        return None;
    }
    Some(version.to_string())
}

/// Run a one-shot version probe over ssh and return the remote's version.
///
/// PRD #161 M1.2 (D3): this probe is part of the connect **floor** — it
/// detects an unreachable host or a missing/foreign binary so the user gets
/// an actionable message before the terminal is handed over. It no longer
/// *compares* the remote version against the laptop's: the laptop is only
/// ssh + a terminal, so a laptop↔remote version difference guards nothing.
/// The returned version string instead feeds the newer-only upgrade nudge
/// ([`maybe_nudge_upgrade`]); an un-upgraded remote connects normally.
///
/// Two failure modes are distinguished so the caller can give the user an
/// actionable message:
///
/// - **Host-unreachable** — ssh itself failed to connect (transport error,
///   surfaced by `SshError::ConnectionRefused` / `Io` / `Other`). Wrapped
///   into `HostUnreachable` with ssh's stderr verbatim.
/// - **Binary-missing** — ssh succeeded but the remote shell reported
///   "command not found" (exit 127) or stdout doesn't look like a version
///   line. Wrapped into `RemoteBinaryMissing` with a `remote upgrade` hint.
///
/// On success the parsed remote version string is returned (e.g. `0.31.0`).
///
/// Auth failures and host-key issues are folded into `HostUnreachable` —
/// the message tells the user to check their ssh config, which is the right
/// recourse for both cases. The `connect` flow can't paper over a missing
/// key the way `remote add` can't, so the wording is uniform.
pub fn probe_remote_version(
    executor: &dyn SshExecutor,
    target: &SshTarget,
    name: &str,
    install_path: &str,
) -> Result<String, RemoteConnectError> {
    // The probe command runs through the remote shell; we use the absolute
    // install_path because non-interactive ssh shells typically don't have
    // ~/.local/bin on PATH (same fix as ea8c748).
    let cmd = format!("{install_path} --version");
    let result = executor.run(target, &cmd);
    match result {
        Err(ssh_err) => Err(map_probe_ssh_error(name, ssh_err)),
        Ok(output) => {
            // Exit 127 (and the typical bash "command not found" message) is
            // the canonical "binary missing" signal. We also treat any
            // non-zero exit whose stderr mentions "not found" as missing —
            // some shells use 126/127 inconsistently for permission vs
            // missing.
            if output.status == 127
                || output.stderr.to_ascii_lowercase().contains("not found")
                || output.stderr.to_ascii_lowercase().contains("no such file")
            {
                return Err(RemoteConnectError::RemoteBinaryMissing {
                    name: name.to_string(),
                    install_path: install_path.to_string(),
                });
            }
            // Any other non-zero exit means the remote ran *something* and
            // it failed; surface it as a host-unreachable-style error so the
            // user sees the underlying message rather than a misleading
            // "binary missing" hint.
            if output.status != 0 {
                let detail = if output.stderr.trim().is_empty() {
                    format!("ssh exited with status {}", output.status)
                } else {
                    output.stderr.trim().to_string()
                };
                return Err(RemoteConnectError::HostUnreachable {
                    name: name.to_string(),
                    detail,
                });
            }
            match parse_version_output(&output.stdout) {
                // PRD #161 M1.2: no laptop↔remote comparison — just surface
                // the remote version for the newer-only nudge decision.
                Some(remote_version) => Ok(remote_version),
                None => {
                    // Status was 0 but stdout doesn't look like a version
                    // line — e.g. `dot-agent-deck` was replaced with a stub
                    // script. Treat as binary-missing because `remote
                    // upgrade` is the right recovery path.
                    Err(RemoteConnectError::RemoteBinaryMissing {
                        name: name.to_string(),
                        install_path: install_path.to_string(),
                    })
                }
            }
        }
    }
}

/// Maximum bytes of stdout the protocol probe accepts before declaring the
/// remote response unparseable. A legitimate `AttachResponse` from `daemon
/// hello` is well under 100 bytes; the cap leaves ~1000x headroom for any
/// future fields and still bounds the in-memory capture so a hostile or
/// broken remote binary that streams unbounded stdout (PRD #76 M2.21 audit
/// P2) can't push the laptop into memory pressure before the probe parses.
/// 64 KiB also matches the typical Linux pipe buffer, so the cap is reached
/// in one buffer's worth of reads.
const PROBE_PROTOCOL_STDOUT_CAP: usize = 64 * 1024;

/// PRD #76 M2.21: run the protocol-version handshake over ssh and decide
/// whether the wire format is compatible.
///
/// The remote runs `<install_path> daemon hello`, which prints a JSON
/// `AttachResponse` carrying `server_version` (the remote binary's
/// compiled-in [`PROTOCOL_VERSION`]). The protocol version is a property of
/// the binary, not of a running daemon, so the static print is equivalent to
/// a Hello round-trip against a running daemon — and avoids spawning the
/// daemon just to answer a version probe.
///
/// Failure modes:
///
/// - Remote answered with `{"ok":false,...}` →
///   [`RemoteConnectError::RemoteHandshakeError`]. The wire shape is
///   compatible; the daemon is signaling an error. User's recovery is to
///   investigate the remote, not to upgrade.
/// - Remote prints a `server_version` that differs from the laptop's
///   [`PROTOCOL_VERSION`] →
///   [`RemoteConnectError::ProtocolMismatch`] with `upgrade_hint` naming
///   which side is older.
/// - Remote exits non-zero (e.g. pre-M2.21 binary that doesn't recognize
///   `daemon hello`) → `ProtocolMismatch { remote: None, .. }`.
/// - Remote exits 0 but stdout doesn't parse as a JSON response with a
///   `server_version` field → same treatment as "remote too old".
///
/// PRD #161 M1.2 (D3): the `server_version == PROTOCOL_VERSION` arm no longer
/// compares the finer-grained `build_version`. That laptop↔remote build-id
/// comparison guarded nothing (the laptop is only ssh + a terminal), so a
/// matched-protocol remote is accepted regardless of build id and an
/// un-upgraded remote connects normally. The structural `PROTOCOL_VERSION`
/// floor ("remote too old to handshake") stays. On success the probe returns
/// the remote's `running_agents` count (when the reply carries one) so the
/// upgrade nudge can state the restart cost; the static `daemon hello` CLI
/// has no registry to enumerate, so that is `None` today.
///
/// Transport failures (`SshError`) fold into `HostUnreachable` via the same
/// `map_probe_ssh_error` the binary-version probe uses — the user's
/// recovery hint (check ssh config) is identical.
///
/// Stdout is capped at [`PROBE_PROTOCOL_STDOUT_CAP`] bytes; see that
/// constant for the threat model.
pub fn probe_remote_protocol(
    executor: &dyn SshExecutor,
    target: &SshTarget,
    name: &str,
    install_path: &str,
) -> Result<Option<usize>, RemoteConnectError> {
    // SAFETY (audit P3b): `install_path` is currently a module-level
    // constant (`REMOTE_INSTALL_PATH`) with no shell metacharacters. If it
    // ever becomes user- or registry-configurable, this interpolation needs
    // a shell-quoting / validation pass before that change ships — the
    // command is handed to the remote shell via `ssh <target> "<cmd>"`, so
    // an attacker-controlled `install_path` could inject arbitrary remote
    // commands. A regression test feeding spaces/quotes/metacharacters in
    // `install_path` would catch this.
    let cmd = format!("{install_path} daemon hello");
    let result = executor.run_capped(target, &cmd, PROBE_PROTOCOL_STDOUT_CAP);
    match result {
        Err(ssh_err) => Err(map_probe_ssh_error(name, ssh_err)),
        Ok(output) => {
            if output.status != 0 {
                // A pre-M2.21 binary doesn't know `daemon hello`. clap exits
                // with status 2 and a "unrecognized subcommand" message; some
                // shells fold the message into stdout, others stderr —
                // either way, the right reading is "remote is older than the
                // handshake landed". The non-zero exit also covers any other
                // unexpected runtime failure on the remote (broken install,
                // dependency missing, etc.) which the user resolves the same
                // way: re-run `remote upgrade`.
                return Err(RemoteConnectError::ProtocolMismatch {
                    name: name.to_string(),
                    remote: None,
                    local: PROTOCOL_VERSION,
                    upgrade_hint: format!("Run `dot-agent-deck remote upgrade {name}`"),
                });
            }
            let resp: AttachResponse = match serde_json::from_str(output.stdout.trim()) {
                Ok(r) => r,
                Err(_) => {
                    return Err(RemoteConnectError::ProtocolMismatch {
                        name: name.to_string(),
                        remote: None,
                        local: PROTOCOL_VERSION,
                        upgrade_hint: format!("Run `dot-agent-deck remote upgrade {name}`"),
                    });
                }
            };
            // Check `ok` before `server_version`: a daemon that responds
            // with `{"ok":false,"error":"..."}` is healthy enough to speak
            // the wire format but is signaling a runtime error. Collapsing
            // it into the legacy-remote hint would point users at the wrong
            // recovery action (audit P3).
            if !resp.ok {
                return Err(RemoteConnectError::RemoteHandshakeError {
                    name: name.to_string(),
                    message: resp.error.unwrap_or_default(),
                });
            }
            match resp.server_version {
                Some(v) if v == PROTOCOL_VERSION => {
                    // PRD #161 M1.2: the wire format is compatible. No
                    // build-id comparison (deleted with the laptop↔remote
                    // probe). Surface the running-agent count when the reply
                    // carries one so the upgrade nudge can state the restart
                    // cost; the static `daemon hello` CLI omits it (`None`).
                    Ok(resp.running_agents.map(|s| s.count))
                }
                Some(v) => {
                    let upgrade_hint = if v < PROTOCOL_VERSION {
                        format!("Run `dot-agent-deck remote upgrade {name}`")
                    } else {
                        "Upgrade your laptop binary".to_string()
                    };
                    Err(RemoteConnectError::ProtocolMismatch {
                        name: name.to_string(),
                        remote: Some(v),
                        local: PROTOCOL_VERSION,
                        upgrade_hint,
                    })
                }
                None => Err(RemoteConnectError::ProtocolMismatch {
                    name: name.to_string(),
                    remote: None,
                    local: PROTOCOL_VERSION,
                    upgrade_hint: format!("Run `dot-agent-deck remote upgrade {name}`"),
                }),
            }
        }
    }
}

/// Translate an `SshError` from the version probe into the connect-side
/// error. Auth + host-key failures fold into `HostUnreachable` because the
/// recovery hint (check ssh config / known_hosts) is the same for the user.
fn map_probe_ssh_error(name: &str, err: SshError) -> RemoteConnectError {
    let detail = match &err {
        SshError::ConnectionRefused { detail, .. } => detail.clone(),
        SshError::AuthFailed { detail, .. } => detail.clone(),
        SshError::Io { source, .. } => source.to_string(),
        SshError::HostKeyVerificationFailed { .. } => err.to_string(),
        SshError::Other { detail, .. } => detail.clone(),
    };
    RemoteConnectError::HostUnreachable {
        name: name.to_string(),
        detail,
    }
}

/// Build the `ssh -t` command that runs the deck TUI on the remote.
///
/// Shape: `ssh -t -o ConnectTimeout=N -p PORT [-i KEY] -- <user@host> env DOT_AGENT_DECK_VIA_DAEMON=1 <install_path>`.
///
/// Notes worth keeping in mind for future edits:
/// - `-t` forces a pty allocation. The remote TUI needs a real terminal to
///   render; without `-t` ratatui sees a pipe and bails.
/// - `BatchMode=yes` from `remote::SystemSshExecutor` is *intentionally
///   absent* here. Connect IS the user's interactive entry point; if ssh
///   needs to prompt for a passphrase or accept a host key, that prompt
///   should reach the user's terminal.
/// - The remote command is passed as a single argv entry. `env VAR=val cmd`
///   is portable on every remote shell we support; we don't need to reach
///   for ssh's `SendEnv` / `AcceptEnv` dance.
/// - The user@host argument goes through `arg(...)` (no shell), so even a
///   hostile host string can't shell-inject locally. Same defense as
///   `remote::SystemSshExecutor::build_command`.
pub fn build_connect_command(target: &SshTarget, install_path: &str) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-t");
    // ConnectTimeout caps only the pre-handshake phase. Once the session is up
    // ssh would otherwise keep it alive indefinitely with no liveness probing,
    // so a connection killed by laptop sleep is never noticed and the TUI
    // freezes on a dead socket. ServerAlive* (PRD #148) makes ssh probe the
    // peer over the encrypted channel (works through NAT/firewalls, unlike
    // TCPKeepAlive) and abort after ServerAliveCountMax consecutive unanswered
    // probes — so a dead connection is dropped within ~interval×count instead
    // of hanging forever. CountMax here is higher than the remote.rs probe
    // path's (=1) so a brief real network blip doesn't tear down a live
    // interactive session.
    cmd.arg("-o")
        .arg(format!("ConnectTimeout={}", probe_timeout_secs()));
    let keepalive_interval = format!("ServerAliveInterval={LIVE_KEEPALIVE_INTERVAL_SECS}");
    let keepalive_count = format!("ServerAliveCountMax={LIVE_KEEPALIVE_COUNT_MAX}");
    cmd.arg("-o").arg(&keepalive_interval);
    cmd.arg("-o").arg(&keepalive_count);
    cmd.arg("-p").arg(target.port.to_string());
    if let Some(key) = &target.key {
        cmd.arg("-i").arg(key);
    }
    cmd.arg("--");
    cmd.arg(target.user_host());
    // `env VAR=value cmd` runs `cmd` with VAR set in its environment. We
    // hard-code the install_path expansion to the remote shell — `~` will
    // be expanded by the remote shell, which is what we want (and what the
    // install pipeline relies on).
    cmd.arg(format!(
        "env {VIA_DAEMON_ENV}=1 {install_path}",
        VIA_DAEMON_ENV = VIA_DAEMON_ENV,
        install_path = install_path,
    ));
    cmd
}

/// Abstraction over actually spawning `ssh -t` and waiting on it. Tests use
/// a fake spawner to assert exit-code propagation and `last_connected`
/// bookkeeping without spawning a real ssh.
pub trait ConnectSpawner {
    /// Spawn the connect command, inherit stdio, and block until the child
    /// exits. Return ssh's exit code (or `1` if the child died of a signal).
    fn spawn(&self, target: &SshTarget, install_path: &str) -> Result<i32, std::io::Error>;
}

/// Production spawner: builds the ssh command via [`build_connect_command`]
/// and runs it inheriting stdin/stdout/stderr (Command::status does this by
/// default).
pub struct SystemConnectSpawner;

impl SystemConnectSpawner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SystemConnectSpawner {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectSpawner for SystemConnectSpawner {
    fn spawn(&self, target: &SshTarget, install_path: &str) -> Result<i32, std::io::Error> {
        let mut cmd = build_connect_command(target, install_path);
        let status = cmd.status()?;
        Ok(exit_code_from_status(&status))
    }
}

/// PRD #148: abstraction over the inter-attempt backoff sleep in the
/// reconnect loop. Production sleeps for real ([`SleepBackoff`]); unit tests
/// inject a recorder that returns immediately and counts invocations, so the
/// reconnect state machine can be exercised without real wall-clock delays.
/// Modeled on the [`ConnectSpawner`] / [`SshExecutor`] seams already used to
/// keep the connect path testable.
pub trait ReconnectBackoff {
    /// Sleep before the next reconnect attempt. `attempt` is the 1-based index
    /// of the reconnect about to be made (the production impl uses a fixed
    /// delay and ignores it; the parameter leaves room for an exponential
    /// policy later without another signature change).
    fn backoff(&self, attempt: usize);
}

/// Production backoff: sleeps [`RECONNECT_BACKOFF`] on the calling thread.
pub struct SleepBackoff;

impl SleepBackoff {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SleepBackoff {
    fn default() -> Self {
        Self::new()
    }
}

impl ReconnectBackoff for SleepBackoff {
    fn backoff(&self, _attempt: usize) {
        std::thread::sleep(RECONNECT_BACKOFF);
    }
}

/// PRD #161 M1.2: abstraction over running `remote upgrade` (binary swap only)
/// from the connect nudge's `y` path. Modeled on the [`ConnectSpawner`] /
/// [`ReconnectBackoff`] seams: production swaps the remote binary via
/// [`crate::remote::upgrade`]; unit tests inject a fake that records the call
/// and returns a scripted success/failure so the nudge's upgrade-then-connect
/// and upgrade-failure-fallback branches are exercisable without ssh.
///
/// **Binary-swap-only by contract (D3):** the upgrader MUST NOT restart the
/// remote daemon or kill agents. Any daemon restart is the Part-A handshake's
/// job on the remote's own machine — the connect `y`-path only orchestrates
/// the binary swap, then connects.
pub trait RemoteUpgrader {
    /// Upgrade remote `name` to `version` (the laptop's version). `Ok(())` on
    /// success; `Err(msg)` carries a user-facing failure reason so the nudge
    /// can print a clear fallback line before connecting to the existing
    /// version (D4: never strand).
    fn upgrade(&self, name: &str, version: &str) -> Result<(), String>;
}

/// Production upgrader: runs the binary-swap-only [`crate::remote::upgrade`]
/// flow against the registry at `remotes_path`. Uses a fresh, *uncapped*
/// [`crate::remote::SystemSshExecutor`] (NOT the wallclock-capped probe
/// executor) because a release download + install can legitimately run longer
/// than the short version-probe timeout.
pub struct SystemRemoteUpgrader {
    remotes_path: PathBuf,
}

impl SystemRemoteUpgrader {
    pub fn new(remotes_path: PathBuf) -> Self {
        Self { remotes_path }
    }
}

impl RemoteUpgrader for SystemRemoteUpgrader {
    fn upgrade(&self, name: &str, version: &str) -> Result<(), String> {
        let opts = crate::remote::UpgradeOptions {
            name: name.to_string(),
            version: version.to_string(),
            no_install: false,
            release_base: crate::remote::RELEASE_BASE.to_string(),
        };
        let executor = crate::remote::SystemSshExecutor::new();
        crate::remote::upgrade(&opts, &executor, &self.remotes_path)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// PRD #161 M1.2 (D3): is the laptop strictly newer than the remote? The
/// connect nudge is **newer-only** — it never suggests a downgrade or a no-op
/// same-version upgrade — so an equal or older laptop returns `false` and no
/// prompt is shown. Both sides are parsed as semver (stripping the optional
/// `v` prefix the install pipeline may carry); if either fails to parse we
/// conservatively return `false`, so a malformed version string can never
/// trigger a spurious upgrade prompt.
fn laptop_is_newer(local: &str, remote: &str) -> bool {
    let parse = |s: &str| semver::Version::parse(s.strip_prefix('v').unwrap_or(s)).ok();
    match (parse(local), parse(remote)) {
        (Some(l), Some(r)) => l > r,
        _ => false,
    }
}

/// PRD #161 M1.2 (D3/D4): the one-step, laptop-side, pre-handover upgrade
/// nudge that replaces the deleted laptop↔remote enforcement.
///
/// Shown **only** when the laptop is strictly newer than the remote
/// ([`laptop_is_newer`]) *and* stdin is a TTY (`is_tty`); otherwise this is a
/// no-op and `connect` proceeds against the existing remote. The prompt
/// defaults to **No** — empty / `Enter` / EOF / anything that isn't an
/// explicit `y`/`yes`:
///
/// - `y`/`yes` → run `remote upgrade` (binary swap only) via `upgrader`, then
///   connect. If the upgrade fails, print a clear fallback line and connect to
///   the **existing** version anyway (never strand — D4). The Part-A handshake
///   then owns any daemon restart on attach.
/// - `Enter` / `n` / EOF → connect as-is against the existing version.
///
/// `agent_count` (from the `daemon hello` probe's `running_agents`) is folded
/// into the prompt as "(N running agents)" when known, so the user sees the
/// restart cost the upgrade incurs on attach. It is `None` on the current
/// static-probe path; the note is simply omitted in that case.
///
/// Generic over `BufRead` / `Write` so tests inject fake I/O (same seam as
/// [`pick_remote`]).
#[allow(clippy::too_many_arguments)]
fn maybe_nudge_upgrade<R: BufRead, W: Write>(
    upgrader: &dyn RemoteUpgrader,
    name: &str,
    remote_version: &str,
    local_version: &str,
    agent_count: Option<usize>,
    is_tty: bool,
    input: &mut R,
    output: &mut W,
) -> Result<(), RemoteConnectError> {
    // Newer-only: never suggest a downgrade or a same-version no-op.
    if !laptop_is_newer(local_version, remote_version) {
        return Ok(());
    }
    // Non-TTY: no one to answer the prompt — connect as-is, no nudge.
    if !is_tty {
        return Ok(());
    }

    let agents_note = match agent_count {
        Some(n) if n > 0 => format!(" ({n} running agents)"),
        _ => String::new(),
    };
    write!(
        output,
        "Remote '{name}' runs {remote_version}; you have {local_version}{agents_note}. Upgrade and connect? [y/N] "
    )?;
    output.flush()?;

    let mut line = String::new();
    let n = input.read_line(&mut line)?;
    let answer = line.trim();
    // Default N: empty input / bare Enter / EOF / anything but an explicit yes.
    let yes = n != 0 && (answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"));
    if !yes {
        return Ok(());
    }

    // `y` → binary-swap upgrade to the laptop's version, then connect. A
    // failure falls back to connecting the existing version with a clear
    // message; the failure semantics are owned by `remote upgrade` (D3/D4).
    match upgrader.upgrade(name, local_version) {
        Ok(()) => Ok(()),
        Err(msg) => {
            writeln!(
                output,
                "warning: upgrade of remote '{name}' failed: {msg}\nConnecting to the existing {remote_version} install instead."
            )?;
            Ok(())
        }
    }
}

/// PRD #148: best-effort restore of a sane local terminal after auto-reconnect
/// gives up.
///
/// When ssh dies on a transport failure, the *remote* TUI never ran its own
/// teardown (leave alternate screen, show cursor, disable mouse reporting), so
/// the laptop terminal is left in the remote app's screen state. A successful
/// reconnect re-inits the terminal via the next `ssh -t`; but when we exhaust
/// the retry budget there is no new session to fix it, so we reset it here.
///
/// ssh normally restores the local line discipline itself (it owns the raw
/// mode it set for `-t`), but we still attempt a cooked-mode restore in case
/// it didn't. Everything is best-effort: errors are ignored (no controlling
/// tty under tests; an unwritable terminal can't be helped on the give-up
/// path anyway).
fn restore_local_terminal() {
    // Cooked-mode restore in case ssh's own cleanup didn't run.
    let _ = crossterm::terminal::disable_raw_mode();
    // Leave the alternate screen, show the cursor, disable the common
    // mouse-tracking modes, and reset SGR attributes — the state a
    // ratatui/crossterm TUI typically leaves set.
    let mut out = std::io::stdout();
    let _ =
        out.write_all(b"\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[0m");
    let _ = out.flush();
}

/// Map a child `ExitStatus` to the conventional shell exit code.
///
/// - Normal exit: pass through `status.code()` verbatim, so a remote shell
///   that exits 17 makes the laptop process exit 17 (mirrors ssh's own
///   contract one level up).
/// - Killed by signal: encode as `128 + signal`. SIGINT (Ctrl-C) → 130,
///   SIGTERM → 143, SIGKILL → 137. This is what every Unix shell does and
///   what scripts checking `$?` already expect; mapping signal-exits to a
///   bare `1` (the M2.9 first cut) erased that information.
/// - Truly unknown (neither code nor signal): fall back to `1`. This branch
///   is unreachable on Unix in practice — `ExitStatus` is always one of the
///   two — but Rust's stdlib doesn't promise it, so we keep the fallback
///   rather than `unreachable!()`-ing.
fn exit_code_from_status(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    1
}

/// Bump `last_connected` on the registry entry whose name matches `name`.
/// Returns the new timestamp on success; absent entry is a silent no-op
/// (the entry could have been removed concurrently — better to skip than to
/// crash the post-session bookkeeping).
fn touch_last_connected(name: &str, path: &Path) -> Result<Option<String>, RemoteConfigError> {
    let mut registry = RemotesFile::load(path)?;
    let Some(idx) = registry.remotes.iter().position(|r| r.name == name) else {
        return Ok(None);
    };
    let now = chrono::Utc::now().to_rfc3339();
    registry.remotes[idx].last_connected = Some(now.clone());
    registry.save(path)?;
    Ok(Some(now))
}

/// PRD #148: classify a probe error as the "host not reachable *yet*" class
/// (transient) vs a genuine incompatibility (fatal). Every ssh-transport-level
/// probe failure folds into [`RemoteConnectError::HostUnreachable`] (see
/// `map_probe_ssh_error`), so that single variant captures the reachability
/// class. Everything else — binary missing, protocol/build mismatch, handshake
/// rejection, spawn/IO/registry failures — is a problem retrying can't fix, so
/// it stays fatal even on a reconnect and we never blindly retry into an
/// incompatible or absent remote.
fn is_reachability_error(err: &RemoteConnectError) -> bool {
    matches!(err, RemoteConnectError::HostUnreachable { .. })
}

/// PRD #148: which situation triggered a transport failure. This controls
/// ONLY the user-visible "we'll retry" line printed before the backoff — the
/// give-up message, terminal restore, and bounded-retry logic are identical
/// for both. Distinguishing them matters because the two read very differently
/// to a user (Greptile PR #152 finding 1): a dropped live session is "you were
/// connected and it broke", a still-unreachable reconnect probe is "the host
/// hasn't come back yet".
#[derive(Clone, Copy)]
enum TransportFailureKind {
    /// A live `ssh -t` session was up and then dropped (spawn returned 255).
    SessionDropped,
    /// A reconnect-attempt probe found the host still unreachable; no session
    /// ran this round.
    StillUnreachable,
}

/// PRD #148: the stderr line printed before backing off for another attempt.
/// Pure (no I/O) so it can be unit-tested directly without capturing process
/// stderr. `attempt` is the 1-based number of the attempt that just failed;
/// the message advertises `attempt + 1`, the one we're about to make.
fn retry_message(kind: TransportFailureKind, name: &str, attempt: usize) -> String {
    let next = attempt + 1;
    match kind {
        // Live session dropped — "connection lost" is accurate here.
        TransportFailureKind::SessionDropped => {
            format!(
                "connection to '{name}' lost — reconnecting… (attempt {next}/{MAX_CONNECT_ATTEMPTS})"
            )
        }
        // Host was already unreachable this round; "connection lost" would
        // mislead the user into thinking a fresh session dropped again.
        TransportFailureKind::StillUnreachable => {
            format!(
                "'{name}' not reachable yet — retrying… (attempt {next}/{MAX_CONNECT_ATTEMPTS})"
            )
        }
    }
}

/// PRD #148: shared bounded-retry decision for a transport failure on attempt
/// `attempt` (1-based) — either a spawn that returned 255 ([`TransportFailureKind::SessionDropped`])
/// or a reconnect-time reachability probe failure
/// ([`TransportFailureKind::StillUnreachable`]). Returns `Some(exit_code)` when
/// the retry budget is exhausted (caller should `return Ok(code)`); returns
/// `None` after backing off (caller should `continue` to the next attempt).
///
/// On give-up it restores a sane local terminal — the remote TUI never got to
/// leave alt screen / raw mode when the transport died — and prints the
/// "giving up" line to stderr, so BOTH paths restore the terminal and surface
/// the same give-up message (only the per-attempt "we'll retry" line, via
/// [`retry_message`], differs by `kind`). Keeping this in one place is what
/// guarantees a still-unreachable host on a reconnect can't escape the loop
/// without counting against the budget, backing off, and (on exhaustion)
/// restoring the terminal.
fn on_transport_failure(
    attempt: usize,
    name: &str,
    kind: TransportFailureKind,
    backoff: &dyn ReconnectBackoff,
) -> Option<i32> {
    if attempt >= MAX_CONNECT_ATTEMPTS {
        // Budget exhausted: the remote stayed unreachable across every attempt.
        restore_local_terminal();
        eprintln!(
            "connection to '{}' lost and could not be re-established after {} attempts — giving up.",
            name, MAX_CONNECT_ATTEMPTS
        );
        return Some(SSH_TRANSPORT_FAILURE_EXIT_CODE);
    }
    // Message goes to stderr, between the handed-over terminal sessions.
    eprintln!("{}", retry_message(kind, name, attempt));
    backoff.backoff(attempt);
    None
}

/// Orchestrate one connect: probe → spawn `ssh -t` → record `last_connected`,
/// with PRD #148 auto-reconnect wrapped around the probe/spawn step.
///
/// The whole probe + spawn + bookkeeping flow runs inside a loop keyed on
/// ssh's transport-failure exit code (255). ssh passes a clean remote exit
/// through verbatim (TUI quit → 0, Ctrl-C → 130, SIGTERM → 143, remote panic
/// → its own non-255 code) and reserves 255 for its own connection/auth
/// failures, so we reconnect *only* on a dropped transport — never on a user
/// quitting or a crashing remote TUI (which would just crash again):
///
/// - **exit 255** → print a "reconnecting…" line to stderr, back off
///   ([`ReconnectBackoff`], injectable so tests don't really sleep), and loop
///   to re-probe + re-spawn. The remote daemon is external and persistent
///   (#93) and the remote TUI restores its view from daemon state on startup
///   (#89), so a fresh `ssh -t` re-attaches to the *same* running agents.
/// - **any other code** → terminal; returned verbatim. ssh's exit code
///   propagates as the laptop process's exit code (a remote shell that exits
///   17 makes the laptop exit 17 — same contract one level up).
///
/// The re-probe each attempt doubles as the reachability gate: right after
/// sleep/wake the laptop's wifi/DHCP may not be back, so the *initial*
/// connect's probe is fatal (returns the existing "Could not reach…" error),
/// but a probe **reachability** failure on a *reconnect* attempt
/// ([`is_reachability_error`]) folds into the SAME bounded-retry/backoff/
/// give-up path a dropped session uses — otherwise a still-down network would
/// let the reconnect escape the loop without counting against the budget,
/// backing off, or restoring the terminal. Genuine incompatibilities
/// (protocol mismatch, missing binary) stay fatal even on a reconnect.
///
/// Reconnection is bounded by [`MAX_CONNECT_ATTEMPTS`]. When exhausted we
/// restore a sane local terminal (the remote TUI never got to leave alt
/// screen / raw mode when ssh died mid-session), print a "giving up" line, and
/// return [`SSH_TRANSPORT_FAILURE_EXIT_CODE`] — surfacing the transport
/// failure as the process exit code rather than looping forever.
///
/// `last_connected` is only updated on a genuine clean exit (status 0), never
/// on an intermediate 255 reconnect:
///
/// - User exits the TUI cleanly → laptop registry records the session.
/// - User Ctrl-C's the ssh, daemon dies, network drops mid-session, or we
///   exhaust reconnects → no bookkeeping update; the registry only ever shows
///   sessions that actually ran to completion.
#[allow(clippy::too_many_arguments)]
pub fn run_connect<R: BufRead, W: Write>(
    entry: &RemoteEntry,
    executor: &dyn SshExecutor,
    spawner: &dyn ConnectSpawner,
    backoff: &dyn ReconnectBackoff,
    upgrader: &dyn RemoteUpgrader,
    remotes_path: &Path,
    local_version: &str,
    install_path: &str,
    input: &mut R,
    output: &mut W,
    is_tty: bool,
) -> Result<i32, RemoteConnectError> {
    let target = entry.ssh_target();

    // 1-based count of connect attempts made (initial connect + reconnects).
    // This is the retry budget: a transport failure on attempt N — a spawn
    // that returned 255, OR (on a reconnect) a reachability probe failure —
    // gives up once N reaches MAX_CONNECT_ATTEMPTS.
    let mut attempt = 0usize;
    loop {
        attempt += 1;

        // Stage 1: binary-version probe. PRD #161 M1.2 (D3): this no longer
        // compares laptop↔remote versions (the laptop is only ssh + a
        // terminal, so the comparison guarded nothing). It still catches an
        // unreachable host or a missing/foreign binary (the connect floor) and
        // returns the remote version for the newer-only nudge below. A probe
        // *error* is fatal on the initial connect (returns the existing "Could
        // not reach…" UX), but on a RECONNECT a reachability failure (the
        // normal "wifi/DHCP not back yet" state after sleep/wake) folds into
        // the same bounded-retry/backoff/give-up path a dropped session uses —
        // see `on_transport_failure`.
        let remote_version =
            match probe_remote_version(executor, &target, &entry.name, install_path) {
                Ok(v) => v,
                Err(e) if attempt > 1 && is_reachability_error(&e) => {
                    match on_transport_failure(
                        attempt,
                        &entry.name,
                        TransportFailureKind::StillUnreachable,
                        backoff,
                    ) {
                        Some(code) => return Ok(code),
                        None => continue,
                    }
                }
                Err(e) => return Err(e),
            };

        // Stage 1b: protocol-version handshake — the structural floor ("remote
        // too old to handshake" stays; the build-id comparison was removed with
        // the laptop probe, see PRD #161 M1.2). Returns the remote's
        // running-agent count when the reply carries one, so the nudge can
        // state the restart cost. Same reachability-vs-fatal split as the
        // version probe: a transient transport failure on a reconnect retries,
        // but a protocol incompatibility stays fatal so we never re-spawn into
        // an incompatible remote.
        let agent_count = match probe_remote_protocol(executor, &target, &entry.name, install_path)
        {
            Ok(count) => count,
            Err(e) if attempt > 1 && is_reachability_error(&e) => {
                match on_transport_failure(
                    attempt,
                    &entry.name,
                    TransportFailureKind::StillUnreachable,
                    backoff,
                ) {
                    Some(code) => return Ok(code),
                    None => continue,
                }
            }
            Err(e) => return Err(e),
        };

        // Stage 1c: one-step, pre-handover upgrade nudge (PRD #161 M1.2). Only
        // on the *initial* connect (attempt 1) — a reconnect after a transport
        // drop must re-attach without re-prompting. Newer-only + non-TTY-skip +
        // default-N are decided inside `maybe_nudge_upgrade`; on `y` it runs the
        // binary-swap `remote upgrade` then returns so we connect to the
        // upgraded remote (a failed upgrade falls back to the existing version).
        if attempt == 1 {
            maybe_nudge_upgrade(
                upgrader,
                &entry.name,
                &remote_version,
                local_version,
                agent_count,
                is_tty,
                input,
                output,
            )?;
        }

        // Stage 2: hand the terminal over. This blocks until the user exits.
        let exit_code = spawner.spawn(&target, install_path).map_err(|source| {
            RemoteConnectError::SpawnFailed {
                name: entry.name.clone(),
                source,
            }
        })?;

        // Stage 3: route on the exit code. Reconnect ONLY on ssh's
        // transport-failure code (255); any other code is terminal.
        if exit_code == SSH_TRANSPORT_FAILURE_EXIT_CODE {
            match on_transport_failure(
                attempt,
                &entry.name,
                TransportFailureKind::SessionDropped,
                backoff,
            ) {
                Some(code) => return Ok(code),
                None => continue,
            }
        }

        // Terminal exit (clean 0, Ctrl-C 130, SIGTERM 143, non-255 crash).
        // Bookkeeping only on a genuine clean exit — see doc comment.
        if exit_code == 0 {
            // Registry I/O failures here are reported but don't fail the
            // command — the user already finished their session, and a busted
            // registry shouldn't surface as a connect error after the fact.
            if let Err(e) = touch_last_connected(&entry.name, remotes_path) {
                eprintln!(
                    "warning: connect to '{}' succeeded but the registry update failed: {}",
                    entry.name, e
                );
            }
        }

        return Ok(exit_code);
    }
}

/// Public entry point used by `main.rs`'s `run_connect` handler. Wires up
/// the production executor + spawner so the call site stays a one-liner;
/// tests construct their own `run_connect` calls with fakes.
pub fn run_connect_default(
    entry: &RemoteEntry,
    remotes_path: &Path,
    local_version: &str,
) -> Result<i32, RemoteConnectError> {
    use crate::remote::SystemSshExecutor;
    // Probe executor carries the wallclock cap; without this, a remote that
    // accepts the TCP connection but never produces stdout would pin
    // `connect` indefinitely (cmd.output() has no timeout of its own).
    let executor = SystemSshExecutor::with_wallclock_timeout(probe_timeout_secs());
    let spawner = SystemConnectSpawner::new();
    let backoff = SleepBackoff::new();
    // The upgrader runs the binary-swap `remote upgrade` with its OWN uncapped
    // executor (a release download can outlast the short probe timeout).
    let upgrader = SystemRemoteUpgrader::new(remotes_path.to_path_buf());
    // PRD #161 M1.2: the nudge reads from stdin / writes the prompt to stdout
    // and is skipped entirely when stdin is not a TTY.
    let stdin = std::io::stdin();
    let is_tty = stdin.is_terminal();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    run_connect(
        entry,
        &executor,
        &spawner,
        &backoff,
        &upgrader,
        remotes_path,
        local_version,
        REMOTE_INSTALL_PATH,
        &mut input,
        &mut output,
        is_tty,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(OsStr::to_string_lossy)
            .map(|s| s.into_owned())
            .collect()
    }

    fn ssh_target(target: &str, port: u16) -> SshTarget {
        SshTarget::parse(target, port, None)
    }

    // -- ssh -t command construction --

    #[test]
    fn build_connect_command_has_t_flag_and_via_daemon_env() {
        let target = ssh_target("viktor@host.example.com", 22);
        let cmd = build_connect_command(&target, "~/.local/bin/dot-agent-deck");
        let args = args_of(&cmd);

        // -t must be present (remote TUI requires a pty); install_path is
        // passed verbatim and the env var is set on the remote shell.
        assert_eq!(args[0], "-t", "must request remote pty: {args:?}");
        // PRD #148: the live session carries ConnectTimeout *and* the
        // ServerAlive* keepalive pair so a sleep-killed connection is detected
        // and dropped instead of hanging forever. The keepalive count is the
        // tolerant value (3), distinct from the probe path's fail-fast 1.
        assert!(
            args.iter().any(|a| a.starts_with("ConnectTimeout=")),
            "must set ConnectTimeout: {args:?}"
        );
        assert!(
            args.iter()
                .any(|a| a == &format!("ServerAliveInterval={LIVE_KEEPALIVE_INTERVAL_SECS}")),
            "must set ServerAliveInterval={LIVE_KEEPALIVE_INTERVAL_SECS}: {args:?}"
        );
        assert!(
            args.iter()
                .any(|a| a == &format!("ServerAliveCountMax={LIVE_KEEPALIVE_COUNT_MAX}")),
            "must set ServerAliveCountMax={LIVE_KEEPALIVE_COUNT_MAX}: {args:?}"
        );
        assert!(
            args.iter()
                .any(|a| a == "DOT_AGENT_DECK_VIA_DAEMON=1"
                    || a.contains("DOT_AGENT_DECK_VIA_DAEMON=1")),
            "must set DOT_AGENT_DECK_VIA_DAEMON=1: {args:?}"
        );
        // `--` precedes the destination so a hostile-looking host string
        // can't be reinterpreted as a flag (same hygiene as
        // remote::SystemSshExecutor::build_command).
        let dash_dash_pos = args
            .iter()
            .position(|a| a == "--")
            .expect("-- must precede destination");
        // Destination is one argv entry — not double-quoted, not joined.
        assert_eq!(
            args[dash_dash_pos + 1],
            "viktor@host.example.com",
            "destination must be a single un-quoted argv entry: {args:?}"
        );
        // Remote command is the LAST argv entry and contains the install
        // path verbatim — the remote shell expands `~`.
        let remote_cmd = args.last().expect("remote command must be present");
        assert!(
            remote_cmd.contains("~/.local/bin/dot-agent-deck"),
            "remote command must use install_path verbatim: {remote_cmd}"
        );
        assert!(
            remote_cmd.starts_with("env DOT_AGENT_DECK_VIA_DAEMON=1 "),
            "remote command must lead with env var: {remote_cmd}"
        );
    }

    #[test]
    fn build_connect_command_passes_port_and_key() {
        let target = SshTarget {
            host: "host".to_string(),
            user: Some("u".to_string()),
            port: 2222,
            key: Some(std::path::PathBuf::from("/tmp/key id_rsa")),
        };
        let cmd = build_connect_command(&target, "~/.local/bin/dot-agent-deck");
        let args = args_of(&cmd);
        // -p PORT must appear (custom port survives the round-trip).
        let p_pos = args.iter().position(|a| a == "-p").expect("missing -p");
        assert_eq!(args[p_pos + 1], "2222");
        // -i KEY argument preserved as a single argv entry, including spaces.
        let i_pos = args.iter().position(|a| a == "-i").expect("missing -i");
        assert_eq!(args[i_pos + 1], "/tmp/key id_rsa");
    }

    #[test]
    fn build_connect_command_omits_key_when_none() {
        let target = ssh_target("h", 22);
        let cmd = build_connect_command(&target, REMOTE_INSTALL_PATH);
        let args = args_of(&cmd);
        assert!(
            !args.iter().any(|a| a == "-i"),
            "no -i when key is None: {args:?}"
        );
    }

    // ----- parse_version_output edge cases -----

    #[test]
    fn parse_version_output_accepts_canonical_and_v_prefixed() {
        // Both shapes the install pipeline produces are accepted; the parser
        // returns the version token verbatim (callers compare strings).
        assert_eq!(
            parse_version_output("dot-agent-deck 0.24.5\n"),
            Some("0.24.5".to_string())
        );
        assert_eq!(
            parse_version_output("dot-agent-deck v0.24.5-rc.1\n"),
            Some("v0.24.5-rc.1".to_string())
        );
    }

    #[test]
    fn parse_version_output_rejects_wrong_program_name() {
        // A binary named something else at the install path must NOT be
        // mistaken for dot-agent-deck — even if it prints a sensible version
        // line. The probe upgrades this to RemoteBinaryMissing because
        // `remote upgrade` is the right recovery path.
        assert_eq!(parse_version_output("nano 7.0\n"), None);
        assert_eq!(parse_version_output("ssh OpenSSH_9.0\n"), None);
    }

    #[test]
    fn parse_version_output_rejects_garbage_and_empty() {
        // Stub script / missing program / unparseable output all collapse to
        // None; the probe layer turns this into RemoteBinaryMissing.
        assert_eq!(parse_version_output(""), None);
        assert_eq!(parse_version_output("\n\n"), None);
        assert_eq!(parse_version_output("hello world"), None);
        // Right program name but version doesn't start with a digit.
        assert_eq!(parse_version_output("dot-agent-deck unknown"), None);
        // Right program name but version has no `.` separator (a build that
        // accidentally printed a single integer is unlikely to be ours).
        assert_eq!(parse_version_output("dot-agent-deck 9"), None);
    }

    // ----- signal exit-code mapping -----

    #[cfg(unix)]
    #[test]
    fn exit_code_from_status_passes_through_normal_exit() {
        // A plain `exit 17` from the remote should round-trip verbatim,
        // mirroring ssh's own contract one level up.
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(17 << 8);
        assert_eq!(exit_code_from_status(&status), 17);
    }

    #[cfg(unix)]
    #[test]
    fn exit_code_from_status_maps_signal_to_128_plus_signal() {
        // `from_raw` packs signals into the low 7 bits of the status word,
        // matching how the kernel reports them. SIGINT(2) → 130, SIGTERM(15)
        // → 143, SIGKILL(9) → 137 — the conventional shell encoding that
        // every `$?` consumer already understands.
        use std::os::unix::process::ExitStatusExt;
        let sigint = std::process::ExitStatus::from_raw(2);
        assert_eq!(exit_code_from_status(&sigint), 130);
        let sigterm = std::process::ExitStatus::from_raw(15);
        assert_eq!(exit_code_from_status(&sigterm), 143);
        let sigkill = std::process::ExitStatus::from_raw(9);
        assert_eq!(exit_code_from_status(&sigkill), 137);
    }

    // ----- PRD #148: auto-reconnect state machine -----

    use crate::remote::SshOutput;
    use std::cell::Cell;

    const TEST_LOCAL_VERSION: &str = "9.9.9";

    /// Fake executor whose probes always succeed: `--version` echoes the
    /// laptop version and `daemon hello` returns a matching protocol/build
    /// handshake. Counts invocations so a test can confirm the *full* probe
    /// (version + protocol) re-runs on every reconnect attempt.
    struct ProbeOkExecutor {
        local_version: String,
        runs: Cell<usize>,
    }

    impl ProbeOkExecutor {
        fn new(local_version: &str) -> Self {
            Self {
                local_version: local_version.to_string(),
                runs: Cell::new(0),
            }
        }
        fn run_count(&self) -> usize {
            self.runs.get()
        }
    }

    impl SshExecutor for ProbeOkExecutor {
        fn run(&self, _target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
            self.runs.set(self.runs.get() + 1);
            if command.contains("daemon hello") {
                // `hello()` carries server_version == PROTOCOL_VERSION and
                // build_version == local_build_id(), which is exactly what
                // probe_remote_protocol compares against in-process — so the
                // handshake matches without any env juggling.
                let body = serde_json::to_string(&AttachResponse::hello(PROTOCOL_VERSION))
                    .expect("serialize hello");
                Ok(SshOutput {
                    status: 0,
                    stdout: body,
                    stderr: String::new(),
                })
            } else if command.contains("--version") {
                Ok(SshOutput {
                    status: 0,
                    stdout: format!("dot-agent-deck {}\n", self.local_version),
                    stderr: String::new(),
                })
            } else {
                panic!("unexpected probe command: {command}");
            }
        }
    }

    /// Fake executor that lets the FIRST connect's probe succeed, then fails
    /// every later `--version` probe with an ssh transport error. The connect
    /// path folds that into [`RemoteConnectError::HostUnreachable`] — the
    /// canonical "host not reachable yet" state on a reconnect right after
    /// sleep/wake, before wifi/DHCP recover. Used to prove a reconnect-time
    /// reachability failure is bounded (counts against the budget, backs off)
    /// instead of escaping the loop on the first re-probe.
    ///
    /// `daemon hello` always succeeds, but it's only ever reached on the first
    /// attempt: on reconnects the `--version` probe fails first and short-
    /// circuits before the protocol handshake runs.
    struct UnreachableAfterFirstConnectExecutor {
        local_version: String,
        version_calls: Cell<usize>,
    }

    impl UnreachableAfterFirstConnectExecutor {
        fn new(local_version: &str) -> Self {
            Self {
                local_version: local_version.to_string(),
                version_calls: Cell::new(0),
            }
        }
    }

    impl SshExecutor for UnreachableAfterFirstConnectExecutor {
        fn run(&self, target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
            if command.contains("daemon hello") {
                let body = serde_json::to_string(&AttachResponse::hello(PROTOCOL_VERSION))
                    .expect("serialize hello");
                return Ok(SshOutput {
                    status: 0,
                    stdout: body,
                    stderr: String::new(),
                });
            }
            if command.contains("--version") {
                let n = self.version_calls.get();
                self.version_calls.set(n + 1);
                if n == 0 {
                    // First connect: reachable.
                    return Ok(SshOutput {
                        status: 0,
                        stdout: format!("dot-agent-deck {}\n", self.local_version),
                        stderr: String::new(),
                    });
                }
                // Every reconnect attempt: host not reachable yet. An ssh
                // transport error maps to HostUnreachable in probe_remote_version.
                return Err(SshError::ConnectionRefused {
                    host: target.host.clone(),
                    port: target.port,
                    detail: "connection refused".to_string(),
                });
            }
            panic!("unexpected probe command: {command}");
        }
    }

    /// Fake spawner returning a scripted sequence of exit codes, counting
    /// spawns. Past the end of the script it repeats the last code, so "always
    /// 255" is a one-element script and an unexpected extra spawn surfaces as a
    /// failed count assertion rather than a panic.
    struct ScriptedSpawner {
        codes: Vec<i32>,
        calls: Cell<usize>,
    }

    impl ScriptedSpawner {
        fn new(codes: Vec<i32>) -> Self {
            assert!(!codes.is_empty(), "script needs at least one exit code");
            Self {
                codes,
                calls: Cell::new(0),
            }
        }
        fn spawn_count(&self) -> usize {
            self.calls.get()
        }
    }

    impl ConnectSpawner for ScriptedSpawner {
        fn spawn(&self, _target: &SshTarget, _install_path: &str) -> Result<i32, std::io::Error> {
            let n = self.calls.get();
            self.calls.set(n + 1);
            let code = self
                .codes
                .get(n)
                .copied()
                .unwrap_or_else(|| *self.codes.last().expect("non-empty"));
            Ok(code)
        }
    }

    /// Fake backoff that records how many times it was asked to sleep and never
    /// actually sleeps, so reconnect tests run instantly.
    struct RecordingBackoff {
        calls: Cell<usize>,
    }

    impl RecordingBackoff {
        fn new() -> Self {
            Self {
                calls: Cell::new(0),
            }
        }
        fn count(&self) -> usize {
            self.calls.get()
        }
    }

    impl ReconnectBackoff for RecordingBackoff {
        fn backoff(&self, _attempt: usize) {
            self.calls.set(self.calls.get() + 1);
        }
    }

    /// Fake `RemoteUpgrader` that records each `upgrade` call (name + version)
    /// and returns a scripted result. `fail` makes every call return `Err`, so
    /// the nudge's upgrade-failure fallback can be exercised without ssh.
    struct RecordingUpgrader {
        calls: std::cell::RefCell<Vec<(String, String)>>,
        fail: bool,
    }

    impl RecordingUpgrader {
        fn new() -> Self {
            Self {
                calls: std::cell::RefCell::new(Vec::new()),
                fail: false,
            }
        }
        fn failing() -> Self {
            Self {
                calls: std::cell::RefCell::new(Vec::new()),
                fail: true,
            }
        }
        fn calls(&self) -> Vec<(String, String)> {
            self.calls.borrow().clone()
        }
    }

    impl RemoteUpgrader for RecordingUpgrader {
        fn upgrade(&self, name: &str, version: &str) -> Result<(), String> {
            self.calls
                .borrow_mut()
                .push((name.to_string(), version.to_string()));
            if self.fail {
                Err("install failed: download 404".to_string())
            } else {
                Ok(())
            }
        }
    }

    /// Fake executor whose `--version` probe reports a *specific* remote
    /// version (independent of the laptop's) and whose `daemon hello` is always
    /// compatible. Lets a test drive the un-upgraded-remote case (remote older
    /// than the laptop) end-to-end through `run_connect`.
    struct VersionExecutor {
        remote_version: String,
    }

    impl VersionExecutor {
        fn new(remote_version: &str) -> Self {
            Self {
                remote_version: remote_version.to_string(),
            }
        }
    }

    impl SshExecutor for VersionExecutor {
        fn run(&self, _target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
            if command.contains("daemon hello") {
                let body = serde_json::to_string(&AttachResponse::hello(PROTOCOL_VERSION))
                    .expect("serialize hello");
                Ok(SshOutput {
                    status: 0,
                    stdout: body,
                    stderr: String::new(),
                })
            } else if command.contains("--version") {
                Ok(SshOutput {
                    status: 0,
                    stdout: format!("dot-agent-deck {}\n", self.remote_version),
                    stderr: String::new(),
                })
            } else {
                panic!("unexpected probe command: {command}");
            }
        }
    }

    /// Drive `run_connect` with the nudge disabled (non-TTY) and a no-op
    /// upgrader, so the reconnect/exit-code tests below stay focused on the
    /// PRD #148 state machine. PRD #161 M1.2 added the nudge seam; these tests
    /// don't exercise it (the probe reports the same version as the laptop, so
    /// `laptop_is_newer` is false regardless).
    fn run_connect_no_nudge(
        entry: &RemoteEntry,
        executor: &dyn SshExecutor,
        spawner: &dyn ConnectSpawner,
        backoff: &dyn ReconnectBackoff,
        path: &Path,
    ) -> Result<i32, RemoteConnectError> {
        let upgrader = RecordingUpgrader::new();
        let mut input: &[u8] = b"";
        let mut output: Vec<u8> = Vec::new();
        run_connect(
            entry,
            executor,
            spawner,
            backoff,
            &upgrader,
            path,
            TEST_LOCAL_VERSION,
            REMOTE_INSTALL_PATH,
            &mut input,
            &mut output,
            false,
        )
    }

    fn test_entry(name: &str) -> RemoteEntry {
        RemoteEntry {
            name: name.to_string(),
            kind: "ssh".to_string(),
            host: "viktor@host.example.com".to_string(),
            port: 22,
            key: None,
            version: TEST_LOCAL_VERSION.to_string(),
            added_at: "2026-06-12T00:00:00Z".to_string(),
            upgraded_at: None,
            last_connected: None,
        }
    }

    /// Write a registry containing `entry` to a fresh temp file. Returns the
    /// tempdir (kept alive by the caller so the file isn't removed) and the
    /// registry path.
    fn registry_with(entry: &RemoteEntry) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("remotes.toml");
        RemotesFile {
            remotes: vec![entry.clone()],
        }
        .save(&path)
        .expect("save registry");
        (dir, path)
    }

    #[test]
    fn reconnect_then_clean_exit_returns_zero() {
        // [255, 0] ⇒ exactly two spawns + one reconnect, returns 0. Backoff
        // fires once; the full probe re-runs on the reconnect; last_connected
        // is recorded once (on the final clean exit, not the 255 drop).
        let entry = test_entry("prod");
        let (_dir, path) = registry_with(&entry);
        let executor = ProbeOkExecutor::new(TEST_LOCAL_VERSION);
        let spawner = ScriptedSpawner::new(vec![255, 0]);
        let backoff = RecordingBackoff::new();

        let code = run_connect_no_nudge(&entry, &executor, &spawner, &backoff, &path)
            .expect("run_connect should succeed after one reconnect");

        assert_eq!(code, 0, "clean exit on the second attempt");
        assert_eq!(
            spawner.spawn_count(),
            2,
            "exactly two spawns: initial + one reconnect"
        );
        assert_eq!(
            backoff.count(),
            1,
            "backoff invoked once, for the single reconnect"
        );
        assert_eq!(
            executor.run_count(),
            4,
            "full probe (version + protocol) re-ran on each of the two attempts"
        );
        let reloaded = RemotesFile::load(&path).expect("reload registry");
        assert!(
            reloaded.remotes[0].last_connected.is_some(),
            "last_connected recorded on the clean exit"
        );
    }

    #[test]
    fn clean_exit_does_not_reconnect() {
        // [0] ⇒ one spawn, no reconnect, returns 0.
        let entry = test_entry("prod");
        let (_dir, path) = registry_with(&entry);
        let executor = ProbeOkExecutor::new(TEST_LOCAL_VERSION);
        let spawner = ScriptedSpawner::new(vec![0]);
        let backoff = RecordingBackoff::new();

        let code = run_connect_no_nudge(&entry, &executor, &spawner, &backoff, &path)
            .expect("run_connect");

        assert_eq!(code, 0);
        assert_eq!(spawner.spawn_count(), 1, "single spawn, no reconnect");
        assert_eq!(backoff.count(), 0, "no backoff on a clean exit");
    }

    #[test]
    fn ctrl_c_exit_is_terminal_no_reconnect() {
        // [130] (Ctrl-C / SIGINT) is a user-intent exit, not a transport
        // failure ⇒ one spawn, no reconnect, returned verbatim. Non-zero exit
        // ⇒ no last_connected bookkeeping.
        let entry = test_entry("prod");
        let (_dir, path) = registry_with(&entry);
        let executor = ProbeOkExecutor::new(TEST_LOCAL_VERSION);
        let spawner = ScriptedSpawner::new(vec![130]);
        let backoff = RecordingBackoff::new();

        let code = run_connect_no_nudge(&entry, &executor, &spawner, &backoff, &path)
            .expect("run_connect");

        assert_eq!(code, 130, "Ctrl-C exit returned verbatim");
        assert_eq!(spawner.spawn_count(), 1, "single spawn, no reconnect");
        assert_eq!(backoff.count(), 0, "no backoff on a terminal exit");
        let reloaded = RemotesFile::load(&path).expect("reload registry");
        assert!(
            reloaded.remotes[0].last_connected.is_none(),
            "no bookkeeping on a non-zero exit"
        );
    }

    #[test]
    fn repeated_transport_failure_is_bounded() {
        // Always 255 ⇒ exactly MAX_CONNECT_ATTEMPTS spawns, then give up with
        // the transport-failure exit code. Backoff fires only *between*
        // attempts (MAX_CONNECT_ATTEMPTS - 1 times). The 255 path never records
        // last_connected, so it stays None.
        let entry = test_entry("prod");
        let (_dir, path) = registry_with(&entry);
        let executor = ProbeOkExecutor::new(TEST_LOCAL_VERSION);
        let spawner = ScriptedSpawner::new(vec![255]);
        let backoff = RecordingBackoff::new();

        let code = run_connect_no_nudge(&entry, &executor, &spawner, &backoff, &path)
            .expect("run_connect returns Ok with the terminal-error code");

        assert_eq!(
            code, SSH_TRANSPORT_FAILURE_EXIT_CODE,
            "give-up returns the transport-failure exit code"
        );
        assert_eq!(
            spawner.spawn_count(),
            MAX_CONNECT_ATTEMPTS,
            "spawns are capped at the retry budget"
        );
        assert_eq!(
            backoff.count(),
            MAX_CONNECT_ATTEMPTS - 1,
            "backoff invoked between attempts only"
        );
        let reloaded = RemotesFile::load(&path).expect("reload registry");
        assert!(
            reloaded.remotes[0].last_connected.is_none(),
            "no bookkeeping on a transport-failure give-up"
        );
    }

    #[test]
    fn reconnect_time_unreachable_is_bounded_not_immediate() {
        // Regression for Blocker 1: a re-probe that keeps failing with the
        // reachability error after a first successful connect + a 255 drop must
        // be BOUNDED — counted against the budget, backed off, and given up on
        // with the terminal error code — NOT returned immediately on the first
        // re-probe failure.
        //
        // Trace with MAX_CONNECT_ATTEMPTS = 5:
        //   attempt 1: probe ok → spawn 255 (drop)            → backoff(1)
        //   attempts 2..=4: --version probe → HostUnreachable → backoff(2..4)
        //   attempt 5: --version probe → HostUnreachable      → give up → 255
        // So: exactly ONE spawn (the initial connect), MAX-1 backoffs, return
        // 255. The buggy code would back off once then return Err on attempt 2.
        let entry = test_entry("prod");
        let (_dir, path) = registry_with(&entry);
        let executor = UnreachableAfterFirstConnectExecutor::new(TEST_LOCAL_VERSION);
        let spawner = ScriptedSpawner::new(vec![255]);
        let backoff = RecordingBackoff::new();

        let code = run_connect_no_nudge(&entry, &executor, &spawner, &backoff, &path).expect(
            "run_connect folds reconnect-time unreachable into bounded retry, returns Ok(255)",
        );

        assert_eq!(
            code, SSH_TRANSPORT_FAILURE_EXIT_CODE,
            "give-up after exhausting reconnects returns the transport-failure code, \
             not an early HostUnreachable error"
        );
        assert_eq!(
            backoff.count(),
            MAX_CONNECT_ATTEMPTS - 1,
            "every reconnect attempt backed off — proves it did NOT return early \
             after the first re-probe failure (the bug would back off once)"
        );
        assert_eq!(
            spawner.spawn_count(),
            1,
            "only the initial connect spawned; reconnects fail at the probe before spawning"
        );
        // restore_local_terminal() runs on this give-up path: it is the first
        // line of `on_transport_failure`'s budget-exhausted branch, and the
        // only way to reach a returned 255 here is through that branch — so the
        // 255 return above proves restore ran (asserted by code inspection, not
        // a separate observable seam, to avoid threading a restore-injection
        // param through run_connect and every call site).
        let reloaded = RemotesFile::load(&path).expect("reload registry");
        assert!(
            reloaded.remotes[0].last_connected.is_none(),
            "no bookkeeping when reconnects exhaust on an unreachable host"
        );
    }

    #[test]
    fn retry_message_differentiates_drop_from_unreachable() {
        // Greptile PR #152 finding 1: the spawn-255 live-drop path and the
        // reconnect-time probe-unreachable path must read differently. Both
        // advertise the *next* attempt (attempt + 1) and share the em-dash +
        // ellipsis style; only the live-drop line says "connection … lost".
        let dropped = retry_message(TransportFailureKind::SessionDropped, "prod", 1);
        assert_eq!(
            dropped,
            format!("connection to 'prod' lost — reconnecting… (attempt 2/{MAX_CONNECT_ATTEMPTS})")
        );

        let unreachable = retry_message(TransportFailureKind::StillUnreachable, "prod", 2);
        assert_eq!(
            unreachable,
            format!("'prod' not reachable yet — retrying… (attempt 3/{MAX_CONNECT_ATTEMPTS})")
        );

        // The whole point of the fix: the two paths must NOT print the same
        // line, and the unreachable line must not claim a connection was lost.
        assert_ne!(
            retry_message(TransportFailureKind::SessionDropped, "prod", 1),
            retry_message(TransportFailureKind::StillUnreachable, "prod", 1),
        );
        assert!(
            !unreachable.contains("lost"),
            "the still-unreachable line must not say a connection was 'lost': {unreachable}"
        );
    }

    // ----- PRD #161 M4.3: connect version-comparison removal + upgrade nudge --

    #[test]
    fn unupgraded_older_remote_connects_normally() {
        // Success criterion (PRD #161): a laptop at 0.31.1 connecting to a
        // remote at 0.31.0 connects normally — no block, no error, no forced
        // upgrade. The deleted laptop↔remote comparison would have hard-failed
        // here; with it gone the older remote's matched TUI+daemon just connect.
        let entry = test_entry("prod");
        let (_dir, path) = registry_with(&entry);
        let executor = VersionExecutor::new("0.31.0");
        let spawner = ScriptedSpawner::new(vec![0]);
        let backoff = RecordingBackoff::new();
        let upgrader = RecordingUpgrader::new();
        let mut input: &[u8] = b"";
        let mut output: Vec<u8> = Vec::new();

        let code = run_connect(
            &entry,
            &executor,
            &spawner,
            &backoff,
            &upgrader,
            &path,
            "0.31.1", // laptop strictly newer than the 0.31.0 remote
            REMOTE_INSTALL_PATH,
            &mut input,
            &mut output,
            false, // non-TTY → nudge auto-skipped, connect as-is
        )
        .expect("un-upgraded older remote must connect without a block");

        assert_eq!(code, 0, "connects and exits cleanly");
        assert_eq!(spawner.spawn_count(), 1, "single spawn, no block before it");
        assert!(
            upgrader.calls().is_empty(),
            "no upgrade forced on a non-TTY connect to an older remote"
        );
    }

    #[test]
    fn nudge_is_newer_only() {
        // The nudge is offered only when the laptop is strictly newer than the
        // remote; an equal or older laptop must NOT prompt (never suggest a
        // downgrade).
        assert!(laptop_is_newer("0.31.1", "0.31.0"), "newer laptop -> nudge");
        assert!(!laptop_is_newer("0.31.0", "0.31.0"), "equal -> no nudge");
        assert!(
            !laptop_is_newer("0.31.0", "0.31.1"),
            "older laptop -> no nudge (never downgrade)"
        );
        // `v` prefix tolerated on either side.
        assert!(laptop_is_newer("v0.32.0", "0.31.9"));
        // Unparseable version is conservative: no nudge.
        assert!(!laptop_is_newer("garbage", "0.31.0"));

        // End-to-end through the prompt: an equal-version laptop on a TTY with
        // 'y' queued still writes NO prompt and runs no upgrade.
        let upgrader = RecordingUpgrader::new();
        let mut input: &[u8] = b"y\n";
        let mut output: Vec<u8> = Vec::new();
        maybe_nudge_upgrade(
            &upgrader,
            "prod",
            "0.31.0",
            "0.31.0",
            None,
            true,
            &mut input,
            &mut output,
        )
        .expect("nudge");
        assert!(output.is_empty(), "equal version writes no prompt");
        assert!(
            upgrader.calls().is_empty(),
            "equal version runs no upgrade even with 'y' on stdin"
        );
    }

    #[test]
    fn nudge_default_n_connects_without_upgrade() {
        // Newer laptop on a TTY: bare Enter (empty answer) defaults to No, so
        // no upgrade runs and connect proceeds against the existing version.
        // The agent count from the probe is surfaced in the prompt.
        let upgrader = RecordingUpgrader::new();
        let mut input: &[u8] = b"\n";
        let mut output: Vec<u8> = Vec::new();
        maybe_nudge_upgrade(
            &upgrader,
            "prod",
            "0.31.0",
            "0.31.1",
            Some(2),
            true,
            &mut input,
            &mut output,
        )
        .expect("nudge");
        let prompt = String::from_utf8(output).unwrap();
        assert!(
            prompt.contains("Upgrade and connect? [y/N]"),
            "prompt shown: {prompt}"
        );
        assert!(
            prompt.contains("2 running agents"),
            "agent count surfaced in the prompt: {prompt}"
        );
        assert!(
            upgrader.calls().is_empty(),
            "Enter defaults to No -> no upgrade"
        );

        // An explicit 'n' behaves the same.
        let upgrader = RecordingUpgrader::new();
        let mut input: &[u8] = b"n\n";
        let mut output: Vec<u8> = Vec::new();
        maybe_nudge_upgrade(
            &upgrader,
            "prod",
            "0.31.0",
            "0.31.1",
            None,
            true,
            &mut input,
            &mut output,
        )
        .expect("nudge");
        assert!(upgrader.calls().is_empty(), "'n' -> no upgrade");
    }

    #[test]
    fn nudge_yes_runs_upgrade_then_connects() {
        // 'y' on a newer-laptop TTY runs `remote upgrade` to the laptop's
        // version, then returns Ok so connect proceeds against the upgraded
        // remote.
        let upgrader = RecordingUpgrader::new();
        let mut input: &[u8] = b"y\n";
        let mut output: Vec<u8> = Vec::new();
        maybe_nudge_upgrade(
            &upgrader,
            "prod",
            "0.31.0",
            "0.31.1",
            None,
            true,
            &mut input,
            &mut output,
        )
        .expect("nudge");
        assert_eq!(
            upgrader.calls(),
            vec![("prod".to_string(), "0.31.1".to_string())],
            "y upgrades the remote to the laptop's version before connecting"
        );
    }

    #[test]
    fn nudge_upgrade_failure_falls_back_to_existing_version() {
        // PRD #161 D4 (never strand): a failed `remote upgrade` must NOT
        // propagate as an error — the nudge prints a clear fallback line and
        // connect proceeds against the EXISTING version.
        let upgrader = RecordingUpgrader::failing();
        let mut input: &[u8] = b"y\n";
        let mut output: Vec<u8> = Vec::new();
        maybe_nudge_upgrade(
            &upgrader,
            "prod",
            "0.31.0",
            "0.31.1",
            None,
            true,
            &mut input,
            &mut output,
        )
        .expect("upgrade failure must fall back, not error out");
        assert_eq!(upgrader.calls().len(), 1, "the upgrade was attempted");
        let msg = String::from_utf8(output).unwrap();
        assert!(msg.contains("failed"), "clear failure message: {msg}");
        assert!(
            msg.contains("existing 0.31.0 install"),
            "falls back to the existing remote version: {msg}"
        );
    }

    #[test]
    fn nudge_non_tty_auto_skips() {
        // Even with the laptop strictly newer and a 'y' queued on stdin, a
        // non-TTY stdin skips the prompt entirely and connects as-is.
        let upgrader = RecordingUpgrader::new();
        let mut input: &[u8] = b"y\n";
        let mut output: Vec<u8> = Vec::new();
        maybe_nudge_upgrade(
            &upgrader,
            "prod",
            "0.31.0",
            "0.31.1",
            Some(3),
            false,
            &mut input,
            &mut output,
        )
        .expect("nudge");
        assert!(output.is_empty(), "non-TTY writes no prompt");
        assert!(upgrader.calls().is_empty(), "non-TTY runs no upgrade");
    }

    #[test]
    fn run_connect_nudge_y_upgrades_then_connects_end_to_end() {
        // Full path through run_connect: newer laptop, TTY, 'y' → the binary
        // swap runs, then ssh -t spawns and the session exits cleanly. Proves
        // the y-path orchestrates `remote upgrade` then connects.
        let entry = test_entry("prod");
        let (_dir, path) = registry_with(&entry);
        let executor = VersionExecutor::new("0.31.0");
        let spawner = ScriptedSpawner::new(vec![0]);
        let backoff = RecordingBackoff::new();
        let upgrader = RecordingUpgrader::new();
        let mut input: &[u8] = b"y\n";
        let mut output: Vec<u8> = Vec::new();

        let code = run_connect(
            &entry,
            &executor,
            &spawner,
            &backoff,
            &upgrader,
            &path,
            "0.31.1",
            REMOTE_INSTALL_PATH,
            &mut input,
            &mut output,
            true,
        )
        .expect("connect after the y-upgrade");

        assert_eq!(code, 0, "connected and exited cleanly after the upgrade");
        assert_eq!(spawner.spawn_count(), 1, "connected once after the upgrade");
        assert_eq!(
            upgrader.calls(),
            vec![("prod".to_string(), "0.31.1".to_string())],
            "y ran `remote upgrade` to the laptop version before connecting"
        );
    }
}
