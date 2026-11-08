//! What an installed agent's `--version` output says about its release, as a
//! lane-2 measurement decides it (`tests/support/installed_agent.rs`). Pure,
//! so the fast tier checks it against the deck's own release scanner
//! (`doorbell_036_installed_release_shape_follows_the_release_scanner`).

/// The release an agent's `--version` output names, judged by the shape the
/// deck's release scanner latches from the agent's start-up header
/// (`src/release_scanner.rs`): a bare `MAJOR.MINOR.PATCH`, which the scanner
/// then parses with `semver::Version::parse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionShape {
    /// A bare release, the shape the scanner latches.
    Release(String),
    /// A version with a pre-release or build tag (`2.1.320-beta.1`,
    /// `2.1.320+abc`). The scanner latches nothing from such a header, so
    /// the deck rings no doorbell for that agent and nothing can be measured.
    Tagged(String),
    /// No version where the agent prints one.
    NoRelease,
}

/// Reads `text`, the `--version` output of `program` (`claude` or `codex`).
/// Claude Code prints `<release> (Claude Code)`, so its version is the first
/// word; Codex prints `codex-cli <release>`, so its version is the last.
pub fn version_shape(program: &str, text: &str) -> VersionShape {
    let mut words = text.split_whitespace();
    let word = match program {
        "claude" => words.next(),
        "codex" => words.last(),
        other => panic!("no `--version` shape is known for `{other}`"),
    };
    let Some(word) = word else {
        return VersionShape::NoRelease;
    };
    match semver::Version::parse(word) {
        Ok(version) if version.pre.is_empty() && version.build.is_empty() => {
            VersionShape::Release(word.to_string())
        }
        Ok(_) => VersionShape::Tagged(word.to_string()),
        Err(_) => VersionShape::NoRelease,
    }
}
