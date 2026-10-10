//! L1 host-of-this-deck overlay tests against a hostile daemon's reply (PRD
//! #1258 audit A2): the overlay renders a bounded amount of scrubbed text
//! whatever the report holds. The report is injected straight into the render
//! seam, past the client's own size bounds (`protocol/host-metrics/007`–`009`),
//! so these prove the renderer's defence in depth on its own.

use dot_agent_deck::daemon_client::HostMetricsReport;
use dot_agent_deck::daemon_protocol::{DiskUsage, HostMetrics, MAX_DISK_ROLES, MAX_ROLE_BYTES};
use dot_agent_deck::ui::render_host_metrics_overlay_to_buffer;
use ratatui::buffer::Buffer;
use spec::spec;

fn text(buffer: &Buffer) -> String {
    let area = buffer.area();
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn report(roles: impl IntoIterator<Item = String>) -> HostMetricsReport {
    HostMetricsReport::Available(HostMetrics {
        disks: roles
            .into_iter()
            .map(|role| DiskUsage {
                role,
                free_bytes: Some(1024_u64.pow(3)),
                total_bytes: Some(2 * 1024_u64.pow(3)),
            })
            .collect(),
        load_per_cpu: Some(0.5),
        cpu_count: Some(4),
        memory_used_bytes: None,
        memory_available_bytes: None,
        sampled_at_ms: 1,
        sample_age_ms: 0,
    })
}

fn assert_no_control_or_bidi(buffer: &Buffer) {
    let area = buffer.area();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let symbol = buffer[(x, y)].symbol();
            assert!(
                !symbol.chars().any(|c| {
                    c.is_control() || dot_agent_deck::untrusted_text::is_bidi_format_char(c)
                }),
                "cell ({x}, {y}) holds {symbol:?}"
            );
        }
    }
}

/// Scenario: Render the host overlay for a report whose one role carries ESC,
/// a clear-screen CSI sequence, a newline and a right-to-left override; the
/// role row shows the scrubbed name on one line and no buffer cell holds a
/// control character or bidi override.
#[spec("dashboard/host-metrics/005")]
#[test]
fn dashboard_host_metrics_005_control_characters_in_a_role_are_scrubbed() {
    let buffer = render_host_metrics_overlay_to_buffer(
        &report(["\u{1b}[2Jro\nle\u{202e}x".to_string()]),
        80,
        24,
    );
    let rendered = text(&buffer);
    assert!(
        rendered
            .lines()
            .any(|line| line.contains("[2Jrolex") && line.contains("1 GiB free of 2 GiB total")),
        "the scrubbed role keeps its row\n{rendered}"
    );
    assert_no_control_or_bidi(&buffer);
}

/// Scenario: Render the host overlay for a report carrying 65,525 roles, the
/// count that used to overflow the overlay's height arithmetic; it renders
/// without panicking, inside the terminal, and draws at most the client's
/// role bound of rows.
#[spec("dashboard/host-metrics/006")]
#[test]
fn dashboard_host_metrics_006_an_oversized_role_count_renders_bounded() {
    let buffer = render_host_metrics_overlay_to_buffer(
        &report((0..65_525).map(|i| format!("role{i}"))),
        80,
        200,
    );
    let rendered = text(&buffer);
    let rows = rendered
        .lines()
        .filter(|line| line.contains("free of"))
        .count();
    assert_eq!(rows, MAX_DISK_ROLES, "{rendered}");
    assert!(
        rendered.contains("Sample age"),
        "the rows after the roles are still drawn\n{rendered}"
    );
}

/// Scenario: Render the host overlay for a report whose one role is 100 KiB
/// long; the row shows the role clamped to the client's role bound with a
/// trailing ellipsis, and its disk figures stay on the same row.
#[spec("dashboard/host-metrics/007")]
#[test]
fn dashboard_host_metrics_007_an_oversized_role_name_is_clamped() {
    let buffer = render_host_metrics_overlay_to_buffer(&report(["r".repeat(100 * 1024)]), 200, 24);
    let rendered = text(&buffer);
    let row = rendered
        .lines()
        .find(|line| line.contains("free of"))
        .unwrap_or_else(|| panic!("missing role row\n{rendered}"));
    let shown = row.trim_start_matches([' ', '│']);
    let role: String = shown.chars().take_while(|&c| c == 'r').collect();
    assert!(
        !role.is_empty() && role.len() < MAX_ROLE_BYTES,
        "role clamped below {MAX_ROLE_BYTES} bytes: {row:?}"
    );
    assert!(row.contains('…'), "the clamp is marked: {row:?}");
    assert!(row.contains("1 GiB free of 2 GiB total"), "{row:?}");
}
