//! L1 rendering of the persistent stale hook binary notice.
use dot_agent_deck::hook_binary::{HookBinaryNotice, HookBinaryReason};
use dot_agent_deck::ui::render_hook_notice_to_buffer;
use spec::spec;

fn text(notices: &[HookBinaryNotice], width: u16) -> String {
    let buffer = render_hook_notice_to_buffer(notices, width, 1);
    (0..width)
        .map(|x| buffer[(x, 0)].symbol())
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Scenario: Render a stale hook notice at full width, at 120 and at 60
/// columns, then render an empty notice list. Full width names both agents, the
/// binary, both versions and the command; at 120 the path is shortened from the
/// middle; at 60 the row keeps the agents and the old version and shows no
/// cut-down command; an empty list draws nothing.
#[spec("hooks/stale/001")]
#[test]
fn stale_001_notice_names_the_binary_and_preserves_the_remedy() {
    let notice = HookBinaryNotice {
        binary: "/opt/homebrew/bin/dot-agent-deck".into(),
        agents: vec!["Claude Code".into(), "Codex".into()],
        version: Some("0.45.1".into()),
        daemon_version: "0.46.0".into(),
        reason: HookBinaryReason::Older,
        remedy: "Run:".into(),
        command: Some("brew upgrade dot-agent-deck".into()),
    };
    let wide = text(std::slice::from_ref(&notice), 180);
    for part in [
        "Claude Code",
        "Codex",
        &notice.binary,
        "0.45.1",
        "0.46.0",
        "Run: brew upgrade dot-agent-deck",
    ] {
        assert!(wide.contains(part), "missing {part:?}: {wide}");
    }
    insta::assert_snapshot!("hook_notice_wide", wide);
    let middle = text(std::slice::from_ref(&notice), 120);
    assert!(
        middle.contains("(/") && middle.contains("…") && middle.contains("deck)"),
        "the path should retain both ends with a middle ellipsis: {middle}"
    );
    assert!(
        !middle.contains(&notice.binary),
        "path did not truncate: {middle}"
    );
    let narrow = text(&[notice], 60);
    assert!(narrow.contains("…"), "notice should truncate: {narrow}");
    for part in ["Claude Code", "Codex", "0.45.1"] {
        assert!(narrow.contains(part), "missing {part:?}: {narrow}");
    }
    assert!(
        !narrow.contains("Run:") && !narrow.contains("brew"),
        "a command needs the facts beside it: {narrow}"
    );
    insta::assert_snapshot!("hook_notice_narrow", narrow);
    assert_eq!(text(&[], 60), "", "empty notices must draw no row");
}

/// Scenario (Qodo on #1656): Render a two-agent stale notice whose remedy is
/// the long `hooks install` command at 160 columns, where the command alone
/// fits but not beside the sentence naming the agents and both versions. The
/// row shows that sentence and the upgrade-or-reinstall advice instead of a
/// clipped `Cl…` lead before the command; at 300 columns it shows the command.
#[spec("hooks/stale/005")]
#[test]
fn stale_005_a_command_is_shown_only_beside_the_agents_and_versions() {
    let command = "/home/u/.local/bin/dot-agent-deck hooks install --agent claude-code && \
                   /home/u/.local/bin/dot-agent-deck hooks install --agent codex";
    let notice = HookBinaryNotice {
        binary: "/opt/homebrew/bin/dot-agent-deck".into(),
        agents: vec!["Claude Code".into(), "Codex".into()],
        version: Some("0.45.1".into()),
        daemon_version: "0.46.0".into(),
        reason: HookBinaryReason::Older,
        remedy: "Run:".into(),
        command: Some(command.into()),
    };
    let width = 160;
    let run = format!("⚠  — Run: {command}");
    assert!(
        unicode_width::UnicodeWidthStr::width(run.as_str()) <= width,
        "the command alone must fit the row, or this test proves nothing"
    );
    let row = text(std::slice::from_ref(&notice), width as u16);
    for part in [
        "Claude Code, Codex",
        "0.45.1",
        "0.46.0",
        "Upgrade the dot-agent-deck",
    ] {
        assert!(row.contains(part), "missing {part:?}: {row}");
    }
    assert!(!row.contains("hooks install --agent"), "{row}");
    insta::assert_snapshot!("hook_notice_command_without_room_for_facts", row);
    let wide = text(std::slice::from_ref(&notice), 300);
    assert!(wide.contains(&format!("Run: {command}")), "{wide}");
    assert!(
        wide.contains("Claude Code, Codex hooks run dot-agent-deck 0.45.1"),
        "{wide}"
    );
}
