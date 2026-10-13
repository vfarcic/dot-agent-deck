//! PRD #1258, CLAUDE.md rule 22: the TUI's host overlay uses the desktop deck
//! card's words and formats. Both read `tests/fixtures/host-metrics-copy.json`
//! (the desktop in `desktop/src/components/HostMetricsPanel.copy.test.tsx`), so
//! a word changed in one client alone fails the other client's test.

use dot_agent_deck::daemon_client::HostMetricsReport;
use dot_agent_deck::ui::render_host_metrics_overlay_to_buffer;
use ratatui::buffer::Buffer;
use spec::spec;

fn shared() -> serde_json::Value {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/host-metrics-copy.json"
    )))
    .expect("the shared host-metrics copy parses")
}

/// The overlay's body lines, inside the border, trimmed.
fn body(buffer: &Buffer) -> Vec<String> {
    let area = buffer.area();
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .map(|line| line.trim().trim_matches(['│', ' ']).to_owned())
        .filter(|line| !line.is_empty())
        .collect()
}

fn text(value: &serde_json::Value) -> &str {
    value.as_str().expect("a string in the shared copy")
}

/// Scenario: Render the host overlay for each sample in the shared host-metrics
/// copy; the title, subtitle and every label/value row match the fixture the
/// desktop card is tested against, in order, and a deck without host metrics
/// shows the fixture's not-available sentences.
#[spec("dashboard/host-metrics/008")]
#[test]
fn dashboard_host_metrics_008_words_match_the_shared_copy() {
    let shared = shared();
    for case in shared["samples"].as_array().expect("samples") {
        let sample = serde_json::from_value(case["sample"].clone()).expect("a daemon sample");
        let lines = body(&render_host_metrics_overlay_to_buffer(
            &HostMetricsReport::Available(sample),
            80,
            24,
        ));
        assert!(
            lines.iter().any(|l| l == text(&shared["title"])),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l == text(&shared["subtitle"])),
            "{lines:#?}"
        );
        let expected: Vec<(String, String)> = case["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|row| (text(&row[0]).to_owned(), text(&row[1]).to_owned()))
            .collect();
        // A row is the label, padding, then the value; every other body line
        // (title, subtitle, the close hint) has no such split.
        let shown: Vec<(String, String)> = lines
            .iter()
            .filter_map(|line| {
                expected.iter().find_map(|(label, _)| {
                    let value = line.strip_prefix(label.as_str())?;
                    value
                        .starts_with(' ')
                        .then(|| (label.clone(), value.trim().to_owned()))
                })
            })
            .collect();
        assert_eq!(shown, expected, "{lines:#?}");
    }

    let lines = body(&render_host_metrics_overlay_to_buffer(
        &HostMetricsReport::NotAvailable,
        80,
        24,
    ));
    for sentence in shared["notAvailable"].as_array().expect("notAvailable") {
        assert!(lines.iter().any(|l| l == text(sentence)), "{lines:#?}");
    }
}
