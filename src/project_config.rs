use std::path::Path;

use serde::Deserialize;

pub const CONFIG_FILE_NAME: &str = ".dot-agent-deck.toml";

#[derive(Debug, thiserror::Error)]
pub enum ProjectConfigError {
    #[error("Failed to read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("Failed to parse {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub modes: Vec<ModeConfig>,
    #[serde(default)]
    pub orchestrations: Vec<OrchestrationConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModeConfig {
    pub name: String,
    #[serde(default)]
    pub init_command: Option<String>,
    #[serde(default)]
    pub panes: Vec<ModePersistentPane>,
    #[serde(default)]
    pub rules: Vec<ModeRule>,
    #[serde(default = "default_reactive_panes")]
    pub reactive_panes: usize,
}

fn default_reactive_panes() -> usize {
    2
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModePersistentPane {
    pub command: String,
    pub name: Option<String>,
    #[serde(default = "default_pane_watch")]
    pub watch: bool,
}

fn default_pane_watch() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModeRule {
    pub pattern: String,
    #[serde(default)]
    pub watch: bool,
    pub interval: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrchestrationConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_max_rounds")]
    pub max_rounds: usize,
    #[serde(default)]
    pub auto: bool,
    pub roles: Vec<OrchestrationRoleConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrchestrationRoleConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub start: bool,
    pub prompt_template: String,
}

fn default_max_rounds() -> usize {
    3
}

pub fn load_project_config(dir: &Path) -> Result<Option<ProjectConfig>, ProjectConfigError> {
    let path = dir.join(CONFIG_FILE_NAME);
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let config: ProjectConfig =
                toml::from_str(&contents).map_err(|source| ProjectConfigError::Parse {
                    path: path.display().to_string(),
                    source,
                })?;
            Ok(Some(config))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ProjectConfigError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_full_config() {
        let toml = r#"
[[modes]]
name = "kubernetes-operations"
shell_init = "devbox shell"

[[modes.panes]]
command = "kubectl get applications -n argocd -w"
name = "ArgoCD Apps"

[[modes.panes]]
command = "kubectl get events -A -w"
name = "Events"

[[modes.rules]]
pattern = "kubectl\\s+.*(describe|explain)"
watch = false

[[modes.rules]]
pattern = "kubectl\\s+.*(get|top|logs)"
watch = true
interval = 2

[[modes.rules]]
pattern = "helm\\s+.*(status|list)"
watch = false
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.modes.len(), 1);

        let mode = &config.modes[0];
        assert_eq!(mode.name, "kubernetes-operations");
        assert_eq!(mode.panes.len(), 2);
        assert_eq!(
            mode.panes[0].command,
            "kubectl get applications -n argocd -w"
        );
        assert_eq!(mode.panes[0].name.as_deref(), Some("ArgoCD Apps"));
        assert_eq!(mode.panes[1].command, "kubectl get events -A -w");
        assert_eq!(mode.panes[1].name.as_deref(), Some("Events"));
        assert_eq!(mode.rules.len(), 3);
        assert_eq!(mode.rules[0].pattern, "kubectl\\s+.*(describe|explain)");
        assert!(!mode.rules[0].watch);
        assert!(mode.rules[0].interval.is_none());
        assert_eq!(mode.rules[1].pattern, "kubectl\\s+.*(get|top|logs)");
        assert!(mode.rules[1].watch);
        assert_eq!(mode.rules[1].interval, Some(2));
        assert!(!mode.rules[2].watch);
    }

    #[test]
    fn parse_minimal_config() {
        let toml = r#"
[[modes]]
name = "minimal"

[[modes.panes]]
command = "echo hello"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        let mode = &config.modes[0];
        assert_eq!(mode.name, "minimal");
        assert_eq!(mode.panes.len(), 1);
        assert!(mode.rules.is_empty());
    }

    #[test]
    fn watch_defaults_to_false() {
        let toml = r#"
[[modes]]
name = "test"

[[modes.rules]]
pattern = "some pattern"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        let rule = &config.modes[0].rules[0];
        assert!(!rule.watch);
        assert!(rule.interval.is_none());
    }

    #[test]
    fn pane_watch_defaults_to_true() {
        let toml = r#"
[[modes]]
name = "test"

[[modes.panes]]
command = "kubectl get pods"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert!(config.modes[0].panes[0].watch);
    }

    #[test]
    fn pane_watch_can_be_set_to_false() {
        let toml = r#"
[[modes]]
name = "test"

[[modes.panes]]
command = "kubectl get pods -w"
watch = false
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert!(!config.modes[0].panes[0].watch);
    }

    #[test]
    fn pane_name_defaults_to_none() {
        let toml = r#"
[[modes]]
name = "test"

[[modes.panes]]
command = "cargo test"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert!(config.modes[0].panes[0].name.is_none());
    }

    #[test]
    fn load_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let result = load_project_config(dir.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_malformed_toml_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE_NAME), "not valid { toml").unwrap();
        let result = load_project_config(dir.path());
        assert!(matches!(result, Err(ProjectConfigError::Parse { .. })));
    }

    #[test]
    fn load_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"
[[modes]]
name = "test-mode"

[[modes.panes]]
command = "echo hi"
name = "Greeter"
"#;
        std::fs::write(dir.path().join(CONFIG_FILE_NAME), toml).unwrap();
        let config = load_project_config(dir.path()).unwrap().unwrap();
        assert_eq!(config.modes[0].name, "test-mode");
        assert_eq!(config.modes[0].panes[0].name.as_deref(), Some("Greeter"));
    }

    #[test]
    fn reactive_panes_defaults_to_two() {
        let toml = r#"
[[modes]]
name = "test"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.modes[0].reactive_panes, 2);
    }

    #[test]
    fn reactive_panes_configurable() {
        let toml = r#"
[[modes]]
name = "test"
reactive_panes = 4
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.modes[0].reactive_panes, 4);
    }

    #[test]
    fn parse_full_orchestration_config() {
        let toml = r#"
[[orchestrations]]
name = "tdd-cycle"
max_rounds = 5
auto = true

[[orchestrations.roles]]
name = "tester"
command = "claude"
start = true
prompt_template = "Write failing tests."

[[orchestrations.roles]]
name = "coder"
command = "claude --model sonnet"
prompt_template = "Make the tests pass."
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.orchestrations.len(), 1);
        let orch = &config.orchestrations[0];
        assert_eq!(orch.name, "tdd-cycle");
        assert_eq!(orch.max_rounds, 5);
        assert!(orch.auto);
        assert_eq!(orch.roles.len(), 2);
        assert_eq!(orch.roles[0].name, "tester");
        assert_eq!(orch.roles[0].command, "claude");
        assert!(orch.roles[0].start);
        assert_eq!(orch.roles[0].prompt_template, "Write failing tests.");
        assert_eq!(orch.roles[1].name, "coder");
        assert!(!orch.roles[1].start);
    }

    #[test]
    fn parse_orchestration_alongside_modes() {
        let toml = r#"
[[modes]]
name = "dev"

[[modes.panes]]
command = "echo hi"

[[orchestrations]]
name = "review"

[[orchestrations.roles]]
name = "writer"
command = "claude"
start = true
prompt_template = "Write code."

[[orchestrations.roles]]
name = "reviewer"
command = "claude"
prompt_template = "Review code."
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.modes.len(), 1);
        assert_eq!(config.orchestrations.len(), 1);
    }

    #[test]
    fn orchestration_max_rounds_defaults_to_three() {
        let toml = r#"
[[orchestrations]]
name = "test"

[[orchestrations.roles]]
name = "a"
command = "claude"
start = true
prompt_template = "Do A."

[[orchestrations.roles]]
name = "b"
command = "claude"
prompt_template = "Do B."
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.orchestrations[0].max_rounds, 3);
    }

    #[test]
    fn orchestration_auto_defaults_to_false() {
        let toml = r#"
[[orchestrations]]
name = "test"

[[orchestrations.roles]]
name = "a"
command = "claude"
start = true
prompt_template = "Do A."

[[orchestrations.roles]]
name = "b"
command = "claude"
prompt_template = "Do B."
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert!(!config.orchestrations[0].auto);
    }

    #[test]
    fn orchestration_role_start_defaults_to_false() {
        let toml = r#"
[[orchestrations]]
name = "test"

[[orchestrations.roles]]
name = "worker"
command = "claude"
prompt_template = "Work."
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert!(!config.orchestrations[0].roles[0].start);
    }

    #[test]
    fn modes_only_config_still_works() {
        let toml = r#"
[[modes]]
name = "dev"

[[modes.panes]]
command = "echo hi"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.modes.len(), 1);
        assert!(config.orchestrations.is_empty());
    }

    #[test]
    fn orchestrations_only_config_works() {
        let toml = r#"
[[orchestrations]]
name = "test"

[[orchestrations.roles]]
name = "a"
command = "claude"
start = true
prompt_template = "Do A."

[[orchestrations.roles]]
name = "b"
command = "claude"
prompt_template = "Do B."
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert!(config.modes.is_empty());
        assert_eq!(config.orchestrations.len(), 1);
    }

    #[test]
    fn missing_required_pattern_is_error() {
        let toml = r#"
[[modes]]
name = "test"

[[modes.rules]]
watch = true
"#;
        let result: Result<ProjectConfig, _> = toml::from_str(toml);
        assert!(result.is_err());
    }
}
