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
    assert!(!closed.contains("Working root"), "{closed}");
    assert_eq!(text(&frames[0]), closed, "Escape restores the dashboard");
}

/// Scenario: Render a fixed daemon sample into a TestBackend and snapshot the host title, three disk roles, load per core, core count and memory. An unreadable disk field and a second sample with unreadable load, cores, memory and disks must show explicit unknown markers rather than invented zero readings.
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
    for invented in ["0 GiB", "0 cores", "0.00"] {
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

/// Scenario: Open the host overlay on a sample whose daemon judged one disk
/// critical, one a warning, one ok and one not at all, the load a warning and
/// memory ok. Each judged value is drawn in its level's terminal colour, a
/// warning or critical value is followed by its word, an ok value has none,
/// and a value with no level is drawn in the default colour.
#[spec("dashboard/host-metrics/010")]
#[test]
fn dashboard_host_metrics_010_values_are_coloured_by_their_level_with_words() {
    use ratatui::style::Color;
    let gib = 1024_u64.pow(3);
    let report = HostMetricsReport::Available(
        serde_json::from_value(json!({
            "disks": [
                {"role": "working_root", "free_bytes": 4 * gib, "total_bytes": 100 * gib, "level": "critical"},
                {"role": "worktree_parent", "free_bytes": 10 * gib, "total_bytes": 100 * gib, "level": "warn"},
                {"role": "temp_root", "free_bytes": 50 * gib, "total_bytes": 100 * gib, "level": "ok"},
                {"role": "scratch", "free_bytes": gib, "total_bytes": 2 * gib}
            ],
            "load_per_cpu": 1.0,
            "cpu_count": 4,
            "load_level": "warn",
            "memory_used_bytes": 8 * gib,
            "memory_available_bytes": 8 * gib,
            "memory_level": "ok",
            "sampled_at_ms": 1_700_000_000_000_u64,
            "sample_age_ms": 10
        }))
        .expect("a judged daemon sample"),
    );
    let buffer = render_host_metrics_overlay_to_buffer(&report, 100, 32);
    let rendered = text(&buffer);
    assert_line_has(
        &rendered,
        "Working root",
        &["4 GiB free of 100 GiB total (critical)"],
    );
    assert_line_has(
        &rendered,
        "Worktree parent",
        &["10 GiB free of 100 GiB total (warning)"],
    );
    assert_line_has(
        &rendered,
        "Load per core",
        &["1.00 across 4 cores (warning)"],
    );
    for plain in ["Temp root", "scratch", "Memory available", "Memory used"] {
        let line = rendered.lines().find(|l| l.contains(plain)).unwrap();
        assert!(!line.contains('('), "{plain} has no word: {line}");
    }

    // The colour of the first cell of each row's value.
    let value_colour = |label: &str, value: &str| -> Color {
        let area = buffer.area();
        for y in area.y..area.bottom() {
            let row: String = (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            if row.contains(label)
                && let Some(at) = row.find(value)
            {
                let x = area.x + row[..at].chars().count() as u16;
                return buffer[(x, y)].fg;
            }
        }
        panic!("no {label:?} row with {value:?}\n{rendered}");
    };
    assert_eq!(value_colour("Working root", "4 GiB"), Color::Red);
    assert_eq!(value_colour("Working root", "(critical)"), Color::Red);
    assert_eq!(value_colour("Worktree parent", "10 GiB"), Color::Yellow);
    assert_eq!(value_colour("Temp root", "50 GiB"), Color::Green);
    assert_eq!(value_colour("scratch", "1 GiB"), Color::Reset);
    assert_eq!(value_colour("Load per core", "1.00"), Color::Yellow);
    assert_eq!(value_colour("Memory available", "8 GiB"), Color::Green);
    assert_eq!(value_colour("Memory used", "8 GiB"), Color::Reset);
    // The labels stay in the default colour; only the value is judged.
    assert_eq!(value_colour("Working root", "Working root"), Color::Reset);
    insta::assert_snapshot!(rendered);
}

/// A critical disk, a warning disk, a warning load and two ok readings.
fn judged_report() -> HostMetricsReport {
    let gib = 1024_u64.pow(3);
    HostMetricsReport::Available(
        serde_json::from_value(json!({
            "disks": [
                {"role": "working_root", "free_bytes": 4 * gib, "total_bytes": 100 * gib, "level": "critical"},
                {"role": "worktree_parent", "free_bytes": 10 * gib, "total_bytes": 100 * gib, "level": "warn"},
                {"role": "temp_root", "free_bytes": 50 * gib, "total_bytes": 100 * gib, "level": "ok"}
            ],
            "load_per_cpu": 1.0,
            "cpu_count": 4,
            "load_level": "warn",
            "memory_used_bytes": 8 * gib,
            "memory_available_bytes": 8 * gib,
            "memory_level": "ok",
            "sampled_at_ms": 1_700_000_000_000_u64,
            "sample_age_ms": 10
        }))
        .expect("a judged daemon sample"),
    )
}

/// Scenario: Open the host overlay on a terminal 60 columns wide, then on one
/// 34 wide, with a critical disk, a warning disk and a warning load. A row too
/// long for the overlay puts its `(critical)` or `(warning)` on the line below
/// its figures, so the word is still shown in full on both widths, in the
/// level's colour; a row that fits keeps the word beside its figures.
#[spec("dashboard/host-metrics/011")]
#[test]
fn dashboard_host_metrics_011_level_words_survive_a_narrow_terminal() {
    use ratatui::style::Color;
    let report = judged_report();
    for width in [60, 34] {
        let buffer = render_host_metrics_overlay_to_buffer(&report, width, 32);
        let rendered = text(&buffer);
        assert_eq!(
            rendered.matches("(critical)").count(),
            1,
            "{width} wide\n{rendered}"
        );
        assert_eq!(
            rendered.matches("(warning)").count(),
            2,
            "{width} wide\n{rendered}"
        );
        assert!(
            rendered.contains("Memory available"),
            "{width} wide\n{rendered}"
        );
        let area = buffer.area();
        let colour_of = |word: &str| {
            for y in area.y..area.bottom() {
                let row: String = (area.x..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                if let Some(at) = row.find(word) {
                    let x = area.x + row[..at].chars().count() as u16;
                    return buffer[(x, y)].fg;
                }
            }
            panic!("no {word}\n{rendered}");
        };
        assert_eq!(colour_of("(critical)"), Color::Red, "{width} wide");
        assert_eq!(colour_of("(warning)"), Color::Yellow, "{width} wide");
        if width == 60 {
            let disk = rendered
                .lines()
                .position(|l| l.contains("Working root"))
                .unwrap();
            let next = rendered.lines().nth(disk + 1).unwrap();
            assert!(next.contains("(critical)"), "the word is below\n{rendered}");
            assert_line_has(&rendered, "Load per core", &["(warning)"]);
            insta::assert_snapshot!(rendered);
        }
    }
}

/// Scenario: Open the host overlay with a critical disk, a warning disk and a
/// warning load on every terminal width from 34 columns down to 1. Wherever the
/// overlay has room for the word `critical`, every non-ok reading shows its
/// whole level word in its level's colour — in parentheses when they fit too;
/// below that, it draws no reading at all rather than one in a level's colour
/// with its word cut.
#[spec("dashboard/host-metrics/012")]
#[test]
fn dashboard_host_metrics_012_level_words_survive_every_narrow_width() {
    use ratatui::style::Color;
    let report = judged_report();
    // The overlay is the terminal less two columns, its body two less again.
    const MIN_TERMINAL_WIDTH: u16 = 4 + "critical".len() as u16;
    for width in 1..=34_u16 {
        let body = usize::from(width.saturating_sub(4));
        let buffer = render_host_metrics_overlay_to_buffer(&report, width, 32);
        let rendered = text(&buffer);
        let area = buffer.area();
        let health = |c: Color| matches!(c, Color::Red | Color::Yellow | Color::Green);
        if width < MIN_TERMINAL_WIDTH {
            assert!(
                !buffer.content().iter().any(|cell| health(cell.fg)),
                "{width} wide draws a reading without room for its word\n{rendered}"
            );
            continue;
        }
        assert_eq!(
            rendered.matches("critical").count(),
            1,
            "{width} wide\n{rendered}"
        );
        assert_eq!(
            rendered.matches("warning").count(),
            2,
            "{width} wide\n{rendered}"
        );
        if body >= "(critical)".len() {
            assert!(rendered.contains("(critical)"), "{width} wide\n{rendered}");
        }
        if body >= "(warning)".len() {
            assert_eq!(
                rendered.matches("(warning)").count(),
                2,
                "{width} wide\n{rendered}"
            );
        }
        let colour_of = |word: &str| {
            for y in area.y..area.bottom() {
                let row: String = (area.x..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                if let Some(at) = row.find(word) {
                    let x = area.x + row[..at].chars().count() as u16;
                    return buffer[(x, y)].fg;
                }
            }
            panic!("no {word}\n{rendered}");
        };
        assert_eq!(colour_of("critical"), Color::Red, "{width} wide");
        assert_eq!(colour_of("warning"), Color::Yellow, "{width} wide");
    }
}
