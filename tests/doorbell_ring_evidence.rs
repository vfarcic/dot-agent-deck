//! `inbox/doorbell/046` (the hook and wire half): a prompt hook reports the
//! ring token its prompt carried (`src/inbox_prompt.rs`).
use std::path::PathBuf;

use dot_agent_deck::event::{InboxComponent, InboxFetchConsumer, InboxFetchRequest, PromptShape};
use dot_agent_deck::inbox_prompt::{Envelope, HookHandOver, one_shot_request, ring_of};
use spec::spec;

fn payload_prompt(stem: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{stem}-user-prompt-submit.json"));
    let payload: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    payload["prompt"].as_str().unwrap().to_string()
}

/// Scenario: Read the prompts Claude Code 2.1.310 and Codex 0.164.0 and 0.160.0 handed their UserPromptSubmit hooks for a submitted token-led doorbell line, and variants of them: with the user's text around the line, an older deck's line with no token, a malformed token, and an invalid message id. The submitted line alone is an exact ring with its token and message; around other text it is a ring with other text; the others are no ring.
#[spec("inbox/doorbell/046")]
#[test]
fn doorbell_046_the_ring_is_read_off_real_prompt_payloads() {
    for stem in [
        "claude-inbox/r5wake/claude-2.1.310",
        "codex-inbox/r5wake/codex-0.164.0",
        "codex-inbox/r5wake/codex-0.160.0",
    ] {
        let prompt = payload_prompt(stem);
        let ring = ring_of(&prompt).unwrap_or_else(|| panic!("{stem}: no ring in {prompt:?}"));
        assert_eq!(ring.shape, PromptShape::Exact, "{stem}");
        assert_eq!(&prompt[1..7], ring.token, "{stem}");
        assert!(prompt.contains(&ring.id), "{stem}");
        assert_eq!(
            ring_of(&format!("  {prompt}\n")).unwrap().shape,
            PromptShape::Exact,
            "surrounding whitespace is not other text"
        );
        for merged in [
            format!("{prompt}fix the build"),
            format!("please look\n{prompt}"),
            format!("{prompt} and also this"),
        ] {
            let ring = ring_of(&merged).expect("the line is still there");
            assert_eq!(ring.shape, PromptShape::WithOtherText, "{merged:?}");
        }
        let untokened = &prompt[8..];
        assert_eq!(
            ring_of(untokened),
            None,
            "an older deck's line carries no ring"
        );
        let shouting = prompt.replacen(&prompt[1..7], "ABCDEF", 1);
        assert_eq!(ring_of(&shouting), None);
        let short = prompt.replacen(&prompt[1..7], "abc", 1);
        assert_eq!(ring_of(&short), None);
    }
    assert_eq!(ring_of("an ordinary prompt"), None);
    assert_eq!(
        ring_of(
            "(abcdfg) dot-agent-deck: message m-NOTANID-0000000000000000 waiting, run: dot-agent-deck inbox fetch"
        ),
        None
    );
}

/// Scenario: Build the doorbell fetch a prompt hook sends for the message its prompt's line names, with the ring that line carried, with a ring naming another message, and with none; then decode the wire forms. The fetch carries the ring's token and shape only for the message the line named; without a ring the wire is what an older hook sends; an unknown shape decodes as unknown.
#[spec("inbox/doorbell/046")]
#[test]
fn doorbell_046_the_hook_fetch_carries_the_ring() {
    let prompt = payload_prompt("codex-inbox/r5wake/codex-0.164.0");
    let ring = ring_of(&prompt).unwrap();
    let envelope = Envelope::Context {
        hook_event_name: "UserPromptSubmit".to_string(),
    };
    let request = |rung: &str, ring| {
        one_shot_request(
            &HookHandOver {
                component: InboxComponent::CodexHooks,
                consumer: InboxFetchConsumer::DoorbellHook,
                rung: Some(rung),
                ring,
                envelope: &envelope,
                framing: "framing",
            },
            "pane".to_string(),
            Some("agent".to_string()),
            "token".to_string(),
        )
    };
    let with = request(&ring.id, Some(&ring));
    assert_eq!(with.doorbell.as_deref(), Some(ring.id.as_str()));
    assert_eq!(with.doorbell_ring.as_deref(), Some(ring.token.as_str()));
    assert_eq!(with.doorbell_prompt, Some(PromptShape::Exact));
    let other = request("m-aaaaaaaa-0000000000000000", Some(&ring));
    assert_eq!(other.doorbell_ring, None, "a ring naming another message");
    let none = request(&ring.id, None);
    assert_eq!(none.doorbell_ring, None);
    let wire = serde_json::to_value(&none).unwrap();
    assert!(wire.get("doorbell_ring").is_none() && wire.get("doorbell_prompt").is_none());
    let mut wire = serde_json::to_value(&with).unwrap();
    assert_eq!(wire["doorbell_prompt"], "exact");
    wire["doorbell_prompt"] = "a_future_shape".into();
    let decoded: InboxFetchRequest = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded.doorbell_prompt, Some(PromptShape::Unknown));
}
