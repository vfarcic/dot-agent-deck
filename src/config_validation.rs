use crate::project_config::ProjectConfig;
use regex::Regex;
use std::collections::HashSet;

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

        // max_rounds must be > 0.
        if orch.max_rounds == 0 {
            issues.push(ValidationIssue {
                severity: Severity::Error,
                scope: orch.name.clone(),
                message: "max_rounds must be greater than 0".to_string(),
            });
        }
    }

    issues
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
        }
    }

    fn make_role(name: &str, start: bool) -> OrchestrationRoleConfig {
        OrchestrationRoleConfig {
            name: name.to_string(),
            command: "claude".to_string(),
            start,
            prompt_template: format!("Do {name}."),
        }
    }

    fn make_orchestration(name: &str, roles: Vec<OrchestrationRoleConfig>) -> OrchestrationConfig {
        OrchestrationConfig {
            name: name.to_string(),
            max_rounds: 3,
            auto: false,
            roles,
        }
    }

    fn make_orch_config(orchestrations: Vec<OrchestrationConfig>) -> ProjectConfig {
        ProjectConfig {
            modes: vec![],
            orchestrations,
        }
    }

    fn make_mode(name: &str, rules: Vec<ModeRule>) -> ModeConfig {
        ModeConfig {
            name: name.to_string(),
            init_command: None,
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
            name: "dev".to_string(),
            init_command: None,
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
            name: "empty".to_string(),
            init_command: None,
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
    fn orchestration_max_rounds_zero_is_error() {
        let config = make_orch_config(vec![OrchestrationConfig {
            name: "zero".to_string(),
            max_rounds: 0,
            auto: false,
            roles: vec![make_role("a", true), make_role("b", false)],
        }]);
        let issues = validate_config(&config);
        assert!(issues.iter().any(|i| i.severity == Severity::Error
            && i.message.contains("max_rounds must be greater than 0")));
    }
}
