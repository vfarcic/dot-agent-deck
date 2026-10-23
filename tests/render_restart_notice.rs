//! PRD #1666 part 2 (G-fix7d): the TUI's restart notice — the agents a deck
//! restart stopped, in a footer row of its own until the user dismisses it,
//! and the list of every one of them. L1: in-process `TestBackend` and
//! `insta`, no PTY.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::ui::{UiMode, render_restart_notice_key_sequence_to_buffers};
use spec::spec;

fn text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Scenario: On the dashboard, a deck restart has stopped two agents. The footer shows one row above the button bar saying the deck restarted and naming both, with an `[Agents a]` and a `[Dismiss c]` button; pressing `c` removes the row.
#[spec("lifecycle/handshake/013")]
#[test]
fn handshake_013_the_restart_notice_has_a_footer_row_of_its_own() {
    let frames = render_restart_notice_key_sequence_to_buffers(
        &["coder", "reviewer"],
        UiMode::Normal,
        None,
        &[key(KeyCode::Char('c'))],
        100,
        10,
    );
    let shown = text(&frames[0]);
    assert!(
        shown.contains("The deck restarted; these agents were stopped: coder, reviewer."),
        "{shown}"
    );
    insta::assert_snapshot!(shown);
    assert!(
        !text(&frames[1]).contains("The deck restarted"),
        "the dismiss key left the notice"
    );
}

/// Scenario: A deck restart stopped five agents while the user types a filter; the field and the notice share the footer without stealing typed `ac`. Help and confirmation dialogs retain keyboard ownership and leave the notice visible when both fit; Down in a drawn Quit dialog selects Stop. At forty rows Help instead draws completely over the notice, which returns when Help closes.
#[spec("lifecycle/handshake/013")]
#[test]
fn handshake_013_the_restart_notice_shows_beside_a_text_field() {
    let frames = render_restart_notice_key_sequence_to_buffers(
        &["coder", "reviewer", "tester", "docs", "release"],
        UiMode::Filter,
        None,
        &[],
        90,
        10,
    );
    let shown = text(&frames[0]);
    assert!(
        shown.contains(
            "The deck restarted; these agents were stopped: coder, reviewer, tester and 2 more."
        ),
        "{shown}"
    );
    insta::assert_snapshot!(shown);
    for (mode, field) in [(UiMode::Filter, "/"), (UiMode::Rename, "Rename:")] {
        let typed = render_restart_notice_key_sequence_to_buffers(
            &["coder", "reviewer", "tester", "docs", "release"],
            mode,
            None,
            &[key(KeyCode::Char('a')), key(KeyCode::Char('c'))],
            90,
            10,
        );
        let last = text(typed.last().unwrap());
        assert!(last.contains(&format!("{field} ac")), "{mode:?}: {last}");
        assert!(last.contains("and 2 more."), "{mode:?}: {last}");
        assert!(!last.contains("Agents stopped by"), "{mode:?}: {last}");
        assert!(last.contains("[Agents] [Dismiss]"), "{mode:?}: {last}");
        assert!(
            !last.contains("[Agents a]") && !last.contains("[Dismiss c]"),
            "{mode:?}: the field offers keys that belong to typing: {last}"
        );
    }
    for mode in [
        UiMode::Help,
        UiMode::QuitConfirm,
        UiMode::StopConfirm,
        UiMode::CloseConfirm,
    ] {
        let frames = render_restart_notice_key_sequence_to_buffers(
            &["coder", "reviewer"],
            mode,
            None,
            &[key(KeyCode::Char('a')), key(KeyCode::Char('c'))],
            120,
            if mode == UiMode::Help { 48 } else { 40 },
        );
        for frame in &frames[1..] {
            let shown = text(frame);
            assert_eq!(
                frame, &frames[0],
                "{mode:?}: notice shortcuts stole modal input"
            );
            assert!(
                shown.contains("these agents were stopped: coder, reviewer."),
                "restart notice hidden in {mode:?}:\n{shown}"
            );
            assert!(!shown.contains("│ Restart notice"), "{mode:?}: {shown}");
        }
    }
    let quit = render_restart_notice_key_sequence_to_buffers(
        &["coder", "reviewer"],
        UiMode::QuitConfirm,
        None,
        &[key(KeyCode::Down)],
        120,
        40,
    );
    let before = text(&quit[0]);
    let after = text(&quit[1]);
    assert!(
        before.contains("> Detach"),
        "Quit did not start on Detach:\n{before}"
    );
    assert!(
        after.contains("> Stop"),
        "Down in the drawn Quit dialog did not select Stop:\n{after}"
    );
    assert!(
        after.contains("these agents were stopped: coder, reviewer."),
        "Quit navigation lost the restart notice:\n{after}"
    );
    let help = render_restart_notice_key_sequence_to_buffers(
        &["coder", "reviewer"],
        UiMode::Help,
        None,
        &[key(KeyCode::Esc)],
        120,
        40,
    );
    let open = text(&help[0]);
    let without_notice =
        render_restart_notice_key_sequence_to_buffers(&[], UiMode::Help, None, &[], 120, 40);
    let help_rows = |shown: &str| {
        shown
            .lines()
            .filter_map(|line| {
                let start = line.find('│')?;
                let end = line.rfind('│')?;
                Some(line[start..end + '│'.len_utf8()].to_string())
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        help_rows(&open),
        help_rows(&text(&without_notice[0])),
        "the notice clipped Help content"
    );
    assert!(
        open.lines()
            .any(|line| line.contains("a / c") && line.contains("Restart notice: list / dismiss")),
        "Help lost its last binding: {open}"
    );
    assert!(
        open.contains("Press ? or Esc"),
        "Help lost its closing hint: {open}"
    );
    assert!(
        !open.contains("these agents were stopped:"),
        "Help did not take the full frame: {open}"
    );
    let closed = text(&help[1]);
    assert!(
        closed.contains("these agents were stopped: coder, reviewer."),
        "closing Help lost the notice: {closed}"
    );
}

/// Scenario: A deck restart stopped twenty agents, more than a sixteen-row terminal shows at once. Press `a` to open the list of every agent the notice names, then PageDown to its end: the title says which of the twenty are shown and the last agent is on screen. Escape closes the list and the footer still shows the notice.
#[spec("lifecycle/handshake/013")]
#[test]
fn handshake_013_the_restart_notice_lists_every_agent() {
    let labels: Vec<String> = (1..=20).map(|i| format!("agent-{i:02}")).collect();
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    let frames = render_restart_notice_key_sequence_to_buffers(
        &labels,
        UiMode::Normal,
        None,
        &[
            key(KeyCode::Char('a')),
            key(KeyCode::PageDown),
            key(KeyCode::PageDown),
            key(KeyCode::Esc),
        ],
        80,
        16,
    );
    let opened = text(&frames[1]);
    assert!(opened.contains("agent-01"), "{opened}");
    assert!(!opened.contains("agent-20"), "{opened}");
    let end = text(&frames[3]);
    assert!(
        end.contains("agent-20"),
        "the last agent is unreachable: {end}"
    );
    insta::assert_snapshot!(end);
    let closed = text(&frames[4]);
    assert!(
        closed.contains("The deck restarted; these agents were stopped: agent-01"),
        "closing the list dismissed the notice: {closed}"
    );
}

/// The text inside the restart list's border on every row of `shown`, with all
/// whitespace removed, so an entry wrapped over several rows reads as one.
fn list_text(shown: &str) -> String {
    let rows: Vec<Vec<char>> = shown.lines().map(|line| line.chars().collect()).collect();
    let Some((top, left)) = rows.iter().enumerate().find_map(|(y, row)| {
        let x = row.iter().position(|c| *c == '┌')?;
        row[x..]
            .iter()
            .collect::<String>()
            .starts_with("┌ Restart notice")
            .then_some((y, x))
    }) else {
        return String::new();
    };
    let right = rows[top].iter().rposition(|c| *c == '┐').unwrap();
    rows[top + 1..]
        .iter()
        .take_while(|row| row.get(left) == Some(&'│'))
        .map(|row| row[left + 1..right].iter().collect::<String>())
        .collect::<String>()
        .split_whitespace()
        .collect()
}

/// Scenario: A deck restart stopped two agents whose 80-character labels share their first 68 characters, each with a long role and orchestration. Press `a` to open the list at 80 columns and at 40: each entry wraps onto rows of its own, so both labels' differing endings, roles and orchestrations can be read in full; at 40 columns and 14 rows, scrolling down with `j` reaches the end of the last entry.
#[spec("lifecycle/handshake/013")]
#[test]
fn handshake_013_the_restart_list_wraps_long_names() {
    use dot_agent_deck::daemon_client::StoppedAgent;
    use dot_agent_deck::ui::render_restart_notice_agents_key_sequence_to_buffers;
    let prefix = "nightly-dependency-upgrade-for-the-payments-service-and-its-workers-";
    assert_eq!(prefix.len(), 68);
    let agents: Vec<StoppedAgent> = ["first-alpha", "second-omega"]
        .iter()
        .map(|suffix| {
            let label = format!("{prefix}{suffix:->12}");
            assert_eq!(label.len(), 80);
            StoppedAgent {
                label,
                role: Some("integration-reviewer-of-release-candidates".into()),
                orchestration: Some("quarterly-platform-upgrade-orchestration".into()),
            }
        })
        .collect();
    let squash = |text: &str| text.split_whitespace().collect::<String>();
    for (width, height) in [(80, 24), (40, 30)] {
        let frames = render_restart_notice_agents_key_sequence_to_buffers(
            &agents,
            UiMode::Normal,
            None,
            &[key(KeyCode::Char('a'))],
            width,
            height,
        );
        let shown = text(&frames[1]);
        let list = list_text(&shown);
        for agent in &agents {
            assert!(
                list.contains(&squash(&agent.to_string())),
                "{width} columns: {agent} cannot be read in full:\n{shown}"
            );
        }
        if width == 80 {
            insta::assert_snapshot!(shown);
        }
    }
    let mut keys = vec![key(KeyCode::Char('a'))];
    keys.extend(std::iter::repeat_n(key(KeyCode::Char('j')), 12));
    let frames = render_restart_notice_agents_key_sequence_to_buffers(
        &agents,
        UiMode::Normal,
        None,
        &keys,
        40,
        14,
    );
    let opened = list_text(&text(&frames[1]));
    assert!(
        !opened.contains("second-omega"),
        "the short list fit without scrolling"
    );
    let end = text(frames.last().unwrap());
    assert!(
        list_text(&end).contains(&squash(
            "in quarterly-platform-upgrade-orchestration) [Dismiss c]"
        )),
        "scrolling never reached the last entry's end:\n{end}"
    );
}
