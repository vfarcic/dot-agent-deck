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
//!    `gh attestation verify`, the new binary's `--version`, and the re-hash
//!    right before a staged file is installed.
//! 5. [`execute`] — downloading the release assets and carrying a confirmed
//!    plan out.
//!
//! Nothing here upgrades anything on its own: a plan is something a client
//! shows and the user confirms ([`cli`] is the `dot-agent-deck upgrade`
//! subcommand, which asks on a terminal and takes `--yes` otherwise).
//!
//! Every subprocess and every filesystem question goes through [`Host`], so a
//! test fakes the machine instead of needing one; [`SystemHost`] is the real
//! one. The staged downloads themselves are real files in a private directory
//! [`execute`] creates per upgrade. `docs/develop/self-upgrade.md` describes
//! the design and the trust boundary.

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
pub use plan::PlanLine;
pub use plan::{PlanAction, PlanOptions, Releases, UpgradePlan};
pub use verify::{Provenance, ProvenanceCheck};

pub use crate::version::ReleaseChannel;

/// The CLI binary's file name, inside every install and every release asset
/// name.
pub const CLI_BINARY: &str = "dot-agent-deck";

/// How often a running client asks again whether a newer release exists, after
/// the check it makes at start. The desktop app uses it; one value so both
/// clients notice a release equally soon (CLAUDE.md rule 22). Six hours keeps a
/// client that runs for days current while staying far inside GitHub's
/// unauthenticated limit of 60 API requests an hour.
pub const UPDATE_RECHECK_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(6 * 60 * 60);

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

/// How long a probe may take: a question asked of the machine while looking
/// at an install or checking a download (`--version`, `dpkg-query`,
/// `brew --prefix`, `gh auth status`, `codesign -dv`, `hdiutil detach`). Each
/// answers in well under a second when it works; one that hangs (a wrapper
/// script waiting on something) must not hold a release check or a dialog.
pub const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long a check of a download may take when it asks a service: `gh
/// attestation verify` (GitHub's attestation API and Sigstore), and the new
/// app's `codesign --verify --deep` and `spctl --assess`, which can consult
/// Apple's notarization service.
pub const VERIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a command that carries an upgrade out may take: `brew upgrade`,
/// `pkexec … install` and `pkexec … apt-get install` (which wait for the user
/// to answer the password prompt), `hdiutil attach` and `ditto`. Long enough
/// for a slow download inside `brew` or a user who steps away from the
/// prompt, and still an end: a stuck command is stopped and named rather
/// than leaving the dialog on Upgrading for good.
pub const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// The machine, as far as upgrading needs to ask about it. [`SystemHost`] is
/// the real one; tests implement it to fake an install.
pub trait Host: Send + Sync {
    /// Run `program` with `args` as a probe, bounded by [`PROBE_TIMEOUT`].
    fn run(&self, program: &Path, args: &[&OsStr]) -> std::io::Result<CommandOutput> {
        self.run_within(program, args, PROBE_TIMEOUT)
    }
    /// Run `program` with `args` and wait for it for at most `timeout`. One
    /// still running then is stopped, and the error says so
    /// ([`std::io::ErrorKind::TimedOut`]).
    fn run_within(
        &self,
        program: &Path,
        args: &[&OsStr],
        timeout: std::time::Duration,
    ) -> std::io::Result<CommandOutput>;
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
    fn run_within(
        &self,
        program: &Path,
        args: &[&OsStr],
        timeout: std::time::Duration,
    ) -> std::io::Result<CommandOutput> {
        let mut command = std::process::Command::new(program);
        command.args(args);
        if let Some(path) = &self.path {
            command.env("PATH", path);
        }
        run_bounded(&mut command, timeout)
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

/// Run `command` (stdin closed, stdout and stderr captured) for at most
/// `timeout`. A command still running then is killed, and the error says how
/// long it was given and whether it could be stopped: a command `pkexec`
/// started runs as root, which this user cannot signal, so it may still be
/// running. Its output is read on two threads so a chatty command cannot fill
/// a pipe and stall; after a timeout they are left to finish on their own, as
/// a grandchild may hold the pipes open.
fn run_bounded(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
) -> std::io::Result<CommandOutput> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (tx, rx) = std::sync::mpsc::channel();
    for (stream, pipe) in [
        (
            0,
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
        (
            1,
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            let _ = tx.send((stream, bytes));
        });
    }
    drop(tx);
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let now = Instant::now();
        if now >= deadline {
            let stopped = child.kill().is_ok();
            if stopped {
                let _ = child.wait();
            }
            return Err(timed_out(timeout, stopped));
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left.max(Duration::from_millis(100))) {
            Ok((0, bytes)) => stdout = bytes,
            Ok((_, bytes)) => stderr = bytes,
            Err(_) => break,
        }
    }
    Ok(CommandOutput {
        success: status.success(),
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// A command that outlived `timeout`, in words for the user.
fn timed_out(timeout: std::time::Duration, stopped: bool) -> std::io::Error {
    let within = describe_duration(timeout);
    let what = if stopped {
        format!("it did not finish within {within} and was stopped")
    } else {
        format!(
            "it did not finish within {within} and could not be stopped, so it may still be running"
        )
    };
    std::io::Error::new(std::io::ErrorKind::TimedOut, what)
}

/// `duration` in the largest whole unit that fits: "15 minutes", "60
/// seconds", "200 milliseconds".
fn describe_duration(duration: std::time::Duration) -> String {
    let plural = |n: u128, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    let ms = duration.as_millis();
    if ms % 60_000 == 0 && ms > 0 {
        plural(ms / 60_000, "minute")
    } else if ms % 1000 == 0 && ms > 0 {
        plural(ms / 1000, "second")
    } else {
        plural(ms, "millisecond")
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
        "The upgrade plan said build provenance would be checked, but it cannot be now: {reason}. Nothing was changed."
    )]
    ProvenanceUnavailable { reason: String },
    #[error(
        "{path} changed after it was checked (expected {expected}, got {actual}), so it was not installed. Nothing was changed."
    )]
    StagedChanged {
        path: String,
        expected: String,
        actual: String,
    },
    #[error(
        "WARNING: {target} was installed, but it is NOT the verified build (expected {expected}, got {actual}). It was not run. Do not use it: reinstall dot-agent-deck from {release}."
    )]
    InstalledMismatch {
        target: String,
        expected: String,
        actual: String,
        release: String,
    },
    #[error(
        "The download folder {root} is not safe to use: {why}. Nothing was changed. Remove it or make it private to you, then try again."
    )]
    StagingUnsafe { root: String, why: String },
    /// The privilege prompt (`pkexec`) did not install a file that was
    /// downloaded and checked. `install` is the command that installs the
    /// staged file instead, when one can be shown safely.
    #[error("`{command}` failed: {detail}")]
    PrivilegeFailed {
        command: String,
        detail: String,
        install: Option<String>,
        version: String,
    },
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

impl UpgradeError {
    /// What the user can do instead, shown after the error itself: for a
    /// failed privilege prompt, the command that installs the file that was
    /// already downloaded and checked, or how to upgrade manually when that
    /// command cannot be shown safely.
    pub fn fallback(&self) -> Vec<PlanLine> {
        match self {
            Self::PrivilegeFailed {
                install: Some(command),
                ..
            } => vec![
                PlanLine::Text(plan::PROMPT_FAILED.to_string()),
                PlanLine::Command(command.clone()),
            ],
            Self::PrivilegeFailed {
                install: None,
                version,
                ..
            } => vec![PlanLine::Text(plan::manual_upgrade_line(version))],
            _ => Vec::new(),
        }
    }
}

impl From<std::io::Error> for UpgradeError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// Run `program args` through `host` for at most `timeout`, mapping a
/// failure, a timeout included, to [`UpgradeError::CommandFailed`] named
/// after the command line.
pub(crate) fn run_checked(
    host: &dyn Host,
    program: &Path,
    args: &[&OsStr],
    timeout: std::time::Duration,
) -> Result<CommandOutput, UpgradeError> {
    let command = display_command(program, args);
    let output =
        host.run_within(program, args, timeout)
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

/// The release channel `installation` follows: `Prerelease` for the
/// `dot-agent-deck-beta` Homebrew formula, which only ever receives
/// prereleases, or for any copy whose own version is a prerelease; `Stable`
/// otherwise. Each copy on the machine is planned on its own channel
/// ([`plan::plan`]), so a stable copy beside a prerelease one is never offered
/// a prerelease, and a copy on the beta formula is offered only a prerelease
/// ([`plan::Releases`]).
pub fn release_channel(installation: &Installation) -> ReleaseChannel {
    match installation.method {
        InstallMethod::Homebrew {
            formula: HomebrewFormula::Beta,
            ..
        } => ReleaseChannel::Prerelease,
        _ => ReleaseChannel::of_version(&installation.version),
    }
}

/// Whether `latest` is a newer release than `current` (both with or without a
/// leading `v`). An unparseable version is never newer.
pub fn is_newer(current: &str, latest: &str) -> bool {
    let Ok(current) = semver::Version::parse(current.strip_prefix('v').unwrap_or(current)) else {
        return false;
    };
    crate::version::should_notify(&current, latest).is_some()
}

// Real subprocesses through `/bin/sh`: native Windows is unsupported (#164).
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const SH: &str = "/bin/sh";

    #[test]
    fn host_001_a_command_past_its_bound_is_stopped_and_named() {
        let started = Instant::now();
        let err = SystemHost::default()
            .run_within(
                Path::new(SH),
                &[OsStr::new("-c"), OsStr::new("sleep 30")],
                Duration::from_millis(200),
            )
            .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(
            err.to_string(),
            "it did not finish within 200 milliseconds and was stopped"
        );

        // As an upgrade step, the error names the command that timed out.
        let err = run_checked(
            &SystemHost::default(),
            Path::new(SH),
            &[OsStr::new("-c"), OsStr::new("sleep 30")],
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "`/bin/sh -c 'sleep 30'` failed: it did not finish within 200 milliseconds and was stopped"
        );
    }

    #[test]
    fn host_002_a_command_inside_its_bound_answers_in_full() {
        let output = SystemHost::default()
            .run_within(
                Path::new(SH),
                &[
                    OsStr::new("-c"),
                    OsStr::new("echo out; echo err >&2; exit 3"),
                ],
                Duration::from_secs(10),
            )
            .unwrap();
        assert_eq!(
            output,
            CommandOutput {
                success: false,
                code: Some(3),
                stdout: "out\n".into(),
                stderr: "err\n".into(),
            }
        );
    }

    #[test]
    fn host_003_the_bounds_are_ordered_and_described() {
        assert!(PROBE_TIMEOUT < VERIFY_TIMEOUT && VERIFY_TIMEOUT < INSTALL_TIMEOUT);
        assert_eq!(describe_duration(INSTALL_TIMEOUT), "15 minutes");
        assert_eq!(describe_duration(VERIFY_TIMEOUT), "1 minute");
        assert_eq!(describe_duration(PROBE_TIMEOUT), "15 seconds");
        assert_eq!(
            timed_out(INSTALL_TIMEOUT, false).to_string(),
            "it did not finish within 15 minutes and could not be stopped, so it may still be running"
        );
    }
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
        /// Each command line run, with the bound it was given.
        pub bounds: Mutex<Vec<(String, std::time::Duration)>>,
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

        /// The bound the command line starting with `prefix` was run with.
        pub fn bound_of(&self, prefix: &str) -> Option<std::time::Duration> {
            self.bounds
                .lock()
                .unwrap()
                .iter()
                .find(|(line, _)| line.starts_with(prefix))
                .map(|(_, bound)| *bound)
        }
    }

    impl Host for FakeHost {
        fn run_within(
            &self,
            program: &Path,
            args: &[&OsStr],
            timeout: std::time::Duration,
        ) -> std::io::Result<CommandOutput> {
            let words: Vec<String> = std::iter::once(program.as_os_str())
                .chain(args.iter().copied())
                .map(|w| w.to_string_lossy().into_owned())
                .collect();
            let line = words.join(" ");
            self.log.lock().unwrap().push(line.clone());
            self.bounds.lock().unwrap().push((line.clone(), timeout));
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
