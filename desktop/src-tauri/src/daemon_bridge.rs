use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dot_agent_deck::daemon_attach::{
    DAEMON_START_POLL_TIMEOUT, ensure_daemon_running, spawn_daemon_serve_detached_with_exe,
};
use dot_agent_deck::daemon_client::{DaemonClient, issue_command};
#[cfg(test)]
use dot_agent_deck::daemon_protocol::RunningAgentsSummary;
use dot_agent_deck::daemon_protocol::{AttachRequest, AttachResponse, PROTOCOL_VERSION};
use dot_agent_deck::platform::ipc::IpcStream;

use crate::dto::{
    BootstrapOptions, ConnectionStatus, DesktopConnection, DesktopSnapshot, desktop_project_cwd,
    disconnected_snapshot, map_agent, safe_message, socket_path_text,
};

const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
struct HandshakeInfo {
    status: ConnectionStatus,
    error: Option<String>,
    server_protocol_version: Option<u32>,
    daemon_build_version: Option<String>,
    daemon_version: Option<String>,
    running_agent_count: Option<usize>,
}

pub(crate) struct TrustedDaemon {
    pub(crate) client: DaemonClient,
    connection: DesktopConnection,
}

impl TrustedDaemon {
    pub(crate) fn require_compatible(&self) -> Result<(), String> {
        if self.connection.status == ConnectionStatus::Connected {
            Ok(())
        } else {
            Err(self
                .connection
                .error
                .clone()
                .unwrap_or_else(|| "daemon is not protocol-compatible".into()))
        }
    }
}

/// Opt-in development escape hatch for the build-stamp check.
///
/// The handshake refuses a daemon whose git-describe stamp differs from the
/// desktop's, which is right for a shipped app: two builds can share a wire
/// format and still disagree about what a field means, and the desktop may not
/// recycle a daemon that owns live agents. In development the same rule makes
/// the app unusable against the daemon you already run — every commit restamps
/// the desktop, and a released daemon never matches a branch build at all.
///
/// This ONLY relaxes the stamp comparison. The `PROTOCOL_VERSION` check runs
/// first and is never bypassed, so an actually-incompatible wire is still
/// refused; and the mismatch stays visible in the connection message rather
/// than being swallowed.
fn build_mismatch_bypassed() -> bool {
    matches!(
        std::env::var(BUILD_MISMATCH_BYPASS_ENV).as_deref(),
        Ok("1") | Ok("true")
    )
}

/// Env var enabling [`build_mismatch_bypassed`].
const BUILD_MISMATCH_BYPASS_ENV: &str = "DOT_AGENT_DECK_DESKTOP_ALLOW_BUILD_MISMATCH";

fn classify_handshake(
    response: &AttachResponse,
    client_build: &str,
    allow_build_mismatch: bool,
) -> HandshakeInfo {
    let server_protocol_version = response.server_version;
    let daemon_build_version = response.build_version.clone();
    let daemon_version = response.daemon_version.clone();
    let running_agent_count = response
        .running_agents
        .as_ref()
        .map(|summary| summary.count);

    let mut build_mismatch_was_bypassed = false;
    let error = if !response.ok {
        Some(
            response
                .error
                .clone()
                .unwrap_or_else(|| "daemon rejected Hello".into()),
        )
    } else if server_protocol_version != Some(PROTOCOL_VERSION) {
        Some(format!(
            "protocol mismatch: desktop expects {PROTOCOL_VERSION}, daemon reports {}",
            server_protocol_version
                .map(|version| version.to_string())
                .unwrap_or_else(|| "no version".into())
        ))
    } else if daemon_build_version.as_deref() != Some(client_build) {
        let stamps = format!(
            "build mismatch: desktop is {client_build}, daemon is {}",
            daemon_build_version.as_deref().unwrap_or("unreported")
        );
        if allow_build_mismatch {
            // Reached only AFTER the protocol check above returned equal, so
            // the wire shape is already agreed; what differs is the git-describe
            // stamp. Kept in `error` (not dropped) so the caveat stays on screen
            // for the whole session rather than being silently forgotten.
            build_mismatch_was_bypassed = true;
            Some(format!(
                "{stamps}. Bypassed by {BUILD_MISMATCH_BYPASS_ENV}; protocol {PROTOCOL_VERSION} matched on both sides. Development only — a stamp difference can still mean divergent behaviour behind an identical wire."
            ))
        } else {
            let recovery = match running_agent_count {
                Some(0) => "No live agents are reported; use Replace daemon to start the matching bundled build.".into(),
                Some(count) => format!(
                    "The daemon reports {count} live agent{}; stop them individually before replacing the daemon.",
                    if count == 1 { "" } else { "s" }
                ),
                None => "The daemon could not report its live-agent count, so automatic replacement is disabled.".into(),
            };
            Some(format!("{stamps}. {recovery}"))
        }
    } else {
        None
    };

    HandshakeInfo {
        status: if error.is_some() && !build_mismatch_was_bypassed {
            ConnectionStatus::Incompatible
        } else {
            ConnectionStatus::Connected
        },
        error: error.map(safe_message),
        server_protocol_version,
        daemon_build_version,
        daemon_version,
        running_agent_count,
    }
}

fn connection_from_handshake(handshake: HandshakeInfo) -> DesktopConnection {
    DesktopConnection {
        status: handshake.status,
        socket_path: socket_path_text(),
        error: handshake.error,
        client_protocol_version: PROTOCOL_VERSION,
        server_protocol_version: handshake.server_protocol_version,
        client_build_version: dot_agent_deck::build_id::local_build_id(),
        daemon_build_version: handshake.daemon_build_version,
        daemon_version: handshake.daemon_version,
        running_agent_count: handshake.running_agent_count,
    }
}

async fn hello(socket_path: &Path) -> Result<HandshakeInfo, String> {
    let client_build = dot_agent_deck::build_id::local_build_id();
    let stream = IpcStream::connect(socket_path)
        .await
        .map_err(|error| safe_message(error.to_string()))?;
    let (mut reader, mut writer) = stream.into_split();
    let response = issue_command(
        &mut reader,
        &mut writer,
        &AttachRequest::Hello {
            client_version: PROTOCOL_VERSION,
            client_build_version: Some(client_build.clone()),
        },
    )
    .await
    .map_err(|error| safe_message(error.to_string()))?;
    Ok(classify_handshake(
        &response,
        &client_build,
        build_mismatch_bypassed(),
    ))
}

pub(crate) async fn trusted_daemon() -> Result<TrustedDaemon, String> {
    let socket_path = dot_agent_deck::config::attach_socket_path();
    dot_agent_deck::platform::fsperm::verify_endpoint_trusted(&socket_path).map_err(|reason| {
        safe_message(format!(
            "refusing to connect to daemon endpoint {}: {reason}",
            socket_path.to_string_lossy()
        ))
    })?;
    let connection = connection_from_handshake(hello(&socket_path).await?);
    Ok(TrustedDaemon {
        client: DaemonClient::new(socket_path),
        connection,
    })
}

pub(crate) async fn get_snapshot() -> DesktopSnapshot {
    let daemon = match trusted_daemon().await {
        Ok(daemon) => daemon,
        Err(error) => return disconnected_snapshot(error),
    };
    if daemon.connection.status != ConnectionStatus::Connected {
        return DesktopSnapshot {
            connection: daemon.connection,
            agents: Vec::new(),
            project_cwd: desktop_project_cwd(),
            protocol_version: PROTOCOL_VERSION,
            source: "daemon",
        };
    }

    match daemon.client.list_agents().await {
        Ok(records) => DesktopSnapshot {
            connection: daemon.connection,
            agents: records.into_iter().map(map_agent).collect(),
            project_cwd: desktop_project_cwd(),
            protocol_version: PROTOCOL_VERSION,
            source: "daemon",
        },
        Err(error) => disconnected_snapshot(error.to_string()),
    }
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        true
    }
}

fn daemon_binary_name() -> &'static str {
    if cfg!(windows) {
        "dot-agent-deck.exe"
    } else {
        "dot-agent-deck"
    }
}

fn resolve_daemon_executable() -> Result<PathBuf, String> {
    if let Some(raw) = std::env::var_os("DOT_AGENT_DECK_BINARY") {
        let path = PathBuf::from(raw);
        if is_executable_file(&path) {
            return Ok(path);
        }
        return Err(format!(
            "DOT_AGENT_DECK_BINARY is not an executable file: {}",
            path.to_string_lossy()
        ));
    }

    if let Ok(current_exe) = std::env::current_exe()
        && let Some(parent) = current_exe.parent()
    {
        let sibling = parent.join(daemon_binary_name());
        if sibling != current_exe && is_executable_file(&sibling) {
            return Ok(sibling);
        }

        for ancestor in parent.ancestors() {
            let candidate = ancestor.join(daemon_binary_name());
            if candidate != current_exe && is_executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }

    if let Some(path_env) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path_env) {
            let candidate = directory.join(daemon_binary_name());
            if is_executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }

    Err(
        "dot-agent-deck CLI was not found; build/install it, place it next to the desktop binary, or set DOT_AGENT_DECK_BINARY before launching the desktop app"
            .into(),
    )
}

pub(crate) async fn bootstrap(options: &BootstrapOptions) -> DesktopSnapshot {
    let current = get_snapshot().await;
    if current.connection.status != ConnectionStatus::Disconnected || !options.start_if_missing {
        return current;
    }

    let socket_path = dot_agent_deck::config::attach_socket_path();
    let state_dir = dot_agent_deck::config::state_dir();
    let state_dir_for_spawn = state_dir.clone();
    let start_result = ensure_daemon_running(
        &socket_path,
        &state_dir,
        move || {
            let executable = resolve_daemon_executable()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            spawn_daemon_serve_detached_with_exe(&state_dir_for_spawn, &executable).map(|_| ())
        },
        DAEMON_POLL_INTERVAL,
        DAEMON_START_POLL_TIMEOUT,
    )
    .await;

    match start_result {
        Ok(()) => get_snapshot().await,
        Err(error) => disconnected_snapshot(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_hello_is_connected() {
        let response = AttachResponse::hello(PROTOCOL_VERSION);
        let info = classify_handshake(&response, response.build_version.as_deref().unwrap(), false);
        assert_eq!(info.status, ConnectionStatus::Connected);
        assert!(info.error.is_none());
    }

    #[test]
    fn protocol_mismatch_is_visible_and_never_treated_as_disconnected() {
        let response = AttachResponse::hello(PROTOCOL_VERSION + 1);
        let info = classify_handshake(&response, response.build_version.as_deref().unwrap(), false);
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        assert!(info.error.unwrap().contains("protocol mismatch"));
    }

    #[test]
    fn zero_agent_build_mismatch_points_to_safe_replacement() {
        let response = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());
        let info = classify_handshake(&response, "desktop-other-build", false);
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        let error = info.error.unwrap();
        assert!(error.contains("build mismatch"));
        assert!(error.contains("use Replace daemon"));
    }

    #[test]
    fn live_agent_build_mismatch_blocks_replacement() {
        let response =
            AttachResponse::hello(PROTOCOL_VERSION).with_running_agents(RunningAgentsSummary {
                count: 2,
                names: vec!["coder".into(), "tester".into()],
            });
        let info = classify_handshake(&response, "desktop-other-build", false);
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        assert!(
            info.error
                .unwrap()
                .contains("stop them individually before replacing")
        );
    }

    /// A stamp difference is downgraded to a warning, not silence: the deck
    /// connects, but the connection message still names both builds so the
    /// caveat survives for the whole session.
    #[test]
    fn bypassed_build_mismatch_connects_and_keeps_the_warning_visible() {
        let response = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());
        let info = classify_handshake(&response, "desktop-other-build", true);
        assert_eq!(info.status, ConnectionStatus::Connected);
        let error = info.error.expect("bypass must not swallow the mismatch");
        assert!(error.contains("build mismatch"), "{error}");
        assert!(error.contains(BUILD_MISMATCH_BYPASS_ENV), "{error}");
    }

    /// The load-bearing one. The bypass exists to relax a *stamp* comparison
    /// once the wire is known to agree; it must never let an actually
    /// incompatible protocol through, because that is the check that keeps a
    /// newer client from misreading an older daemon's frames.
    #[test]
    fn bypass_never_rescues_a_protocol_mismatch() {
        let response = AttachResponse::hello(PROTOCOL_VERSION + 1);
        let info = classify_handshake(&response, "desktop-other-build", true);
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        assert!(info.error.unwrap().contains("protocol mismatch"));
    }

    /// A daemon that reports no build stamp at all is still a mismatch, and the
    /// bypass covers it the same way — otherwise the escape hatch would have a
    /// hole exactly where the least is known about the peer.
    #[test]
    fn bypass_covers_an_unreported_daemon_stamp() {
        let mut response = AttachResponse::hello(PROTOCOL_VERSION);
        response.build_version = None;
        let info = classify_handshake(&response, "desktop-build", true);
        assert_eq!(info.status, ConnectionStatus::Connected);
        assert!(info.error.unwrap().contains("unreported"));
    }

    /// Only `1` and `true` arm it. An unset variable, an empty string, or a
    /// stray value leaves the refusal in place: this is a switch someone turns
    /// on deliberately, not one they trip over.
    #[test]
    fn bypass_env_is_opt_in_and_ignores_stray_values() {
        for (value, expected) in [
            (None, false),
            (Some(""), false),
            (Some("0"), false),
            (Some("yes"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            // SAFETY: nextest runs each test in its own process, and this test
            // owns the variable for that process.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(BUILD_MISMATCH_BYPASS_ENV, value),
                    None => std::env::remove_var(BUILD_MISMATCH_BYPASS_ENV),
                }
            }
            assert_eq!(build_mismatch_bypassed(), expected, "value {value:?}");
        }
        unsafe { std::env::remove_var(BUILD_MISMATCH_BYPASS_ENV) };
    }
}
