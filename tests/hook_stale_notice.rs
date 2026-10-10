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

/// Scenario: Render a stale hook notice at full width and at 60 columns, then
/// render an empty notice list. Full width names both agents, the binary and
/// versions; a narrow row keeps the remedy, and an empty list draws nothing.
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
    assert!(
        narrow.contains("Run: brew upgrade dot-agent-deck"),
        "remedy clipped: {narrow}"
    );
    insta::assert_snapshot!("hook_notice_narrow", narrow);
    assert_eq!(text(&[], 60), "", "empty notices must draw no row");
}
