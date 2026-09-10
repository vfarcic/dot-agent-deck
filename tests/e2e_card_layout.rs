#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Reel-eligible PTY coverage for the dashboard card width contract. The test
//! drives a genuine interactive Claude Code turn through one client while a
//! second, fixed-size client records the same live card before and after it
//! attaches the agent's pane.

mod common;

use std::cell::RefCell;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::event::{AgentType, EventType};
use spec::spec;

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";
const PANE_NAME_SUFFIX: &str = "card-layout-haiku";
const SENTINEL: &str = "card-layout-sentinel-8f3c2a.txt";
const SENTINEL_PREFIX: &str = "card-layout-sentinel-";
const RECORDING_COLS: u16 = 68;
const RECORDING_ROWS: u16 = 16;
const CONTROL_COLS: u16 = 120;
const CONTROL_ROWS: u16 = 40;

#[derive(Debug)]
struct CardSnapshot {
    rows: Vec<String>,
    border: common::BorderWeight,
}

impl CardSnapshot {
    fn bottom(&self) -> &str {
        self.rows.last().expect("card has a bottom border")
    }

    fn inner(&self) -> &[String] {
        &self.rows[1..self.rows.len() - 1]
    }

    fn height(&self) -> usize {
        self.rows.len()
    }

    fn inner_width(&self) -> usize {
        self.rows[0].chars().count().saturating_sub(2)
    }

    fn bottom_label_is_right_aligned(&self, label: &str) -> bool {
        self.bottom()
            .ends_with(&format!("{label} {}", self.border.bottom_right))
    }

    fn row_of(&self, needle: &str) -> usize {
        self.rows
            .iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("card is missing {needle:?}:\n{}", self.rows.join("\n")))
    }
}

#[derive(Debug)]
struct BorderSegment {
    start: usize,
    end: usize,
    text: String,
}

fn border_segments(row: &str, left: char, right: char) -> Vec<BorderSegment> {
    // Segment indices count Unicode scalars, so fixtures must keep every prefix
    // left of a card boundary to width-1 terminal cells.
    let chars: Vec<char> = row.chars().collect();
    chars
        .iter()
        .enumerate()
        .filter(|(_, ch)| **ch == left)
        .filter_map(|(start, _)| {
            let end = chars
                .iter()
                .enumerate()
                .skip(start + 1)
                .find_map(|(index, ch)| (*ch == right).then_some(index))?;
            Some(BorderSegment {
                start,
                end,
                text: chars[start..=end].iter().collect(),
            })
        })
        .collect()
}

/// The substring of `row` spanning exactly `start..=end`, provided the glyphs
/// AT those two columns are `left` and `right`.
///
/// Indexes the two columns directly rather than searching [`border_segments`]
/// for a matching pair. That matters when `left == right`, as it is for a card's
/// verticals: pairing each `left` with the NEXT `right` yields only CONSECUTIVE
/// pairs `(p0,p1), (p1,p2), …` and never `(p0,p2)`, so a content row carrying a
/// stray `│` between the card's own edges would fail to extract — and fail in
/// `first_card`'s panic, whose message points nowhere near the cause. Direct
/// indexing removes that class. It gives up only the incidental "no other
/// `right` in between" property, which no caller relied on, and keeps the
/// same-weight same-column coherence that is the actual contract (review of
/// #465, N4).
fn border_segment_at(
    row: &str,
    left: char,
    right: char,
    start: usize,
    end: usize,
) -> Option<String> {
    let chars: Vec<char> = row.chars().collect();
    (start < end && *chars.get(start)? == left && *chars.get(end)? == right)
        .then(|| chars[start..=end].iter().collect())
}

/// The leftmost, topmost complete same-weight card rectangle on `grid`.
///
/// Weight-agnostic by way of [`common::BORDER_WEIGHTS`] because `a861c8d`
/// promoted the SELECTED card from a plain border to a thick one and this
/// parser, pinned to `┌`/`┘`, went red on a product change that was correct —
/// issue #460. Being weight-agnostic costs nothing here: a card still has to
/// present one weight's corners at identical columns on its top row, EVERY
/// middle row and its bottom row, so a plain top with a thick bottom is still
/// rejected.
fn try_first_card(grid: &str) -> Option<CardSnapshot> {
    let lines: Vec<&str> = grid.lines().collect();
    let mut leftmost: Option<(usize, usize, CardSnapshot)> = None;
    for (top, line) in lines.iter().enumerate() {
        for border in common::BORDER_WEIGHTS {
            for top_segment in border_segments(line, border.top_left, border.top_right) {
                for (bottom, bottom_line) in lines.iter().enumerate().skip(top + 1) {
                    let Some(bottom_segment) = border_segment_at(
                        bottom_line,
                        border.bottom_left,
                        border.bottom_right,
                        top_segment.start,
                        top_segment.end,
                    ) else {
                        continue;
                    };
                    let mut rows = Vec::with_capacity(bottom - top + 1);
                    rows.push(top_segment.text.clone());
                    let middle = lines[top + 1..bottom]
                        .iter()
                        .map(|line| {
                            border_segment_at(
                                line,
                                border.vertical,
                                border.vertical,
                                top_segment.start,
                                top_segment.end,
                            )
                        })
                        .collect::<Option<Vec<_>>>();
                    let Some(middle) = middle else {
                        continue;
                    };
                    rows.extend(middle);
                    rows.push(bottom_segment);

                    let candidate = CardSnapshot { rows, border };
                    let candidate_key = (top_segment.start, top);
                    // compute_frame_layout puts dashboard cards left of panes in
                    // every layout exercised here — `ActiveTabView::Dashboard`
                    // and `Orchestration` both route through `split_cards_area`,
                    // which returns `(dashboard_area, panes_area)` in that
                    // order. `ActiveTabView::Mode` is the concrete
                    // counterexample (`src/ui.rs`: "50/50 horizontal split:
                    // agent pane left, side panes right"), so pointing this test
                    // at a Mode tab would invalidate the leftmost-complete-
                    // rectangle selection below (review of #465, N2).
                    if leftmost
                        .as_ref()
                        .is_none_or(|(start, row, _)| candidate_key < (*start, *row))
                    {
                        leftmost = Some((top_segment.start, top, candidate));
                    }
                    break;
                }
            }
        }
    }

    leftmost.map(|(_, _, card)| card)
}

fn first_card(grid: &str) -> CardSnapshot {
    try_first_card(grid).unwrap_or_else(|| panic!("grid has no complete card border:\n{grid}"))
}

fn tools_from_full_label(bottom: &str) -> usize {
    bottom
        .split_once("Tools: ")
        .and_then(|(_, rest)| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .unwrap_or_else(|| panic!("bottom border has no full Tools counter: {bottom:?}"))
}

fn assert_stats_are_border_only(card: &CardSnapshot, context: &str) {
    let inner = card.inner().join("\n");
    assert!(
        !inner.contains("Last:") && !inner.contains("Tools:"),
        "{context} card must keep Last/Tools out of content rows:\n{}",
        card.rows.join("\n")
    );
}

/// Scenario: Launch a fixed laptop-proportioned dashboard plus a second real client on the same daemon, then use that client's Ctrl+N flow to run interactive Claude Haiku and discover a uniquely named sentinel with Bash. Hold the observer's full-width real-agent card, attach its live pane without resizing the recorded PTY, and return to the narrowed dashboard so the bottom-border counters visibly degrade while every content row and both corners remain intact.
#[spec("dashboard/card-stats/005")]
#[test]
fn card_stats_005_real_agent_card_narrows_without_restructuring() {
    skip_unless!(common::check_claude_available());

    let deck = TuiDeck::builder()
        .with_pty_size(RECORDING_COLS, RECORDING_ROWS)
        .with_imported_claude_credentials()
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    let attach_socket = deck.attach_socket_path().to_string_lossy().into_owned();
    let hook_socket = deck.hook_socket_path().to_string_lossy().into_owned();
    let control = TuiDeck::builder()
        .with_pty_size(CONTROL_COLS, CONTROL_ROWS)
        .with_env("DOT_AGENT_DECK_ATTACH_SOCKET", attach_socket)
        .with_env("DOT_AGENT_DECK_SOCKET", hook_socket)
        .without_success_recording()
        .launch_with_fixture("minimal");
    control.wait_for_string("No active sessions");

    std::fs::write(control.workdir().join(SENTINEL), "reel fixture\n")
        .expect("write card-layout sentinel");
    let cwd = control.workdir().to_path_buf();
    let mut trust_paths = vec![cwd.to_string_lossy().into_owned()];
    if let Ok(canonical) = cwd.canonicalize() {
        let canonical = canonical.to_string_lossy().into_owned();
        if !trust_paths.contains(&canonical) {
            trust_paths.push(canonical);
        }
    }
    common::seed_claude_trust_in_home(deck.home_dir(), &trust_paths)
        .expect("seed Claude onboarding and project trust");

    let events = deck.subscribe_events();
    control.send_keys(b"\x0e");
    control.wait_for_string("Select Directory");
    control.send_keys(b" ");
    control.wait_for_string("New Agent");
    control.send_keys(b"\t");
    control.send_keys(PANE_NAME_SUFFIX.as_bytes());
    control.send_keys(b"\t");
    control.send_keys(format!("claude --model {HAIKU_MODEL} --allowedTools Bash").as_bytes());
    let (submit_col, submit_row) = control.wait_for_in_grid("[Submit]");
    control.click(submit_col, submit_row);

    assert!(
        control.wait_for_grid_string_within("? for shortcuts", Duration::from_secs(120)),
        "the genuine interactive Claude prompt never became ready:\n{}",
        control.snapshot_grid()
    );
    let records = common::agent_records_on(deck.attach_socket_path());
    let agent_id = records
        .iter()
        .find(|record| {
            record
                .display_name
                .as_deref()
                .is_some_and(|name| name.ends_with(PANE_NAME_SUFFIX))
        })
        .unwrap_or_else(|| {
            panic!(
                "the real Claude pane ending in {PANE_NAME_SUFFIX:?} must be registered; records={records:?}"
            )
        })
        .id
        .clone();

    let prompt = format!(
        "Use Bash exactly once to run sleep 4; ls -1. Then respond with only the exact complete filename beginning with {SENTINEL_PREFIX} that the listing revealed. Do not use any other tool."
    );
    control.send_keys(prompt.as_bytes());
    control.send_keys(b"\r");
    events.wait_for(
        |event| {
            event.agent_id.as_deref() == Some(agent_id.as_str())
                && event.agent_type == AgentType::ClaudeCode
                && event.event_type == EventType::Thinking
        },
        Duration::from_secs(120),
    );

    control.send_keys(b"\x04");
    control.wait_for_string("Dir:");
    let tool_start = events.wait_for(
        |event| {
            event.agent_id.as_deref() == Some(agent_id.as_str())
                && event.agent_type == AgentType::ClaudeCode
                && event.event_type == EventType::ToolStart
                && event.tool_name.as_deref() == Some("Bash")
        },
        Duration::from_secs(120),
    );
    assert!(
        deck.wait_for_grid_string_within("Working", Duration::from_secs(30)),
        "the real Bash call never painted a live Working badge:\n{}",
        deck.snapshot_grid()
    );

    let working_wide = first_card(&deck.snapshot_grid());
    assert!(
        working_wide.rows[0].contains("Working")
            && working_wide.inner().iter().any(|row| row.contains("Bash")),
        "the live card must show the real Working badge and Bash tool line:\n{}",
        working_wide.rows.join("\n")
    );
    tools_from_full_label(working_wide.bottom());
    assert_stats_are_border_only(&working_wide, "working wide");
    common::wait_until(Duration::from_secs(2), || false);

    assert!(
        control.wait_for_grid_string_within(SENTINEL, Duration::from_secs(120)),
        "Claude never visibly reported the full sentinel discovered by ls (the prompt contained only {SENTINEL_PREFIX:?}):\n{}",
        control.snapshot_grid()
    );
    events.wait_for(
        |event| {
            event.agent_id.as_deref() == Some(agent_id.as_str())
                && event.agent_type == AgentType::ClaudeCode
                && event.event_type == EventType::Idle
                && event.timestamp >= tool_start.timestamp
        },
        Duration::from_secs(120),
    );
    assert!(
        deck.wait_for_grid_string_within("Idle", Duration::from_secs(30)),
        "the completed real turn never painted Idle:\n{}",
        deck.snapshot_grid()
    );

    let wide = first_card(&deck.snapshot_grid());
    let tool_count = tools_from_full_label(wide.bottom());
    assert!(tool_count > 0, "wide card must retain the real tool count");
    // Successful extraction from the raw grid already requires matching-weight
    // bottom corners at the exact columns of this card's top corners.
    assert!(
        wide.bottom_label_is_right_aligned(&format!(" Tools: {tool_count}")),
        "the full stats label must be right-aligned against the wide card's bottom-right corner:\n{}",
        wide.rows.join("\n")
    );
    assert_stats_are_border_only(&wide, "idle wide");
    common::wait_until(Duration::from_secs(6), || false);

    drop(control);
    deck.send_keys(b"1");
    assert!(
        deck.wait_for_grid_string_within(SENTINEL, Duration::from_secs(30)),
        "the fixed-size recording client did not attach the completed real Claude pane with its sentinel response:\n{}",
        deck.snapshot_grid()
    );
    common::wait_until(Duration::from_secs(2), || false);
    deck.send_keys(b"\x04");
    let narrowed_snapshot = RefCell::new(None);
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(30), |grid| {
            try_first_card(grid).is_some_and(|card| {
                let complete = card.height() == wide.height()
                    && ["Dir:", "Prmt:", "Bash"]
                        .iter()
                        .all(|field| card.rows.iter().any(|row| row.contains(field)));
                let degraded = card.bottom().contains('·')
                    && !card.bottom().contains("Last:")
                    && !card.bottom().contains("Tools:");
                if complete && degraded {
                    narrowed_snapshot.replace(Some(card));
                    true
                } else {
                    false
                }
            })
        }),
        "the card never selected a shorter stats rung after its pane narrowed the dashboard:\n{}",
        deck.snapshot_grid()
    );
    let narrow = narrowed_snapshot
        .into_inner()
        .expect("the successful narrowed-card predicate stores its complete snapshot");
    // The successful try_first_card extraction is itself the raw-grid corner
    // check: both matching-weight bottom corners had to align with the top span.
    assert!(
        narrow.bottom_label_is_right_aligned(&format!(" · {tool_count} tools")),
        "narrow card must right-align the shorter stats rung and retain the same tool count:\n{}",
        narrow.rows.join("\n")
    );
    assert_stats_are_border_only(&narrow, "narrow");
    assert!(
        narrow.inner_width() < wide.inner_width(),
        "opening the pane must narrow the same card without resizing the recording PTY\nWIDE ({} inner cols):\n{}\nNARROW ({} inner cols):\n{}",
        wide.inner_width(),
        wide.rows.join("\n"),
        narrow.inner_width(),
        narrow.rows.join("\n")
    );
    assert_eq!(
        narrow.height(),
        wide.height(),
        "opening the pane must not change card height\nWIDE:\n{}\nNARROW:\n{}",
        wide.rows.join("\n"),
        narrow.rows.join("\n")
    );
    for field in ["Dir:", "Prmt:", "Bash"] {
        assert_eq!(
            narrow.row_of(field),
            wide.row_of(field),
            "{field} must remain on the same card row when the pane narrows the dashboard\nWIDE:\n{}\nNARROW:\n{}",
            wide.rows.join("\n"),
            narrow.rows.join("\n")
        );
    }
    common::wait_until(Duration::from_secs(3), || false);
}
