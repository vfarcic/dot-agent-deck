//! What to offer for one installed copy and a newer release, and every word a
//! client shows about it.
//!
//! [`plan`] is pure. Both clients render [`UpgradePlan::lines`] as they are, so
//! the TUI, the desktop app and `dot-agent-deck upgrade` say the same thing
//! (CLAUDE.md rule 22). Nothing here acts: a plan is shown, and only
//! [`super::execute::execute`] carries out one the user confirmed.

use std::path::{Path, PathBuf};

use super::detect::{CopyKind, HomebrewFormula, InstallMethod, Installation, SourceReason};
use super::{DESKTOP_APP_BUNDLE, shell_word};

/// What [`plan`] needs to know about the client that will show the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanOptions {
    /// Where verified downloads are kept for a command the user runs with
    /// `sudo`, so the plan can name the exact file before it exists. One
    /// subdirectory per version is used under it.
    pub staging_root: PathBuf,
    /// Whether this client can raise a graphical privilege prompt
    /// (`pkexec`) itself. The desktop app on Linux can; a terminal client
    /// cannot without fighting its own screen, so it shows the command.
    pub can_prompt_for_privilege: bool,
}

impl PlanOptions {
    /// The defaults for a terminal client: staging under the deck's state
    /// directory, no privilege prompt.
    pub fn terminal() -> Self {
        Self {
            staging_root: crate::platform::paths::state_dir().join("upgrade"),
            can_prompt_for_privilege: false,
        }
    }
}

/// What a client offers for one copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAction {
    /// The copy already runs the latest release.
    UpToDate,
    /// The deck does not change this install; the plan's text says what the
    /// user does instead.
    NotifyOnly,
    /// The user runs `command` themselves.
    ShowCommand { command: String },
    /// `brew upgrade <formula>`.
    BrewUpgrade {
        brew: PathBuf,
        formula: HomebrewFormula,
    },
    /// Download `asset`, verify it, and atomically replace `target`.
    ReplaceBinary { target: PathBuf, asset: String },
    /// Download `asset` and verify it into `staged`; the user then runs
    /// `command` (`sudo install …`) because `target` is not writable — or,
    /// with `pkexec`, the client runs it behind a privilege prompt.
    StagedInstall {
        target: PathBuf,
        asset: String,
        staged: PathBuf,
        command: String,
        pkexec: Option<PathBuf>,
    },
    /// Download the `.deb`, verify it into `staged`, and install it with
    /// `pkexec apt-get install` when `pkexec` is set, otherwise show `command`.
    InstallDeb {
        asset: String,
        staged: PathBuf,
        command: String,
        pkexec: Option<PathBuf>,
    },
    /// Download the `.dmg`, verify the app inside it, and swap it for `app`.
    SwapApp {
        app: PathBuf,
        asset: String,
        team_id: String,
    },
    /// The app cannot be replaced from here; the user downloads `url`.
    ManualDownload { url: String },
}

/// A copy, the release it could move to, and what to offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradePlan {
    pub installation: Installation,
    /// The newer release's version, without a leading `v`.
    pub latest: String,
    pub action: PlanAction,
}

/// The URL a release asset downloads from in a browser.
pub fn release_asset_url(version: &str, asset: &str) -> String {
    format!(
        "{}/v{version}/{asset}",
        crate::repo_identity::RELEASE_DOWNLOAD_BASE
    )
}

/// Decide what to offer `installation` for release `latest`. Pure.
pub fn plan(installation: &Installation, latest: &str, options: &PlanOptions) -> UpgradePlan {
    let latest = latest.strip_prefix('v').unwrap_or(latest).to_string();
    let action = if !super::is_newer(&installation.version, &latest) {
        PlanAction::UpToDate
    } else {
        action_for(installation, &latest, options)
    };
    UpgradePlan {
        installation: installation.clone(),
        latest,
        action,
    }
}

fn action_for(installation: &Installation, latest: &str, options: &PlanOptions) -> PlanAction {
    let platform = installation.platform;
    let cli_asset = platform.map(|p| p.cli_asset().to_string());
    let desktop_asset = platform.and_then(|p| p.desktop_asset()).map(str::to_string);
    let staged = |asset: &str| options.staging_root.join(format!("v{latest}")).join(asset);
    let pkexec = || {
        installation
            .tools
            .pkexec
            .clone()
            .filter(|_| options.can_prompt_for_privilege)
    };

    match &installation.method {
        InstallMethod::Nix | InstallMethod::Source { .. } | InstallMethod::SystemPackage { .. } => {
            PlanAction::NotifyOnly
        }
        InstallMethod::Homebrew { formula, prefix } => {
            let brew = installation
                .tools
                .brew
                .clone()
                .filter(|brew| brew.starts_with(prefix))
                .unwrap_or_else(|| prefix.join("bin/brew"));
            match installation.tools.brew {
                Some(_) => PlanAction::BrewUpgrade {
                    brew,
                    formula: *formula,
                },
                None => PlanAction::ShowCommand {
                    command: format!("brew upgrade {}", formula.name()),
                },
            }
        }
        InstallMethod::DownloadedWritable { binary } => match cli_asset {
            Some(asset) => PlanAction::ReplaceBinary {
                target: binary.clone(),
                asset,
            },
            None => PlanAction::NotifyOnly,
        },
        InstallMethod::DownloadedNonWritable { binary } => match cli_asset {
            Some(asset) => {
                let staged = staged(&asset);
                PlanAction::StagedInstall {
                    command: sudo_install_command(&staged, binary),
                    target: binary.clone(),
                    asset,
                    staged,
                    pkexec: pkexec(),
                }
            }
            None => PlanAction::NotifyOnly,
        },
        InstallMethod::DesktopDeb => match desktop_asset {
            Some(asset) => {
                let staged = staged(&asset);
                PlanAction::InstallDeb {
                    command: format!("sudo apt install {}", shell_word(&staged.to_string_lossy())),
                    asset,
                    staged,
                    pkexec: pkexec(),
                }
            }
            None => PlanAction::NotifyOnly,
        },
        InstallMethod::DesktopDmg {
            app,
            parent_writable,
            team_id,
        } => match (desktop_asset, team_id) {
            (Some(asset), Some(team_id)) if *parent_writable => PlanAction::SwapApp {
                app: app.clone(),
                asset,
                team_id: team_id.clone(),
            },
            (Some(asset), _) => PlanAction::ManualDownload {
                url: release_asset_url(latest, &asset),
            },
            (None, _) => PlanAction::NotifyOnly,
        },
    }
}

/// `sudo install -m 0755 <staged> <target>`, quoted for a POSIX shell.
fn sudo_install_command(staged: &Path, target: &Path) -> String {
    format!(
        "sudo install -m 0755 {} {}",
        shell_word(&staged.to_string_lossy()),
        shell_word(&target.to_string_lossy())
    )
}

impl UpgradePlan {
    /// Whether the client can carry this plan out once the user confirms.
    pub fn is_actionable(&self) -> bool {
        matches!(
            self.action,
            PlanAction::BrewUpgrade { .. }
                | PlanAction::ReplaceBinary { .. }
                | PlanAction::StagedInstall { .. }
                | PlanAction::InstallDeb { .. }
                | PlanAction::SwapApp { .. }
        )
    }

    /// What the copy is called in every message.
    pub fn label(&self) -> &'static str {
        match self.installation.copy {
            CopyKind::Cli => "dot-agent-deck",
            CopyKind::Desktop => "Agent Deck (desktop app)",
        }
    }

    /// The first line: the same words as the TUI's footer badge.
    pub fn headline(&self) -> String {
        match self.action {
            PlanAction::UpToDate => format!(
                "{} is up to date (v{}).",
                self.label(),
                self.installation.version
            ),
            _ => format!(
                "{}: update available: v{} (current: v{})",
                self.label(),
                self.latest,
                self.installation.version
            ),
        }
    }

    /// The question a client asks before acting, or `None` when there is
    /// nothing to confirm.
    pub fn confirm_question(&self) -> Option<String> {
        self.is_actionable()
            .then(|| format!("Upgrade {} to v{}?", self.label(), self.latest))
    }

    /// The whole plan as the lines a client shows: the headline, how the copy
    /// was installed, and what upgrading does or what the user does instead.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![self.headline()];
        if self.action == PlanAction::UpToDate {
            return lines;
        }
        let latest = &self.latest;
        let installation = &self.installation;
        let exe = installation.executable.display();
        match (&installation.method, &self.action) {
            (InstallMethod::Nix, _) => lines.push(format!(
                "Installed with Nix ({exe}), so it is not changed from here. Update your flake input (for example `nix flake update`) and rebuild, or run `nix profile upgrade`."
            )),
            (InstallMethod::Source { reason }, _) => {
                let how = match reason {
                    SourceReason::BuildTree => "it runs from a cargo build directory",
                    SourceReason::CargoInstall => "it was installed with `cargo install`",
                    SourceReason::DirtyTree => "it was built from a checkout with uncommitted changes",
                };
                lines.push(format!(
                    "Built from source ({how}: {exe}), so it is not replaced from here. Check out v{latest} and rebuild."
                ));
            }
            (InstallMethod::SystemPackage { package }, _) => lines.push(format!(
                "Installed by the system package `{package}` ({exe}), so it is not replaced from here. Upgrade that package with your package manager."
            )),
            (_, PlanAction::ShowCommand { command }) => {
                lines.push(format!(
                    "Installed with Homebrew ({exe}), but `brew` was not found. Upgrade it with:"
                ));
                lines.push(format!("  {command}"));
            }
            (_, PlanAction::BrewUpgrade { formula, .. }) => lines.push(format!(
                "Installed with Homebrew ({exe}). Upgrading runs `brew upgrade {}`, which installs the tap's latest release.",
                formula.name()
            )),
            (_, PlanAction::ReplaceBinary { target, asset }) => {
                lines.push(format!(
                    "Downloaded binary at {}. Upgrading downloads `{asset}` from release v{latest}, checks it, and replaces {}.",
                    target.display(),
                    target.display()
                ));
                lines.push(self.provenance_line());
            }
            (_, PlanAction::StagedInstall {
                target,
                asset,
                command,
                pkexec,
                ..
            }) => {
                lines.push(format!(
                    "Downloaded binary at {}, which you cannot write to.",
                    target.display()
                ));
                match pkexec {
                    Some(_) => lines.push(format!(
                        "Upgrading downloads `{asset}` from release v{latest}, checks it, and asks for your password to install it. Without the prompt, install it with:"
                    )),
                    None => lines.push(format!(
                        "Upgrading downloads `{asset}` from release v{latest} and checks it. Then install it with:"
                    )),
                }
                lines.push(format!("  {command}"));
                lines.push(self.provenance_line());
            }
            (_, PlanAction::InstallDeb {
                asset,
                command,
                pkexec,
                ..
            }) => {
                lines.push(format!(
                    "Installed from the Agent Deck `.deb` (package `{}`).",
                    super::DESKTOP_DEB_PACKAGE
                ));
                match pkexec {
                    Some(_) => lines.push(format!(
                        "Upgrading downloads `{asset}` from release v{latest}, checks it, and asks for your password to install it. Without the prompt, install it with:"
                    )),
                    None => lines.push(format!(
                        "Upgrading downloads `{asset}` from release v{latest} and checks it. Then install it with:"
                    )),
                }
                lines.push(format!("  {command}"));
                lines.push(self.provenance_line());
            }
            (_, PlanAction::SwapApp { app, asset, .. }) => {
                lines.push(format!(
                    "Agent Deck at {}. Upgrading downloads `{asset}` from release v{latest}, checks its checksum, signature and notarization, and replaces the app. Agent Deck then restarts to run v{latest}.",
                    app.display()
                ));
                lines.push(self.provenance_line());
            }
            (InstallMethod::DesktopDmg {
                app,
                parent_writable,
                team_id,
            }, PlanAction::ManualDownload { url }) => {
                let folder = app
                    .parent()
                    .map_or_else(|| "its folder".to_string(), |dir| dir.display().to_string());
                let why = if team_id.is_none() {
                    "This copy of Agent Deck is not signed, so it is not replaced in place.".to_string()
                } else if !parent_writable {
                    format!("{folder} is not writable by you, so Agent Deck cannot be replaced from here.")
                } else {
                    "Agent Deck cannot be replaced from here.".to_string()
                };
                lines.push(why);
                lines.push(format!(
                    "Download {url}, open it, and drag {DESKTOP_APP_BUNDLE} into {folder}, replacing the old one."
                ));
            }
            _ => lines.push(format!(
                "No release v{latest} build exists for this platform. See {}/releases.",
                crate::repo_identity::URL
            )),
        }
        lines
    }

    /// The plan as one block of text.
    pub fn text(&self) -> String {
        self.lines().join("\n")
    }

    /// Whether build provenance will be checked, said before the user
    /// confirms. The checksum is always checked.
    fn provenance_line(&self) -> String {
        match self.installation.tools.gh {
            Some(_) => "Its checksum is checked against the release's checksum file, and that file's build provenance with `gh attestation verify`.".to_string(),
            None => "Its checksum is checked against the release's checksum file. Build provenance is not checked, because the GitHub CLI (`gh`) is not installed.".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_upgrade::detect::{Platform, Tools};

    fn options() -> PlanOptions {
        PlanOptions {
            staging_root: PathBuf::from("/home/u/.local/state/dot-agent-deck/upgrade"),
            can_prompt_for_privilege: false,
        }
    }

    fn install(
        copy: CopyKind,
        exe: &str,
        platform: Platform,
        method: InstallMethod,
    ) -> Installation {
        Installation {
            copy,
            executable: PathBuf::from(exe),
            version: "0.45.0".into(),
            platform: Some(platform),
            method,
            tools: Tools::default(),
        }
    }

    fn cli(exe: &str, method: InstallMethod) -> Installation {
        install(CopyKind::Cli, exe, Platform::LinuxAmd64, method)
    }

    // ── One test per matrix row ──

    #[test]
    fn plan_001_linux_homebrew_runs_brew_upgrade_for_its_formula() {
        for formula in [HomebrewFormula::Stable, HomebrewFormula::Beta] {
            let mut found = cli(
                "/home/linuxbrew/.linuxbrew/Cellar/x/0.45.0/bin/dot-agent-deck",
                InstallMethod::Homebrew {
                    formula,
                    prefix: PathBuf::from("/home/linuxbrew/.linuxbrew"),
                },
            );
            found.tools.brew = Some(PathBuf::from("/home/linuxbrew/.linuxbrew/bin/brew"));
            let plan = plan(&found, "v0.46.0", &options());
            assert_eq!(
                plan.action,
                PlanAction::BrewUpgrade {
                    brew: PathBuf::from("/home/linuxbrew/.linuxbrew/bin/brew"),
                    formula,
                }
            );
            assert!(
                plan.text()
                    .contains(&format!("brew upgrade {}", formula.name()))
            );
            assert!(plan.is_actionable());
        }
    }

    #[test]
    fn plan_002_homebrew_without_brew_shows_the_command() {
        let found = cli(
            "/opt/homebrew/Cellar/dot-agent-deck/0.45.0/bin/dot-agent-deck",
            InstallMethod::Homebrew {
                formula: HomebrewFormula::Stable,
                prefix: PathBuf::from("/opt/homebrew"),
            },
        );
        let plan = plan(&found, "0.46.0", &options());
        assert_eq!(
            plan.action,
            PlanAction::ShowCommand {
                command: "brew upgrade dot-agent-deck".into()
            }
        );
        assert!(!plan.is_actionable());
    }

    #[test]
    fn plan_003_linux_downloaded_writable_replaces_in_place() {
        let exe = "/home/u/.local/bin/dot-agent-deck";
        for platform in [Platform::LinuxAmd64, Platform::LinuxArm64] {
            let found = install(
                CopyKind::Cli,
                exe,
                platform,
                InstallMethod::DownloadedWritable {
                    binary: PathBuf::from(exe),
                },
            );
            let plan = plan(&found, "0.46.0", &options());
            assert_eq!(
                plan.action,
                PlanAction::ReplaceBinary {
                    target: PathBuf::from(exe),
                    asset: platform.cli_asset().into(),
                }
            );
            assert!(plan.text().contains("`gh`) is not installed"));
        }
    }

    #[test]
    fn plan_004_linux_downloaded_non_writable_shows_exact_sudo_install() {
        let exe = "/usr/local/bin/dot-agent-deck";
        let found = cli(
            exe,
            InstallMethod::DownloadedNonWritable {
                binary: PathBuf::from(exe),
            },
        );
        let plan = plan(&found, "0.46.0", &options());
        let staged =
            "/home/u/.local/state/dot-agent-deck/upgrade/v0.46.0/dot-agent-deck-linux-amd64";
        assert_eq!(
            plan.action,
            PlanAction::StagedInstall {
                target: PathBuf::from(exe),
                asset: "dot-agent-deck-linux-amd64".into(),
                staged: PathBuf::from(staged),
                command: format!("sudo install -m 0755 {staged} {exe}"),
                pkexec: None,
            }
        );
        assert!(
            plan.text()
                .contains(&format!("  sudo install -m 0755 {staged} {exe}"))
        );
    }

    #[test]
    fn plan_005_non_writable_uses_pkexec_only_where_the_client_can_prompt() {
        let exe = "/usr/local/bin/dot-agent-deck";
        let mut found = cli(
            exe,
            InstallMethod::DownloadedNonWritable {
                binary: PathBuf::from(exe),
            },
        );
        found.tools.pkexec = Some(PathBuf::from("/usr/bin/pkexec"));
        let terminal = plan(&found, "0.46.0", &options());
        assert!(matches!(
            terminal.action,
            PlanAction::StagedInstall { pkexec: None, .. }
        ));
        let desktop = plan(
            &found,
            "0.46.0",
            &PlanOptions {
                can_prompt_for_privilege: true,
                ..options()
            },
        );
        assert!(matches!(
            desktop.action,
            PlanAction::StagedInstall {
                pkexec: Some(_),
                ..
            }
        ));
        assert!(desktop.text().contains("asks for your password"));
    }

    #[test]
    fn plan_006_nix_is_notify_only_with_flake_advice() {
        let found = cli(
            "/nix/store/x-dot-agent-deck/bin/dot-agent-deck",
            InstallMethod::Nix,
        );
        let plan = plan(&found, "0.46.0", &options());
        assert_eq!(plan.action, PlanAction::NotifyOnly);
        assert!(plan.text().contains("Update your flake input"));
        assert!(plan.confirm_question().is_none());
    }

    #[test]
    fn plan_007_source_build_is_notify_only() {
        let found = cli(
            "/home/u/deck/target/release/dot-agent-deck",
            InstallMethod::Source {
                reason: SourceReason::BuildTree,
            },
        );
        let plan = plan(&found, "0.46.0", &options());
        assert_eq!(plan.action, PlanAction::NotifyOnly);
        assert!(plan.text().contains("Built from source"));
    }

    #[test]
    fn plan_008_desktop_deb_shows_apt_command_or_pkexec() {
        let mut found = install(
            CopyKind::Desktop,
            "/usr/bin/dot-agent-deck-desktop",
            Platform::LinuxAmd64,
            InstallMethod::DesktopDeb,
        );
        let staged = "/home/u/.local/state/dot-agent-deck/upgrade/v0.46.0/dot-agent-deck-desktop-alpha-linux-amd64.deb";
        let terminal = plan(&found, "0.46.0", &options());
        assert_eq!(
            terminal.action,
            PlanAction::InstallDeb {
                asset: "dot-agent-deck-desktop-alpha-linux-amd64.deb".into(),
                staged: PathBuf::from(staged),
                command: format!("sudo apt install {staged}"),
                pkexec: None,
            }
        );
        found.tools.pkexec = Some(PathBuf::from("/usr/bin/pkexec"));
        let desktop = plan(
            &found,
            "0.46.0",
            &PlanOptions {
                can_prompt_for_privilege: true,
                ..options()
            },
        );
        assert!(matches!(
            desktop.action,
            PlanAction::InstallDeb {
                pkexec: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn plan_009_macos_dmg_in_applications_swaps_in_place() {
        let found = install(
            CopyKind::Desktop,
            "/Applications/Agent Deck.app/Contents/MacOS/dot-agent-deck-desktop",
            Platform::MacosArm64,
            InstallMethod::DesktopDmg {
                app: PathBuf::from("/Applications/Agent Deck.app"),
                parent_writable: true,
                team_id: Some("TEAM123".into()),
            },
        );
        let plan = plan(&found, "0.46.0", &options());
        assert_eq!(
            plan.action,
            PlanAction::SwapApp {
                app: PathBuf::from("/Applications/Agent Deck.app"),
                asset: "dot-agent-deck-desktop-alpha-macos-arm64.dmg".into(),
                team_id: "TEAM123".into(),
            }
        );
        assert!(plan.text().contains("signature and notarization"));
    }

    #[test]
    fn plan_010_macos_dmg_in_non_writable_dir_says_what_to_do() {
        let found = install(
            CopyKind::Desktop,
            "/Volumes/Shared/Agent Deck.app/Contents/MacOS/dot-agent-deck-desktop",
            Platform::MacosArm64,
            InstallMethod::DesktopDmg {
                app: PathBuf::from("/Volumes/Shared/Agent Deck.app"),
                parent_writable: false,
                team_id: Some("TEAM123".into()),
            },
        );
        let plan = plan(&found, "0.46.0", &options());
        let url = release_asset_url("0.46.0", "dot-agent-deck-desktop-alpha-macos-arm64.dmg");
        assert_eq!(plan.action, PlanAction::ManualDownload { url: url.clone() });
        let text = plan.text();
        assert!(
            text.contains("/Volumes/Shared is not writable by you"),
            "{text}"
        );
        assert!(text.contains(&url), "{text}");
        assert!(!plan.is_actionable());
    }

    #[test]
    fn plan_011_unsigned_running_app_is_not_swapped() {
        let found = install(
            CopyKind::Desktop,
            "/Applications/Agent Deck.app/Contents/MacOS/dot-agent-deck-desktop",
            Platform::MacosArm64,
            InstallMethod::DesktopDmg {
                app: PathBuf::from("/Applications/Agent Deck.app"),
                parent_writable: true,
                team_id: None,
            },
        );
        let plan = plan(&found, "0.46.0", &options());
        assert!(matches!(plan.action, PlanAction::ManualDownload { .. }));
        assert!(plan.text().contains("not signed"));
    }

    #[test]
    fn plan_012_macos_cli_rows_match_linux() {
        // macOS (Apple silicon and Intel) CLI rows: the same actions as the
        // Linux rows for the same method, with the darwin asset.
        let exe = "/Users/u/.local/bin/dot-agent-deck";
        for platform in [Platform::MacosArm64, Platform::MacosAmd64] {
            let found = install(
                CopyKind::Cli,
                exe,
                platform,
                InstallMethod::DownloadedWritable {
                    binary: PathBuf::from(exe),
                },
            );
            assert_eq!(
                plan(&found, "0.46.0", &options()).action,
                PlanAction::ReplaceBinary {
                    target: PathBuf::from(exe),
                    asset: platform.cli_asset().into(),
                }
            );
        }
    }

    #[test]
    fn plan_013_up_to_date_offers_nothing() {
        let exe = "/home/u/.local/bin/dot-agent-deck";
        let found = cli(
            exe,
            InstallMethod::DownloadedWritable {
                binary: PathBuf::from(exe),
            },
        );
        for latest in ["0.45.0", "v0.44.9", "garbage"] {
            let plan = plan(&found, latest, &options());
            assert_eq!(plan.action, PlanAction::UpToDate);
            assert_eq!(plan.lines().len(), 1);
        }
    }

    #[test]
    fn plan_014_headline_matches_the_tui_badge_and_paths_are_quoted() {
        let exe = "/opt/my tools/dot-agent-deck";
        let mut found = cli(
            exe,
            InstallMethod::DownloadedNonWritable {
                binary: PathBuf::from(exe),
            },
        );
        found.tools.gh = Some(PathBuf::from("/usr/bin/gh"));
        let plan = plan(&found, "0.46.0", &options());
        assert_eq!(
            plan.headline(),
            "dot-agent-deck: update available: v0.46.0 (current: v0.45.0)"
        );
        let PlanAction::StagedInstall { command, .. } = &plan.action else {
            panic!("expected a staged install");
        };
        assert!(
            command.ends_with("'/opt/my tools/dot-agent-deck'"),
            "{command}"
        );
        assert!(plan.text().contains("gh attestation verify"));
        assert_eq!(
            plan.confirm_question().as_deref(),
            Some("Upgrade dot-agent-deck to v0.46.0?")
        );
    }
}
