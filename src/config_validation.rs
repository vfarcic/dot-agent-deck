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

/// Issue #308 audit (MEDIUM): how much of a project-controlled VALUE a
/// diagnostic quotes back before it stops and reports a count instead.
///
/// A value is not prose — it is a mode name, a role name, a regex, an agent
/// name — so 120 characters is far past the longest plausible real one while
/// leaving an absurd one unable to fill a screen. The count that replaces the
/// tail is what keeps the diagnostic honest: "this is longer than it looks" is
/// itself the finding when a config carries a 100 KB mode name.
const MAX_QUOTED_VALUE_CHARS: usize = 120;

/// Issue #308 audit (MEDIUM): the per-line ceiling on a whole rendered
/// `message`.
///
/// Deliberately an order of magnitude above [`MAX_QUOTED_VALUE_CHARS`] and
/// applied to the *message* rather than to each value inside it: a message is
/// mostly the deck's own explanatory prose (the unknown-agent warning ends with
/// the full list of shipped agent names, which is legitimately long and must not
/// be cut), so this is a flood backstop for values no producer thought to bound,
/// not a legibility budget. Anything that trips it is already pathological.
const MAX_DIAGNOSTIC_CHARS: usize = 2000;

/// Issue #308 audit (MEDIUM): the ONE place a `ValidationIssue` becomes terminal
/// output, and therefore the one place it is made safe to be terminal output.
///
/// `dot-agent-deck validate` writes each issue straight to stderr with
/// `eprintln!("{issue}")`, and `.dot-agent-deck.toml` travels with a repository
/// — a clone, a contributor branch, a PR checkout. Both fields interpolate raw
/// strings from that file: `scope` is a mode or orchestration name verbatim, and
/// several messages quote a role name, a regex pattern or (issue #308) a
/// declared `agent = "…"`. Without this, a repo shipping
/// `agent = "x\n[error] 'trusted': validation passed"` makes `validate` print a
/// line the deck never authored; an ANSI-bearing value repaints the terminal it
/// lands in; an overlong one floods it, or a CI log.
///
/// **Sanitising at the `Display` impl rather than at each producer is the
/// point**, exactly as issue #576 concluded for `session_warnings`
/// (`flush_session_warnings`, `src/ui.rs`). There are a dozen `ValidationIssue`
/// construction sites today and they only grow; per-producer escaping means
/// every future one must remember, while one seam at the single consumer covers
/// them all by construction and cannot be forgotten by a later addition. So
/// producers keep building plain, readable strings — the escaping is invisible
/// at the call site, which is why it is documented here at the seam that
/// enforces it.
///
/// Note the scope of the claim: only the diagnostic *representation* changes.
/// Whether a name is recognized is still decided by exact registry matching on
/// the raw value (see [`unknown_agent_issue`]), so nothing here can turn an
/// unknown agent into a known one — this seam runs strictly after that decision.
impl std::fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let level = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        // `scope` is a bare config value with no prose around it, so it gets the
        // tighter value bound; `message` is mostly ours and gets the flood
        // backstop. Both bound BEFORE escaping: escaping expands, so cutting
        // afterwards could sever a `\u{1b}` in half and emit `\u{1` — a bound
        // that corrupts its own output.
        write!(
            f,
            "[{}] '{}': {}",
            level,
            escape_for_terminal(&bound_chars(&self.scope, MAX_QUOTED_VALUE_CHARS)),
            escape_for_terminal(&bound_chars(&self.message, MAX_DIAGNOSTIC_CHARS)),
        )
    }
}

/// `s` cut to at most `max` CHARACTERS, with the tail replaced by the count it
/// actually had. Borrows unchanged in the ordinary case.
///
/// Characters, not bytes, so the cut can never land inside a UTF-8 sequence and
/// the reported number matches what a reader would count.
fn bound_chars(s: &str, max: usize) -> std::borrow::Cow<'_, str> {
    let total = s.chars().count();
    if total <= max {
        return std::borrow::Cow::Borrowed(s);
    }
    let head: String = s.chars().take(max).collect();
    std::borrow::Cow::Owned(format!("{head}… ({total} characters total)"))
}

/// Escape everything in `s` that a terminal would ACT on rather than show,
/// borrowing unchanged when there is nothing to do (the overwhelmingly common
/// case — a diagnostic about a well-behaved config).
///
/// Two families, because neither alone is enough:
///
/// - [`char::is_control`] — Unicode category `Cc`: C0 (U+0000..=U+001F, so ESC,
///   LF, CR and NUL), DEL (U+007F), and C1 (U+0080..=U+009F, which some
///   terminals still act on). This is the same predicate `ratatui-core` filters
///   on and the same one issue #576's exit flush escapes, so this sink is safe
///   on exactly the terms the deck's other text sinks already are.
/// - [`crate::build_version_handshake::is_bidi_format_char`] — the bidi
///   overrides and isolates, category `Cf`, which `is_control` does NOT catch
///   and which visually reorder the text around them without changing a byte.
///   Reused from the build-handshake render seam rather than respelled here, so
///   there is one definition of that set to keep correct.
///
/// Escaping rather than stripping, for issue #576's reason: this is a
/// diagnostic, and a value that silently loses characters reads as a *different*
/// value — whereas `\r` in the output preserves the evidence that something odd
/// was in the config. The spelling is Rust's own. A literal backslash is
/// deliberately not escaped: nothing parses this output back, and doubling it
/// would cost real legibility on the messages that quote a Windows path or a
/// regex.
fn escape_for_terminal(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.chars().any(needs_escape) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if needs_escape(c) => out.push_str(&format!("\\u{{{:02x}}}", c as u32)),
            other => out.push(other),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// The predicate behind [`escape_for_terminal`] — see its doc for why the set is
/// the union of two Unicode categories rather than just `Cc`.
fn needs_escape(c: char) -> bool {
    c.is_control() || crate::build_version_handshake::is_bidi_format_char(c)
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
///
/// Issue #308 audit (MEDIUM): the match is made on the RAW `name` and stays
/// exact and fail-closed — an unrecognized value is a warning and resolves to
/// `AgentType::None`, and nothing below can change that verdict. Only the
/// quoted-back copy is bounded, because the message goes to a terminal and this
/// value came out of a repository's `.dot-agent-deck.toml`. Control and bidi
/// characters in it are handled at the output seam
/// ([`ValidationIssue`]'s `Display`), which covers every field of every issue
/// rather than this one call site.
fn unknown_agent_issue(scope: &str, declared: Option<&str>) -> Option<ValidationIssue> {
    let name = declared?.trim();
    if name.is_empty() || crate::agent_registry::detect_from_basename(name).is_some() {
        return None;
    }
    let quoted = bound_chars(name, MAX_QUOTED_VALUE_CHARS);
    Some(ValidationIssue {
        severity: Severity::Warning,
        scope: scope.to_string(),
        message: format!(
            "unknown agent '{quoted}' — this pane will have no agent and no wrapper; known agents: {}",
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

    /// Issue #308 audit (MEDIUM): every control character a `.dot-agent-deck.toml`
    /// can carry into a diagnostic is escaped to printable text before
    /// `dot-agent-deck validate` writes it to a terminal. The LF case is the
    /// exploit: without escaping it forges a whole extra line that reads as the
    /// deck's own verdict.
    #[test]
    fn display_escapes_control_characters_from_config_values() {
        let issue = ValidationIssue {
            severity: Severity::Warning,
            scope: "mode\rname".to_string(),
            message: "unknown agent 'x\n[error] \'trusted\': validation passed'".to_string(),
        };
        let rendered = format!("{issue}");

        assert!(
            !rendered.contains('\n') && !rendered.contains('\r'),
            "no raw line-structure character survives to the terminal; got {rendered:?}"
        );
        assert!(
            rendered.contains("mode\\rname"),
            "CR is escaped in the scope rather than overwriting the printed line; got {rendered:?}"
        );
        assert!(
            rendered.contains("\\n[error]"),
            "the forged line is shown inert on the deck's own single line; got {rendered:?}"
        );
    }

    /// Issue #308 audit (MEDIUM): the whole escaped set, one character per case
    /// — ESC (the ANSI lead-in), NUL, DEL, a C1 control that `is_control` covers
    /// but a naive `< 0x20` test would not, and TAB. Each becomes printable
    /// text; none reaches the terminal as a byte it could act on.
    #[test]
    fn display_escapes_esc_nul_del_c1_and_tab() {
        for (raw, expected) in [
            ('\u{1b}', "\\u{1b}"), // ESC — starts every ANSI sequence
            ('\u{0}', "\\u{00}"),  // NUL
            ('\u{7f}', "\\u{7f}"), // DEL
            ('\u{9b}', "\\u{9b}"), // C1 CSI — a single-byte ANSI introducer
            ('\t', "\\t"),         // TAB, spelled the conventional way
        ] {
            let issue = ValidationIssue {
                severity: Severity::Error,
                scope: "dev".to_string(),
                message: format!("bad{raw}value"),
            };
            let rendered = format!("{issue}");
            assert!(
                rendered.contains(expected),
                "U+{:04X} must render as {expected}; got {rendered:?}",
                raw as u32
            );
            assert!(
                !rendered.contains(raw),
                "U+{:04X} must not survive as a raw character; got {rendered:?}",
                raw as u32
            );
        }
    }

    /// Issue #308 audit (MEDIUM): bidi overrides are category `Cf`, so
    /// `char::is_control` does not catch them — they are neutralised by the
    /// second half of the predicate. Left raw, a RIGHT-TO-LEFT OVERRIDE
    /// visually reorders the rest of the line without changing a byte of it.
    #[test]
    fn display_escapes_bidi_formatting_characters() {
        let issue = ValidationIssue {
            severity: Severity::Warning,
            scope: "dev".to_string(),
            message: "unknown agent '\u{202e}dessap noitadilav'".to_string(),
        };
        let rendered = format!("{issue}");

        assert!(
            rendered.contains("\\u{202e}"),
            "the RLO is shown as text; got {rendered:?}"
        );
        assert!(
            !rendered.contains('\u{202e}'),
            "no raw bidi override reaches the terminal; got {rendered:?}"
        );
    }

    /// Issue #308 audit (MEDIUM): an absurdly long declared agent name is quoted
    /// back as a short prefix plus its true length, so a hostile config cannot
    /// flood a terminal or a CI log — and the count says out loud that the value
    /// was longer than what is shown.
    #[test]
    fn overlong_declared_agent_name_is_bounded_in_the_warning() {
        let long = "z".repeat(50_000);
        let mut mode = make_mode("declared", vec![]);
        mode.agent = Some(long.clone());
        let config = make_config(vec![mode]);

        let issues = validate_config(&config);
        let warning = issues
            .iter()
            .find(|i| i.message.contains("unknown agent"))
            .expect("an unrecognized declaration warns");
        let rendered = format!("{warning}");

        assert!(
            !rendered.contains(&long),
            "the full value is never echoed back"
        );
        assert!(
            rendered.contains(&"z".repeat(MAX_QUOTED_VALUE_CHARS)),
            "the prefix that identifies the typo survives; got {rendered:?}"
        );
        assert!(
            rendered.contains("(50000 characters total)"),
            "the true length is reported instead of the tail; got {rendered:?}"
        );
        assert!(
            rendered.len() < 4_000,
            "the whole line stays terminal-sized; got {} bytes",
            rendered.len()
        );
        assert!(
            rendered.contains("known agents:"),
            "bounding the value must not truncate the deck's own advice; got {rendered:?}"
        );
    }

    /// Issue #308 audit (MEDIUM): the flood backstop covers fields no producer
    /// thought to bound — here the `scope`, which is a mode name copied verbatim
    /// out of the config with no prose around it.
    #[test]
    fn overlong_scope_is_bounded_at_the_output_seam() {
        let issue = ValidationIssue {
            severity: Severity::Error,
            scope: "s".repeat(10_000),
            message: "duplicate mode name".to_string(),
        };
        let rendered = format!("{issue}");

        assert!(
            rendered.contains("(10000 characters total)"),
            "an unbounded scope is cut and counted; got {} bytes",
            rendered.len()
        );
        assert!(
            rendered.ends_with("duplicate mode name"),
            "the message still follows the bounded scope; got {rendered:?}"
        );
    }

    /// Issue #308 audit (MEDIUM): sanitising is a display concern only. An
    /// escapable character in a declared name must not make an unknown agent
    /// look known, or a known one look unknown — the registry match is still on
    /// the raw value, exact and fail-closed.
    #[test]
    fn sanitising_does_not_change_which_names_are_recognized() {
        let mut sneaky = make_mode("sneaky", vec![]);
        // A real agent name wrapped in characters the escaper would rewrite.
        sneaky.agent = Some("\u{202e}claude\u{1b}".to_string());
        let mut plain = make_mode("plain", vec![]);
        plain.agent = Some("claude".to_string());

        let issues = validate_config(&make_config(vec![sneaky, plain]));
        let warned: Vec<&str> = issues
            .iter()
            .filter(|i| i.message.contains("unknown agent"))
            .map(|i| i.scope.as_str())
            .collect();

        assert_eq!(
            warned,
            vec!["sneaky"],
            "the decorated name stays unknown and the bare one stays known"
        );
    }
}
