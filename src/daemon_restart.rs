//! PRD #1487: the daemon side of [`AttachRequest::RestartDaemon`] — the pieces
//! the handler arm in `daemon_protocol.rs` composes, kept here so each one is
//! unit-testable without a socket.
//!
//! - [`InstallRecord`] is what the daemon remembers about its own binary,
//!   captured once at `daemon serve` start.
//! - [`resolve_restart_target`] turns that into "the binary now installed at my
//!   own path" (PRD D2). The client never sends a path.
//! - [`verify_restart_target`] checks that binary answers `--version` with the
//!   expected version before anything is stopped.
//! - [`stop_set`] and [`restart_decision`] are the policy (PRD D6): an idle
//!   daemon restarts without asking; a live one names everything at stake and
//!   waits for that exact set to come back as `confirm`.
//! - [`RestartControl`] serialises requests and carries the accepted target
//!   to `run_daemon_with`, which spawns it once the sockets are released.
//!
//! It also holds the two JSON shapes the remote plumbing subcommands print
//! (`daemon probe --json`, `daemon restart-installed --json`), so the laptop
//! side parses exactly what the remote side wrote.
//!
//! [`AttachRequest::RestartDaemon`]: crate::daemon_protocol::AttachRequest::RestartDaemon

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::agent_pty::AgentRecord;
use crate::daemon_protocol::{
    AttachResponse, RestartAgent, RestartDaemonReply, RestartRefusalReason, RestartStopSet,
};
use crate::state::OrchestrationRoleRecord;

/// How long `<target> --version` may take before the daemon gives up on it
/// and refuses the restart. Generous, because a cold first exec of a freshly
/// downloaded binary can be slow on a loaded host, and a refusal here costs
/// nothing but a retry.
pub const RESTART_VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// The most of `<target> --version`'s stdout the daemon keeps. One short line
/// is the honest answer; this bounds a binary that never stops printing.
const VERSION_OUTPUT_CAP: u64 = 8 * 1024;

/// What the daemon recorded about its own binary when it started.
///
/// Captured once, at `daemon serve` start, because after an upgrade replaced
/// the file `current_exe()` may report a path with a ` (deleted)` suffix or a
/// Homebrew keg that no longer exists — the startup path is the stable fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRecord {
    pub startup_exe: PathBuf,
}

impl InstallRecord {
    /// This process's own executable, as `daemon serve` sees it at startup.
    /// An unreadable `current_exe()` yields an empty path, which
    /// [`resolve_restart_target`] answers with `TargetUnresolvable`.
    pub fn capture() -> Self {
        Self {
            startup_exe: std::env::current_exe().unwrap_or_default(),
        }
    }

    /// A record that resolves to nothing — the default for an in-process or
    /// test daemon, which must never spawn its own test harness as a
    /// "successor".
    pub fn unresolved() -> Self {
        Self {
            startup_exe: PathBuf::new(),
        }
    }
}

/// "The binary now installed at my own path" (PRD D2), from the path the
/// daemon started from. Pure.
///
/// 1. A trailing ` (deleted)` is stripped — Linux `/proc/self/exe` reports it
///    once an atomic `mv` replaced the inode or `brew cleanup` removed the keg.
/// 2. `<prefix>/Cellar/<formula>/<ver>/bin/<name>` becomes
///    `<prefix>/bin/<name>`, the link `brew upgrade` repoints (Linux resolves
///    symlinks in `current_exe`; on macOS it is already the link path).
/// 3. Otherwise the stripped path itself.
/// 4. A non-absolute or empty path is `TargetUnresolvable`.
pub fn resolve_restart_target(startup_exe: &Path) -> Result<PathBuf, RestartRefusalReason> {
    let raw = startup_exe.to_string_lossy();
    let stripped = raw.strip_suffix(" (deleted)").unwrap_or(&raw);
    if stripped.is_empty() {
        return Err(RestartRefusalReason::TargetUnresolvable);
    }
    let path = PathBuf::from(stripped);
    if !path.is_absolute() {
        return Err(RestartRefusalReason::TargetUnresolvable);
    }
    if let Some(linked) = homebrew_link_for(&path) {
        return Ok(linked);
    }
    Ok(path)
}

/// `<prefix>/Cellar/<formula>/<ver>/bin/<name>` → `<prefix>/bin/<name>`.
fn homebrew_link_for(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    let bin = path.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    let version_dir = bin.parent()?;
    let formula_dir = version_dir.parent()?;
    let cellar = formula_dir.parent()?;
    if cellar.file_name()? != "Cellar" {
        return None;
    }
    let prefix = cellar.parent()?;
    Some(prefix.join("bin").join(name))
}

/// Check the restart target before anything is stopped: a regular file with an
/// exec bit, whose `--version` exits 0 within `timeout` and prints
/// `dot-agent-deck X.Y.Z` — matching `expected` when one is given (a leading
/// `v` on either side is ignored). Returns the reported version.
///
/// Catches a wrong-architecture build, a half-written file, and a brew upgrade
/// that unlinked the old binary without linking the new one. Blocking — the
/// handler runs it in `spawn_blocking`.
pub fn verify_restart_target(
    target: &Path,
    expected: Option<&str>,
    timeout: Duration,
) -> Result<String, (RestartRefusalReason, String)> {
    let shown = target.display();
    let meta = std::fs::metadata(target).map_err(|e| {
        (
            RestartRefusalReason::TargetMissing,
            format!("the installed build at {shown} is not there ({e})"),
        )
    })?;
    if !meta.is_file() {
        return Err((
            RestartRefusalReason::TargetMissing,
            format!("the installed build at {shown} is not a regular file"),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err((
                RestartRefusalReason::TargetMissing,
                format!("the installed build at {shown} is not executable"),
            ));
        }
    }

    let did_not_answer = |why: String| {
        (
            RestartRefusalReason::TargetDidNotAnswer,
            format!("the installed build at {shown} did not answer `--version`: {why}"),
        )
    };
    let stdout = run_version_bounded(target, timeout).map_err(did_not_answer)?;
    let version = crate::version::parse_version_output(&stdout).ok_or_else(|| {
        did_not_answer(format!(
            "it printed {:?}, which is not `dot-agent-deck X.Y.Z`",
            stdout.trim()
        ))
    })?;
    if let Some(expected) = expected
        && strip_v(expected) != strip_v(&version)
    {
        return Err((
            RestartRefusalReason::VersionMismatch,
            format!(
                "the installed build at {shown} reports {version}, not the expected {expected}"
            ),
        ));
    }
    Ok(version)
}

fn strip_v(v: &str) -> &str {
    v.strip_prefix('v').unwrap_or(v)
}

/// Run `<target> --version` with a wall-clock bound and a byte cap on stdout.
fn run_version_bounded(target: &Path, timeout: Duration) -> Result<String, String> {
    use std::process::{Command, Stdio};
    let deadline = Instant::now() + timeout;
    // A file written moments ago can still be held open for writing by a
    // concurrent fork elsewhere in this process (ETXTBSY); that clears in
    // milliseconds, so retry briefly rather than refuse a restart over it.
    let mut child = loop {
        match Command::new(target)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => break child,
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("it could not be started ({e})")),
        }
    };
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(out) = stdout {
            let _ = out.take(VERSION_OUTPUT_CAP).read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("it did not exit within {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("waiting for it failed ({e})")),
        }
    };
    if !status.success() {
        return Err(format!("it exited with {status}"));
    }
    // A grandchild holding the pipe open must not hang the daemon: wait for the
    // reader only until the same deadline (plus a moment for the final read).
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(200));
    let buf = rx
        .recv_timeout(remaining)
        .map_err(|_| "its output did not close".to_string())?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// What a restart would stop, from the same two sources the `StopDaemon` and
/// `ListAgents` arms read (`AppState::live_orchestration_roles` and
/// `AgentPtyRegistry::agent_records`).
pub fn stop_set(roles: &[OrchestrationRoleRecord], agents: &[AgentRecord]) -> RestartStopSet {
    RestartStopSet {
        agents: agents
            .iter()
            .map(|r| RestartAgent {
                id: r.id.clone(),
                label: r.display_name.clone().unwrap_or_else(|| r.id.clone()),
                pane_id: r.pane_id_env.clone(),
                cwd: r.cwd.clone(),
            })
            .collect(),
        roles: roles.to_vec(),
    }
}

/// The D6 policy, pure. `None` means "go ahead"; `Some` is the
/// [`RestartDaemonReply::NeedsConfirmation`] to answer with.
///
/// - Nothing at stake: go ahead, whatever `confirm` says — there is no question
///   to answer, which is what lets an idle daemon restart with no TTY.
/// - Something at stake and no `confirm`: ask (`stale = false`).
/// - `confirm` names the same targets: go ahead.
/// - `confirm` names different targets: ask again with the current set
///   (`stale = true`) — the user agreed to stop something else.
pub fn restart_decision(
    at_stake: &RestartStopSet,
    confirm: Option<&RestartStopSet>,
) -> Option<RestartDaemonReply> {
    if at_stake.is_empty() {
        return None;
    }
    match confirm {
        Some(confirmed) if confirmed.same_targets(at_stake) => None,
        Some(_) => Some(RestartDaemonReply::NeedsConfirmation {
            at_stake: at_stake.clone(),
            stale: true,
        }),
        None => Some(RestartDaemonReply::NeedsConfirmation {
            at_stake: at_stake.clone(),
            stale: false,
        }),
    }
}

/// Per-daemon restart state, shared by every connection.
///
/// - `lock` serialises handlers: it is held from the first check to the
///   moment the request is accepted, so a second client gets an immediate
///   `InProgress` rather than a queue.
/// - `accepted` latches: once one request is accepted, every later one is
///   `InProgress`.
/// - `successor` is the verified target, consumed by `run_daemon_with` after
///   the sockets are released.
#[derive(Debug)]
pub struct RestartControl {
    lock: tokio::sync::Mutex<()>,
    accepted: AtomicBool,
    successor: StdMutex<Option<PathBuf>>,
    install: InstallRecord,
}

impl RestartControl {
    pub fn new(install: InstallRecord) -> Self {
        Self {
            lock: tokio::sync::Mutex::new(()),
            accepted: AtomicBool::new(false),
            successor: StdMutex::new(None),
            install,
        }
    }

    /// What the daemon recorded about its own binary at startup.
    pub fn install(&self) -> &InstallRecord {
        &self.install
    }

    /// Take the handler lock, or `None` when another restart holds it or one
    /// was already accepted.
    pub fn try_begin(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        if self.accepted.load(Ordering::SeqCst) {
            return None;
        }
        let guard = self.lock.try_lock().ok()?;
        // Re-check under the lock: a request accepted between the load above
        // and the lock must still win.
        if self.accepted.load(Ordering::SeqCst) {
            return None;
        }
        Some(guard)
    }

    /// Latch the acceptance and record the successor to spawn (`None` in
    /// `ClientSpawns` mode, where the client starts its own build).
    pub fn mark_accepted(&self, successor: Option<PathBuf>) {
        *self.successor.lock().unwrap_or_else(|p| p.into_inner()) = successor;
        self.accepted.store(true, Ordering::SeqCst);
    }

    /// Whether a restart has been accepted.
    pub fn is_accepted(&self) -> bool {
        self.accepted.load(Ordering::SeqCst)
    }

    /// The accepted successor, once. `run_daemon_with` calls this after its
    /// serve loop has returned and its sockets are released.
    pub fn take_successor(&self) -> Option<PathBuf> {
        self.successor
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }
}

impl Default for RestartControl {
    fn default() -> Self {
        Self::new(InstallRecord::unresolved())
    }
}

/// What `dot-agent-deck daemon probe --json` prints: whether a daemon is
/// running at this host's endpoint, and its `Hello` reply when one is. Never
/// lazy-spawns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonProbe {
    pub running: bool,
    #[serde(default)]
    pub hello: Option<AttachResponse>,
}

/// What `dot-agent-deck daemon restart-installed --json` prints.
///
/// - `running = false`: no daemon at this host's endpoint; nothing to restart.
/// - `unsupported = true`: the running daemon does not advertise
///   `restart-daemon`, so nothing was sent.
/// - otherwise `reply` is the daemon's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRestartReport {
    pub running: bool,
    #[serde(default)]
    pub reply: Option<RestartDaemonReply>,
    #[serde(default)]
    pub unsupported: bool,
}

/// Hex-encode a [`RestartStopSet`]'s JSON for `restart-installed
/// --confirm-hex`. Hex keeps the argument free of shell metacharacters,
/// because the ssh route takes one command string and no stdin.
pub fn encode_stop_set_hex(set: &RestartStopSet) -> String {
    let json = serde_json::to_vec(set).unwrap_or_default();
    let mut out = String::with_capacity(json.len() * 2);
    for b in json {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The inverse of [`encode_stop_set_hex`].
pub fn decode_stop_set_hex(hex: &str) -> Result<RestartStopSet, String> {
    let hex = hex.trim();
    if !hex.len().is_multiple_of(2) {
        return Err("odd number of hex digits".into());
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| {
            hex.get(i..i + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| format!("not a hex digit pair at offset {i}"))
        })
        .collect::<Result<Vec<u8>, String>>()?;
    serde_json::from_slice(&bytes).map_err(|e| format!("not a stop set: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_protocol::{
        AttachRequest, CAP_RESTART_DAEMON, DAEMON_CAPABILITIES, RestartSuccessor,
    };

    fn agent(id: &str) -> RestartAgent {
        RestartAgent {
            id: id.into(),
            label: format!("label-{id}"),
            pane_id: Some(format!("p-{id}")),
            cwd: Some("/w".into()),
        }
    }

    fn role(pane: &str, role: &str, orch: &str) -> OrchestrationRoleRecord {
        OrchestrationRoleRecord {
            pane_id: pane.into(),
            role: role.into(),
            orchestration: orch.into(),
            is_orchestrator: role == "orchestrator",
        }
    }

    fn set(agents: &[&str], roles: &[(&str, &str, &str)]) -> RestartStopSet {
        RestartStopSet {
            agents: agents.iter().map(|a| agent(a)).collect(),
            roles: roles.iter().map(|(p, r, o)| role(p, r, o)).collect(),
        }
    }

    // ---- wire ----

    #[test]
    fn restart_request_round_trips_and_defaults_its_fields() {
        let req = AttachRequest::RestartDaemon {
            confirm: Some(set(&["a1"], &[("p1", "coder", "o")])),
            expected_version: Some("0.46.0".into()),
            successor: RestartSuccessor::ClientSpawns,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["op"], "restart-daemon");
        assert_eq!(json["successor"], "client-spawns");
        match serde_json::from_value::<AttachRequest>(json).unwrap() {
            AttachRequest::RestartDaemon {
                confirm,
                expected_version,
                successor,
            } => {
                assert_eq!(confirm, Some(set(&["a1"], &[("p1", "coder", "o")])));
                assert_eq!(expected_version.as_deref(), Some("0.46.0"));
                assert_eq!(successor, RestartSuccessor::ClientSpawns);
            }
            other => panic!("decoded as {other:?}"),
        }

        // A bare frame: every field optional, successor defaults to Installed.
        let bare: AttachRequest =
            serde_json::from_str(r#"{"op":"restart-daemon"}"#).expect("bare frame decodes");
        assert!(matches!(
            bare,
            AttachRequest::RestartDaemon {
                confirm: None,
                expected_version: None,
                successor: RestartSuccessor::Installed,
            }
        ));
    }

    #[test]
    fn restart_replies_round_trip_with_their_outcome_tags() {
        let replies = [
            RestartDaemonReply::Accepted {
                from_version: "0.45.0".into(),
                to_version: Some("0.46.0".into()),
                successor: RestartSuccessor::Installed,
                stopping: set(&["a1"], &[]),
            },
            RestartDaemonReply::NeedsConfirmation {
                at_stake: set(&["a1", "a2"], &[("p1", "orchestrator", "o")]),
                stale: true,
            },
            RestartDaemonReply::Refused {
                reason: RestartRefusalReason::VersionMismatch,
                message: "m".into(),
            },
        ];
        let tags = ["accepted", "needs-confirmation", "refused"];
        for (reply, tag) in replies.into_iter().zip(tags) {
            let mut resp = AttachResponse::ok();
            resp.restart = Some(reply.clone());
            let json = serde_json::to_value(&resp).unwrap();
            assert_eq!(json["restart"]["outcome"], tag);
            let back: AttachResponse = serde_json::from_value(json).unwrap();
            assert_eq!(back.restart, Some(reply));
        }
        // An unrelated response omits the field entirely.
        let json = serde_json::to_value(AttachResponse::ok()).unwrap();
        assert!(json.get("restart").is_none());
    }

    #[test]
    fn an_unknown_refusal_reason_decodes_as_unknown() {
        let reply: RestartDaemonReply = serde_json::from_str(
            r#"{"outcome":"refused","reason":"some-future-reason","message":"x"}"#,
        )
        .unwrap();
        assert_eq!(
            reply,
            RestartDaemonReply::Refused {
                reason: RestartRefusalReason::Unknown,
                message: "x".into()
            }
        );
    }

    #[test]
    fn the_restart_capability_is_advertised() {
        assert_eq!(CAP_RESTART_DAEMON, "restart-daemon");
        assert!(DAEMON_CAPABILITIES.contains(&CAP_RESTART_DAEMON));
        let hello =
            AttachResponse::hello(crate::daemon_protocol::PROTOCOL_VERSION).with_capabilities();
        assert!(
            hello
                .capabilities
                .unwrap()
                .iter()
                .any(|c| c == CAP_RESTART_DAEMON)
        );
    }

    #[test]
    fn confirm_hex_round_trips_and_rejects_garbage() {
        let s = set(&["a1"], &[("p1", "coder", "o; rm -rf /")]);
        let hex = encode_stop_set_hex(&s);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(decode_stop_set_hex(&hex).unwrap(), s);
        assert!(decode_stop_set_hex("abc").is_err());
        assert!(decode_stop_set_hex("zz").is_err());
        assert!(decode_stop_set_hex("7b").is_err());
    }

    // ---- resolve_restart_target ----

    #[cfg(unix)]
    #[test]
    fn resolve_strips_the_deleted_suffix() {
        assert_eq!(
            resolve_restart_target(Path::new("/home/u/.local/bin/dot-agent-deck (deleted)")),
            Ok(PathBuf::from("/home/u/.local/bin/dot-agent-deck"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_maps_a_homebrew_keg_to_the_prefix_link() {
        assert_eq!(
            resolve_restart_target(Path::new(
                "/home/linuxbrew/.linuxbrew/Cellar/dot-agent-deck/0.45.0/bin/dot-agent-deck"
            )),
            Ok(PathBuf::from(
                "/home/linuxbrew/.linuxbrew/bin/dot-agent-deck"
            ))
        );
        // …including a keg `brew cleanup` already removed.
        assert_eq!(
            resolve_restart_target(Path::new(
                "/opt/homebrew/Cellar/dot-agent-deck/0.45.0/bin/dot-agent-deck (deleted)"
            )),
            Ok(PathBuf::from("/opt/homebrew/bin/dot-agent-deck"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_keeps_a_plain_path() {
        for p in [
            "/home/u/.local/bin/dot-agent-deck",
            "/opt/homebrew/bin/dot-agent-deck",
            "/w/target/debug/dot-agent-deck",
            // A `bin` under something that is not a Cellar keg stays as it is.
            "/x/notcellar/f/1.0/bin/dot-agent-deck",
        ] {
            assert_eq!(resolve_restart_target(Path::new(p)), Ok(PathBuf::from(p)));
        }
    }

    #[test]
    fn resolve_refuses_a_relative_or_empty_path() {
        for p in ["", "dot-agent-deck", "bin/dot-agent-deck", " (deleted)"] {
            assert_eq!(
                resolve_restart_target(Path::new(p)),
                Err(RestartRefusalReason::TargetUnresolvable),
                "{p:?}"
            );
        }
        assert_eq!(
            resolve_restart_target(&InstallRecord::unresolved().startup_exe),
            Err(RestartRefusalReason::TargetUnresolvable)
        );
    }

    // ---- verify_restart_target ----

    #[cfg(unix)]
    fn script(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "#!/bin/sh\n{body}").unwrap();
            f.sync_all().unwrap();
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn verify_refuses_a_missing_target() {
        let dir = tempfile::tempdir().unwrap();
        let err = verify_restart_target(&dir.path().join("absent"), None, RESTART_VERIFY_TIMEOUT)
            .unwrap_err();
        assert_eq!(err.0, RestartRefusalReason::TargetMissing);
        // A directory is not a regular file either.
        let err = verify_restart_target(dir.path(), None, RESTART_VERIFY_TIMEOUT).unwrap_err();
        assert_eq!(err.0, RestartRefusalReason::TargetMissing);
    }

    #[cfg(unix)]
    #[test]
    fn verify_refuses_a_non_executable_target() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "dad", "echo 'dot-agent-deck 0.46.0'", 0o644);
        let err = verify_restart_target(&p, None, RESTART_VERIFY_TIMEOUT).unwrap_err();
        assert_eq!(err.0, RestartRefusalReason::TargetMissing);
        assert!(err.1.contains("not executable"), "{}", err.1);
    }

    #[cfg(unix)]
    #[test]
    fn verify_refuses_a_target_that_does_not_answer_version() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("fails", "exit 3"),
            ("garbage", "echo 'hello world'"),
            ("other-program", "echo 'nano 7.0'"),
        ] {
            let p = script(dir.path(), name, body, 0o755);
            let err = verify_restart_target(&p, None, RESTART_VERIFY_TIMEOUT).unwrap_err();
            assert_eq!(err.0, RestartRefusalReason::TargetDidNotAnswer, "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn verify_times_out_on_a_target_that_hangs() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "hangs", "exec sleep 30", 0o755);
        let started = Instant::now();
        let err = verify_restart_target(&p, None, Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.0, RestartRefusalReason::TargetDidNotAnswer);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the bound must hold: {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_refuses_a_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "dad", "echo 'dot-agent-deck 0.45.0'", 0o755);
        let err = verify_restart_target(&p, Some("0.46.0"), RESTART_VERIFY_TIMEOUT).unwrap_err();
        assert_eq!(err.0, RestartRefusalReason::VersionMismatch);
        assert!(
            err.1.contains("0.45.0") && err.1.contains("0.46.0"),
            "{}",
            err.1
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_accepts_a_matching_target() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "dad", "echo 'dot-agent-deck 0.46.0'", 0o755);
        assert_eq!(
            verify_restart_target(&p, Some("0.46.0"), RESTART_VERIFY_TIMEOUT),
            Ok("0.46.0".to_string())
        );
        // A leading `v` on either side is not a mismatch.
        assert_eq!(
            verify_restart_target(&p, Some("v0.46.0"), RESTART_VERIFY_TIMEOUT),
            Ok("0.46.0".to_string())
        );
        // No expectation: any parsable version is accepted.
        assert_eq!(
            verify_restart_target(&p, None, RESTART_VERIFY_TIMEOUT),
            Ok("0.46.0".to_string())
        );
    }

    // ---- restart_decision ----

    #[test]
    fn an_idle_daemon_needs_no_confirmation() {
        let empty = RestartStopSet::default();
        assert_eq!(restart_decision(&empty, None), None);
        // A stale confirm against an idle daemon is no reason to ask.
        assert_eq!(restart_decision(&empty, Some(&set(&["a1"], &[]))), None);
    }

    #[test]
    fn a_live_daemon_without_confirm_asks() {
        let live = set(&["a1"], &[("p1", "orchestrator", "o")]);
        assert_eq!(
            restart_decision(&live, None),
            Some(RestartDaemonReply::NeedsConfirmation {
                at_stake: live.clone(),
                stale: false
            })
        );
        // Roles alone count too.
        let roles_only = set(&[], &[("p1", "coder", "o")]);
        assert!(matches!(
            restart_decision(&roles_only, None),
            Some(RestartDaemonReply::NeedsConfirmation { stale: false, .. })
        ));
    }

    #[test]
    fn a_matching_confirm_goes_ahead() {
        let live = set(&["a1"], &[("p1", "orchestrator", "o")]);
        assert_eq!(restart_decision(&live, Some(&live.clone())), None);
    }

    #[test]
    fn a_mismatching_confirm_asks_again_as_stale() {
        let live = set(&["a1", "a2"], &[("p1", "orchestrator", "o")]);
        for stale_confirm in [
            set(&["a1"], &[("p1", "orchestrator", "o")]),
            set(&["a1", "a2"], &[]),
            set(&["a1", "a2"], &[("p1", "coder", "o")]),
            set(&["a1", "a2", "a3"], &[("p1", "orchestrator", "o")]),
        ] {
            assert_eq!(
                restart_decision(&live, Some(&stale_confirm)),
                Some(RestartDaemonReply::NeedsConfirmation {
                    at_stake: live.clone(),
                    stale: true
                }),
                "{stale_confirm:?}"
            );
        }
    }

    #[test]
    fn confirmation_is_compared_order_insensitively_on_identity_only() {
        let live = set(
            &["a1", "a2"],
            &[("p1", "orchestrator", "o"), ("p2", "coder", "o")],
        );
        let mut reordered = set(
            &["a2", "a1"],
            &[("p2", "coder", "o"), ("p1", "orchestrator", "o")],
        );
        // Display-only fields differ too.
        for a in &mut reordered.agents {
            a.label = "renamed".into();
            a.cwd = None;
            a.pane_id = None;
        }
        reordered.roles[0].is_orchestrator = true;
        assert!(live.same_targets(&reordered));
        assert_eq!(restart_decision(&live, Some(&reordered)), None);
    }

    #[test]
    fn stop_set_labels_agents_like_the_running_summary() {
        let json = serde_json::json!({
            "id": "a1", "pane_id_env": "p1", "display_name": null, "cwd": "/w",
            "tab_membership": null, "agent_type": null, "rows": 24, "cols": 80,
        });
        let unnamed: AgentRecord = serde_json::from_value(json.clone()).unwrap();
        let mut named = unnamed.clone();
        named.id = "a2".into();
        named.display_name = Some("Coder".into());
        let roles = vec![role("p1", "coder", "o")];
        let s = stop_set(&roles, &[unnamed, named]);
        assert_eq!(s.roles, roles);
        assert_eq!(s.agents[0].label, "a1");
        assert_eq!(s.agents[0].pane_id.as_deref(), Some("p1"));
        assert_eq!(s.agents[0].cwd.as_deref(), Some("/w"));
        assert_eq!(s.agents[1].label, "Coder");
    }

    // ---- RestartControl ----

    #[test]
    fn restart_control_serialises_and_latches() {
        let control = RestartControl::default();
        let first = control.try_begin().expect("first request takes the lock");
        assert!(
            control.try_begin().is_none(),
            "a second is refused while held"
        );
        drop(first);
        let again = control.try_begin().expect("released when not accepted");
        control.mark_accepted(Some(PathBuf::from("/x/dot-agent-deck")));
        drop(again);
        assert!(control.is_accepted());
        assert!(control.try_begin().is_none(), "acceptance latches");
        assert_eq!(
            control.take_successor(),
            Some(PathBuf::from("/x/dot-agent-deck"))
        );
        assert_eq!(control.take_successor(), None, "consumed once");
    }
}
