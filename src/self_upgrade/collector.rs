//! What an upgrade of the CLI does about the per-run resource collector
//! (`dot-agent-deck-collector`, issue #2064), in the same words in the TUI, the
//! CLI and the desktop app.
//!
//! A release ships the collector as its own Linux asset beside the CLI's,
//! listed in the same `checksums.txt` and covered by the same build
//! provenance. Homebrew and Nix install both binaries from one package, so
//! upgrading the package upgrades both. A collector downloaded from a release
//! is a file of its own, which nothing else upgrades, so once an upgrade has
//! replaced, installed or staged a downloaded CLI, the client runs
//! [`after_upgrade`]: a collector **in the same folder as that CLI**, the
//! place `docs/telemetry.md` installs it, is upgraded to the same release with
//! the same checks (the manifest's provenance as the plan promised, the
//! checksum, the new file's `--version`, and the re-hash before it is
//! installed). One found anywhere else, one the folder only links to, and one
//! a system package owns are left alone, and the result says why; with no
//! collector, nothing is installed.
//!
//! A collector that is running keeps running the old file. When the systemd
//! user service `docs/telemetry.md` sets up is running the file that was
//! replaced, it is restarted (`systemctl --user try-restart`), which the plan
//! said before the user confirmed; otherwise the result says that a running
//! collector keeps its old version until it is restarted.
//!
//! Every command, `systemctl` included, and every question about the machine
//! goes through the [`Host`], so a test fakes the machine.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::execute::{
    ReleaseSource, Staging, UNSYNCED_WARNING, atomic_replace, download_verified, write_new_file,
};
use super::{
    CommandOutput, CopyKind, Host, Outcome, PlanAction, PlanLine, UpgradeError, UpgradePlan, plan,
};

/// The collector's file name, as every install method names it.
pub const COLLECTOR_BINARY: &str = "dot-agent-deck-collector";

/// The systemd user service `docs/telemetry.md` sets up.
pub const COLLECTOR_SERVICE: &str = "dot-agent-deck-collector.service";

/// What restarts that service, for the user to run.
pub const RESTART_COMMAND: &str = "systemctl --user restart dot-agent-deck-collector";

/// How long the service's restart may take: stopping the collector waits up
/// to 10 seconds for its last send (`docs/telemetry.md`), and systemd's own
/// stop timeout comes on top.
pub const RESTART_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// What the upgrade did about the collector: the lines a client shows after
/// the CLI's own result, and whether an attempted collector upgrade failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub lines: Vec<PlanLine>,
    /// False when a collector was found to upgrade and upgrading it failed.
    /// A collector left alone, or none, is not a failure.
    pub ok: bool,
}

impl Report {
    fn said(lines: Vec<PlanLine>) -> Self {
        Self { lines, ok: true }
    }

    /// The collector step could not run: `detail` says why.
    pub fn failed(detail: &str) -> Self {
        Self {
            lines: vec![PlanLine::Text(format!(
                "The resource collector was not upgraded: {detail}"
            ))],
            ok: false,
        }
    }

    /// The user cancelled the upgrade before the collector was changed.
    pub fn cancelled() -> Self {
        Self::failed("the upgrade was cancelled before it changed the collector")
    }
}

/// How the CLI beside the collector was installed by the upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The CLI's folder is writable: the collector is replaced in place.
    Replace,
    /// The CLI's folder is not writable: the collector is downloaded, checked
    /// and staged, and the user installs it with the command shown.
    Stage,
}

/// What is where the collector would be, beside the CLI the upgrade changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Finding {
    /// The upgrade changed no downloaded CLI on Linux: nothing to say.
    NotApplicable,
    /// No collector in the CLI's folder.
    Absent { cli_dir: PathBuf },
    /// A collector in the CLI's folder that this upgrade does not own.
    LeftAlone { path: PathBuf, why: String },
    /// A collector in the CLI's folder that does not report a release.
    NoVersion { path: PathBuf },
    /// A collector in the CLI's folder that is not older than the release.
    NotOlder { path: PathBuf, version: String },
    /// A release collector in the CLI's folder, older than the release.
    Behind {
        path: PathBuf,
        version: String,
        mode: Mode,
        asset: &'static str,
    },
}

/// The CLI `outcome` of `plan` changed, and how: the file it replaced in a
/// writable folder, or the target it installed behind a prompt or staged for
/// the user to install. `None` for any other upgrade: Homebrew's formula
/// carries the collector itself, Nix and source builds are not changed from
/// here, and the desktop app's own upgrade installs no collector.
fn changed_cli(plan: &UpgradePlan, outcome: &Outcome) -> Option<(PathBuf, Mode)> {
    if plan.installation.copy != CopyKind::Cli {
        return None;
    }
    match (&plan.action, outcome) {
        (PlanAction::ReplaceBinary { .. }, Outcome::Replaced { path, .. }) => {
            Some((path.clone(), Mode::Replace))
        }
        (PlanAction::StagedInstall { target, .. }, Outcome::Installed { .. })
        | (PlanAction::StagedInstall { target, .. }, Outcome::Staged { .. }) => {
            Some((target.clone(), Mode::Stage))
        }
        _ => None,
    }
}

/// The version `dot-agent-deck-collector --version` printed, without a
/// leading `v`, or `None` when it did not answer as the collector at a
/// release version.
pub(crate) fn parse_collector_version(stdout: &str) -> Option<String> {
    let mut words = stdout.split_whitespace();
    if words.next()? != COLLECTOR_BINARY {
        return None;
    }
    let version = words.next()?;
    let version = version.strip_prefix('v').unwrap_or(version);
    let starts_with_digit = version.chars().next()?.is_ascii_digit();
    (starts_with_digit && version.contains('.') && !crate::version::is_unknown_version(version))
        .then(|| version.to_string())
}

/// What the collector at `path` reports, through `host`. A cancelled
/// `--version` is the error the upgrade stops with.
fn collector_version(host: &dyn Host, path: &Path) -> Result<Option<String>, UpgradeError> {
    let args = [OsStr::new("--version")];
    let answer = super::unless_cancelled(host.run(path, &args), path, &args)?;
    Ok(answer
        .ok()
        .filter(|output| output.success)
        .and_then(|output| parse_collector_version(&output.stdout)))
}

/// Check that the downloaded collector at `candidate` answers as the
/// collector at `expected`.
fn verify_collector_version(
    host: &dyn Host,
    candidate: &Path,
    expected: &str,
) -> Result<(), UpgradeError> {
    match collector_version(host, candidate)? {
        Some(actual) if actual == expected => Ok(()),
        actual => Err(UpgradeError::Io(format!(
            "The downloaded collector reports {} instead of {COLLECTOR_BINARY} {expected}, so it was not installed.",
            actual.map_or_else(|| "no collector version".to_string(), |v| format!("v{v}"))
        ))),
    }
}

/// Look beside the CLI `outcome` of `plan` changed for a collector, and
/// decide what to do with it.
pub(crate) fn survey(
    host: &dyn Host,
    plan: &UpgradePlan,
    outcome: &Outcome,
) -> Result<Finding, UpgradeError> {
    let Some(platform) = plan.installation.platform.filter(|p| p.is_linux()) else {
        // The collector runs on Linux only, so macOS has none to upgrade.
        return Ok(Finding::NotApplicable);
    };
    let Some((cli, mode)) = changed_cli(plan, outcome) else {
        return Ok(Finding::NotApplicable);
    };
    let Some(cli_dir) = cli.parent().map(Path::to_path_buf) else {
        return Ok(Finding::NotApplicable);
    };
    let path = cli_dir.join(COLLECTOR_BINARY);
    if !host.is_executable(&path) {
        return Ok(Finding::Absent { cli_dir });
    }
    let canonical_dir = host
        .canonicalize(&cli_dir)
        .unwrap_or_else(|| cli_dir.clone());
    match host.canonicalize(&path) {
        Some(resolved) if resolved.parent() == Some(canonical_dir.as_path()) => {}
        Some(resolved) => {
            return Ok(Finding::LeftAlone {
                why: format!(
                    "it links to {}, which is not a file in that folder, so it was installed another way",
                    resolved.display()
                ),
                path,
            });
        }
        None => {
            return Ok(Finding::LeftAlone {
                why: "where it leads could not be read".to_string(),
                path,
            });
        }
    }
    if let Some(dpkg_query) = &plan.installation.tools.dpkg_query
        && let Some(package) = super::detect::dpkg_owner(host, dpkg_query, &path)
    {
        return Ok(Finding::LeftAlone {
            why: format!("the system package `{package}` owns it"),
            path,
        });
    }
    let Some(version) = collector_version(host, &path)? else {
        return Ok(Finding::NoVersion { path });
    };
    if !super::is_newer(&version, &plan.latest) {
        return Ok(Finding::NotOlder { path, version });
    }
    let Some(asset) = platform.collector_asset() else {
        return Ok(Finding::NotApplicable);
    };
    Ok(Finding::Behind {
        path,
        version,
        mode,
        asset,
    })
}

/// The first collector on `PATH`, when it is not `beside` (the one in the
/// upgraded CLI's folder), with what it reports.
fn elsewhere(host: &dyn Host, beside: &Path) -> Option<(PathBuf, Option<String>)> {
    let found = host.find_program(COLLECTOR_BINARY)?;
    let canonical = |path: &Path| {
        host.canonicalize(path)
            .unwrap_or_else(|| path.to_path_buf())
    };
    if canonical(&found) == canonical(beside) {
        return None;
    }
    let version = collector_version(host, &found).ok().flatten();
    Some((found, version))
}

/// What to say about a collector found on `PATH` outside the upgraded CLI's
/// folder: it is not this upgrade's to replace.
fn elsewhere_line(path: &Path, version: Option<&str>) -> PlanLine {
    let reports = version.map_or_else(String::new, |v| format!(", v{v},"));
    PlanLine::Text(format!(
        "The resource collector first on your PATH, {}{reports} is not in the upgraded CLI's folder, so it was left as it is. Upgrade it the way you installed it.",
        path.display()
    ))
}

/// What to tell the user about the resource collector after `outcome` of
/// `plan`, upgrading a release collector beside the CLI the upgrade changed.
/// Empty when the upgrade changed no downloaded CLI on Linux.
///
/// Every await is a download, before the collector is changed, so a client
/// may drop this at an await to cancel it, as it does [`super::execute::execute`].
pub async fn after_upgrade(
    host: &dyn Host,
    plan: &UpgradePlan,
    outcome: &Outcome,
    source: &ReleaseSource,
    staging_root: &Path,
) -> Report {
    let finding = match survey(host, plan, outcome) {
        Ok(finding) => finding,
        Err(error) => return Report::failed(&error.to_string()),
    };
    let beside = match &finding {
        Finding::NotApplicable => return Report::said(Vec::new()),
        Finding::Absent { cli_dir } => cli_dir.join(COLLECTOR_BINARY),
        Finding::LeftAlone { path, .. }
        | Finding::NoVersion { path }
        | Finding::NotOlder { path, .. }
        | Finding::Behind { path, .. } => path.clone(),
    };
    let mut report = match finding {
        Finding::NotApplicable => unreachable!("returned above"),
        Finding::Absent { cli_dir } => Report::said(vec![PlanLine::Text(format!(
            "No resource collector ({COLLECTOR_BINARY}) is in {}, so none was upgraded.",
            cli_dir.display()
        ))]),
        Finding::LeftAlone { path, why } => Report::said(vec![PlanLine::Text(format!(
            "The resource collector at {} was left as it is: {why}. Upgrade it the way you installed it.",
            path.display()
        ))]),
        Finding::NoVersion { path } => Report::said(vec![PlanLine::Text(format!(
            "The resource collector at {} does not report a release version, so it was left as it is.",
            path.display()
        ))]),
        Finding::NotOlder { path, version } => Report::said(vec![PlanLine::Text(format!(
            "The resource collector at {} already runs v{version}, so it was left as it is.",
            path.display()
        ))]),
        Finding::Behind {
            path,
            version,
            mode,
            asset,
        } => {
            upgrade(
                host,
                plan,
                source,
                staging_root,
                &path,
                &version,
                mode,
                asset,
            )
            .await
        }
    };
    if let Some((path, version)) = elsewhere(host, &beside) {
        report.lines.push(elsewhere_line(&path, version.as_deref()));
    }
    report
}

/// Upgrade the release collector at `path`, which reports `previous`, to
/// `plan`'s release.
#[allow(clippy::too_many_arguments)]
async fn upgrade(
    host: &dyn Host,
    plan: &UpgradePlan,
    source: &ReleaseSource,
    staging_root: &Path,
    path: &Path,
    previous: &str,
    mode: Mode,
    asset: &str,
) -> Report {
    let version = plan.latest.as_str();
    // The CLI is current once this step runs, so a later upgrade plans it as
    // up to date and does not come back for the collector: the user replaces
    // it by hand instead of being told to try again.
    let failed = |error: UpgradeError| Report {
        lines: vec![
            PlanLine::Text(format!(
                "The resource collector at {} was not upgraded and stays on v{previous}: {error}",
                path.display()
            )),
            PlanLine::Text(manual_line(asset, version, path)),
        ],
        ok: false,
    };
    let mut staging = match Staging::create(staging_root, version) {
        Ok(staging) => staging,
        Err(error) => return failed(error),
    };
    let downloaded = match download_verified(
        host,
        source,
        version,
        asset,
        &plan.provenance,
        staging.dir(),
    )
    .await
    {
        Ok(downloaded) => downloaded,
        Err(error) => return failed(error),
    };
    match mode {
        Mode::Replace => {
            let synced = match atomic_replace(
                host,
                path,
                &downloaded.bytes,
                &downloaded.sha256,
                |candidate| verify_collector_version(host, candidate, version),
            ) {
                Ok(synced) => synced,
                Err(error) => return failed(error),
            };
            let mut lines = vec![PlanLine::Text(format!(
                "Upgraded the resource collector at {} from v{previous} to v{version}, checked the same way.",
                path.display()
            ))];
            if !synced {
                lines.push(PlanLine::Text(UNSYNCED_WARNING.to_string()));
            }
            lines.extend(restart(host, path, previous, version));
            Report::said(lines)
        }
        Mode::Stage => {
            let staged = staging.dir().join(asset);
            if let Err(error) = write_new_file(&staged, &downloaded.bytes, 0o755)
                .and_then(|()| verify_collector_version(host, &staged, version))
            {
                return failed(error);
            }
            let command = plan::install_binary_command(
                plan.installation.platform,
                &staged,
                path,
                &downloaded.sha256,
            );
            let mut lines = vec![PlanLine::Text(format!(
                "Downloaded and checked v{version} of the resource collector at {}, which you cannot write to.",
                path.display()
            ))];
            match command {
                Some(command) => {
                    staging.keep();
                    lines.push(PlanLine::Text("Install it with:".to_string()));
                    lines.push(PlanLine::Command(command));
                }
                None => lines.push(PlanLine::Text(format!(
                    "Its path cannot be shown safely in a command. {}",
                    manual_line(asset, version, path)
                ))),
            }
            lines.extend(restart_later(host, path, previous));
            Report::said(lines)
        }
    }
}

/// How to replace the collector at `path` by hand with `asset` of release
/// `version`.
fn manual_line(asset: &str, version: &str, path: &Path) -> String {
    format!(
        "Replace it by hand: download `{asset}` from release v{version}, check it against that release's checksums.txt, and save it over {}.",
        path.display()
    )
}

/// What systemd says about the collector's user service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Service {
    /// No `systemctl`, no user manager, or no such unit.
    Unknown,
    /// The unit exists and is not running.
    Stopped,
    /// The unit is running the file at this path.
    Running(PathBuf),
}

/// Read `systemctl --user show`'s `ActiveState=` and `ExecStart=` lines.
pub(crate) fn parse_service(stdout: &str) -> Service {
    let mut active = None;
    let mut exec = None;
    let mut loaded = true;
    for line in stdout.lines() {
        if let Some(state) = line.strip_prefix("ActiveState=") {
            active = Some(state.trim().to_string());
        } else if let Some(state) = line.strip_prefix("LoadState=") {
            loaded = state.trim() == "loaded";
        } else if let Some(start) = line.strip_prefix("ExecStart=") {
            // `{ path=/home/u/.local/bin/dot-agent-deck-collector ; argv[]=… ; … }`
            exec = start.split("path=").nth(1).and_then(|rest| {
                let path = rest.split(" ;").next()?.trim();
                (!path.is_empty()).then(|| PathBuf::from(path))
            });
        }
    }
    if !loaded {
        return Service::Unknown;
    }
    match (active.as_deref(), exec) {
        (Some("active" | "activating" | "reloading"), Some(path)) => Service::Running(path),
        (Some(_), _) => Service::Stopped,
        (None, _) => Service::Unknown,
    }
}

/// The collector's user service, through `host`'s `systemctl`.
fn service(host: &dyn Host) -> (Option<PathBuf>, Service) {
    let Some(systemctl) = host.find_program("systemctl") else {
        return (None, Service::Unknown);
    };
    let answer = host.run(
        &systemctl,
        &[
            OsStr::new("--user"),
            OsStr::new("show"),
            OsStr::new(COLLECTOR_SERVICE),
            OsStr::new("--property=LoadState"),
            OsStr::new("--property=ActiveState"),
            OsStr::new("--property=ExecStart"),
        ],
    );
    let service = match answer {
        Ok(CommandOutput {
            success: true,
            stdout,
            ..
        }) => parse_service(&stdout),
        _ => Service::Unknown,
    };
    (Some(systemctl), service)
}

/// Whether `running` (the service's `ExecStart` program) is the collector
/// at `path`.
fn runs(host: &dyn Host, running: &Path, path: &Path) -> bool {
    let canonical = |p: &Path| host.canonicalize(p).unwrap_or_else(|| p.to_path_buf());
    running == path || canonical(running) == canonical(path)
}

/// Said when no service was restarted: a collector the user started keeps
/// the old version.
fn keeps_running(previous: &str) -> PlanLine {
    PlanLine::Text(format!(
        "A collector that was already running from it keeps running v{previous} until you restart it."
    ))
}

/// After the collector at `path` was replaced: restart its user service when
/// that service runs it, and otherwise say that a running collector keeps
/// the old version.
fn restart(host: &dyn Host, path: &Path, previous: &str, version: &str) -> Vec<PlanLine> {
    match service(host) {
        (Some(systemctl), Service::Running(running)) if runs(host, &running, path) => {
            let args = [
                OsStr::new("--user"),
                OsStr::new("try-restart"),
                OsStr::new(COLLECTOR_SERVICE),
            ];
            match host.run_within(&systemctl, &args, RESTART_TIMEOUT) {
                Ok(output) if output.success => vec![PlanLine::Text(format!(
                    "Restarted the {COLLECTOR_SERVICE} user service, so it now runs v{version}."
                ))],
                Ok(output) => restart_failed(previous, &super::failure_detail(&output)),
                Err(error) => restart_failed(previous, &error.to_string()),
            }
        }
        (_, Service::Running(running)) => vec![
            PlanLine::Text(format!(
                "The {COLLECTOR_SERVICE} user service runs {}, not the upgraded file, so it was not restarted.",
                running.display()
            )),
            keeps_running(previous),
        ],
        _ => vec![keeps_running(previous)],
    }
}

fn restart_failed(previous: &str, detail: &str) -> Vec<PlanLine> {
    vec![
        PlanLine::Text(format!(
            "Restarting the {COLLECTOR_SERVICE} user service failed ({detail}), so it may still run v{previous}. Restart it with:"
        )),
        PlanLine::Command(RESTART_COMMAND.to_string()),
    ]
}

/// After the collector at `path` was staged: once the user installs it, a
/// running collector needs a restart.
fn restart_later(host: &dyn Host, path: &Path, previous: &str) -> Vec<PlanLine> {
    match service(host) {
        (_, Service::Running(running)) if runs(host, &running, path) => vec![
            PlanLine::Text(format!(
                "Its {COLLECTOR_SERVICE} user service keeps running v{previous} until it is restarted. Once it is installed, restart it with:"
            )),
            PlanLine::Command(RESTART_COMMAND.to_string()),
        ],
        _ => vec![PlanLine::Text(format!(
            "Once it is installed, a collector that was already running from it keeps running v{previous} until you restart it."
        ))],
    }
}

/// What the plan says about the collector before the user confirms, for a
/// downloaded CLI on Linux; `None` for any other plan.
pub(crate) fn plan_line(plan: &UpgradePlan) -> Option<PlanLine> {
    if plan.installation.copy != CopyKind::Cli
        || !plan.installation.platform.is_some_and(|p| p.is_linux())
    {
        return None;
    }
    let latest = &plan.latest;
    let line = match plan.action {
        PlanAction::ReplaceBinary { .. } => format!(
            "A resource collector ({COLLECTOR_BINARY}) from a release in the same folder is upgraded to v{latest} too, checked the same way, and its systemd user service is restarted when it runs that file."
        ),
        PlanAction::StagedInstall { .. } => format!(
            "A resource collector ({COLLECTOR_BINARY}) from a release in the same folder is downloaded and checked too, and the command to install it is shown once it is verified."
        ),
        _ => return None,
    };
    Some(PlanLine::Text(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_upgrade::detect::{InstallMethod, Installation, Platform, Tools};
    use crate::self_upgrade::test_host::{FakeHost, fail, ok};
    use crate::self_upgrade::{Provenance, ProvenanceCheck, verify};

    const CLI: &str = "/home/u/.local/bin/dot-agent-deck";
    const BESIDE: &str = "/home/u/.local/bin/dot-agent-deck-collector";
    const SYSTEMCTL: &str = "/usr/bin/systemctl";

    fn cli_plan(platform: Platform, action: PlanAction) -> UpgradePlan {
        UpgradePlan {
            installation: Installation {
                copy: CopyKind::Cli,
                executable: PathBuf::from(CLI),
                version: "0.49.3".into(),
                platform: Some(platform),
                method: InstallMethod::DownloadedWritable {
                    binary: PathBuf::from(CLI),
                },
                tools: Tools::default(),
            },
            latest: "0.49.5".into(),
            action,
            provenance: ProvenanceCheck::Unavailable {
                reason: "`gh` is not installed".into(),
            },
            stable_switch: None,
        }
    }

    fn replace_plan() -> UpgradePlan {
        cli_plan(
            Platform::LinuxAmd64,
            PlanAction::ReplaceBinary {
                target: PathBuf::from(CLI),
                asset: "dot-agent-deck-linux-amd64".into(),
            },
        )
    }

    fn staged_plan() -> UpgradePlan {
        cli_plan(
            Platform::LinuxAmd64,
            PlanAction::StagedInstall {
                target: PathBuf::from(CLI),
                asset: "dot-agent-deck-linux-amd64".into(),
                pkexec: None,
            },
        )
    }

    fn replaced() -> Outcome {
        Outcome::Replaced {
            path: PathBuf::from(CLI),
            version: "0.49.5".into(),
            provenance: Provenance::Skipped {
                reason: "`gh` is not installed".into(),
            },
            synced: true,
        }
    }

    fn collector_at(host: FakeHost, path: &str, version: &str) -> FakeHost {
        host.exe(path).answer(
            &format!("{path} --version"),
            ok(&format!("{COLLECTOR_BINARY} {version}\n")),
        )
    }

    fn texts(lines: &[PlanLine]) -> Vec<String> {
        plan::render_lines(lines)
    }

    /// The collector's `--version` answer is read as the collector's, never
    /// as the deck's, and a build that does not know its version is no
    /// release.
    #[test]
    fn collector_001_reads_only_the_collectors_version() {
        assert_eq!(
            parse_collector_version("dot-agent-deck-collector 0.49.4\n"),
            Some("0.49.4".into())
        );
        assert_eq!(
            parse_collector_version("dot-agent-deck-collector v0.50.0-beta.1"),
            Some("0.50.0-beta.1".into())
        );
        assert_eq!(parse_collector_version("dot-agent-deck 0.49.4"), None);
        assert_eq!(
            parse_collector_version("dot-agent-deck-collector dev"),
            None
        );
        assert_eq!(parse_collector_version(""), None);
    }

    /// A release collector beside a replaced CLI, older than the release, is
    /// the one to upgrade in place; beside a staged CLI it is staged too.
    #[test]
    fn collector_002_an_older_collector_beside_the_cli_is_behind() {
        let host = collector_at(FakeHost::new(), BESIDE, "0.49.3");
        assert_eq!(
            survey(&host, &replace_plan(), &replaced()).unwrap(),
            Finding::Behind {
                path: PathBuf::from(BESIDE),
                version: "0.49.3".into(),
                mode: Mode::Replace,
                asset: "dot-agent-deck-collector-linux-amd64",
            }
        );
        let staged = Outcome::Staged {
            path: PathBuf::from("/stage/dot-agent-deck-linux-amd64"),
            command: None,
            version: "0.49.5".into(),
            provenance: Provenance::Verified,
        };
        let Finding::Behind { mode, .. } = survey(&host, &staged_plan(), &staged).unwrap() else {
            panic!("a staged CLI's collector is behind too");
        };
        assert_eq!(mode, Mode::Stage);
        let arm = cli_plan(
            Platform::LinuxArm64,
            PlanAction::ReplaceBinary {
                target: PathBuf::from(CLI),
                asset: "dot-agent-deck-linux-arm64".into(),
            },
        );
        let Finding::Behind { asset, .. } = survey(&host, &arm, &replaced()).unwrap() else {
            panic!("arm64 has a collector asset");
        };
        assert_eq!(asset, "dot-agent-deck-collector-linux-arm64");
    }

    /// No collector beside the CLI: nothing is downloaded or installed, and
    /// the result says so; one elsewhere on PATH is named and left alone.
    #[test]
    fn collector_003_absent_beside_the_cli_installs_nothing() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let source = ReleaseSource {
            api_url: String::new(),
            list_url: String::new(),
            download_base: "http://127.0.0.1:1/download".into(),
        };
        let host = FakeHost::new();
        let report = runtime.block_on(after_upgrade(
            &host,
            &replace_plan(),
            &replaced(),
            &source,
            Path::new("/nonexistent-staging"),
        ));
        assert!(report.ok);
        assert_eq!(
            texts(&report.lines),
            [
                "No resource collector (dot-agent-deck-collector) is in /home/u/.local/bin, so none was upgraded."
            ]
        );
        assert!(host.ran().is_empty(), "{:?}", host.ran());

        let host = collector_at(
            FakeHost::new().on_path("/opt/collector/bin"),
            "/opt/collector/bin/dot-agent-deck-collector",
            "0.40.0",
        );
        let report = runtime.block_on(after_upgrade(
            &host,
            &replace_plan(),
            &replaced(),
            &source,
            Path::new("/nonexistent-staging"),
        ));
        assert!(report.ok);
        let lines = texts(&report.lines);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("No resource collector"), "{lines:?}");
        assert_eq!(
            lines[1],
            "The resource collector first on your PATH, /opt/collector/bin/dot-agent-deck-collector, v0.40.0, is not in the upgraded CLI's folder, so it was left as it is. Upgrade it the way you installed it."
        );
        assert_eq!(
            host.ran(),
            ["/opt/collector/bin/dot-agent-deck-collector --version"],
            "only asked its version"
        );
    }

    /// A collector the CLI's folder only links to, one a system package owns,
    /// one already at the release and one with no release version are left
    /// alone.
    #[cfg(unix)]
    #[test]
    fn collector_004_a_collector_installed_another_way_is_left_alone() {
        let linked = collector_at(
            FakeHost::new(),
            "/home/u/src/deck/target/release/dot-agent-deck-collector",
            "0.49.3",
        )
        .link(
            BESIDE,
            "/home/u/src/deck/target/release/dot-agent-deck-collector",
        );
        let Finding::LeftAlone { why, .. } = survey(&linked, &replace_plan(), &replaced()).unwrap()
        else {
            panic!("a link elsewhere is left alone");
        };
        assert!(why.contains("target/release"), "{why}");

        let mut owned_plan = replace_plan();
        owned_plan.installation.tools.dpkg_query = Some(PathBuf::from("/usr/bin/dpkg-query"));
        let owned = collector_at(FakeHost::new(), BESIDE, "0.49.3")
            .exe("/usr/bin/dpkg-query")
            .answer(
                &format!("/usr/bin/dpkg-query -S {BESIDE}"),
                ok(&format!("telemetry-tools: {BESIDE}\n")),
            );
        assert_eq!(
            survey(&owned, &owned_plan, &replaced()).unwrap(),
            Finding::LeftAlone {
                path: PathBuf::from(BESIDE),
                why: "the system package `telemetry-tools` owns it".into(),
            }
        );

        let current = collector_at(FakeHost::new(), BESIDE, "0.49.5");
        assert_eq!(
            survey(&current, &replace_plan(), &replaced()).unwrap(),
            Finding::NotOlder {
                path: PathBuf::from(BESIDE),
                version: "0.49.5".into(),
            }
        );

        let silent = FakeHost::new()
            .exe(BESIDE)
            .answer(&format!("{BESIDE} --version"), fail("no"));
        assert_eq!(
            survey(&silent, &replace_plan(), &replaced()).unwrap(),
            Finding::NoVersion {
                path: PathBuf::from(BESIDE),
            }
        );
    }

    /// Only an upgrade that changed a downloaded CLI on Linux looks for a
    /// collector: Homebrew's formula carries it, the desktop app installs
    /// none, and macOS has none.
    #[test]
    fn collector_005_only_a_downloaded_linux_cli_has_a_collector_to_upgrade() {
        let host = collector_at(FakeHost::new(), BESIDE, "0.49.3");
        let mac = cli_plan(
            Platform::MacosArm64,
            PlanAction::ReplaceBinary {
                target: PathBuf::from(CLI),
                asset: "dot-agent-deck-darwin-arm64".into(),
            },
        );
        assert_eq!(
            survey(&host, &mac, &replaced()).unwrap(),
            Finding::NotApplicable
        );
        let brewed = Outcome::BrewUpgraded {
            formula: "dot-agent-deck",
            reported: Some("0.49.5".into()),
        };
        assert_eq!(
            survey(&host, &replace_plan(), &brewed).unwrap(),
            Finding::NotApplicable
        );
        let mut desktop = replace_plan();
        desktop.installation.copy = CopyKind::Desktop;
        assert_eq!(
            survey(&host, &desktop, &replaced()).unwrap(),
            Finding::NotApplicable
        );
        assert!(host.ran().is_empty(), "{:?}", host.ran());
        assert_eq!(plan_line(&mac), None);
        assert_eq!(plan_line(&desktop), None);
    }

    /// The plan says, before the user confirms, what happens to a collector
    /// in the same folder.
    #[test]
    fn collector_006_the_plan_says_what_happens_to_the_collector() {
        let replace = plan_line(&replace_plan()).expect("a line for a replaced CLI");
        assert!(
            replace.render().contains("upgraded to v0.49.5 too")
                && replace
                    .render()
                    .contains("systemd user service is restarted"),
            "{replace:?}"
        );
        assert!(replace_plan().items().contains(&replace));
        let staged = plan_line(&staged_plan()).expect("a line for a staged CLI");
        assert!(
            staged.render().contains("command to install it is shown"),
            "{staged:?}"
        );
    }

    /// `systemctl --user show` is read for whether the service runs, and
    /// which file.
    #[test]
    fn collector_007_reads_the_services_state() {
        let running = format!(
            "LoadState=loaded\nActiveState=active\nExecStart={{ path={BESIDE} ; argv[]={BESIDE} ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }}\n"
        );
        assert_eq!(
            parse_service(&running),
            Service::Running(PathBuf::from(BESIDE))
        );
        assert_eq!(
            parse_service(&format!(
                "LoadState=loaded\nActiveState=inactive\nExecStart={{ path={BESIDE} ; }}\n"
            )),
            Service::Stopped
        );
        assert_eq!(
            parse_service("LoadState=not-found\nActiveState=inactive\nExecStart=\n"),
            Service::Unknown
        );
        assert_eq!(parse_service(""), Service::Unknown);
    }

    fn systemd(active: &str, runs: &str) -> FakeHost {
        FakeHost::new()
            .on_path("/usr/bin")
            .exe(SYSTEMCTL)
            .answer(
                &format!(
                    "{SYSTEMCTL} --user show {COLLECTOR_SERVICE} --property=LoadState --property=ActiveState --property=ExecStart"
                ),
                ok(&format!(
                    "LoadState=loaded\nActiveState={active}\nExecStart={{ path={runs} ; argv[]={runs} ; }}\n"
                )),
            )
    }

    /// A user service running the replaced file is restarted; one running
    /// another file, or none, is not, and the result says a running
    /// collector keeps the old version.
    #[cfg(unix)]
    #[test]
    fn collector_008_restarts_only_the_service_that_runs_the_replaced_file() {
        let restart_line = format!("{SYSTEMCTL} --user try-restart {COLLECTOR_SERVICE}");
        let host = systemd("active", BESIDE).answer(&restart_line, ok(""));
        let lines = texts(&restart(&host, Path::new(BESIDE), "0.49.3", "0.49.5"));
        assert_eq!(
            lines,
            [
                "Restarted the dot-agent-deck-collector.service user service, so it now runs v0.49.5."
            ]
        );
        assert!(host.ran().contains(&restart_line), "{:?}", host.ran());
        assert_eq!(host.bound_of(&restart_line), Some(RESTART_TIMEOUT));

        let host = systemd("active", "/opt/other/dot-agent-deck-collector");
        let lines = texts(&restart(&host, Path::new(BESIDE), "0.49.3", "0.49.5"));
        assert!(lines[0].contains("not the upgraded file"), "{lines:?}");
        assert!(lines[1].contains("keeps running v0.49.3"), "{lines:?}");
        assert!(!host.ran().iter().any(|line| line.contains("restart")));

        let host = systemd("inactive", BESIDE);
        let lines = texts(&restart(&host, Path::new(BESIDE), "0.49.3", "0.49.5"));
        assert_eq!(
            lines,
            [
                "A collector that was already running from it keeps running v0.49.3 until you restart it."
            ]
        );
        assert!(!host.ran().iter().any(|line| line.contains("restart")));

        let lines = texts(&restart(
            &FakeHost::new(),
            Path::new(BESIDE),
            "0.49.3",
            "0.49.5",
        ));
        assert!(lines[0].contains("keeps running v0.49.3"), "{lines:?}");

        let host = systemd("active", BESIDE).answer(&restart_line, fail("Access denied"));
        let lines = restart(&host, Path::new(BESIDE), "0.49.3", "0.49.5");
        assert!(
            texts(&lines)[0].contains("failed (Access denied)"),
            "{lines:?}"
        );
        assert_eq!(
            lines.last(),
            Some(&PlanLine::Command(RESTART_COMMAND.to_string()))
        );

        let staged = restart_later(&systemd("active", BESIDE), Path::new(BESIDE), "0.49.3");
        assert_eq!(
            staged.last(),
            Some(&PlanLine::Command(RESTART_COMMAND.to_string()))
        );
    }

    /// A tiny HTTP server answering `GET <path>` with the bytes `files` maps
    /// it to, and 404 otherwise.
    fn serve(files: Vec<(String, Vec<u8>)>) -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                let _ = reader.read_line(&mut request);
                loop {
                    let mut header = String::new();
                    match reader.read_line(&mut header) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if header == "\r\n" => break,
                        Ok(_) => {}
                    }
                }
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let body = files
                    .iter()
                    .find(|(p, _)| p == path)
                    .map(|(_, b)| b.clone());
                let (status, body) = match body {
                    Some(body) => ("200 OK", body),
                    None => ("404 Not Found", Vec::new()),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        format!("http://127.0.0.1:{port}/download")
    }

    /// The whole step against a release served locally: the collector beside
    /// a replaced CLI is downloaded, checked against the manifest and its
    /// `--version`, replaced, and its running user service restarted. A
    /// download that does not match the manifest leaves it as it was and
    /// fails the step.
    #[cfg(unix)]
    #[test]
    fn collector_009_upgrades_and_restarts_a_collector_beside_the_cli() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let staging_root = dir.path().join("upgrade");
        let cli = bin.join(crate::self_upgrade::CLI_BINARY);
        let beside = bin.join(COLLECTOR_BINARY);
        std::fs::write(&beside, b"old collector").unwrap();
        let asset = "dot-agent-deck-collector-linux-amd64";
        let new = b"new collector".to_vec();
        let manifest = format!("{}  {asset}\n", verify::sha256_hex(&new));
        let base = serve(vec![
            (format!("/download/v0.49.5/{asset}"), new.clone()),
            (
                "/download/v0.49.5/checksums.txt".into(),
                manifest.into_bytes(),
            ),
        ]);
        let source = ReleaseSource {
            api_url: String::new(),
            list_url: String::new(),
            download_base: base,
        };
        let mut plan = replace_plan();
        plan.installation.executable = cli.clone();
        plan.action = PlanAction::ReplaceBinary {
            target: cli.clone(),
            asset: "dot-agent-deck-linux-amd64".into(),
        };
        let outcome = Outcome::Replaced {
            path: cli.clone(),
            version: "0.49.5".into(),
            provenance: Provenance::Verified,
            synced: true,
        };
        let beside_str = beside.to_str().unwrap();
        let temp = bin.join(format!(
            ".{COLLECTOR_BINARY}.upgrade-{}",
            std::process::id()
        ));
        let restart_line = format!("{SYSTEMCTL} --user try-restart {COLLECTOR_SERVICE}");
        let host = collector_at(systemd("active", beside_str), beside_str, "0.49.3")
            .exe(temp.to_str().unwrap())
            .answer(
                &format!("{} --version", temp.display()),
                ok("dot-agent-deck-collector 0.49.5\n"),
            )
            .answer(&restart_line, ok(""));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        prepare(&staging_root);
        let report = runtime.block_on(after_upgrade(
            &host,
            &plan,
            &outcome,
            &source,
            &staging_root,
        ));
        let lines = texts(&report.lines);
        assert!(report.ok, "{lines:?}");
        assert_eq!(std::fs::read(&beside).unwrap(), new, "{lines:?}");
        assert_eq!(
            lines[0],
            format!(
                "Upgraded the resource collector at {} from v0.49.3 to v0.49.5, checked the same way.",
                beside.display()
            )
        );
        assert!(lines[1].starts_with("Restarted the"), "{lines:?}");
        assert!(host.ran().contains(&restart_line), "{:?}", host.ran());

        // A download that does not match the manifest changes nothing.
        std::fs::write(&beside, b"old collector").unwrap();
        let bad = serve(vec![
            (format!("/download/v0.49.5/{asset}"), b"tampered".to_vec()),
            (
                "/download/v0.49.5/checksums.txt".into(),
                format!("{}  {asset}\n", verify::sha256_hex(&new)).into_bytes(),
            ),
        ]);
        let source = ReleaseSource {
            download_base: bad,
            ..source
        };
        let report = runtime.block_on(after_upgrade(
            &host,
            &plan,
            &outcome,
            &source,
            &staging_root,
        ));
        let lines = texts(&report.lines);
        assert!(!report.ok, "{lines:?}");
        assert!(lines[0].contains("stays on v0.49.3"), "{lines:?}");
        assert!(
            lines[0].contains("does not match checksums.txt"),
            "{lines:?}"
        );
        // A later upgrade sees the CLI current and does not come back for the
        // collector, so the result says how to replace it by hand.
        assert!(
            lines[1].starts_with("Replace it by hand: download `dot-agent-deck-collector-linux-amd64` from release v0.49.5"),
            "{lines:?}"
        );
        assert_eq!(std::fs::read(&beside).unwrap(), b"old collector");
    }

    /// A staging root an upgrade accepts: a private directory of the user's.
    #[cfg(unix)]
    fn prepare(root: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(root).unwrap();
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}
