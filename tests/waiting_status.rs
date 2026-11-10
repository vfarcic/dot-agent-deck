//! Issue #1757 — the `Waiting` status: an agent whose turn ended while a shell
//! command it started in that same turn is still running.
//!
//! The daemon decides which commands are awaited and stamps the count on the
//! background aggregate it already publishes (`BackgroundWork::awaited`); each
//! client folds `Idle` plus an awaited command into `SessionStatus::Waiting`.
//! This file covers the client fold and hydration (`status/waiting/002`), the
//! wire compatibility in both directions (`status/waiting/003`), and the
//! public readers that decide busy-versus-idle (`status/waiting/005`).
//!
//! Fast tier, in-process, no subprocess. RED until issue #1757 lands:
//! `SessionStatus::Waiting` and `BackgroundWork::awaited` do not exist yet, so
//! this test binary fails to compile; that is the point, and it is kept in a
//! file of its own so no other test binary depends on the new names.

use dot_agent_deck::config::BellConfig;
use dot_agent_deck::dispatched_units::is_busy;
use dot_agent_deck::event::{AgentEvent, BackgroundWork};
use dot_agent_deck::palette::{highest_priority_status, status_color};
use dot_agent_deck::state::{AppState, SessionSnapshot, SessionState, SessionStatus};
use dot_agent_deck::telemetry::tracker::status_name;
use serde_json::{Value, json};
use spec::spec;

const PANE: &str = "waiting-pane";
const AGENT: &str = "waiting-agent";
const SINCE_MS: i64 = 1_767_225_000_000;

/// An event as the daemon broadcasts it to a client, optionally carrying the
/// daemon's background aggregate and its revision.
fn frame(event_type: &str, background: Option<(Value, u64)>) -> AgentEvent {
    let mut wire = json!({
        "session_id": "waiting-session", "pane_id": PANE, "agent_id": AGENT,
        "agent_type": "claude_code", "event_type": event_type,
        "timestamp": chrono::Utc::now(),
    });
    if let Some((work, revision)) = background {
        wire["background"] = work;
        wire["background_revision"] = json!(revision);
    }
    serde_json::from_value(wire).unwrap()
}

/// The daemon's aggregate for `commands` running commands, `awaited` of them
/// started in the turn that just ended.
fn work(commands: u32, awaited: u32) -> Value {
    json!({"commands": commands, "subagents": 0, "since_ms": SINCE_MS, "awaited": awaited})
}

/// The aggregate the daemon publishes when there is nothing to show.
fn cleared() -> Value {
    json!({"commands": 0, "subagents": 0, "since_ms": null})
}

/// The card a client holds for [`PANE`], whichever key it sits under — a
/// hydrated card starts under a placeholder id.
fn card(state: &AppState) -> &SessionState {
    state
        .sessions
        .values()
        .filter(|session| session.pane_id.as_deref() == Some(PANE))
        .max_by_key(|session| session.last_activity)
        .unwrap_or_else(|| panic!("no card for {PANE:?}; sessions: {:?}", state.sessions))
}

fn client() -> AppState {
    let mut state = AppState::default();
    state.register_pane(PANE.into());
    state
}

/// A client card folded into Waiting the way a live broadcast does it.
fn waiting_client() -> AppState {
    let mut state = client();
    state.apply_event(frame("session_start", None));
    state.apply_event(frame("thinking", Some((cleared(), 1))));
    state.apply_event(frame("idle", Some((work(1, 1), 2))));
    state
}

/// Scenario: A client folds the daemon's broadcasts for one pane: a turn ends carrying one awaited command, a repeated Idle arrives without an aggregate, the command's exit publishes a newer empty aggregate, and an older copy is replayed. The card reads Waiting until the newer aggregate clears it, then Idle, and the stale replay cannot bring Waiting back; a new prompt or tool call moves a Waiting card exactly as it moves an Idle one; and a card hydrated from a Waiting snapshot reads Waiting.
#[spec("status/waiting/002")]
#[test]
fn status_waiting_002_client_fold_derives_waiting_from_the_aggregate() {
    let mut tui = waiting_client();
    assert_eq!(
        card(&tui).status,
        SessionStatus::Waiting,
        "Idle with an awaited command is Waiting"
    );
    let background = card(&tui).background_work().expect("the Background line");
    assert_eq!((background.commands, background.awaited), (1, 1));

    // A repeated Idle with no aggregate inherits the held copy.
    tui.apply_event(frame("idle", None));
    assert_eq!(card(&tui).status, SessionStatus::Waiting);

    // The command exits: the monitor's frame carries the newer, empty aggregate.
    tui.apply_event(frame("shell_idle", Some((cleared(), 3))));
    assert_eq!(card(&tui).status, SessionStatus::Idle);
    assert!(card(&tui).background_work().is_none());

    // An older copy replayed later cannot resurrect Waiting.
    tui.apply_event(frame("idle", Some((work(1, 1), 2))));
    assert_eq!(
        card(&tui).status,
        SessionStatus::Idle,
        "a stale aggregate replay must not bring Waiting back"
    );

    // An aggregate still counting the command but no longer awaiting it (a new
    // turn ended while it ran) is Idle with the Background line.
    tui.apply_event(frame("idle", Some((work(1, 0), 4))));
    assert_eq!(card(&tui).status, SessionStatus::Idle);
    assert_eq!(
        card(&tui)
            .background_work()
            .map(|w| (w.commands, w.awaited)),
        Some((1, 0))
    );

    // A new turn moves a Waiting card as it moves an Idle one.
    for (event_type, want) in [
        ("thinking", SessionStatus::Thinking),
        ("tool_start", SessionStatus::Working),
    ] {
        let mut tui = waiting_client();
        assert_eq!(card(&tui).status, SessionStatus::Waiting, "premise");
        tui.apply_event(frame(event_type, Some((cleared(), 3))));
        assert_eq!(card(&tui).status, want, "{event_type} from Waiting");
    }

    // Hydration: a client attaching to a daemon whose card is Waiting.
    let snapshot: SessionSnapshot = serde_json::from_value(json!({
        "status": "Waiting", "agent_type": "claude_code", "tool_count": 0,
        "background": work(1, 1), "background_revision": 5,
    }))
    .unwrap();
    let mut attached = client();
    attached.seed_hydrated_session(PANE.into(), None, None, Some(AGENT.into()), Some(&snapshot));
    assert_eq!(card(&attached).status, SessionStatus::Waiting);
    assert_eq!(
        card(&attached).background_work().map(|w| w.awaited),
        Some(1),
        "hydration keeps the awaited count"
    );
    attached.apply_event(frame("shell_idle", Some((cleared(), 6))));
    assert_eq!(
        card(&attached).status,
        SessionStatus::Idle,
        "a hydrated Waiting card clears like a live one"
    );
}

/// Scenario: Serialize a live snapshot whose status is Waiting and whose background carries an awaited command, then decode it with a reader that predates both. The older reader decodes the whole snapshot with the status as Unknown, Waiting stays a bare string on the wire, an aggregate without `awaited` decodes to zero, and an aggregate with zero awaited leaves the field off the wire.
#[spec("status/waiting/003")]
#[test]
fn status_waiting_003_wire_stays_compatible_both_ways() {
    let mut snapshot = card(&waiting_client()).live_snapshot();
    snapshot.status = SessionStatus::Waiting;
    let wire = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(
        wire["status"],
        json!("Waiting"),
        "Waiting must stay a UNIT variant: a map breaks every older reader's decode"
    );
    assert_eq!(wire["background"]["awaited"], json!(1));

    // A reader that predates the variant and the field, mirroring the
    // pre-#1757 shapes (`status/blocked/007` pins the same for Blocked).
    #[derive(Debug, PartialEq, serde::Deserialize)]
    enum OldStatus {
        Thinking,
        Working,
        Compacting,
        WaitingForInput,
        Idle,
        Error,
        Blocked,
        #[serde(other)]
        Unknown,
    }
    #[derive(Debug, PartialEq, serde::Deserialize)]
    struct OldBackground {
        commands: u32,
        subagents: u32,
        #[serde(default)]
        since_ms: Option<i64>,
    }
    #[derive(Debug, serde::Deserialize)]
    struct OldSnapshot {
        status: OldStatus,
        background: Option<OldBackground>,
    }
    let old: OldSnapshot = serde_json::from_value(wire.clone())
        .expect("an older reader must still decode the whole snapshot");
    assert_eq!(
        old.status,
        OldStatus::Unknown,
        "older clients show it as Idle"
    );
    assert_eq!(
        old.background,
        Some(OldBackground {
            commands: 1,
            subagents: 0,
            since_ms: Some(SINCE_MS)
        }),
        "an older reader keeps the Background line"
    );

    // This build reads its own wire back.
    let back: SessionSnapshot = serde_json::from_value(wire).unwrap();
    assert_eq!(back.status, SessionStatus::Waiting);
    assert_eq!(back.background.map(|w| w.awaited), Some(1));

    // An older daemon's aggregate has no `awaited` and decodes to zero, so a
    // newer client keeps Idle with the Background line.
    let legacy: BackgroundWork =
        serde_json::from_value(json!({"commands": 1, "subagents": 0, "since_ms": SINCE_MS}))
            .unwrap();
    assert_eq!(legacy.awaited, 0);
    let mut tui = client();
    tui.apply_event(frame("session_start", None));
    tui.apply_event(frame(
        "idle",
        Some((
            json!({"commands": 1, "subagents": 0, "since_ms": SINCE_MS}),
            1,
        )),
    ));
    assert_eq!(card(&tui).status, SessionStatus::Idle);
    assert!(card(&tui).background_work().is_some());

    // Zero awaited is omitted, so the wire an older reader sees for a
    // carried-over command is byte-for-byte today's.
    let zero = BackgroundWork {
        commands: 1,
        subagents: 0,
        since_ms: Some(SINCE_MS),
        awaited: 0,
    };
    let zero_wire = serde_json::to_value(zero).unwrap();
    assert!(
        zero_wire.get("awaited").is_none(),
        "awaited = 0 must be omitted: {zero_wire}"
    );
    assert_eq!(
        zero_wire,
        json!({"commands": 1, "subagents": 0, "since_ms": SINCE_MS})
    );
}

/// Scenario: Ask each public reader of "busy or idle" about a Waiting agent. The bell stays silent even with every switch on, the close refusal counts it as busy, it has a colour of its own and ranks between the working states and Idle, telemetry publishes it as `awaiting_command` while Needs input keeps `waiting`, and the doorbell treats its prompt as free.
#[spec("status/waiting/005")]
#[test]
fn status_waiting_005_public_readers_decide_waiting_by_d4() {
    // Bell: the work is not done, so even `on_idle` does not ring for it.
    let every_switch = BellConfig {
        enabled: true,
        on_waiting_for_input: true,
        on_idle: true,
        on_error: true,
    };
    assert!(!every_switch.should_bell(&SessionStatus::Waiting));
    assert!(every_switch.should_bell(&SessionStatus::Idle), "control");

    // Close refusal: a Waiting agent is busy.
    assert!(is_busy(&SessionStatus::Waiting));
    assert!(!is_busy(&SessionStatus::Idle), "control");

    // Palette: its own colour, distinct from Idle, Working and Needs input.
    let waiting = status_color(&SessionStatus::Waiting);
    for other in [
        SessionStatus::Idle,
        SessionStatus::Working,
        SessionStatus::WaitingForInput,
    ] {
        assert_ne!(waiting, status_color(&other), "Waiting vs {other:?}");
    }
    // Priority: between the working states and Idle.
    assert_eq!(
        highest_priority_status(&[SessionStatus::Idle, SessionStatus::Waiting]),
        SessionStatus::Waiting
    );
    assert_eq!(
        highest_priority_status(&[SessionStatus::Waiting, SessionStatus::Idle]),
        SessionStatus::Waiting
    );
    for busier in [
        SessionStatus::Thinking,
        SessionStatus::Working,
        SessionStatus::WaitingForInput,
        SessionStatus::Error,
    ] {
        assert_eq!(
            highest_priority_status(&[SessionStatus::Waiting, busier.clone()]),
            busier,
            "{busier:?} outranks Waiting"
        );
    }

    // Telemetry: a new `deck.status` value; `waiting` keeps meaning Needs input.
    assert_eq!(status_name(&SessionStatus::Waiting), "awaiting_command");
    assert_eq!(status_name(&SessionStatus::WaitingForInput), "waiting");

    // Doorbell: a Waiting agent's prompt is free, so a message may start a turn.
    let tui = waiting_client();
    assert_eq!(card(&tui).status, SessionStatus::Waiting, "premise");
    assert!(
        tui.agent_doorbell_idle(AGENT, PANE),
        "the doorbell treats a Waiting agent as idle"
    );
    let mut working = waiting_client();
    working.apply_event(frame("thinking", Some((cleared(), 3))));
    assert!(!working.agent_doorbell_idle(AGENT, PANE), "control");
}

/// A [`frame`] stamped at `at` rather than now, so a test can match a
/// snapshot's activity watermark to the card's exactly.
fn frame_at(
    event_type: &str,
    background: Option<(Value, u64)>,
    at: chrono::DateTime<chrono::Utc>,
) -> AgentEvent {
    let mut event = frame(event_type, background);
    event.timestamp = at;
    event
}

/// A hydration snapshot for [`PANE`]'s agent: `status`, the background copy
/// at `revision`, and the activity watermark `at`.
fn snapshot_at(
    status: &str,
    background: Value,
    revision: u64,
    at: chrono::DateTime<chrono::Utc>,
) -> SessionSnapshot {
    serde_json::from_value(json!({
        "status": status, "agent_type": "claude_code", "tool_count": 0,
        "background": background, "background_revision": revision,
        "last_activity_ms": at.timestamp_millis(),
    }))
    .unwrap()
}

/// Scenario: A client already holds the pane's card when a hydration snapshot arrives whose activity stamp is no newer than the card's, so the card keeps its own status but takes the snapshot's newer background revision. A Waiting card given a newer cleared aggregate (a retirement, which moves no activity stamp) reads Idle with no background line, and an Idle card given a newer aggregate that awaits a command reads Waiting; a fresh card still hydrates Waiting.
#[spec("status/waiting/013")]
#[test]
fn status_waiting_013_kept_card_hydration_folds_the_winning_aggregate() {
    // Millisecond-exact, so the snapshot's stamp equals the card's.
    let at = chrono::DateTime::from_timestamp_millis(chrono::Utc::now().timestamp_millis())
        .expect("a valid instant");

    // A kept Waiting card; the daemon then retired its spawn's background work
    // and the snapshot reports Idle at the same activity watermark.
    let mut tui = client();
    tui.apply_event(frame_at("session_start", None, at));
    tui.apply_event(frame_at("thinking", Some((cleared(), 1)), at));
    tui.apply_event(frame_at("idle", Some((work(1, 1), 2)), at));
    assert_eq!(card(&tui).status, SessionStatus::Waiting, "premise");
    assert_eq!(card(&tui).last_activity, at, "premise: equal watermark");
    let retired = snapshot_at("Idle", cleared(), 3, at);
    tui.seed_hydrated_session(PANE.into(), None, None, Some(AGENT.into()), Some(&retired));
    assert!(
        card(&tui).background_work().is_none(),
        "the newer cleared aggregate is taken"
    );
    assert_eq!(
        card(&tui).status,
        SessionStatus::Idle,
        "a card with no background work must not stay Waiting"
    );

    // The same race with an OLDER snapshot stamp: the status is refused all the
    // more, and the fold still follows the winning aggregate.
    let mut tui = client();
    tui.apply_event(frame_at("session_start", None, at));
    tui.apply_event(frame_at("thinking", Some((cleared(), 1)), at));
    tui.apply_event(frame_at("idle", Some((work(1, 1), 2)), at));
    let older = at - chrono::Duration::seconds(5);
    tui.seed_hydrated_session(
        PANE.into(),
        None,
        None,
        Some(AGENT.into()),
        Some(&snapshot_at("Idle", cleared(), 3, older)),
    );
    assert_eq!(card(&tui).status, SessionStatus::Idle, "older stamp");

    // A kept Idle card whose snapshot carries a newer aggregate that awaits a
    // command reads Waiting.
    let mut tui = client();
    tui.apply_event(frame_at("session_start", None, at));
    tui.apply_event(frame_at("thinking", Some((cleared(), 1)), at));
    tui.apply_event(frame_at("idle", Some((cleared(), 2)), at));
    assert_eq!(card(&tui).status, SessionStatus::Idle, "premise");
    tui.seed_hydrated_session(
        PANE.into(),
        None,
        None,
        Some(AGENT.into()),
        Some(&snapshot_at("Waiting", work(1, 1), 3, at)),
    );
    assert_eq!(
        card(&tui).background_work().map(|w| w.awaited),
        Some(1),
        "the newer awaited aggregate is taken"
    );
    assert_eq!(card(&tui).status, SessionStatus::Waiting);

    // An older snapshot revision changes neither the copy nor the status.
    let mut tui = waiting_client();
    tui.seed_hydrated_session(
        PANE.into(),
        None,
        None,
        Some(AGENT.into()),
        Some(&snapshot_at("Idle", cleared(), 1, older)),
    );
    assert_eq!(card(&tui).status, SessionStatus::Waiting, "older revision");

    // Control: a fresh card still hydrates Waiting from a Waiting snapshot.
    let mut fresh = client();
    fresh.seed_hydrated_session(
        PANE.into(),
        None,
        None,
        Some(AGENT.into()),
        Some(&snapshot_at("Waiting", work(1, 1), 5, at)),
    );
    assert_eq!(card(&fresh).status, SessionStatus::Waiting, "fresh card");
}
