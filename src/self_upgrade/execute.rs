//! Carrying out a plan the user confirmed.
//!
//! The network half ([`ReleaseSource`], [`download_verified`]) is async; what
//! it hands on is bytes that already passed [`super::verify`], and the
//! replacing half ([`atomic_replace`], [`swap_app`], …) is synchronous and runs
//! every subprocess through a [`Host`] by absolute path.
//!
//! Every upgrade stages its files in a directory of its own ([`Staging`]):
//! created fresh, with an unpredictable name and mode `0700`, inside a staging
//! root that must be a real directory the user owns and nobody else can write
//! to ([`prepare_staging_root`]). Each staged file is created new, never
//! following a symlink ([`write_new_file`]), and hashed again right before it
//! is handed to whatever installs it ([`verify::rehash`]).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::detect::Installation;
use super::plan::{self, PlanAction, PlanLine, Releases, UpgradePlan};
use super::verify::{self, Provenance, ProvenanceCheck};
use super::{
    CLI_BINARY, CLI_MANIFEST, DESKTOP_APP_BUNDLE, DESKTOP_MANIFEST, Host, INSTALL_TIMEOUT,
    UpgradeError, VERIFY_TIMEOUT, run_checked,
};
use crate::version::ReleaseChannel;

/// The largest asset accepted (the desktop disk image is the largest, at tens
/// of megabytes).
const MAX_DOWNLOAD_BYTES: usize = 512 * 1024 * 1024;

/// Where releases are looked up and downloaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseSource {
    /// A GitHub `releases/latest` API endpoint: the stable channel's lookup.
    pub api_url: String,
    /// A GitHub `releases` list API endpoint: the prerelease channel's lookup
    /// ([`crate::version::ReleaseChannel`]), and where the newest prerelease
    /// is read for a copy on the `dot-agent-deck-beta` formula.
    pub list_url: String,
    /// The base release assets hang off: `<base>/v<version>/<asset>`.
    pub download_base: String,
}

impl ReleaseSource {
    /// This build's repository ([`crate::repo_identity`]).
    ///
    /// Under the `e2e` feature only, `DOT_AGENT_DECK_TEST_RELEASES_API_URL`,
    /// `DOT_AGENT_DECK_TEST_RELEASES_LIST_API_URL` and
    /// `DOT_AGENT_DECK_TEST_RELEASE_DOWNLOAD_BASE` replace them, so an L2 test
    /// can point the real binary at a fake release server. A download URL a
    /// shipped binary reads from its environment would be a way to make it
    /// install something else, so a release build carries no such switch —
    /// the reason `effective_current_exe` in `src/platform/paths.rs` gives
    /// for gating seams on the feature.
    pub fn from_build() -> Self {
        #[allow(unused_mut)]
        let mut source = Self {
            api_url: crate::repo_identity::RELEASES_API_URL.to_string(),
            list_url: crate::repo_identity::RELEASES_LIST_API_URL.to_string(),
            download_base: crate::repo_identity::RELEASE_DOWNLOAD_BASE.to_string(),
        };
        #[cfg(feature = "e2e")]
        {
            if let Ok(url) = std::env::var("DOT_AGENT_DECK_TEST_RELEASES_API_URL") {
                source.api_url = url;
            }
            if let Ok(url) = std::env::var("DOT_AGENT_DECK_TEST_RELEASES_LIST_API_URL") {
                source.list_url = url;
            }
            if let Ok(base) = std::env::var("DOT_AGENT_DECK_TEST_RELEASE_DOWNLOAD_BASE") {
                source.download_base = base;
            }
        }
        source
    }

    /// The releases to plan this machine's copies against, each on its own
    /// channel ([`super::plan::plan`]): the newest stable release whenever a
    /// copy follows the stable channel, and the release list whenever a copy
    /// follows the prerelease channel (a prerelease build, or the
    /// `dot-agent-deck-beta` formula). [`Releases::latest`] is the newest
    /// release on the running copy's channel ([`super::release_channel`]).
    pub async fn releases_for(
        &self,
        running: &Installation,
        other: Option<&Installation>,
    ) -> Result<Releases, UpgradeError> {
        let lookups = Lookups::for_copies(running, other);
        let tags = crate::version::fetch_release_tags(
            &self.api_url,
            &self.list_url,
            lookups.stable,
            lookups.list,
        )
        .await
        .map_err(UpgradeError::ReleaseLookup)?;
        releases_from(super::release_channel(running), &tags)
    }

    pub fn asset_url(&self, version: &str, asset: &str) -> String {
        format!(
            "{}/v{version}/{asset}",
            self.download_base.trim_end_matches('/')
        )
    }
}

/// Which release lookups a machine's copies need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Lookups {
    /// GitHub's `releases/latest`: some copy follows the stable channel.
    stable: bool,
    /// The release list: some copy follows the prerelease channel.
    list: bool,
}

impl Lookups {
    fn for_copies(running: &Installation, other: Option<&Installation>) -> Self {
        let channels: Vec<_> = std::iter::once(running)
            .chain(other)
            .map(super::release_channel)
            .collect();
        Self {
            stable: channels.contains(&ReleaseChannel::Stable),
            list: channels.contains(&ReleaseChannel::Prerelease),
        }
    }
}

/// [`Releases`] from what the lookups found, `latest` being the newest on
/// the running copy's `channel`.
fn releases_from(
    channel: ReleaseChannel,
    tags: &crate::version::ReleaseTags,
) -> Result<Releases, UpgradeError> {
    let latest = match channel {
        ReleaseChannel::Stable => tags.stable.as_deref(),
        ReleaseChannel::Prerelease => tags.newest.as_deref(),
    }
    .ok_or_else(|| UpgradeError::ReleaseLookup("no published release names a version".into()))?;
    Ok(Releases {
        latest: release_version(latest)?,
        stable: tags.stable.as_deref().map(release_version).transpose()?,
        prerelease: tags
            .prerelease
            .as_deref()
            .map(release_version)
            .transpose()?,
    })
}

/// `tag` without its leading `v`, refused unless it is a version.
fn release_version(tag: &str) -> Result<String, UpgradeError> {
    let version = tag.strip_prefix('v').unwrap_or(tag);
    semver::Version::parse(version)
        .map_err(|_| UpgradeError::ReleaseLookup(format!("`{tag}` is not a release version")))?;
    Ok(version.to_string())
}

async fn fetch(url: &str) -> Result<Vec<u8>, UpgradeError> {
    let failed = |detail: String| UpgradeError::Download {
        url: url.to_string(),
        detail,
    };
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(15 * 60))
        .build()
        .map_err(|e| failed(e.to_string()))?;
    let mut response = client
        .get(url)
        .header(
            "User-Agent",
            concat!("dot-agent-deck/", env!("DAD_VERSION")),
        )
        .send()
        .await
        .map_err(|e| failed(e.to_string()))?;
    if !response.status().is_success() {
        return Err(failed(format!("the server answered {}", response.status())));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| failed(e.to_string()))? {
        if bytes.len() + chunk.len() > MAX_DOWNLOAD_BYTES {
            return Err(failed(format!(
                "it is larger than {} MiB",
                MAX_DOWNLOAD_BYTES / 1024 / 1024
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// A downloaded asset that passed its checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    pub bytes: Vec<u8>,
    /// Its SHA-256, as the checksum manifest lists it.
    pub sha256: String,
    pub provenance: Provenance,
}

/// Download `asset` of release `version` and its checksum manifest, verify the
/// manifest's provenance (as `provenance` says the plan promised) and the
/// asset's checksum.
///
/// The manifest is written once, as a new file in the private `staging_dir`,
/// for `gh` to read; the checksums are parsed from the same bytes in memory,
/// and [`verify::verify_provenance`] ties the two together by digest.
pub async fn download_verified(
    host: &dyn Host,
    source: &ReleaseSource,
    version: &str,
    asset: &str,
    provenance: &ProvenanceCheck,
    staging_dir: &Path,
) -> Result<Downloaded, UpgradeError> {
    let manifest_name = if asset.starts_with("dot-agent-deck-desktop-") {
        DESKTOP_MANIFEST
    } else {
        CLI_MANIFEST
    };
    let manifest = fetch(&source.asset_url(version, manifest_name)).await?;
    let manifest_path = staging_dir.join(manifest_name);
    write_new_file(&manifest_path, &manifest, 0o600)?;
    let provenance =
        verify::verify_provenance(host, provenance, &manifest_path, &manifest, version)?;
    let manifest = String::from_utf8_lossy(&manifest);
    let bytes = fetch(&source.asset_url(version, asset)).await?;
    let sha256 = verify::verify_checksum(&manifest, manifest_name, asset, &bytes)?;
    Ok(Downloaded {
        bytes,
        sha256,
        provenance,
    })
}

/// What an upgrade did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The binary at `path` now runs `version`.
    Replaced {
        path: PathBuf,
        version: String,
        provenance: Provenance,
    },
    /// `brew upgrade` ran and the Homebrew binary now reports `reported`,
    /// the offered release.
    BrewUpgraded {
        formula: &'static str,
        reported: Option<String>,
    },
    /// `brew upgrade` ran, but the Homebrew binary does not report `offered`:
    /// it reports `reported`, or nothing. The usual reason is that the tap
    /// does not carry the GitHub release yet. Not an upgrade.
    BrewNotUpgraded {
        formula: &'static str,
        reported: Option<String>,
        offered: String,
    },
    /// The verified file is at `path`; the user runs `command` to install it.
    /// `command` is `None` when it could not be shown safely
    /// ([`plan::install_binary_command`]).
    Staged {
        path: PathBuf,
        command: Option<String>,
        version: String,
        provenance: Provenance,
    },
    /// The package or binary was installed behind a privilege prompt.
    Installed {
        version: String,
        provenance: Provenance,
    },
    /// The app at `app` was replaced; it runs `version` once restarted.
    /// `mount_left` is where the release's disk image is still attached when
    /// detaching it failed.
    AppReplaced {
        app: PathBuf,
        version: String,
        provenance: Provenance,
        mount_left: Option<PathBuf>,
    },
}

impl Outcome {
    /// Whether the copy now has the offered release (or, once staged, will
    /// once the user runs the command). `false` for a `brew upgrade` that
    /// left the copy on another version: nothing was upgraded, so a client
    /// reports it as a failure and offers it again.
    pub fn upgraded(&self) -> bool {
        !matches!(self, Self::BrewNotUpgraded { .. })
    }

    /// What the TUI adds after upgrading the copy it runs from to `version`:
    /// the running process is still the old build until it is restarted. Once
    /// the copy is staged, the user installs it first. `None` for the desktop
    /// app, which is not the TUI the user restarts.
    pub fn tui_restart_line(&self, version: &str) -> Option<PlanLine> {
        let line = match self {
            Self::AppReplaced { .. } | Self::BrewNotUpgraded { .. } => return None,
            Self::Staged { .. } => {
                format!("Once it is installed, quit the TUI and start it again to run v{version}.")
            }
            Self::Replaced { .. } | Self::BrewUpgraded { .. } | Self::Installed { .. } => {
                format!("Quit the TUI and start it again to run v{version}.")
            }
        };
        Some(PlanLine::Text(line))
    }

    /// What the desktop app's notice says once its own copy was installed
    /// with this outcome — the `.dmg` swap, or the `.deb` installed behind the
    /// password prompt: the running app is still the old build until it is
    /// relaunched. `None` for any other outcome.
    pub fn app_relaunch_notice(&self) -> Option<String> {
        match self {
            Self::AppReplaced { version, .. } | Self::Installed { version, .. } => Some(format!(
                "Agent Deck v{version} is installed. Relaunch to run it."
            )),
            _ => None,
        }
    }

    /// What to tell the user, as a terminal prints it ([`Self::items`]).
    pub fn lines(&self) -> Vec<String> {
        plan::render_lines(&self.items())
    }

    /// What to tell the user, line by line, in the same words in every client.
    pub fn items(&self) -> Vec<PlanLine> {
        let text = PlanLine::Text;
        match self {
            Self::Replaced {
                path,
                version,
                provenance,
            } => vec![
                text(format!("Upgraded {} to v{version}.", path.display())),
                text(provenance.message()),
            ],
            Self::BrewUpgraded { formula, reported } => vec![text(match reported {
                Some(version) => {
                    format!("`brew upgrade {formula}` finished; it now reports v{version}.")
                }
                None => format!("`brew upgrade {formula}` finished."),
            })],
            Self::BrewNotUpgraded {
                formula,
                reported: Some(reported),
                offered,
            } => vec![text(format!(
                "`brew upgrade {formula}` finished, but {CLI_BINARY} still reports v{reported}, not v{offered}: Homebrew does not offer v{offered} yet. Try again later."
            ))],
            Self::BrewNotUpgraded {
                formula,
                reported: None,
                offered,
            } => vec![text(format!(
                "`brew upgrade {formula}` finished, but the upgraded {CLI_BINARY} did not report its version, so it is not known to run v{offered}. Check with `{CLI_BINARY} --version`."
            ))],
            Self::Staged {
                command,
                version,
                provenance,
                ..
            } => {
                let mut lines = match command {
                    Some(command) => vec![
                        text("Downloaded and checked. Install it with:".to_string()),
                        PlanLine::Command(command.clone()),
                    ],
                    None => vec![
                        text("Downloaded and checked.".to_string()),
                        text(plan::manual_upgrade_line(version)),
                    ],
                };
                lines.push(text(provenance.message()));
                lines
            }
            Self::Installed {
                version,
                provenance,
            } => vec![
                text(format!("Installed v{version}.")),
                text(provenance.message()),
            ],
            Self::AppReplaced {
                app,
                version,
                provenance,
                mount_left,
            } => {
                let mut lines = vec![
                    text(format!(
                        "Replaced {} with v{version}. Quit and reopen Agent Deck to run it.",
                        app.display()
                    )),
                    text(provenance.message()),
                ];
                if let Some(mount) = mount_left {
                    lines.extend(still_mounted(mount));
                }
                lines
            }
        }
    }
}

/// Carry out `plan`. The caller has shown it and the user confirmed.
/// `staging_root` is where this upgrade creates its private staging
/// directory ([`super::plan::PlanOptions::staging_root`]).
pub async fn execute(
    host: &dyn Host,
    plan: &UpgradePlan,
    source: &ReleaseSource,
    staging_root: &Path,
) -> Result<Outcome, UpgradeError> {
    let version = plan.latest.as_str();
    let platform = plan.installation.platform;
    let provenance = &plan.provenance;
    match &plan.action {
        PlanAction::UpToDate
        | PlanAction::NotifyOnly
        | PlanAction::ShowCommand { .. }
        | PlanAction::ManualDownload { .. } => Err(UpgradeError::NotActionable(plan.text())),
        PlanAction::BrewUpgrade { brew, formula } => {
            run_checked(
                host,
                brew,
                &[OsStr::new("upgrade"), OsStr::new(formula.name())],
                INSTALL_TIMEOUT,
            )?;
            // `brew upgrade` succeeds when the formula is already at the
            // tap's newest, which lags the GitHub release for a while after
            // it is published. Only the copy's own answer says it moved.
            let reported = brew
                .parent()
                .map(|bin| bin.join(CLI_BINARY))
                .and_then(|deck| super::reported_version(host, &deck));
            if reported.as_deref() == Some(version) {
                Ok(Outcome::BrewUpgraded {
                    formula: formula.name(),
                    reported,
                })
            } else {
                Ok(Outcome::BrewNotUpgraded {
                    formula: formula.name(),
                    reported,
                    offered: version.to_string(),
                })
            }
        }
        PlanAction::ReplaceBinary { target, asset } => {
            let staging = Staging::create(staging_root, version)?;
            let downloaded =
                download_verified(host, source, version, asset, provenance, staging.dir()).await?;
            atomic_replace(target, &downloaded.bytes, &downloaded.sha256, |candidate| {
                verify::verify_binary_version(host, candidate, version)
            })?;
            Ok(Outcome::Replaced {
                path: target.clone(),
                version: version.to_string(),
                provenance: downloaded.provenance,
            })
        }
        PlanAction::StagedInstall {
            target,
            asset,
            pkexec,
        } => {
            let mut staging = Staging::create(staging_root, version)?;
            let downloaded =
                download_verified(host, source, version, asset, provenance, staging.dir()).await?;
            let staged = staging.dir().join(asset);
            write_new_file(&staged, &downloaded.bytes, 0o755)?;
            verify::verify_binary_version(host, &staged, version)?;
            let command =
                plan::install_binary_command(platform, &staged, target, &downloaded.sha256);
            match pkexec {
                Some(pkexec) => {
                    install_binary_privileged(
                        host,
                        pkexec,
                        &staged,
                        target,
                        &downloaded.sha256,
                        version,
                        command,
                    )
                    .inspect_err(|e| staging.keep_after(e))?;
                    Ok(Outcome::Installed {
                        version: version.to_string(),
                        provenance: downloaded.provenance,
                    })
                }
                None => {
                    if command.is_some() {
                        staging.keep();
                    }
                    Ok(Outcome::Staged {
                        path: staged,
                        command,
                        version: version.to_string(),
                        provenance: downloaded.provenance,
                    })
                }
            }
        }
        PlanAction::InstallDeb { asset, pkexec } => {
            let mut staging = Staging::create(staging_root, version)?;
            let downloaded =
                download_verified(host, source, version, asset, provenance, staging.dir()).await?;
            let staged = staging.dir().join(asset);
            write_new_file(&staged, &downloaded.bytes, 0o644)?;
            let command = plan::install_deb_command(&staged, &downloaded.sha256);
            match pkexec {
                Some(pkexec) => {
                    verify::rehash(&staged, &downloaded.sha256)?;
                    install_deb(
                        host,
                        pkexec,
                        &staged,
                        &plan.installation.version,
                        version,
                        command,
                    )
                    .inspect_err(|e| staging.keep_after(e))?;
                    Ok(Outcome::Installed {
                        version: version.to_string(),
                        provenance: downloaded.provenance,
                    })
                }
                None => {
                    if command.is_some() {
                        staging.keep();
                    }
                    Ok(Outcome::Staged {
                        path: staged,
                        command,
                        version: version.to_string(),
                        provenance: downloaded.provenance,
                    })
                }
            }
        }
        PlanAction::SwapApp {
            app,
            asset,
            team_id,
        } => {
            let staging = Staging::create(staging_root, version)?;
            let downloaded =
                download_verified(host, source, version, asset, provenance, staging.dir()).await?;
            let dmg = staging.dir().join(asset);
            write_new_file(&dmg, &downloaded.bytes, 0o644)?;
            verify::rehash(&dmg, &downloaded.sha256)?;
            let mount_left = swap_app(host, &dmg, app, team_id, version, staging.dir())?;
            Ok(Outcome::AppReplaced {
                app: app.clone(),
                version: version.to_string(),
                provenance: downloaded.provenance,
                mount_left,
            })
        }
    }
}

/// The release page a user reinstalls from.
fn release_page(version: &str) -> String {
    format!("{}/releases/tag/v{version}", crate::repo_identity::URL)
}

/// The exit codes `pkexec` itself returns when its prompt was dismissed or
/// authorization failed, before it ran anything.
const PROMPT_EXIT_CODES: [i32; 2] = [126, 127];

/// How a command run behind `pkexec` went wrong.
enum Privileged {
    /// The prompt did not authorize it, or `pkexec` could not be run: nothing
    /// was installed.
    Prompt { command: String, detail: String },
    /// It started and did not complete: it failed once past the prompt, or
    /// outlived its bound, and was stopped unless it `may_still_be_running`.
    Unfinished {
        command: String,
        detail: String,
        may_still_be_running: bool,
    },
}

/// Run `pkexec args` for at most [`INSTALL_TIMEOUT`] and say how it failed.
fn run_privileged(host: &dyn Host, pkexec: &Path, args: &[&OsStr]) -> Result<(), Privileged> {
    let command = super::display_command(pkexec, args);
    match host.run_within(pkexec, args, INSTALL_TIMEOUT) {
        Ok(output) if output.success => Ok(()),
        Ok(output) => {
            let detail = match output.stderr.trim() {
                "" => match output.code {
                    Some(code) => format!("exit {code}"),
                    None => "killed by a signal".to_string(),
                },
                stderr => stderr.to_string(),
            };
            if output
                .code
                .is_some_and(|code| PROMPT_EXIT_CODES.contains(&code))
            {
                Err(Privileged::Prompt { command, detail })
            } else {
                Err(Privileged::Unfinished {
                    command,
                    detail,
                    may_still_be_running: false,
                })
            }
        }
        Err(error) => match super::timed_out_stopped(&error) {
            Some(stopped) => Err(Privileged::Unfinished {
                command,
                detail: error.to_string(),
                may_still_be_running: !stopped,
            }),
            None => Err(Privileged::Prompt {
                command,
                detail: error.to_string(),
            }),
        },
    }
}

/// [`run_privileged`]'s failure as the error a client shows. `found` says
/// what is at the target of an install that did not complete, asked only once
/// nothing may still be writing it; `install` is the command that installs
/// the verified file, offered for a failed prompt, and for an unfinished
/// install only when the target is known not to be the new version.
fn privileged_error(
    failure: Privileged,
    target: plan::InstallTarget,
    found: impl FnOnce() -> plan::Found,
    install: Option<String>,
    version: &str,
) -> UpgradeError {
    match failure {
        Privileged::Prompt { command, detail } => UpgradeError::PrivilegeFailed {
            command,
            detail,
            install,
            version: version.to_string(),
        },
        Privileged::Unfinished {
            command,
            detail,
            may_still_be_running,
        } => {
            let found = if may_still_be_running {
                plan::Found::NotChecked
            } else {
                found()
            };
            UpgradeError::InstallUnfinished(Box::new(super::UnfinishedInstall {
                command,
                detail,
                may_still_be_running,
                target,
                install: install.filter(|_| found.known_not_new()),
                found,
                version: version.to_string(),
            }))
        }
    }
}

/// What is at `target` after an install of the build hashing to `sha256`
/// stopped, `previous` being the target's hash before it started.
fn found_binary(target: &Path, previous: Option<&str>, sha256: &str) -> plan::Found {
    match verify::file_sha256(target) {
        Ok(actual) if actual == sha256 => plan::Found::New,
        Ok(actual) if Some(actual.as_str()) == previous => plan::Found::Previous,
        Ok(_) => plan::Found::Neither,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => plan::Found::Missing,
        Err(e) => plan::Found::Unreadable(e.to_string()),
    }
}

/// What `dpkg-query` says of the desktop package after an install from
/// `previous` to `version` stopped.
fn found_package(host: &dyn Host, previous: &str, version: &str) -> plan::Found {
    let output = host.run(
        Path::new(DPKG_QUERY),
        &[
            OsStr::new("-W"),
            OsStr::new("-f=${Version} ${db:Status-Abbrev}"),
            OsStr::new(super::DESKTOP_DEB_PACKAGE),
        ],
    );
    let output = match output {
        Ok(output) if output.success => output,
        Ok(output) => {
            let why = match output.stderr.trim() {
                "" => "`dpkg-query` did not answer".to_string(),
                stderr => stderr.to_string(),
            };
            return plan::Found::Unreadable(why);
        }
        Err(e) => return plan::Found::Unreadable(e.to_string()),
    };
    let mut words = output.stdout.split_whitespace();
    let (installed, status) = (words.next(), words.next());
    match (installed, status) {
        (Some(installed), Some("ii")) if installed == version => plan::Found::New,
        (Some(installed), Some("ii")) if installed == previous => plan::Found::Previous,
        _ => plan::Found::Neither,
    }
}

const DPKG_QUERY: &str = "/usr/bin/dpkg-query";

/// Install the verified binary at `staged` over `target` behind `pkexec`.
///
/// `staged` is hashed again right before `pkexec` gets it. Once installed,
/// `target` (root-owned now, so no longer the user's to change) is hashed
/// against the verified digest BEFORE it is run for its `--version`: a
/// mismatch means what root installed is not the verified build, which is
/// reported as such and never executed. `fallback` is the command the user
/// runs when the prompt itself fails, or when an install that did not
/// complete left a target known not to be the new build.
///
/// A failure `pkexec` reports for its prompt is
/// [`UpgradeError::PrivilegeFailed`]. Any other failure, a timeout included,
/// is [`UpgradeError::InstallUnfinished`]: once the target is no longer being
/// written, it is hashed against both the verified digest and its own digest
/// from before the install, so the result can say which it holds.
pub fn install_binary_privileged(
    host: &dyn Host,
    pkexec: &Path,
    staged: &Path,
    target: &Path,
    sha256: &str,
    version: &str,
    fallback: Option<String>,
) -> Result<(), UpgradeError> {
    verify::rehash(staged, sha256)?;
    let previous = verify::file_sha256(target).ok();
    run_privileged(
        host,
        pkexec,
        &[
            OsStr::new("/usr/bin/install"),
            OsStr::new("-m"),
            OsStr::new("0755"),
            staged.as_os_str(),
            target.as_os_str(),
        ],
    )
    .map_err(|failure| {
        privileged_error(
            failure,
            plan::InstallTarget::Binary(target.to_path_buf()),
            || found_binary(target, previous.as_deref(), sha256),
            fallback,
            version,
        )
    })?;
    let actual = verify::file_sha256(target).unwrap_or_else(|e| format!("unreadable: {e}"));
    if actual != sha256 {
        return Err(UpgradeError::InstalledMismatch {
            target: target.display().to_string(),
            expected: sha256.to_string(),
            actual,
            release: release_page(version),
        });
    }
    verify::verify_binary_version(host, target, version)
}

/// The private directory one upgrade stages its files in. Removed when
/// dropped, unless kept for a command the user still has to run.
pub struct Staging {
    dir: PathBuf,
    keep: bool,
}

impl Staging {
    /// Create a fresh staging directory for release `version` under `root`:
    /// `v<version>-<random>`, mode `0700`, created exclusively, so two
    /// upgrades never share one and nobody can prepare it in advance. `root`
    /// must pass [`prepare_staging_root`].
    pub fn create(root: &Path, version: &str) -> Result<Self, UpgradeError> {
        prepare_staging_root(root)?;
        for _ in 0..16 {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes)
                .map_err(|e| UpgradeError::Io(format!("OS randomness is unavailable: {e}")))?;
            let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            let dir = root.join(format!("v{version}-{suffix}"));
            match create_dir_private(&dir) {
                Ok(()) => return Ok(Self { dir, keep: false }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(UpgradeError::Io(format!(
            "Cannot create a staging directory in {}.",
            root.display()
        )))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Leave the directory in place: the user still installs from it.
    pub fn keep(&mut self) {
        self.keep = true;
    }

    /// Keep the directory when `error` hands the user a command that installs
    /// from it, or comes from an install that may still be reading it.
    fn keep_after(&mut self, error: &UpgradeError) {
        let keep = match error {
            UpgradeError::PrivilegeFailed { install, .. } => install.is_some(),
            UpgradeError::InstallUnfinished(unfinished) => {
                unfinished.install.is_some() || unfinished.may_still_be_running
            }
            _ => false,
        };
        if keep {
            self.keep();
        }
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// Make sure `root` can hold private staging directories: create it (mode
/// `0700`) when missing, then refuse it unless it is a real directory — not a
/// symlink — owned by the current user and not writable by group or others.
/// Anything else lets another user prepare or replace what is staged in it.
pub fn prepare_staging_root(root: &Path) -> Result<(), UpgradeError> {
    let unsafe_root = |why: &str| UpgradeError::StagingUnsafe {
        root: root.display().to_string(),
        why: why.to_string(),
    };
    if let Some(parent) = root.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match create_dir_private(root) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let meta = std::fs::symlink_metadata(root)?;
    if meta.file_type().is_symlink() {
        return Err(unsafe_root("it is a symbolic link"));
    }
    if !meta.is_dir() {
        return Err(unsafe_root("it is not a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        if meta.uid() != me {
            return Err(unsafe_root("it belongs to another user"));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(unsafe_root("other users can write to it"));
        }
    }
    Ok(())
}

/// Create the one directory `dir` (not its parents), private to the user.
fn create_dir_private(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir(dir)
}

/// Create `dir` and any missing parents, private to the user.
fn create_private_dir_all(dir: &Path) -> Result<(), UpgradeError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// Write `bytes` to a NEW file at `path` with `mode`. It fails rather than
/// write through anything already there, a symlink included (`O_EXCL`, and
/// `O_NOFOLLOW` on Unix).
pub fn write_new_file(path: &Path, bytes: &[u8], mode: u32) -> Result<(), UpgradeError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    std::io::Write::write_all(&mut file, bytes)?;
    set_mode(&file, mode)?;
    file.sync_all()?;
    Ok(())
}

/// Open `path` for reading without following a symlink in its last component.
pub(crate) fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

#[cfg(unix)]
fn set_mode(file: &std::fs::File, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_file: &std::fs::File, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// Replace `target` with `bytes` atomically: write a temporary file in the
/// same directory, fsync it, run `check` on it (the new binary's `--version`),
/// hash it again against `sha256`, and only then rename it over `target` and
/// fsync the directory. On any failure the temporary file is removed and
/// `target` is left as it was.
///
/// The replaced binary's mode is not preserved: the new file is always
/// written `0o755`, whatever `target`'s mode was.
pub fn atomic_replace(
    target: &Path,
    bytes: &[u8],
    sha256: &str,
    check: impl FnOnce(&Path) -> Result<(), UpgradeError>,
) -> Result<(), UpgradeError> {
    let dir = target
        .parent()
        .ok_or_else(|| UpgradeError::Io(format!("{} has no directory", target.display())))?;
    let name = target
        .file_name()
        .map_or_else(|| CLI_BINARY.into(), |n| n.to_string_lossy().into_owned());
    let temp = dir.join(format!(".{name}.upgrade-{}", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    let result = (|| {
        write_new_file(&temp, bytes, 0o755)?;
        check(&temp)?;
        verify::rehash(&temp, sha256)?;
        std::fs::rename(&temp, target)?;
        #[cfg(unix)]
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Install a verified `.deb` behind `pkexec`, upgrading the package from
/// `previous` to `version`. `fallback` is the command the user runs when the
/// prompt itself fails. A failure past the prompt is classified as
/// [`install_binary_privileged`]'s is, the package's state read from
/// `dpkg-query` in place of a hash.
pub fn install_deb(
    host: &dyn Host,
    pkexec: &Path,
    deb: &Path,
    previous: &str,
    version: &str,
    fallback: Option<String>,
) -> Result<(), UpgradeError> {
    run_privileged(
        host,
        pkexec,
        &[
            OsStr::new("/usr/bin/apt-get"),
            OsStr::new("install"),
            OsStr::new("-y"),
            deb.as_os_str(),
        ],
    )
    .map_err(|failure| {
        privileged_error(
            failure,
            plan::InstallTarget::Package,
            || found_package(host, previous, version),
            fallback,
            version,
        )
    })
}

const HDIUTIL: &str = "/usr/bin/hdiutil";
const CODESIGN: &str = "/usr/bin/codesign";
const SPCTL: &str = "/usr/sbin/spctl";
const DITTO: &str = "/usr/bin/ditto";

/// The checks an app bundle must pass before it replaces the running one: a
/// valid deep signature, the running app's Team ID, and (with `assess`)
/// Gatekeeper's acceptance, which is what notarization buys.
fn check_app(host: &dyn Host, app: &Path, team_id: &str, assess: bool) -> Result<(), UpgradeError> {
    let failed = |e: UpgradeError| UpgradeError::AppCheckFailed(e.to_string());
    run_checked(
        host,
        Path::new(CODESIGN),
        &[
            OsStr::new("--verify"),
            OsStr::new("--deep"),
            OsStr::new("--strict"),
            app.as_os_str(),
        ],
        VERIFY_TIMEOUT,
    )
    .map_err(failed)?;
    match super::detect::team_id(host, app) {
        Some(found) if found == team_id => {}
        found => {
            return Err(UpgradeError::AppCheckFailed(format!(
                "it is signed by Team ID {}, not {team_id} like the running app",
                found.as_deref().unwrap_or("(none)")
            )));
        }
    }
    if assess {
        run_checked(
            host,
            Path::new(SPCTL),
            &[
                OsStr::new("--assess"),
                OsStr::new("--type"),
                OsStr::new("execute"),
                app.as_os_str(),
            ],
            VERIFY_TIMEOUT,
        )
        .map_err(failed)?;
    }
    Ok(())
}

/// What the user reads when the release's disk image is still attached at a
/// mount point whose path cannot be shown faithfully, before the command
/// that lists the attached images.
pub const STILL_MOUNTED_UNSHOWABLE: &str = "Warning: the release's disk image is still attached, at a folder whose path cannot be shown safely. List the attached images with the command below, then detach this release's with `hdiutil detach -force` and its mount point:";

/// The lines that tell the user the release's disk image is still attached
/// at `mount`, and how to detach it. The detach command is built only for a
/// path it can name faithfully ([`plan::detach_image_command`]); for any
/// other, the user is told how to find the image instead.
pub(crate) fn still_mounted(mount: &Path) -> Vec<PlanLine> {
    match plan::detach_image_command(Path::new(HDIUTIL), mount) {
        Some(command) => vec![
            PlanLine::Text(format!(
                "Warning: the release's disk image is still attached at {}. Detach it with:",
                mount.display()
            )),
            PlanLine::Command(command),
        ],
        None => vec![
            PlanLine::Text(STILL_MOUNTED_UNSHOWABLE.to_string()),
            PlanLine::Command(format!("{HDIUTIL} info")),
        ],
    }
}

/// Whether something is at `mount`, the empty folder an attach was given: a
/// file system mounted on it, or anything in it.
fn occupied(mount: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Some(here), Some(parent)) = (
            std::fs::metadata(mount).ok(),
            mount
                .parent()
                .and_then(|parent| std::fs::metadata(parent).ok()),
        ) && here.dev() != parent.dev()
        {
            return true;
        }
    }
    std::fs::read_dir(mount).is_ok_and(|mut entries| entries.next().is_some())
}

/// A disk image attached at `mount`, detached when this is dropped — on an
/// early return or a panic as much as at the end — unless [`Self::detach`]
/// already did. It is held from before `hdiutil attach` runs, so an attach
/// that mounted the image and then failed, timed out or panicked is cleaned
/// up too: a detach runs only when something is at the mount point, which
/// this upgrade created empty in its private staging directory.
struct Attached<'a> {
    host: &'a dyn Host,
    mount: PathBuf,
    attached: bool,
}

impl<'a> Attached<'a> {
    /// Attach `dmg` read-only at `mount`. When the attach fails, whatever it
    /// left at `mount` is detached; when that cannot be, the error is
    /// [`UpgradeError::StillMounted`].
    fn attach(host: &'a dyn Host, dmg: &Path, mount: &Path) -> Result<Self, UpgradeError> {
        let mut attached = Self {
            host,
            mount: mount.to_path_buf(),
            attached: true,
        };
        let result = run_checked(
            host,
            Path::new(HDIUTIL),
            &[
                OsStr::new("attach"),
                OsStr::new("-readonly"),
                OsStr::new("-nobrowse"),
                OsStr::new("-noautoopen"),
                OsStr::new("-mountpoint"),
                mount.as_os_str(),
                dmg.as_os_str(),
            ],
            INSTALL_TIMEOUT,
        );
        match result {
            Ok(_) => Ok(attached),
            Err(error) if attached.detach() => Err(error),
            Err(error) => {
                // Reported in the error; dropping the guard would only try
                // the same detach again.
                attached.attached = false;
                Err(UpgradeError::StillMounted {
                    error: Box::new(error),
                    mount: mount.to_path_buf(),
                })
            }
        }
    }

    /// Detach the image, forcing it when a plain detach fails. Returns
    /// whether it is detached; with nothing at the mount point there is
    /// nothing to detach.
    fn detach(&mut self) -> bool {
        if self.attached && !occupied(&self.mount) {
            self.attached = false;
        }
        if self.attached {
            let detach = |extra: &[&OsStr]| {
                let mut args = vec![OsStr::new("detach"), self.mount.as_os_str()];
                args.extend_from_slice(extra);
                self.host
                    .run(Path::new(HDIUTIL), &args)
                    .is_ok_and(|out| out.success)
            };
            self.attached = !(detach(&[]) || detach(&[OsStr::new("-force")]));
        }
        !self.attached
    }
}

impl Drop for Attached<'_> {
    fn drop(&mut self) {
        if !self.detach() {
            tracing::warn!(
                mount = %self.mount.display(),
                "the release's disk image could not be detached"
            );
        }
    }
}

/// Replace the app bundle at `app` with the one inside the verified `dmg`.
///
/// The image is attached read-only under `work_dir`; the app in it must pass
/// [`check_app`] and its bundled CLI must report `version`. It is copied next to
/// `app` with `ditto`, checked again, and swapped in by two renames, the second
/// of which is rolled back if it fails. A detach is attempted on every path
/// that returns or unwinds, an attach that failed after mounting included
/// ([`Attached`]). When it cannot be, a successful swap returns where it is
/// still attached, and a failed one says so in its error
/// ([`UpgradeError::StillMounted`]).
pub fn swap_app(
    host: &dyn Host,
    dmg: &Path,
    app: &Path,
    team_id: &str,
    version: &str,
    work_dir: &Path,
) -> Result<Option<PathBuf>, UpgradeError> {
    let mount = work_dir.join("mount");
    create_private_dir_all(&mount)?;
    let mut attached = Attached::attach(host, dmg, &mount)?;
    let result = swap_from_mount(host, &mount, app, team_id, version);
    let detached = attached.detach();
    match result {
        Ok(()) => Ok((!detached).then_some(mount)),
        Err(error) if detached => Err(error),
        Err(error) => Err(UpgradeError::StillMounted {
            error: Box::new(error),
            mount,
        }),
    }
}

fn swap_from_mount(
    host: &dyn Host,
    mount: &Path,
    app: &Path,
    team_id: &str,
    version: &str,
) -> Result<(), UpgradeError> {
    let new_app = mount.join(DESKTOP_APP_BUNDLE);
    check_app(host, &new_app, team_id, true)?;
    verify::verify_binary_version(
        host,
        &new_app.join("Contents/MacOS").join(CLI_BINARY),
        version,
    )
    .map_err(|e| UpgradeError::AppCheckFailed(e.to_string()))?;
    let parent = app
        .parent()
        .ok_or_else(|| UpgradeError::Io(format!("{} has no folder", app.display())))?;
    let pid = std::process::id();
    let incoming = parent.join(format!(".{DESKTOP_APP_BUNDLE}.upgrade-{pid}"));
    let outgoing = parent.join(format!(".{DESKTOP_APP_BUNDLE}.old-{pid}"));
    let _ = std::fs::remove_dir_all(&incoming);
    // The copy is checked with `assess = false` on purpose. The image is
    // attached read-only, so what `ditto` copies is the bundle Gatekeeper just
    // assessed on the mounted side, byte for byte; the copy is still checked
    // again for a valid deep signature and the running app's Team ID, which
    // is what would catch a copy that came out different. So the Gatekeeper
    // assessment belongs on the mounted side: do not drop it there, and do not
    // "fix" this call to assess again.
    let copied = run_checked(
        host,
        Path::new(DITTO),
        &[new_app.as_os_str(), incoming.as_os_str()],
        INSTALL_TIMEOUT,
    )
    .and_then(|_| check_app(host, &incoming, team_id, false));
    if let Err(e) = copied {
        let _ = std::fs::remove_dir_all(&incoming);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(app, &outgoing) {
        let _ = std::fs::remove_dir_all(&incoming);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&incoming, app) {
        let _ = std::fs::rename(&outgoing, app);
        let _ = std::fs::remove_dir_all(&incoming);
        return Err(e.into());
    }
    let _ = std::fs::remove_dir_all(&outgoing);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_upgrade::CommandOutput;
    use crate::self_upgrade::detect::InstallMethod;
    use crate::self_upgrade::test_host::{FakeHost, fail, ok};

    #[test]
    fn execute_001_atomic_replace_swaps_content_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dot-agent-deck");
        std::fs::write(&target, b"old").unwrap();
        let mut checked = None;
        atomic_replace(&target, b"new", &verify::sha256_hex(b"new"), |candidate| {
            assert_eq!(std::fs::read(candidate).unwrap(), b"new");
            assert_eq!(
                candidate.parent(),
                target.parent(),
                "the temp file is in the same directory"
            );
            checked = Some(candidate.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!checked.unwrap().exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn execute_002_atomic_replace_keeps_old_binary_when_check_fails() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dot-agent-deck");
        std::fs::write(&target, b"old").unwrap();
        let err = atomic_replace(&target, b"new", &verify::sha256_hex(b"new"), |_| {
            Err(UpgradeError::VersionMismatch {
                expected: "0.46.0".into(),
                actual: "v0.45.0".into(),
            })
        })
        .unwrap_err();
        assert!(matches!(err, UpgradeError::VersionMismatch { .. }));
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn execute_003_install_deb_runs_apt_behind_pkexec() {
        let host = FakeHost::new().exe("/usr/bin/pkexec").answer(
            "/usr/bin/pkexec /usr/bin/apt-get install -y /s/x.deb",
            ok(""),
        );
        install_deb(
            &host,
            Path::new("/usr/bin/pkexec"),
            Path::new("/s/x.deb"),
            "0.45.0",
            "0.46.0",
            None,
        )
        .unwrap();
        assert_eq!(
            host.ran(),
            ["/usr/bin/pkexec /usr/bin/apt-get install -y /s/x.deb"]
        );

        let host = FakeHost::new().exe("/usr/bin/pkexec").answer(
            "/usr/bin/pkexec /usr/bin/apt-get install -y /s/x.deb",
            fail("Request dismissed"),
        );
        let err = install_deb(
            &host,
            Path::new("/usr/bin/pkexec"),
            Path::new("/s/x.deb"),
            "0.45.0",
            "0.46.0",
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Request dismissed"), "{err}");
    }

    /// A fake macOS: `hdiutil attach` populates the mount point with an app,
    /// `ditto` copies a directory, `codesign -dv` reports `team` for any app.
    fn fake_mac(team: &'static str, spctl_ok: bool) -> FakeHost {
        FakeHost::new()
            .handle(HDIUTIL, |args| {
                if args[0] == "attach" {
                    let mount = PathBuf::from(&args[5]);
                    let macos = mount.join(DESKTOP_APP_BUNDLE).join("Contents/MacOS");
                    std::fs::create_dir_all(&macos).unwrap();
                    std::fs::write(macos.join(CLI_BINARY), b"new").unwrap();
                }
                ok("")
            })
            .handle(DITTO, |args| {
                let (from, to) = (PathBuf::from(&args[0]), PathBuf::from(&args[1]));
                let macos = to.join("Contents/MacOS");
                std::fs::create_dir_all(&macos).unwrap();
                std::fs::copy(
                    from.join("Contents/MacOS").join(CLI_BINARY),
                    macos.join(CLI_BINARY),
                )
                .unwrap();
                ok("")
            })
            .handle(CODESIGN, move |args| {
                if args[0] == "-dv" {
                    CommandOutput {
                        success: true,
                        code: Some(0),
                        stdout: String::new(),
                        stderr: format!("TeamIdentifier={team}\n"),
                    }
                } else {
                    ok("")
                }
            })
            .handle(
                SPCTL,
                move |_| if spctl_ok { ok("") } else { fail("rejected") },
            )
    }

    fn installed_app(root: &Path) -> PathBuf {
        let app = root.join("Applications").join(DESKTOP_APP_BUNDLE);
        let macos = app.join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::write(macos.join(CLI_BINARY), b"old").unwrap();
        app
    }

    fn with_version_answer(host: FakeHost, mount: &Path) -> FakeHost {
        let bundled = mount
            .join(DESKTOP_APP_BUNDLE)
            .join("Contents/MacOS")
            .join(CLI_BINARY);
        host.answer(
            &format!("{} --version", bundled.display()),
            ok("dot-agent-deck 0.46.0\n"),
        )
    }

    #[test]
    fn execute_004_swap_app_replaces_bundle_after_checks() {
        let root = tempfile::tempdir().unwrap();
        let app = installed_app(root.path());
        let work = root.path().join("work");
        let host = with_version_answer(fake_mac("TEAM123", true), &work.join("mount"));
        swap_app(
            &host,
            Path::new("/s/x.dmg"),
            &app,
            "TEAM123",
            "0.46.0",
            &work,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(app.join("Contents/MacOS").join(CLI_BINARY)).unwrap(),
            b"new"
        );
        let leftovers: Vec<_> = std::fs::read_dir(app.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            leftovers,
            [DESKTOP_APP_BUNDLE],
            "no staging copies are left"
        );
        assert!(
            host.ran()
                .iter()
                .any(|line| line.starts_with(&format!("{HDIUTIL} detach"))),
            "the image is detached"
        );
    }

    /// [`fake_mac`] whose `hdiutil detach` succeeds only as the listed kinds:
    /// `plain` without `-force`, `force` with it.
    fn fake_mac_detaching(spctl_ok: bool, plain: bool, force: bool) -> FakeHost {
        let mut host = fake_mac("TEAM123", spctl_ok);
        let attach = host.handlers.remove(HDIUTIL).unwrap();
        host.handle_io(HDIUTIL, move |args| {
            if args[0] == "detach" {
                let forced = args.iter().any(|arg| arg == "-force");
                Ok(if (forced && force) || (!forced && plain) {
                    ok("")
                } else {
                    fail("hdiutil: couldn't unmount \"disk4\" - Resource busy")
                })
            } else {
                attach(args)
            }
        })
    }

    // Unix paths the fake host answers for: native Windows is unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn execute_023_an_image_that_will_not_detach_is_named_in_the_outcome() {
        let root = tempfile::tempdir().unwrap();
        let app = installed_app(root.path());
        let work = root.path().join("work");
        let mount = work.join("mount");

        // A forced detach still works: nothing to report.
        let host = with_version_answer(fake_mac_detaching(true, false, true), &mount);
        assert_eq!(
            swap_app(
                &host,
                Path::new("/s/x.dmg"),
                &app,
                "TEAM123",
                "0.46.0",
                &work
            ),
            Ok(None)
        );
        assert!(
            host.ran()
                .contains(&format!("{HDIUTIL} detach {} -force", mount.display())),
            "{:?}",
            host.ran()
        );

        // Neither works: the swap still happened, and the outcome says where
        // the image is still attached and how to detach it.
        let root = tempfile::tempdir().unwrap();
        let app = installed_app(root.path());
        let work = root.path().join("work");
        let mount = work.join("mount");
        let host = with_version_answer(fake_mac_detaching(true, false, false), &mount);
        let left = swap_app(
            &host,
            Path::new("/s/x.dmg"),
            &app,
            "TEAM123",
            "0.46.0",
            &work,
        )
        .unwrap();
        assert_eq!(left.as_deref(), Some(mount.as_path()));
        let outcome = Outcome::AppReplaced {
            app: app.clone(),
            version: "0.46.0".into(),
            provenance: Provenance::Verified,
            mount_left: left,
        };
        let lines = outcome.lines();
        assert_eq!(
            lines[2],
            format!(
                "Warning: the release's disk image is still attached at {}. Detach it with:",
                mount.display()
            )
        );
        assert_eq!(
            lines[3],
            format!("  {HDIUTIL} detach -force {}", mount.display())
        );
        assert!(outcome.upgraded());
    }

    #[test]
    fn execute_024_a_failed_swap_that_leaves_the_image_attached_says_so() {
        let root = tempfile::tempdir().unwrap();
        let app = installed_app(root.path());
        let work = root.path().join("work");
        let mount = work.join("mount");
        // Gatekeeper refuses the new app, and the image will not detach.
        let host = with_version_answer(fake_mac_detaching(false, false, false), &mount);
        let err = swap_app(
            &host,
            Path::new("/s/x.dmg"),
            &app,
            "TEAM123",
            "0.46.0",
            &work,
        )
        .unwrap_err();
        assert!(
            matches!(&err, UpgradeError::StillMounted { error, .. } if matches!(**error, UpgradeError::AppCheckFailed(_))),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("The new Agent Deck app failed a check"),
            "{err}"
        );
        let fallback = plan::render_lines(&err.fallback()).join("\n");
        assert!(
            fallback.contains(&format!("still attached at {}", mount.display())),
            "{fallback}"
        );
        assert_eq!(
            std::fs::read(app.join("Contents/MacOS").join(CLI_BINARY)).unwrap(),
            b"old"
        );
    }

    #[test]
    fn execute_025_the_image_is_detached_even_when_the_swap_panics() {
        let root = tempfile::tempdir().unwrap();
        let app = installed_app(root.path());
        let work = root.path().join("work");
        let host = fake_mac("TEAM123", true).handle(CODESIGN, |_| panic!("codesign blew up"));
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            swap_app(
                &host,
                Path::new("/s/x.dmg"),
                &app,
                "TEAM123",
                "0.46.0",
                &work,
            )
        }));
        assert!(caught.is_err());
        assert!(
            host.ran()
                .iter()
                .any(|line| line.starts_with(&format!("{HDIUTIL} detach"))),
            "{:?}",
            host.ran()
        );
    }

    #[test]
    fn execute_005_swap_app_refuses_another_team_id() {
        let root = tempfile::tempdir().unwrap();
        let app = installed_app(root.path());
        let work = root.path().join("work");
        let host = with_version_answer(fake_mac("SOMEONEELSE", true), &work.join("mount"));
        let err = swap_app(
            &host,
            Path::new("/s/x.dmg"),
            &app,
            "TEAM123",
            "0.46.0",
            &work,
        )
        .unwrap_err();
        assert!(err.to_string().contains("SOMEONEELSE"), "{err}");
        assert_eq!(
            std::fs::read(app.join("Contents/MacOS").join(CLI_BINARY)).unwrap(),
            b"old"
        );
        assert!(host.ran().iter().any(|line| line.contains("detach")));
    }

    #[test]
    fn execute_020_each_command_gets_its_bound() {
        use crate::self_upgrade::PROBE_TIMEOUT;
        let root = tempfile::tempdir().unwrap();
        let app = installed_app(root.path());
        let work = root.path().join("work");
        let host = with_version_answer(fake_mac("TEAM123", true), &work.join("mount"));
        swap_app(
            &host,
            Path::new("/s/x.dmg"),
            &app,
            "TEAM123",
            "0.46.0",
            &work,
        )
        .unwrap();
        for (prefix, bound) in [
            (format!("{HDIUTIL} attach"), INSTALL_TIMEOUT),
            (format!("{CODESIGN} --verify"), VERIFY_TIMEOUT),
            (format!("{CODESIGN} -dv"), PROBE_TIMEOUT),
            (format!("{SPCTL} --assess"), VERIFY_TIMEOUT),
            (format!("{DITTO} "), INSTALL_TIMEOUT),
            (format!("{HDIUTIL} detach"), PROBE_TIMEOUT),
        ] {
            assert_eq!(host.bound_of(&prefix), Some(bound), "{prefix}");
        }
        let bundled = work
            .join("mount")
            .join(DESKTOP_APP_BUNDLE)
            .join("Contents/MacOS")
            .join(CLI_BINARY);
        assert_eq!(
            host.bound_of(&format!("{} --version", bundled.display())),
            Some(PROBE_TIMEOUT)
        );

        let host = FakeHost::new().exe(PKEXEC).handle(PKEXEC, |_| ok(""));
        install_deb(
            &host,
            Path::new(PKEXEC),
            Path::new("/s/x.deb"),
            "0.45.0",
            "0.46.0",
            None,
        )
        .unwrap();
        assert_eq!(host.bound_of(PKEXEC), Some(INSTALL_TIMEOUT));
    }

    #[test]
    fn execute_006_swap_app_refuses_what_gatekeeper_rejects() {
        let root = tempfile::tempdir().unwrap();
        let app = installed_app(root.path());
        let work = root.path().join("work");
        let host = with_version_answer(fake_mac("TEAM123", false), &work.join("mount"));
        assert!(matches!(
            swap_app(
                &host,
                Path::new("/s/x.dmg"),
                &app,
                "TEAM123",
                "0.46.0",
                &work
            ),
            Err(UpgradeError::AppCheckFailed(_))
        ));
        assert_eq!(
            std::fs::read(app.join("Contents/MacOS").join(CLI_BINARY)).unwrap(),
            b"old"
        );
    }

    #[test]
    fn execute_007_release_source_urls() {
        let source = ReleaseSource {
            api_url: "http://127.0.0.1:1/latest".into(),
            list_url: "http://127.0.0.1:1/releases".into(),
            download_base: "http://127.0.0.1:1/download/".into(),
        };
        assert_eq!(
            source.asset_url("0.46.0", "checksums.txt"),
            "http://127.0.0.1:1/download/v0.46.0/checksums.txt"
        );
        let production = ReleaseSource::from_build();
        if !cfg!(feature = "e2e") {
            assert_eq!(production.api_url, crate::repo_identity::RELEASES_API_URL);
            assert_eq!(
                production.list_url,
                crate::repo_identity::RELEASES_LIST_API_URL
            );
        }
    }

    fn copy_at(version: &str, method: InstallMethod) -> Installation {
        Installation {
            copy: crate::self_upgrade::CopyKind::Cli,
            executable: PathBuf::from("/x/dot-agent-deck"),
            version: version.into(),
            platform: Some(crate::self_upgrade::Platform::LinuxAmd64),
            method,
            tools: crate::self_upgrade::detect::Tools::default(),
        }
    }

    #[test]
    fn execute_021_each_copy_is_looked_up_on_its_own_channel() {
        use crate::self_upgrade::HomebrewFormula;
        let downloaded = || InstallMethod::DownloadedWritable {
            binary: PathBuf::from("/x/dot-agent-deck"),
        };
        let formula = |formula| InstallMethod::Homebrew {
            formula,
            prefix: PathBuf::from("/opt/homebrew"),
        };
        let prerelease_app = copy_at("0.48.0-beta.1", downloaded());
        let stable_app = copy_at("0.47.0", downloaded());
        let stable_formula = copy_at("0.46.0", formula(HomebrewFormula::Stable));
        let beta_formula = copy_at("0.48.0-beta.0", formula(HomebrewFormula::Beta));
        let both = Lookups {
            stable: true,
            list: true,
        };
        // A stable formula beside a prerelease app, and the mirror.
        assert_eq!(
            Lookups::for_copies(&prerelease_app, Some(&stable_formula)),
            both
        );
        assert_eq!(Lookups::for_copies(&stable_app, Some(&beta_formula)), both);
        assert_eq!(
            Lookups::for_copies(&stable_app, Some(&stable_formula)),
            Lookups {
                stable: true,
                list: false
            }
        );
        assert_eq!(
            Lookups::for_copies(&prerelease_app, None),
            Lookups {
                stable: false,
                list: true
            }
        );

        // `latest` follows the running copy's channel; the stable release is
        // carried for a stable copy beside it.
        let tags = crate::version::ReleaseTags {
            stable: Some("v0.47.0".into()),
            newest: Some("v0.48.0-beta.1".into()),
            prerelease: Some("v0.48.0-beta.1".into()),
        };
        let releases = releases_from(ReleaseChannel::Prerelease, &tags).unwrap();
        assert_eq!(
            releases,
            Releases {
                latest: "0.48.0-beta.1".into(),
                stable: Some("0.47.0".into()),
                prerelease: Some("0.48.0-beta.1".into()),
            }
        );
        let options = plan::PlanOptions {
            staging_root: PathBuf::from("/stage"),
            can_prompt_for_privilege: false,
            provenance: ProvenanceCheck::Unavailable {
                reason: verify::GH_NOT_INSTALLED.into(),
            },
        };
        assert_eq!(
            plan::plan(&stable_formula, &releases, &options).latest,
            "0.47.0"
        );
        assert_eq!(
            releases_from(ReleaseChannel::Stable, &tags).unwrap().latest,
            "0.47.0"
        );
        assert!(releases_from(ReleaseChannel::Stable, &Default::default()).is_err());
    }

    /// A `brew upgrade` plan for the stable formula from 0.46.0 to 0.47.0,
    /// with `brew` at `/opt/homebrew/bin/brew`.
    fn brew_plan() -> UpgradePlan {
        let mut found = copy_at(
            "0.46.0",
            InstallMethod::Homebrew {
                formula: crate::self_upgrade::HomebrewFormula::Stable,
                prefix: PathBuf::from("/opt/homebrew"),
            },
        );
        found.tools.brew = Some(PathBuf::from("/opt/homebrew/bin/brew"));
        let options = plan::PlanOptions {
            staging_root: PathBuf::from("/stage"),
            can_prompt_for_privilege: false,
            provenance: ProvenanceCheck::Unavailable {
                reason: verify::GH_NOT_INSTALLED.into(),
            },
        };
        let plan = plan::plan(&found, &"0.47.0".into(), &options);
        assert!(matches!(plan.action, PlanAction::BrewUpgrade { .. }));
        plan
    }

    /// Run `plan` on a machine whose upgraded `dot-agent-deck` answers
    /// `--version` with `reported` (or not at all).
    fn brew_upgrade_reporting(reported: Option<&str>) -> Result<Outcome, UpgradeError> {
        let mut host = FakeHost::new()
            .exe("/opt/homebrew/bin/brew")
            .answer("/opt/homebrew/bin/brew upgrade dot-agent-deck", ok(""));
        if let Some(version) = reported {
            host = host.deck("/opt/homebrew/bin/dot-agent-deck", version);
        }
        let source = ReleaseSource {
            api_url: String::new(),
            list_url: String::new(),
            download_base: String::new(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(execute(&host, &brew_plan(), &source, Path::new("/stage")))
    }

    // Unix paths the fake host answers for: native Windows is unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn execute_022_a_brew_upgrade_is_upgraded_only_once_it_reports_the_offered_version() {
        let reached = brew_upgrade_reporting(Some("0.47.0")).unwrap();
        assert_eq!(
            reached.lines(),
            ["`brew upgrade dot-agent-deck` finished; it now reports v0.47.0."]
        );
        assert!(reached.upgraded());

        // Homebrew's tap has not caught up with the GitHub release yet.
        let behind = brew_upgrade_reporting(Some("0.46.0")).unwrap();
        assert!(!behind.upgraded(), "{behind:?}");
        assert_eq!(
            behind.lines(),
            [
                "`brew upgrade dot-agent-deck` finished, but dot-agent-deck still reports v0.46.0, not v0.47.0: Homebrew does not offer v0.47.0 yet. Try again later."
            ]
        );
        assert_eq!(behind.tui_restart_line("0.47.0"), None);

        let silent = brew_upgrade_reporting(None).unwrap();
        assert!(!silent.upgraded(), "{silent:?}");
        assert_eq!(
            silent.lines(),
            [
                "`brew upgrade dot-agent-deck` finished, but the upgraded dot-agent-deck did not report its version, so it is not known to run v0.47.0. Check with `dot-agent-deck --version`."
            ]
        );
    }

    #[test]
    fn execute_008_outcome_lines() {
        let outcome = Outcome::Staged {
            path: PathBuf::from("/s/x"),
            command: Some("sudo install -m 0755 /s/x /usr/local/bin/dot-agent-deck".into()),
            version: "0.46.0".into(),
            provenance: Provenance::Verified,
        };
        assert_eq!(
            outcome.lines()[1],
            "  sudo install -m 0755 /s/x /usr/local/bin/dot-agent-deck"
        );
        assert_eq!(
            outcome.items()[1],
            PlanLine::Command("sudo install -m 0755 /s/x /usr/local/bin/dot-agent-deck".into())
        );

        // No command could be shown safely: the manual route, and no command.
        let unshowable = Outcome::Staged {
            path: PathBuf::from("/s/x"),
            command: None,
            version: "0.46.0".into(),
            provenance: Provenance::Verified,
        };
        let items = unshowable.items();
        assert!(
            !items
                .iter()
                .any(|line| matches!(line, PlanLine::Command(_)))
        );
        assert_eq!(
            items[1],
            PlanLine::Text(plan::manual_upgrade_line("0.46.0"))
        );
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn execute_009_staging_is_a_fresh_private_directory_per_upgrade() {
        let state = tempfile::tempdir().unwrap();
        let root = state.path().join("upgrade");
        let first = Staging::create(&root, "0.46.0").unwrap();
        let second = Staging::create(&root, "0.46.0").unwrap();
        assert_ne!(
            first.dir(),
            second.dir(),
            "two stagings of one release collide"
        );
        for staging in [&first, &second] {
            assert_eq!(staging.dir().parent(), Some(root.as_path()));
            let name = staging
                .dir()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            assert!(
                name.starts_with("v0.46.0-") && name.len() > "v0.46.0-".len() + 8,
                "{name}"
            );
            #[cfg(unix)]
            assert_eq!(mode_of(staging.dir()), 0o700);
        }
        #[cfg(unix)]
        assert_eq!(mode_of(&root), 0o700);

        let (one, two) = (first.dir().to_path_buf(), second.dir().to_path_buf());
        let mut kept = second;
        kept.keep();
        drop(first);
        drop(kept);
        assert!(!one.exists(), "a staging directory is removed when done");
        assert!(two.exists(), "a kept one stays for the user's command");
    }

    #[test]
    fn execute_010_concurrent_stagings_do_not_collide() {
        let state = tempfile::tempdir().unwrap();
        let root = state.path().join("upgrade");
        let dirs: Vec<PathBuf> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let root = root.clone();
                    scope.spawn(move || {
                        let mut staging = Staging::create(&root, "0.46.0").unwrap();
                        staging.keep();
                        write_new_file(&staging.dir().join(CLI_MANIFEST), b"m", 0o600).unwrap();
                        staging.dir().to_path_buf()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let unique: std::collections::HashSet<_> = dirs.iter().collect();
        assert_eq!(unique.len(), dirs.len());
    }

    #[cfg(unix)]
    #[test]
    fn execute_011_a_permissive_staging_root_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir().unwrap();
        let root = state.path().join("upgrade");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = Staging::create(&root, "0.46.0").err().unwrap();
        assert!(matches!(err, UpgradeError::StagingUnsafe { .. }), "{err:?}");
        assert!(
            err.to_string().contains("other users can write to it"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            0,
            "nothing was staged"
        );

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(
            Staging::create(&root, "0.46.0").is_err(),
            "group-writable too"
        );

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            Staging::create(&root, "0.46.0").is_ok(),
            "readable by others is fine"
        );
    }

    #[cfg(unix)]
    #[test]
    fn execute_012_a_symlinked_staging_root_is_refused() {
        let state = tempfile::tempdir().unwrap();
        let elsewhere = state.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let root = state.path().join("upgrade");
        std::os::unix::fs::symlink(&elsewhere, &root).unwrap();
        let err = Staging::create(&root, "0.46.0").err().unwrap();
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);

        let file_root = state.path().join("a-file");
        std::fs::write(&file_root, b"").unwrap();
        let err = Staging::create(&file_root, "0.46.0").err().unwrap();
        assert!(err.to_string().contains("not a directory"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn execute_013_a_planted_manifest_symlink_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        let manifest = dir.path().join(CLI_MANIFEST);
        std::os::unix::fs::symlink(&victim, &manifest).unwrap();
        assert!(write_new_file(&manifest, b"attacker-chosen", 0o600).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");

        let fresh = dir.path().join("fresh");
        write_new_file(&fresh, b"x", 0o600).unwrap();
        assert_eq!(mode_of(&fresh), 0o600);
        assert!(
            write_new_file(&fresh, b"y", 0o600).is_err(),
            "never overwrites"
        );
    }

    #[test]
    fn execute_014_atomic_replace_refuses_a_temp_swapped_after_its_check() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dot-agent-deck");
        std::fs::write(&target, b"old").unwrap();
        let err = atomic_replace(&target, b"new", &verify::sha256_hex(b"new"), |candidate| {
            // Another process swaps the checked file before the rename.
            std::fs::remove_file(candidate).unwrap();
            std::fs::write(candidate, b"evil").unwrap();
            Ok(())
        })
        .unwrap_err();
        assert!(matches!(err, UpgradeError::StagedChanged { .. }), "{err:?}");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    const PKEXEC: &str = "/usr/bin/pkexec";

    /// A staged binary and the target it is installed over, in a temp dir.
    fn staged_install(bytes: &[u8]) -> (tempfile::TempDir, PathBuf, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        std::fs::write(&staged, bytes).unwrap();
        let target = dir.path().join("dot-agent-deck");
        std::fs::write(&target, b"old").unwrap();
        (dir, staged, target, verify::sha256_hex(bytes))
    }

    // Used only by Unix-gated tests: native Windows is unsupported (#164).
    #[cfg(unix)]
    /// What `pkexec` answers when its password prompt is dismissed: exit 126,
    /// before it runs anything.
    fn dismissed() -> CommandOutput {
        CommandOutput {
            success: false,
            code: Some(126),
            stdout: String::new(),
            stderr: "Error executing command as another user: Request dismissed".into(),
        }
    }

    fn unfinished(err: &UpgradeError) -> &crate::self_upgrade::UnfinishedInstall {
        match err {
            UpgradeError::InstallUnfinished(unfinished) => unfinished,
            other => panic!("not an unfinished install: {other:?}"),
        }
    }

    // Used only by Unix-gated tests: native Windows is unsupported (#164).
    #[cfg(unix)]
    fn privileged(
        host: &FakeHost,
        staged: &Path,
        target: &Path,
        sha: &str,
    ) -> (UpgradeError, Option<String>) {
        let command = plan::install_binary_command(None, staged, target, sha);
        let err = install_binary_privileged(
            host,
            Path::new(PKEXEC),
            staged,
            target,
            sha,
            "0.46.0",
            command.clone(),
        )
        .unwrap_err();
        (err, command)
    }

    /// `pkexec /usr/bin/install -m 0755 <from> <to>` writes `installed` to
    /// `<to>` (the verified bytes for an honest install).
    fn installing(installed: Option<&'static [u8]>) -> FakeHost {
        FakeHost::new().exe(PKEXEC).handle(PKEXEC, move |args| {
            let to = PathBuf::from(&args[4]);
            match installed {
                Some(bytes) => std::fs::write(&to, bytes).unwrap(),
                None => {
                    std::fs::copy(&args[3], &to).unwrap();
                }
            }
            ok("")
        })
    }

    #[test]
    fn execute_015_privileged_install_checks_and_then_runs_the_target() {
        let (_dir, staged, target, sha) = staged_install(b"verified build");
        let host = installing(None).answer(
            &format!("{} --version", target.display()),
            ok("dot-agent-deck 0.46.0\n"),
        );
        install_binary_privileged(
            &host,
            Path::new(PKEXEC),
            &staged,
            &target,
            &sha,
            "0.46.0",
            None,
        )
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"verified build");
        assert!(host.ran().last().unwrap().ends_with("--version"));
    }

    #[test]
    fn execute_016_a_file_swapped_after_verification_is_refused_at_the_rehash() {
        let (_dir, staged, target, sha) = staged_install(b"verified build");
        std::fs::remove_file(&staged).unwrap();
        std::fs::write(&staged, b"swapped in later").unwrap();
        let host = installing(None);
        let err = install_binary_privileged(
            &host,
            Path::new(PKEXEC),
            &staged,
            &target,
            &sha,
            "0.46.0",
            None,
        )
        .unwrap_err();
        assert!(matches!(err, UpgradeError::StagedChanged { .. }), "{err:?}");
        assert!(host.ran().is_empty(), "pkexec never ran: {:?}", host.ran());
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
    }

    #[test]
    fn execute_017_an_installed_target_that_does_not_match_is_reported_and_not_run() {
        let (_dir, staged, target, sha) = staged_install(b"verified build");
        // What lands at the target is not what was verified (swapped between
        // the re-hash and `install` reading it).
        let host = installing(Some(b"not the verified build")).answer(
            &format!("{} --version", target.display()),
            ok("dot-agent-deck 0.46.0\n"),
        );
        let err = install_binary_privileged(
            &host,
            Path::new(PKEXEC),
            &staged,
            &target,
            &sha,
            "0.46.0",
            None,
        )
        .unwrap_err();
        assert!(
            matches!(err, UpgradeError::InstalledMismatch { .. }),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("NOT the verified build"), "{message}");
        assert!(message.contains("reinstall"), "{message}");
        assert!(message.contains("/releases/tag/v0.46.0"), "{message}");
        assert!(
            !host.ran().iter().any(|line| line.ends_with("--version")),
            "the mismatched target was executed: {:?}",
            host.ran()
        );
    }

    // A Unix staging path the shown command can name: native Windows is
    // unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn execute_018_a_failed_prompt_hands_over_the_install_command() {
        let (_dir, staged, target, sha) = staged_install(b"verified build");
        let host = FakeHost::new().exe(PKEXEC).handle(PKEXEC, |_| dismissed());
        let command = plan::install_binary_command(None, &staged, &target, &sha);
        let err = install_binary_privileged(
            &host,
            Path::new(PKEXEC),
            &staged,
            &target,
            &sha,
            "0.46.0",
            command.clone(),
        )
        .unwrap_err();
        assert!(
            matches!(err, UpgradeError::PrivilegeFailed { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("Request dismissed"), "{err}");
        assert_eq!(
            err.fallback(),
            vec![
                PlanLine::Text(plan::PROMPT_FAILED.into()),
                PlanLine::Command(command.unwrap()),
            ]
        );
        let unshowable = UpgradeError::PrivilegeFailed {
            command: "pkexec".into(),
            detail: "dismissed".into(),
            install: None,
            version: "0.46.0".into(),
        };
        assert_eq!(
            unshowable.fallback(),
            vec![PlanLine::Text(plan::manual_upgrade_line("0.46.0"))]
        );
    }

    // A Unix staging path the shown command can name: native Windows is
    // unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn execute_019_a_failed_prompt_is_recognised_whatever_the_pkexec_path() {
        // The prompt's failure is recognised by the error's variant, not by
        // parsing the command line it shows, so a pkexec path that the shown
        // command quotes still hands over the install command.
        const QUOTED_PKEXEC: &str = "/opt/my tools/pkexec";
        let (_dir, staged, target, sha) = staged_install(b"verified build");
        let host = FakeHost::new()
            .exe(QUOTED_PKEXEC)
            .handle(QUOTED_PKEXEC, |_| dismissed());
        let command = plan::install_binary_command(None, &staged, &target, &sha);
        let err = install_binary_privileged(
            &host,
            Path::new(QUOTED_PKEXEC),
            &staged,
            &target,
            &sha,
            "0.46.0",
            command.clone(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("'/opt/my tools/pkexec'"),
            "the shown command quotes the path: {err}"
        );
        assert_eq!(
            err.fallback(),
            vec![
                PlanLine::Text(plan::PROMPT_FAILED.into()),
                PlanLine::Command(command.unwrap()),
            ]
        );
    }

    /// Scenario: the privileged install gets past the password prompt,
    /// writes part of the target and fails. That is not a dismissed prompt:
    /// the result says the target may be partly written, that it matches
    /// neither version, and gives the install command again. A failure
    /// `pkexec` reports for the prompt itself (exit 126 or 127) is still the
    /// dismissed-prompt result.
    // A Unix staging path the shown command can name: native Windows is
    // unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn execute_026_a_failed_install_that_changed_the_target_is_not_a_dismissed_prompt() {
        let (_dir, staged, target, sha) = staged_install(b"verified build");
        let host = FakeHost::new().exe(PKEXEC).handle(PKEXEC, |args| {
            std::fs::write(&args[4], b"verif").unwrap();
            fail("/usr/bin/install: error writing: No space left on device")
        });
        let (err, command) = privileged(&host, &staged, &target, &sha);
        let command = command.unwrap();
        let found = unfinished(&err);
        assert!(!found.may_still_be_running);
        assert_eq!(found.found, plan::Found::Neither);
        assert_eq!(found.install.as_ref(), Some(&command));
        assert!(err.to_string().contains("No space left on device"), "{err}");
        assert_eq!(
            err.fallback(),
            vec![
                PlanLine::Text(format!(
                    "The install did not complete, so {} may be partly written.",
                    target.display()
                )),
                PlanLine::Text(
                    "WARNING: it matches neither the previous version nor the verified v0.46.0. Do not run it.".into()
                ),
                PlanLine::Text("Install v0.46.0 with:".into()),
                PlanLine::Command(command),
            ]
        );
        assert!(
            !host.ran().iter().any(|line| line.ends_with("--version")),
            "the damaged target was executed: {:?}",
            host.ran()
        );

        for code in [126, 127] {
            let (_dir, staged, target, sha) = staged_install(b"verified build");
            let host = FakeHost::new()
                .exe(PKEXEC)
                .handle(PKEXEC, move |_| CommandOutput {
                    code: Some(code),
                    ..dismissed()
                });
            let (err, _) = privileged(&host, &staged, &target, &sha);
            assert!(
                matches!(err, UpgradeError::PrivilegeFailed { .. }),
                "{code}: {err:?}"
            );
        }
    }

    /// Scenario: the privileged install outlives its bound and cannot be
    /// stopped, because it runs as root. It may still be writing the target,
    /// so nothing is checked and no install command is offered: the result
    /// says to wait and then check the version, the binary's `--version` or
    /// `dpkg -s` for the package. The staged file is kept for the install
    /// still reading it.
    // A Unix staging path the shown command can name: native Windows is
    // unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn execute_027_a_timed_out_install_that_could_not_be_stopped_offers_no_command() {
        let (_dir, staged, target, sha) = staged_install(b"verified build");
        let host = FakeHost::new().exe(PKEXEC).handle_io(PKEXEC, |args| {
            std::fs::write(&args[4], b"verif").unwrap();
            Err(crate::self_upgrade::timed_out(INSTALL_TIMEOUT, false))
        });
        let (err, _) = privileged(&host, &staged, &target, &sha);
        let found = unfinished(&err);
        assert!(found.may_still_be_running);
        assert_eq!(found.found, plan::Found::NotChecked);
        assert_eq!(found.install, None);
        assert!(
            err.to_string()
                .ends_with("could not be stopped, so it may still be running"),
            "{err}"
        );
        let waiting = "The install did not finish in time and may still be running, so whether v0.46.0 was installed is not known. Do not start it again: wait a few minutes for it to finish, then check which version is installed with:";
        assert_eq!(
            err.fallback(),
            vec![
                PlanLine::Text(waiting.into()),
                PlanLine::Command(format!("{} --version", target.display())),
            ]
        );
        let staging_root = tempfile::tempdir().unwrap();
        let mut staging = Staging::create(&staging_root.path().join("upgrade"), "0.46.0").unwrap();
        staging.keep_after(&err);
        let kept = staging.dir().to_path_buf();
        drop(staging);
        assert!(
            kept.is_dir(),
            "the staged file was removed under the install"
        );

        let host = FakeHost::new().exe(PKEXEC).handle_io(PKEXEC, |_| {
            Err(crate::self_upgrade::timed_out(INSTALL_TIMEOUT, false))
        });
        let err = install_deb(
            &host,
            Path::new(PKEXEC),
            Path::new("/s/x.deb"),
            "0.45.0",
            "0.46.0",
            Some("sudo apt install /s/x.deb".into()),
        )
        .unwrap_err();
        assert_eq!(
            err.fallback(),
            vec![
                PlanLine::Text(waiting.into()),
                PlanLine::Command("dpkg -s agent-deck".into()),
            ]
        );
        assert_eq!(host.ran().len(), 1, "nothing else ran: {:?}", host.ran());
    }

    /// Scenario: the privileged install outlives its bound and is stopped.
    /// The target may be partly written, so it is hashed and the result says
    /// what is there: the previous version (install command given), the
    /// verified new build (no command, check it), or nothing (command given).
    // A Unix staging path the shown command can name: native Windows is
    // unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn execute_028_a_stopped_install_checks_the_target_hash() {
        let stopped = || Err(crate::self_upgrade::timed_out(INSTALL_TIMEOUT, true));
        let partly = |target: &Path| {
            PlanLine::Text(format!(
                "The install did not complete, so {} may be partly written.",
                target.display()
            ))
        };

        let (_dir, staged, target, sha) = staged_install(b"verified build");
        let host = FakeHost::new()
            .exe(PKEXEC)
            .handle_io(PKEXEC, move |_| stopped());
        let (err, command) = privileged(&host, &staged, &target, &sha);
        assert_eq!(
            err.fallback(),
            vec![
                partly(&target),
                PlanLine::Text("It is unchanged: it still holds the previous version.".into()),
                PlanLine::Text("Install v0.46.0 with:".into()),
                PlanLine::Command(command.unwrap()),
            ]
        );

        let (_dir, staged, target, sha) = staged_install(b"verified build");
        let host = FakeHost::new().exe(PKEXEC).handle_io(PKEXEC, move |args| {
            std::fs::copy(&args[3], &args[4]).unwrap();
            stopped()
        });
        let (err, _) = privileged(&host, &staged, &target, &sha);
        assert_eq!(unfinished(&err).found, plan::Found::New);
        assert_eq!(unfinished(&err).install, None);
        assert_eq!(
            err.fallback(),
            vec![
                partly(&target),
                PlanLine::Text(
                    "It matches the verified v0.46.0, so v0.46.0 is installed. Check it with:"
                        .into()
                ),
                PlanLine::Command(format!("{} --version", target.display())),
            ]
        );

        let (_dir, staged, target, sha) = staged_install(b"verified build");
        let host = FakeHost::new().exe(PKEXEC).handle_io(PKEXEC, move |args| {
            std::fs::remove_file(&args[4]).unwrap();
            stopped()
        });
        let (err, command) = privileged(&host, &staged, &target, &sha);
        assert_eq!(
            err.fallback(),
            vec![
                partly(&target),
                PlanLine::Text("It is missing.".into()),
                PlanLine::Text("Install v0.46.0 with:".into()),
                PlanLine::Command(command.unwrap()),
            ]
        );
    }

    /// Scenario: the `.deb` install is stopped, or fails, once past the
    /// prompt. The package's state is asked of `dpkg-query`: installed at the
    /// new version gets no command, the previous version or a half-installed
    /// package gets the install command again, and an unanswered query says
    /// to check with `dpkg -s`.
    #[test]
    fn execute_029_a_stopped_package_install_reports_the_package_state() {
        const QUERY: &str = "/usr/bin/dpkg-query -W -f=${Version} ${db:Status-Abbrev} agent-deck";
        const INSTALL: &str = "sudo apt install /s/x.deb";
        let run = |state: Option<&str>| {
            let host = FakeHost::new()
                .exe(PKEXEC)
                .exe("/usr/bin/dpkg-query")
                .handle(PKEXEC, |_| {
                    fail("E: Sub-process /usr/bin/dpkg returned an error code (1)")
                });
            let host = match state {
                Some(state) => host.answer(QUERY, ok(state)),
                None => host,
            };
            install_deb(
                &host,
                Path::new(PKEXEC),
                Path::new("/s/x.deb"),
                "0.45.0",
                "0.46.0",
                Some(INSTALL.into()),
            )
            .unwrap_err()
        };
        let partly = PlanLine::Text(
            "The install did not complete, so the agent-deck package may be partly installed."
                .into(),
        );
        let install = [
            PlanLine::Text("Install v0.46.0 with:".into()),
            PlanLine::Command(INSTALL.into()),
        ];
        let check = PlanLine::Command("dpkg -s agent-deck".into());

        let mut half = vec![
            partly.clone(),
            PlanLine::Text(
                "The agent-deck package is not fully installed at the previous version or at v0.46.0."
                    .into(),
            ),
        ];
        half.extend(install.clone());
        assert_eq!(run(Some("0.46.0 iU ")).fallback(), half);

        let mut previous = vec![
            partly.clone(),
            PlanLine::Text("It is unchanged: it still holds the previous version.".into()),
        ];
        previous.extend(install);
        assert_eq!(run(Some("0.45.0 ii ")).fallback(), previous);

        assert_eq!(
            run(Some("0.46.0 ii ")).fallback(),
            vec![
                partly.clone(),
                PlanLine::Text(
                    "The agent-deck package reports v0.46.0 installed. Check it with:".into()
                ),
                check.clone(),
            ]
        );
        let unanswered = run(None);
        assert!(
            matches!(unfinished(&unanswered).found, plan::Found::Unreadable(_)),
            "{unanswered:?}"
        );
        assert_eq!(unfinished(&unanswered).install, None);
        assert_eq!(unanswered.fallback().last(), Some(&check));
    }

    // Used only by Unix-gated tests: native Windows is unsupported (#164).
    #[cfg(unix)]
    /// [`fake_mac_detaching`] whose `hdiutil attach` fails: having mounted
    /// the image (the mount point filled) when `mounts`, and by timing out
    /// when `times_out`, else by exiting 1.
    fn fake_mac_attach_failing(
        mounts: bool,
        times_out: bool,
        plain: bool,
        force: bool,
    ) -> FakeHost {
        let mut host = fake_mac_detaching(true, plain, force);
        let detach = host.handlers.remove(HDIUTIL).unwrap();
        host.handle_io(HDIUTIL, move |args| {
            if args[0] != "attach" {
                return detach(args);
            }
            if mounts {
                let mount = PathBuf::from(&args[5]);
                std::fs::create_dir_all(mount.join(DESKTOP_APP_BUNDLE)).unwrap();
            }
            if times_out {
                Err(crate::self_upgrade::timed_out(INSTALL_TIMEOUT, true))
            } else {
                Ok(fail(
                    "hdiutil: attach failed - Resource temporarily unavailable",
                ))
            }
        })
    }

    /// Scenario: `hdiutil attach` mounts the release's image at this
    /// upgrade's mount point and then fails or times out. The mount point is
    /// detached anyway, plain and then forced; when that fails too, the error
    /// says the image is still attached and how to detach it. An attach that
    /// mounted nothing detaches nothing.
    // Unix paths the fake host answers for: native Windows is unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn execute_030_an_attach_that_mounted_and_then_failed_is_detached() {
        let swap = |host: &FakeHost, root: &Path| {
            let app = installed_app(root);
            swap_app(
                host,
                Path::new("/s/x.dmg"),
                &app,
                "TEAM123",
                "0.46.0",
                &root.join("work"),
            )
            .unwrap_err()
        };
        let detaches = |host: &FakeHost| {
            host.ran()
                .into_iter()
                .filter(|line| line.starts_with(&format!("{HDIUTIL} detach")))
                .collect::<Vec<_>>()
        };

        for times_out in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mount = root.path().join("work/mount");
            let host = fake_mac_attach_failing(true, times_out, true, false);
            let err = swap(&host, root.path());
            assert!(
                matches!(err, UpgradeError::CommandFailed { .. }),
                "{times_out}: {err:?}"
            );
            assert_eq!(
                detaches(&host),
                [format!("{HDIUTIL} detach {}", mount.display())],
                "{times_out}"
            );

            let root = tempfile::tempdir().unwrap();
            let mount = root.path().join("work/mount");
            let host = fake_mac_attach_failing(true, times_out, false, false);
            let err = swap(&host, root.path());
            assert!(
                matches!(&err, UpgradeError::StillMounted { error, mount: left }
                    if matches!(**error, UpgradeError::CommandFailed { .. }) && *left == mount),
                "{times_out}: {err:?}"
            );
            assert_eq!(
                detaches(&host),
                [
                    format!("{HDIUTIL} detach {}", mount.display()),
                    format!("{HDIUTIL} detach {} -force", mount.display()),
                ],
                "{times_out}"
            );
            let fallback = plan::render_lines(&err.fallback()).join("\n");
            assert!(
                fallback.contains(&format!("still attached at {}", mount.display())),
                "{fallback}"
            );
        }

        let root = tempfile::tempdir().unwrap();
        let host = fake_mac_attach_failing(false, false, true, true);
        let err = swap(&host, root.path());
        assert!(matches!(err, UpgradeError::CommandFailed { .. }), "{err:?}");
        assert!(detaches(&host).is_empty(), "{:?}", host.ran());
    }

    /// Scenario: the release's image is left attached at a mount point whose
    /// path a terminal or the dialog would not show as the shell reads it: a
    /// newline, a bidi override, bytes that are not UTF-8. No detach command
    /// is built for it; the user is told to list the attached images
    /// instead. A path with an apostrophe and a space is quoted as one shell
    /// word.
    // Unix paths, and non-UTF-8 ones: native Windows is unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn execute_031_a_detach_command_names_only_a_path_it_can_show_faithfully() {
        use std::os::unix::ffi::OsStrExt;
        let unshowable = [
            PathBuf::from("/s/v0.46.0-ab/mo\nunt"),
            PathBuf::from("/s/v0.46.0-ab/\u{202E}tnuom"),
            PathBuf::from(std::ffi::OsStr::from_bytes(b"/s/v0.46.0-ab/mo\xffunt")),
        ];
        for mount in unshowable {
            let lines = still_mounted(&mount);
            assert_eq!(
                lines,
                vec![
                    PlanLine::Text(STILL_MOUNTED_UNSHOWABLE.into()),
                    PlanLine::Command(format!("{HDIUTIL} info")),
                ],
                "{mount:?}"
            );
            let err = UpgradeError::StillMounted {
                error: Box::new(UpgradeError::Io("x".into())),
                mount: mount.clone(),
            };
            assert_eq!(err.fallback(), lines, "{mount:?}");
        }

        let mount = Path::new("/s/it's here/mount");
        assert_eq!(
            still_mounted(mount),
            vec![
                PlanLine::Text(
                    "Warning: the release's disk image is still attached at /s/it's here/mount. Detach it with:".into()
                ),
                PlanLine::Command(format!("{HDIUTIL} detach -force '/s/it'\\''s here/mount'")),
            ]
        );
    }
}
