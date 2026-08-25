use crate::project_config::ProjectConfig;
use regex::Regex;
use std::collections::HashSet;

/// Sanitize a role name for safe use in filenames.
/// Strips path separators, null bytes, and parent-directory sequences.
/// Path separators are removed first so that inputs like `./.` cannot
/// produce `..` after slash removal.
pub fn sanitize_role_name(name: &str) -> String {
    name.replace(['/', '\\', '\0'], "").replace("..", "")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct ValidationIssue {
    pub severity: Severity,
    pub scope: String,
    pub message: String,
}

impl std::fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let level = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(f, "[{}] '{}': {}", level, self.scope, self.message)
    }
}

/// Validate a project config and return a list of issues.
/// Errors should prevent mode activation; warnings are informational.
pub fn validate_config(config: &ProjectConfig) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();

    // Check for duplicate mode names.
    let mut seen_names = HashSet::new();
    for mode in &config.modes {
        if !seen_names.insert(&mode.name) {
            issues.push(ValidationIssue {
                severity: Severity::Warning,
                scope: mode.name.clone(),
                message: "duplicate mode name".to_string(),
            });
        }
    }

    for mode in &config.modes {
        // Reject modes with rules but zero reactive panes.
        if !mode.rules.is_empty() && mode.reactive_panes == 0 {
            issues.push(ValidationIssue {
                severity: Severity::Error,
                scope: mode.name.clone(),
                message: "modes with reactive rules must configure at least one reactive pane"
                    .to_string(),
            });
        }

        // Validate regex patterns.
        for rule in &mode.rules {
            if let Err(e) = Regex::new(&rule.pattern) {
                issues.push(ValidationIssue {
                    severity: Severity::Error,
                    scope: mode.name.clone(),
                    message: format!("invalid regex '{}': {}", rule.pattern, e),
                });
            }
        }

        // Warn if interval is set but watch is false.
        for rule in &mode.rules {
            if rule.interval.is_some() && !rule.watch {
                issues.push(ValidationIssue {
                    severity: Severity::Warning,
                    scope: mode.name.clone(),
                    message: format!(
                        "rule '{}' has interval but watch is false — interval will be ignored",
                        rule.pattern
                    ),
                });
            }
        }
    }

    // Issue #308: a declared `agent = "…"` that no shipped agent claims is a
    // WARNING, not an error — the config still loads and the mode still opens,
    // it just gets no agent. Worth saying out loud because the failure is
    // otherwise silent and looks exactly like the bug the key exists to fix: the
    // pane reads "No agent", which is precisely what a user reaches for this key
    // to stop seeing. `AgentType::None` is what an unrecognized name resolves
    // to, deliberately — the declaration is honored rather than quietly
    // replaced by a guess from the command — so the only place to surface a typo
    // is here.
    for mode in &config.modes {
        if let Some(issue) = unknown_agent_issue(&mode.name, mode.agent.as_deref()) {
            issues.push(issue);
        }
    }

    // Check for duplicate orchestration names.
    let mut seen_orch_names = HashSet::new();
    for orch in &config.orchestrations {
        if !seen_orch_names.insert(&orch.name) {
            issues.push(ValidationIssue {
                severity: Severity::Warning,
                scope: orch.name.clone(),
                message: "duplicate orchestration name".to_string(),
            });
        }
    }

    for orch in &config.orchestrations {
        // Must have at least 2 roles.
        if orch.roles.len() < 2 {
            issues.push(ValidationIssue {
                severity: Severity::Error,
                scope: orch.name.clone(),
                message: "orchestration must have at least 2 roles".to_string(),
            });
        }

        // Exactly one start role.
        let start_count = orch.roles.iter().filter(|r| r.start).count();
        if start_count != 1 {
            issues.push(ValidationIssue {
                severity: Severity::Error,
                scope: orch.name.clone(),
                message: "orchestration must have exactly one role with start = true".to_string(),
            });
        }

        // Reject empty/whitespace role names and commands, and filesystem-unsafe characters.
        for role in &orch.roles {
            if role.name.trim().is_empty() {
                issues.push(ValidationIssue {
                    severity: Severity::Error,
                    scope: orch.name.clone(),
                    message: "role name is empty or whitespace".to_string(),
                });
            } else if role.name.contains("..")
                || role.name.contains('/')
                || role.name.contains('\\')
                || role.name.contains('\0')
            {
                issues.push(ValidationIssue {
                    severity: Severity::Error,
                    scope: orch.name.clone(),
                    message: format!(
                        "role name '{}' contains unsafe path characters (../, /, or \\)",
                        role.name
                    ),
                });
            }
            if role.command.trim().is_empty() {
                issues.push(ValidationIssue {
                    severity: Severity::Error,
                    scope: orch.name.clone(),
                    message: format!("role '{}' has an empty command", role.name),
                });
            }
        }

        // Unique role names.
        let mut seen_role_names = HashSet::new();
        for role in &orch.roles {
            if !seen_role_names.insert(&role.name) {
                issues.push(ValidationIssue {
                    severity: Severity::Error,
                    scope: orch.name.clone(),
                    message: format!("duplicate role name '{}'", role.name),
                });
            }
        }

        // Issue #308: same unknown-name warning for a role declaration.
        for role in &orch.roles {
            if let Some(issue) = unknown_agent_issue(&orch.name, role.agent.as_deref()) {
                issues.push(ValidationIssue {
                    message: format!("role '{}': {}", role.name, issue.message),
                    ..issue
                });
            }
        }

        // Warn about worker roles without descriptions (helps orchestrator know capabilities).
        for role in &orch.roles {
            if !role.start && role.description.is_none() {
                issues.push(ValidationIssue {
                    severity: Severity::Warning,
                    scope: orch.name.clone(),
                    message: format!(
                        "worker role '{}' has no description — orchestrator won't know its capabilities",
                        role.name
                    ),
                });
            }
        }
    }

    issues
}

/// Issue #308: the warning for an `agent = "…"` declaration no shipped agent
/// claims, or `None` when the declaration is absent, blank (which reads as
/// unset) or recognized.
///
/// Resolution goes through exactly the accessor the spawn seams use, so this
/// warns for precisely the values that will produce an agent-less pane and for
/// no others.
fn unknown_agent_issue(scope: &str, declared: Option<&str>) -> Option<ValidationIssue> {
    let name = declared?.trim();
    if name.is_empty() || crate::agent_registry::detect_from_basename(name).is_some() {
        return None;
    }
    Some(ValidationIssue {
        severity: Severity::Warning,
        scope: scope.to_string(),
        message: format!(
            "unknown agent '{name}' — this pane will have no agent and no wrapper; known agents: {}",
            crate::agent_registry::declarable_agent_names().join(", ")
        ),
    })
}

/// Returns true if any issue is an error.
pub fn has_errors(issues: &[ValidationIssue]) -> bool {
    issues.iter().any(|i| i.severity == Severity::Error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_config::{
        ModeConfig, ModeRule, OrchestrationConfig, OrchestrationRoleConfig, ProjectConfig,
    };

    fn make_config(modes: Vec<ModeConfig>) -> ProjectConfig {
        ProjectConfig {
            modes,
            orchestrations: vec![],
            worker_response_timeout_minutes:
                crate::project_config::DEFAULT_WORKER_RESPONSE_TIMEOUT_MINUTES,
        }
    }

    fn make_role(name: &str, start: bool) -> OrchestrationRoleConfig {
        OrchestrationRoleConfig {
            agent: None,
            name: name.to_string(),
            command: "claude".to_string(),
            start,
            description: if start {
                None
            } else {
                Some(format!("Does {name} tasks"))
            },
            prompt_template: None,
            clear: true,
        }
    }

    fn make_orchestration(name: &str, roles: Vec<OrchestrationRoleConfig>) -> OrchestrationConfig {
        OrchestrationConfig {
            name: name.to_string(),
            roles,
        }
    }

    fn make_orch_config(orchestrations: Vec<OrchestrationConfig>) -> ProjectConfig {
        ProjectConfig {
            modes: vec![],
            orchestrations,
            worker_response_timeout_minutes:
                crate::project_config::DEFAULT_WORKER_RESPONSE_TIMEOUT_MINUTES,
        }
    }

    fn make_mode(name: &str, rules: Vec<ModeRule>) -> ModeConfig {
        ModeConfig {
            agent: None,
            name: name.to_string(),
            init_command: None,
            seed_prompt: None,
            panes: vec![],
            rules,
            reactive_panes: 2,
        }
    }

    fn make_rule(pattern: &str, watch: bool, interval: Option<u64>) -> ModeRule {
        ModeRule {
            pattern: pattern.to_string(),
            watch,
            interval,
        }
    }

    #[test]
    fn valid_config_has_no_issues() {
        let config = make_config(vec![make_mode(
            "dev",
            vec![make_rule("cargo\\s+build", false, None)],
        )]);
        let issues = validate_config(&config);
        assert!(issues.is_empty());
    }

    #[test]
    fn invalid_regex_produces_error() {
        let config = make_config(vec![make_mode(
            "dev",
            vec![make_rule("[invalid", false, None)],
        )]);
        let issues = validate_config(&config);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Error);
        assert!(issues[0].message.contains("invalid regex"));
        assert!(has_errors(&issues));
    }

    #[test]
    fn duplicate_mode_names_produce_warning() {
        let config = make_config(vec![make_mode("dev", vec![]), make_mode("dev", vec![])]);
        let issues = validate_config(&config);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Warning);
        assert!(issues[0].message.contains("duplicate"));
        assert!(!has_errors(&issues));
    }

    #[test]
    fn interval_without_watch_produces_warning() {
        let config = make_config(vec![make_mode(
            "dev",
            vec![make_rule("cargo\\s+test", false, Some(5))],
        )]);
        let issues = validate_config(&config);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Warning);
        assert!(issues[0].message.contains("interval will be ignored"));
    }

    #[test]
    fn watch_with_interval_is_valid() {
        let config = make_config(vec![make_mode(
            "dev",
            vec![make_rule("kubectl\\s+get", true, Some(2))],
        )]);
        let issues = validate_config(&config);
        assert!(issues.is_empty());
    }

    #[test]
    fn multiple_issues_across_modes() {
        let config = make_config(vec![
            make_mode("a", vec![make_rule("[bad", false, None)]),
            make_mode("a", vec![make_rule("good", false, Some(3))]),
        ]);
        let issues = validate_config(&config);
        // 1 duplicate name + 1 invalid regex + 1 interval without watch
        assert_eq!(issues.len(), 3);
        assert!(has_errors(&issues));
    }

    #[test]
    fn display_format() {
        let issue = ValidationIssue {
            severity: Severity::Error,
            scope: "dev".to_string(),
            message: "bad regex".to_string(),
        };
        let s = format!("{issue}");
        assert_eq!(s, "[error] 'dev': bad regex");
    }

    #[test]
    fn rules_with_zero_reactive_panes_produces_error() {
        let config = make_config(vec![ModeConfig {
            agent: None,
            name: "dev".to_string(),
            init_command: None,
            seed_prompt: None,
            panes: vec![],
            rules: vec![make_rule("cargo\\s+test", false, None)],
            reactive_panes: 0,
        }]);
        let issues = validate_config(&config);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Error);
        assert!(issues[0].message.contains("reactive pane"));
        assert!(has_errors(&issues));
    }

    #[test]
    fn empty_config_is_valid() {
        let config = make_config(vec![]);
        let issues = validate_config(&config);
        assert!(issues.is_empty());
    }

    #[test]
    fn empty_mode_is_valid() {
        let config = make_config(vec![ModeConfig {
            agent: None,
            name: "empty".to_string(),
            init_command: None,
            seed_prompt: None,
            panes: vec![],
            rules: vec![],
            reactive_panes: 2,
        }]);
        let issues = validate_config(&config);
        assert!(issues.is_empty());
    }

    // --- Orchestration validation tests ---

    #[test]
    fn valid_orchestration_has_no_issues() {
        let config = make_orch_config(vec![make_orchestration(
            "tdd",
            vec![make_role("tester", true), make_role("coder", false)],
        )]);
        let issues = validate_config(&config);
        assert!(issues.is_empty());
    }

    #[test]
    fn orchestration_fewer_than_two_roles_is_error() {
        let config = make_orch_config(vec![make_orchestration(
            "solo",
            vec![make_role("only", true)],
        )]);
        let issues = validate_config(&config);
        assert!(
            issues
                .iter()
                .any(|i| i.severity == Severity::Error && i.message.contains("at least 2 roles"))
        );
    }

    #[test]
    fn orchestration_no_start_role_is_error() {
        let config = make_orch_config(vec![make_orchestration(
            "nostart",
            vec![make_role("a", false), make_role("b", false)],
        )]);
        let issues = validate_config(&config);
        assert!(
            issues
                .iter()
                .any(|i| i.severity == Severity::Error && i.message.contains("start = true"))
        );
    }

    #[test]
    fn orchestration_multiple_start_roles_is_error() {
        let config = make_orch_config(vec![make_orchestration(
            "multistart",
            vec![make_role("a", true), make_role("b", true)],
        )]);
        let issues = validate_config(&config);
        assert!(
            issues
                .iter()
                .any(|i| i.severity == Severity::Error && i.message.contains("start = true"))
        );
    }

    #[test]
    fn orchestration_duplicate_role_names_is_error() {
        let config = make_orch_config(vec![make_orchestration(
            "duproles",
            vec![make_role("worker", true), make_role("worker", false)],
        )]);
        let issues = validate_config(&config);
        assert!(issues
            .iter()
            .any(|i| i.severity == Severity::Error && i.message.contains("duplicate role name")));
    }

    #[test]
    fn orchestration_duplicate_names_produce_warning() {
        let config = make_orch_config(vec![
            make_orchestration("dup", vec![make_role("a", true), make_role("b", false)]),
            make_orchestration("dup", vec![make_role("c", true), make_role("d", false)]),
        ]);
        let issues = validate_config(&config);
        assert!(issues.iter().any(|i| i.severity == Severity::Warning
            && i.message.contains("duplicate orchestration name")));
    }

    #[test]
    fn orchestration_worker_without_description_warns() {
        let config = make_orch_config(vec![make_orchestration(
            "test",
            vec![
                make_role("orchestrator", true),
                OrchestrationRoleConfig {
                    agent: None,
                    name: "worker".to_string(),
                    command: "claude".to_string(),
                    start: false,
                    description: None,
                    prompt_template: None,
                    clear: true,
                },
            ],
        )]);
        let issues = validate_config(&config);
        assert!(
            issues
                .iter()
                .any(|i| i.severity == Severity::Warning && i.message.contains("no description"))
        );
    }

    #[test]
    fn orchestration_role_name_with_path_traversal_is_error() {
        let config = make_orch_config(vec![make_orchestration(
            "test",
            vec![
                make_role("orchestrator", true),
                OrchestrationRoleConfig {
                    agent: None,
                    name: "../evil".to_string(),
                    command: "claude".to_string(),
                    start: false,
                    description: Some("malicious".to_string()),
                    prompt_template: None,
                    clear: true,
                },
            ],
        )]);
        let issues = validate_config(&config);
        assert!(
            issues
                .iter()
                .any(|i| i.severity == Severity::Error && i.message.contains("unsafe path"))
        );
    }

    #[test]
    fn orchestration_role_name_with_slash_is_error() {
        let config = make_orch_config(vec![make_orchestration(
            "test",
            vec![
                make_role("orchestrator", true),
                OrchestrationRoleConfig {
                    agent: None,
                    name: "sub/dir".to_string(),
                    command: "claude".to_string(),
                    start: false,
                    description: Some("slashy".to_string()),
                    prompt_template: None,
                    clear: true,
                },
            ],
        )]);
        let issues = validate_config(&config);
        assert!(
            issues
                .iter()
                .any(|i| i.severity == Severity::Error && i.message.contains("unsafe path"))
        );
    }

    #[test]
    fn orchestration_role_name_with_backslash_is_error() {
        let config = make_orch_config(vec![make_orchestration(
            "test",
            vec![
                make_role("orchestrator", true),
                OrchestrationRoleConfig {
                    agent: None,
                    name: "sub\\dir".to_string(),
                    command: "claude".to_string(),
                    start: false,
                    description: Some("backslash".to_string()),
                    prompt_template: None,
                    clear: true,
                },
            ],
        )]);
        let issues = validate_config(&config);
        assert!(
            issues
                .iter()
                .any(|i| i.severity == Severity::Error && i.message.contains("unsafe path"))
        );
    }

    #[test]
    fn sanitize_role_name_removes_traversal() {
        assert_eq!(sanitize_role_name("../evil"), "evil");
        assert_eq!(sanitize_role_name("sub/dir"), "subdir");
        assert_eq!(sanitize_role_name("sub\\dir"), "subdir");
        assert_eq!(sanitize_role_name("normal-name"), "normal-name");
        assert_eq!(sanitize_role_name("../../etc/passwd"), "etcpasswd");
        assert_eq!(sanitize_role_name("safe_name"), "safe_name");
    }

    #[test]
    fn sanitize_role_name_slash_removal_cannot_create_dotdot() {
        // Slash between dots: removing the slash must not leave ".."
        assert_eq!(sanitize_role_name("./."), "");
        assert_eq!(sanitize_role_name(".\\."), "");
        assert_eq!(sanitize_role_name("./../."), "");
        assert_eq!(sanitize_role_name("a./.b"), "ab");
    }

    /// Issue #308: a declared agent name no shipped agent claims is warned
    /// about — on a role and on a mode — while a recognized name, a blank value
    /// (which reads as unset) and an absent key are all silent.
    #[test]
    fn unknown_declared_agent_warns_on_roles_and_modes() {
        let mut role = make_role("worker", false);
        role.agent = Some("codx".to_string());
        let mut start = make_role("orchestrator", true);
        start.agent = Some("codex".to_string());
        let mut mode = make_mode("declared", vec![]);
        mode.agent = Some("nonsense".to_string());
        let mut blank = make_mode("blank", vec![]);
        blank.agent = Some("  ".to_string());

        let config = ProjectConfig {
            modes: vec![mode, blank, make_mode("plain", vec![])],
            orchestrations: vec![OrchestrationConfig {
                name: "orch".to_string(),
                roles: vec![start, role],
            }],
            worker_response_timeout_minutes:
                crate::project_config::DEFAULT_WORKER_RESPONSE_TIMEOUT_MINUTES,
        };

        let warned: Vec<String> = validate_config(&config)
            .into_iter()
            .filter(|i| i.message.contains("unknown agent"))
            .map(|i| format!("{}|{}", i.scope, i.message))
            .collect();

        assert_eq!(
            warned.len(),
            2,
            "exactly the two unrecognized declarations warn; got {warned:?}"
        );
        assert!(
            warned
                .iter()
                .any(|w| w.starts_with("declared|unknown agent 'nonsense'")),
            "the mode declaration warns under the mode's name; got {warned:?}"
        );
        assert!(
            warned
                .iter()
                .any(|w| w.starts_with("orch|role 'worker': unknown agent 'codx'")),
            "the role declaration warns under the orchestration, naming the role; got {warned:?}"
        );
        assert!(
            warned.iter().all(|w| w.contains("codex")),
            "the warning must list what the user could have written; got {warned:?}"
        );
        assert!(
            !has_errors(&validate_config(&config)),
            "an unknown agent name is advisory — the config still loads"
        );
    }
}
