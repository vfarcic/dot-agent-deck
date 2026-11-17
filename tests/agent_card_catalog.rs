//! PRD #2374: the card catalog's merge and validation, and a report's
//! validation against it, through the contract's pure API
//! (`card_catalog::resolve`, `AgentReport::{parse_json, validate}`). Asserts on
//! the resolved catalog's JSON shape (the one `card catalog --json` prints)
//! rather than on Rust field names.

use dot_agent_deck::agent_cards::AgentReport;
use dot_agent_deck::card_catalog::{ResolvedCatalog, resolve};
use serde_json::{Value, json};
use spec::spec;

dot_agent_deck_test_harness::link!();

fn as_json(catalog: &ResolvedCatalog) -> Value {
    serde_json::to_value(catalog).expect("the resolved catalog serializes")
}

fn names(catalog: &Value, kind: &str) -> Vec<String> {
    catalog[kind]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect()
}

fn entry<'a>(catalog: &'a Value, kind: &str, name: &str) -> &'a Value {
    catalog[kind]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == name)
        .unwrap_or_else(|| panic!("{kind} {name} in {catalog:#}"))
}

fn errors(catalog: &Value) -> Vec<String> {
    catalog["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap().to_string())
        .collect()
}

fn refusal(report: Value, catalog: &ResolvedCatalog) -> (String, String) {
    let parsed = AgentReport::parse_json(&report.to_string());
    let err = match parsed {
        Err(err) => err,
        Ok(parsed) => parsed
            .validate(catalog)
            .expect_err(&format!("must be refused: {report}")),
    };
    (err.code.to_string(), err.message)
}

/// Scenario: Resolve the catalog with no user config, then with user configs
/// that replace, append, remove and get entries wrong, and validate agent
/// reports against the result. Defaults come back in the contract's order; a
/// user entry replaces a default whole or is appended; each bad entry is one
/// error naming it while the rest applies; and reports with a custom field
/// pass while mistyped, reserved or over-bound ones are refused
/// `report_invalid` naming what is wrong.
#[spec("agent-report/catalog/001")]
#[test]
fn catalog_001_merge_validation_and_report_validation() {
    // Defaults, in order.
    let defaults = as_json(&resolve(None));
    assert_eq!(
        names(&defaults, "fields"),
        [
            "progress",
            "next-step",
            "details",
            "pull-request",
            "issue",
            "links"
        ]
    );
    assert_eq!(
        names(&defaults, "actions"),
        ["go-ahead", "merge", "explain"]
    );
    assert_eq!(errors(&defaults), Vec::<String>::new());
    let progress = entry(&defaults, "fields", "progress");
    assert_eq!(progress["type"], "text");
    assert_eq!(progress["placement"], "card");
    assert_eq!(
        progress["description"],
        "What you are doing now, in a few words."
    );
    assert_eq!(progress["source"], "default");
    assert_eq!(entry(&defaults, "fields", "details")["type"], "long_text");
    assert_eq!(entry(&defaults, "fields", "details")["placement"], "detail");
    assert_eq!(
        entry(&defaults, "fields", "pull-request")["type"],
        "pull_request"
    );
    assert_eq!(
        entry(&defaults, "fields", "pull-request")["placement"],
        "compact"
    );
    assert_eq!(entry(&defaults, "fields", "links")["type"], "list");
    let merge = entry(&defaults, "actions", "merge");
    assert_eq!(merge["label"], "Merge it");
    assert_eq!(
        merge["prompt"],
        "Merge it, following this project's own merge procedure, then update your card."
    );
    assert_eq!(merge["always"], false);
    assert_eq!(entry(&defaults, "actions", "explain")["always"], true);
    assert_eq!(
        entry(&defaults, "actions", "go-ahead")["prompt"],
        "Go ahead."
    );

    // Replace whole, append in file order, remove.
    let user = as_json(&resolve(Some(
        r#"
[catalog]
remove = ["go-ahead"]

[[catalog.fields]]
name = "progress"
description = "Replaced progress."
type = "long_text"

[[catalog.fields]]
name = "risk"
description = "How risky the change is."
type = "text"
label = "Risk"
icon = "alert-triangle"

[[catalog.actions]]
name = "ship"
description = "Offer when ready to release."
prompt = "Release it."
confirm = true
"#,
    )));
    assert_eq!(errors(&user), Vec::<String>::new());
    assert_eq!(
        names(&user, "fields"),
        [
            "progress",
            "next-step",
            "details",
            "pull-request",
            "issue",
            "links",
            "risk"
        ]
    );
    let replaced = entry(&user, "fields", "progress");
    assert_eq!(replaced["type"], "long_text");
    assert_eq!(replaced["description"], "Replaced progress.");
    assert_eq!(replaced["source"], "user");
    assert_eq!(
        replaced["icon"],
        Value::Null,
        "replaced whole, not merged key by key"
    );
    assert_eq!(replaced["label"], "Progress");
    assert_eq!(names(&user, "actions"), ["merge", "explain", "ship"]);
    let ship = entry(&user, "actions", "ship");
    assert_eq!(ship["confirm"], true);
    assert_eq!(ship["label"], "Ship", "default label from the name");
    assert_eq!(ship["placement"], "card");

    // Each bad entry is one error naming it; the rest applies.
    let bad = as_json(&resolve(Some(
        r#"
[catalog]
remove = ["no-such-default"]

[[catalog.fields]]
name = "colour"
description = "x"
type = "colour"

[[catalog.fields]]
name = "spot"
description = "x"
type = "text"
placement = "sidebar"

[[catalog.fields]]
name = "Bad_Name"
description = "x"
type = "text"

[[catalog.fields]]
name = "nodesc"
type = "text"

[[catalog.fields]]
name = "extra"
description = "x"
type = "text"
colour = "red"

[[catalog.fields]]
name = "status"
description = "x"
type = "text"

[[catalog.fields]]
name = "good"
description = "Kept."
type = "number"

[[catalog.fields]]
name = "good"
description = "Duplicate."
type = "text"

[[catalog.actions]]
name = "noprompt"
description = "x"
"#,
    )));
    let errs = errors(&bad);
    assert_eq!(errs.len(), 9, "one error per bad entry: {errs:#?}");
    for needle in [
        "no-such-default",
        "\"colour\"",
        "\"spot\"",
        "\"Bad_Name\"",
        "\"nodesc\"",
        "\"extra\"",
        "\"status\"",
        "\"good\"",
        "\"noprompt\"",
    ] {
        assert!(
            errs.iter().any(|e| e.contains(needle)),
            "an error names {needle}: {errs:#?}"
        );
    }
    assert!(
        errs.iter()
            .any(|e| e.starts_with("catalog.fields \"colour\"") && e.contains("unknown type")),
        "{errs:#?}"
    );
    assert_eq!(
        names(&bad, "fields"),
        [
            "progress",
            "next-step",
            "details",
            "pull-request",
            "issue",
            "links",
            "good"
        ],
        "bad entries skipped, the rest applied"
    );
    assert_eq!(
        entry(&bad, "fields", "good")["type"],
        "number",
        "first of a duplicate wins"
    );
    assert_eq!(names(&bad, "actions"), ["go-ahead", "merge", "explain"]);

    // A file that does not parse: defaults and one error.
    let broken = as_json(&resolve(Some("[catalog\nremove = [")));
    assert_eq!(errors(&broken).len(), 1, "{broken:#}");
    assert_eq!(names(&broken, "actions"), ["go-ahead", "merge", "explain"]);

    // Reports against the defaults.
    let catalog = resolve(None);
    let ok = AgentReport::parse_json(
        &json!({"status": "waiting", "summary": "fine",
            "fields": [{"name": "pull-request", "value": "#1"}, {"name": "my-note", "value": "x"}],
            "actions": [{"name": "merge"}, {"name": "rerun", "label": "Rerun", "prompt": "Rerun it."}]})
        .to_string(),
    )
    .expect("a valid report parses");
    ok.validate(&catalog)
        .expect("a custom field defaults to text and passes");

    for (report, needle) in [
        (
            json!({"fields": [{"name": "pull-request", "value": 7}]}),
            "pull-request",
        ),
        (
            json!({"fields": [{"name": "status", "value": "x"}]}),
            "status",
        ),
        (json!({"actions": [{"name": "close"}]}), "close"),
        (
            json!({"actions": [{"name": "merge", "prompt": "Other text."}]}),
            "merge",
        ),
        (json!({"summary": "x".repeat(301)}), "summary"),
        (json!({"summary": "two\nlines"}), "summary"),
        (json!({"colour": "red"}), "colour"),
    ] {
        let (code, message) = refusal(report.clone(), &catalog);
        assert_eq!(code, "report_invalid", "{report}: {message}");
        assert!(
            message.contains(needle),
            "{report}: the message names {needle}: {message}"
        );
    }
    let many_fields: Vec<Value> = (0..33)
        .map(|n| json!({"name": format!("f{n}"), "value": "x"}))
        .collect();
    assert_eq!(
        refusal(json!({"fields": many_fields}), &catalog).0,
        "report_invalid"
    );
    let options: Vec<Value> = (0..9)
        .map(|n| json!({"id": format!("o{n}"), "label": "x"}))
        .collect();
    assert_eq!(
        refusal(
            json!({"questions": [{"id": "q", "text": "?", "options": options}]}),
            &catalog
        )
        .0,
        "report_invalid"
    );
}
