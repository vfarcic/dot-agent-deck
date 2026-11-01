//! PRD #1666 part 3 (Q35-20e): the dashboard card's held-typing row, as the
//! TUI draws it from the daemon's record and its live updates. The words are
//! the shared fixture's (`tests/fixtures/inbox-display.json`), which the
//! desktop tile reads too (CLAUDE.md rule 22).

use std::collections::VecDeque;

use dot_agent_deck::agent_pty::AgentRecord;
use dot_agent_deck::event::{AgentType, InboxHandover, InputHeldUpdate};
use dot_agent_deck::inbox_display::{InputDiscarded, InputHeld, InputHeldCause};
use dot_agent_deck::state::{ActiveTool, AppState, SessionState, SessionStatus};
use dot_agent_deck::ui::{CardDensityKind, render_card_to_buffer};
use spec::spec;

const PANE: &str = "pane-held";

fn render_now() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_767_225_600, 0).expect("a valid fixed instant")
}

fn words() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/inbox-display.json")).unwrap()
}

fn buffer_to_text(buffer: &ratatui::buffer::Buffer) -> String {
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

/// A card for generation `agent` in [`PANE`], as a reported session draws it.
fn session(agent: &str) -> SessionState {
    let now = render_now();
    SessionState {
        session_id: format!("sess-{agent}"),
        agent_type: AgentType::ClaudeCode,
        cwd: Some("/home/dev/api-svc".to_string()),
        status: SessionStatus::Idle,
        blocked: None,
        active_tool: None::<ActiveTool>,
        started_at: now - chrono::Duration::hours(2),
        last_activity: now - chrono::Duration::hours(1),
        recent_events: VecDeque::new(),
        tool_count: 3,
        last_user_prompt: None,
        first_prompts: Vec::new(),
        pane_id: Some(PANE.to_string()),
        agent_id: Some(agent.to_string()),
        display_name: Some("api-svc".to_string()),
        shell_synthetic_working: false,
        native_hooks_reported: false,
        last_native_hook_at: None,
        orchestration_orphaned: false,
        subagent_wait: None,
        output_set_status: false,
        pending_question: None,
        pull_request: None,
        pull_request_revision: 0,
        incarnation: Default::default(),
    }
}

fn held(agent: &str, cause: InputHeldCause) -> InputHeld {
    InputHeld {
        generation: agent.parse().unwrap(),
        cause,
        held_ops: 2,
        held_bytes: 12,
        since_ms: 1_767_225_600_000,
    }
}

fn discarded(generation: u64) -> InputDiscarded {
    InputDiscarded {
        generation,
        held_ops: 2,
        held_bytes: 12,
        at_ms: 1_767_225_600_000,
    }
}

/// The daemon's record for generation `agent` in [`PANE`], with no live hook
/// snapshot (U3-04): the agent has sent no hook.
fn record(agent: &str, input_held: Option<InputHeld>) -> AgentRecord {
    let mut record: AgentRecord = serde_json::from_value(serde_json::json!({
        "id": agent, "pane_id_env": PANE,
        "inbox": { "queued": 0, "fetched": 0, "undeliverable": 0, "wake": "none" }
    }))
    .expect("the daemon's AgentRecord wire shape");
    record.input_held = input_held;
    record
}

/// A state managing [`PANE`] with `agent`'s card, hydrated from `record`.
fn hydrated(agent: &str, record: AgentRecord) -> AppState {
    let mut state = AppState::default();
    state.register_pane(PANE.to_string());
    let card = session(agent);
    state.sessions.insert(card.session_id.clone(), card);
    state.take_inbox_snapshot(&[record]);
    state
}

fn card(state: &AppState, agent: &str) -> SessionState {
    state.sessions[&format!("sess-{agent}")].clone()
}

fn render(session: &SessionState, width: u16) -> String {
    buffer_to_text(&render_card_to_buffer(
        session,
        Some("api-svc"),
        Some(1),
        CardDensityKind::Normal,
        0,
        render_now(),
        false,
        width,
        CardDensityKind::Normal.rendered_height(),
    ))
}

const EVERY_CAUSE: [(&str, InputHeldCause); 7] = [
    ("checking", InputHeldCause::Checking),
    ("check_not_opened", InputHeldCause::CheckNotOpened),
    ("check_not_returned", InputHeldCause::CheckNotReturned),
    ("after_your_ctrl_g", InputHeldCause::AfterYourCtrlG),
    ("queue_full", InputHeldCause::QueueFull),
    ("check_blocked", InputHeldCause::CheckBlocked),
    ("unknown", InputHeldCause::Unknown),
];

/// Scenario: Attach to a daemon whose record says the deck holds typing for an agent that has sent no hook yet, once for each cause, and draw its card at a width where the whole sentence fits. The card shows each cause's shared words; drawn narrower than the sentence, it still shows the sentence's first clause whole, cut only after it.
#[spec("inbox/probe/080")]
#[test]
fn probe_080_the_card_shows_each_held_cause_from_a_record_without_a_hook() {
    let words = words();
    for (key, cause) in EVERY_CAUSE {
        let text = words["inputHeld"][key].as_str().unwrap();
        let state = hydrated("41", record("41", Some(held("41", cause))));
        let shown = card(&state, "41");
        assert_eq!(shown.input_held_notice(), Some(text), "{key}");
        let wide = render(&shown, 160);
        assert!(wide.contains(text), "{key}:\n{wide}");
        insta::assert_snapshot!(format!("probe_080_{key}"), wide);
        let clause = text.split(';').next().unwrap();
        let narrow = render(&shown, (clause.chars().count() + 8) as u16);
        assert!(
            narrow.contains(clause),
            "{key}: first clause cut:\n{narrow}"
        );
        if clause != text {
            assert!(!narrow.contains(text), "{key}: the narrow card is narrower");
        }
        insta::assert_snapshot!(format!("probe_080_{key}_narrow"), narrow);
    }
}

/// Scenario: Hydrate a card whose typing is held, then deliver the daemon's live update that the hold ended, and another that a new hold began. The row disappears with the first update and comes back with the second, and the card's other rows are unchanged.
#[spec("inbox/probe/080")]
#[test]
fn probe_080_a_live_update_clears_and_shows_the_row() {
    let mut state = hydrated(
        "41",
        record("41", Some(held("41", InputHeldCause::Checking))),
    );
    state.apply_input_held_update(&InputHeldUpdate {
        pane_id: PANE.into(),
        agent_id: "41".into(),
        input_held: None,
        input_discarded: None,
    });
    let cleared = card(&state, "41");
    assert_eq!(cleared.input_held_notice(), None);
    insta::assert_snapshot!("probe_080_cleared", render(&cleared, 160));
    state.apply_input_held_update(&InputHeldUpdate {
        pane_id: PANE.into(),
        agent_id: "41".into(),
        input_held: Some(held("41", InputHeldCause::QueueFull)),
        input_discarded: None,
    });
    assert_eq!(
        card(&state, "41").input_held_notice(),
        words()["inputHeld"]["queue_full"].as_str()
    );
}

/// Scenario: A generation retires while typing is held and its successor takes the pane; the daemon's update for the successor says the held typing was discarded. The successor's card shows the shared not-delivered words, which hold their first clause on a narrow card, and a keystroke's update that ends the notice clears it.
#[spec("inbox/probe/080")]
#[test]
fn probe_080_the_successor_card_reports_retired_typing_not_delivered() {
    let words = words();
    let text = words["inputNotDelivered"].as_str().unwrap();
    assert!(text.starts_with(words["notDelivered"].as_str().unwrap()));
    assert!(text.contains("typing"));
    let mut state = hydrated(
        "41",
        record("41", Some(held("41", InputHeldCause::Checking))),
    );
    let successor = session("42");
    state
        .sessions
        .insert(successor.session_id.clone(), successor);
    state.apply_inbox_handover(&InboxHandover {
        pane_id: PANE.into(),
        agent_id: "42".into(),
        summary: record("42", None).inbox.unwrap(),
    });
    state.apply_input_held_update(&InputHeldUpdate {
        pane_id: PANE.into(),
        agent_id: "42".into(),
        input_held: None,
        input_discarded: Some(discarded(41)),
    });
    let shown = card(&state, "42");
    assert_eq!(shown.input_held_notice(), Some(text));
    insta::assert_snapshot!("probe_080_not_delivered", render(&shown, 160));
    let clause = text.split(';').next().unwrap();
    let narrow = render(&shown, (clause.chars().count() + 8) as u16);
    assert!(narrow.contains(clause), "{narrow}");
    insta::assert_snapshot!("probe_080_not_delivered_narrow", narrow);
    state.apply_input_held_update(&InputHeldUpdate {
        pane_id: PANE.into(),
        agent_id: "42".into(),
        input_held: None,
        input_discarded: None,
    });
    assert_eq!(card(&state, "42").input_held_notice(), None);
}

/// Scenario: Deliver held-typing updates out of order around a generation change: the predecessor's late hold after its successor took the pane, a successor's update that arrives before its handover, a hold whose generation is not the one the update names, and a not-delivered notice naming the generation it rides. Only the successor's own update shows, the early one once the handover lands; the late, mismatched and self-naming ones show nothing.
#[spec("inbox/probe/081")]
#[test]
fn probe_081_held_typing_is_bound_to_the_generation_it_names() {
    let checking = words()["inputHeld"]["checking"]
        .as_str()
        .unwrap()
        .to_owned();
    // A successor's update before its handover waits for it.
    let mut state = hydrated("41", record("41", None));
    let successor = session("42");
    state
        .sessions
        .insert(successor.session_id.clone(), successor);
    state.apply_input_held_update(&InputHeldUpdate {
        pane_id: PANE.into(),
        agent_id: "42".into(),
        input_held: Some(held("42", InputHeldCause::Checking)),
        input_discarded: None,
    });
    assert_eq!(card(&state, "41").input_held_notice(), None);
    state.apply_inbox_handover(&InboxHandover {
        pane_id: PANE.into(),
        agent_id: "42".into(),
        summary: record("42", None).inbox.unwrap(),
    });
    assert_eq!(
        card(&state, "42").input_held_notice(),
        Some(checking.as_str())
    );
    // The predecessor's late update changes nothing.
    state.apply_input_held_update(&InputHeldUpdate {
        pane_id: PANE.into(),
        agent_id: "41".into(),
        input_held: Some(held("41", InputHeldCause::QueueFull)),
        input_discarded: None,
    });
    assert_eq!(
        card(&state, "42").input_held_notice(),
        Some(checking.as_str())
    );
    // A hold naming another generation than the update is not shown.
    state.apply_input_held_update(&InputHeldUpdate {
        pane_id: PANE.into(),
        agent_id: "42".into(),
        input_held: Some(held("41", InputHeldCause::QueueFull)),
        input_discarded: None,
    });
    assert_eq!(card(&state, "42").input_held_notice(), None);
    // A not-delivered notice naming the generation it rides is its own
    // retirement's, never shown on it.
    state.apply_input_held_update(&InputHeldUpdate {
        pane_id: PANE.into(),
        agent_id: "42".into(),
        input_held: None,
        input_discarded: Some(discarded(42)),
    });
    assert_eq!(card(&state, "42").input_held_notice(), None);
}

/// Scenario: Hydrate a card from a record with no hook snapshot and no hold, then from one that holds typing, then from one that no longer does. The card follows each snapshot: nothing, the checking words, nothing again; a record's discarded typing shows on its later generation only.
#[spec("inbox/probe/081")]
#[test]
fn probe_081_hydration_reads_the_record_not_the_hook_snapshot() {
    let mut state = hydrated("41", record("41", None));
    assert_eq!(card(&state, "41").input_held_notice(), None);
    state.take_inbox_snapshot(&[record("41", Some(held("41", InputHeldCause::Checking)))]);
    assert_eq!(
        card(&state, "41").input_held_notice(),
        words()["inputHeld"]["checking"].as_str()
    );
    state.take_inbox_snapshot(&[record("41", None)]);
    assert_eq!(card(&state, "41").input_held_notice(), None);
    let mut with_discard = record("41", None);
    with_discard.input_discarded = Some(discarded(40));
    state.take_inbox_snapshot(&[with_discard]);
    assert_eq!(
        card(&state, "41").input_held_notice(),
        words()["inputNotDelivered"].as_str()
    );
    let mut own = record("41", None);
    own.input_discarded = Some(discarded(41));
    state.take_inbox_snapshot(&[own]);
    assert_eq!(card(&state, "41").input_held_notice(), None);
}

fn event(
    session: &str,
    agent: &str,
    event_type: dot_agent_deck::event::EventType,
) -> dot_agent_deck::event::AgentEvent {
    dot_agent_deck::event::AgentEvent {
        session_id: session.to_string(),
        agent_type: AgentType::ClaudeCode,
        event_type,
        tool_name: None,
        tool_detail: None,
        cwd: Some("/home/dev/api-svc".to_string()),
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata: Default::default(),
        pane_id: Some(PANE.to_string()),
        agent_id: Some(agent.to_string()),
        agent_version: None,
        schema_version: None,
        live_target: None,
        background: None,
        background_revision: None,
    }
}

/// Scenario: As in the attached dashboard, a reported generation's typing is held and its session ends, so the pane's card is restored as a placeholder still naming it; a successor that has sent no hook takes the pane, and the daemon says the held typing was discarded, before or after the handover. The pane's card shows the shared not-delivered words either way, and drops them once the successor's own card exists.
#[spec("inbox/probe/081")]
#[test]
fn probe_081_the_pane_card_reports_retired_typing_before_the_successor_reports() {
    use dot_agent_deck::event::EventType;
    let text = words()["inputNotDelivered"].as_str().unwrap().to_owned();
    for update_first in [false, true] {
        let mut state = AppState::default();
        state.register_pane(PANE.to_string());
        state.apply_event(event("sess-1", "1", EventType::SessionStart));
        state.take_inbox_snapshot(&[record("1", None)]);
        state.apply_input_held_update(&InputHeldUpdate {
            pane_id: PANE.into(),
            agent_id: "1".into(),
            input_held: Some(held("1", InputHeldCause::Checking)),
            input_discarded: None,
        });
        state.apply_event(event("sess-1", "1", EventType::SessionEnd));
        let notice = InputHeldUpdate {
            pane_id: PANE.into(),
            agent_id: "2".into(),
            input_held: None,
            input_discarded: Some(discarded(1)),
        };
        let handover = InboxHandover {
            pane_id: PANE.into(),
            agent_id: "2".into(),
            summary: record("2", None).inbox.unwrap(),
        };
        if update_first {
            state.apply_input_held_update(&notice);
            state.apply_inbox_handover(&handover);
        } else {
            state.apply_inbox_handover(&handover);
            state.apply_input_held_update(&notice);
        }
        let shown: Vec<_> = state
            .sessions
            .values()
            .filter_map(|s| s.input_held_notice())
            .collect();
        assert_eq!(
            shown,
            vec![text.as_str()],
            "update first: {update_first}: {:?}",
            state
                .sessions
                .values()
                .map(|s| (&s.session_id, &s.agent_id))
                .collect::<Vec<_>>()
        );
        state.apply_event(event("sess-2", "2", EventType::SessionStart));
        assert_eq!(
            state.sessions["sess-2"].input_held_notice(),
            Some(text.as_str())
        );
        let others: Vec<_> = state
            .sessions
            .values()
            .filter(|s| s.session_id != "sess-2")
            .filter_map(|s| s.input_held_notice())
            .collect();
        assert!(others.is_empty(), "{others:?}");
    }
}

/// Scenario: The pane a stopped generation held typing for has been let go of by this client, and its successor is announced as having discarded that typing before the daemon surfaces the successor's card. Once the surfacing gives the pane's account for that successor, its card shows the not-delivered words; an announcement for another generation than the surfaced one shows nothing.
#[spec("inbox/probe/081")]
#[test]
fn probe_081_an_announcement_before_the_pane_is_surfaced_waits_for_its_account() {
    let text = words()["inputNotDelivered"].as_str().unwrap().to_owned();
    for (announced, surfaced, shows) in [("2", "2", true), ("3", "2", false)] {
        let mut state = AppState::default();
        state.apply_input_held_update(&InputHeldUpdate {
            pane_id: PANE.into(),
            agent_id: announced.into(),
            input_held: None,
            input_discarded: Some(discarded(1)),
        });
        let card = session(surfaced);
        state.sessions.insert(card.session_id.clone(), card);
        state.apply_inbox_summary(PANE, surfaced, &record(surfaced, None).inbox.unwrap());
        assert_eq!(
            state.sessions[&format!("sess-{surfaced}")].input_held_notice(),
            shows.then_some(text.as_str()),
            "announced {announced}, surfaced {surfaced}"
        );
    }
}

/// Scenario: The successor's not-delivered announcement arrives while this client still manages the stopped generation's pane and holds its account; then the pane's close arrives and drops that account, and the daemon surfaces the successor's card. The surfaced card shows the shared not-delivered words.
#[spec("inbox/probe/081")]
#[test]
fn probe_081_an_announcement_before_the_pane_closes_survives_the_close() {
    let text = words()["inputNotDelivered"].as_str().unwrap().to_owned();
    let mut state = hydrated("1", record("1", None));
    state.apply_input_held_update(&InputHeldUpdate {
        pane_id: PANE.into(),
        agent_id: "2".into(),
        input_held: None,
        input_discarded: Some(discarded(1)),
    });
    state.unregister_pane(PANE);
    state.sessions.remove("sess-1");
    let card = session("2");
    state.sessions.insert(card.session_id.clone(), card);
    state.apply_inbox_summary(PANE, "2", &record("2", None).inbox.unwrap());
    assert_eq!(
        state.sessions["sess-2"].input_held_notice(),
        Some(text.as_str())
    );
}

/// Scenario: The pane's generation 2 is announced as carrying the not-delivered notice; then a snapshot of generation 2 that carries none clears the card; then the same generation's summary arrives late, and later the pane's account is closed and its card surfaced again. The card stays cleared each time: a snapshot replaces the word the client kept from the live update (R-E-U2-02).
#[spec("inbox/probe/081")]
#[test]
fn probe_081_a_snapshot_that_clears_outlasts_a_later_account_of_its_generation() {
    let text = words()["inputNotDelivered"].as_str().unwrap().to_owned();
    let mut state = hydrated("2", record("2", None));
    state.apply_input_held_update(&InputHeldUpdate {
        pane_id: PANE.into(),
        agent_id: "2".into(),
        input_held: None,
        input_discarded: Some(discarded(1)),
    });
    assert_eq!(
        card(&state, "2").input_held_notice(),
        Some(text.as_str()),
        "precondition: the live update shows the notice"
    );
    state.take_inbox_snapshot(&[record("2", None)]);
    assert_eq!(card(&state, "2").input_held_notice(), None, "cleared");
    state.apply_inbox_summary(PANE, "2", &record("2", None).inbox.unwrap());
    assert_eq!(
        card(&state, "2").input_held_notice(),
        None,
        "a later summary of the same generation keeps the snapshot's word"
    );
    state.unregister_pane(PANE);
    state.apply_inbox_summary(PANE, "2", &record("2", None).inbox.unwrap());
    assert_eq!(
        card(&state, "2").input_held_notice(),
        None,
        "nor does the account a surfacing re-creates bring it back"
    );
}

/// Scenario: Generation 2 takes the pane from generation 1 and is announced as carrying the not-delivered notice; then generation 1's late held-typing word arrives and is rejected; then the pane's account is closed and generation 2's card surfaced. The surfaced card shows generation 2's not-delivered words: the rejected word never displaced them (R-E-U2-02).
#[spec("inbox/probe/081")]
#[test]
fn probe_081_a_rejected_late_word_never_displaces_the_successors() {
    let text = words()["inputNotDelivered"].as_str().unwrap().to_owned();
    let mut state = hydrated("1", record("1", None));
    state.sessions.remove("sess-1");
    let successor = session("2");
    state
        .sessions
        .insert(successor.session_id.clone(), successor);
    state.apply_inbox_summary(PANE, "2", &record("2", None).inbox.unwrap());
    state.apply_input_held_update(&InputHeldUpdate {
        pane_id: PANE.into(),
        agent_id: "2".into(),
        input_held: None,
        input_discarded: Some(discarded(1)),
    });
    state.apply_input_held_update(&InputHeldUpdate {
        pane_id: PANE.into(),
        agent_id: "1".into(),
        input_held: Some(held("1", InputHeldCause::Checking)),
        input_discarded: None,
    });
    assert_eq!(
        card(&state, "2").input_held_notice(),
        Some(text.as_str()),
        "the predecessor's late word changes nothing on the card"
    );
    state.unregister_pane(PANE);
    state.apply_inbox_summary(PANE, "2", &record("2", None).inbox.unwrap());
    assert_eq!(
        card(&state, "2").input_held_notice(),
        Some(text.as_str()),
        "the re-created account takes the successor's word"
    );
}

/// Scenario: Live words arrive for one more pane than the client keeps words for, none of them managed. The client keeps the bound's worth, the oldest pane's word forgotten first and the newest kept (R-E-U3-02).
#[spec("inbox/probe/081")]
#[test]
fn probe_081_the_kept_words_are_bounded_oldest_first() {
    let mut state = AppState::default();
    for n in 0..65 {
        state.apply_input_held_update(&InputHeldUpdate {
            pane_id: format!("pane-{n:02}"),
            agent_id: "2".into(),
            input_held: Some(held("2", InputHeldCause::Checking)),
            input_discarded: None,
        });
    }
    let kept = state.pending_input_held_panes();
    assert_eq!(kept.len(), 64);
    assert!(
        !kept.contains(&"pane-00".to_string()),
        "oldest goes: {kept:?}"
    );
    assert!(kept.contains(&"pane-64".to_string()), "newest stays");
}

/// Scenario: A live word is kept for a pane this client doesn't manage; then the stream reconnects to another daemon process, whose agent ids start over, and that daemon surfaces a generation with the same id on the pane. Its card shows no held typing: the other process's word was forgotten (R-E-U2-02).
#[spec("inbox/probe/081")]
#[test]
fn probe_081_another_daemon_process_forgets_the_kept_words() {
    let named = |process: &str| serde_json::from_value(serde_json::json!(process)).unwrap();
    let mut state = AppState::default();
    state.learn_stream_incarnation(named("daemon-a"));
    state.apply_input_held_update(&InputHeldUpdate {
        pane_id: PANE.into(),
        agent_id: "2".into(),
        input_held: None,
        input_discarded: Some(discarded(1)),
    });
    assert_eq!(state.pending_input_held_panes(), vec![PANE.to_string()]);
    state.learn_stream_incarnation(named("daemon-b"));
    assert!(state.pending_input_held_panes().is_empty());
    let surfaced = session("2");
    state.sessions.insert(surfaced.session_id.clone(), surfaced);
    state.apply_inbox_summary(PANE, "2", &record("2", None).inbox.unwrap());
    assert_eq!(card(&state, "2").input_held_notice(), None);
}
