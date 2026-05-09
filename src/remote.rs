//! PRD #76 M2.2 — `dot-agent-deck remote add --type=ssh ...`.
//!
//! Registers an ssh-reachable host as a deck environment: verifies
//! reachability, installs the matching `dot-agent-deck` binary on the remote
//! (downloaded from GitHub releases), runs `dot-agent-deck hooks install` on
//! the remote, and writes a registry entry to
//! `~/.config/dot-agent-deck/remotes.toml`.
//!
//! All side-effecting ssh work goes through the `SshExecutor` trait so tests
//! can drive the flow with a `FakeSshExecutor` that records commands and
//! returns canned outputs.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// GitHub releases base URL used to download `dot-agent-deck` binaries onto
/// remote hosts. Hard-coded because Cargo doesn't export
/// `[package.repository]` to the build (and our `Cargo.toml` doesn't set it).
/// Mirrors the URL in `version.rs`.
pub const RELEASE_BASE: &str = "https://github.com/vfarcic/dot-agent-deck/releases/download";

/// Default ssh port.
pub const DEFAULT_SSH_PORT: u16 = 22;

// ---------------------------------------------------------------------------
// SshExecutor abstraction — the seam that lets us test the add flow without
// shelling out to a real `ssh` binary.
// ---------------------------------------------------------------------------

/// Where to ssh to, parsed from the CLI args.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    pub user: Option<String>,
    pub port: u16,
    pub key: Option<PathBuf>,
}

impl SshTarget {
    /// Parse `[user@]host` and combine with `--port` / `--key` flags.
    pub fn parse(target: &str, port: u16, key: Option<PathBuf>) -> Self {
        let (user, host) = match target.split_once('@') {
            Some((u, h)) => (Some(u.to_string()), h.to_string()),
            None => (None, target.to_string()),
        };
        Self {
            host,
            user,
            port,
            key,
        }
    }

    /// `user@host` if a user was given, else just `host`. The form ssh wants
    /// as its destination argument.
    pub fn user_host(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

/// Captured output of one ssh invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Failure modes the executor distinguishes. Mapped from ssh's stderr/exit
/// status by `SystemSshExecutor`; the production matcher is conservative —
/// anything we can't classify ends up as `Other` so the caller can surface
/// stderr verbatim.
#[derive(Debug, Error)]
pub enum SshError {
    #[error(
        "Could not reach {host}:{port}. Check the host is up and ssh is exposed on this port.\nDetails: {detail}"
    )]
    ConnectionRefused {
        host: String,
        port: u16,
        detail: String,
    },
    #[error(
        "ssh authentication to {target} failed. Check your key (`--key`) or `~/.ssh/config`.\nDetails: {detail}"
    )]
    AuthFailed { target: String, detail: String },
    #[error("ssh I/O error contacting {target}: {source}")]
    Io {
        target: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "ssh failed: host key not yet trusted for {target}. If this is a first-time connection, run `ssh {target}` once to accept the host key, then retry `remote add`. If the key has changed unexpectedly, investigate before connecting."
    )]
    HostKeyVerificationFailed { target: String },
    #[error("ssh to {target} failed: {detail}")]
    Other { target: String, detail: String },
}

/// Map ssh's stderr output (when the process exited with 255) onto a typed
/// `SshError` variant. Extracted from `SystemSshExecutor::run` so it can be
/// unit-tested without spawning a process.
fn classify_ssh_error(target: &SshTarget, stderr: &str) -> SshError {
    let lower = stderr.to_ascii_lowercase();
    let detail = stderr.trim().to_string();
    if lower.contains("connection refused")
        || lower.contains("network is unreachable")
        || lower.contains("no route to host")
        || lower.contains("could not resolve hostname")
        || lower.contains("connection timed out")
    {
        return SshError::ConnectionRefused {
            host: target.host.clone(),
            port: target.port,
            detail,
        };
    }
    // Host-key issues are checked BEFORE the generic auth match because under
    // BatchMode=yes the canonical message is "Host key verification failed."
    // and the user's recourse (run `ssh <target>` once) differs from a key
    // mismatch. We treat both first-trust and key-changed scenarios as the
    // same variant — the Display message tells the user to investigate.
    if lower.contains("host key verification failed")
        || lower.contains("remote host identification has changed")
        || lower.contains("are you sure you want to continue connecting")
    {
        return SshError::HostKeyVerificationFailed {
            target: target.user_host(),
        };
    }
    if lower.contains("permission denied") || lower.contains("publickey") {
        return SshError::AuthFailed {
            target: target.user_host(),
            detail,
        };
    }
    SshError::Other {
        target: target.user_host(),
        detail,
    }
}

/// Abstraction over running shell commands on a remote ssh host. The
/// production impl shells out to the `ssh` binary; tests use a fake.
pub trait SshExecutor {
    fn run(&self, target: &SshTarget, command: &str) -> Result<SshOutput, SshError>;
}

/// Production implementation: shells out to the `ssh` binary on the user's
/// machine. No new dependency on a Rust ssh client — we deliberately reuse
/// the user's existing ssh config (`~/.ssh/config`, agent, known_hosts).
pub struct SystemSshExecutor;

impl SystemSshExecutor {
    pub fn new() -> Self {
        Self
    }

    /// Build the `ssh` command without spawning it. Exposed for tests so we
    /// can verify argument quoting without forking a subprocess.
    pub fn build_command(target: &SshTarget, remote_command: &str) -> Command {
        let mut cmd = Command::new("ssh");
        // BatchMode=yes makes ssh fail fast on missing keys/known_hosts
        // instead of hanging on a TTY prompt. Users who haven't trusted the
        // host yet will see an actionable error rather than the deck CLI
        // wedging.
        cmd.arg("-o").arg("BatchMode=yes");
        cmd.arg("-p").arg(target.port.to_string());
        if let Some(key) = &target.key {
            cmd.arg("-i").arg(key);
        }
        cmd.arg("--");
        cmd.arg(target.user_host());
        // Pass the remote command as a single argv entry. ssh joins remaining
        // args with spaces and runs them through the remote shell, so passing
        // one arg keeps quoting predictable. NOTE: `remote_command` is a
        // string we (the deck) construct entirely from internal templates and
        // the resolved version/platform — no user input is interpolated into
        // shell here. The parsed user@host arg also goes through `arg(...)`,
        // not through a shell, so there's no local shell-injection surface.
        cmd.arg(remote_command);
        cmd
    }
}

impl Default for SystemSshExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl SshExecutor for SystemSshExecutor {
    fn run(&self, target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
        let mut cmd = Self::build_command(target, command);
        let output = cmd.output().map_err(|source| SshError::Io {
            target: target.user_host(),
            source,
        })?;
        let status = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        // ssh uses exit 255 to signal its own (i.e. transport/auth) errors;
        // anything else is the remote command's exit code. Only translate to
        // typed transport errors on 255 — otherwise return the SshOutput so
        // callers can decide based on the remote command's status.
        if status == 255 {
            return Err(classify_ssh_error(target, &stderr));
        }

        Ok(SshOutput {
            status,
            stdout,
            stderr,
        })
    }
}

// ---------------------------------------------------------------------------
// Registry: ~/.config/dot-agent-deck/remotes.toml
// ---------------------------------------------------------------------------

/// One row in `remotes.toml`. `host` carries the full `[user@]host` string
/// the user passed on the CLI — we keep it intact so `connect` (M2.4) can
/// re-parse it the same way the user typed it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub version: String,
    pub added_at: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemotesFile {
    #[serde(default)]
    pub remotes: Vec<RemoteEntry>,
}

#[derive(Debug, Error)]
pub enum RemoteConfigError {
    #[error("Failed to read remotes file at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Failed to parse remotes file at {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("Failed to serialize remotes file: {0}")]
    Serialize(#[from] toml::ser::Error),
}

impl RemotesFile {
    /// Load the registry from `path`. Missing file → empty registry.
    pub fn load(path: &Path) -> Result<Self, RemoteConfigError> {
        match std::fs::read_to_string(path) {
            Ok(contents) => toml::from_str(&contents).map_err(|source| RemoteConfigError::Parse {
                path: path.display().to_string(),
                source,
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(RemoteConfigError::Io {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// Atomically replace the file at `path` with the serialized form of
    /// `self`. Creates the parent directory if missing. Writes via a sibling
    /// temp file with mode 0o600, then `rename(2)`s it into place — so a
    /// partial write or a crash mid-save can never leave a half-written
    /// `remotes.toml` for the next run to choke on, and the final file is
    /// owner-only (0o600) regardless of the user's umask.
    pub fn save(&self, path: &Path) -> Result<(), RemoteConfigError> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let parent = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(|source| RemoteConfigError::Io {
            path: parent.display().to_string(),
            source,
        })?;

        let contents = toml::to_string_pretty(self)?;

        // Sibling temp file: same directory as the final path so `rename` is
        // atomic on POSIX filesystems. Pid suffix avoids collisions when
        // multiple deck processes save concurrently to different registries.
        let file_name = path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "remotes.toml".to_string());
        let tmp_path = parent.join(format!("{file_name}.{}.tmp", std::process::id()));

        let open_result = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path);
        let mut tmp_file = open_result.map_err(|source| RemoteConfigError::Io {
            path: tmp_path.display().to_string(),
            source,
        })?;

        let write_result = tmp_file.write_all(contents.as_bytes());
        if let Err(source) = write_result {
            // Best-effort cleanup; ignore secondary errors.
            let _ = std::fs::remove_file(&tmp_path);
            return Err(RemoteConfigError::Io {
                path: tmp_path.display().to_string(),
                source,
            });
        }
        // Defense in depth: if a stale temp file from a crashed previous save
        // existed, OpenOptions::mode() would NOT have re-applied the bits, so
        // re-set them explicitly before the rename.
        let perm_result = tmp_file.set_permissions(std::fs::Permissions::from_mode(0o600));
        if let Err(source) = perm_result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(RemoteConfigError::Io {
                path: tmp_path.display().to_string(),
                source,
            });
        }
        drop(tmp_file);

        std::fs::rename(&tmp_path, path).map_err(|source| {
            let _ = std::fs::remove_file(&tmp_path);
            RemoteConfigError::Io {
                path: path.display().to_string(),
                source,
            }
        })
    }
}

/// Default location for the registry: `$DOT_AGENT_DECK_REMOTES` if set,
/// else `~/.config/dot-agent-deck/remotes.toml`. The env var override is
/// new; tests use it (or pass an explicit path to `add`).
pub fn default_remotes_path() -> PathBuf {
    if let Ok(p) = std::env::var("DOT_AGENT_DECK_REMOTES") {
        return PathBuf::from(p);
    }
    crate::config::dirs_home().join(".config/dot-agent-deck/remotes.toml")
}

// ---------------------------------------------------------------------------
// `remote add` flow
// ---------------------------------------------------------------------------

/// CLI options accepted by `remote add`. `release_base` is overridable so the
/// happy-path test can inject a stub URL without changing global state.
#[derive(Debug, Clone)]
pub struct AddOptions {
    pub name: String,
    pub remote_type: String,
    pub target: String,
    pub port: u16,
    pub key: Option<PathBuf>,
    pub version: String,
    pub no_install: bool,
    pub release_base: String,
}

#[derive(Debug, Error)]
pub enum RemoteAddError {
    #[error(
        "A remote named '{name}' already exists. Use `dot-agent-deck remote remove {name}` first or pick a different name."
    )]
    DuplicateName { name: String },
    #[error("Remote type 'kubernetes' is not yet implemented; coming in Phase 3.")]
    KubernetesNotYetImplemented,
    #[error("Unsupported remote type '{kind}'. Supported: ssh.")]
    UnsupportedType { kind: String },
    #[error("Invalid --version '{input}': must look like '0.24.5' or 'v0.24.5'.")]
    InvalidVersion { input: String },
    #[error(transparent)]
    Ssh(#[from] SshError),
    #[error("Remote arch is `{arch}`; supported: linux-{{amd64,arm64}}, darwin-{{amd64,arm64}}.")]
    UnsupportedArch { arch: String },
    #[error("Failed to detect remote arch (`uname -s -m` exited {status}): {stderr}")]
    UnameFailed { status: i32, stderr: String },
    #[error(
        "Failed to download dot-agent-deck v{version} for {platform} from {url}.\nCheck the remote has internet egress and the version exists.\nDetails: {detail}"
    )]
    DownloadFailed {
        version: String,
        platform: String,
        url: String,
        detail: String,
    },
    #[error("Installed binary reports `{actual}` but expected `{expected}`.")]
    VersionMismatch { actual: String, expected: String },
    #[error("`dot-agent-deck hooks install` on remote failed (exit {status}): {stderr}")]
    HooksInstallFailed { status: i32, stderr: String },
    #[error(transparent)]
    Registry(#[from] RemoteConfigError),
}

/// SemVer-ish pattern accepted by `--version`: optional `v` prefix, three
/// numeric components, optional pre-release suffix. Rejects anything that
/// could carry shell metacharacters into the remote install command.
const VERSION_PATTERN: &str = r"^v?\d+(\.\d+){2}(-[A-Za-z0-9.\-]+)?$";

fn version_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(VERSION_PATTERN).expect("static version regex compiles"))
}

/// Validate a user-supplied version string before it is interpolated into a
/// URL or a remote shell command. Anything that doesn't look like a SemVer
/// version is rejected — this is the primary defense against shell injection
/// via `--version` (e.g. `0.24.5; rm -rf ~`).
pub fn validate_version_string(version: &str) -> Result<(), RemoteAddError> {
    if version_regex().is_match(version) {
        Ok(())
    } else {
        Err(RemoteAddError::InvalidVersion {
            input: version.to_string(),
        })
    }
}

/// Build the install URL and the remote shell command. Validates `version`
/// internally as a defense-in-depth so that even an internal misuse (a code
/// path that bypassed the entry-point check in `add()`) cannot slip a shell
/// metacharacter into the remote command.
fn build_install_command(
    release_base: &str,
    version: &str,
    platform: &str,
) -> Result<(String, String), RemoteAddError> {
    validate_version_string(version)?;
    let url = format!("{release_base}/v{version}/dot-agent-deck-{platform}");
    let install_cmd = format!(
        "mkdir -p ~/.local/bin && curl -fsSL {url} -o ~/.local/bin/dot-agent-deck && chmod 0755 ~/.local/bin/dot-agent-deck"
    );
    Ok((url, install_cmd))
}

/// Convert a `uname -s -m` line to one of our four supported platform tags.
fn detect_platform(uname_stdout: &str) -> Option<&'static str> {
    let trimmed = uname_stdout.trim();
    let mut parts = trimmed.split_whitespace();
    let os = parts.next()?;
    let arch = parts.next()?;
    match (os, arch) {
        ("Linux", "x86_64") => Some("linux-amd64"),
        ("Linux", "aarch64") | ("Linux", "arm64") => Some("linux-arm64"),
        ("Darwin", "x86_64") => Some("darwin-amd64"),
        ("Darwin", "arm64") => Some("darwin-arm64"),
        _ => None,
    }
}

/// Pull the version number out of `dot-agent-deck --version` output.
/// Expected shape: `dot-agent-deck X.Y.Z` (possibly with trailing whitespace).
fn parse_version_output(stdout: &str) -> Option<String> {
    stdout.split_whitespace().nth(1).map(|s| s.to_string())
}

/// Run the `add` flow. Returns the registry entry that was written, so
/// callers (and tests) can assert on it.
pub fn add(
    opts: &AddOptions,
    executor: &dyn SshExecutor,
    remotes_path: &Path,
) -> Result<RemoteEntry, RemoteAddError> {
    // 1. Type validation.
    match opts.remote_type.as_str() {
        "ssh" => {}
        "kubernetes" => return Err(RemoteAddError::KubernetesNotYetImplemented),
        other => {
            return Err(RemoteAddError::UnsupportedType {
                kind: other.to_string(),
            });
        }
    }

    // 2. Version validation — runs BEFORE any ssh call or URL construction so
    //    a malicious `--version` (e.g. `0.24.5; rm -rf ~`) can't reach the
    //    remote shell. Asserted by the `version_string_with_shell_metacharacters_rejected`
    //    test, which checks zero ssh calls were attempted.
    validate_version_string(&opts.version)?;

    // 3. Uniqueness check — done *before* any ssh call so a duplicate name
    //    short-circuits without bothering the remote (and lets the
    //    `duplicate_name_rejected` test assert the fake recorded zero
    //    commands).
    let mut registry = RemotesFile::load(remotes_path)?;
    if registry.remotes.iter().any(|r| r.name == opts.name) {
        return Err(RemoteAddError::DuplicateName {
            name: opts.name.clone(),
        });
    }

    let target = SshTarget::parse(&opts.target, opts.port, opts.key.clone());

    // 3. Pre-flight reachability + arch detection.
    let uname = executor.run(&target, "uname -s -m")?;
    if uname.status != 0 {
        return Err(RemoteAddError::UnameFailed {
            status: uname.status,
            stderr: uname.stderr,
        });
    }
    let platform =
        detect_platform(&uname.stdout).ok_or_else(|| RemoteAddError::UnsupportedArch {
            arch: uname.stdout.trim().to_string(),
        })?;

    // 4. Install or version-check.
    if opts.no_install {
        let v = executor.run(&target, "dot-agent-deck --version")?;
        if v.status != 0 {
            return Err(RemoteAddError::VersionMismatch {
                actual: format!("(exit {}) {}", v.status, v.stderr.trim()),
                expected: opts.version.clone(),
            });
        }
        let actual = parse_version_output(&v.stdout).unwrap_or_else(|| v.stdout.trim().to_string());
        if actual != opts.version {
            return Err(RemoteAddError::VersionMismatch {
                actual,
                expected: opts.version.clone(),
            });
        }
    } else {
        // Single shell command: any step's failure aborts the rest, and
        // we get one stderr+exit pair instead of three round-trips' worth
        // to disentangle. `build_install_command` re-validates the version
        // (defense in depth) so even an internal misuse can't shell-inject.
        let (url, install_cmd) =
            build_install_command(&opts.release_base, &opts.version, platform)?;
        let install = executor.run(&target, &install_cmd)?;
        if install.status != 0 {
            return Err(RemoteAddError::DownloadFailed {
                version: opts.version.clone(),
                platform: platform.to_string(),
                url,
                detail: format!("exit {}: {}", install.status, install.stderr.trim()),
            });
        }
        let v = executor.run(&target, "~/.local/bin/dot-agent-deck --version")?;
        if v.status != 0 {
            return Err(RemoteAddError::VersionMismatch {
                actual: format!("(exit {}) {}", v.status, v.stderr.trim()),
                expected: opts.version.clone(),
            });
        }
        let actual = parse_version_output(&v.stdout).unwrap_or_else(|| v.stdout.trim().to_string());
        if actual != opts.version {
            return Err(RemoteAddError::VersionMismatch {
                actual,
                expected: opts.version.clone(),
            });
        }
    }

    // 5. Hook install on the remote.
    let hooks_bin = if opts.no_install {
        "dot-agent-deck"
    } else {
        "~/.local/bin/dot-agent-deck"
    };
    let hooks = executor.run(&target, &format!("{hooks_bin} hooks install"))?;
    if hooks.status != 0 {
        return Err(RemoteAddError::HooksInstallFailed {
            status: hooks.status,
            stderr: hooks.stderr,
        });
    }
    if !hooks.stdout.is_empty() {
        print!("{}", hooks.stdout);
        if !hooks.stdout.ends_with('\n') {
            println!();
        }
    }

    // 6. Append to registry.
    let entry = RemoteEntry {
        name: opts.name.clone(),
        kind: "ssh".to_string(),
        host: opts.target.clone(),
        port: opts.port,
        key: opts
            .key
            .as_ref()
            .and_then(|p| p.as_os_str().to_str())
            .map(|s| s.to_string())
            .or_else(|| opts.key.as_ref().map(|p| p.to_string_lossy().into_owned())),
        version: opts.version.clone(),
        added_at: chrono::Utc::now().to_rfc3339(),
    };
    registry.remotes.push(entry.clone());
    registry.save(remotes_path)?;

    // 7. Final success line.
    println!(
        "Added remote '{}' (ssh: {}, version {}). Run `dot-agent-deck connect {}` to attach.",
        opts.name, opts.target, opts.version, opts.name,
    );

    Ok(entry)
}

// ---------------------------------------------------------------------------
// Tests for the production SshExecutor's argument construction. Crucially
// these do NOT spawn ssh — they inspect the `Command`'s args, which is
// enough to catch quoting regressions and shell-injection mistakes.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(OsStr::to_string_lossy)
            .map(|s| s.into_owned())
            .collect()
    }

    #[test]
    fn ssh_target_parse_with_user() {
        let t = SshTarget::parse("viktor@hetzner-1.example.com", 2222, None);
        assert_eq!(t.user.as_deref(), Some("viktor"));
        assert_eq!(t.host, "hetzner-1.example.com");
        assert_eq!(t.port, 2222);
        assert_eq!(t.user_host(), "viktor@hetzner-1.example.com");
    }

    #[test]
    fn ssh_target_parse_without_user() {
        let t = SshTarget::parse("hetzner-1.example.com", 22, None);
        assert!(t.user.is_none());
        assert_eq!(t.user_host(), "hetzner-1.example.com");
    }

    #[test]
    fn system_ssh_executor_quotes_arguments_safely() {
        // Hostile-looking host string + a remote command that would be
        // catastrophic if naively interpolated into a shell. Our impl uses
        // `Command::arg` (no shell), so each lands as its own argv entry —
        // the local ssh process never sees a meta-character it can interpret.
        let target = SshTarget {
            host: "host`whoami`".to_string(),
            user: Some("user;rm -rf /".to_string()),
            port: 2222,
            key: Some(PathBuf::from("/tmp/key id_rsa")),
        };
        let cmd = SystemSshExecutor::build_command(&target, "uname -s -m; echo $(id)");
        let args = args_of(&cmd);

        // Order matters: -o BatchMode -p PORT [-i KEY] -- user@host CMD.
        assert_eq!(args[0], "-o");
        assert_eq!(args[1], "BatchMode=yes");
        assert_eq!(args[2], "-p");
        assert_eq!(args[3], "2222");
        assert_eq!(args[4], "-i");
        assert_eq!(args[5], "/tmp/key id_rsa"); // single arg, space preserved
        assert_eq!(args[6], "--");
        assert_eq!(args[7], "user;rm -rf /@host`whoami`");
        // Remote command is one arg — ssh ships it to the remote shell as a
        // single string, but locally it's a single argv entry that the *local*
        // shell never parses.
        assert_eq!(args[8], "uname -s -m; echo $(id)");
        assert_eq!(args.len(), 9);
    }

    #[test]
    fn system_ssh_executor_omits_key_flag_when_none() {
        let target = SshTarget {
            host: "h".to_string(),
            user: None,
            port: 22,
            key: None,
        };
        let cmd = SystemSshExecutor::build_command(&target, "echo hi");
        let args = args_of(&cmd);
        assert!(!args.iter().any(|a| a == "-i"));
        // -- precedes the destination
        let dash_dash_pos = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[dash_dash_pos + 1], "h");
        assert_eq!(args[dash_dash_pos + 2], "echo hi");
    }

    #[test]
    fn detect_platform_known() {
        assert_eq!(detect_platform("Linux x86_64\n"), Some("linux-amd64"));
        assert_eq!(detect_platform("Linux aarch64"), Some("linux-arm64"));
        assert_eq!(detect_platform("Linux arm64"), Some("linux-arm64"));
        assert_eq!(detect_platform("Darwin x86_64"), Some("darwin-amd64"));
        assert_eq!(detect_platform("Darwin arm64"), Some("darwin-arm64"));
    }

    #[test]
    fn detect_platform_unknown() {
        assert_eq!(detect_platform("Linux riscv64"), None);
        assert_eq!(detect_platform("FreeBSD amd64"), None);
        assert_eq!(detect_platform(""), None);
    }

    #[test]
    fn parse_version_output_typical() {
        assert_eq!(
            parse_version_output("dot-agent-deck 0.24.5\n"),
            Some("0.24.5".to_string())
        );
        assert_eq!(
            parse_version_output("dot-agent-deck 1.2.3"),
            Some("1.2.3".to_string())
        );
    }

    #[test]
    fn validate_version_string_accepts_semver_shapes() {
        for v in [
            "0.24.5",
            "v0.24.5",
            "1.2.3",
            "0.0.1",
            "10.20.30",
            "1.0.0-rc.1",
            "0.24.5-pre.2",
        ] {
            assert!(
                validate_version_string(v).is_ok(),
                "expected `{v}` to validate"
            );
        }
    }

    #[test]
    fn validate_version_string_rejects_malformed() {
        for v in [
            "",
            "not-a-version",
            "1.2",
            "1.2.3.4",
            "v1.2",
            "1.2.3 ", // trailing whitespace
            " 1.2.3", // leading whitespace
            "1.2.3;", // metacharacter
            "1.2.3$x",
        ] {
            let err = validate_version_string(v).expect_err(&format!("expected `{v}` to fail"));
            match err {
                RemoteAddError::InvalidVersion { input } => assert_eq!(input, v),
                other => panic!("unexpected error for `{v}`: {other:?}"),
            }
        }
    }

    #[test]
    fn build_install_command_rejects_invalid_version() {
        let err = build_install_command("https://example.test", "0.24.5; rm -rf ~", "linux-amd64")
            .expect_err("malicious version must be rejected by the builder too");
        assert!(matches!(err, RemoteAddError::InvalidVersion { .. }));
    }

    #[test]
    fn classify_ssh_error_host_key_verification_failed() {
        let target = SshTarget::parse("user@host", 22, None);
        let err = classify_ssh_error(&target, "Host key verification failed.\r\n");
        assert!(
            matches!(err, SshError::HostKeyVerificationFailed { .. }),
            "got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("user@host"),
            "Display should name target: {msg}"
        );
        assert!(
            msg.contains("first-time connection"),
            "Display should advise the user: {msg}"
        );
    }

    #[test]
    fn classify_ssh_error_host_key_changed_routes_to_same_variant() {
        let target = SshTarget::parse("h", 22, None);
        let stderr = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n";
        let err = classify_ssh_error(&target, stderr);
        assert!(matches!(err, SshError::HostKeyVerificationFailed { .. }));
    }

    #[test]
    fn classify_ssh_error_connection_refused_still_works() {
        let target = SshTarget::parse("h", 22, None);
        let err = classify_ssh_error(
            &target,
            "ssh: connect to host h port 22: Connection refused",
        );
        assert!(matches!(err, SshError::ConnectionRefused { .. }));
    }

    #[test]
    fn classify_ssh_error_auth_failed_still_works() {
        let target = SshTarget::parse("u@h", 22, None);
        let err = classify_ssh_error(&target, "u@h: Permission denied (publickey).");
        assert!(matches!(err, SshError::AuthFailed { .. }));
    }

    #[test]
    fn remotes_toml_written_at_0o600() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        let file = RemotesFile::default();
        file.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "remotes.toml mode is 0o{mode:o}, expected 0o600"
        );

        // Re-saving over an existing file must keep 0o600 too.
        file.save(&path).unwrap();
        let mode2 = std::fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(
            mode2, 0o600,
            "after rewrite, mode is 0o{mode2:o}, expected 0o600"
        );
    }

    #[test]
    fn remotes_toml_save_creates_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        // Two missing levels — exercises create_dir_all.
        let path = dir.path().join("a/b/remotes.toml");
        let file = RemotesFile::default();
        file.save(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn remotes_file_round_trip_two_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        let file = RemotesFile {
            remotes: vec![
                RemoteEntry {
                    name: "hetzner-1".to_string(),
                    kind: "ssh".to_string(),
                    host: "viktor@hetzner-1.example.com".to_string(),
                    port: 22,
                    key: Some("~/.ssh/id_ed25519".to_string()),
                    version: "0.24.5".to_string(),
                    added_at: "2026-05-09T01:23:45+00:00".to_string(),
                },
                RemoteEntry {
                    name: "lab".to_string(),
                    kind: "ssh".to_string(),
                    host: "lab.local".to_string(),
                    port: 2222,
                    key: None,
                    version: "0.24.5".to_string(),
                    added_at: "2026-05-09T02:00:00+00:00".to_string(),
                },
            ],
        };
        file.save(&path).unwrap();
        let loaded = RemotesFile::load(&path).unwrap();
        assert_eq!(loaded, file);
    }
}
