//! Pure inbox status projection tests. JSON fixtures keep the TDD tree buildable
//! before AgentRecord gains its optional inbox summary.

use dot_agent_deck::agent_pty::AgentRecord;
use dot_agent_deck::daemon_status::{StatusDocument, build_status_agents, format_human};
use serde_json::{Value, json};
use spec::spec;

fn records() -> Vec<AgentRecord> {
    [
        ("native", 2, 1, 0, "native", Some("pi_extension")),
        ("turn-end", 1, 0, 1, "turn_end", Some("codex_hook")),
        ("doorbell", 0, 0, 0, "doorbell", None),
        ("unknown", 1, 0, 0, "none", None),
    ]
    .into_iter()
    .map(|(id, queued, fetched, undeliverable, wake, component)| {
        serde_json::from_value(json!({
            "id":id, "pane_id_env":id,
            "inbox":{"queued":queued,"fetched":fetched,"undeliverable":undeliverable,
                "last_acked_at":null,"wake":wake,"component":component}
        }))
        .unwrap()
    })
    .collect()
}

/// Scenario: Render the daemon status table from native, turn-end, doorbell-only and unknown-agent records. INBOX and WAKE columns must show each row's own pending counts and wake path, including an empty-inbox dash.
#[spec("inbox/status/001")]
#[test]
fn status_001_human_table_reports_inbox_counts_and_wake() {
    let table = format_human(&build_status_agents(records()));
    let mut lines = table.lines();
    let header: Vec<_> = lines.next().unwrap().split('\t').collect();
    let inbox = header
        .iter()
        .position(|column| *column == "INBOX")
        .unwrap_or_else(|| panic!("daemon status must show INBOX column: {table}"));
    let wake = header
        .iter()
        .position(|column| *column == "WAKE")
        .unwrap_or_else(|| panic!("daemon status must show WAKE column: {table}"));
    let rows: Vec<Vec<_>> = lines.map(|line| line.split('\t').collect()).collect();
    for (row, (pending, path)) in rows.iter().zip([
        ("2q 1f", "native"),
        ("1q 0f", "turn-end"),
        ("-", "doorbell"),
        ("1q 0f", "none"),
    ]) {
        assert_eq!(row[inbox], pending, "per-agent inbox count: {table}");
        assert_eq!(row[wake], path, "per-agent wake path: {table}");
    }
    assert_eq!(rows.len(), 4);
}

/// Scenario: Project the same four agent summaries into the daemon status JSON document. Schema version 2 must retain each agent's queued, fetched, undeliverable, acknowledgement and component data with the machine-readable wake spelling.
#[spec("inbox/status/002")]
#[test]
fn status_002_json_preserves_per_agent_inbox_summary() {
    let document =
        serde_json::to_value(StatusDocument::new(build_status_agents(records()))).unwrap();
    assert_eq!(document["schema_version"], 2, "inbox fields are additive");
    for (index, (queued, fetched, undeliverable, wake, component)) in [
        (2, 1, 0, "native", Some("pi_extension")),
        (1, 0, 1, "turn_end", Some("codex_hook")),
        (0, 0, 0, "doorbell", None),
        (1, 0, 0, "none", None),
    ]
    .into_iter()
    .enumerate()
    {
        let summary = &document["agents"][index]["inbox"];
        assert!(
            summary.is_object(),
            "daemon status JSON must carry inbox summary: {document}"
        );
        assert_eq!(summary["queued"], queued);
        assert_eq!(summary["fetched"], fetched);
        assert_eq!(summary["undeliverable"], undeliverable);
        assert_eq!(summary["last_acked_at"], Value::Null);
        assert_eq!(summary["wake"], wake);
        assert_eq!(summary["component"], json!(component));
    }
}
