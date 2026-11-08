//! The agent release a lane-2 measurement runs against: the `claude` or
//! `codex` installed on the contained `PATH` the harness gives a deck pane
//! (issue #2146). PRD #1666 part 3 gave the doorbell one set of facts per
//! agent, so a measurement asserts those facts against whatever release is
//! installed; the recorded captures stay in the repository as the reference
//! (`docs/develop/lane2-installed-release-map.md`).
//!
//! Each test runs its agent's credential preflight first
//! (`skip_unless!(common::check_claude_available())` or the Codex one), then
//! [`or_skip`] over [`claude`] or [`codex`]: no executable on the `PATH`,
//! one whose `--version` names no release, or one whose release carries a
//! pre-release or build tag (which the deck's release scanner cannot latch,
//! [`installed_release`]), prints `SKIP: [e2e] …` and the test returns;
//! `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` turns that into a failure (CLAUDE.md
//! rule 5).
#![allow(dead_code)] // Each test file uses its own subset.

#[path = "installed_release.rs"]
pub mod installed_release;

use crate::common;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// An installed agent, as a measurement selects it.
#[derive(Debug, Clone)]
pub struct Installed {
    /// `claude` or `codex`.
    pub program: &'static str,
    /// The release its `--version` printed.
    pub release: String,
    /// The first `program` on [`Installed::path`], as spelled there.
    pub executable: PathBuf,
    /// The `PATH` it was found on: [`common::path_with_binary_dir`]. A
    /// measurement gives its deck this `PATH`, so the recorder in the pane
    /// resolves the same executable.
    pub path: String,
    /// The SHA-256 of the canonical executable, computed on the first
    /// [`Installed::assert_recorded`] that needs it and kept for the rest.
    sha256: std::sync::OnceLock<String>,
}

impl Installed {
    /// Asserts that a recorder's `result.json` reports this agent: the
    /// release [`Installed::release`] names, and the executable
    /// [`Installed::executable`] resolves to, both paths canonicalized, so a
    /// recorder that measured another executable of the same release fails
    /// too. Claude Code's recorders hash the executable they ran
    /// (`executable`, `sha256`), so the hash is compared as well. Codex's
    /// head-echo recorder reports the `codex` it resolved as `launcher` and
    /// hashes the native executable that launcher starts (`executable`), so
    /// its hash is compared only when the two are the same file.
    pub fn assert_recorded(&self, result: &serde_json::Value, evidence: &Path) {
        let agent = match self.program {
            "claude" => "Claude Code",
            _ => "Codex",
        };
        assert_eq!(
            result["release"],
            self.release.as_str(),
            "the recorder ran another {agent} than the installed one selected; {}",
            evidence.display()
        );
        let resolved_key = match self.program {
            "claude" => "executable",
            _ => "launcher",
        };
        let installed = self.executable.canonicalize().unwrap_or_else(|e| {
            panic!(
                "the installed `{}` at {} no longer resolves: {e}",
                self.program,
                self.executable.display()
            )
        });
        let resolved = result[resolved_key].as_str().unwrap_or_else(|| {
            panic!(
                "the recorder reported no `{resolved_key}`; {}",
                evidence.display()
            )
        });
        assert_eq!(
            Path::new(resolved).canonicalize().ok().as_deref(),
            Some(installed.as_path()),
            "the recorder ran {resolved}, not the installed {} the deck was given; {}",
            installed.display(),
            evidence.display()
        );
        if result["executable"].as_str() == Some(resolved) {
            assert_eq!(
                result["sha256"].as_str(),
                Some(self.sha256.get_or_init(|| sha256_of(&installed)).as_str()),
                "the executable the recorder hashed is not the installed {}; {}",
                installed.display(),
                evidence.display()
            );
        }
    }
}

/// The installed Claude Code: `claude --version` prints `<release> (Claude Code)`.
pub fn claude() -> Result<Installed, String> {
    installed("claude")
}

/// The installed Codex: `codex --version` prints `codex-cli <release>`.
pub fn codex() -> Result<Installed, String> {
    installed("codex")
}

/// The installed agent, or `None` after reporting why it cannot be measured
/// (`SKIP:`, or a failure under `DOT_AGENT_DECK_REQUIRE_REAL_E2E`).
pub fn or_skip(found: Result<Installed, String>) -> Option<Installed> {
    match found {
        Ok(installed) => Some(installed),
        Err(why) => {
            common::_skip_if_err(Err(why));
            None
        }
    }
}

fn installed(program: &'static str) -> Result<Installed, String> {
    let path = common::path_with_binary_dir();
    let executable = std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
        .ok_or_else(|| format!("no `{program}` is installed on the harness's PATH"))?;
    let output = std::process::Command::new(&executable)
        .arg("--version")
        .env("PATH", &path)
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("`{} --version` did not run: {e}", executable.display()))?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let release = match installed_release::version_shape(program, &text) {
        installed_release::VersionShape::Release(release) => release,
        installed_release::VersionShape::Tagged(version) => {
            return Err(format!(
                "{program} {version} is a pre-release or build-tagged build the deck does \
                 not support: nothing measured"
            ));
        }
        installed_release::VersionShape::NoRelease => {
            return Err(format!(
                "`{} --version` printed no release: {text:?}",
                executable.display()
            ));
        }
    };
    eprintln!(
        "INSTALLED-RELEASE: {program} {release} at {}",
        executable.display()
    );
    Ok(Installed {
        program,
        release,
        executable,
        path,
        sha256: std::sync::OnceLock::new(),
    })
}

/// The lowercase hex SHA-256 of the file at `path`, read in chunks: an
/// installed Claude Code is a few hundred MB.
fn sha256_of(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)
        .unwrap_or_else(|e| panic!("cannot read {} to hash it: {e}", path.display()));
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .unwrap_or_else(|e| panic!("cannot read {} to hash it: {e}", path.display()));
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_executable(candidate: &Path) -> bool {
    std::fs::metadata(candidate)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}
