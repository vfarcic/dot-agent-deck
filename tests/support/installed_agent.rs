//! The agent release a lane-2 measurement runs against: the `claude` or
//! `codex` installed on the contained `PATH` the harness gives a deck pane
//! (issue #2146). PRD #1666 part 3 gave the doorbell one set of facts per
//! agent, so a measurement asserts those facts against whatever release is
//! installed; the recorded captures stay in the repository as the reference
//! (`docs/develop/lane2-installed-release-map.md`).
//!
//! Each test runs its agent's credential preflight first
//! (`skip_unless!(common::check_claude_available())` or the Codex one), then
//! [`or_skip`] over [`claude`] or [`codex`]: no executable on the `PATH`, or
//! one whose `--version` names no release, prints `SKIP: [e2e] …` and the
//! test returns; `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` turns that into a
//! failure (CLAUDE.md rule 5).
#![allow(dead_code)] // Each test file uses its own subset.

use crate::common;
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
}

/// The installed Claude Code: `claude --version` prints `<release> (Claude Code)`.
pub fn claude() -> Result<Installed, String> {
    installed("claude", |text| text.split_whitespace().next())
}

/// The installed Codex: `codex --version` prints `codex-cli <release>`.
pub fn codex() -> Result<Installed, String> {
    installed("codex", |text| text.split_whitespace().last())
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

fn installed(
    program: &'static str,
    release_in: fn(&str) -> Option<&str>,
) -> Result<Installed, String> {
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
    let release = release_in(&text)
        .filter(|release| semver::Version::parse(release).is_ok())
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "`{} --version` printed no release: {text:?}",
                executable.display()
            )
        })?;
    eprintln!(
        "INSTALLED-RELEASE: {program} {release} at {}",
        executable.display()
    );
    Ok(Installed {
        program,
        release,
        executable,
        path,
    })
}

fn is_executable(candidate: &Path) -> bool {
    std::fs::metadata(candidate)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}
