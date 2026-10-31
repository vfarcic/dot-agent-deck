//! Issue #2053: `inbox send --name` / `--pane` reach one agent where a folder
//! alone is ambiguous, through the resolver the daemon sends with
//! ([`dot_agent_deck::orchestrator_address::resolve`]) and the filter `roles`
//! previews it with ([`dot_agent_deck::roles_cli::filter`]).

use std::collections::HashMap;

use dot_agent_deck::agent_pty::{AgentRecord, TabMembership};
use dot_agent_deck::orchestrator_address::{
    AddressRefusalReason, MessageAddress, Refusal, Resolution, attribution, reply_address, resolve,
    validate,
};
use dot_agent_deck::roles_cli::filter;

fn record(pane: &str, name: &str, cwd: &str, membership: Option<TabMembership>) -> AgentRecord {
    let mut value = serde_json::json!({
        "id": format!("agent-{pane}"),
        "pane_id_env": pane,
        "display_name": name,
        "cwd": cwd,
    });
    if let Some(membership) = membership {
        value["tab_membership"] = serde_json::to_value(membership).unwrap();
    }
    serde_json::from_value(value).unwrap()
}

fn role(name: &str, cwd: &str, seat: bool) -> TabMembership {
    TabMembership::Orchestration {
        name: name.to_string(),
        role_index: usize::from(!seat),
        role_name: if seat { "orchestrator" } else { "worker" }.to_string(),
        is_start_role: seat,
        orchestration_cwd: Some(cwd.to_string()),
        display_title: None,
        orchestration_id: Some(format!("{name}-id")),
    }
}

/// The issue's shape: two standalone agents, `dispatcher` and
/// `observability`, in one folder; a team in a sibling folder; a standalone
/// `dispatcher` in a subfolder; and the caller somewhere else.
fn records() -> Vec<AgentRecord> {
    vec![
        record("p-d", "dispatcher", "/w/deck", None),
        record("p-o", "observability", "/w/deck", None),
        record("p-sub", "dispatcher", "/w/deck/sub", None),
        record(
            "p-seat",
            "orchestrator",
            "/w/team",
            Some(role("team", "/w/team", true)),
        ),
        record(
            "p-work",
            "worker",
            "/w/team",
            Some(role("team", "/w/team", false)),
        ),
        record("p-caller", "caller", "/w/elsewhere", None),
    ]
}

fn address(folder: Option<&str>, name: Option<&str>, pane: Option<&str>) -> MessageAddress {
    MessageAddress {
        folder: folder.map(str::to_string),
        name: name.map(str::to_string),
        pane: pane.map(str::to_string),
        ..MessageAddress::default()
    }
}

fn as_spelled(folder: &str) -> String {
    folder.to_string()
}

fn target(records: &[AgentRecord], address: &MessageAddress) -> Result<String, Refusal> {
    resolve(records, address, &as_spelled, "p-caller")
        .map(|r| r.target().pane_id_env.clone().unwrap())
}

fn refusal(records: &[AgentRecord], address: &MessageAddress) -> Refusal {
    target(records, address).expect_err("expected a refusal")
}

#[test]
fn a_folder_alone_is_ambiguous_and_points_at_name_and_pane() {
    let records = records();
    let refused = refusal(&records, &address(Some("/w/deck"), None, None));
    assert_eq!(refused.reason, AddressRefusalReason::Ambiguous);
    assert!(
        refused.message.contains("--name") && refused.message.contains("--pane"),
        "{}",
        refused.message
    );
    let panes: Vec<_> = refused
        .candidates
        .iter()
        .map(|c| c.pane_id.as_str())
        .collect();
    assert_eq!(panes, ["p-d", "p-o"]);
}

#[test]
fn a_name_with_a_folder_reaches_that_one_standalone_agent() {
    let records = records();
    assert_eq!(
        target(
            &records,
            &address(Some("/w/deck"), Some("dispatcher"), None)
        )
        .unwrap(),
        "p-d"
    );
    assert_eq!(
        target(
            &records,
            &address(Some("/w/deck"), Some("observability"), None)
        )
        .unwrap(),
        "p-o"
    );
    // The folder rule is the same as for a folder alone: the nearest folder
    // with a match, never a descendant.
    assert_eq!(
        target(
            &records,
            &address(Some("/w/deck/sub/deeper"), Some("dispatcher"), None)
        )
        .unwrap(),
        "p-sub"
    );
    assert_eq!(
        target(
            &records,
            &address(Some("/w/deck/sub"), Some("observability"), None)
        )
        .unwrap(),
        "p-o"
    );
}

#[test]
fn a_name_matching_several_agents_is_still_ambiguous() {
    let records = records();
    let refused = refusal(&records, &address(None, Some("dispatcher"), None));
    assert_eq!(refused.reason, AddressRefusalReason::Ambiguous);
    assert!(refused.message.contains("--pane"), "{}", refused.message);
    let panes: Vec<_> = refused
        .candidates
        .iter()
        .map(|c| c.pane_id.as_str())
        .collect();
    assert_eq!(panes, ["p-d", "p-sub"]);
    // A name matched by one agent alone needs no folder.
    assert_eq!(
        target(&records, &address(None, Some("observability"), None)).unwrap(),
        "p-o"
    );
}

#[test]
fn a_name_never_reaches_an_orchestration_member() {
    let records = records();
    for name in ["orchestrator", "worker"] {
        let refused = refusal(&records, &address(None, Some(name), None));
        assert_eq!(refused.reason, AddressRefusalReason::NoMatch, "{name}");
        assert!(refused.candidates.is_empty());
    }
    let refused = refusal(&records, &address(Some("/w/deck"), Some("nobody"), None));
    assert_eq!(refused.reason, AddressRefusalReason::NoMatch);
    assert!(refused.message.contains("nobody"), "{}", refused.message);
}

#[test]
fn a_pane_reaches_a_standalone_agent_or_an_orchestrator_and_never_a_worker() {
    let records = records();
    assert_eq!(
        target(&records, &address(None, None, Some("p-o"))).unwrap(),
        "p-o"
    );
    match resolve(
        &records,
        &address(None, None, Some("p-seat")),
        &as_spelled,
        "p-caller",
    ) {
        Ok(Resolution::Orchestrator { orchestration, .. }) => {
            assert_eq!(orchestration.name, "team")
        }
        other => panic!("expected the team's orchestrator, got {other:?}"),
    }
    let refused = refusal(&records, &address(None, None, Some("p-work")));
    assert_eq!(refused.reason, AddressRefusalReason::NoMatch);
    assert!(refused.message.contains("worker"), "{}", refused.message);
    // The caller's own pane is never a recipient.
    let refused = refusal(&records, &address(None, None, Some("p-caller")));
    assert_eq!(refused.reason, AddressRefusalReason::NoMatch);
}

#[test]
fn a_pane_intersects_with_the_folder_the_name_and_the_orchestration() {
    let records = records();
    assert_eq!(
        target(
            &records,
            &address(Some("/w/deck"), Some("dispatcher"), Some("p-d"))
        )
        .unwrap(),
        "p-d"
    );
    for (folder, name, pane) in [
        (Some("/w/team"), None, Some("p-d")),
        (None, Some("observability"), Some("p-d")),
        (None, Some("orchestrator"), Some("p-seat")),
    ] {
        let refused = refusal(&records, &address(folder, name, pane));
        assert_eq!(
            refused.reason,
            AddressRefusalReason::NoMatch,
            "{folder:?} {name:?} {pane:?}"
        );
    }
    let named = |orchestration: &str| MessageAddress {
        pane: Some("p-seat".to_string()),
        orchestration: Some(orchestration.to_string()),
        ..MessageAddress::default()
    };
    assert_eq!(target(&records, &named("team")).unwrap(), "p-seat");
    assert_eq!(
        refusal(&records, &named("other")).reason,
        AddressRefusalReason::NoMatch
    );
    // A standalone pane is never an orchestration's orchestrator.
    let standalone = MessageAddress {
        pane: Some("p-d".to_string()),
        orchestration: Some("team".to_string()),
        ..MessageAddress::default()
    };
    assert_eq!(
        refusal(&records, &standalone).reason,
        AddressRefusalReason::NoMatch
    );
}

#[test]
fn a_member_addressing_its_own_orchestrator_by_pane_is_sent_to_work_done() {
    let records = records();
    let refused = resolve(
        &records,
        &address(None, None, Some("p-seat")),
        &as_spelled,
        "p-work",
    )
    .expect_err("a worker never messages its own orchestrator this way");
    assert_eq!(refused.reason, AddressRefusalReason::OwnOrchestration);
    assert!(refused.message.contains("work-done"), "{}", refused.message);
}

#[test]
fn malformed_or_conflicting_agent_addresses_are_invalid() {
    let invalid = |address: MessageAddress| {
        let refused = validate(&address).expect_err("expected an invalid address");
        assert_eq!(refused.reason, AddressRefusalReason::InvalidAddress);
        refused.message
    };
    let message = invalid(MessageAddress {
        name: Some("dispatcher".to_string()),
        orchestration: Some("team".to_string()),
        ..MessageAddress::default()
    });
    assert!(
        message.contains("--name") && message.contains("--orchestration"),
        "{message}"
    );
    for bad in ["", "   ", "a\nb", &"x".repeat(257)] {
        invalid(address(None, Some(bad), None));
        invalid(address(None, None, Some(bad)));
    }
    assert!(validate(&MessageAddress::default()).is_err());
    assert!(validate(&address(None, Some("dispatcher"), None)).is_ok());
    assert!(validate(&address(None, None, Some("p-d"))).is_ok());
}

#[test]
fn the_agent_fields_are_absent_on_the_wire_unless_set() {
    let folder_only = address(Some("/w/deck"), None, None);
    assert_eq!(
        serde_json::to_value(&folder_only).unwrap(),
        serde_json::json!({ "folder": "/w/deck" }),
        "an address without --name or --pane keeps the pre-#2053 shape"
    );
    let named = address(Some("/w/deck"), Some("dispatcher"), Some("p-d"));
    let wire = serde_json::to_value(&named).unwrap();
    assert_eq!(wire["name"], "dispatcher");
    assert_eq!(wire["pane"], "p-d");
    assert_eq!(
        serde_json::from_value::<MessageAddress>(wire).unwrap(),
        named
    );
    assert!(named.names_an_agent() && !folder_only.names_an_agent());
}

#[test]
fn roles_lists_what_a_send_by_name_or_pane_would_reach() {
    let records = records();
    let listed = |address: MessageAddress| -> Vec<String> {
        filter(&records, &address, &HashMap::new())
            .into_iter()
            .map(|r| r.pane_id_env.clone().unwrap())
            .collect()
    };
    assert_eq!(
        listed(address(Some("/w/deck"), Some("dispatcher"), None)),
        ["p-d"]
    );
    assert_eq!(
        listed(address(None, Some("dispatcher"), None)),
        ["p-d", "p-sub"]
    );
    assert_eq!(listed(address(None, None, Some("p-o"))), ["p-o"]);
    // An orchestrator's pane lists its whole team, as --orchestration does.
    assert_eq!(
        listed(address(None, None, Some("p-seat"))),
        ["p-seat", "p-work"]
    );
    assert!(listed(address(None, None, Some("p-work"))).is_empty());
}

#[test]
fn a_reply_to_one_of_two_standalone_agents_in_a_folder_names_it() {
    let records = records();
    let caller = records
        .iter()
        .find(|r| r.pane_id_env.as_deref() == Some("p-d"))
        .unwrap();
    // From the team's orchestrator, `--folder /w/deck` matches both
    // `dispatcher` and `observability`, so the reply names the sender.
    let reply = reply_address(&records, &as_spelled, caller, "p-seat").expect("a reply address");
    assert_eq!(reply, address(Some("/w/deck"), Some("dispatcher"), None));
    let text = attribution(Some(caller), "p-d", Some(&reply));
    assert!(
        text.contains("reply with: dot-agent-deck inbox send --folder /w/deck --name dispatcher --text '<your reply>'"),
        "{text}"
    );
    // A folder alone is still preferred when it reaches the sender alone.
    let lone = records
        .iter()
        .find(|r| r.pane_id_env.as_deref() == Some("p-caller"))
        .unwrap();
    let reply = reply_address(&records, &as_spelled, lone, "p-seat").expect("a reply address");
    assert_eq!(reply, address(Some("/w/elsewhere"), None, None));
}
