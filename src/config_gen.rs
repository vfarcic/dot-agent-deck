use serde::Deserialize;
use std::fmt::Write as _;
use std::sync::OnceLock;

/// Prompt template for instructing an AI agent to generate a `.dot-agent-deck.toml`
/// configuration file by analyzing the project structure.
const CONFIG_GEN_PROMPT_TEMPLATE: &str = include_str!("../assets/config_gen_prompt.md");

/// Generic agent role library. Embedded at build time; the config-gen agent picks
/// from these roles when proposing an orchestration.
const ROLES_TOML: &str = include_str!("../assets/roles.toml");

#[derive(Debug, Deserialize)]
struct RoleTemplate {
    name: String,
    description: String,
    #[serde(default = "default_true")]
    clear: bool,
    #[serde(default)]
    prompt_template: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct RoleLibrary {
    role: Vec<RoleTemplate>,
}

static ROLE_LIBRARY: OnceLock<RoleLibrary> = OnceLock::new();

fn role_library() -> &'static RoleLibrary {
    ROLE_LIBRARY
        .get_or_init(|| toml::from_str(ROLES_TOML).expect("embedded roles.toml must be valid"))
}

fn render_roles_section(library: &RoleLibrary) -> String {
    let mut out = String::new();
    for role in &library.role {
        writeln!(&mut out, "### `{}`\n", role.name).unwrap();
        writeln!(&mut out, "- **Description:** {}", role.description).unwrap();
        writeln!(&mut out, "- **`clear` default:** `{}`", role.clear).unwrap();
        writeln!(
            &mut out,
            "- **Suggested `prompt_template`:** {}\n",
            role.prompt_template
        )
        .unwrap();
    }
    out
}

/// Build the config generation prompt for a specific directory.
pub fn config_gen_prompt(dir: &str) -> String {
    let roles_section = render_roles_section(role_library());
    CONFIG_GEN_PROMPT_TEMPLATE
        .replace("{dir}", dir)
        .replace("{roles}", &roles_section)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_interpolates_directory() {
        let prompt = config_gen_prompt("/home/user/my-project");
        assert!(prompt.contains("/home/user/my-project"));
        assert!(!prompt.contains("{dir}"));
    }

    #[test]
    fn prompt_contains_key_sections() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(prompt.contains("[[modes]]"));
        assert!(prompt.contains("[[modes.panes]]"));
        assert!(prompt.contains("[[modes.rules]]"));
        assert!(prompt.contains(".dot-agent-deck.toml"));
    }

    #[test]
    fn prompt_contains_orchestration_sections() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(prompt.contains("[[orchestrations]]"));
        assert!(prompt.contains("[[orchestrations.roles]]"));
        assert!(prompt.contains("start = true"));
        assert!(prompt.contains("## Orchestrations"));
    }

    #[test]
    fn prompt_contains_orchestration_guidelines() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(prompt.contains("Exactly one `start = true` role"));
        assert!(prompt.contains("All role names must be unique"));
        assert!(prompt.contains("prompt_template"));
        assert!(prompt.contains("description"));
        assert!(prompt.contains("clear"));
    }

    #[test]
    fn prompt_contains_orchestration_example() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(prompt.contains("dev-flow"));
        assert!(prompt.contains("reviewer"));
        assert!(prompt.contains("Propose exactly one orchestration"));
    }

    #[test]
    fn prompt_contains_role_library_section() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(prompt.contains("## Role Library"));
        assert!(!prompt.contains("{roles}"));
    }

    #[test]
    fn prompt_renders_every_library_role() {
        let prompt = config_gen_prompt("/tmp/test");
        for role in &role_library().role {
            assert!(
                prompt.contains(&format!("### `{}`", role.name)),
                "missing role header for `{}`",
                role.name
            );
            assert!(
                prompt.contains(&role.description),
                "missing description for role `{}`",
                role.name
            );
        }
    }

    #[test]
    fn role_library_parses_and_has_expected_roles() {
        let lib = role_library();
        let names: Vec<&str> = lib.role.iter().map(|r| r.name.as_str()).collect();
        for expected in [
            "coder",
            "reviewer",
            "auditor",
            "tester",
            "documenter",
            "release",
            "researcher",
        ] {
            assert!(
                names.contains(&expected),
                "role `{expected}` missing from library; got {names:?}"
            );
        }
        for role in &lib.role {
            assert!(!role.name.is_empty(), "role with empty name");
            assert!(
                !role.description.is_empty(),
                "role `{}` has empty description",
                role.name
            );
            assert!(
                !role.prompt_template.is_empty(),
                "role `{}` has empty prompt_template",
                role.name
            );
        }
    }

    #[test]
    fn prompt_contains_context_handoff_mandate() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(
            prompt.contains("Context-handoff rule"),
            "context-handoff mandate must appear in the orchestration composition guidance"
        );
        assert!(
            prompt.contains("clear = true"),
            "mandate must explain that workers cold-start with clear = true"
        );
    }

    #[test]
    fn every_worker_role_has_missing_context_backstop() {
        let lib = role_library();
        for role in &lib.role {
            assert!(
                role.prompt_template
                    .contains("the orchestrator will re-delegate with"),
                "role `{}` is missing the missing-context backstop sentence",
                role.name
            );
        }
    }

    #[test]
    fn release_role_has_clear_false() {
        let lib = role_library();
        let release = lib
            .role
            .iter()
            .find(|r| r.name == "release")
            .expect("release role present");
        assert!(
            !release.clear,
            "release role should default to clear = false so it can resume after failure"
        );
    }
}
