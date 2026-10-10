//! Carrying out a plan the user confirmed.
//!
//! The network half ([`ReleaseSource`], [`download_verified`]) is async; what
//! it hands on is bytes that already passed [`super::verify`], and the
//! replacing half ([`atomic_replace`], [`swap_app`], …) is synchronous and runs
//! every subprocess through a [`Host`] by absolute path.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::plan::{PlanAction, PlanOptions, UpgradePlan};
use super::verify::{self, Provenance};
use super::{
    CLI_BINARY, CLI_MANIFEST, DESKTOP_APP_BUNDLE, DESKTOP_MANIFEST, Host, UpgradeError, run_checked,
};

/// The largest asset accepted (the desktop disk image is the largest, at tens
/// of megabytes).
const MAX_DOWNLOAD_BYTES: usize = 512 * 1024 * 1024;

/// Where releases are looked up and downloaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseSource {
    /// A GitHub `releases/latest` API endpoint.
    pub api_url: String,
    /// The base release assets hang off: `<base>/v<version>/<asset>`.
    pub download_base: String,
}

impl ReleaseSource {
    /// This build's repository ([`crate::repo_identity`]).
    ///
    /// Under the `e2e` feature only, `DOT_AGENT_DECK_TEST_RELEASES_API_URL` and
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
            download_base: crate::repo_identity::RELEASE_DOWNLOAD_BASE.to_string(),
        };
        #[cfg(feature = "e2e")]
        {
            if let Ok(url) = std::env::var("DOT_AGENT_DECK_TEST_RELEASES_API_URL") {
                source.api_url = url;
            }
            if let Ok(base) = std::env::var("DOT_AGENT_DECK_TEST_RELEASE_DOWNLOAD_BASE") {
                source.download_base = base;
            }
        }
        source
    }

    /// The latest release's version, without a leading `v`.
    pub async fn latest_version(&self) -> Result<String, UpgradeError> {
        let tag = crate::version::fetch_latest_tag(&self.api_url)
            .await
            .map_err(UpgradeError::ReleaseLookup)?;
        let version = tag.strip_prefix('v').unwrap_or(&tag);
        semver::Version::parse(version).map_err(|_| {
            UpgradeError::ReleaseLookup(format!("`{tag}` is not a release version"))
        })?;
        Ok(version.to_string())
    }

    pub fn asset_url(&self, version: &str, asset: &str) -> String {
        format!(
            "{}/v{version}/{asset}",
            self.download_base.trim_end_matches('/')
        )
    }
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
    pub provenance: Provenance,
}

/// Download `asset` of release `version` and its checksum manifest, verify the
/// manifest's provenance (when `gh` can) and the asset's checksum. The manifest
/// is kept in `staging_dir` for `gh` to read.
pub async fn download_verified(
    host: &dyn Host,
    source: &ReleaseSource,
    version: &str,
    asset: &str,
    gh: Option<&Path>,
    staging_dir: &Path,
) -> Result<Downloaded, UpgradeError> {
    let manifest_name = if asset.starts_with("dot-agent-deck-desktop-") {
        DESKTOP_MANIFEST
    } else {
        CLI_MANIFEST
    };
    create_private_dir(staging_dir)?;
    let manifest = fetch(&source.asset_url(version, manifest_name)).await?;
    let manifest_path = staging_dir.join(manifest_name);
    std::fs::write(&manifest_path, &manifest)?;
    let provenance = verify::verify_provenance(host, gh, &manifest_path)?;
    let manifest = String::from_utf8_lossy(&manifest);
    let bytes = fetch(&source.asset_url(version, asset)).await?;
    verify::verify_checksum(&manifest, manifest_name, asset, &bytes)?;
    Ok(Downloaded { bytes, provenance })
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
    /// `brew upgrade` ran; `reported` is what the Homebrew binary now says.
    BrewUpgraded {
        formula: &'static str,
        reported: Option<String>,
    },
    /// The verified file is at `path`; the user runs `command` to install it.
    Staged {
        path: PathBuf,
        command: String,
        provenance: Provenance,
    },
    /// The package or binary was installed behind a privilege prompt.
    Installed {
        version: String,
        provenance: Provenance,
    },
    /// The app at `app` was replaced; it runs `version` once restarted.
    AppReplaced {
        app: PathBuf,
        version: String,
        provenance: Provenance,
    },
}

impl Outcome {
    /// What to tell the user, in the same words in every client.
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::Replaced {
                path,
                version,
                provenance,
            } => vec![
                format!("Upgraded {} to v{version}.", path.display()),
                provenance.message(),
            ],
            Self::BrewUpgraded { formula, reported } => vec![match reported {
                Some(version) => {
                    format!("`brew upgrade {formula}` finished; it now reports v{version}.")
                }
                None => format!("`brew upgrade {formula}` finished."),
            }],
            Self::Staged {
                command,
                provenance,
                ..
            } => vec![
                "Downloaded and checked. Install it with:".to_string(),
                format!("  {command}"),
                provenance.message(),
            ],
            Self::Installed {
                version,
                provenance,
            } => vec![format!("Installed v{version}."), provenance.message()],
            Self::AppReplaced {
                app,
                version,
                provenance,
            } => vec![
                format!(
                    "Replaced {} with v{version}. Quit and reopen Agent Deck to run it.",
                    app.display()
                ),
                provenance.message(),
            ],
        }
    }
}

/// Carry out `plan`. The caller has shown it and the user confirmed.
pub async fn execute(
    host: &dyn Host,
    plan: &UpgradePlan,
    source: &ReleaseSource,
    options: &PlanOptions,
) -> Result<Outcome, UpgradeError> {
    let version = plan.latest.as_str();
    let staging_dir = options.staging_root.join(format!("v{version}"));
    let gh = plan.installation.tools.gh.as_deref();
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
            )?;
            let reported = brew
                .parent()
                .map(|bin| bin.join(CLI_BINARY))
                .and_then(|deck| super::reported_version(host, &deck));
            Ok(Outcome::BrewUpgraded {
                formula: formula.name(),
                reported,
            })
        }
        PlanAction::ReplaceBinary { target, asset } => {
            let downloaded =
                download_verified(host, source, version, asset, gh, &staging_dir).await?;
            atomic_replace(target, &downloaded.bytes, |candidate| {
                verify::verify_binary_version(host, candidate, version)
            })?;
            let _ = std::fs::remove_dir_all(&staging_dir);
            Ok(Outcome::Replaced {
                path: target.clone(),
                version: version.to_string(),
                provenance: downloaded.provenance,
            })
        }
        PlanAction::StagedInstall {
            target,
            asset,
            staged,
            command,
            pkexec,
        } => {
            let downloaded =
                download_verified(host, source, version, asset, gh, &staging_dir).await?;
            write_staged(staged, &downloaded.bytes, 0o755)?;
            verify::verify_binary_version(host, staged, version)?;
            match pkexec {
                Some(pkexec) => {
                    run_checked(
                        host,
                        pkexec,
                        &[
                            OsStr::new("/usr/bin/install"),
                            OsStr::new("-m"),
                            OsStr::new("0755"),
                            staged.as_os_str(),
                            target.as_os_str(),
                        ],
                    )?;
                    verify::verify_binary_version(host, target, version)?;
                    let _ = std::fs::remove_dir_all(&staging_dir);
                    Ok(Outcome::Installed {
                        version: version.to_string(),
                        provenance: downloaded.provenance,
                    })
                }
                None => Ok(Outcome::Staged {
                    path: staged.clone(),
                    command: command.clone(),
                    provenance: downloaded.provenance,
                }),
            }
        }
        PlanAction::InstallDeb {
            asset,
            staged,
            command,
            pkexec,
        } => {
            let downloaded =
                download_verified(host, source, version, asset, gh, &staging_dir).await?;
            write_staged(staged, &downloaded.bytes, 0o644)?;
            match pkexec {
                Some(pkexec) => {
                    install_deb(host, pkexec, staged)?;
                    let _ = std::fs::remove_dir_all(&staging_dir);
                    Ok(Outcome::Installed {
                        version: version.to_string(),
                        provenance: downloaded.provenance,
                    })
                }
                None => Ok(Outcome::Staged {
                    path: staged.clone(),
                    command: command.clone(),
                    provenance: downloaded.provenance,
                }),
            }
        }
        PlanAction::SwapApp {
            app,
            asset,
            team_id,
        } => {
            let downloaded =
                download_verified(host, source, version, asset, gh, &staging_dir).await?;
            let dmg = staging_dir.join(asset);
            write_staged(&dmg, &downloaded.bytes, 0o644)?;
            swap_app(host, &dmg, app, team_id, version, &staging_dir)?;
            let _ = std::fs::remove_dir_all(&staging_dir);
            Ok(Outcome::AppReplaced {
                app: app.clone(),
                version: version.to_string(),
                provenance: downloaded.provenance,
            })
        }
    }
}

fn create_private_dir(dir: &Path) -> Result<(), UpgradeError> {
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

/// Write `bytes` to `path` (replacing what is there) with `mode`.
fn write_staged(path: &Path, bytes: &[u8], mode: u32) -> Result<(), UpgradeError> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let _ = std::fs::remove_file(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    std::io::Write::write_all(&mut file, bytes)?;
    set_mode(&file, mode)?;
    file.sync_all()?;
    Ok(())
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
/// and only then rename it over `target` and fsync the directory. On any
/// failure the temporary file is removed and `target` is left as it was.
pub fn atomic_replace(
    target: &Path,
    bytes: &[u8],
    check: impl FnOnce(&Path) -> Result<(), UpgradeError>,
) -> Result<(), UpgradeError> {
    let dir = target
        .parent()
        .ok_or_else(|| UpgradeError::Io(format!("{} has no directory", target.display())))?;
    let name = target
        .file_name()
        .map_or_else(|| CLI_BINARY.into(), |n| n.to_string_lossy().into_owned());
    let temp = dir.join(format!(".{name}.upgrade-{}", std::process::id()));
    let result = (|| {
        write_staged(&temp, bytes, 0o755)?;
        check(&temp)?;
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

/// Install a verified `.deb` behind `pkexec`.
pub fn install_deb(host: &dyn Host, pkexec: &Path, deb: &Path) -> Result<(), UpgradeError> {
    run_checked(
        host,
        pkexec,
        &[
            OsStr::new("/usr/bin/apt-get"),
            OsStr::new("install"),
            OsStr::new("-y"),
            deb.as_os_str(),
        ],
    )
    .map(|_| ())
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
        )
        .map_err(failed)?;
    }
    Ok(())
}

/// Replace the app bundle at `app` with the one inside the verified `dmg`.
///
/// The image is attached read-only under `work_dir`; the app in it must pass
/// [`check_app`] and its bundled CLI must report `version`. It is copied next to
/// `app` with `ditto`, checked again, and swapped in by two renames, the second
/// of which is rolled back if it fails. The image is detached either way.
pub fn swap_app(
    host: &dyn Host,
    dmg: &Path,
    app: &Path,
    team_id: &str,
    version: &str,
    work_dir: &Path,
) -> Result<(), UpgradeError> {
    let mount = work_dir.join("mount");
    create_private_dir(&mount)?;
    run_checked(
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
    )?;
    let result = swap_from_mount(host, &mount, app, team_id, version);
    let detach = |extra: &[&OsStr]| {
        let mut args = vec![OsStr::new("detach"), mount.as_os_str()];
        args.extend_from_slice(extra);
        host.run(Path::new(HDIUTIL), &args)
            .is_ok_and(|out| out.success)
    };
    if !detach(&[]) {
        detach(&[OsStr::new("-force")]);
    }
    result
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
    let copied = run_checked(
        host,
        Path::new(DITTO),
        &[new_app.as_os_str(), incoming.as_os_str()],
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
    use crate::self_upgrade::test_host::{FakeHost, fail, ok};

    #[test]
    fn execute_001_atomic_replace_swaps_content_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dot-agent-deck");
        std::fs::write(&target, b"old").unwrap();
        let mut checked = None;
        atomic_replace(&target, b"new", |candidate| {
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
        let err = atomic_replace(&target, b"new", |_| {
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
        install_deb(&host, Path::new("/usr/bin/pkexec"), Path::new("/s/x.deb")).unwrap();
        assert_eq!(
            host.ran(),
            ["/usr/bin/pkexec /usr/bin/apt-get install -y /s/x.deb"]
        );

        let host = FakeHost::new().exe("/usr/bin/pkexec").answer(
            "/usr/bin/pkexec /usr/bin/apt-get install -y /s/x.deb",
            fail("Request dismissed"),
        );
        let err =
            install_deb(&host, Path::new("/usr/bin/pkexec"), Path::new("/s/x.deb")).unwrap_err();
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
            download_base: "http://127.0.0.1:1/download/".into(),
        };
        assert_eq!(
            source.asset_url("0.46.0", "checksums.txt"),
            "http://127.0.0.1:1/download/v0.46.0/checksums.txt"
        );
        let production = ReleaseSource::from_build();
        if !cfg!(feature = "e2e") {
            assert_eq!(production.api_url, crate::repo_identity::RELEASES_API_URL);
        }
    }

    #[test]
    fn execute_008_outcome_lines() {
        let outcome = Outcome::Staged {
            path: PathBuf::from("/s/x"),
            command: "sudo install -m 0755 /s/x /usr/local/bin/dot-agent-deck".into(),
            provenance: Provenance::Verified,
        };
        assert_eq!(
            outcome.lines()[1],
            "  sudo install -m 0755 /s/x /usr/local/bin/dot-agent-deck"
        );
    }
}
