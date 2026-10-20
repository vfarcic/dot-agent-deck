//! PRD #143 — `dot-agent-deck daemon install` / `daemon uninstall`: run the
//! daemon as a user service, so it starts at login (or boot, with systemd
//! lingering) and comes back after a crash or an upgrade restart.
//!
//! The service is a systemd user unit on Linux and a launchd LaunchAgent on
//! macOS. Both files are generated here rather than copied from the docs,
//! because the recipes they replace failed on the one host-specific value they
//! hardcoded: the binary's path. [`crate::platform::paths::durable_binary_path`]
//! already answers "which path goes into another program's persistent
//! configuration" for agent hooks, and the unit or plist is exactly that.
//!
//! Three properties the rest of this module is built around:
//!
//! - **Neither command stops a running daemon.** Stopping a daemon stops its
//!   agents, and that is `daemon stop`'s decision to make (issue #770), not a
//!   side effect of installing. [`plan_install`] starts the service only when
//!   no daemon answers at the endpoint, and never restarts an active one;
//!   [`plan_uninstall`] disables and removes the file and leaves a running
//!   daemon alone.
//! - **Restart on failure, not on every exit.** A crash, a kill and the
//!   upgrade restart (`SUPERVISED_RESTART_EXIT`, see
//!   [`crate::daemon_restart`]) bring the daemon back; `daemon stop` and the
//!   TUI's **Stop** exit 0 and stay stopped, on both platforms.
//! - **The service listens where the installing shell looks.** The endpoint
//!   overrides that shell has set are written into the service
//!   ([`carried_env`]), and `install` waits for the started daemon to answer
//!   at this shell's endpoint before it reports success.
//!
//! The launchd half has not been run on a Mac; see the PRD's Decisions.

use std::path::{Path, PathBuf};

/// The systemd user unit's name, which the docs and the upgrade
/// troubleshooting already use.
pub const SYSTEMD_UNIT: &str = "dot-agent-deck.service";

/// The published page the generated unit points at.
pub const DOCS_URL: &str = "https://agent-deck.devopstoolkit.ai/docs/daemon-service.md";

/// The LaunchAgent's label, and its plist's file stem.
pub const LAUNCHD_LABEL: &str = "ai.devopstoolkit.dot-agent-deck";

/// Written into every service: a supervised daemon with no agents must not
/// exit on the idle timer, or the service manager would be left with nothing
/// running (systemd) or start it again over and over (launchd).
pub const IDLE_SHUTDOWN_ENV: (&str, &str) = ("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "0");

/// The installing shell's variables a service needs to reach the same daemon
/// endpoint and state directory as that shell's clients. Credentials are
/// deliberately not here: the deck never writes them into a file.
pub const CARRIED_ENV: &[&str] = &[
    "DOT_AGENT_DECK_SOCKET",
    "DOT_AGENT_DECK_ATTACH_SOCKET",
    "DOT_AGENT_DECK_STATE_DIR",
];

/// The service manager `daemon install` targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceManager {
    Systemd,
    Launchd,
}

impl ServiceManager {
    /// This platform's manager, or `None` where `daemon install` is not
    /// supported (Windows). WSL is Linux here.
    pub fn for_this_platform() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::Launchd)
        } else if cfg!(target_os = "linux") {
            Some(Self::Systemd)
        } else {
            None
        }
    }

    /// Where the unit or plist lives: `$XDG_CONFIG_HOME/systemd/user/`
    /// (default `~/.config`) for systemd, `~/Library/LaunchAgents/` for
    /// launchd.
    pub fn service_file(self, home: &Path, xdg_config_home: Option<&Path>) -> PathBuf {
        match self {
            Self::Systemd => xdg_config_home
                .map(Path::to_path_buf)
                .unwrap_or_else(|| home.join(".config"))
                .join("systemd")
                .join("user")
                .join(SYSTEMD_UNIT),
            Self::Launchd => home
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
        }
    }

    /// The variables carried from the installing shell, beyond
    /// [`CARRIED_ENV`]. macOS sets no `XDG_RUNTIME_DIR`, so the sockets fall
    /// back to `$TMPDIR`, which a LaunchAgent is not guaranteed to inherit.
    fn extra_carried_env(self) -> &'static [&'static str] {
        match self {
            Self::Systemd => &[],
            Self::Launchd => &["TMPDIR"],
        }
    }
}

/// What a generated service runs: the binary and its environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    pub binary: String,
    pub env: Vec<(String, String)>,
}

/// The environment written into the service: [`IDLE_SHUTDOWN_ENV`], then each
/// carried variable `lookup` finds set and non-empty, in a fixed order.
///
/// A carried value must be an absolute path: a relative one would resolve
/// against the service's working directory, not the shell's, and so name a
/// different socket.
pub fn carried_env(
    manager: ServiceManager,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Vec<(String, String)>, String> {
    let mut env = vec![(
        IDLE_SHUTDOWN_ENV.0.to_string(),
        IDLE_SHUTDOWN_ENV.1.to_string(),
    )];
    for name in CARRIED_ENV.iter().chain(manager.extra_carried_env()) {
        let Some(value) = lookup(name).filter(|v| !v.is_empty()) else {
            continue;
        };
        if !Path::new(&value).is_absolute() {
            return Err(format!(
                "{name} is set to `{value}`, which is not an absolute path; the service \
                 would resolve it from a different directory than this shell does"
            ));
        }
        env.push((name.to_string(), value));
    }
    Ok(env)
}

/// Refuse a value no service file can hold on one line.
fn reject_control_chars(what: &str, value: &str) -> Result<(), String> {
    if value.contains(|c: char| c.is_control()) {
        return Err(format!("{what} contains a control character: {value:?}"));
    }
    Ok(())
}

/// A string inside double quotes in a systemd unit: `%` is a specifier and
/// `\` and `"` are quoting, so all three are escaped.
fn systemd_quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '%' => out.push_str("%%"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The systemd user unit for `spec`.
///
/// `ExecStart` also expands `$`, so the binary's path escapes it as `$$`.
/// `WorkingDirectory=~` starts the daemon in the home directory, as a daemon
/// lazy-spawned from a shell there would be: project configuration is found
/// by walking up from the daemon's directory, which from `/` would miss
/// `~/.dot-agent-deck.toml`. `RestartSec` spaces out a crash loop, which at
/// systemd's 100 ms default would reach its start limit within a second.
pub fn render_systemd_unit(spec: &ServiceSpec) -> Result<String, String> {
    reject_control_chars("the binary path", &spec.binary)?;
    let mut unit = String::from(
        "# Written by `dot-agent-deck daemon install`; `daemon uninstall` removes it.\n\
         # Put local changes in a drop-in (`systemctl --user edit dot-agent-deck.service`):\n\
         # a reinstall rewrites this file.\n\
         [Unit]\n\
         Description=dot-agent-deck daemon\n",
    );
    unit.push_str(&format!(
        "Documentation={DOCS_URL}\n\n[Service]\nWorkingDirectory=~\n"
    ));
    unit.push_str(&format!(
        "ExecStart={} daemon serve\n",
        systemd_quoted(&spec.binary).replace('$', "$$")
    ));
    for (name, value) in &spec.env {
        reject_control_chars(name, value)?;
        unit.push_str(&format!(
            "Environment={}\n",
            systemd_quoted(&format!("{name}={value}"))
        ));
    }
    unit.push_str(
        "Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
    );
    Ok(unit)
}

fn xml_escaped(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The LaunchAgent plist for `spec`, run from `home` (see
/// [`render_systemd_unit`] for why) and logging to [`launchd_log_path`].
///
/// `KeepAlive` is `{SuccessfulExit: false}`, launchd's spelling of systemd's
/// `Restart=on-failure`: a plain `KeepAlive true` would start the daemon again
/// after `daemon stop`.
pub fn render_launchd_plist(spec: &ServiceSpec, home: &Path) -> Result<String, String> {
    reject_control_chars("the binary path", &spec.binary)?;
    let log = launchd_log_path(home);
    let log = log.to_string_lossy();
    let home = home.to_string_lossy();
    reject_control_chars("the home directory", &home)?;
    let mut env = String::new();
    for (name, value) in &spec.env {
        reject_control_chars(name, value)?;
        env.push_str(&format!(
            "    <key>{}</key>\n    <string>{}</string>\n",
            xml_escaped(name),
            xml_escaped(value)
        ));
    }
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by `dot-agent-deck daemon install`; `daemon uninstall` removes it. A reinstall rewrites this file. -->
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{binary}</string>
    <string>daemon</string>
    <string>serve</string>
  </array>
  <key>WorkingDirectory</key>
  <string>{home}</string>
  <key>EnvironmentVariables</key>
  <dict>
{env}  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        binary = xml_escaped(&spec.binary),
        home = xml_escaped(&home),
        log = xml_escaped(&log),
    ))
}

/// Where the LaunchAgent's output goes.
pub fn launchd_log_path(home: &Path) -> PathBuf {
    home.join("Library").join("Logs").join("dot-agent-deck.log")
}

/// What `install` found before deciding what to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallObservation {
    /// The service is already running (systemd: the unit is active; launchd:
    /// the job is loaded).
    pub service_running: bool,
    /// A daemon answers at this shell's endpoint.
    pub daemon_live: bool,
}

/// What `install` does after writing the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPlan {
    /// Nothing is running: enable the service and start it now.
    EnableAndStart,
    /// The service is already running: enable it and leave it running. The
    /// new file takes effect when it next starts.
    EnableKeepRunning,
    /// A daemon the service manager did not start holds the endpoint: enable
    /// the service for the next login without starting it, since a second
    /// daemon could not bind and would only fail and be retried.
    EnableOnly,
}

pub fn plan_install(observed: InstallObservation) -> InstallPlan {
    match (observed.service_running, observed.daemon_live) {
        (true, _) => InstallPlan::EnableKeepRunning,
        (false, true) => InstallPlan::EnableOnly,
        (false, false) => InstallPlan::EnableAndStart,
    }
}

/// What `uninstall` does with a loaded launchd job. (systemd needs no
/// decision: `disable` without `--now` never stops the unit.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallPlan {
    /// Unload the job: nothing it started is running.
    Unload,
    /// Leave the job loaded, because unloading a launchd job stops its
    /// daemon. It is gone at the next login, and `daemon stop` ends it for
    /// good before then.
    LeaveLoaded,
    /// Nothing is loaded.
    NothingLoaded,
}

pub fn plan_uninstall(job_loaded: bool, daemon_live: bool) -> UninstallPlan {
    match (job_loaded, daemon_live) {
        (false, _) => UninstallPlan::NothingLoaded,
        (true, true) => UninstallPlan::LeaveLoaded,
        (true, false) => UninstallPlan::Unload,
    }
}

/// Write `content` to `path`, creating its directory, through a temporary
/// file renamed into place so a reader never sees half a unit. Returns whether
/// the file changed.
pub fn write_service_file(path: &Path, content: &str) -> std::io::Result<bool> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == content) {
        return Ok(false);
    }
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("service file has no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

/// Whether `loginctl show-user <uid> -p Linger --value` output means
/// lingering is off. Anything unrecognised is not reported, so a host without
/// logind prints no advice that does not apply to it.
pub fn linger_is_off(loginctl_stdout: &str) -> bool {
    loginctl_stdout.trim() == "no"
}

/// Run a service-manager tool, naming it when it cannot be started at all.
fn run_tool(program: &str, args: &[&str]) -> Result<std::process::Output, String> {
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run `{program}`: {e}"))
}

/// [`run_tool`], requiring success; the error carries the tool's stderr.
fn run_tool_ok(program: &str, args: &[&str]) -> Result<std::process::Output, String> {
    let output = run_tool(program, args)?;
    if output.status.success() {
        return Ok(output);
    }
    Err(format!(
        "`{program} {}` failed ({}): {}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Whether a daemon answers at this shell's attach endpoint. Anything other
/// than a clean "nothing is listening" counts as live: something holds the
/// endpoint, and a service started beside it could not bind.
async fn daemon_answers() -> bool {
    let client = crate::daemon_client::DaemonClient::new(
        crate::endpoint_resolve::client_attach_socket_path(),
    );
    !matches!(
        tokio::time::timeout(std::time::Duration::from_secs(3), client.probe_running()).await,
        Ok(Ok(None))
    )
}

/// Poll until a daemon answers at this shell's endpoint, for as long as a
/// lazy-spawned daemon is given to bind.
async fn wait_for_daemon() -> bool {
    let deadline = tokio::time::Instant::now() + crate::daemon_attach::daemon_start_poll_timeout();
    loop {
        if daemon_answers().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn unsupported() -> String {
    "`daemon install` supports systemd (Linux) and launchd (macOS) only".to_string()
}

/// This user's uid, as the service-manager commands spell it. Never reached on
/// Windows, where [`ServiceManager::for_this_platform`] refuses first.
fn uid() -> String {
    #[cfg(unix)]
    {
        crate::platform::paths::current_uid().to_string()
    }
    #[cfg(not(unix))]
    {
        String::new()
    }
}

fn launchd_domain() -> String {
    format!("gui/{}", uid())
}

fn launchd_target() -> String {
    format!("{}/{LAUNCHD_LABEL}", launchd_domain())
}

/// `dot-agent-deck daemon install`. Prints what it did on stdout; an `Err`
/// is the message the caller prints before exiting non-zero.
pub async fn run_install() -> Result<(), String> {
    let manager = ServiceManager::for_this_platform().ok_or_else(unsupported)?;
    let binary = crate::platform::paths::durable_binary_path()
        .map_err(|e| format!("could not find an installed dot-agent-deck to run: {e}"))?;
    let home = crate::platform::paths::home_dir();
    let spec = ServiceSpec {
        binary,
        env: carried_env(manager, |name| std::env::var(name).ok())?,
    };
    let file = manager.service_file(&home, crate::platform::paths::xdg_config_home().as_deref());
    let content = match manager {
        ServiceManager::Systemd => render_systemd_unit(&spec)?,
        ServiceManager::Launchd => render_launchd_plist(&spec, &home)?,
    };
    let changed = write_service_file(&file, &content)
        .map_err(|e| format!("could not write {}: {e}", file.display()))?;
    println!(
        "{} {} (runs {} daemon serve)",
        if changed { "wrote" } else { "unchanged:" },
        file.display(),
        spec.binary
    );

    match manager {
        ServiceManager::Systemd => install_systemd(changed).await?,
        ServiceManager::Launchd => install_launchd(&file, changed).await?,
    }
    Ok(())
}

async fn install_systemd(changed: bool) -> Result<(), String> {
    run_tool_ok("systemctl", &["--user", "daemon-reload"])?;
    let service_running = run_tool(
        "systemctl",
        &["--user", "is-active", "--quiet", SYSTEMD_UNIT],
    )?
    .status
    .success();
    let daemon_live = daemon_answers().await;
    match plan_install(InstallObservation {
        service_running,
        daemon_live,
    }) {
        InstallPlan::EnableAndStart => {
            run_tool_ok("systemctl", &["--user", "enable", "--now", SYSTEMD_UNIT])?;
            if !wait_for_daemon().await {
                return Err(format!(
                    "{SYSTEMD_UNIT} started, but no daemon answered at {} in time. \
                     Check `systemctl --user status {SYSTEMD_UNIT}` and \
                     `journalctl --user -u {SYSTEMD_UNIT}`; if the service is running, \
                     this shell looks for the daemon somewhere else (compare \
                     `echo $XDG_RUNTIME_DIR` here and in the service)",
                    crate::endpoint_resolve::client_attach_socket_path().display()
                ));
            }
            println!("the daemon is running as {SYSTEMD_UNIT}");
        }
        InstallPlan::EnableKeepRunning => {
            run_tool_ok("systemctl", &["--user", "enable", SYSTEMD_UNIT])?;
            if changed {
                println!(
                    "{SYSTEMD_UNIT} is already running and was left running; the new unit \
                     applies from its next start (`systemctl --user restart {SYSTEMD_UNIT}` \
                     applies it now and stops its agents)"
                );
            } else {
                println!("{SYSTEMD_UNIT} is already running");
            }
        }
        InstallPlan::EnableOnly => {
            run_tool_ok("systemctl", &["--user", "enable", SYSTEMD_UNIT])?;
            println!(
                "a daemon not started by systemd is running, so {SYSTEMD_UNIT} was enabled \
                 but not started; it starts at your next login. To switch now: \
                 `dot-agent-deck daemon stop`, then `systemctl --user start {SYSTEMD_UNIT}`"
            );
        }
    }
    let uid = uid();
    if let Ok(output) = run_tool("loginctl", &["show-user", &uid, "-p", "Linger", "--value"])
        && output.status.success()
        && linger_is_off(&String::from_utf8_lossy(&output.stdout))
    {
        println!(
            "lingering is off, so the service starts when you log in, not at boot. \
             To start it at boot: sudo loginctl enable-linger \"$USER\""
        );
    }
    Ok(())
}

async fn install_launchd(file: &Path, changed: bool) -> Result<(), String> {
    let file = file.to_string_lossy();
    let target = launchd_target();
    let service_running = run_tool("launchctl", &["print", &target])?.status.success();
    let daemon_live = daemon_answers().await;
    match plan_install(InstallObservation {
        service_running,
        daemon_live,
    }) {
        InstallPlan::EnableAndStart => {
            run_tool_ok("launchctl", &["bootstrap", &launchd_domain(), &file])?;
            if !wait_for_daemon().await {
                return Err(format!(
                    "the LaunchAgent was loaded, but no daemon answered at {} in time. \
                     Check `launchctl print {target}` and ~/Library/Logs/dot-agent-deck.log",
                    crate::endpoint_resolve::client_attach_socket_path().display()
                ));
            }
            println!("the daemon is running as LaunchAgent {LAUNCHD_LABEL}");
        }
        InstallPlan::EnableKeepRunning => {
            if changed {
                println!(
                    "LaunchAgent {LAUNCHD_LABEL} is already loaded and was left running; \
                     the new plist applies from your next login"
                );
            } else {
                println!("LaunchAgent {LAUNCHD_LABEL} is already loaded");
            }
        }
        InstallPlan::EnableOnly => {
            println!(
                "a daemon not started by launchd is running, so the LaunchAgent was not \
                 loaded; it loads at your next login. To switch now: \
                 `dot-agent-deck daemon stop`, then `launchctl bootstrap {} {file}`",
                launchd_domain()
            );
        }
    }
    Ok(())
}

/// `dot-agent-deck daemon uninstall`. Removes the service so it no longer
/// starts, and leaves a running daemon running.
pub async fn run_uninstall() -> Result<(), String> {
    let manager = ServiceManager::for_this_platform().ok_or_else(unsupported)?;
    let home = crate::platform::paths::home_dir();
    let file = manager.service_file(&home, crate::platform::paths::xdg_config_home().as_deref());
    let still_running = match manager {
        ServiceManager::Systemd => {
            // `disable` without `--now` never stops the unit. A unit that was
            // never installed fails here, which is not an error to report.
            let _ = run_tool("systemctl", &["--user", "disable", SYSTEMD_UNIT])?;
            remove_service_file(&file)?;
            run_tool_ok("systemctl", &["--user", "daemon-reload"])?;
            run_tool(
                "systemctl",
                &["--user", "is-active", "--quiet", SYSTEMD_UNIT],
            )?
            .status
            .success()
        }
        ServiceManager::Launchd => {
            let target = launchd_target();
            let loaded = run_tool("launchctl", &["print", &target])?.status.success();
            let plan = plan_uninstall(loaded, loaded && daemon_answers().await);
            if plan == UninstallPlan::Unload {
                run_tool_ok("launchctl", &["bootout", &target])?;
            }
            remove_service_file(&file)?;
            plan == UninstallPlan::LeaveLoaded
        }
    };
    if still_running {
        println!(
            "the daemon the service started is still running, with its agents; it will not \
             start again at login. `dot-agent-deck daemon stop` stops it"
        );
    }
    Ok(())
}

fn remove_service_file(file: &Path) -> Result<(), String> {
    match std::fs::remove_file(file) {
        Ok(()) => {
            println!("removed {}", file.display());
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("not installed: {} does not exist", file.display());
            Ok(())
        }
        Err(e) => Err(format!("could not remove {}: {e}", file.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            binary: "/home/u/.local/bin/dot-agent-deck".into(),
            env: vec![("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS".into(), "0".into())],
        }
    }

    #[test]
    fn systemd_unit_runs_serve_restarts_on_failure_and_starts_with_the_user_manager() {
        let unit = render_systemd_unit(&spec()).unwrap();
        assert!(unit.contains("ExecStart=\"/home/u/.local/bin/dot-agent-deck\" daemon serve\n"));
        assert!(unit.contains("Environment=\"DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0\"\n"));
        assert!(unit.contains("Restart=on-failure\n"));
        assert!(!unit.contains("Restart=always"));
        assert!(unit.contains("WantedBy=default.target\n"));
        assert!(unit.contains("WorkingDirectory=~\n"));
    }

    #[test]
    fn systemd_unit_escapes_specifiers_variables_and_quotes() {
        let unit = render_systemd_unit(&ServiceSpec {
            binary: "/opt/my $dir/100%/\"x\"\\dot-agent-deck".into(),
            env: vec![("DOT_AGENT_DECK_STATE_DIR".into(), "/s/50%".into())],
        })
        .unwrap();
        assert!(
            unit.contains(r#"ExecStart="/opt/my $$dir/100%%/\"x\"\\dot-agent-deck" daemon serve"#),
            "{unit}"
        );
        assert!(unit.contains(r#"Environment="DOT_AGENT_DECK_STATE_DIR=/s/50%%""#));
    }

    #[test]
    fn a_newline_cannot_inject_a_directive() {
        let mut bad = spec();
        bad.binary = "/x\nExecStartPre=/bin/evil".into();
        assert!(render_systemd_unit(&bad).is_err());
        assert!(render_launchd_plist(&bad, Path::new("/h")).is_err());
        let mut bad = spec();
        bad.env
            .push(("DOT_AGENT_DECK_SOCKET".into(), "/a\n[Service]".into()));
        assert!(render_systemd_unit(&bad).is_err());
        assert!(render_launchd_plist(&bad, Path::new("/h")).is_err());
        assert!(render_launchd_plist(&spec(), Path::new("/h\n")).is_err());
    }

    #[test]
    fn launchd_plist_restarts_only_on_failure_and_escapes_xml() {
        let plist = render_launchd_plist(
            &ServiceSpec {
                binary: "/Users/a&b/<bin>/dot-agent-deck".into(),
                env: spec().env,
            },
            Path::new("/Users/u"),
        )
        .unwrap();
        assert!(plist.contains("<key>WorkingDirectory</key>\n  <string>/Users/u</string>"));
        assert!(plist.contains("<string>/Users/u/Library/Logs/dot-agent-deck.log</string>"));
        assert!(plist.contains("<string>/Users/a&amp;b/&lt;bin&gt;/dot-agent-deck</string>"));
        assert!(plist.contains(
            "<key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>"
        ));
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(
            plist.contains("<key>DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS</key>\n    <string>0</string>")
        );
        assert!(plist.contains(&format!("<string>{LAUNCHD_LABEL}</string>")));
    }

    #[test]
    fn service_files_live_where_each_manager_reads_them() {
        let home = Path::new("/home/u");
        assert_eq!(
            ServiceManager::Systemd.service_file(home, None),
            Path::new("/home/u/.config/systemd/user/dot-agent-deck.service")
        );
        assert_eq!(
            ServiceManager::Systemd.service_file(home, Some(Path::new("/cfg"))),
            Path::new("/cfg/systemd/user/dot-agent-deck.service")
        );
        assert_eq!(
            ServiceManager::Launchd.service_file(Path::new("/Users/u"), None),
            Path::new("/Users/u/Library/LaunchAgents/ai.devopstoolkit.dot-agent-deck.plist")
        );
    }

    #[test]
    fn carried_env_keeps_the_endpoint_overrides_and_nothing_else() {
        let lookup = |name: &str| match name {
            "DOT_AGENT_DECK_ATTACH_SOCKET" => Some("/s/attach.sock".to_string()),
            "DOT_AGENT_DECK_SOCKET" => Some(String::new()),
            "TMPDIR" => Some("/var/folders/x/T/".to_string()),
            "ANTHROPIC_API_KEY" => Some("secret".to_string()),
            _ => None,
        };
        let systemd = carried_env(ServiceManager::Systemd, lookup).unwrap();
        assert_eq!(
            systemd,
            vec![
                ("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS".into(), "0".into()),
                (
                    "DOT_AGENT_DECK_ATTACH_SOCKET".into(),
                    "/s/attach.sock".into()
                ),
            ]
        );
        let launchd = carried_env(ServiceManager::Launchd, lookup).unwrap();
        assert_eq!(
            launchd.last(),
            Some(&("TMPDIR".to_string(), "/var/folders/x/T/".to_string()))
        );
        assert!(!launchd.iter().any(|(k, _)| k == "ANTHROPIC_API_KEY"));
    }

    #[test]
    fn carried_env_refuses_a_relative_override() {
        let err = carried_env(ServiceManager::Systemd, |name| {
            (name == "DOT_AGENT_DECK_STATE_DIR").then(|| "state".to_string())
        })
        .unwrap_err();
        assert!(err.contains("DOT_AGENT_DECK_STATE_DIR"), "{err}");
    }

    #[test]
    fn install_never_starts_beside_a_running_daemon() {
        let plan = |service_running, daemon_live| {
            plan_install(InstallObservation {
                service_running,
                daemon_live,
            })
        };
        assert_eq!(plan(false, false), InstallPlan::EnableAndStart);
        assert_eq!(plan(false, true), InstallPlan::EnableOnly);
        assert_eq!(plan(true, true), InstallPlan::EnableKeepRunning);
        assert_eq!(plan(true, false), InstallPlan::EnableKeepRunning);
    }

    #[test]
    fn uninstall_never_unloads_a_job_whose_daemon_is_running() {
        assert_eq!(plan_uninstall(true, true), UninstallPlan::LeaveLoaded);
        assert_eq!(plan_uninstall(true, false), UninstallPlan::Unload);
        assert_eq!(plan_uninstall(false, true), UninstallPlan::NothingLoaded);
    }

    #[test]
    fn write_service_file_creates_the_directory_and_reports_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/unit.service");
        assert!(write_service_file(&path, "one").unwrap());
        assert!(!write_service_file(&path, "one").unwrap());
        assert!(write_service_file(&path, "two").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        assert!(!path.with_extension("service.tmp").exists());
    }

    #[test]
    fn linger_advice_only_for_an_explicit_no() {
        assert!(linger_is_off("no\n"));
        assert!(!linger_is_off("yes\n"));
        assert!(!linger_is_off(""));
    }
}
