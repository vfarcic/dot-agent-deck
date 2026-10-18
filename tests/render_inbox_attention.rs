//! PRD #1666 §6.4: the TUI's deck-messages picker — the rows a card lists for
//! Retry or Cancel, in the words the desktop tile uses, and the keys that press
//! them. L1: in-process `TestBackend` and `insta`, no PTY.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::event::{AgentEvent, AgentType, EventType};
use dot_agent_deck::inbox_display::AttentionKind;
use dot_agent_deck::inbox_hub::InboxAttentionItem;
use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::state::AppState;
use dot_agent_deck::ui::{
    InboxAttentionPress, inbox_attention_key_presses, inbox_attention_run,
    render_inbox_attention_overlay_to_buffer,
};
use spec::spec;
use std::collections::HashMap;

fn words() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/inbox-display.json")).unwrap()
}

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

fn item(id: &str, kind: AttentionKind, revision: u64) -> InboxAttentionItem {
    InboxAttentionItem {
        message_id: id.to_string(),
        revision,
        kind,
        from_label: "the orchestrator".to_string(),
        subject: format!("review {}", &id[id.len() - 3..]),
        task_id: None,
    }
}

const FIRST: &str = "m-k3xzq2ab-0f3c9a71d2e4b850";
const SECOND: &str = "m-k3xzq2ab-0f3c9a71d2e4b851";

/// A card for `agent-q` in `pane-q` whose daemon summary lists `items`.
fn state_with(items: &[InboxAttentionItem]) -> AppState {
    let mut state = AppState::default();
    state.register_pane("pane-q".to_string());
    state.apply_event(AgentEvent {
        session_id: "gen-q".into(),
        agent_type: AgentType::OpenCode,
        event_type: EventType::SessionStart,
        tool_name: None,
        tool_detail: None,
        cwd: Some("/home/dev/api-svc".into()),
        timestamp: chrono::DateTime::from_timestamp(1_767_225_600, 0).unwrap(),
        user_prompt: None,
        metadata: HashMap::new(),
        pane_id: Some("pane-q".into()),
        agent_id: Some("agent-q".into()),
        agent_version: None,
        schema_version: None,
        live_target: None,
        background: None,
        background_revision: None,
    });
    let summary: dot_agent_deck::inbox_hub::InboxSummary =
        serde_json::from_value(serde_json::json!({
            "queued": 0, "fetched": items.len(), "undeliverable": 0,
            "wake": "native", "component": "opencode_plugin",
            "attention": items
        }))
        .expect("the daemon's InboxSummary wire shape with attention items");
    state.apply_inbox_summary("pane-q", "agent-q", &summary);
    state
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Scenario: Open the picker over two deck messages, one uncertain and one out of attempts. Each row names its sender and subject and says what it means, in the words the desktop tile uses, and the footer offers Retry and Cancel.
#[spec("inbox/status/019")]
#[test]
fn status_019_the_picker_lists_each_message_in_the_shared_words() {
    let words = words();
    let items = [
        item(FIRST, AttentionKind::Uncertain, 4),
        item(SECOND, AttentionKind::OutOfAttempts, 9),
    ];
    let rendered = text(&render_inbox_attention_overlay_to_buffer(
        &items, 0, None, 100, 16,
    ));
    assert!(
        rendered.contains(words["attentionTitle"].as_str().unwrap()),
        "{rendered}"
    );
    assert!(
        rendered.contains("▸ the orchestrator · review 850"),
        "{rendered}"
    );
    let reason: String = words["uncertainReason"]
        .as_str()
        .unwrap()
        .chars()
        .take(60)
        .collect();
    assert!(rendered.contains(&reason), "{rendered}");
    let exhausted: String = words["outOfAttempts"]
        .as_str()
        .unwrap()
        .chars()
        .take(60)
        .collect();
    assert!(rendered.contains(&exhausted), "{rendered}");
    assert!(
        rendered.contains(words["retry"].as_str().unwrap()),
        "{rendered}"
    );
    assert!(
        rendered.contains(words["cancel"].as_str().unwrap()),
        "{rendered}"
    );
    insta::assert_snapshot!(rendered);
}

/// Scenario: After a press the picker shows the deck's answer under the rows — here that another client acted first — in the words the desktop tile uses.
#[spec("inbox/status/020")]
#[test]
fn status_020_the_picker_shows_the_decks_answer() {
    let words = words();
    let items = [item(FIRST, AttentionKind::HeldLong, 4)];
    let stale = words["actionStale"].as_str().unwrap();
    let rendered = text(&render_inbox_attention_overlay_to_buffer(
        &items,
        0,
        Some(stale),
        100,
        12,
    ));
    let head: String = stale.chars().take(60).collect();
    assert!(rendered.contains(&head), "{rendered}");
    let held: String = words["heldLong"]
        .as_str()
        .unwrap()
        .chars()
        .take(60)
        .collect();
    assert!(rendered.contains(&held), "{rendered}");
}

/// Scenario: On a card listing two deck messages, press `i` to open the picker, `r` to Retry the first, `j` to move to the second and `r` then `c`. Retry is sent for the first at its revision; the second is out of attempts, so `r` sends nothing and `c` sends its Cancel; Escape closes the picker.
#[spec("inbox/status/021")]
#[test]
fn status_021_the_picker_keys_send_retry_and_cancel_at_each_rows_revision() {
    let state = state_with(&[
        item(FIRST, AttentionKind::Uncertain, 4),
        item(SECOND, AttentionKind::OutOfAttempts, 9),
    ]);
    let (open, presses) = inbox_attention_key_presses(
        &KeybindingConfig::default(),
        &state,
        "gen-q",
        key(KeyCode::Char('i')),
        &[
            key(KeyCode::Char('r')),
            key(KeyCode::Char('j')),
            key(KeyCode::Char('r')),
            key(KeyCode::Char('c')),
        ],
    );
    assert!(open, "the picker stays open across presses");
    let press = |message_id: &str, revision, retry| InboxAttentionPress {
        pane_id: "pane-q".into(),
        agent_id: "agent-q".into(),
        message_id: message_id.into(),
        revision,
        retry,
    };
    assert_eq!(
        presses,
        vec![press(FIRST, 4, true), press(SECOND, 9, false)]
    );
    let (open, presses) = inbox_attention_key_presses(
        &KeybindingConfig::default(),
        &state,
        "gen-q",
        key(KeyCode::Char('i')),
        &[key(KeyCode::Esc), key(KeyCode::Char('r'))],
    );
    assert!(!open, "Escape closes the picker");
    assert!(presses.is_empty(), "a closed picker presses nothing");
    let (open, _) = inbox_attention_key_presses(
        &KeybindingConfig::default(),
        &state_with(&[]),
        "gen-q",
        key(KeyCode::Char('i')),
        &[],
    );
    assert!(!open, "a card with nothing to decide on opens no picker");
}

/// Scenario: A card lists nine uncertain deck messages and the terminal is only fourteen rows tall, so the picker cannot show them all. Open it with `i`, press `j` eight times to reach the last one, and look: the last message's row and the cursor are on screen, the title says which of the nine are shown, and `c` sends that message's Cancel. With an answer to show, the answer and the cursor's row are both still on screen, and so are they, with the keys, at nine and at seven rows; at five rows, the fewest that leave the picker a row, the answer is.
#[spec("inbox/status/022")]
#[test]
fn status_022_a_long_picker_keeps_the_cursor_row_and_the_answer_in_view() {
    let ids: Vec<String> = (0..9)
        .map(|n| format!("m-k3xzq2ab-0f3c9a71d2e4b8{n:02}"))
        .collect();
    let items: Vec<InboxAttentionItem> = ids
        .iter()
        .enumerate()
        .map(|(n, id)| item(id, AttentionKind::Uncertain, 10 + n as u64))
        .collect();
    let state = state_with(&items);
    let mut keys = vec![key(KeyCode::Char('j')); 8];
    let run = inbox_attention_run(
        &KeybindingConfig::default(),
        &state,
        "gen-q",
        key(KeyCode::Char('i')),
        &keys,
        (100, 14),
    );
    assert!(run.open);
    let rendered = text(run.screen.as_ref().expect("the picker is drawn"));
    let last = &ids[8];
    assert!(
        rendered.contains(&format!(
            "▸ the orchestrator · review {}",
            &last[last.len() - 3..]
        )),
        "the cursor's row is in view:\n{rendered}"
    );
    assert!(
        rendered.contains("of 9"),
        "the title says what is shown:\n{rendered}"
    );
    keys.push(key(KeyCode::Char('c')));
    let run = inbox_attention_run(
        &KeybindingConfig::default(),
        &state,
        "gen-q",
        key(KeyCode::Char('i')),
        &keys,
        (100, 14),
    );
    assert_eq!(
        run.presses,
        vec![InboxAttentionPress {
            pane_id: "pane-q".into(),
            agent_id: "agent-q".into(),
            message_id: last.clone(),
            revision: 18,
            retry: false,
        }],
        "the press acts on the row on screen"
    );
    let words = words();
    let stale = words["actionStale"].as_str().unwrap();
    let rendered = text(&render_inbox_attention_overlay_to_buffer(
        &items,
        8,
        Some(stale),
        100,
        14,
    ));
    let head: String = stale.chars().take(40).collect();
    assert!(
        rendered.contains(&head),
        "the answer is in view:\n{rendered}"
    );
    assert!(
        rendered.contains(&format!(
            "▸ the orchestrator · review {}",
            &last[last.len() - 3..]
        )),
        "and so is the cursor's row:\n{rendered}"
    );
    // A short split terminal. The TUI declares no minimum height; five rows
    // is the smallest at which the picker has a row inside its border, and
    // there the answer takes it.
    for (height, cursor_and_keys) in [(9u16, true), (7, true), (5, false)] {
        let rendered = text(&render_inbox_attention_overlay_to_buffer(
            &items,
            8,
            Some(stale),
            100,
            height,
        ));
        assert!(
            rendered.contains(&head),
            "{height} rows: the answer is in view:\n{rendered}"
        );
        if cursor_and_keys {
            assert!(
                rendered.contains(&format!(
                    "▸ the orchestrator · review {}",
                    &last[last.len() - 3..]
                )),
                "{height} rows: and so is the cursor's row:\n{rendered}"
            );
            assert!(
                rendered.contains("[Cancel c]"),
                "{height} rows: and the keys:\n{rendered}"
            );
        }
    }
}

/// Scenario: Open the picker over a deck message a Pi agent may not have received. Its row warns that Retry may make the agent get it twice, `r` sends Retry and `c` sends Cancel at the revision the row showed, and when a turn started before the press reached the deck, the picker shows the deck's answer that the message no longer needs a decision.
#[spec("inbox/status/024")]
#[test]
fn status_024_the_picker_offers_retry_and_cancel_on_a_message_that_may_not_have_reached_the_agent()
{
    let words = words();
    let items = [item(FIRST, AttentionKind::NotStarted, 4)];
    let rendered = text(&render_inbox_attention_overlay_to_buffer(
        &items, 0, None, 100, 16,
    ));
    let reason: String = words["notStartedReason"]
        .as_str()
        .unwrap()
        .chars()
        .take(60)
        .collect();
    assert!(rendered.contains(&reason), "{rendered}");
    for said in [
        "Retry sends it again, so the agent may get it",
        "twice. Cancel stops",
    ] {
        assert!(
            rendered.contains(said),
            "the duplicate is said before the press, in full:\n{rendered}"
        );
    }
    for button in ["retry", "cancel"] {
        assert!(
            rendered.contains(words[button].as_str().unwrap()),
            "{rendered}"
        );
    }
    insta::assert_snapshot!(rendered);

    let state = state_with(&items);
    let (open, presses) = inbox_attention_key_presses(
        &KeybindingConfig::default(),
        &state,
        "gen-q",
        key(KeyCode::Char('i')),
        &[key(KeyCode::Char('r')), key(KeyCode::Char('c'))],
    );
    assert!(open);
    let press = |retry| InboxAttentionPress {
        pane_id: "pane-q".into(),
        agent_id: "agent-q".into(),
        message_id: FIRST.into(),
        revision: 4,
        retry,
    };
    assert_eq!(presses, vec![press(true), press(false)]);

    // The deck refuses a Retry whose row a started turn withdrew
    // (inbox/attempts/021); the picker says so in the shared words.
    let answer = dot_agent_deck::inbox_display::action_text(true, "not_applicable");
    assert_eq!(answer, words["actionGone"].as_str().unwrap());
    let rendered = text(&render_inbox_attention_overlay_to_buffer(
        &items,
        0,
        Some(answer),
        100,
        16,
    ));
    let head: String = answer.chars().take(40).collect();
    assert!(rendered.contains(&head), "{rendered}");
}
