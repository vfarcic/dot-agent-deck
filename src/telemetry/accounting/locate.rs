//! PRD #1858: where the deck finds each run's usage record, and which paths
//! it hands a reader at all.
//!
//! Every path a reader opens is built here from values the deck holds — an
//! agent's configuration directory, the session id and working directory an
//! agent's hook reported, the deck's own run id — or, for Codex, admitted
//! here from the path Codex's hook reported: under the agent's own sessions
//! directory, named as the agent names it. A reader then opens it without
//! following a symlink in its last component and refuses anything but a
//! regular file of this user's ([`super::JsonlReader`]).

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use super::Ident;

/// The most project directories [`claude_transcript`] looks through for a
/// session it cannot find where its directory name says.
pub const MAX_PROJECT_DIRS: usize = 4096;

/// The longest directory name Claude Code derives from a working directory
/// without shortening it; a longer one is looked for by session id instead.
const MAX_PLAIN_PROJECT_NAME: usize = 200;

/// An absolute path from an environment value, or `None`.
fn absolute(value: Option<&OsStr>) -> Option<PathBuf> {
    let path = PathBuf::from(value.filter(|v| !v.is_empty())?);
    path.is_absolute().then_some(path)
}

/// Claude Code's configuration directory: `CLAUDE_CONFIG_DIR`, else
/// `.claude` in `HOME`, each only when absolute.
pub fn claude_config_dir(
    claude_config_dir: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    absolute(claude_config_dir).or_else(|| absolute(home).map(|h| h.join(".claude")))
}

/// Codex's home: `CODEX_HOME`, else `.codex` in `HOME`, each only when
/// absolute.
pub fn codex_home(codex_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    absolute(codex_home).or_else(|| absolute(home).map(|h| h.join(".codex")))
}

/// Whether `id` can name a Claude Code session's transcript: 1 to 128 ASCII
/// letters, digits, `-` and `_`, so it cannot climb out of a directory.
pub fn admissible_session_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The directory name Claude Code keeps a working directory's transcripts
/// under: every character but an ASCII letter or digit becomes `-`. `None`
/// for a name longer than Claude Code keeps unshortened.
pub fn claude_project_dir_name(cwd: &str) -> Option<String> {
    let name: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    (name.len() <= MAX_PLAIN_PROJECT_NAME).then_some(name)
}

/// The transcript of Claude Code session `session_id`, started in `cwd`:
/// `projects/<cwd's directory name>/<session_id>.jsonl` under
/// `config_dir`, or, when that is not there (a working directory whose name
/// Claude Code shortened, or one the hook reported differently), the file of
/// that name in any of the first [`MAX_PROJECT_DIRS`] project directories.
/// `None` for a session id that could name another file, or when no such
/// regular file exists. A directory symlinked into `projects` is not
/// searched.
pub fn claude_transcript(
    config_dir: &Path,
    cwd: Option<&str>,
    session_id: &str,
) -> Option<PathBuf> {
    if !admissible_session_id(session_id) {
        return None;
    }
    let projects = config_dir.join("projects");
    let file = format!("{session_id}.jsonl");
    let is_file = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file());
    if let Some(name) = cwd.and_then(claude_project_dir_name) {
        let path = projects.join(name).join(&file);
        if is_file(&path) {
            return Some(path);
        }
    }
    std::fs::read_dir(&projects)
        .ok()?
        .flatten()
        .take(MAX_PROJECT_DIRS)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.path().join(&file))
        .find(|path| is_file(path))
}

/// The rollout path a Codex hook reported, admitted only when it is
/// absolute, has no `..`, resolves to a file named `rollout-*.jsonl`, and
/// resolves to somewhere under `sessions` in `codex_home`. Returns the
/// resolved path.
pub fn admit_codex_rollout(reported: &str, codex_home: &Path) -> Option<PathBuf> {
    let path = Path::new(reported);
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return None;
    }
    let resolved = std::fs::canonicalize(path).ok()?;
    let root = std::fs::canonicalize(codex_home.join("sessions")).ok()?;
    let name = resolved.file_name()?.to_str()?;
    (resolved.starts_with(&root) && name.starts_with("rollout-") && name.ends_with(".jsonl"))
        .then_some(resolved)
}

/// The usage file of run `run_id` in `dir`, the deck's own per-run
/// directory: `<run_id>.usage.jsonl`. `None` for a run id that is not an
/// identifier or that holds a `/`.
pub fn usage_file(dir: &Path, run_id: &str) -> Option<PathBuf> {
    let id = Ident::new(run_id)?;
    (!id.as_str().contains('/') && !id.as_str().starts_with('.'))
        .then(|| dir.join(format!("{}.usage.jsonl", id.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    #[test]
    fn config_dirs_come_from_the_variable_then_home_and_must_be_absolute() {
        let os = |s: &str| Some(OsStr::new(s).to_owned());
        assert_eq!(
            claude_config_dir(os("/c").as_deref(), os("/h").as_deref()),
            Some(PathBuf::from("/c"))
        );
        assert_eq!(
            claude_config_dir(None, os("/h").as_deref()),
            Some(PathBuf::from("/h/.claude"))
        );
        assert_eq!(
            claude_config_dir(os("rel").as_deref(), os("rel").as_deref()),
            None
        );
        assert_eq!(
            codex_home(None, os("/h").as_deref()),
            Some(PathBuf::from("/h/.codex"))
        );
        assert_eq!(codex_home(os("").as_deref(), None), None);
    }

    #[test]
    fn a_project_directory_name_replaces_everything_but_letters_and_digits() {
        assert_eq!(
            claude_project_dir_name("/home/u/code/dot-agent.deck").as_deref(),
            Some("-home-u-code-dot-agent-deck")
        );
        assert_eq!(claude_project_dir_name(&"/a".repeat(101)), None);
    }

    /// The transcript is found where the directory name says, else by its
    /// session id in another project directory; a session id that could name
    /// another file is refused, and a symlinked transcript is not taken.
    #[test]
    fn a_transcript_is_found_by_directory_then_by_session_id() {
        let dir = crate::test_temp::tempdir().unwrap();
        let config = dir.path();
        let id = "0f1e2d3c-aaaa-bbbb-cccc-1234567890ab";
        let here = config.join("projects").join("-work-repo");
        std::fs::create_dir_all(&here).unwrap();
        File::create(here.join(format!("{id}.jsonl"))).unwrap();
        assert_eq!(
            claude_transcript(config, Some("/work/repo"), id),
            Some(here.join(format!("{id}.jsonl")))
        );
        assert_eq!(
            claude_transcript(config, Some("/elsewhere"), id),
            Some(here.join(format!("{id}.jsonl"))),
            "found by session id when the directory differs"
        );
        assert_eq!(claude_transcript(config, Some("/work/repo"), "../x"), None);
        assert_eq!(claude_transcript(config, Some("/work/repo"), "a/b"), None);
        assert_eq!(claude_transcript(config, None, "missing"), None);
        #[cfg(unix)]
        {
            let link_id = "linked";
            std::os::unix::fs::symlink(
                here.join(format!("{id}.jsonl")),
                here.join(format!("{link_id}.jsonl")),
            )
            .unwrap();
            assert_eq!(claude_transcript(config, Some("/work/repo"), link_id), None);
        }
    }

    /// A rollout is admitted only under the Codex home's `sessions`, by its
    /// own name, as the path it resolves to.
    #[test]
    fn a_rollout_is_admitted_only_under_the_sessions_root() {
        let dir = crate::test_temp::tempdir().unwrap();
        let home = dir.path().join(".codex");
        let day = home.join("sessions/2026/10/26");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = day.join("rollout-2026-10-26T10-00-00-abc.jsonl");
        File::create(&rollout).unwrap();
        let other = dir.path().join("rollout-elsewhere.jsonl");
        File::create(&other).unwrap();
        let wrong_name = day.join("notes.jsonl");
        File::create(&wrong_name).unwrap();
        let canonical = std::fs::canonicalize(&rollout).unwrap();
        assert_eq!(
            admit_codex_rollout(rollout.to_str().unwrap(), &home),
            Some(canonical)
        );
        assert_eq!(admit_codex_rollout(other.to_str().unwrap(), &home), None);
        assert_eq!(
            admit_codex_rollout(wrong_name.to_str().unwrap(), &home),
            None
        );
        let climbing = format!(
            "{}/../26/{}",
            day.display(),
            "rollout-2026-10-26T10-00-00-abc.jsonl"
        );
        assert_eq!(admit_codex_rollout(&climbing, &home), None);
        assert_eq!(admit_codex_rollout("rollout-x.jsonl", &home), None);
        #[cfg(unix)]
        {
            let link = day.join("rollout-link.jsonl");
            std::os::unix::fs::symlink(&other, &link).unwrap();
            assert_eq!(
                admit_codex_rollout(link.to_str().unwrap(), &home),
                None,
                "a link out of the root resolves outside it"
            );
        }
    }

    #[test]
    fn a_usage_file_is_named_by_an_identifier_run_id_only() {
        let dir = Path::new("/state/telemetry");
        assert_eq!(
            usage_file(dir, "run-1a2b"),
            Some(dir.join("run-1a2b.usage.jsonl"))
        );
        assert_eq!(usage_file(dir, "../x"), None);
        assert_eq!(usage_file(dir, "a/b"), None);
        assert_eq!(usage_file(dir, "a b"), None);
        assert_eq!(usage_file(dir, ".hidden"), None);
    }
}
