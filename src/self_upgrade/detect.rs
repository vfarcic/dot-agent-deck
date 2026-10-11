//! How a copy of dot-agent-deck was installed.
//!
//! [`detect`] is pure: it reads only [`DetectInputs`], so every row of issue
//! #1635's platform and install-method matrix is a unit test with faked
//! inputs. [`inspect`] gathers the real inputs for one executable through a
//! [`Host`], and [`running`] does that for this process.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::{DESKTOP_DEB_PACKAGE, Host};

/// A platform a release ships assets for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    LinuxAmd64,
    LinuxArm64,
    MacosAmd64,
    MacosArm64,
}

impl Platform {
    /// The platform for Rust's `std::env::consts::{OS, ARCH}` spelling.
    pub fn from_os_arch(os: &str, arch: &str) -> Option<Self> {
        match (os, arch) {
            ("linux", "x86_64") => Some(Self::LinuxAmd64),
            ("linux", "aarch64") => Some(Self::LinuxArm64),
            ("macos", "x86_64") => Some(Self::MacosAmd64),
            ("macos", "aarch64") => Some(Self::MacosArm64),
            _ => None,
        }
    }

    /// The platform this process runs on, when releases ship for it.
    pub fn current() -> Option<Self> {
        Self::from_os_arch(std::env::consts::OS, std::env::consts::ARCH)
    }

    pub fn is_macos(self) -> bool {
        matches!(self, Self::MacosAmd64 | Self::MacosArm64)
    }

    pub fn is_linux(self) -> bool {
        !self.is_macos()
    }

    /// The release asset holding the CLI binary (`release.yml`'s
    /// `artifact_suffix` matrix).
    pub fn cli_asset(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "dot-agent-deck-linux-amd64",
            Self::LinuxArm64 => "dot-agent-deck-linux-arm64",
            Self::MacosAmd64 => "dot-agent-deck-darwin-amd64",
            Self::MacosArm64 => "dot-agent-deck-darwin-arm64",
        }
    }

    /// The release asset holding the desktop app, on the platforms that have
    /// one (`release.yml`'s desktop `asset_suffix` matrix).
    pub fn desktop_asset(self) -> Option<&'static str> {
        match self {
            Self::MacosArm64 => Some("dot-agent-deck-desktop-alpha-macos-arm64.dmg"),
            Self::LinuxAmd64 => Some("dot-agent-deck-desktop-alpha-linux-amd64.deb"),
            Self::LinuxArm64 | Self::MacosAmd64 => None,
        }
    }
}

/// Which of the two separately installed copies this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyKind {
    /// The `dot-agent-deck` binary: the TUI, the CLI and the daemon.
    Cli,
    /// The Agent Deck desktop app, and the CLI bundled inside it.
    Desktop,
}

/// The two Homebrew formulas the tap publishes (`docs/installation.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomebrewFormula {
    Stable,
    Beta,
}

impl HomebrewFormula {
    pub fn name(self) -> &'static str {
        match self {
            Self::Stable => "dot-agent-deck",
            Self::Beta => "dot-agent-deck-beta",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        [Self::Stable, Self::Beta]
            .into_iter()
            .find(|formula| formula.name() == name)
    }
}

/// Why a copy counts as built from source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceReason {
    /// It runs from a cargo `target/` directory.
    BuildTree,
    /// It was installed with `cargo install` (it sits in `~/.cargo/bin`).
    CargoInstall,
    /// It was built from a checkout with uncommitted changes.
    DirtyTree,
}

/// How a copy was installed — one row of issue #1635's matrix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    /// A Homebrew formula's keg under `<prefix>/Cellar/<formula>/`.
    Homebrew {
        formula: HomebrewFormula,
        prefix: PathBuf,
    },
    /// A downloaded release binary in a directory the user can write.
    DownloadedWritable { binary: PathBuf },
    /// A downloaded release binary in a directory the user cannot write.
    DownloadedNonWritable { binary: PathBuf },
    /// A Nix store path. Never touched.
    Nix,
    /// Built from source. Never replaced.
    Source { reason: SourceReason },
    /// A file another system package owns.
    SystemPackage { package: String },
    /// The desktop app from the `.dmg`, at `app`.
    DesktopDmg {
        app: PathBuf,
        /// Whether the user can write the directory that holds `app`.
        parent_writable: bool,
        /// The running app's code-signing Team ID, `None` when it is unsigned.
        team_id: Option<String>,
    },
    /// The desktop app from the `.deb`, owned by [`DESKTOP_DEB_PACKAGE`].
    DesktopDeb,
}

impl InstallMethod {
    /// The copy this install is of, when the install itself says so: the
    /// desktop packages hold a bundled CLI that upgrades with the app.
    fn implied_copy(&self) -> Option<CopyKind> {
        matches!(self, Self::DesktopDmg { .. } | Self::DesktopDeb).then_some(CopyKind::Desktop)
    }
}

/// Everything [`detect`] reads. [`inspect`] fills it from the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectInputs {
    /// The executable, with every symlink resolved.
    pub executable: PathBuf,
    /// Its build id (`DAD_BUILD_ID`), when known; only a `-dirty` suffix is
    /// read.
    pub build_id: Option<String>,
    pub platform: Option<Platform>,
    /// Whether the executable sits in a cargo build tree.
    pub in_build_tree: bool,
    /// Whether the user can write the directory an upgrade replaces into: the
    /// directory holding the `.app` for an app bundle, the executable's own
    /// directory otherwise.
    pub target_dir_writable: bool,
    /// `brew --prefix`, for a Homebrew outside the default prefixes.
    pub brew_prefix: Option<PathBuf>,
    /// The Debian package that owns the executable (`dpkg-query -S`).
    pub dpkg_owner: Option<String>,
    /// The enclosing app bundle's Team ID (`codesign -dv`), when signed.
    pub app_team_id: Option<String>,
}

/// Classify an install. Pure.
///
/// The order matters where two rules could both match: a Nix store path, a
/// Homebrew keg and a package-owned file are each definitive about who owns
/// the file, so they come before the build-tree check, which comes before the
/// app-bundle check (a `tauri build` leaves an `.app` inside `target/`).
pub fn detect(inputs: &DetectInputs) -> InstallMethod {
    let exe = &inputs.executable;
    if exe.starts_with("/nix/store") {
        return InstallMethod::Nix;
    }
    if let Some((prefix, formula)) = homebrew_keg(exe)
        && is_homebrew_prefix(&prefix, inputs.brew_prefix.as_deref())
    {
        return InstallMethod::Homebrew { formula, prefix };
    }
    match inputs.dpkg_owner.as_deref() {
        Some(DESKTOP_DEB_PACKAGE) => return InstallMethod::DesktopDeb,
        Some(package) => {
            return InstallMethod::SystemPackage {
                package: package.to_string(),
            };
        }
        None => {}
    }
    if let Some(reason) = source_reason(inputs) {
        return InstallMethod::Source { reason };
    }
    if inputs.platform.is_some_and(Platform::is_macos)
        && let Some(app) = app_bundle_of(exe)
    {
        return InstallMethod::DesktopDmg {
            app,
            parent_writable: inputs.target_dir_writable,
            team_id: inputs.app_team_id.clone(),
        };
    }
    if inputs.target_dir_writable {
        InstallMethod::DownloadedWritable {
            binary: exe.clone(),
        }
    } else {
        InstallMethod::DownloadedNonWritable {
            binary: exe.clone(),
        }
    }
}

/// `<prefix>` and the formula when `exe` is inside `<prefix>/Cellar/<formula>/`
/// for one of the tap's formulas.
fn homebrew_keg(exe: &Path) -> Option<(PathBuf, HomebrewFormula)> {
    exe.ancestors().skip(1).find_map(|dir| {
        if dir.file_name() != Some(OsStr::new("Cellar")) {
            return None;
        }
        let formula = exe.strip_prefix(dir).ok()?.components().next()?;
        let formula = HomebrewFormula::from_name(formula.as_os_str().to_str()?)?;
        Some((dir.parent()?.to_path_buf(), formula))
    })
}

/// Whether `prefix` is a Homebrew prefix: one of the defaults `remote upgrade`
/// probes ([`crate::remote::HOMEBREW_PREFIXES`]), or the one `brew --prefix`
/// reports.
fn is_homebrew_prefix(prefix: &Path, brew_prefix: Option<&Path>) -> bool {
    crate::remote::HOMEBREW_PREFIXES
        .iter()
        .any(|known| prefix == Path::new(known))
        || brew_prefix == Some(prefix)
}

fn source_reason(inputs: &DetectInputs) -> Option<SourceReason> {
    if inputs.in_build_tree {
        return Some(SourceReason::BuildTree);
    }
    let in_cargo_bin = inputs
        .executable
        .parent()
        .is_some_and(|dir| dir.ends_with(".cargo/bin"));
    if in_cargo_bin {
        return Some(SourceReason::CargoInstall);
    }
    if inputs
        .build_id
        .as_deref()
        .is_some_and(|id| id.ends_with("-dirty"))
    {
        return Some(SourceReason::DirtyTree);
    }
    None
}

/// The `.app` bundle `exe` runs from, when it is `<X>.app/Contents/MacOS/<file>`.
pub fn app_bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let is_bundle = macos.file_name() == Some(OsStr::new("MacOS"))
        && contents.file_name() == Some(OsStr::new("Contents"))
        && app.extension() == Some(OsStr::new("app"));
    is_bundle.then(|| app.to_path_buf())
}

/// The tools an upgrade may use, where they are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tools {
    pub brew: Option<PathBuf>,
    pub gh: Option<PathBuf>,
    pub pkexec: Option<PathBuf>,
    pub dpkg_query: Option<PathBuf>,
}

impl Tools {
    /// Look the tools up on `host`. `brew` is looked for on `PATH` and then at
    /// each default prefix, as `remote upgrade` does, because a GUI app's
    /// `PATH` often lacks it.
    pub fn find(host: &dyn Host) -> Self {
        let brew = host.find_program("brew").or_else(|| {
            crate::remote::HOMEBREW_PREFIXES
                .iter()
                .map(|prefix| Path::new(prefix).join("bin/brew"))
                .find(|brew| host.is_executable(brew))
        });
        Self {
            brew,
            gh: host.find_program("gh"),
            pkexec: host.find_program("pkexec"),
            dpkg_query: host.find_program("dpkg-query"),
        }
    }
}

/// One installed copy: where it is, what it reports, and how it was installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    pub copy: CopyKind,
    /// The executable, with every symlink resolved.
    pub executable: PathBuf,
    /// The version it reports, without a leading `v`.
    pub version: String,
    pub platform: Option<Platform>,
    pub method: InstallMethod,
    pub tools: Tools,
}

/// Inspect the copy at `executable` (already canonical) reporting `version`.
/// `copy` is what the caller knows it to be; a desktop package's bundled CLI is
/// the desktop copy whatever the caller said.
pub fn inspect(
    host: &dyn Host,
    copy: CopyKind,
    executable: &Path,
    version: &str,
    build_id: Option<&str>,
    platform: Option<Platform>,
) -> Installation {
    let mut tools = Tools::find(host);
    let inputs = gather_inputs(host, &tools, executable, build_id, platform);
    let method = detect(&inputs);
    if let InstallMethod::Homebrew { prefix, .. } = &method {
        tools.brew = formula_brew(host, tools.brew.take(), prefix);
    }
    Installation {
        copy: method.implied_copy().unwrap_or(copy),
        executable: executable.to_path_buf(),
        version: version.strip_prefix('v').unwrap_or(version).to_string(),
        platform,
        method,
        tools,
    }
}

/// The `brew` that upgrades a formula installed under `prefix`: `found`
/// when it is that prefix's own, else `<prefix>/bin/brew` when it exists and
/// is executable (a machine with two Homebrew prefixes finds the other one on
/// `PATH`). Otherwise `found` is kept as it was, and the plan, which runs only
/// a `brew` inside the prefix, shows the command instead.
fn formula_brew(host: &dyn Host, found: Option<PathBuf>, prefix: &Path) -> Option<PathBuf> {
    if found.as_ref().is_some_and(|brew| brew.starts_with(prefix)) {
        return found;
    }
    let own = prefix.join("bin/brew");
    if host.is_executable(&own) {
        Some(own)
    } else {
        found
    }
}

fn gather_inputs(
    host: &dyn Host,
    tools: &Tools,
    executable: &Path,
    build_id: Option<&str>,
    platform: Option<Platform>,
) -> DetectInputs {
    let app = app_bundle_of(executable).filter(|_| platform.is_some_and(Platform::is_macos));
    let target_dir = app
        .as_deref()
        .and_then(Path::parent)
        .or_else(|| executable.parent());
    let brew_prefix = match (homebrew_keg(executable), &tools.brew) {
        (Some((prefix, _)), Some(brew)) if !is_homebrew_prefix(&prefix, None) => host
            .run(brew, &[OsStr::new("--prefix")])
            .ok()
            .filter(|out| out.success)
            .map(|out| PathBuf::from(out.stdout.trim())),
        _ => None,
    };
    let dpkg_owner = match &tools.dpkg_query {
        Some(dpkg_query) if platform.is_some_and(Platform::is_linux) => {
            dpkg_owner(host, dpkg_query, executable)
        }
        _ => None,
    };
    let app_team_id = app.as_deref().and_then(|app| team_id(host, app));
    DetectInputs {
        executable: executable.to_path_buf(),
        build_id: build_id.map(str::to_string),
        platform,
        in_build_tree: crate::platform::paths::is_build_artifact_path(executable),
        target_dir_writable: target_dir.is_some_and(|dir| host.dir_writable(dir)),
        brew_prefix,
        dpkg_owner,
        app_team_id,
    }
}

/// The package `dpkg-query -S` says owns `path`. Its answer is
/// `<package>[:<arch>][, <package>…]: <path>`; the first package is taken.
fn dpkg_owner(host: &dyn Host, dpkg_query: &Path, path: &Path) -> Option<String> {
    let output = host
        .run(dpkg_query, &[OsStr::new("-S"), path.as_os_str()])
        .ok()?;
    if !output.success {
        return None;
    }
    parse_dpkg_owner(&output.stdout, path)
}

pub(crate) fn parse_dpkg_owner(stdout: &str, path: &Path) -> Option<String> {
    let path = path.to_str()?;
    stdout.lines().find_map(|line| {
        let packages = line.strip_suffix(path)?.strip_suffix(": ")?;
        let first = packages.split(", ").next()?;
        let name = first.split(':').next()?.trim();
        (!name.is_empty()).then(|| name.to_string())
    })
}

/// The app's Team ID from `codesign -dv`, which writes `TeamIdentifier=<id>`
/// to stderr; `None` when it is unsigned or ad-hoc signed.
pub(crate) fn team_id(host: &dyn Host, app: &Path) -> Option<String> {
    team_id_from(host.run(Path::new(CODESIGN), &team_id_args(app)))
}

/// [`team_id`], except that a cancelled `codesign -dv` is the error the
/// upgrade stops with ([`super::unless_cancelled`]).
pub(crate) fn team_id_unless_cancelled(
    host: &dyn Host,
    app: &Path,
) -> Result<Option<String>, super::UpgradeError> {
    let args = team_id_args(app);
    let program = Path::new(CODESIGN);
    super::unless_cancelled(host.run(program, &args), program, &args).map(team_id_from)
}

const CODESIGN: &str = "/usr/bin/codesign";

fn team_id_args(app: &Path) -> [&OsStr; 3] {
    [
        OsStr::new("-dv"),
        OsStr::new("--verbose=2"),
        app.as_os_str(),
    ]
}

/// The Team ID a `codesign -dv` run reports ([`team_id`]).
fn team_id_from(output: std::io::Result<super::CommandOutput>) -> Option<String> {
    let output = output.ok()?;
    if !output.success {
        return None;
    }
    parse_team_id(&output.stderr)
}

pub(crate) fn parse_team_id(codesign_stderr: &str) -> Option<String> {
    codesign_stderr.lines().find_map(|line| {
        let id = line.strip_prefix("TeamIdentifier=")?.trim();
        (!id.is_empty() && id != "not set").then(|| id.to_string())
    })
}

/// This process's own copy.
pub fn running(host: &dyn Host, copy: CopyKind) -> Result<Installation, super::UpgradeError> {
    let executable = running_executable(host).ok_or(super::UpgradeError::NoExecutable)?;
    Ok(inspect(
        host,
        copy,
        &executable,
        &running_version(),
        running_build_id(),
        Platform::current(),
    ))
}

/// This process's executable, with every symlink resolved. Under the `e2e`
/// feature only, `DOT_AGENT_DECK_TEST_RUNNING_EXE` names it instead, taken as
/// given, so an L2 test can make the real binary look installed somewhere it
/// is not — a folder the test can write, or a `/nix/store` path no test can
/// create. The in-process override `effective_current_exe` reads cannot reach
/// a spawned binary, which is why this is an environment variable; it is
/// gated on the feature for the reason that function's doc gives.
fn running_executable(host: &dyn Host) -> Option<PathBuf> {
    #[cfg(feature = "e2e")]
    if let Ok(exe) = std::env::var("DOT_AGENT_DECK_TEST_RUNNING_EXE")
        && !exe.is_empty()
    {
        return Some(PathBuf::from(exe));
    }
    crate::platform::paths::executable_path()
        .map(PathBuf::from)
        .and_then(|path| host.canonicalize(&path))
}

/// This build's id, which only [`detect`]'s `-dirty` check reads. Under the
/// `e2e` feature, a test that replaced the running version
/// (`DOT_AGENT_DECK_TEST_RUNNING_VERSION`) gets none: the id describes the
/// real build, not the version the test made it report, and a test binary
/// built from a checkout with uncommitted changes would otherwise be detected
/// as a source build on one machine and not on another.
fn running_build_id() -> Option<&'static str> {
    #[cfg(feature = "e2e")]
    if std::env::var("DOT_AGENT_DECK_TEST_RUNNING_VERSION").is_ok_and(|v| !v.is_empty()) {
        return None;
    }
    Some(env!("DAD_BUILD_ID"))
}

/// The version this build reports. Under the `e2e` feature only,
/// `DOT_AGENT_DECK_TEST_RUNNING_VERSION` replaces it, so an L2 test can make the
/// real binary look older than the release its fake server offers. Gated on
/// the feature rather than read in every build, for the reason
/// `effective_current_exe` in `src/platform/paths.rs` gives.
fn running_version() -> String {
    #[cfg(feature = "e2e")]
    if let Ok(version) = std::env::var("DOT_AGENT_DECK_TEST_RUNNING_VERSION")
        && !version.is_empty()
    {
        return version;
    }
    crate::version::current_version().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_upgrade::test_host::{FakeHost, fail};
    // Used only by Unix-gated tests: native Windows is unsupported (#164).
    #[cfg(unix)]
    use crate::self_upgrade::test_host::ok;

    fn inputs(exe: &str) -> DetectInputs {
        DetectInputs {
            executable: PathBuf::from(exe),
            build_id: Some("0.46.0-gabc1234".into()),
            platform: Some(Platform::LinuxAmd64),
            in_build_tree: false,
            target_dir_writable: true,
            brew_prefix: None,
            dpkg_owner: None,
            app_team_id: None,
        }
    }

    // ── Matrix rows: CLI/TUI ──

    #[test]
    fn detect_001_homebrew_stable_formula() {
        for prefix in ["/opt/homebrew", "/usr/local", "/home/linuxbrew/.linuxbrew"] {
            let exe = format!("{prefix}/Cellar/dot-agent-deck/0.46.0/bin/dot-agent-deck");
            assert_eq!(
                detect(&inputs(&exe)),
                InstallMethod::Homebrew {
                    formula: HomebrewFormula::Stable,
                    prefix: PathBuf::from(prefix),
                }
            );
        }
    }

    #[test]
    fn detect_002_homebrew_beta_formula() {
        let mut input =
            inputs("/opt/homebrew/Cellar/dot-agent-deck-beta/0.47.0-rc.1/bin/dot-agent-deck");
        input.platform = Some(Platform::MacosArm64);
        assert_eq!(
            detect(&input),
            InstallMethod::Homebrew {
                formula: HomebrewFormula::Beta,
                prefix: PathBuf::from("/opt/homebrew"),
            }
        );
    }

    #[test]
    fn detect_003_homebrew_custom_prefix_only_when_brew_reports_it() {
        let exe = "/srv/brew/Cellar/dot-agent-deck/0.46.0/bin/dot-agent-deck";
        let mut input = inputs(exe);
        assert_eq!(
            detect(&input),
            InstallMethod::DownloadedWritable {
                binary: PathBuf::from(exe)
            },
            "a `Cellar` directory outside any Homebrew prefix is not Homebrew"
        );
        input.brew_prefix = Some(PathBuf::from("/srv/brew"));
        assert!(matches!(
            detect(&input),
            InstallMethod::Homebrew {
                formula: HomebrewFormula::Stable,
                ..
            }
        ));
    }

    #[test]
    fn detect_004_downloaded_binary_writable_dir() {
        let exe = "/home/u/.local/bin/dot-agent-deck";
        assert_eq!(
            detect(&inputs(exe)),
            InstallMethod::DownloadedWritable {
                binary: PathBuf::from(exe)
            }
        );
    }

    #[test]
    fn detect_005_downloaded_binary_non_writable_dir() {
        let exe = "/usr/local/bin/dot-agent-deck";
        let mut input = inputs(exe);
        input.target_dir_writable = false;
        assert_eq!(
            detect(&input),
            InstallMethod::DownloadedNonWritable {
                binary: PathBuf::from(exe)
            }
        );
    }

    #[test]
    fn detect_006_nix_store_path() {
        let mut input = inputs("/nix/store/abc123-dot-agent-deck-0.46.0/bin/dot-agent-deck");
        input.build_id = Some("0.46.0-unknown".into());
        assert_eq!(detect(&input), InstallMethod::Nix);
    }

    #[test]
    fn detect_007_source_build_tree() {
        let mut input = inputs("/home/u/code/deck/target/release/dot-agent-deck");
        input.in_build_tree = true;
        assert_eq!(
            detect(&input),
            InstallMethod::Source {
                reason: SourceReason::BuildTree
            }
        );
    }

    #[test]
    fn detect_008_source_cargo_install() {
        assert_eq!(
            detect(&inputs("/home/u/.cargo/bin/dot-agent-deck")),
            InstallMethod::Source {
                reason: SourceReason::CargoInstall
            }
        );
    }

    #[test]
    fn detect_009_source_dirty_build() {
        let mut input = inputs("/home/u/.local/bin/dot-agent-deck");
        input.build_id = Some("0.46.0-gabc1234-dirty".into());
        assert_eq!(
            detect(&input),
            InstallMethod::Source {
                reason: SourceReason::DirtyTree
            }
        );
    }

    #[test]
    fn detect_010_macos_intel_and_wsl_cli_rows_match_linux() {
        // macOS Intel and WSL run the same detection as Linux for the CLI:
        // the method depends on the path, not on the platform.
        for platform in [Platform::MacosAmd64, Platform::LinuxAmd64] {
            let mut input = inputs("/usr/local/bin/dot-agent-deck");
            input.platform = Some(platform);
            input.target_dir_writable = false;
            assert!(matches!(
                detect(&input),
                InstallMethod::DownloadedNonWritable { .. }
            ));
        }
    }

    #[test]
    fn detect_011_other_system_package() {
        let mut input = inputs("/usr/bin/dot-agent-deck");
        input.dpkg_owner = Some("dot-agent-deck-distro".into());
        assert_eq!(
            detect(&input),
            InstallMethod::SystemPackage {
                package: "dot-agent-deck-distro".into()
            }
        );
    }

    // ── Matrix rows: desktop ──

    #[test]
    fn detect_012_desktop_deb() {
        let mut input = inputs("/usr/bin/dot-agent-deck-desktop");
        input.dpkg_owner = Some("agent-deck".into());
        input.target_dir_writable = false;
        assert_eq!(detect(&input), InstallMethod::DesktopDeb);
    }

    #[test]
    fn detect_013_desktop_dmg_in_writable_applications() {
        let mut input =
            inputs("/Applications/Agent Deck.app/Contents/MacOS/dot-agent-deck-desktop");
        input.platform = Some(Platform::MacosArm64);
        input.app_team_id = Some("TEAM123".into());
        assert_eq!(
            detect(&input),
            InstallMethod::DesktopDmg {
                app: PathBuf::from("/Applications/Agent Deck.app"),
                parent_writable: true,
                team_id: Some("TEAM123".into()),
            }
        );
    }

    #[test]
    fn detect_014_desktop_dmg_in_non_writable_dir() {
        let mut input = inputs("/Volumes/Shared/Agent Deck.app/Contents/MacOS/dot-agent-deck");
        input.platform = Some(Platform::MacosArm64);
        input.target_dir_writable = false;
        assert!(matches!(
            detect(&input),
            InstallMethod::DesktopDmg {
                parent_writable: false,
                ..
            }
        ));
    }

    #[test]
    fn detect_015_tauri_build_output_is_source_not_dmg() {
        let mut input = inputs(
            "/home/u/deck/target/release/bundle/macos/Agent Deck.app/Contents/MacOS/dot-agent-deck-desktop",
        );
        input.platform = Some(Platform::MacosArm64);
        input.in_build_tree = true;
        assert_eq!(
            detect(&input),
            InstallMethod::Source {
                reason: SourceReason::BuildTree
            }
        );
    }

    // ── The impure wrapper, against a fake host ──

    // Unix install layout: native Windows is unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn inspect_001_bundled_cli_in_deb_is_the_desktop_copy() {
        let host = FakeHost::new()
            .deck("/usr/bin/dot-agent-deck", "0.46.0")
            .exe("/usr/bin/dpkg-query")
            .on_path("/usr/bin")
            .answer(
                "/usr/bin/dpkg-query -S /usr/bin/dot-agent-deck",
                ok("agent-deck: /usr/bin/dot-agent-deck\n"),
            );
        let found = inspect(
            &host,
            CopyKind::Cli,
            Path::new("/usr/bin/dot-agent-deck"),
            "0.46.0",
            None,
            Some(Platform::LinuxAmd64),
        );
        assert_eq!(found.method, InstallMethod::DesktopDeb);
        assert_eq!(found.copy, CopyKind::Desktop);
    }

    // Unix install layout: native Windows is unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn inspect_004_the_formulas_own_brew_is_used_only_when_it_exists() {
        // Two Homebrew prefixes: the `brew` on PATH is /opt/homebrew's, the
        // keg is in /home/linuxbrew/.linuxbrew.
        let keg = "/home/linuxbrew/.linuxbrew/Cellar/dot-agent-deck/0.46.0/bin/dot-agent-deck";
        let machine = || {
            FakeHost::new()
                .on_path("/opt/homebrew/bin")
                .exe("/opt/homebrew/bin/brew")
                .deck(keg, "0.46.0")
        };
        let inspect_on = |host: &FakeHost| {
            inspect(
                host,
                CopyKind::Cli,
                Path::new(keg),
                "0.46.0",
                None,
                Some(Platform::LinuxAmd64),
            )
        };

        let without = inspect_on(&machine());
        assert!(matches!(without.method, InstallMethod::Homebrew { .. }));
        assert_eq!(
            without.tools.brew,
            Some(PathBuf::from("/opt/homebrew/bin/brew")),
            "the prefix's own brew does not exist, so none is substituted"
        );

        let with = inspect_on(&machine().exe("/home/linuxbrew/.linuxbrew/bin/brew"));
        assert_eq!(
            with.tools.brew,
            Some(PathBuf::from("/home/linuxbrew/.linuxbrew/bin/brew"))
        );
    }

    #[test]
    fn inspect_002_reads_team_id_of_running_app() {
        let app_exe = "/Applications/Agent Deck.app/Contents/MacOS/dot-agent-deck";
        let host = FakeHost::new()
            .deck(app_exe, "0.46.0")
            .writable("/Applications")
            .answer(
                "/usr/bin/codesign -dv --verbose=2 /Applications/Agent Deck.app",
                crate::self_upgrade::CommandOutput {
                    success: true,
                    code: Some(0),
                    stdout: String::new(),
                    stderr:
                        "Identifier=ai.devopstoolkit.agentdeck.desktop\nTeamIdentifier=TEAM123\n"
                            .into(),
                },
            );
        let found = inspect(
            &host,
            CopyKind::Cli,
            Path::new(app_exe),
            "v0.46.0",
            None,
            Some(Platform::MacosArm64),
        );
        assert_eq!(found.version, "0.46.0");
        assert_eq!(
            found.method,
            InstallMethod::DesktopDmg {
                app: PathBuf::from("/Applications/Agent Deck.app"),
                parent_writable: true,
                team_id: Some("TEAM123".into()),
            }
        );
    }

    #[test]
    fn inspect_003_unsigned_app_has_no_team_id() {
        let host = FakeHost::new().answer(
            "/usr/bin/codesign -dv --verbose=2 /Applications/Agent Deck.app",
            fail("code object is not signed at all"),
        );
        assert_eq!(
            team_id(&host, Path::new("/Applications/Agent Deck.app")),
            None
        );
        assert_eq!(parse_team_id("TeamIdentifier=not set\n"), None);
    }

    #[test]
    fn parse_dpkg_owner_takes_first_package_and_drops_arch() {
        let path = Path::new("/usr/bin/dot-agent-deck");
        assert_eq!(
            parse_dpkg_owner("agent-deck:amd64, other: /usr/bin/dot-agent-deck\n", path),
            Some("agent-deck".into())
        );
        assert_eq!(
            parse_dpkg_owner("diversion by x from: /usr/bin/y\n", path),
            None
        );
    }

    #[test]
    fn platform_assets_match_release_names() {
        assert_eq!(
            Platform::LinuxArm64.cli_asset(),
            "dot-agent-deck-linux-arm64"
        );
        assert_eq!(
            Platform::MacosArm64.cli_asset(),
            "dot-agent-deck-darwin-arm64"
        );
        assert_eq!(Platform::LinuxArm64.desktop_asset(), None);
        assert_eq!(Platform::MacosAmd64.desktop_asset(), None);
        assert_eq!(
            Platform::from_os_arch("macos", "aarch64"),
            Some(Platform::MacosArm64)
        );
        assert_eq!(Platform::from_os_arch("windows", "x86_64"), None);
    }
}
