//! L1 rendering of the persistent stale hook binary notice.
use dot_agent_deck::hook_binary::{
    HookBinaryNotice, HookBinaryReason, REMEDY_UPGRADE_OR_REINSTALL,
};
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
/// middle; at 60 the row keeps the agents and both versions and shows no
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
    for part in ["Claude Code", "Codex", "0.45.1", "0.46.0"] {
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
/// row shows the agents and both versions with the whole upgrade-or-reinstall
/// advice instead of a clipped `Cl…` lead before the command; at 300 columns
/// it shows the full sentence and the command.
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
        REMEDY_UPGRADE_OR_REINSTALL,
    ] {
        assert!(row.contains(part), "missing {part:?}: {row}");
    }
    assert!(!row.contains("hooks install --agent"), "{row}");
    assert!(!row.contains("⚠ Cl…"), "{row}");
    insta::assert_snapshot!("hook_notice_command_without_room_for_facts", row);
    let wide = text(std::slice::from_ref(&notice), 300);
    assert!(wide.contains(&format!("Run: {command}")), "{wide}");
    assert!(
        wide.contains("Claude Code, Codex hooks run dot-agent-deck 0.45.1"),
        "{wide}"
    );
}

fn homebrew_notice() -> HookBinaryNotice {
    HookBinaryNotice {
        binary: "/opt/homebrew/bin/dot-agent-deck".into(),
        agents: vec!["Claude Code".into(), "Codex".into()],
        version: Some("0.45.1".into()),
        daemon_version: "0.46.0".into(),
        reason: HookBinaryReason::Older,
        remedy: "Run:".into(),
        command: Some("brew upgrade dot-agent-deck".into()),
    }
}

/// Scenario (Qodo on #1656): Render the two-agent Homebrew notice at 80 and
/// at 100 columns, where the sentence naming the agents and both versions does
/// not fit beside the command. Each row shows `Run: brew upgrade
/// dot-agent-deck` whole and both versions, with the facts shortened instead,
/// and no cut-off upgrade-or-reinstall advice.
#[spec("hooks/stale/006")]
#[test]
fn stale_006_a_short_command_stays_whole_on_a_narrow_row() {
    let notice = homebrew_notice();
    for width in [80u16, 100] {
        let row = text(std::slice::from_ref(&notice), width);
        assert!(
            unicode_width::UnicodeWidthStr::width(row.as_str()) <= width as usize,
            "{width}: {row}"
        );
        for part in [
            "Claude Code",
            "0.45.1",
            "0.46.0",
            "Run: brew upgrade dot-agent-deck",
        ] {
            assert!(row.contains(part), "{width}: missing {part:?}: {row}");
        }
        assert!(!row.contains("Upgrade the"), "{width}: {row}");
        insta::assert_snapshot!(format!("hook_notice_homebrew_{width}"), row);
    }
}

/// Scenario (Qodo on #1656): Render two stale notices at 80 columns. The row
/// draws the first notice and ends with `(+1 more)`, which is never cut to
/// make room for the notice or its remedy.
#[spec("hooks/stale/007")]
#[test]
fn stale_007_the_count_of_further_notices_survives_a_narrow_row() {
    let notices = [homebrew_notice(), homebrew_notice()];
    for width in [40u16, 80, 180] {
        let row = text(&notices, width);
        assert!(row.ends_with(" (+1 more)"), "{width}: {row}");
    }
    let row = text(&notices, 80);
    for part in ["Claude Code", "0.45.1", "0.46.0"] {
        assert!(row.contains(part), "missing {part:?}: {row}");
    }
    insta::assert_snapshot!("hook_notice_two_notices_80", text(&notices, 80));
}
