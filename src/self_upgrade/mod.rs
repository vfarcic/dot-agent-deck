//! Upgrading the copies of dot-agent-deck installed on this machine (issue
//! #1635): the CLI/TUI binary and the desktop app, each through the mechanism it
//! was installed with.
//!
//! One module for both clients, so the TUI and the desktop app say the same
//! thing in the same words (CLAUDE.md rule 22). The desktop crate reaches it
//! through its path dependency on this package. The pieces, in the order a
//! client uses them:
//!
//! 1. [`detect`] — how a copy was installed. [`detect::detect`] is a pure
//!    function of [`detect::DetectInputs`]; [`detect::inspect`] gathers the real
//!    inputs through a [`Host`].
//! 2. [`plan`] — what to offer for that install and a newer release, and the
//!    user-facing text for it. Every wording a client shows lives there.
//! 3. [`discover`] — the OTHER copy on the machine (the desktop app from the
//!    CLI, the CLI from the desktop app), which gets its own detection and its
//!    own plan.
//! 4. [`verify`] — the release checksum manifest, build provenance through
//!    `gh attestation verify`, and the new binary's `--version`.
//! 5. [`execute`] — downloading the release assets and carrying a confirmed
//!    plan out.
//!
//! Nothing here upgrades anything on its own: a plan is something a client
//! shows and the user confirms ([`cli`] is the `dot-agent-deck upgrade`
//! subcommand, which asks on a terminal and takes `--yes` otherwise).
//!
//! Every subprocess and every filesystem question goes through [`Host`], so a
//! test fakes the machine instead of needing one; [`SystemHost`] is the real
//! one.

pub mod cli;
pub mod detect;
pub mod discover;
pub mod execute;
pub mod plan;
pub mod verify;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub use detect::{CopyKind, HomebrewFormula, InstallMethod, Installation, Platform, SourceReason};
pub use discover::OtherCopy;
pub use execute::{Outcome, ReleaseSource};
pub use plan::{PlanAction, PlanOptions, UpgradePlan};
pub use verify::Provenance;

/// The CLI binary's file name, inside every install and every release asset
/// name.
pub const CLI_BINARY: &str = "dot-agent-deck";

/// The command a client shows where it cannot carry an upgrade out itself.
pub const UPGRADE_COMMAND: &str = "dot-agent-deck upgrade";

/// The release's checksum manifest for the CLI assets (`task checksums` in
/// `Taskfile.yml`, uploaded by `release.yml`).
pub const CLI_MANIFEST: &str = "checksums.txt";

/// The release's checksum manifest for the desktop assets (`release.yml`'s
/// "Generate desktop checksums" step).
pub const DESKTOP_MANIFEST: &str = "checksums-desktop-alpha.txt";

/// The Debian package the desktop `.deb` installs (Tauri derives it from
/// `productName` in `desktop/src-tauri/tauri.conf.json`; `docs/installation.md`
/// names it). It ships `/usr/bin/dot-agent-deck-desktop` and the CLI at
/// [`DEB_BUNDLED_CLI`].
pub const DESKTOP_DEB_PACKAGE: &str = "agent-deck";

/// The CLI the desktop `.deb` installs beside the app. It upgrades with the
/// package and is not a separate copy.
pub const DEB_BUNDLED_CLI: &str = "/usr/bin/dot-agent-deck";

/// The macOS app bundle's directory name (`productName` plus `.app`).
pub const DESKTOP_APP_BUNDLE: &str = "Agent Deck.app";

/// What a subprocess produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// Whether it exited with status 0.
    pub success: bool,
    /// Its exit code, when it exited rather than being killed.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// The machine, as far as upgrading needs to ask about it. [`SystemHost`] is
/// the real one; tests implement it to fake an install.
pub trait Host: Send + Sync {
    /// Run `program` with `args` and wait for it.
    fn run(&self, program: &Path, args: &[&OsStr]) -> std::io::Result<CommandOutput>;
    /// The first executable called `name` on `PATH`.
    fn find_program(&self, name: &str) -> Option<PathBuf>;
    /// Whether `path` is an executable regular file.
    fn is_executable(&self, path: &Path) -> bool;
    /// Whether `path` exists.
    fn exists(&self, path: &Path) -> bool;
    /// `path` with every symlink resolved, when it exists.
    fn canonicalize(&self, path: &Path) -> Option<PathBuf>;
    /// Whether the current user can create files in `dir`.
    fn dir_writable(&self, dir: &Path) -> bool;
    /// The user's home directory.
    fn home(&self) -> Option<PathBuf>;
    /// Whether this is Linux running under WSL.
    fn is_wsl(&self) -> bool;
}

/// [`Host`] for the machine this process runs on.
#[derive(Debug, Clone, Default)]
pub struct SystemHost {
    /// The `PATH` to search, when it is not this process's own (the desktop
    /// app passes its login shell's).
    pub path: Option<std::ffi::OsString>,
}

impl Host for SystemHost {
    fn run(&self, program: &Path, args: &[&OsStr]) -> std::io::Result<CommandOutput> {
        let mut command = std::process::Command::new(program);
        command.args(args).stdin(std::process::Stdio::null());
        if let Some(path) = &self.path {
            command.env("PATH", path);
        }
        let output = command.output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn find_program(&self, name: &str) -> Option<PathBuf> {
        let path = self.path.clone().or_else(|| std::env::var_os("PATH"))?;
        std::env::split_paths(&path)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(name))
            .find(|candidate| self.is_executable(candidate))
    }

    fn is_executable(&self, path: &Path) -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok()
    }

    fn dir_writable(&self, dir: &Path) -> bool {
        // Creating a file is the only answer that accounts for ACLs, a
        // read-only mount and root-owned directories alike.
        let probe = dir.join(format!(
            ".dot-agent-deck-write-probe-{}",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                true
            }
            Err(_) => false,
        }
    }

    fn home(&self) -> Option<PathBuf> {
        Some(crate::platform::paths::home_dir())
    }

    fn is_wsl(&self) -> bool {
        if !cfg!(target_os = "linux") {
            return false;
        }
        std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
    }
}

/// Why an upgrade did not happen. Every message is written for the user.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UpgradeError {
    #[error("Cannot find the running dot-agent-deck executable.")]
    NoExecutable,
    #[error("Cannot check for a newer release: {0}")]
    ReleaseLookup(String),
    #[error("Cannot download {url}: {detail}")]
    Download { url: String, detail: String },
    #[error("{manifest} has no entry for {asset}, so it cannot be checked. Nothing was changed.")]
    ChecksumMissing { asset: String, manifest: String },
    #[error(
        "{manifest} lists {asset} more than once with different checksums, so it cannot be checked. Nothing was changed."
    )]
    ChecksumAmbiguous { asset: String, manifest: String },
    #[error(
        "{asset} does not match {manifest} (expected {expected}, got {actual}). Nothing was changed."
    )]
    ChecksumMismatch {
        asset: String,
        manifest: String,
        expected: String,
        actual: String,
    },
    #[error(
        "The build provenance of {manifest} could not be verified, so nothing was changed. `gh attestation verify` said: {detail}"
    )]
    ProvenanceFailed { manifest: String, detail: String },
    #[error(
        "The downloaded binary reports {actual} instead of dot-agent-deck {expected}. Nothing was changed."
    )]
    VersionMismatch { expected: String, actual: String },
    #[error("The new Agent Deck app failed a check, so nothing was changed: {0}")]
    AppCheckFailed(String),
    #[error("`{command}` failed: {detail}")]
    CommandFailed { command: String, detail: String },
    #[error("{0}")]
    Io(String),
    #[error("This install is not upgraded from here: {0}")]
    NotActionable(String),
}

impl From<std::io::Error> for UpgradeError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// Run `program args` through `host`, mapping a failure to
/// [`UpgradeError::CommandFailed`] named after the command line.
pub(crate) fn run_checked(
    host: &dyn Host,
    program: &Path,
    args: &[&OsStr],
) -> Result<CommandOutput, UpgradeError> {
    let command = display_command(program, args);
    let output = host
        .run(program, args)
        .map_err(|e| UpgradeError::CommandFailed {
            command: command.clone(),
            detail: e.to_string(),
        })?;
    if output.success {
        Ok(output)
    } else {
        let detail = match output.stderr.trim() {
            "" => match output.code {
                Some(code) => format!("exit {code}"),
                None => "killed by a signal".to_string(),
            },
            stderr => stderr.to_string(),
        };
        Err(UpgradeError::CommandFailed { command, detail })
    }
}

/// A command line as a user would type it into a POSIX shell.
pub(crate) fn display_command(program: &Path, args: &[&OsStr]) -> String {
    std::iter::once(program.as_os_str())
        .chain(args.iter().copied())
        .map(|word| shell_word(&word.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `word` quoted for a POSIX shell when it needs to be.
pub(crate) fn shell_word(word: &str) -> String {
    crate::platform::paths::shell_quote_if_needed(word)
}

/// The version `binary --version` reports, without a leading `v`, or `None`
/// when it does not answer as dot-agent-deck.
pub fn reported_version(host: &dyn Host, binary: &Path) -> Option<String> {
    let output = host.run(binary, &[OsStr::new("--version")]).ok()?;
    if !output.success {
        return None;
    }
    let version = crate::version::parse_version_output(&output.stdout)?;
    Some(version.strip_prefix('v').unwrap_or(&version).to_string())
}

/// Whether `latest` is a newer release than `current` (both with or without a
/// leading `v`). An unparseable version is never newer.
pub fn is_newer(current: &str, latest: &str) -> bool {
    let Ok(current) = semver::Version::parse(current.strip_prefix('v').unwrap_or(current)) else {
        return false;
    };
    crate::version::should_notify(&current, latest).is_some()
}

#[cfg(test)]
pub(crate) mod test_host {
    //! A fake [`Host`]: a set of files, a table of command answers, and a log
    //! of what ran.

    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    type Handler = Box<dyn Fn(&[String]) -> CommandOutput + Send + Sync>;

    #[derive(Default)]
    pub struct FakeHost {
        pub executables: HashSet<PathBuf>,
        pub files: HashSet<PathBuf>,
        pub links: HashMap<PathBuf, PathBuf>,
        pub writable: HashSet<PathBuf>,
        pub path: Vec<PathBuf>,
        pub home: Option<PathBuf>,
        pub wsl: bool,
        pub answers: HashMap<String, CommandOutput>,
        pub handlers: HashMap<String, Handler>,
        pub log: Mutex<Vec<String>>,
    }

    pub fn ok(stdout: &str) -> CommandOutput {
        CommandOutput {
            success: true,
            code: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    pub fn fail(stderr: &str) -> CommandOutput {
        CommandOutput {
            success: false,
            code: Some(1),
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    impl FakeHost {
        pub fn new() -> Self {
            Self::default()
        }

        /// An executable at `path`.
        pub fn exe(mut self, path: &str) -> Self {
            self.executables.insert(PathBuf::from(path));
            self.files.insert(PathBuf::from(path));
            self
        }

        /// A deck binary at `path` that answers `--version` with `version`.
        pub fn deck(self, path: &str, version: &str) -> Self {
            self.exe(path).answer(
                &format!("{path} --version"),
                ok(&format!("dot-agent-deck {version}\n")),
            )
        }

        pub fn link(mut self, from: &str, to: &str) -> Self {
            self.links.insert(PathBuf::from(from), PathBuf::from(to));
            self
        }

        pub fn writable(mut self, dir: &str) -> Self {
            self.writable.insert(PathBuf::from(dir));
            self
        }

        pub fn on_path(mut self, dir: &str) -> Self {
            self.path.push(PathBuf::from(dir));
            self
        }

        pub fn home(mut self, dir: &str) -> Self {
            self.home = Some(PathBuf::from(dir));
            self
        }

        pub fn wsl(mut self) -> Self {
            self.wsl = true;
            self
        }

        /// What `command_line` (program and arguments joined by spaces)
        /// answers.
        pub fn answer(mut self, command_line: &str, output: CommandOutput) -> Self {
            self.answers.insert(command_line.to_string(), output);
            self
        }

        /// Run `handler` for every invocation of `program`.
        pub fn handle(
            mut self,
            program: &str,
            handler: impl Fn(&[String]) -> CommandOutput + Send + Sync + 'static,
        ) -> Self {
            self.handlers.insert(program.to_string(), Box::new(handler));
            self
        }

        pub fn ran(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl Host for FakeHost {
        fn run(&self, program: &Path, args: &[&OsStr]) -> std::io::Result<CommandOutput> {
            let words: Vec<String> = std::iter::once(program.as_os_str())
                .chain(args.iter().copied())
                .map(|w| w.to_string_lossy().into_owned())
                .collect();
            let line = words.join(" ");
            self.log.lock().unwrap().push(line.clone());
            if let Some(handler) = self.handlers.get(&words[0]) {
                return Ok(handler(&words[1..]));
            }
            match self.answers.get(&line) {
                Some(output) => Ok(output.clone()),
                None if self.executables.contains(program) => Ok(fail("no answer configured")),
                None => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            }
        }

        fn find_program(&self, name: &str) -> Option<PathBuf> {
            self.path
                .iter()
                .map(|dir| dir.join(name))
                .find(|candidate| self.is_executable(candidate))
        }

        fn is_executable(&self, path: &Path) -> bool {
            let path = self.links.get(path).map_or(path, PathBuf::as_path);
            self.executables.contains(path)
        }

        fn exists(&self, path: &Path) -> bool {
            self.files.contains(path)
                || self.links.contains_key(path)
                || self.files.iter().any(|file| file.starts_with(path))
        }

        fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
            if let Some(target) = self.links.get(path) {
                return Some(target.clone());
            }
            self.exists(path).then(|| path.to_path_buf())
        }

        fn dir_writable(&self, dir: &Path) -> bool {
            self.writable.contains(dir)
        }

        fn home(&self) -> Option<PathBuf> {
            self.home.clone()
        }

        fn is_wsl(&self) -> bool {
            self.wsl
        }
    }
}
