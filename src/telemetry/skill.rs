//! Which skill an agent's skill tool loaded — for `deck.skill` events on the
//! `agent.run` span (issue #2087).
//!
//! **A skill's name is sent only when a skill directory on this machine
//! defines it.** The name arrives as an argument of a tool call, which the deck
//! otherwise never sends, so the hook does not pass it on as it came: it keeps
//! the name only when it has the shape of a skill directory's name
//! ([`is_skill_name`]) and `<root>/<name>/SKILL.md` exists under one of the
//! skill roots the agents read ([`defined_skill`]): a project's
//! `.claude/skills`, `.agents/skills` or `.opencode/skill(s)` in the call's
//! working directory or a directory above it up to the repository's top, or
//! the user's own skill directories. So what can reach a backend is the name
//! of a directory the user or a project created, as the orchestration and role
//! names from a configuration do — never the tool's free-text arguments, a
//! path, or a name the model made up. The shape check comes first, so no path
//! is built from a name that could leave the root.
//!
//! The tool call is still counted when its name is not kept: the event goes out
//! without `deck.skill.name`, so the share of calls whose skill is known can be
//! measured.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::event::AgentType;

/// The event metadata key the hook puts a defined skill's name under.
pub const SKILL_NAME_METADATA_KEY: &str = "skill_name";

/// The longest skill name kept: Claude Code's own limit for a skill's name.
pub const MAX_SKILL_NAME_LEN: usize = 64;

/// The project skill roots, relative to the call's working directory or a
/// directory above it: Claude Code's, the cross-agent `.agents` one, and
/// OpenCode's two.
const PROJECT_SKILL_ROOTS: &[&str] = &[
    ".claude/skills",
    ".agents/skills",
    ".opencode/skills",
    ".opencode/skill",
];

/// Directories looked at, from the working directory up, when no repository's
/// top stops the walk sooner.
const MAX_ANCESTORS: usize = 32;

/// The input field naming the skill when `tool_name` is `agent`'s skill tool:
/// Claude Code's `Skill` takes `skill`, OpenCode's `skill` takes `name`. Codex
/// and Pi load a skill by reading its file with their ordinary tools, and
/// Devin reports no skill tool, so for them there is none.
pub fn skill_tool_field(agent: &AgentType, tool_name: Option<&str>) -> Option<&'static str> {
    match (agent, tool_name?) {
        (AgentType::ClaudeCode, "Skill") => Some("skill"),
        (AgentType::OpenCode, "skill") => Some("name"),
        _ => None,
    }
}

/// Whether `tool_name` is `agent`'s skill tool.
pub fn is_skill_tool(agent: &AgentType, tool_name: Option<&str>) -> bool {
    skill_tool_field(agent, tool_name).is_some()
}

/// Whether `name` has the shape of a skill directory's name: lowercase ASCII
/// letters, digits, `-`, `_` and `.`, starting with a letter or digit, at most
/// [`MAX_SKILL_NAME_LEN`] bytes. No `/`, no `..`, no `:` (a plugin's
/// namespaced skill), no uppercase, no space.
pub fn is_skill_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.len() <= MAX_SKILL_NAME_LEN
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b))
        && !name.contains("..")
}

/// The user's own skill roots, from this process's environment: Claude Code's
/// (`$CLAUDE_CONFIG_DIR/skills`, else `~/.claude/skills`), `~/.agents/skills`
/// and OpenCode's (`$XDG_CONFIG_HOME/opencode/skill(s)`, else under
/// `~/.config`). Relative values are ignored.
pub fn user_skill_roots() -> Vec<PathBuf> {
    let var = |key: &str| {
        std::env::var_os(key)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    user_skill_roots_from(
        var("HOME").as_deref(),
        var("CLAUDE_CONFIG_DIR").as_deref(),
        var("XDG_CONFIG_HOME").as_deref(),
    )
}

fn user_skill_roots_from(
    home: Option<&Path>,
    claude_config_dir: Option<&Path>,
    xdg_config_home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    match (claude_config_dir, home) {
        (Some(dir), _) => roots.push(dir.join("skills")),
        (None, Some(home)) => roots.push(home.join(".claude/skills")),
        (None, None) => {}
    }
    if let Some(home) = home {
        roots.push(home.join(".agents/skills"));
    }
    let config = xdg_config_home
        .map(Path::to_path_buf)
        .or_else(|| home.map(|h| h.join(".config")));
    if let Some(config) = config {
        roots.push(config.join("opencode/skills"));
        roots.push(config.join("opencode/skill"));
    }
    roots
}

/// `name`, when it passes [`is_skill_name`] and a skill directory defines it:
/// `<root>/<name>/SKILL.md` is a file under a project root
/// ([`PROJECT_SKILL_ROOTS`]) of `cwd` or a directory above it, up to and
/// including the first that holds `.git`, or under one of `user_roots`. A
/// relative or absent `cwd` looks at the user roots only.
pub fn defined_skill(name: &str, cwd: Option<&Path>, user_roots: &[PathBuf]) -> Option<String> {
    if !is_skill_name(name) {
        return None;
    }
    let defines = |root: &Path| root.join(name).join("SKILL.md").is_file();
    if let Some(cwd) = cwd.filter(|c| c.is_absolute()) {
        for dir in cwd.ancestors().take(MAX_ANCESTORS) {
            if PROJECT_SKILL_ROOTS.iter().any(|r| defines(&dir.join(r))) {
                return Some(name.to_string());
            }
            if dir.join(".git").exists() {
                break;
            }
        }
    }
    user_roots
        .iter()
        .any(|root| defines(root))
        .then(|| name.to_string())
}

/// The name of the defined skill a skill-tool call of `agent` loads, from its
/// `tool_input`, as [`defined_skill`] keeps it. `None` for any other tool.
pub fn called_skill(
    agent: &AgentType,
    tool_name: Option<&str>,
    tool_input: Option<&Value>,
    cwd: Option<&str>,
    user_roots: &[PathBuf],
) -> Option<String> {
    let field = skill_tool_field(agent, tool_name)?;
    let name = tool_input?.get(field)?.as_str()?;
    defined_skill(name, cwd.map(Path::new), user_roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn define(root: &Path, name: &str) {
        std::fs::create_dir_all(root.join(name)).unwrap();
        std::fs::write(root.join(name).join("SKILL.md"), "---\nname: x\n---\n").unwrap();
    }

    #[test]
    fn a_skill_name_is_a_plain_directory_name() {
        for ok in [
            "pr-create",
            "merge-pr",
            "a",
            "dot-ai-changelog-fragment",
            "v2.1",
            "x_y",
        ] {
            assert!(is_skill_name(ok), "{ok}");
        }
        let long = "a".repeat(MAX_SKILL_NAME_LEN + 1);
        for bad in [
            "",
            "..",
            "a..b",
            "../x",
            "a/b",
            "/etc",
            ".hidden",
            "-x",
            "Upper",
            "two words",
            "plugin:skill",
            "naïve",
            long.as_str(),
        ] {
            assert!(!is_skill_name(bad), "{bad}");
        }
    }

    #[test]
    fn only_the_agents_skill_tool_names_a_field() {
        assert_eq!(
            skill_tool_field(&AgentType::ClaudeCode, Some("Skill")),
            Some("skill")
        );
        assert_eq!(
            skill_tool_field(&AgentType::OpenCode, Some("skill")),
            Some("name")
        );
        assert_eq!(
            skill_tool_field(&AgentType::ClaudeCode, Some("skill")),
            None
        );
        assert_eq!(skill_tool_field(&AgentType::OpenCode, Some("Skill")), None);
        assert_eq!(skill_tool_field(&AgentType::Codex, Some("Skill")), None);
        assert_eq!(skill_tool_field(&AgentType::Pi, Some("skill")), None);
        assert_eq!(skill_tool_field(&AgentType::ClaudeCode, Some("Bash")), None);
        assert_eq!(skill_tool_field(&AgentType::ClaudeCode, None), None);
    }

    #[test]
    fn a_project_skill_is_found_from_a_subdirectory_up_to_the_repository_top() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let nested = repo.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        define(&repo.join(".claude/skills"), "pr-create");
        define(&repo.join(".opencode/skill"), "oc-skill");
        // Above the repository's top: not this project's.
        define(&dir.path().join(".claude/skills"), "outside");

        assert_eq!(
            defined_skill("pr-create", Some(&nested), &[]).as_deref(),
            Some("pr-create")
        );
        assert_eq!(
            defined_skill("oc-skill", Some(&repo), &[]).as_deref(),
            Some("oc-skill")
        );
        assert_eq!(defined_skill("outside", Some(&nested), &[]), None);
        assert_eq!(defined_skill("missing", Some(&nested), &[]), None);
        // A directory without SKILL.md defines nothing.
        std::fs::create_dir_all(repo.join(".claude/skills/empty")).unwrap();
        assert_eq!(defined_skill("empty", Some(&repo), &[]), None);
        // A relative working directory looks at no project.
        assert_eq!(
            defined_skill("pr-create", Some(Path::new("repo")), &[]),
            None
        );
    }

    #[test]
    fn a_path_shaped_name_is_refused_before_any_path_is_built() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join(".claude/skills");
        define(&skills, "real");
        // `<root>/../skills/real/SKILL.md` exists, so only the shape check
        // keeps this name out.
        assert_eq!(defined_skill("../skills/real", Some(dir.path()), &[]), None);
        assert_eq!(
            defined_skill("real", Some(dir.path()), &[]).as_deref(),
            Some("real")
        );
    }

    #[test]
    fn a_user_skill_is_found_in_the_user_roots() {
        let home = tempfile::tempdir().unwrap();
        let roots = user_skill_roots_from(Some(home.path()), None, None);
        assert_eq!(
            roots,
            vec![
                home.path().join(".claude/skills"),
                home.path().join(".agents/skills"),
                home.path().join(".config/opencode/skills"),
                home.path().join(".config/opencode/skill"),
            ]
        );
        define(&home.path().join(".claude/skills"), "mine");
        assert_eq!(defined_skill("mine", None, &roots).as_deref(), Some("mine"));

        let config = tempfile::tempdir().unwrap();
        let roots =
            user_skill_roots_from(Some(home.path()), Some(config.path()), Some(config.path()));
        assert_eq!(roots[0], config.path().join("skills"));
        assert_eq!(roots[2], config.path().join("opencode/skills"));
        assert_eq!(defined_skill("mine", None, &roots), None);
        assert!(user_skill_roots_from(None, None, None).is_empty());
    }

    #[test]
    fn a_call_names_its_skill_from_the_agents_field_only() {
        let dir = tempfile::tempdir().unwrap();
        define(&dir.path().join(".claude/skills"), "pr-create");
        let cwd = dir.path().to_str();
        let claude = json!({"skill": "pr-create", "args": "Closes #1"});
        assert_eq!(
            called_skill(
                &AgentType::ClaudeCode,
                Some("Skill"),
                Some(&claude),
                cwd,
                &[]
            )
            .as_deref(),
            Some("pr-create")
        );
        let opencode = json!({"name": "pr-create"});
        assert_eq!(
            called_skill(
                &AgentType::OpenCode,
                Some("skill"),
                Some(&opencode),
                cwd,
                &[]
            )
            .as_deref(),
            Some("pr-create")
        );
        // The arguments are never read as a name.
        let args_only = json!({"args": "pr-create"});
        assert_eq!(
            called_skill(
                &AgentType::ClaudeCode,
                Some("Skill"),
                Some(&args_only),
                cwd,
                &[]
            ),
            None
        );
        assert_eq!(
            called_skill(
                &AgentType::ClaudeCode,
                Some("Read"),
                Some(&claude),
                cwd,
                &[]
            ),
            None
        );
    }
}
