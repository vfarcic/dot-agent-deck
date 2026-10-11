//! Finding the other copy on this machine: the desktop app from the CLI, and
//! the CLI from the desktop app (issue #1635, goal 4).
//!
//! A copy counts as installed when its file exists and answers `--version`.
//! The one found is inspected through its OWN install method
//! ([`super::detect::inspect`]), so it is upgraded through its own row of the
//! matrix, never the running copy's.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::detect::{self, CopyKind, Installation, Platform, app_bundle_of};
use super::{
    CLI_BINARY, DEB_BUNDLED_CLI, DESKTOP_APP_BUNDLE, DESKTOP_DEB_PACKAGE, Host, reported_version,
};

/// What looking for the other copy found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OtherCopy {
    /// There is no desktop app for this platform (Linux arm64, macOS Intel,
    /// WSL), so "upgrade both" is the CLI alone. Not an error.
    NotOffered,
    /// The other copy is not installed.
    NotFound,
    Found(Box<Installation>),
}

/// Where the desktop app looks for a CLI after `PATH`.
const WELL_KNOWN_CLI_DIRS: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "~/.local/bin",
    "/home/linuxbrew/.linuxbrew/bin",
];

/// `path`, an absolute system location this module looks in for the other
/// copy (`/Applications`, the `.deb`'s `/usr/bin/dot-agent-deck`, the
/// well-known CLI folders). Under the `e2e` feature only,
/// `DOT_AGENT_DECK_TEST_SYSTEM_ROOT` re-roots it under a folder the test owns,
/// so an L2 test finds only the copies it put there and never one installed
/// on the machine running it. Gated on the feature for the reason
/// `effective_current_exe` in `src/platform/paths.rs` gives.
fn system_path(path: &str) -> PathBuf {
    #[cfg(feature = "e2e")]
    if let Some(root) =
        std::env::var_os("DOT_AGENT_DECK_TEST_SYSTEM_ROOT").filter(|root| !root.is_empty())
    {
        return Path::new(&root).join(path.trim_start_matches('/'));
    }
    PathBuf::from(path)
}

/// Find the copy that is not `running`. `path` is the `PATH` to search for a
/// CLI (the desktop app passes its login shell's); `None` searches the
/// host's own.
pub fn other_copy(host: &dyn Host, running: &Installation, path: Option<&OsStr>) -> OtherCopy {
    match running.copy {
        CopyKind::Cli => find_desktop(host, running.platform),
        CopyKind::Desktop => find_cli(host, running, path),
    }
}

/// The desktop app, from the CLI's side.
pub fn find_desktop(host: &dyn Host, platform: Option<Platform>) -> OtherCopy {
    let Some(platform) = platform.filter(|p| p.desktop_asset().is_some()) else {
        return OtherCopy::NotOffered;
    };
    if host.is_wsl() {
        return OtherCopy::NotOffered;
    }
    let candidates: Vec<PathBuf> = if platform.is_macos() {
        let mut apps = vec![system_path("/Applications").join(DESKTOP_APP_BUNDLE)];
        if let Some(home) = host.home() {
            apps.push(home.join("Applications").join(DESKTOP_APP_BUNDLE));
        }
        apps.into_iter()
            .map(|app| app.join("Contents/MacOS").join(CLI_BINARY))
            .collect()
    } else if deb_installed(host) {
        vec![system_path(DEB_BUNDLED_CLI)]
    } else {
        Vec::new()
    };
    candidates
        .iter()
        .find_map(|candidate| answering_copy(host, CopyKind::Desktop, candidate, Some(platform)))
        .map_or(OtherCopy::NotFound, |found| {
            OtherCopy::Found(Box::new(found))
        })
}

/// Whether dpkg reports the desktop package as installed.
fn deb_installed(host: &dyn Host) -> bool {
    let Some(dpkg_query) = host.find_program("dpkg-query") else {
        return false;
    };
    host.run(
        &dpkg_query,
        &[
            OsStr::new("-W"),
            OsStr::new("-f=${db:Status-Abbrev}"),
            OsStr::new(DESKTOP_DEB_PACKAGE),
        ],
    )
    .is_ok_and(|out| out.success && out.stdout.starts_with("ii"))
}

/// The CLI, from the desktop app's side: `PATH`, then the well-known install
/// directories. The CLI bundled inside the running app (or any app bundle, or
/// the `.deb`) upgrades with the app and is skipped.
pub fn find_cli(host: &dyn Host, running: &Installation, path: Option<&OsStr>) -> OtherCopy {
    let home = host.home();
    let mut dirs: Vec<PathBuf> = match path {
        Some(path) => std::env::split_paths(path).collect(),
        None => Vec::new(),
    };
    if path.is_none()
        && let Some(found) = host.find_program(CLI_BINARY)
        && let Some(dir) = found.parent()
    {
        dirs.push(dir.to_path_buf());
    }
    dirs.extend(
        WELL_KNOWN_CLI_DIRS
            .iter()
            .filter_map(|dir| match dir.strip_prefix("~/") {
                Some(rest) => home.as_ref().map(|home| home.join(rest)),
                None => Some(system_path(dir)),
            }),
    );
    let running_app = app_bundle_of(&running.executable);
    let mut seen = Vec::new();
    for dir in dirs.into_iter().filter(|dir| dir.is_absolute()) {
        let candidate = dir.join(CLI_BINARY);
        if !host.is_executable(&candidate) {
            continue;
        }
        let Some(canonical) = host.canonicalize(&candidate) else {
            continue;
        };
        let bundled = canonical == running.executable
            || app_bundle_of(&canonical).is_some()
            || running_app
                .as_ref()
                .is_some_and(|app| canonical.starts_with(app));
        if bundled || seen.contains(&canonical) {
            continue;
        }
        seen.push(canonical.clone());
        if let Some(found) = answering_copy(host, CopyKind::Cli, &canonical, running.platform)
            && found.copy == CopyKind::Cli
        {
            return OtherCopy::Found(Box::new(found));
        }
    }
    OtherCopy::NotFound
}

/// The copy at `candidate`, when it exists and answers `--version`.
fn answering_copy(
    host: &dyn Host,
    copy: CopyKind,
    candidate: &Path,
    platform: Option<Platform>,
) -> Option<Installation> {
    if !host.exists(candidate) {
        return None;
    }
    let canonical = host.canonicalize(candidate)?;
    let version = reported_version(host, &canonical)?;
    Some(detect::inspect(
        host, copy, &canonical, &version, None, platform,
    ))
}

// Every test here models a Unix install layout: native Windows is
// unsupported (#164).
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::self_upgrade::detect::{HomebrewFormula, InstallMethod, Tools};
    use crate::self_upgrade::plan::{PlanAction, PlanOptions, plan};
    use crate::self_upgrade::test_host::{FakeHost, ok};

    fn running(
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

    fn running_cli(platform: Platform) -> Installation {
        let exe = "/home/u/.local/bin/dot-agent-deck";
        running(
            CopyKind::Cli,
            exe,
            platform,
            InstallMethod::DownloadedWritable {
                binary: PathBuf::from(exe),
            },
        )
    }

    fn options() -> PlanOptions {
        PlanOptions {
            staging_root: PathBuf::from("/stage"),
            can_prompt_for_privilege: false,
            provenance: crate::self_upgrade::ProvenanceCheck::Unavailable {
                reason: crate::self_upgrade::verify::GH_NOT_INSTALLED.into(),
            },
        }
    }

    #[test]
    fn discover_001_no_desktop_on_linux_arm64_or_macos_intel() {
        let host = FakeHost::new();
        for platform in [Platform::LinuxArm64, Platform::MacosAmd64] {
            assert_eq!(
                other_copy(&host, &running_cli(platform), None),
                OtherCopy::NotOffered
            );
        }
    }

    #[test]
    fn discover_002_no_desktop_under_wsl() {
        let host = FakeHost::new().wsl();
        assert_eq!(
            other_copy(&host, &running_cli(Platform::LinuxAmd64), None),
            OtherCopy::NotOffered
        );
    }

    #[test]
    fn discover_003_cli_finds_dmg_app_in_applications() {
        let bundled = "/Applications/Agent Deck.app/Contents/MacOS/dot-agent-deck";
        let host = FakeHost::new()
            .deck(bundled, "0.45.0")
            .writable("/Applications")
            .home("/Users/u")
            .answer(
                "/usr/bin/codesign -dv --verbose=2 /Applications/Agent Deck.app",
                crate::self_upgrade::CommandOutput {
                    success: true,
                    code: Some(0),
                    stdout: String::new(),
                    stderr: "TeamIdentifier=TEAM123\n".into(),
                },
            );
        let OtherCopy::Found(found) = other_copy(&host, &running_cli(Platform::MacosArm64), None)
        else {
            panic!("the app should be found");
        };
        assert_eq!(found.copy, CopyKind::Desktop);
        assert!(matches!(
            plan(&found, &"0.46.0".into(), &options()).action,
            PlanAction::SwapApp { .. }
        ));
    }

    #[test]
    fn discover_004_cli_finds_app_in_home_applications() {
        let bundled = "/Users/u/Applications/Agent Deck.app/Contents/MacOS/dot-agent-deck";
        let host = FakeHost::new().deck(bundled, "0.45.0").home("/Users/u");
        let OtherCopy::Found(found) = other_copy(&host, &running_cli(Platform::MacosArm64), None)
        else {
            panic!("the app should be found");
        };
        assert_eq!(
            found.executable,
            PathBuf::from(bundled),
            "the app in ~/Applications is found"
        );
    }

    #[test]
    fn discover_005_app_that_does_not_answer_is_not_installed() {
        let host = FakeHost::new()
            .exe("/Applications/Agent Deck.app/Contents/MacOS/dot-agent-deck")
            .home("/Users/u");
        assert_eq!(
            other_copy(&host, &running_cli(Platform::MacosArm64), None),
            OtherCopy::NotFound
        );
    }

    #[test]
    fn discover_006_tui_finds_deb_desktop_and_plans_its_install() {
        let host = FakeHost::new()
            .exe("/usr/bin/dpkg-query")
            .on_path("/usr/bin")
            .deck(DEB_BUNDLED_CLI, "0.45.0")
            .answer(
                "/usr/bin/dpkg-query -W -f=${db:Status-Abbrev} agent-deck",
                ok("ii "),
            )
            .answer(
                "/usr/bin/dpkg-query -S /usr/bin/dot-agent-deck",
                ok("agent-deck: /usr/bin/dot-agent-deck\n"),
            );
        let OtherCopy::Found(found) = other_copy(&host, &running_cli(Platform::LinuxAmd64), None)
        else {
            panic!("the .deb should be found");
        };
        assert_eq!(found.method, InstallMethod::DesktopDeb);
        let plan = plan(&found, &"0.46.0".into(), &options());
        assert!(
            plan.text()
                .contains("from release v0.46.0 and checks it. The command to install it"),
            "{}",
            plan.text()
        );
    }

    #[test]
    fn discover_007_deb_not_installed_is_not_found() {
        let host = FakeHost::new()
            .exe("/usr/bin/dpkg-query")
            .on_path("/usr/bin")
            .answer(
                "/usr/bin/dpkg-query -W -f=${db:Status-Abbrev} agent-deck",
                ok("un "),
            );
        assert_eq!(
            other_copy(&host, &running_cli(Platform::LinuxAmd64), None),
            OtherCopy::NotFound
        );
    }

    #[test]
    fn discover_008_dmg_app_finds_homebrew_cli_and_plans_brew() {
        let keg = "/opt/homebrew/Cellar/dot-agent-deck/0.45.0/bin/dot-agent-deck";
        let host = FakeHost::new()
            .link("/opt/homebrew/bin/dot-agent-deck", keg)
            .deck(keg, "0.45.0")
            .exe("/opt/homebrew/bin/brew")
            .home("/Users/u");
        let app = running(
            CopyKind::Desktop,
            "/Applications/Agent Deck.app/Contents/MacOS/dot-agent-deck-desktop",
            Platform::MacosArm64,
            InstallMethod::DesktopDmg {
                app: PathBuf::from("/Applications/Agent Deck.app"),
                parent_writable: true,
                team_id: Some("T".into()),
            },
        );
        let OtherCopy::Found(found) = other_copy(&host, &app, Some(OsStr::new("/usr/bin:/bin")))
        else {
            panic!("the Homebrew CLI should be found");
        };
        assert_eq!(found.copy, CopyKind::Cli);
        assert_eq!(
            plan(&found, &"0.46.0".into(), &options()).action,
            PlanAction::BrewUpgrade {
                brew: PathBuf::from("/opt/homebrew/bin/brew"),
                formula: HomebrewFormula::Stable,
            }
        );
    }

    #[test]
    fn discover_009_desktop_skips_its_own_bundled_cli_and_searches_path_first() {
        let bundled = "/Applications/Agent Deck.app/Contents/MacOS/dot-agent-deck";
        let host = FakeHost::new()
            .link("/usr/local/bin/dot-agent-deck", bundled)
            .deck(bundled, "0.45.0")
            .deck("/Users/u/bin/dot-agent-deck", "0.45.0")
            .writable("/Users/u/bin")
            .home("/Users/u");
        let app = running(
            CopyKind::Desktop,
            bundled,
            Platform::MacosArm64,
            InstallMethod::DesktopDmg {
                app: PathBuf::from("/Applications/Agent Deck.app"),
                parent_writable: true,
                team_id: Some("T".into()),
            },
        );
        let OtherCopy::Found(found) =
            other_copy(&host, &app, Some(OsStr::new("/usr/local/bin:/Users/u/bin")))
        else {
            panic!("the CLI on PATH should be found");
        };
        assert_eq!(
            found.executable,
            PathBuf::from("/Users/u/bin/dot-agent-deck")
        );
        assert_eq!(
            found.method,
            InstallMethod::DownloadedWritable {
                binary: PathBuf::from("/Users/u/bin/dot-agent-deck")
            }
        );
    }

    #[test]
    fn discover_010_deb_desktop_skips_its_packaged_cli() {
        let host = FakeHost::new()
            .exe("/usr/bin/dpkg-query")
            .deck(DEB_BUNDLED_CLI, "0.45.0")
            .answer(
                "/usr/bin/dpkg-query -S /usr/bin/dot-agent-deck",
                ok("agent-deck: /usr/bin/dot-agent-deck\n"),
            )
            .on_path("/usr/bin")
            .home("/home/u");
        let app = running(
            CopyKind::Desktop,
            "/usr/bin/dot-agent-deck-desktop",
            Platform::LinuxAmd64,
            InstallMethod::DesktopDeb,
        );
        assert_eq!(
            other_copy(&host, &app, Some(OsStr::new("/usr/bin"))),
            OtherCopy::NotFound,
            "the .deb's own /usr/bin/dot-agent-deck upgrades with the package"
        );
    }

    #[test]
    fn discover_011_desktop_finds_cli_in_local_bin_when_path_lacks_it() {
        let host = FakeHost::new()
            .deck("/home/u/.local/bin/dot-agent-deck", "0.45.0")
            .writable("/home/u/.local/bin")
            .home("/home/u");
        let app = running(
            CopyKind::Desktop,
            "/usr/bin/dot-agent-deck-desktop",
            Platform::LinuxAmd64,
            InstallMethod::DesktopDeb,
        );
        let OtherCopy::Found(found) = other_copy(&host, &app, Some(OsStr::new("/usr/bin"))) else {
            panic!("~/.local/bin should be searched");
        };
        assert!(matches!(
            plan(&found, &"0.46.0".into(), &options()).action,
            PlanAction::ReplaceBinary { .. }
        ));
    }
}
