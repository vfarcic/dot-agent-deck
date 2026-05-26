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

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Command;

use thiserror::Error;

use crate::build_id::local_build_id;
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
    /// The remote binary reports a different version than the laptop. M2.9
    /// surfaces this as a *warning* (the connect proceeds); the variant
    /// exists so callers and tests can pattern-match on the outcome of the
    /// probe without parsing log lines. Future PRDs may upgrade specific
    /// breaking-version pairs into hard failures.
    #[error("Remote '{name}' runs dot-agent-deck {remote}; laptop runs {local}.")]
    VersionMismatch {
        name: String,
        remote: String,
        local: String,
    },
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
    /// attach-protocol versions. Unlike `VersionMismatch` (which only warns),
    /// a protocol-version mismatch is fatal: the wire format isn't
    /// backward-compatible and live updates would silently fail. `remote`
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
    /// PRD #103 M1.4: the laptop and the remote have compatible
    /// `PROTOCOL_VERSION`s but different `DAD_BUILD_ID`s. Same-tag-different-commit
    /// (or dirty-tree) skew is the exact case that PROTOCOL_VERSION alone
    /// can't catch — semantically divergent handler code behind a stable
    /// wire shape. `remote` is `None` when the remote omits `build_version`
    /// entirely (pre-PRD-103 binary): treated identically to a real mismatch
    /// so the user upgrades the remote and the laptop gains the precision
    /// on the next probe.
    ///
    /// Recovery routes to `remote upgrade <name>` (NOT `daemon stop` — the
    /// local-daemon-stop command is only meaningful for the laptop-side
    /// stale-daemon case).
    #[error("Remote '{name}' was built as {remote_str}; laptop was built as {local}. Run `dot-agent-deck remote upgrade {name}` to re-install the remote at the current build.",
            remote_str = remote.as_deref().unwrap_or("<unknown>"))]
    BuildVersionMismatch {
        name: String,
        remote: Option<String>,
        local: String,
    },
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

/// Outcome of `probe_remote_version`. The orchestrator turns this into a
/// stderr warning (`Mismatch`) or proceeds silently (`Match`); the explicit
/// enum keeps the test surface small without leaking probe internals into
/// the caller.
#[derive(Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Remote version equals the laptop's (string equality after the same
    /// `dot-agent-deck X.Y.Z` parse the install pipeline uses).
    Match,
    /// Remote version doesn't match the laptop's. Carry both strings so the
    /// caller can render a single warning line; M2.9 does not block on a
    /// mismatch (per task constraints — tighter version policy is a future
    /// PRD).
    Mismatch { remote: String, local: String },
}

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

/// Run a one-shot version probe over ssh and classify the outcome.
///
/// Three failure modes are distinguished so the caller can give the user an
/// actionable message:
///
/// - **Host-unreachable** — ssh itself failed to connect (transport error,
///   surfaced by `SshError::ConnectionRefused` / `Io` / `Other`). Wrapped
///   into `HostUnreachable` with ssh's stderr verbatim.
/// - **Binary-missing** — ssh succeeded but the remote shell reported
///   "command not found" (exit 127) or stdout doesn't look like a version
///   line. Wrapped into `RemoteBinaryMissing` with a `remote upgrade` hint.
/// - **Version-mismatch** — both sides report a version, but they don't
///   match. Returned as `ProbeOutcome::Mismatch` (NOT an error) so the
///   orchestrator can warn-and-continue per the task constraints.
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
    local_version: &str,
) -> Result<ProbeOutcome, RemoteConnectError> {
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
                Some(remote_version) if remote_version == local_version => Ok(ProbeOutcome::Match),
                Some(remote_version) => Ok(ProbeOutcome::Mismatch {
                    remote: remote_version,
                    local: local_version.to_string(),
                }),
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
/// - PRD #103 M1.4: `server_version` matches but `build_version` differs (or
///   is missing on the remote) → [`RemoteConnectError::BuildVersionMismatch`]
///   whose user-facing message names both build_ids and points at
///   `dot-agent-deck remote upgrade <name>`. The local-side `daemon stop`
///   command is NOT suggested here — that recovery applies only to the
///   laptop's own stale daemon, not to a remote.
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
) -> Result<(), RemoteConnectError> {
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
                    // PRD #103 M1.4: PROTOCOL_VERSION matches — now check
                    // the finer-grained `build_version` (which catches
                    // same-tag-different-commit skew the protocol version
                    // can't). A `None` here means a pre-PRD-103 remote
                    // binary that doesn't emit `build_version`; treat it
                    // identically to a real mismatch so the user re-runs
                    // `remote upgrade <name>` and the laptop gains the
                    // precision on the next probe.
                    // Route through `local_build_id()` rather than
                    // `env!("DAD_BUILD_ID")` so the
                    // `DOT_AGENT_DECK_BUILD_ID_OVERRIDE` test hook (used by
                    // the M4.2 integration tests) takes effect on the
                    // remote-mismatch path too.
                    let local_build = local_build_id();
                    match resp.build_version.as_deref() {
                        Some(remote_build) if remote_build == local_build.as_str() => Ok(()),
                        Some(remote_build) => Err(RemoteConnectError::BuildVersionMismatch {
                            name: name.to_string(),
                            remote: Some(remote_build.to_string()),
                            local: local_build,
                        }),
                        None => Err(RemoteConnectError::BuildVersionMismatch {
                            name: name.to_string(),
                            remote: None,
                            local: local_build,
                        }),
                    }
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
    // ConnectTimeout (separate from session-runtime — once the session is up,
    // ssh keeps it alive indefinitely). Aligned with the version probe so the
    // two stages have the same fail-fast budget.
    cmd.arg("-o")
        .arg(format!("ConnectTimeout={}", probe_timeout_secs()));
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

/// Orchestrate one connect: probe → spawn `ssh -t` → record `last_connected`.
///
/// Returns ssh's exit code so the caller can propagate it as the laptop
/// process's exit code (mirroring `ssh`'s own behavior — a remote shell
/// that exits 17 makes `ssh` exit 17, and we keep that contract one level
/// up). `last_connected` is only updated on a clean exit (status 0):
///
/// - User exits the TUI cleanly → laptop registry records the session.
/// - User Ctrl-C's the ssh, daemon dies, network drops mid-session → no
///   bookkeeping update; the registry only ever shows sessions that
///   actually ran to completion.
///
/// `_cli_theme` and `_continue_session` are accepted to preserve the
/// existing call site (M2.7-era stub) but are *intentionally ignored* in
/// M2.9. The remote TUI runs on the remote, so a laptop-side `--theme` /
/// `--continue` would have no effect on what the user sees. Piping them
/// through as remote flags is a future ergonomic improvement (deferred to
/// M5.5 when the connect UX is documented end-to-end).
pub fn run_connect(
    entry: &RemoteEntry,
    executor: &dyn SshExecutor,
    spawner: &dyn ConnectSpawner,
    remotes_path: &Path,
    local_version: &str,
    install_path: &str,
) -> Result<i32, RemoteConnectError> {
    let target = entry.ssh_target();

    // Stage 1: binary-version probe. Errors short-circuit; warnings (mismatch)
    // get surfaced on stderr and we proceed.
    match probe_remote_version(executor, &target, &entry.name, install_path, local_version)? {
        ProbeOutcome::Match => {}
        ProbeOutcome::Mismatch { remote, local } => {
            eprintln!(
                "warning: remote '{}' runs dot-agent-deck {}; laptop runs {}. Run `dot-agent-deck remote upgrade {}` to align.",
                entry.name, remote, local, entry.name
            );
        }
    }

    // Stage 1b: protocol-version handshake. A binary-version match doesn't
    // guarantee wire-format compatibility (the protocol is bumped
    // independently of the binary semver, see PRD #76 M2.21), and a
    // binary-version mismatch can still be safe if the protocol versions
    // agree. Fail the connect with a clear error on any protocol disagreement
    // — the wire format isn't backward-compatible and the silent failure
    // mode (frozen dashboard, no error) is worse than a hard-stop here.
    probe_remote_protocol(executor, &target, &entry.name, install_path)?;

    // Stage 2: hand the terminal over. This blocks until the user exits.
    let exit_code =
        spawner
            .spawn(&target, install_path)
            .map_err(|source| RemoteConnectError::SpawnFailed {
                name: entry.name.clone(),
                source,
            })?;

    // Stage 3: bookkeeping. Only on a clean exit — see doc comment.
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

    Ok(exit_code)
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
    run_connect(
        entry,
        &executor,
        &spawner,
        remotes_path,
        local_version,
        REMOTE_INSTALL_PATH,
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
}
