//! L1 host-of-this-deck overlay tests: production key handling and rendering,
//! with daemon responses injected so host load cannot change the snapshots.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::daemon_client::HostMetricsReport;
use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::ui::{
    render_help_overlay_with_bindings_to_buffer, render_host_metrics_key_sequence_to_buffers,
    render_host_metrics_overlay_to_buffer,
};
use ratatui::buffer::Buffer;
use serde_json::json;
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

fn sample() -> HostMetricsReport {
    let gib = 1024_u64.pow(3);
    HostMetricsReport::Available(
        serde_json::from_value(json!({
            "disks": [
                {"role": "working_root", "free_bytes": 128 * gib, "total_bytes": 512 * gib},
                {"role": "worktree_parent", "free_bytes": 64 * gib, "total_bytes": 256 * gib},
                {"role": "temp_root", "free_bytes": null, "total_bytes": 16 * gib}
            ],
            "load_per_cpu": 0.75,
            "cpu_count": 8,
            "memory_used_bytes": 12 * gib,
            "memory_available_bytes": 20 * gib,
            "sampled_at_ms": 1_700_000_000_000_u64,
            "sample_age_ms": 1500
        }))
        .expect("fixed daemon HostMetrics sample"),
    )
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn assert_line_has(rendered: &str, label: &str, values: &[&str]) {
    let line = rendered
        .lines()
        .find(|line| line.contains(label))
        .unwrap_or_else(|| panic!("missing {label:?} row\n{rendered}"));
    for value in values {
        assert!(
            line.contains(value),
            "{label:?} must show {value:?}\n{rendered}"
        );
    }
}

/// Scenario: Render the dashboard, press the default `m` host-metrics key, and then press Escape through the production key handlers. The host overlay must appear over the dashboard and disappear again without opening another dialog.
#[spec("dashboard/host-metrics/001")]
#[test]
fn dashboard_host_metrics_001_default_key_opens_and_escape_closes() {
    let frames = render_host_metrics_key_sequence_to_buffers(
        &KeybindingConfig::default(),
        &sample(),
        &[key(KeyCode::Char('m')), key(KeyCode::Esc)],
        100,
        32,
    );
    assert_eq!(frames.len(), 3, "initial frame plus one per key");
    assert!(!text(&frames[0]).contains("Host of this deck"));
    assert!(
        text(&frames[1]).contains("Host of this deck"),
        "{}",
        text(&frames[1])
    );
    assert!(
        text(&frames[1]).contains("Working root"),
        "{}",
        text(&frames[1])
    );
    let closed = text(&frames[2]);
    assert!(!closed.contains("Host of this deck"), "{closed}");
    assert!(!closed.contains("Sample age"), "{closed}");
    assert_eq!(text(&frames[0]), closed, "Escape restores the dashboard");
}

/// Scenario: Render a fixed daemon sample into a TestBackend and snapshot the host title, three disk roles, load per core, core count, memory and sample age. An unreadable disk field and a second sample with unreadable load, cores, memory and disks must show explicit unknown markers rather than invented zero readings.
#[spec("dashboard/host-metrics/002")]
#[test]
fn dashboard_host_metrics_002_overlay_content_and_unknown_fields() {
    let rendered = text(&render_host_metrics_overlay_to_buffer(&sample(), 100, 32));
    assert!(rendered.contains("Host of this deck"), "{rendered}");
    assert_line_has(
        &rendered,
        "Working root",
        &["128", "512", "GiB", "free", "total"],
    );
    assert_line_has(
        &rendered,
        "Worktree parent",
        &["64", "256", "GiB", "free", "total"],
    );
    assert_line_has(
        &rendered,
        "Temp root",
        &["unknown", "16", "GiB", "free", "total"],
    );
    assert_line_has(&rendered, "Load per core", &["0.75", "8", "cores"]);
    assert_line_has(&rendered, "Memory used", &["12", "GiB"]);
    assert_line_has(&rendered, "Memory available", &["20", "GiB"]);
    assert_line_has(&rendered, "Sample age", &["1500", "ms"]);
    assert!(
        !regex::Regex::new(r"\b0(?:\.0+)? GiB\b")
            .unwrap()
            .is_match(&rendered),
        "absence must not become zero\n{rendered}"
    );
    insta::assert_snapshot!(rendered);

    let unknown = HostMetricsReport::Available(
        serde_json::from_value(json!({
            "disks": [
                {"role": "working_root"}, {"role": "worktree_parent"}, {"role": "temp_root"}
            ],
            "sampled_at_ms": 1_700_000_000_000_u64,
            "sample_age_ms": 1500
        }))
        .expect("absent fields are valid daemon metrics"),
    );
    let rendered = text(&render_host_metrics_overlay_to_buffer(&unknown, 100, 32));
    for label in [
        "Working root",
        "Worktree parent",
        "Temp root",
        "Load per core",
        "Memory used",
        "Memory available",
    ] {
        assert_line_has(&rendered, label, &["unknown"]);
    }
    let load = rendered
        .lines()
        .find(|line| line.contains("Load per core"))
        .unwrap();
    assert!(
        load.matches("unknown").count() >= 2,
        "load and core count are independently unknown\n{rendered}"
    );
    assert!(!rendered.contains("0 GiB"), "{rendered}");
    assert!(!rendered.contains("0 cores"), "{rendered}");
    assert!(!rendered.contains("0.00"), "{rendered}");
}

/// Scenario: Open the overlay with a daemon result that lacks the host-metrics capability. It must say “not available from this deck” under the host title and must not fill the metric rows with zero readings.
#[spec("dashboard/host-metrics/003")]
#[test]
fn dashboard_host_metrics_003_old_deck_is_explicitly_unavailable() {
    let rendered = text(&render_host_metrics_overlay_to_buffer(
        &HostMetricsReport::NotAvailable,
        100,
        32,
    ));
    assert!(rendered.contains("Host of this deck"), "{rendered}");
    assert!(
        rendered.contains("not available from this deck"),
        "{rendered}"
    );
    for invented in ["0 GiB", "0 cores", "0.00", "Sample age: 0"] {
        assert!(
            !rendered.contains(invented),
            "unavailable deck invents {invented:?}\n{rendered}"
        );
    }
}

/// Scenario: Read a dashboard configuration remapping host_metrics from `m` to `F2`, then press the old key, the new key, and Escape through the production handlers. Only F2 opens the overlay, Escape restores the dashboard, and help advertises the configured key.
#[spec("keybindings/remap/004")]
#[test]
fn remap_004_host_metrics_uses_configured_key() {
    let (config, warnings) =
        KeybindingConfig::from_toml_str("[dashboard]\nhost_metrics = \"F2\"\n")
            .expect("host_metrics is a dashboard action");
    assert!(warnings.is_empty(), "{warnings:?}");
    let frames = render_host_metrics_key_sequence_to_buffers(
        &config,
        &sample(),
        &[
            key(KeyCode::Char('m')),
            key(KeyCode::F(2)),
            key(KeyCode::Esc),
        ],
        100,
        32,
    );
    assert_eq!(frames.len(), 4);
    assert_eq!(text(&frames[0]), text(&frames[1]), "old key is unbound");
    assert!(
        text(&frames[2]).contains("Host of this deck"),
        "{}",
        text(&frames[2])
    );
    assert_eq!(text(&frames[0]), text(&frames[3]));
    let help = text(&render_help_overlay_with_bindings_to_buffer(
        &config, 110, 60,
    ));
    let row = help
        .lines()
        .find(|line| line.to_lowercase().contains("host"))
        .unwrap_or_else(|| panic!("help must document the host overlay\n{help}"));
    assert!(
        row.contains("F2"),
        "help must show the remapped host-metrics key\n{help}"
    );
}
