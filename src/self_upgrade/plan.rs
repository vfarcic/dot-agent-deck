//! What to offer for one installed copy and a newer release, and every word a
//! client shows about it.
//!
//! [`plan`] is pure. Both clients render [`UpgradePlan::lines`] as they are, so
//! the TUI, the desktop app and `dot-agent-deck upgrade` say the same thing
//! (CLAUDE.md rule 22). Nothing here acts: a plan is shown, and only
//! [`super::execute::execute`] carries out one the user confirmed.

use std::path::{Path, PathBuf};

use super::detect::{
    CopyKind, HomebrewFormula, InstallMethod, Installation, Platform, SourceReason,
};
use super::verify::ProvenanceCheck;
use super::{DESKTOP_APP_BUNDLE, Host, shell_word};

/// What [`plan`] needs to know about the client that will show the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanOptions {
    /// The folder each upgrade creates its own private, randomly named staging
    /// directory in ([`super::execute::execute`]). It must be a directory the
    /// user owns that nobody else can write to; an upgrade refuses it
    /// otherwise. Because the staging directory's name is only known once it
    /// exists, a plan cannot name the staged file: the install command is shown
    /// once the download is verified.
    pub staging_root: PathBuf,
    /// Whether this client can raise a graphical privilege prompt
    /// (`pkexec`) itself. The desktop app on Linux can; a terminal client
    /// cannot without fighting its own screen, so it shows the command.
    pub can_prompt_for_privilege: bool,
    /// Whether build provenance can be checked on this machine
    /// ([`ProvenanceCheck::detect`]), decided before the plan is shown so the
    /// plan says which it will be, and carried into the plan so an upgrade
    /// keeps that promise.
    pub provenance: ProvenanceCheck,
}

impl PlanOptions {
    /// The folder upgrades stage their downloads in: `upgrade` under the deck's
    /// state directory.
    pub fn default_staging_root() -> PathBuf {
        crate::platform::paths::state_dir().join("upgrade")
    }

    /// The defaults for a terminal client: staging under the deck's state
    /// directory, no privilege prompt, and provenance as `gh` on `host`
    /// allows. Runs `gh auth status`.
    pub fn terminal(host: &dyn Host) -> Self {
        Self {
            staging_root: Self::default_staging_root(),
            can_prompt_for_privilege: false,
            provenance: ProvenanceCheck::detect(host),
        }
    }
}

/// One line of a plan or a result: prose, or a command the user runs. A client
/// shows a command as code and may offer to copy it; [`PlanLine::Command`]
/// carries the exact command, never shortened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanLine {
    Text(String),
    Command(String),
}

impl PlanLine {
    /// The line as a terminal prints it: a command indented by two spaces.
    pub fn render(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Command(command) => format!("  {command}"),
        }
    }
}

/// Render `items` as the lines a terminal prints.
pub fn render_lines(items: &[PlanLine]) -> Vec<String> {
    items.iter().map(PlanLine::render).collect()
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
    /// Download `asset` and verify it into a private staging directory; the
    /// user then runs the `sudo install …` command shown once it is verified,
    /// because `target` is not writable — or, with `pkexec`, the client
    /// installs it behind a privilege prompt.
    StagedInstall {
        target: PathBuf,
        asset: String,
        pkexec: Option<PathBuf>,
    },
    /// Download the `.deb` and verify it into a private staging directory, and
    /// install it with `pkexec apt-get install` when `pkexec` is set, otherwise
    /// show the `sudo apt install …` command once it is verified.
    InstallDeb {
        asset: String,
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
    /// Whether build provenance will be checked, as the plan said.
    pub provenance: ProvenanceCheck,
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
        provenance: options.provenance.clone(),
    }
}

fn action_for(installation: &Installation, latest: &str, options: &PlanOptions) -> PlanAction {
    let platform = installation.platform;
    let cli_asset = platform.map(|p| p.cli_asset().to_string());
    let desktop_asset = platform.and_then(|p| p.desktop_asset()).map(str::to_string);
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
            Some(asset) => PlanAction::StagedInstall {
                target: binary.clone(),
                asset,
                pkexec: pkexec(),
            },
            None => PlanAction::NotifyOnly,
        },
        InstallMethod::DesktopDeb => match desktop_asset {
            Some(asset) => PlanAction::InstallDeb {
                asset,
                pkexec: pkexec(),
            },
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

/// The longest path, in characters, a shown command names. A longer one is
/// not shown as a command at all rather than shown shortened.
pub const MAX_SHOWN_PATH_CHARS: usize = 1024;

/// Whether `c` is an invisible formatting character: the bidi controls, the
/// zero-width characters, the soft hyphen and the byte-order mark. A path
/// carrying one reads as something other than what a shell runs.
fn is_format_char(c: char) -> bool {
    crate::untrusted_text::is_bidi_format_char(c)
        || matches!(
            c,
            '\u{00AD}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200D}'
                | '\u{2060}'..='\u{2064}'
                | '\u{206A}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
        )
}

/// `path` as it can appear in a shown command, or `None` when it cannot be
/// shown faithfully: not UTF-8, longer than [`MAX_SHOWN_PATH_CHARS`], or
/// carrying a control or formatting character, which a reader would not see
/// but a shell would run. A backslash is refused too, because `sha256sum -c`
/// reads one in a file name as an escape.
fn shown_path(path: &Path) -> Option<&str> {
    let text = path.to_str()?;
    let clean = !text.is_empty()
        && text.chars().count() <= MAX_SHOWN_PATH_CHARS
        && !text
            .chars()
            .any(|c| c.is_control() || is_format_char(c) || c == '\\');
    clean.then_some(text)
}

/// `echo '<sha256>  <staged>' | sha256sum -c - && `, the checksum check a
/// shown command runs before `sudo`, so the file it installs is the one that
/// was verified even if it changed after the deck checked it. macOS has
/// `shasum -a 256` rather than `sha256sum`.
fn checksum_prefix(platform: Option<Platform>, staged: &str, sha256: &str) -> String {
    let checker = if platform.is_some_and(Platform::is_macos) {
        "shasum -a 256 -c -"
    } else {
        "sha256sum -c -"
    };
    format!(
        "echo {} | {checker} && ",
        shell_word(&format!("{sha256}  {staged}"))
    )
}

/// The command that installs the verified binary at `staged` over `target`:
/// a checksum check, then `sudo install -m 0755 <staged> <target>`, quoted for
/// a POSIX shell. `None` when either path cannot be shown faithfully
/// ([`shown_path`]); the client then shows [`manual_upgrade_line`].
pub fn install_binary_command(
    platform: Option<Platform>,
    staged: &Path,
    target: &Path,
    sha256: &str,
) -> Option<String> {
    let (staged, target) = (shown_path(staged)?, shown_path(target)?);
    Some(format!(
        "{}sudo install -m 0755 {} {}",
        checksum_prefix(platform, staged, sha256),
        shell_word(staged),
        shell_word(target)
    ))
}

/// The command that installs the verified `.deb` at `staged`: a checksum
/// check, then `sudo apt install <staged>`. `None` as for
/// [`install_binary_command`].
pub fn install_deb_command(staged: &Path, sha256: &str) -> Option<String> {
    let staged = shown_path(staged)?;
    Some(format!(
        "{}sudo apt install {}",
        checksum_prefix(Some(Platform::LinuxAmd64), staged, sha256),
        shell_word(staged)
    ))
}

/// What to do when no install command can be shown for a verified download.
pub fn manual_upgrade_line(version: &str) -> String {
    format!(
        "No install command is shown, because a path in it is too long or contains characters that cannot be shown safely. Upgrade manually from {}/releases/tag/v{version}.",
        crate::repo_identity::URL
    )
}

/// What the desktop app says when its privilege prompt did not install a file
/// that was downloaded and checked, before the command that installs it.
pub const PROMPT_FAILED: &str =
    "It was downloaded and checked, but not installed. Install it with:";

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

    /// The whole plan as the lines a terminal prints ([`Self::items`]
    /// rendered).
    pub fn lines(&self) -> Vec<String> {
        render_lines(&self.items())
    }

    /// The whole plan, line by line: the headline, how the copy was installed,
    /// and what upgrading does or what the user does instead.
    pub fn items(&self) -> Vec<PlanLine> {
        let mut lines = vec![PlanLine::Text(self.headline())];
        if self.action == PlanAction::UpToDate {
            return lines;
        }
        let latest = &self.latest;
        let installation = &self.installation;
        let exe = installation.executable.display();
        let text = PlanLine::Text;
        match (&installation.method, &self.action) {
            (InstallMethod::Nix, _) => lines.push(text(format!(
                "Installed with Nix ({exe}), so it is not changed from here. Update your flake input (for example `nix flake update`) and rebuild, or run `nix profile upgrade`."
            ))),
            (InstallMethod::Source { reason }, _) => {
                let how = match reason {
                    SourceReason::BuildTree => "it runs from a cargo build directory",
                    SourceReason::CargoInstall => "it was installed with `cargo install`",
                    SourceReason::DirtyTree => "it was built from a checkout with uncommitted changes",
                };
                lines.push(text(format!(
                    "Built from source ({how}: {exe}), so it is not replaced from here. Check out v{latest} and rebuild."
                )));
            }
            (InstallMethod::SystemPackage { package }, _) => lines.push(text(format!(
                "Installed by the system package `{package}` ({exe}), so it is not replaced from here. Upgrade that package with your package manager."
            ))),
            (_, PlanAction::ShowCommand { command }) => {
                lines.push(text(format!(
                    "Installed with Homebrew ({exe}), but `brew` was not found. Upgrade it with:"
                )));
                lines.push(PlanLine::Command(command.clone()));
            }
            (_, PlanAction::BrewUpgrade { formula, .. }) => lines.push(text(format!(
                "Installed with Homebrew ({exe}). Upgrading runs `brew upgrade {}`, which installs the tap's latest release.",
                formula.name()
            ))),
            (_, PlanAction::ReplaceBinary { target, asset }) => {
                lines.push(text(format!(
                    "Downloaded binary at {}. Upgrading downloads `{asset}` from release v{latest}, checks it, and replaces {}.",
                    target.display(),
                    target.display()
                )));
                lines.push(text(self.provenance_line()));
            }
            (_, PlanAction::StagedInstall { target, asset, pkexec }) => {
                lines.push(text(format!(
                    "Downloaded binary at {}, which you cannot write to.",
                    target.display()
                )));
                lines.push(text(staged_install_line(asset, latest, pkexec.is_some())));
                lines.push(text(self.provenance_line()));
            }
            (_, PlanAction::InstallDeb { asset, pkexec }) => {
                lines.push(text(format!(
                    "Installed from the Agent Deck `.deb` (package `{}`).",
                    super::DESKTOP_DEB_PACKAGE
                )));
                lines.push(text(staged_install_line(asset, latest, pkexec.is_some())));
                lines.push(text(self.provenance_line()));
            }
            (_, PlanAction::SwapApp { app, asset, .. }) => {
                lines.push(text(format!(
                    "Agent Deck at {}. Upgrading downloads `{asset}` from release v{latest}, checks its checksum, signature and notarization, and replaces the app. Agent Deck then restarts to run v{latest}.",
                    app.display()
                )));
                lines.push(text(self.provenance_line()));
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
                lines.push(text(why));
                lines.push(text(format!(
                    "Download {url}, open it, and drag {DESKTOP_APP_BUNDLE} into {folder}, replacing the old one."
                )));
            }
            _ => lines.push(text(format!(
                "No release v{latest} build exists for this platform. See {}/releases.",
                crate::repo_identity::URL
            ))),
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
        match &self.provenance {
            ProvenanceCheck::Available { .. } => "Its checksum is checked against the release's checksum file, and that file's build provenance with `gh attestation verify`.".to_string(),
            ProvenanceCheck::Unavailable { reason } => format!(
                "Its checksum is checked against the release's checksum file. Build provenance will NOT be checked: {reason}."
            ),
        }
    }
}

/// What a staged install does, before the user confirms. The staged file's
/// path is not known until the download is verified, so neither is the
/// command that installs it.
fn staged_install_line(asset: &str, latest: &str, prompt: bool) -> String {
    if prompt {
        format!(
            "Upgrading downloads `{asset}` from release v{latest}, checks it, and asks for your password to install it. If the prompt does not install it, the command to install it is shown then."
        )
    } else {
        format!(
            "Upgrading downloads `{asset}` from release v{latest} and checks it. The command to install it, which checks the checksum again before `sudo`, is shown once the download is verified."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_upgrade::detect::Tools;

    fn options() -> PlanOptions {
        PlanOptions {
            staging_root: PathBuf::from("/home/u/.local/state/dot-agent-deck/upgrade"),
            can_prompt_for_privilege: false,
            provenance: ProvenanceCheck::Unavailable {
                reason: crate::self_upgrade::verify::GH_NOT_INSTALLED.into(),
            },
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
            assert!(plan.text().contains(
                "Build provenance will NOT be checked: the GitHub CLI (`gh`) is not installed."
            ));
        }
    }

    #[test]
    fn plan_004_linux_downloaded_non_writable_shows_the_command_once_verified() {
        let exe = "/usr/local/bin/dot-agent-deck";
        let found = cli(
            exe,
            InstallMethod::DownloadedNonWritable {
                binary: PathBuf::from(exe),
            },
        );
        let plan = plan(&found, "0.46.0", &options());
        assert_eq!(
            plan.action,
            PlanAction::StagedInstall {
                target: PathBuf::from(exe),
                asset: "dot-agent-deck-linux-amd64".into(),
                pkexec: None,
            }
        );
        // The staged path is not known before the download, so the plan names
        // no command; it says when the command is shown.
        assert!(
            !plan
                .items()
                .iter()
                .any(|line| matches!(line, PlanLine::Command(_))),
            "{plan:?}"
        );
        assert!(
            plan.text()
                .contains("is shown once the download is verified"),
            "{}",
            plan.text()
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
        let terminal = plan(&found, "0.46.0", &options());
        assert_eq!(
            terminal.action,
            PlanAction::InstallDeb {
                asset: "dot-agent-deck-desktop-alpha-linux-amd64.deb".into(),
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
        let found = cli(
            exe,
            InstallMethod::DownloadedNonWritable {
                binary: PathBuf::from(exe),
            },
        );
        let plan = plan(
            &found,
            "0.46.0",
            &PlanOptions {
                provenance: ProvenanceCheck::Available {
                    gh: PathBuf::from("/usr/bin/gh"),
                },
                ..options()
            },
        );
        assert_eq!(
            plan.headline(),
            "dot-agent-deck: update available: v0.46.0 (current: v0.45.0)"
        );
        let command = install_binary_command(
            plan.installation.platform,
            Path::new("/s/staged"),
            Path::new(exe),
            &"a".repeat(64),
        )
        .unwrap();
        assert!(
            command.ends_with("'/opt/my tools/dot-agent-deck'"),
            "{command}"
        );
        assert!(plan.text().contains("gh attestation verify"));
        assert!(!plan.text().contains("NOT be checked"));
        assert_eq!(
            plan.confirm_question().as_deref(),
            Some("Upgrade dot-agent-deck to v0.46.0?")
        );
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// The words of `command` as a POSIX shell splits them, for the plain
    /// subset a shown command uses: unquoted words, `'…'` runs and `'\''`.
    fn shell_words(command: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut word = String::new();
        let mut in_word = false;
        let mut chars = command.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\'' => {
                    in_word = true;
                    for q in chars.by_ref() {
                        if q == '\'' {
                            break;
                        }
                        word.push(q);
                    }
                }
                '\\' => {
                    in_word = true;
                    word.extend(chars.next());
                }
                ' ' => {
                    if in_word {
                        words.push(std::mem::take(&mut word));
                        in_word = false;
                    }
                }
                c => {
                    in_word = true;
                    word.push(c);
                }
            }
        }
        if in_word {
            words.push(word);
        }
        words
    }

    #[test]
    fn plan_015_shown_commands_check_the_checksum_before_sudo() {
        let staged = Path::new(
            "/home/u/.local/state/dot-agent-deck/upgrade/v0.46.0-ab12/dot-agent-deck-linux-amd64",
        );
        let target = Path::new("/usr/local/bin/dot-agent-deck");
        let linux =
            install_binary_command(Some(Platform::LinuxAmd64), staged, target, SHA).unwrap();
        assert_eq!(
            linux,
            format!(
                "echo '{SHA}  {}' | sha256sum -c - && sudo install -m 0755 {} {}",
                staged.display(),
                staged.display(),
                target.display()
            )
        );
        let mac = install_binary_command(Some(Platform::MacosArm64), staged, target, SHA).unwrap();
        assert!(
            mac.contains("| shasum -a 256 -c - && sudo install"),
            "{mac}"
        );

        let deb = install_deb_command(staged, SHA).unwrap();
        assert_eq!(
            deb,
            format!(
                "echo '{SHA}  {}' | sha256sum -c - && sudo apt install {}",
                staged.display(),
                staged.display()
            )
        );
    }

    #[test]
    fn plan_016_shown_commands_stay_correctly_quoted() {
        let staged = Path::new("/home/o'brien/my state/up/x");
        let target = Path::new("/opt/my tools/dot-agent-deck");
        let command =
            install_binary_command(Some(Platform::LinuxAmd64), staged, target, SHA).unwrap();
        let words = shell_words(&command);
        assert_eq!(
            words,
            [
                "echo",
                &format!("{SHA}  /home/o'brien/my state/up/x"),
                "|",
                "sha256sum",
                "-c",
                "-",
                "&&",
                "sudo",
                "install",
                "-m",
                "0755",
                "/home/o'brien/my state/up/x",
                "/opt/my tools/dot-agent-deck",
            ]
        );
    }

    #[test]
    fn plan_017_no_command_for_a_path_with_control_or_format_characters() {
        let target = Path::new("/usr/local/bin/dot-agent-deck");
        for bad in [
            "/home/u/state\n/x",
            "/home/u/st\u{1b}[2Jate/x",
            "/home/u/\u{202E}etats/x",
            "/home/u/sta\u{200B}te/x",
            "/home/u/state\u{FEFF}/x",
            "/home/u/state\u{0085}/x",
            "/home/u/back\\slash/x",
        ] {
            assert_eq!(
                install_binary_command(Some(Platform::LinuxAmd64), Path::new(bad), target, SHA),
                None,
                "{bad:?}"
            );
            assert_eq!(install_deb_command(Path::new(bad), SHA), None, "{bad:?}");
            assert_eq!(
                install_binary_command(Some(Platform::LinuxAmd64), target, Path::new(bad), SHA),
                None,
                "{bad:?} as the target"
            );
        }
        assert!(manual_upgrade_line("0.46.0").contains("/releases/tag/v0.46.0"));
    }

    #[test]
    fn plan_018_a_long_path_is_shown_whole_or_not_at_all() {
        let target = Path::new("/usr/local/bin/dot-agent-deck");
        let long_dir = format!("/{}", "segment-".repeat(120));
        let long = PathBuf::from(format!("{long_dir}/dot-agent-deck-linux-amd64"));
        assert!(long.to_str().unwrap().chars().count() <= MAX_SHOWN_PATH_CHARS);
        let command =
            install_binary_command(Some(Platform::LinuxAmd64), &long, target, SHA).unwrap();
        assert!(
            command.len() > 2048,
            "longer than the desktop's message cap"
        );
        assert_eq!(shell_words(&command)[11], long.to_str().unwrap());

        let too_long = PathBuf::from(format!("/{}", "x".repeat(MAX_SHOWN_PATH_CHARS)));
        assert_eq!(
            install_binary_command(Some(Platform::LinuxAmd64), &too_long, target, SHA),
            None
        );
    }

    #[test]
    fn plan_019_the_plan_says_why_provenance_will_not_be_checked() {
        let exe = "/home/u/.local/bin/dot-agent-deck";
        let found = cli(
            exe,
            InstallMethod::DownloadedWritable {
                binary: PathBuf::from(exe),
            },
        );
        let reason = "`gh auth status` failed: Timeout trying to log in";
        let plan = plan(
            &found,
            "0.46.0",
            &PlanOptions {
                provenance: ProvenanceCheck::Unavailable {
                    reason: reason.into(),
                },
                ..options()
            },
        );
        assert_eq!(
            plan.provenance,
            ProvenanceCheck::Unavailable {
                reason: reason.into()
            }
        );
        assert!(
            plan.text()
                .contains(&format!("Build provenance will NOT be checked: {reason}.")),
            "{}",
            plan.text()
        );
    }

    #[test]
    fn plan_020_a_beta_copy_follows_the_prerelease_channel_and_is_offered_a_newer_beta() {
        use crate::self_upgrade::{ReleaseChannel, release_channel};
        let mut brew_beta = cli(
            "/home/linuxbrew/.linuxbrew/Cellar/x/0.47.0-beta.1/bin/dot-agent-deck",
            InstallMethod::Homebrew {
                formula: HomebrewFormula::Beta,
                prefix: PathBuf::from("/home/linuxbrew/.linuxbrew"),
            },
        );
        brew_beta.version = "0.47.0-beta.1".into();
        brew_beta.tools.brew = Some(PathBuf::from("/home/linuxbrew/.linuxbrew/bin/brew"));
        assert_eq!(release_channel(&brew_beta), ReleaseChannel::Prerelease);
        let plan_beta = plan(&brew_beta, "v0.47.0-beta.2", &options());
        assert_eq!(plan_beta.latest, "0.47.0-beta.2");
        assert!(
            matches!(
                plan_beta.action,
                PlanAction::BrewUpgrade {
                    formula: HomebrewFormula::Beta,
                    ..
                }
            ),
            "{:?}",
            plan_beta.action
        );

        // A downloaded prerelease is on the same channel by its version alone.
        let exe = "/home/u/.local/bin/dot-agent-deck";
        let mut downloaded = cli(
            exe,
            InstallMethod::DownloadedWritable {
                binary: PathBuf::from(exe),
            },
        );
        downloaded.version = "0.47.0-beta.1".into();
        assert_eq!(release_channel(&downloaded), ReleaseChannel::Prerelease);
        assert!(matches!(
            plan(&downloaded, "0.47.0-beta.2", &options()).action,
            PlanAction::ReplaceBinary { .. }
        ));
        // Even the beta formula pinned to a stable-looking version follows
        // the formula's channel.
        brew_beta.version = "0.46.0".into();
        assert_eq!(release_channel(&brew_beta), ReleaseChannel::Prerelease);
    }

    #[test]
    fn plan_021_a_stable_copy_follows_the_stable_channel() {
        use crate::self_upgrade::{ReleaseChannel, release_channel};
        let exe = "/home/u/.local/bin/dot-agent-deck";
        let downloaded = cli(
            exe,
            InstallMethod::DownloadedWritable {
                binary: PathBuf::from(exe),
            },
        );
        assert_eq!(release_channel(&downloaded), ReleaseChannel::Stable);
        let brew_stable = cli(
            "/home/linuxbrew/.linuxbrew/Cellar/x/0.45.0/bin/dot-agent-deck",
            InstallMethod::Homebrew {
                formula: HomebrewFormula::Stable,
                prefix: PathBuf::from("/home/linuxbrew/.linuxbrew"),
            },
        );
        assert_eq!(release_channel(&brew_stable), ReleaseChannel::Stable);
    }
}
