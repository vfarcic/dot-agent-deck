use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// The protocol agreed and only the build stamp differed, so an override is
    /// legitimate. Never set when the wire itself is incompatible.
    build_stamp_mismatch_only: bool,
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

/// Whether the build-stamp comparison is being relaxed, and by what.
///
/// The handshake refuses a daemon whose git-describe stamp differs from the
/// desktop's, which is right by default: two builds can share a wire format and
/// still disagree about what a field means, and the desktop may not recycle a
/// daemon that owns live agents. But the refusal on its own left the user with
/// nowhere to go — a released daemon never matches a branch build, an installed
/// CLI and a downloaded `.app` update on different cadences, and **Replace
/// daemon** is deliberately disabled while agents are live (issue #801).
///
/// Relaxing it ONLY relaxes the stamp comparison. The `PROTOCOL_VERSION` check
/// runs first and is never bypassed, so an actually-incompatible wire is still
/// refused; and the mismatch stays visible in the connection message rather
/// than being swallowed, for whichever of the two switches turned it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildMismatchAllowance {
    /// The stamp check applies: a difference refuses the connection.
    Refuse,
    /// Relaxed for the whole process by [`BUILD_MISMATCH_BYPASS_ENV`].
    Env,
    /// Relaxed for this app session by an explicit in-app confirmation.
    Session,
}

impl BuildMismatchAllowance {
    fn allows(self) -> bool {
        !matches!(self, Self::Refuse)
    }
}

/// Env var arming [`BuildMismatchAllowance::Env`].
const BUILD_MISMATCH_BYPASS_ENV: &str = "DOT_AGENT_DECK_DESKTOP_ALLOW_BUILD_MISMATCH";

/// The in-app allowance, armed by `DesktopAction::AllowBuildMismatch`.
///
/// Deliberately a process-global rather than anything in `DesktopState`: it is
/// SESSION-scoped, so it lives exactly as long as the app process and is never
/// written anywhere. Quitting the app re-arms the refusal, and the user
/// re-affirms the next time they meet the same daemon.
static SESSION_BUILD_MISMATCH_ALLOWED: AtomicBool = AtomicBool::new(false);

/// Arms [`BuildMismatchAllowance::Session`] for the rest of this app session.
pub(crate) fn allow_build_mismatch_this_session() {
    set_session_build_mismatch_allowance(true);
}

fn set_session_build_mismatch_allowance(allowed: bool) {
    SESSION_BUILD_MISMATCH_ALLOWED.store(allowed, Ordering::Relaxed);
}

/// Only `1` and `true` arm the env switch — see the test below.
fn env_allows_build_mismatch() -> bool {
    matches!(
        std::env::var(BUILD_MISMATCH_BYPASS_ENV).as_deref(),
        Ok("1") | Ok("true")
    )
}

/// The two switches are independent and either one is enough. The env var is
/// read first only so that its long-standing wording is what a developer who
/// set it sees; the in-app allowance is unreachable while it is set, because
/// the app never refuses and so never offers the button.
fn build_mismatch_allowance() -> BuildMismatchAllowance {
    if env_allows_build_mismatch() {
        BuildMismatchAllowance::Env
    } else if SESSION_BUILD_MISMATCH_ALLOWED.load(Ordering::Relaxed) {
        BuildMismatchAllowance::Session
    } else {
        BuildMismatchAllowance::Refuse
    }
}

fn classify_handshake(
    response: &AttachResponse,
    client_build: &str,
    allowance: BuildMismatchAllowance,
) -> HandshakeInfo {
    let server_protocol_version = response.server_version;
    let daemon_build_version = response.build_version.clone();
    let daemon_version = response.daemon_version.clone();
    let running_agent_count = response
        .running_agents
        .as_ref()
        .map(|summary| summary.count);

    // Set ONLY inside the stamp branch, which the protocol check guards. A
    // rejected Hello and a protocol mismatch both return before it, so neither
    // can advertise an override that the bypass would refuse to honour anyway.
    let mut build_stamp_mismatch_only = false;
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
        // Reached only AFTER the protocol check above returned equal, so the
        // wire shape is already agreed and what differs is the git-describe
        // stamp alone. That is exactly the mismatch an override may relax.
        build_stamp_mismatch_only = true;
        let stamps = format!(
            "build mismatch: desktop is {client_build}, daemon is {}",
            daemon_build_version.as_deref().unwrap_or("unreported")
        );
        // Whichever switch is armed, the mismatch is kept in `error` (not
        // dropped) so the caveat stays on screen for the whole session rather
        // than being silently forgotten.
        build_mismatch_was_bypassed = allowance.allows();
        match allowance {
            BuildMismatchAllowance::Env => Some(format!(
                "{stamps}. Bypassed by {BUILD_MISMATCH_BYPASS_ENV}; protocol {PROTOCOL_VERSION} matched on both sides. Development only — a stamp difference can still mean divergent behaviour behind an identical wire."
            )),
            BuildMismatchAllowance::Session => Some(format!(
                "{stamps}. Connected anyway for this session; protocol {PROTOCOL_VERSION} matched on both sides. A stamp difference can still mean divergent behaviour behind an identical wire."
            )),
            BuildMismatchAllowance::Refuse => {
                let recovery = match running_agent_count {
                    Some(0) => "No live agents are reported; use Replace daemon to start the matching bundled build, or Connect anyway to keep this one.".into(),
                    Some(count) => format!(
                        "The daemon reports {count} live agent{}; stop them individually before replacing the daemon, or Connect anyway to keep this one.",
                        if count == 1 { "" } else { "s" }
                    ),
                    None => "The daemon could not report its live-agent count, so automatic replacement is disabled; Connect anyway keeps this one.".into(),
                };
                Some(format!("{stamps}. {recovery}"))
            }
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
        build_stamp_mismatch_only,
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
        build_stamp_mismatch_only: handshake.build_stamp_mismatch_only,
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
        build_mismatch_allowance(),
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
    use std::sync::{Mutex, MutexGuard};

    /// The env var and the session flag are both process-global, so the tests
    /// that write either one take this first. Under nextest each test owns its
    /// own process and the lock is free; under a plain `cargo test` the whole
    /// module shares one process and without it a test that sets the env var
    /// would be read by a test asserting it is unset.
    static ALLOWANCE_LOCK: Mutex<()> = Mutex::new(());

    /// Holds the lock and restores BOTH switches on drop, including on a panic,
    /// so one failing assertion cannot leave the rest of the module armed.
    struct AllowanceGuard(#[allow(dead_code)] MutexGuard<'static, ()>);

    impl AllowanceGuard {
        fn acquire() -> Self {
            let guard = ALLOWANCE_LOCK
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            clear_allowance_switches();
            Self(guard)
        }
    }

    impl Drop for AllowanceGuard {
        fn drop(&mut self) {
            clear_allowance_switches();
        }
    }

    fn clear_allowance_switches() {
        // SAFETY: every test that touches the variable holds `ALLOWANCE_LOCK`,
        // so no other thread in this process is reading the environment here.
        unsafe { std::env::remove_var(BUILD_MISMATCH_BYPASS_ENV) };
        set_session_build_mismatch_allowance(false);
    }

    fn set_bypass_env(value: Option<&str>) {
        // SAFETY: as above — guarded by `ALLOWANCE_LOCK`.
        unsafe {
            match value {
                Some(value) => std::env::set_var(BUILD_MISMATCH_BYPASS_ENV, value),
                None => std::env::remove_var(BUILD_MISMATCH_BYPASS_ENV),
            }
        }
    }

    #[test]
    fn matching_hello_is_connected() {
        let response = AttachResponse::hello(PROTOCOL_VERSION);
        let info = classify_handshake(
            &response,
            response.build_version.as_deref().unwrap(),
            BuildMismatchAllowance::Refuse,
        );
        assert_eq!(info.status, ConnectionStatus::Connected);
        assert!(info.error.is_none());
        assert!(!info.build_stamp_mismatch_only);
    }

    #[test]
    fn protocol_mismatch_is_visible_and_never_treated_as_disconnected() {
        let response = AttachResponse::hello(PROTOCOL_VERSION + 1);
        let info = classify_handshake(
            &response,
            response.build_version.as_deref().unwrap(),
            BuildMismatchAllowance::Refuse,
        );
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        assert!(info.error.unwrap().contains("protocol mismatch"));
    }

    #[test]
    fn zero_agent_build_mismatch_points_to_safe_replacement() {
        let response = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());
        let info = classify_handshake(
            &response,
            "desktop-other-build",
            BuildMismatchAllowance::Refuse,
        );
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
        let info = classify_handshake(
            &response,
            "desktop-other-build",
            BuildMismatchAllowance::Refuse,
        );
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        let error = info.error.unwrap();
        assert!(
            error.contains("stop them individually before replacing"),
            "{error}"
        );
        // Issue #801: replacement is refused while agents are live, and that is
        // correct — but it used to be the ONLY thing offered, which left a user
        // with nine running agents no way into the app at all.
        assert!(error.contains("Connect anyway"), "{error}");
    }

    /// A stamp difference is downgraded to a warning, not silence: the deck
    /// connects, but the connection message still names both builds so the
    /// caveat survives for the whole session.
    #[test]
    fn bypassed_build_mismatch_connects_and_keeps_the_warning_visible() {
        let response = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());
        let info = classify_handshake(
            &response,
            "desktop-other-build",
            BuildMismatchAllowance::Env,
        );
        assert_eq!(info.status, ConnectionStatus::Connected);
        let error = info.error.expect("bypass must not swallow the mismatch");
        assert!(error.contains("build mismatch"), "{error}");
        assert!(error.contains(BUILD_MISMATCH_BYPASS_ENV), "{error}");
    }

    /// The in-app override says so in its own words. Naming the env var here
    /// would tell a user who pressed a button to go looking for a shell
    /// variable they never set — and a `.app` launched from Finder could not
    /// have received one anyway (issue #801).
    #[test]
    fn session_override_connects_and_names_itself_rather_than_the_env_var() {
        let response =
            AttachResponse::hello(PROTOCOL_VERSION).with_running_agents(RunningAgentsSummary {
                count: 9,
                names: vec!["coder".into()],
            });
        let info = classify_handshake(
            &response,
            "desktop-other-build",
            BuildMismatchAllowance::Session,
        );
        assert_eq!(info.status, ConnectionStatus::Connected);
        let error = info
            .error
            .expect("the override must not swallow the mismatch");
        assert!(error.contains("build mismatch"), "{error}");
        assert!(
            error.contains("Connected anyway for this session"),
            "{error}"
        );
        assert!(
            error.contains("divergent behaviour behind an identical wire"),
            "{error}"
        );
        assert!(!error.contains(BUILD_MISMATCH_BYPASS_ENV), "{error}");
    }

    /// The load-bearing one. The bypass exists to relax a *stamp* comparison
    /// once the wire is known to agree; it must never let an actually
    /// incompatible protocol through, because that is the check that keeps a
    /// newer client from misreading an older daemon's frames.
    #[test]
    fn bypass_never_rescues_a_protocol_mismatch() {
        let response = AttachResponse::hello(PROTOCOL_VERSION + 1);
        for allowance in [BuildMismatchAllowance::Env, BuildMismatchAllowance::Session] {
            let info = classify_handshake(&response, "desktop-other-build", allowance);
            assert_eq!(info.status, ConnectionStatus::Incompatible, "{allowance:?}");
            assert!(
                info.error.unwrap().contains("protocol mismatch"),
                "{allowance:?}"
            );
        }
    }

    /// The other half of the same property, and the one the UI reads: a
    /// protocol mismatch must not ADVERTISE an override either. Offering
    /// Connect anyway there would put a button on screen that cannot work —
    /// pressing it re-runs the handshake and gets refused again — and would
    /// teach the user that the wire check is negotiable. It is not.
    #[test]
    fn protocol_mismatch_never_advertises_an_override() {
        let response = AttachResponse::hello(PROTOCOL_VERSION + 1);
        for allowance in [
            BuildMismatchAllowance::Refuse,
            BuildMismatchAllowance::Env,
            BuildMismatchAllowance::Session,
        ] {
            let info = classify_handshake(&response, "desktop-other-build", allowance);
            assert!(!info.build_stamp_mismatch_only, "{allowance:?}");
        }
    }

    /// A Hello the daemon itself rejected is not a stamp problem either, so it
    /// carries no override — the stamp branch is never even reached.
    #[test]
    fn rejected_hello_never_advertises_an_override() {
        let mut response = AttachResponse::hello(PROTOCOL_VERSION);
        response.ok = false;
        response.error = Some("daemon is shutting down".into());
        let info = classify_handshake(
            &response,
            response.build_version.as_deref().unwrap(),
            BuildMismatchAllowance::Refuse,
        );
        assert_eq!(info.status, ConnectionStatus::Incompatible);
        assert!(!info.build_stamp_mismatch_only);
    }

    /// What the UI switches on: the protocol agreed and only the stamp differs,
    /// so an override is legitimate. True whether or not one is already armed —
    /// the flag describes the mismatch, not the response to it.
    #[test]
    fn build_mismatch_advertises_a_stamp_only_override() {
        let response = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());
        for allowance in [
            BuildMismatchAllowance::Refuse,
            BuildMismatchAllowance::Env,
            BuildMismatchAllowance::Session,
        ] {
            let info = classify_handshake(&response, "desktop-other-build", allowance);
            assert!(info.build_stamp_mismatch_only, "{allowance:?}");
        }
    }

    /// A daemon that reports no build stamp at all is still a mismatch, and the
    /// bypass covers it the same way — otherwise the escape hatch would have a
    /// hole exactly where the least is known about the peer.
    #[test]
    fn bypass_covers_an_unreported_daemon_stamp() {
        let mut response = AttachResponse::hello(PROTOCOL_VERSION);
        response.build_version = None;
        let info = classify_handshake(&response, "desktop-build", BuildMismatchAllowance::Env);
        assert_eq!(info.status, ConnectionStatus::Connected);
        assert!(info.build_stamp_mismatch_only);
        assert!(info.error.unwrap().contains("unreported"));
    }

    /// Only `1` and `true` arm it. An unset variable, an empty string, or a
    /// stray value leaves the refusal in place: this is a switch someone turns
    /// on deliberately, not one they trip over.
    #[test]
    fn bypass_env_is_opt_in_and_ignores_stray_values() {
        let _guard = AllowanceGuard::acquire();
        for (value, expected) in [
            (None, false),
            (Some(""), false),
            (Some("0"), false),
            (Some("yes"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            set_bypass_env(value);
            assert_eq!(env_allows_build_mismatch(), expected, "value {value:?}");
        }
    }

    /// The runtime allowance is honoured on its own, with no environment at
    /// all — which is the entire point: a `.app` launched from Finder inherits
    /// no shell environment, so the env var was never a remedy a user could
    /// reach (issue #801).
    #[test]
    fn session_allowance_is_honoured_without_the_env_var() {
        let _guard = AllowanceGuard::acquire();
        assert_eq!(build_mismatch_allowance(), BuildMismatchAllowance::Refuse);

        allow_build_mismatch_this_session();

        assert_eq!(build_mismatch_allowance(), BuildMismatchAllowance::Session);
        let response = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());
        let info = classify_handshake(&response, "desktop-other-build", build_mismatch_allowance());
        assert_eq!(info.status, ConnectionStatus::Connected);
    }

    /// The two switches are independent: neither one clears or requires the
    /// other, and either alone is enough. A developer's env var keeps working
    /// exactly as before, and a user's in-app confirmation does not depend on
    /// it.
    #[test]
    fn env_and_session_switches_are_independent() {
        let _guard = AllowanceGuard::acquire();
        assert_eq!(build_mismatch_allowance(), BuildMismatchAllowance::Refuse);

        set_bypass_env(Some("1"));
        assert_eq!(build_mismatch_allowance(), BuildMismatchAllowance::Env);

        set_bypass_env(None);
        assert_eq!(build_mismatch_allowance(), BuildMismatchAllowance::Refuse);

        set_session_build_mismatch_allowance(true);
        assert_eq!(build_mismatch_allowance(), BuildMismatchAllowance::Session);

        // Both armed is still allowed, and still names one reason rather than
        // inventing a third.
        set_bypass_env(Some("true"));
        assert!(build_mismatch_allowance().allows());

        set_session_build_mismatch_allowance(false);
        assert_eq!(build_mismatch_allowance(), BuildMismatchAllowance::Env);
    }

    /// The session allowance survives into the NEXT handshake rather than the
    /// one that was refused: nothing caches a verdict, so classifying the same
    /// response again after arming it connects.
    #[test]
    fn the_next_handshake_after_arming_the_session_allowance_connects() {
        let _guard = AllowanceGuard::acquire();
        let response = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());

        let refused =
            classify_handshake(&response, "desktop-other-build", build_mismatch_allowance());
        assert_eq!(refused.status, ConnectionStatus::Incompatible);
        assert!(refused.build_stamp_mismatch_only);

        allow_build_mismatch_this_session();

        let retried =
            classify_handshake(&response, "desktop-other-build", build_mismatch_allowance());
        assert_eq!(retried.status, ConnectionStatus::Connected);
        assert!(
            retried
                .error
                .expect("the caveat must survive the override")
                .contains("build mismatch")
        );
    }
}
