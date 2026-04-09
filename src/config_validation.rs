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
    pub mode_name: String,
    pub message: String,
}

impl std::fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let level = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(f, "[{}] mode '{}': {}", level, self.mode_name, self.message)
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
                mode_name: mode.name.clone(),
                message: "duplicate mode name".to_string(),
            });
        }
    }

    for mode in &config.modes {
        // Validate regex patterns.
        for rule in &mode.rules {
            if let Err(e) = Regex::new(&rule.pattern) {
                issues.push(ValidationIssue {
                    severity: Severity::Error,
                    mode_name: mode.name.clone(),
                    message: format!("invalid regex '{}': {}", rule.pattern, e),
                });
            }
        }

        // Warn if interval is set but watch is false.
        for rule in &mode.rules {
            if rule.interval.is_some() && !rule.watch {
                issues.push(ValidationIssue {
                    severity: Severity::Warning,
                    mode_name: mode.name.clone(),
                    message: format!(
                        "rule '{}' has interval but watch is false — interval will be ignored",
                        rule.pattern
                    ),
                });
            }
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
    use crate::project_config::{ModeConfig, ModeRule, ProjectConfig};

    fn make_config(modes: Vec<ModeConfig>) -> ProjectConfig {
        ProjectConfig { modes }
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
            mode_name: "dev".to_string(),
            message: "bad regex".to_string(),
        };
        let s = format!("{issue}");
        assert_eq!(s, "[error] mode 'dev': bad regex");
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
}
