//! Configuration contracts for agent-authored publications and one-taker jobs.
use dot_agent_deck::config_validation::{has_errors, validate_config};
use dot_agent_deck::project_config::ProjectConfig;
use spec::spec;

fn config(key: &str, name: &str, placement: &str) -> ProjectConfig {
    let entry = format!("{key} = [{name:?}]\n");
    toml::from_str(&format!(
        "{}[[orchestrations]]\nname = 'team'\n{}[[orchestrations.roles]]\nname = 'orchestrator'\ncommand = 'cat'\nstart = true\n[[orchestrations.roles]]\nname = 'coder'\ncommand = 'cat'\n{}",
        if placement == "dispatch" { format!("[dispatch]\n{entry}") } else { String::new() },
        if placement == "orchestration" { entry.clone() } else { String::new() },
        if placement == "role" { entry } else { String::new() },
    )).unwrap()
}

fn validate_slugs(key: &str, placements: &[&str]) {
    for placement in placements {
        for good in ["a", "release-ready", &"a".repeat(64)] {
            let issues = validate_config(&config(key, good, placement));
            assert!(
                !has_errors(&issues),
                "valid {placement}.{key}={good:?}: {issues:?}"
            );
        }
        for bad in [
            "",
            "-leading",
            "Upper",
            "with_underscore",
            "two.words",
            "../scope",
            &"a".repeat(65),
        ] {
            let issues = validate_config(&config(key, bad, placement));
            assert!(
                has_errors(&issues),
                "invalid {placement}.{key}={bad:?} must be refused, not silently ignored: {issues:?}"
            );
            assert!(
                issues
                    .iter()
                    .any(|issue| issue.message.contains(key) || issue.scope.contains(key)),
                "diagnostic must name {key}: {issues:?}"
            );
        }
    }
}

/// Scenario: Validate project files with orchestration, role and dispatch topic subscriptions. Lowercase slugs of up to 64 characters work, and malformed topic names produce an actionable validation error.
#[spec("inbox/publish/001")]
#[test]
fn publish_001_subscription_names_are_validated_at_every_config_seam() {
    validate_slugs("subscribe", &["orchestration", "role", "dispatch"]);
}

/// Scenario: Validate a worker role's configured offer pools. Valid slugs work, while empty, unsafe or oversized pool names are rejected with the take_offers setting named in the diagnostic.
#[spec("inbox/offer/001")]
#[test]
fn offer_001_pool_names_are_validated() {
    validate_slugs("take_offers", &["role"]);
}
