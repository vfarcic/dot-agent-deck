//! SC-08 additive wire contracts. Assumptions are in fixtures/release-check/wire/README.md.
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, DAEMON_CAPABILITIES, PROTOCOL_VERSION,
};
use dot_agent_deck::inbox_display::{CardInbox, HoldDetail, WakeHold};
use dot_agent_deck::inbox_hub::InboxSummary;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use spec::spec;

#[path = "fixtures/release-check/wire/old_client.rs"]
mod old;

#[cfg(unix)]
#[path = "fixtures/release-check/wire/scripted_daemon.rs"]
mod scripted_daemon;

/// Scenario: A deliberately broken caller writes run verbs on pending sockets and Hello-accepted sockets, then returns Unsupported without waiting for the verbs' replies. Capture must retain every frame, so the same no-verbs assertion used by the client matrix rejects both callers.
#[cfg(unix)]
#[spec("inbox/release-check/002")]
#[tokio::test]
async fn release_check_002_capture_detects_a_write_before_unsupported() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    use dot_agent_deck::daemon_client::GatedQuery;
    use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP, read_frame, write_frame};

    let request = json!({"op":"release-check-run","agent":"claude"});
    let payload = serde_json::to_vec(&request).unwrap();
    let mut frame = vec![KIND_REQ];
    frame.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
    frame.extend_from_slice(&payload);
    for accepted_first in [false, true] {
        let frame = frame.clone();
        let (result, sent) = scripted_daemon::capture(None, false, |path| async move {
            for _ in 0..3 {
                if accepted_first {
                    let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
                    write_frame(&mut stream, KIND_REQ, br#"{"op":"hello"}"#)
                        .await
                        .unwrap();
                    let (kind, _) = read_frame(&mut stream).await.unwrap().unwrap();
                    assert_eq!(kind, KIND_RESP);
                    // The Hello has been answered, so this exercises an accepted
                    // reader whose final verb can still be buffered at return.
                    assert_eq!(stream.try_write(&frame).unwrap(), frame.len());
                } else {
                    // Tiny synchronous writes never yield to the current-thread
                    // server. All peers close with frames in the accept backlog.
                    let mut stream = UnixStream::connect(&path).unwrap();
                    stream.write_all(&frame).unwrap();
                }
            }
            GatedQuery::<()>::Unsupported
        })
        .await;
        assert_eq!(result, GatedQuery::Unsupported);
        assert_eq!(
            sent,
            vec![request.clone(); 3],
            "capture lost queued verb frames; accepted_first={accepted_first}"
        );
        let panic = std::panic::catch_unwind(|| scripted_daemon::assert_no_verbs(&sent))
            .expect_err("the client matrix's no-verbs assertion must reject this control");
        let message = panic.downcast_ref::<String>().unwrap();
        assert!(message.contains("unsupported call sent"), "{message}");
        assert!(message.contains("release-check-run"), "{message}");
    }
    let (result, sent) =
        scripted_daemon::capture(None, false, |_| async { GatedQuery::<()>::Unsupported }).await;
    assert_eq!(result, GatedQuery::Unsupported);
    scripted_daemon::assert_no_verbs(&sent);
}

const CAUSES: &[&str] = &[
    "release_check_running",
    "release_check_differs",
    "release_check_inconclusive",
    "release_check_unavailable",
    "release_check_tripped",
    "release_check_no_reference",
    "release_check_off",
];
const FACTS: &[&str] = &[
    "release_recogniser",
    "ready_composer",
    "input_mode",
    "first_modal",
    "key_effects",
    "echo",
    "head_echo",
    "submit_route",
    "layout",
    "parser_cost",
    "placeholder_empty",
    "delete_key",
    "transcript_echo",
    "unrecognised_frame",
    "ambiguous_frame",
    "transient_content",
    "unrecognised_history",
    "mode_transient",
    "unrecognised_control",
    "unterminated_frame",
    "recogniser_disagrees",
    "endpoint",
    "unexpected_request",
    "timing",
    "namespace_entry",
];
const REASONS: &[&str] = &[
    "queue_full",
    "unterminated_frame",
    "limit_transition",
    "limit_trace",
    "limit_disk",
    "limit_mock",
    "cli",
    "launch",
    "runner_stopped",
    "namespace_entry_refused",
    "cleanup_inventory_unstable",
    "cleanup_migrant",
    "no_process_containment",
    "subreaper",
    "namespace_filter",
    "pid_namespace",
    "no_network_namespace",
    "executable_changed",
    "no_cdhash",
    "no_keychain_evidence",
];

fn roundtrip<T: Serialize + DeserializeOwned>(value: Value) -> Value {
    serde_json::to_value(serde_json::from_value::<T>(value).expect("decode wire value"))
        .expect("encode wire value")
}

fn detail(cause: &str) -> Value {
    json!({"cause": cause, "agent": "claude", "release": "2.1.313",
        "fact": "echo", "reason": "no_process_containment",
        "path_component": "old-path", "filesystem": "old-fs"})
}

fn summary(detail: Value) -> Value {
    json!({"queued": 2, "fetched": 0, "undeliverable": 0, "wake": "doorbell",
        "wake_hold": "cannot_type", "hold_detail": detail,
        "integration_command": "keep-old-command", "receipts_degraded": "keep-old-notice"})
}

/// Scenario: Feed literal new hold details and snapshots to the frozen old client, independently of the new producer. Every new cause and a future unknown cause retain today's cannot_type notice, and new snapshot fields do not stop the old attach reader decoding its reply.
#[spec("inbox/release-check/001")]
#[test]
fn release_check_001_frozen_old_reader_accepts_literal_new_fields() {
    for cause in CAUSES.iter().copied().chain(["release_check_future"]) {
        let frozen: old::InboxSummary = serde_json::from_value(summary(detail(cause))).unwrap();
        assert_eq!(
            frozen.hold_detail.as_ref().unwrap().cause,
            old::HoldCause::Unknown
        );
        assert_eq!(
            frozen
                .hold_detail
                .as_ref()
                .unwrap()
                .path_component
                .as_deref(),
            Some("old-path")
        );
        assert_eq!(
            frozen.hold_detail.as_ref().unwrap().filesystem.as_deref(),
            Some("old-fs")
        );
        assert_eq!(
            frozen.cannot_type_notice().as_deref(),
            Some("✉2 waiting for the agent to fetch it. Send it a prompt.")
        );
    }
    let frozen: old::AttachResponse =
        serde_json::from_value(snapshot("passed", "complete")).unwrap();
    assert!(frozen.ok);
    assert!(frozen.error.is_none());
    let current: InboxSummary =
        serde_json::from_value(summary(json!({"cause":"future_cause"}))).unwrap();
    let card = CardInbox {
        pending: 2,
        hold: current.wake_hold,
        hold_detail: current.hold_detail,
        ..Default::default()
    };
    assert_eq!(
        card.hold_text().as_deref(),
        Some("✉2 waiting for the agent to fetch it. Send it a prompt.")
    );
}

/// Scenario: Show each release-check hold on a card with waiting messages. The card uses the design's specific check notice, agent and release instead of silently falling back to the old generic fetch-it sentence.
#[spec("inbox/release-check/001")]
#[test]
fn release_check_001_cards_show_the_designs_release_check_words() {
    for (cause, agent, expected) in [
        (
            "release_check_running",
            "claude",
            "✉2 waiting: checking Claude Code 2.1.313 on this machine.",
        ),
        (
            "release_check_tripped",
            "claude",
            "✉2 waiting: Claude Code 2.1.313 behaved differently from its check. Check its pane.",
        ),
        (
            "release_check_no_reference",
            "codex",
            "✉2 waiting: the deck has no checked release of Codex to compare with. Send it a prompt.",
        ),
        (
            "release_check_off",
            "claude",
            "✉2 waiting: release checks are off. Send it a prompt.",
        ),
    ] {
        let mut input = detail(cause);
        input["agent"] = json!(agent);
        let card = CardInbox {
            pending: 2,
            hold: Some(WakeHold::CannotType),
            hold_detail: Some(serde_json::from_value(input).unwrap()),
            ..Default::default()
        };
        assert_eq!(card.hold_text().as_deref(), Some(expected), "{cause}");
    }
    for (cause, prefix, suffix) in [
        (
            "release_check_differs",
            "✉2 waiting: Claude Code 2.1.313 didn't pass the deck's check (",
            "). Send it a prompt.",
        ),
        (
            "release_check_inconclusive",
            "✉2 waiting: the deck couldn't check Claude Code 2.1.313 (",
            "). Send it a prompt, or check again.",
        ),
        (
            "release_check_unavailable",
            "✉2 waiting: the deck can't check Claude Code 2.1.313 on this machine (",
            "). Send it a prompt.",
        ),
    ] {
        let card = CardInbox {
            pending: 2,
            hold: Some(WakeHold::CannotType),
            hold_detail: Some(serde_json::from_value(detail(cause)).unwrap()),
            ..Default::default()
        };
        let words = card.hold_text().unwrap();
        assert!(
            words.starts_with(prefix) && words.ends_with(suffix),
            "{cause}: {words}"
        );
        let explanation = &words[prefix.len()..words.len() - suffix.len()];
        assert!(
            !explanation.trim().is_empty(),
            "{cause}: explanation is required"
        );
        assert!(
            !words.contains("no_process_containment"),
            "reason must be in words: {words}"
        );
    }
}

/// Scenario: Decode and re-encode every release-check hold cause with all optional fields, then read the emitted summary with the frozen pre-SC-08 client. The old client keeps its fields and shows its existing fetch-it notice, including for a future unknown cause.
#[spec("inbox/release-check/001")]
#[test]
fn release_check_001_causes_roundtrip_and_old_client_keeps_its_notice() {
    for cause in CAUSES {
        let input = summary(detail(cause));
        let emitted = roundtrip::<InboxSummary>(input.clone());
        assert_eq!(emitted["hold_detail"], input["hold_detail"], "{cause}");
        let frozen: old::InboxSummary = serde_json::from_value(emitted).unwrap();
        assert_eq!(
            frozen.hold_detail.as_ref().unwrap().cause,
            old::HoldCause::Unknown
        );
        assert_eq!(
            frozen
                .hold_detail
                .as_ref()
                .unwrap()
                .path_component
                .as_deref(),
            Some("old-path")
        );
        assert_eq!(
            frozen.hold_detail.as_ref().unwrap().filesystem.as_deref(),
            Some("old-fs")
        );
        assert_eq!(
            frozen.notices.integration_command.as_deref(),
            Some("keep-old-command")
        );
        assert_eq!(
            frozen.notices.receipts_degraded.as_deref(),
            Some("keep-old-notice")
        );
        assert_eq!(
            frozen.cannot_type_notice().as_deref(),
            Some("✉2 waiting for the agent to fetch it. Send it a prompt.")
        );
    }
    let future = summary(detail("release_check_from_the_future"));
    let frozen: old::InboxSummary = serde_json::from_value(future.clone()).unwrap();
    let current: InboxSummary = serde_json::from_value(future).unwrap();
    let card = CardInbox {
        pending: 2,
        hold: current.wake_hold,
        hold_detail: current.hold_detail,
        ..Default::default()
    };
    assert_eq!(card.hold_text(), frozen.cannot_type_notice());
    let control: old::InboxSummary =
        serde_json::from_value(summary(json!({"cause":"filesystem"}))).unwrap();
    assert_eq!(
        control.hold_detail.unwrap().cause,
        old::HoldCause::Filesystem
    );
}

/// Scenario: Read release-check details at their valid bounds and with invalid identifiers or future enum strings. Existing path fields retain their bounds, new fields are optional, and unknown fact/reason values become other rather than peer-supplied prose.
#[spec("inbox/release-check/001")]
#[test]
fn release_check_001_detail_bounds_and_future_values() {
    let old = roundtrip::<HoldDetail>(json!({"cause":"io"}));
    for field in ["agent", "release", "fact", "reason"] {
        assert!(old.get(field).is_none(), "{field}");
    }
    for agent in dot_agent_deck::agent_registry::ALL {
        let mut input = detail("release_check_running");
        input["agent"] = json!(agent.detect_basenames[0]);
        assert_eq!(
            roundtrip::<HoldDetail>(input)["agent"],
            agent.detect_basenames[0]
        );
    }
    for release in ["a".repeat(64), "2.1.313-rc.1+build".into()] {
        let mut input = detail("release_check_running");
        input["release"] = json!(release);
        assert_eq!(roundtrip::<HoldDetail>(input)["release"], release);
    }
    for release in [
        "".into(),
        "a".repeat(65),
        "../../record".into(),
        "2.1\n313".into(),
        "é".repeat(32),
    ] {
        let mut input = detail("release_check_running");
        input["release"] = json!(release);
        assert!(
            roundtrip::<HoldDetail>(input).get("release").is_none(),
            "invalid {release:?}"
        );
    }
    for (field, values) in [("fact", FACTS), ("reason", REASONS)] {
        for value in values {
            let mut input = detail("release_check_differs");
            input[field] = json!(value);
            assert_eq!(roundtrip::<HoldDetail>(input)[field], *value, "{field}");
        }
    }
    let mut future = detail("release_check_inconclusive");
    for field in ["agent", "fact", "reason"] {
        future[field] = json!("untrusted\n".repeat(1000));
    }
    for field in ["path_component", "filesystem"] {
        future[field] = json!("é".repeat(300));
    }
    let bounded = roundtrip::<HoldDetail>(future);
    for field in ["agent", "fact", "reason"] {
        assert_eq!(bounded[field], "other", "{field}");
    }
    for field in ["path_component", "filesystem"] {
        assert_eq!(bounded[field].as_str().unwrap().chars().count(), 256);
    }
    let unknown = roundtrip::<HoldDetail>(json!({"cause":"future_cause"}));
    assert_eq!(unknown["cause"], "unknown");
    assert_eq!(
        WakeHold::CannotType.words(),
        Some("waiting for the agent to fetch it. Send it a prompt.")
    );
}

fn snapshot(state: &str, progress: &str) -> Value {
    json!({"ok":true,"release_checks":[{"agent":"claude","release":"2.1.313",
        "identity":"0123456789abcdef","state":state,"progress":progress,
        "queued_at_ms":1700000000000_i64,"started_at_ms":1700000001000_i64,
        "finished_at_ms":1700000002000_i64,"queue_position":8}]})
}

/// Scenario: Read old replies, empty snapshots, every journal state/progress and first queued or running rows with no result state or later times. Missing/null optional fields stay absent, valid sixteen-lowercase-hex display identities survive while invalid ones disappear, future enums become other, and the additive capability and requests keep protocol 12.
#[spec("inbox/release-check/001")]
#[test]
fn release_check_001_snapshots_capability_and_request_roundtrips() {
    assert_eq!(
        PROTOCOL_VERSION, 12,
        "SC-08 is additive and capability-gated"
    );
    assert!(DAEMON_CAPABILITIES.contains(&"release_check"));
    let hello = roundtrip::<AttachResponse>(
        json!({"ok":true,"capabilities":["release_check","future_capability"]}),
    );
    let frozen: old::AttachResponse = serde_json::from_value(hello).unwrap();
    assert_eq!(
        frozen.capabilities.unwrap(),
        ["release_check", "future_capability"]
    );
    for absent in [json!({"ok":true}), json!({"ok":true,"release_checks":null})] {
        assert!(
            roundtrip::<AttachResponse>(absent)
                .get("release_checks")
                .is_none()
        );
    }
    assert_eq!(
        roundtrip::<AttachResponse>(json!({"ok":true,"release_checks":[]}))["release_checks"],
        json!([])
    );
    for state in [
        "passed",
        "differs",
        "inconclusive",
        "unavailable",
        "revoked",
        "forgotten",
    ] {
        for progress in ["queued", "running", "complete"] {
            let input = snapshot(state, progress);
            let emitted = roundtrip::<AttachResponse>(input.clone());
            assert_eq!(
                emitted["release_checks"], input["release_checks"],
                "{state}/{progress}"
            );
            let frozen: old::AttachResponse = serde_json::from_value(emitted).unwrap();
            assert!(frozen.ok);
            assert!(frozen.error.is_none());
            assert!(frozen.inbox_messages.is_none());
        }
    }
    for progress in ["queued", "running"] {
        for explicit_null in [false, true] {
            let mut input = snapshot("passed", progress);
            let row = input["release_checks"][0].as_object_mut().unwrap();
            for field in [
                "state",
                "queued_at_ms",
                "started_at_ms",
                "finished_at_ms",
                "queue_position",
            ] {
                row.remove(field);
                if explicit_null {
                    row.insert(field.into(), Value::Null);
                }
            }
            let emitted = roundtrip::<AttachResponse>(input);
            let row = &emitted["release_checks"][0];
            assert_eq!(row["progress"], progress);
            assert_eq!(row["identity"], "0123456789abcdef");
            for field in [
                "state",
                "queued_at_ms",
                "started_at_ms",
                "finished_at_ms",
                "queue_position",
            ] {
                assert!(
                    row.get(field).is_none(),
                    "{progress}, null={explicit_null}: absent {field} must not invent a value: {row}"
                );
            }
        }
    }
    // Remove each optional field individually too: later timestamps and a result
    // state cannot be prerequisites for decoding any of the other present fields.
    for field in [
        "state",
        "queued_at_ms",
        "started_at_ms",
        "finished_at_ms",
        "queue_position",
    ] {
        for explicit_null in [false, true] {
            let mut input = snapshot("passed", "running");
            let row = input["release_checks"][0].as_object_mut().unwrap();
            row.remove(field);
            let expected = Value::Object(row.clone());
            if explicit_null {
                row.insert(field.into(), Value::Null);
            }
            let emitted = roundtrip::<AttachResponse>(input);
            assert_eq!(
                emitted["release_checks"][0], expected,
                "missing/null {field} must leave the other snapshot fields intact"
            );
        }
    }
    for identity in ["0000000000000000", "ffffffffffffffff"] {
        let mut input = snapshot("passed", "complete");
        input["release_checks"][0]["identity"] = json!(identity);
        assert_eq!(
            roundtrip::<AttachResponse>(input)["release_checks"][0]["identity"],
            identity
        );
    }
    for identity in [
        "".into(),
        "a".repeat(15),
        "a".repeat(17),
        "g".repeat(16),
        "0123456789ABCDEF".into(),
        "0123456789abcde\n".into(),
        "0123456789abcde\u{1b}".into(),
        "é".repeat(8),
        "f".repeat(4096),
    ] {
        let mut input = snapshot("passed", "complete");
        input["release_checks"][0]["identity"] = json!(identity);
        let mut expected = input["release_checks"][0].clone();
        expected.as_object_mut().unwrap().remove("identity");
        let emitted = roundtrip::<AttachResponse>(input);
        assert_eq!(
            emitted["release_checks"],
            json!([expected]),
            "invalid display identity must become absent without losing its row or other fields, never truncated: {identity:?}"
        );
    }
    let mut future = snapshot("future_state", "future_progress");
    let row = &mut future["release_checks"][0];
    row["identity"] = json!("f".repeat(4096));
    row["release"] = json!("x".repeat(65));
    row["agent"] = json!("future_agent");
    row["queue_position"] = json!(999);
    let emitted = roundtrip::<AttachResponse>(future);
    let row = &emitted["release_checks"][0];
    assert_eq!(row["state"], "other");
    assert_eq!(row["progress"], "other");
    assert_eq!(row["agent"], "other");
    assert!(row.get("release").is_none());
    assert!(row.get("identity").is_none());
    assert!(
        row.get("queue_position")
            .is_none_or(|v| v.as_u64().is_some_and(|p| p <= 8))
    );
    for request in [
        json!({"op":"release-check-run","agent":"claude"}),
        json!({"op":"release-check-cancel"}),
        json!({"op":"release-check-forget","agent":"claude","release":"2.1.313"}),
    ] {
        assert_eq!(roundtrip::<AttachRequest>(request.clone()), request);
    }
}
