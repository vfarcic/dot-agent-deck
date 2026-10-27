//! PRD #1666 part 3 (Q35-09): the composer probe's wire declarations, before
//! anything sets them. Each new field and value is read by a frozen copy of
//! the v0.49.0 readers and each v0.49.0 payload by this build's; every value a
//! newer deck may add is tolerated; and the shared sender withholds a
//! `composer_invocation` from a daemon that has not named the capability on
//! the same connection. The daemon's own arm is in `src/daemon.rs`
//! (`composer_invocation_tests`).

use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::{
    COMPOSER_PROBE, ComposerClaim, ComposerDecision, ComposerInvocationReply,
    ComposerInvocationRequest, ComposerOutcome, DaemonMessage, ExportRead, HookCapabilitiesReply,
    InboxNotice,
};
use dot_agent_deck::inbox_display::{
    CardInbox, HoldCause, HoldDetail, InputHeld, InputHeldCause, MAX_HOLD_DETAIL_CHARS, WakeHold,
};
use dot_agent_deck::inbox_hub::InboxSummary;
use dot_agent_deck::question::{AnswerSettlement, PendingQuestion, SettlementOutcome};
use dot_agent_deck::state::SessionSnapshot;
use serde_json::{Value, json};
use spec::spec;

#[path = "../src/test_temp.rs"]
mod test_temp;

/// v0.49.0's readers, field for field where this commit adds beside them:
/// every required field, and every optional one at the same level, the
/// flattened card notices included. Copied from the `v0.49.0` tag's
/// `src/daemon_protocol.rs`, `src/question.rs`, `src/state.rs`,
/// `src/inbox_hub.rs` and `src/event.rs`. Nested values this commit does not
/// touch are kept as JSON. No `deny_unknown_fields`, as v0.49.0 has none on
/// any of them.
#[allow(dead_code)]
mod v049 {
    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "op", rename_all = "kebab-case")]
    pub enum AttachRequest {
        AnswerQuestion {
            agent_id: String,
            question_id: String,
            answers: Vec<Value>,
            #[serde(default)]
            confirmed_always: bool,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            revision: Option<u64>,
        },
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct PendingQuestion {
        pub id: String,
        pub kind: Value,
        pub questions: Vec<Value>,
        pub raised_at_ms: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tool: Option<Value>,
        pub channel: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub subagent_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub revision: Option<u64>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct SessionSnapshot {
        pub status: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agent_type: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub active_tool: Option<Value>,
        pub tool_count: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub first_prompts: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub last_user_prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub live_target: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub last_activity_ms: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub blocked: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub subagent_wait: Option<Value>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub output_set_status: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub hook_generation: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub delivery_notice: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pending_question: Option<PendingQuestion>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pull_request: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pull_request_revision: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub background: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub background_revision: Option<u64>,
    }

    /// v0.49.0 has no `wake_hold`: the doorbell's hold reasons are this
    /// branch's, so neither `wake_hold` nor `hold_detail` reaches it.
    #[derive(Debug, Serialize, Deserialize)]
    pub struct InboxSummary {
        pub queued: usize,
        pub fetched: usize,
        #[serde(default)]
        pub claimed: usize,
        pub undeliverable: usize,
        #[serde(default)]
        pub last_acked_at: Option<Value>,
        pub wake: Value,
        #[serde(default)]
        pub component: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub wake_stalled: bool,
        #[serde(flatten, default)]
        pub notices: InboxCardNotices,
    }

    /// v0.49.0's card notices, flattened into its summary and its notice.
    #[derive(Debug, Default, Serialize, Deserialize)]
    pub struct InboxCardNotices {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub attention: Vec<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub integration: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub integration_command: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub receipts_degraded: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub evidence_paused: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub admission_refused: Option<String>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct InboxNotice {
        pub pane_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agent_id: Option<String>,
        pub message_id: String,
        pub subject: String,
        pub from_label: String,
        pub state: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reason: Option<Value>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub returned: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub wake_stalled: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub wake_update: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub attention_update: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub requeued: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub provenance: Option<Value>,
        #[serde(flatten, default)]
        pub notices: InboxCardNotices,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct HookCapabilitiesReply {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub task_scoped_completion: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub message: Option<String>,
    }

    /// v0.49.0's hook-socket verbs that answer something, enough to show a
    /// tag it does not know is refused rather than taken for one of them.
    #[derive(Debug, Deserialize)]
    #[serde(tag = "message_type")]
    pub enum DaemonMessage {
        #[serde(rename = "hook_capabilities")]
        HookCapabilities { pane_id: String },
        #[serde(rename = "question")]
        Question { pane_id: String },
    }

    /// What v0.49.0 falls back to for a line that is no `DaemonMessage`.
    #[derive(Debug, Deserialize)]
    pub struct AgentEvent {
        pub session_id: String,
        pub agent_type: Value,
        pub event_type: Value,
        pub timestamp: Value,
    }

    /// This branch's `WakeHold` before Q35-09, which a client built from it
    /// reads `wake_hold` with.
    #[derive(Debug, PartialEq, Eq, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum WakeHoldBeforeQ35 {
        PossibleDraft,
        AwaitingPrompt,
        PasteOpen,
        CannotType,
        NotTaken,
        UncertainInput,
        DialogDuringRing,
        #[serde(other)]
        Unknown,
    }
}

fn settlement(outcome: SettlementOutcome) -> AnswerSettlement {
    AnswerSettlement {
        question_id: "q-1".into(),
        revision: 4,
        client_token: 77,
        outcome,
    }
}

fn input_held(cause: InputHeldCause) -> InputHeld {
    InputHeld {
        generation: 3,
        cause,
        held_ops: 2,
        held_bytes: 9,
        since_ms: 1_700_000_000_000,
    }
}

fn question_json() -> Value {
    json!({
        "id": "q-1",
        "kind": "permission",
        "questions": [{"prompt": "Run it?", "options": []}],
        "raised_at_ms": 5,
        "channel": "held",
        "revision": 4,
    })
}

fn summary_json() -> Value {
    json!({
        "queued": 1, "fetched": 0, "undeliverable": 0, "wake": "doorbell",
        "integration": "not_armed", "evidence_paused": 2,
    })
}

fn notice_json() -> Value {
    json!({
        "pane_id": "p",
        "message_id": "m",
        "subject": "s",
        "from_label": "the orchestrator",
        "state": "queued",
        "wake_update": true,
        "provenance": {"kind": "dispatch_result", "unit_id": "u-1", "phase": "ended"},
        "receipts_degraded": "disk full",
    })
}

const EVERY_SETTLEMENT: [(SettlementOutcome, &str); 5] = [
    (SettlementOutcome::Written, "written"),
    (SettlementOutcome::Partial, "partial"),
    (SettlementOutcome::Superseded, "superseded"),
    (SettlementOutcome::Retired, "retired"),
    (SettlementOutcome::WriteFailed, "write_failed"),
];

const EVERY_INPUT_HELD: [(InputHeldCause, &str); 6] = [
    (InputHeldCause::Checking, "checking"),
    (InputHeldCause::CheckNotOpened, "check_not_opened"),
    (InputHeldCause::CheckNotReturned, "check_not_returned"),
    (InputHeldCause::AfterYourCtrlG, "after_your_ctrl_g"),
    (InputHeldCause::QueueFull, "queue_full"),
    (InputHeldCause::CheckBlocked, "check_blocked"),
];

const EVERY_HOLD_CAUSE: [(HoldCause, &str); 6] = [
    (HoldCause::Filesystem, "filesystem"),
    (HoldCause::WritableDirectory, "writable_directory"),
    (HoldCause::ForeignOwner, "foreign_owner"),
    (HoldCause::Acl, "acl"),
    (HoldCause::ExportFile, "export_file"),
    (HoldCause::Io, "io"),
];

/// Scenario: This build writes a pane snapshot holding a question with a queued answer, every settlement outcome and every input-held cause, an inbox summary and an inbox notice holding each new hold reason with its detail, and a capabilities answer naming the composer probe; a v0.49.0 reader decodes every one of them, keeping what it knew and dropping the new keys.
#[spec("inbox/probe/016")]
#[test]
fn probe_016_a_v049_reader_takes_every_new_value() {
    for (cause, _) in EVERY_INPUT_HELD {
        let mut snapshot: SessionSnapshot =
            serde_json::from_value(json!({"status": "Idle", "tool_count": 2})).unwrap();
        let mut question: PendingQuestion = serde_json::from_value(question_json()).unwrap();
        question.answer_queued = true;
        snapshot.pending_question = Some(question);
        snapshot.input_held = Some(input_held(cause));
        snapshot.answer_settlements = EVERY_SETTLEMENT
            .iter()
            .map(|(outcome, _)| settlement(*outcome))
            .collect();
        let wire = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(wire["pending_question"]["answer_queued"], true);
        assert!(wire["input_held"].is_object(), "{wire}");
        assert_eq!(wire["answer_settlements"].as_array().unwrap().len(), 5);
        let old: v049::SessionSnapshot = serde_json::from_value(wire).unwrap();
        assert_eq!(old.tool_count, 2);
        let old_question = old.pending_question.expect("the question survives");
        assert_eq!(old_question.id, "q-1");
        assert_eq!(old_question.revision, Some(4));
    }

    for hold in [WakeHold::NeedsFetch, WakeHold::CannotCheck] {
        let detail = HoldDetail::new(HoldCause::Filesystem, Some("/mnt/zfs"), Some("zfs"));
        let mut summary: InboxSummary = serde_json::from_value(summary_json()).unwrap();
        summary.wake_hold = Some(hold);
        summary.hold_detail = Some(detail.clone());
        let wire = serde_json::to_value(&summary).unwrap();
        assert_eq!(wire["hold_detail"]["cause"], "filesystem");
        let old: v049::InboxSummary = serde_json::from_value(wire).unwrap();
        assert_eq!(old.queued, 1);
        assert_eq!(old.notices.integration, Some(json!("not_armed")));
        assert_eq!(old.notices.evidence_paused, Some(2));

        let mut notice: InboxNotice = serde_json::from_value(notice_json()).unwrap();
        notice.wake_hold = Some(hold);
        notice.hold_detail = Some(detail);
        let wire = serde_json::to_value(&notice).unwrap();
        assert!(wire["hold_detail"].is_object(), "{wire}");
        let old: v049::InboxNotice = serde_json::from_value(wire).unwrap();
        assert_eq!(old.message_id, "m");
        assert!(old.wake_update);
        assert_eq!(
            old.provenance,
            Some(json!({"kind": "dispatch_result", "unit_id": "u-1", "phase": "ended"}))
        );
        assert_eq!(old.notices.receipts_degraded.as_deref(), Some("disk full"));
    }

    let capabilities = HookCapabilitiesReply {
        task_scoped_completion: Some(1),
        composer_probe: Some(COMPOSER_PROBE),
        ..Default::default()
    };
    let wire = serde_json::to_value(&capabilities).unwrap();
    assert_eq!(wire["composer_probe"], 1);
    let old: v049::HookCapabilitiesReply = serde_json::from_value(wire).unwrap();
    assert_eq!(old.task_scoped_completion, Some(1));
}

/// Scenario: A v0.49.0 daemon's pane snapshot, question, inbox summary, inbox notice and capabilities answer are read by this build: no answer is queued, no input is held, no settlement is listed, no hold detail is set and the composer probe is not served. Nothing is written for a new field left unset.
#[spec("inbox/probe/016")]
#[test]
fn probe_016_this_build_takes_v049_payloads() {
    let old_question: v049::PendingQuestion = serde_json::from_value(question_json()).unwrap();
    let old_snapshot = v049::SessionSnapshot {
        status: json!("Idle"),
        agent_type: None,
        active_tool: None,
        tool_count: 0,
        first_prompts: Vec::new(),
        last_user_prompt: None,
        live_target: None,
        last_activity_ms: Some(9),
        blocked: None,
        subagent_wait: None,
        output_set_status: false,
        hook_generation: None,
        delivery_notice: None,
        pending_question: Some(old_question),
        pull_request: None,
        pull_request_revision: None,
        background: None,
        background_revision: None,
    };
    let snapshot: SessionSnapshot =
        serde_json::from_value(serde_json::to_value(&old_snapshot).unwrap()).unwrap();
    assert_eq!(snapshot.input_held, None);
    assert!(snapshot.answer_settlements.is_empty());
    let question = snapshot.pending_question.clone().expect("the question");
    assert!(!question.answer_queued);
    assert_eq!(question.revision, Some(4));
    let rewritten = serde_json::to_value(&snapshot).unwrap();
    for key in ["input_held", "answer_settlements"] {
        assert!(rewritten.get(key).is_none(), "{key}: {rewritten}");
    }
    assert!(
        rewritten["pending_question"].get("answer_queued").is_none(),
        "{rewritten}"
    );

    let old_summary: v049::InboxSummary = serde_json::from_value(summary_json()).unwrap();
    let summary: InboxSummary =
        serde_json::from_value(serde_json::to_value(&old_summary).unwrap()).unwrap();
    assert_eq!(summary.hold_detail, None);
    assert_eq!(
        summary.notices.integration,
        Some(dot_agent_deck::inbox_display::IntegrationNotice::NotArmed)
    );
    assert_eq!(summary.notices.evidence_paused, Some(2));
    assert!(
        serde_json::to_value(&summary)
            .unwrap()
            .get("hold_detail")
            .is_none()
    );
    let old_notice: v049::InboxNotice = serde_json::from_value(notice_json()).unwrap();
    let notice: InboxNotice =
        serde_json::from_value(serde_json::to_value(&old_notice).unwrap()).unwrap();
    assert_eq!(notice.hold_detail, None);
    assert!(notice.provenance.is_some(), "{notice:?}");
    assert_eq!(
        notice.notices.receipts_degraded.as_deref(),
        Some("disk full")
    );

    let old_capabilities = v049::HookCapabilitiesReply {
        task_scoped_completion: Some(1),
        code: None,
        message: None,
    };
    let line = serde_json::to_string(&old_capabilities).unwrap();
    let capabilities: HookCapabilitiesReply = serde_json::from_str(&line).unwrap();
    assert_eq!(capabilities.composer_probe, None);
    assert_eq!(
        dot_agent_deck::hook::confirms_composer_probe(&line),
        Err(None)
    );
}

/// Scenario: This build's answer request carrying a client token is read by a v0.49.0 daemon as the same answer, the token dropped; a request with no token writes no token key; and a v0.49.0 client's request is read with no token.
#[spec("inbox/probe/016")]
#[test]
fn probe_016_an_old_daemon_ignores_the_answer_token() {
    let answer = json!({"question_index": 0, "option_indices": [1]});
    let tokened: AttachRequest = serde_json::from_value(json!({
        "op": "answer-question",
        "agent_id": "7",
        "question_id": "q-1",
        "answers": [answer],
        "confirmed_always": true,
        "revision": 4,
        "client_token": 77,
    }))
    .unwrap();
    let AttachRequest::AnswerQuestion { client_token, .. } = &tokened else {
        panic!("{tokened:?}");
    };
    assert_eq!(*client_token, Some(77));
    let wire = serde_json::to_value(&tokened).unwrap();
    assert_eq!(wire["client_token"], 77);
    let v049::AttachRequest::AnswerQuestion {
        agent_id,
        question_id,
        answers,
        confirmed_always,
        revision,
    } = serde_json::from_value(wire).unwrap();
    assert_eq!(
        (agent_id.as_str(), question_id.as_str()),
        ("7", "q-1"),
        "the answer is the same answer"
    );
    assert_eq!(answers, vec![answer.clone()]);
    assert!(confirmed_always);
    assert_eq!(revision, Some(4));

    let old = v049::AttachRequest::AnswerQuestion {
        agent_id: "7".into(),
        question_id: "q-1".into(),
        answers: vec![answer],
        confirmed_always: false,
        revision: Some(4),
    };
    let untokened: AttachRequest =
        serde_json::from_value(serde_json::to_value(&old).unwrap()).unwrap();
    let AttachRequest::AnswerQuestion {
        client_token,
        revision,
        ..
    } = &untokened
    else {
        panic!("{untokened:?}");
    };
    assert_eq!((*client_token, *revision), (None, Some(4)));
    assert!(
        serde_json::to_value(&untokened)
            .unwrap()
            .get("client_token")
            .is_none()
    );
}

/// Scenario: Every new outcome, cause, decision, read and hold reason keeps its wire spelling and reads back as itself; one a newer deck adds reads as unknown, inside a whole snapshot, notice or reply as well as alone; a client built before this commit reads the two new hold reasons as unknown; and a card carries each new hold reason through its journal.
#[spec("inbox/probe/016")]
#[test]
fn probe_016_unknown_new_values_are_tolerated() {
    for (outcome, spelling) in EVERY_SETTLEMENT {
        assert_eq!(serde_json::to_value(outcome).unwrap(), json!(spelling));
        assert_eq!(
            serde_json::from_value::<SettlementOutcome>(json!(spelling)).unwrap(),
            outcome
        );
    }
    for (cause, spelling) in EVERY_INPUT_HELD {
        assert_eq!(serde_json::to_value(cause).unwrap(), json!(spelling));
        let held: InputHeld =
            serde_json::from_value(serde_json::to_value(input_held(cause)).unwrap()).unwrap();
        assert_eq!(held, input_held(cause));
    }
    for (cause, spelling) in EVERY_HOLD_CAUSE {
        assert_eq!(serde_json::to_value(cause).unwrap(), json!(spelling));
        assert_eq!(
            serde_json::from_value::<HoldCause>(json!(spelling)).unwrap(),
            cause
        );
    }

    let snapshot: SessionSnapshot = serde_json::from_value(json!({
        "status": "Idle",
        "tool_count": 0,
        "input_held": {
            "generation": 3, "cause": "held_for_a_reason_from_later",
            "held_ops": 1, "held_bytes": 1, "since_ms": 0
        },
        "answer_settlements": [
            {"question_id": "q-1", "revision": 4, "client_token": 77, "outcome": "acknowledged"},
            {"question_id": "q-1", "revision": 4, "client_token": 78, "outcome": "written"},
        ],
    }))
    .unwrap();
    assert_eq!(
        snapshot.input_held.map(|held| held.cause),
        Some(InputHeldCause::Unknown)
    );
    assert_eq!(
        snapshot
            .answer_settlements
            .iter()
            .map(|s| (s.client_token, s.outcome))
            .collect::<Vec<_>>(),
        vec![
            (77, SettlementOutcome::Unknown),
            (78, SettlementOutcome::Written)
        ]
    );

    let mut notice = notice_json();
    notice["wake_hold"] = json!("a_hold_from_later");
    notice["hold_detail"] = json!({"cause": "a_cause_from_later", "filesystem": "zfs"});
    let notice: InboxNotice = serde_json::from_value(notice).unwrap();
    assert_eq!(notice.wake_hold, Some(WakeHold::Unknown));
    assert_eq!(
        notice.hold_detail,
        Some(HoldDetail {
            cause: HoldCause::Unknown,
            path_component: None,
            filesystem: Some("zfs".into()),
        })
    );

    for (hold, spelling) in [
        (WakeHold::NeedsFetch, "needs_fetch"),
        (WakeHold::CannotCheck, "cannot_check"),
    ] {
        assert_eq!(serde_json::to_value(hold).unwrap(), json!(spelling));
        assert_eq!(
            serde_json::from_value::<WakeHold>(json!(spelling)).unwrap(),
            hold
        );
        assert_eq!(
            serde_json::from_value::<v049::WakeHoldBeforeQ35>(json!(spelling)).unwrap(),
            v049::WakeHoldBeforeQ35::Unknown,
            "a client built before this commit reads {spelling} as unknown"
        );
        // Q35-10 gave both their words; neither warns with nothing waiting.
        assert!(hold.words().is_some(), "{spelling} has its words");
        assert!(hold.notice_clause().is_some(), "{spelling} has its clause");
        assert_eq!(hold.alone_words(), None);
        let card = CardInbox {
            pending: 1,
            hold: Some(hold),
            ..CardInbox::default()
        };
        assert_eq!(CardInbox::decode(&card.encode()), Some(card.clone()));
    }
    let journal = |hold| {
        CardInbox {
            hold: Some(hold),
            ..CardInbox::default()
        }
        .encode()
    };
    assert_ne!(
        journal(WakeHold::NeedsFetch),
        journal(WakeHold::CannotCheck)
    );
    assert_ne!(journal(WakeHold::NeedsFetch), journal(WakeHold::Unknown));
    assert_ne!(journal(WakeHold::CannotCheck), journal(WakeHold::Unknown));

    for (outcome, wire) in [
        (
            ComposerOutcome::Mine {
                read: ExportRead::Empty,
            },
            json!({"kind": "mine", "read": "empty"}),
        ),
        (
            ComposerOutcome::MineWithNotice {
                read: ExportRead::NonEmpty,
            },
            json!({"kind": "mine_with_notice", "read": "non_empty"}),
        ),
        (
            ComposerOutcome::Mine {
                read: ExportRead::Unreadable,
            },
            json!({"kind": "mine", "read": "unreadable"}),
        ),
        (ComposerOutcome::Uncertain, json!({"kind": "uncertain"})),
        (ComposerOutcome::Unarmed, json!({"kind": "unarmed"})),
    ] {
        assert_eq!(serde_json::to_value(outcome).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<ComposerOutcome>(wire).unwrap(),
            outcome
        );
    }
    let reply: ComposerInvocationReply =
        serde_json::from_value(json!({"outcome": {"kind": "mine_twice"}})).unwrap();
    assert_eq!(reply.outcome, Some(ComposerOutcome::Unknown));
    let reply: ComposerInvocationReply =
        serde_json::from_value(json!({"outcome": {"kind": "mine", "read": "half_empty"}})).unwrap();
    assert_eq!(
        reply.outcome,
        Some(ComposerOutcome::Mine {
            read: ExportRead::Unknown
        })
    );
    assert_eq!(
        serde_json::from_value::<ComposerDecision>(json!("deferred")).unwrap(),
        ComposerDecision::Unknown
    );
    assert_eq!(
        serde_json::from_value::<ComposerClaim>(json!("unknown")).unwrap(),
        ComposerClaim::Unknown
    );
    assert_eq!(
        serde_json::from_value::<ComposerClaim>(json!("nested")).unwrap(),
        ComposerClaim::Unknown
    );
}

/// Scenario: A hold detail's path component and filesystem name longer than the bound are cut to it, whether built here or read off the wire, and shorter ones are kept whole.
#[spec("inbox/probe/016")]
#[test]
fn probe_016_hold_detail_strings_are_bounded() {
    let long = "é".repeat(MAX_HOLD_DETAIL_CHARS + 5);
    let detail = HoldDetail::new(HoldCause::Io, Some(&long), Some(&long));
    for text in [&detail.path_component, &detail.filesystem] {
        assert_eq!(
            text.as_deref().map(|t| t.chars().count()),
            Some(MAX_HOLD_DETAIL_CHARS)
        );
    }
    let wire: HoldDetail = serde_json::from_value(json!({
        "cause": "io", "path_component": long, "filesystem": long
    }))
    .unwrap();
    assert_eq!(wire, detail, "read off the wire, bounded the same");
    let exact = "x".repeat(MAX_HOLD_DETAIL_CHARS);
    assert_eq!(
        HoldDetail::new(HoldCause::Io, Some(&exact), None).path_component,
        Some(exact)
    );
    assert_eq!(
        HoldDetail::new(HoldCause::Acl, Some("/tmp"), Some("tmpfs")),
        HoldDetail {
            cause: HoldCause::Acl,
            path_component: Some("/tmp".into()),
            filesystem: Some("tmpfs".into()),
        }
    );
    assert_eq!(
        HoldDetail::new(HoldCause::Acl, None, None),
        HoldDetail {
            cause: HoldCause::Acl,
            path_component: None,
            filesystem: None,
        }
    );
}

fn request() -> ComposerInvocationRequest {
    ComposerInvocationRequest {
        pane_id: "p".into(),
        token: Some("t".into()),
        generation: 3,
        decision: ComposerDecision::Armed,
        claim: Some(ComposerClaim::Owned),
        seg_seen: Some(5),
        argv: vec!["/tmp/claude-1000/x.md".into()],
    }
}

/// Scenario: A `composer_invocation` line is neither a hook verb nor an agent event to a v0.49.0 daemon, so it is logged as malformed and answered with nothing rather than taken for anything; this build reads it back as the same request, which claims its pane and presents its token, needs a token that attests the agent, is answered by its handler rather than at admission, and is refused by the gate in its own reply type; the capabilities question, refused by the gate, carries its code and confirms no verb.
#[spec("inbox/probe/016")]
#[test]
fn probe_016_an_old_daemon_takes_no_invocation() {
    let line = serde_json::to_string(&DaemonMessage::ComposerInvocation(request())).unwrap();
    assert!(
        line.contains(r#""message_type":"composer_invocation""#),
        "{line}"
    );
    assert!(serde_json::from_str::<v049::DaemonMessage>(&line).is_err());
    assert!(serde_json::from_str::<v049::AgentEvent>(&line).is_err());
    let message = serde_json::from_str::<DaemonMessage>(&line).unwrap();
    match &message {
        DaemonMessage::ComposerInvocation(back) => assert_eq!(*back, request()),
        other => panic!("{other:?}"),
    }
    assert_eq!(message.verb(), "composer_invocation");
    assert_eq!(message.claimed_pane(), "p");
    assert_eq!(message.presented_token(), Some("t"));
    assert!(message.requires_attested_agent());
    assert_eq!(message.claimed_agent(), None);
    assert_eq!(message.provenance_ack_reply(), None);
    let refusal = message
        .provenance_refusal_reply("unattested", "who?")
        .expect("a refusal line");
    assert_eq!(
        serde_json::from_str::<ComposerInvocationReply>(&refusal).unwrap(),
        ComposerInvocationReply::refused("unattested", "who?")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&refusal).unwrap(),
        json!({"code": "unattested", "message": "who?"})
    );

    // The sender's question, refused at the gate, comes back with its code
    // and names no verb, so the sender withholds the invocation and says why.
    let question =
        DaemonMessage::HookCapabilities(dot_agent_deck::event::HookCapabilitiesRequest {
            pane_id: "p".into(),
            token: None,
        });
    let refusal = question
        .provenance_refusal_reply("missing_token", "no token")
        .expect("a refusal line");
    assert_eq!(
        serde_json::from_str::<Value>(&refusal).unwrap(),
        json!({"code": "missing_token", "message": "no token"})
    );
    assert_eq!(
        dot_agent_deck::hook::confirms_composer_probe(&refusal),
        Err(Some(("missing_token".into(), "no token".into())))
    );
}

#[cfg(unix)]
mod sender {
    use super::*;
    use dot_agent_deck::hook::{Gated, send_composer_invocation};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// A daemon that answers the capabilities question with `answer`, then
    /// answers a mutation with `reply`, and records every line it read.
    fn stub(
        answer: &'static str,
        reply: &'static str,
    ) -> (
        std::path::PathBuf,
        tempfile::TempDir,
        Arc<Mutex<Vec<String>>>,
    ) {
        let dir = test_temp::tempdir().unwrap();
        let path = dir.path().join("hook.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = lines.clone();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                let answering = if line.contains("hook_capabilities") {
                    answer
                } else {
                    reply
                };
                seen.lock().unwrap().push(std::mem::take(&mut line));
                if writeln!(writer, "{answering}").is_err() {
                    return;
                }
            }
        });
        (path, dir, lines)
    }

    fn invocations(lines: &Mutex<Vec<String>>) -> usize {
        lines
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains("composer_invocation"))
            .count()
    }

    /// Scenario: The shared sender asks a daemon which verbs it serves before a `composer_invocation`: a daemon that names task-scoped completion and input-mode reports but not the composer probe, one that names another version of it, one that refuses the question, and one that answers something else each get no invocation, the refusal coming back with its code. A daemon that names this version is sent the invocation once, on the same connection, and its answer comes back.
    #[spec("inbox/probe/016")]
    #[test]
    fn probe_016_the_sender_withholds_an_invocation_without_the_capability() {
        let unsupported = r#"{"code":"unsupported","message":"no"}"#;
        for answer in [
            r#"{"task_scoped_completion":1,"input_mode_report":1}"#,
            r#"{"composer_probe":2}"#,
            r#"{"composer_probe":0}"#,
            "{}",
            r#"{"kind":"signal_ack","accepted":true}"#,
        ] {
            let (path, _dir, lines) = stub(answer, unsupported);
            let sent = send_composer_invocation(&path, &request(), Duration::from_millis(500));
            assert!(
                matches!(sent, Gated::NotConfirmed { refusal: None, .. }),
                "{answer}: {sent:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
            assert_eq!(invocations(&lines), 0, "{answer}");
            assert_eq!(
                lines.lock().unwrap().len(),
                1,
                "{answer}: the question alone"
            );
        }

        let (path, _dir, lines) = stub(r#"{"code":"unattested","message":"who?"}"#, unsupported);
        let sent = send_composer_invocation(&path, &request(), Duration::from_millis(500));
        assert_eq!(
            sent,
            Gated::NotConfirmed {
                refusal: Some(("unattested".into(), "who?".into())),
                why: "the daemon did not confirm the verb",
            }
        );
        assert_eq!(invocations(&lines), 0);

        let mine = r#"{"outcome":{"kind":"mine","read":"empty"}}"#;
        let (path, _dir, lines) = stub(r#"{"composer_probe":1}"#, mine);
        let sent = send_composer_invocation(&path, &request(), Duration::from_millis(500));
        assert_eq!(sent, Gated::Reply(mine.to_string()));
        assert_eq!(invocations(&lines), 1);
        let seen = lines.lock().unwrap().clone();
        let question: Value = serde_json::from_str(&seen[0]).unwrap();
        assert_eq!(
            question,
            json!({"message_type": "hook_capabilities", "pane_id": "p", "token": "t"})
        );
        match serde_json::from_str::<DaemonMessage>(&seen[1]).unwrap() {
            DaemonMessage::ComposerInvocation(sent) => assert_eq!(sent, request()),
            other => panic!("{other:?}"),
        }
    }
}
