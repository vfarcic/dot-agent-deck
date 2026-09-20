//! The OpenAI-compatible chat-completions protocol.
//!
//! The second half of PRD #802's provider choice. [`super::remote`] owns the
//! transport, the credential and the failure wording; this module owns one wire
//! dialect — the request body, the response schema, and reading the answer back
//! out of a chat completion. [`super::remote::Protocol`] is what selects it.
//!
//! # Why a second protocol at all
//!
//! Because a provider choice with one implementation is not a choice. Commands
//! needs a key ([`crate::settings::IntentBackend`] has the measurements that
//! made that deliberate), and the moment it does, the endpoint, the model and
//! the key have to be the user's to pick — which means speaking the dialect
//! their provider speaks. `/v1/chat/completions` with a `json_schema` response
//! format is the one most of them speak: OpenAI's own service, the gateways in
//! front of it, and `llama.cpp`'s server all answer it.
//!
//! # The envelope, and the shorthand that must NOT be used
//!
//! ```json
//! "response_format": {
//!   "type": "json_schema",
//!   "json_schema": { "name": "…", "strict": true, "schema": { … } }
//! }
//! ```
//!
//! **`llama.cpp` also accepts `{"type": "json_schema", "schema": {…}}`** — the
//! schema hoisted one level, with no `json_schema` wrapper and no `strict`. That
//! shorthand returns **HTTP 200 and is silently not enforced**, which is the
//! worst failure shape available: a constraint the caller believes it has. The
//! nested form above is the one that is enforced, and it is the only one
//! [`request_body`] can emit.
//!
//! **Enforcement was probed rather than assumed.** Against Anthropic's own
//! OpenAI-compatible endpoint on 2026-09-20, a request whose `action` enum held
//! a single bogus token came back holding that token — over the answer the model
//! plainly wanted to give. So the grammar is doing the work, not the prompt.
//!
//! # Where it has and has not been run
//!
//! Two servers: a local `llama.cpp` in PRD #802's reconnaissance, and
//! Anthropic's `/v1/chat/completions` compatibility endpoint above. It has
//! **not** been run against `api.openai.com`, whose coordinates are this
//! protocol's preset — the key available on the development box had no credits.
//! What that leaves unverified is the preset's provider and model, not the
//! protocol; `docs/develop/desktop-gui.md` carries the same statement in its
//! **what is NOT verified** list rather than leaving a green check to imply
//! otherwise.
//!
//! # Three differences from the Anthropic protocol, each forced
//!
//! - **The instructions go in the `system` turn.** There is no tool, so there is
//!   no `description` field to put them in. The state goes with them and the
//!   transcript stays the one user turn, which keeps the volatile half last
//!   exactly as [`super::remote::request_body`] does.
//! - **Every property is `required`.** OpenAI's strict structured outputs refuse
//!   a schema where a declared property is optional, so `params` cannot simply
//!   be left out of `required` the way the tool-use schema leaves it. Each param
//!   is a `["string", "null"]` union instead — the documented way to express an
//!   optional field under strict mode — and [`super::IntentAnswer`]'s own
//!   deserializer drops the nulls, so a param the chosen action does not take
//!   arrives downstream as absent and becomes
//!   [`super::VoiceOutcome::ParamMissing`] exactly as before.
//! - **The answer is text.** A `tool_use` block is a JSON value; a chat
//!   completion's `content` is a string that has to be parsed. It is read
//!   through [`super::prompt::extract_answer`], whose tolerance is what keeps a
//!   server that accepted the envelope without enforcing it from turning a
//!   fenced-but-correct answer into a failure sentence.

use serde_json::{Value, json};

use super::prompt::{action_enum, param_names, state};
use super::resolver::{IntentAnswer, IntentError, IntentRequest};
use super::schema::{AnnotatedCommand, TOOL_INSTRUCTIONS, TOOL_NAME};

/// A ceiling on the answer, not a budget for it — [`super::remote`]'s constant
/// and its reasoning, spelled `max_completion_tokens`.
///
/// **`max_tokens` is the older spelling and is deliberately not sent.** OpenAI
/// deprecated it for chat completions and its newer models reject it outright,
/// while every server measured here accepts the current name — including
/// Anthropic's compatibility endpoint, checked rather than assumed.
const MAX_COMPLETION_TOKENS: u32 = 256;

/// The `json_schema` the reply is constrained to.
///
/// See the module docs for why every property is `required` and why each param
/// is nullable. The shape is otherwise [`super::remote::tool_definition`]'s
/// `input_schema`: the action enum comes from the annotated command list the
/// backend was handed, and the params are enumerated from the table, so a row
/// that adds a param adds a property without anyone editing this file.
pub fn response_schema(commands: &[AnnotatedCommand]) -> Value {
    let names = param_names(commands);
    let properties: serde_json::Map<String, Value> = names
        .iter()
        .map(|name| (name.clone(), json!({ "type": ["string", "null"] })))
        .collect();
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "description": "The id of the action to run, or `none` when nothing listed matches.",
                "enum": action_enum(commands),
            },
            "params": {
                "type": "object",
                "description": "The params the chosen action declares, as the user referred to them \
    — or the agent's `label`, for a reference the user made by state. Every param is listed; send \
    `null` for the ones the chosen action does not take. The app resolves each one against live \
    state.",
                "properties": properties,
                "required": names,
                "additionalProperties": false,
            },
        },
        "required": ["action", "params"],
        "additionalProperties": false,
    })
}

/// The whole request body.
///
/// The instructions and the live state are the system turn and the transcript is
/// the user turn, which is the split [`super::remote::request_body`] makes and
/// for the same reason: the commands and the agent list are the same across
/// consecutive utterances and the transcript is not.
pub fn request_body(request: &IntentRequest<'_>, model: &str) -> Value {
    json!({
        "model": model,
        "max_completion_tokens": MAX_COMPLETION_TOKENS,
        "messages": [
            {
                "role": "system",
                "content": format!(
                    "You route ONE spoken utterance to ONE action in a desktop app.\n\n{}\n\nState:\n{}",
                    TOOL_INSTRUCTIONS,
                    state(request),
                ),
            },
            { "role": "user", "content": request.transcript.text() },
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": TOOL_NAME,
                "strict": true,
                "schema": response_schema(request.commands),
            },
        },
    })
}

/// The answer, out of `choices[0].message.content`.
///
/// Three replies that are not an answer are told apart, because the remedies
/// differ: a **refusal** (the model declined, which is not the backend being
/// broken), a reply cut off at the token ceiling, and a reply with no content at
/// all.
pub fn parse_response(payload: &Value) -> Result<IntentAnswer, IntentError> {
    let choice = &payload["choices"][0];
    if let Some(refusal) = choice["message"]["refusal"].as_str()
        && !refusal.trim().is_empty()
    {
        return Err(IntentError::Backend(
            "the command backend declined to answer that".into(),
        ));
    }
    if choice["finish_reason"] == "length" {
        return Err(IntentError::Backend(
            "the command backend's answer was cut off before it finished".into(),
        ));
    }
    let Some(content) = choice["message"]["content"].as_str() else {
        return Err(IntentError::Backend(
            "the command backend answered without picking an action".into(),
        ));
    };
    super::prompt::extract_answer(content).map_err(|_| {
        IntentError::Backend(
            "the command backend picked an action this build could not read".into(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::Transcript;
    use crate::voice::fixtures::role_agent as agent;
    use crate::voice::schema::annotate;
    use crate::voice::table::{Screen, table};

    // Pure, like `remote`'s: a `Value` in and a `Value` out, no socket opened
    // at all. What the request and the reply look like on the wire is exactly
    // what these assert; the transport belongs to the credentialed lane.

    fn commands() -> Vec<AnnotatedCommand> {
        annotate(table(), Screen::Deck)
    }

    fn body(said: &str) -> Value {
        let commands = commands();
        let agents = vec![agent("1", "tester")];
        let transcript = Transcript::new(said);
        request_body(
            &IntentRequest {
                transcript: &transcript,
                commands: &commands,
                agents: &agents,
            },
            "a-model",
        )
    }

    // -- the request shape -------------------------------------------------

    #[test]
    fn voice_openai_request_uses_the_nested_json_schema_envelope() {
        // The shorthand `{"type":"json_schema","schema":{…}}` is accepted by
        // llama.cpp, returns 200, and is NOT enforced. This asserts the nested
        // form — the one that is — rather than merely that a schema is present.
        let body = body("show me the tester");
        let format = &body["response_format"];
        assert_eq!(format["type"], "json_schema");
        assert_eq!(format["json_schema"]["name"], TOOL_NAME);
        assert_eq!(format["json_schema"]["strict"], true);
        assert!(
            format["schema"].is_null(),
            "the schema must be nested under `json_schema`, not hoisted: {format}"
        );
        assert_eq!(format["json_schema"]["schema"]["type"], "object");
    }

    #[test]
    fn voice_openai_request_constrains_the_action_to_the_table_plus_the_escape() {
        let schema = &body("show me the tester")["response_format"]["json_schema"]["schema"];
        let actions: Vec<&str> = schema["properties"]["action"]["enum"]
            .as_array()
            .expect("an array")
            .iter()
            .map(|value| value.as_str().expect("a string"))
            .collect();
        assert_eq!(
            actions,
            vec![
                "open_agent",
                "open_overview",
                "open_deck",
                "close_agent_view",
                "open_settings",
                "voice_off",
                "list_commands",
                "none"
            ]
        );
    }

    #[test]
    fn voice_openai_request_schema_is_strict_legal_everywhere() {
        // Strict structured outputs refuse an object that does not set
        // `additionalProperties: false`, and refuse a declared property that is
        // not in `required`. Both are a 400 at run time and silence here
        // otherwise, so both are walked.
        let schema = body("show me the tester")["response_format"]["json_schema"]["schema"].clone();
        fn walk(value: &Value, path: &str) {
            if let Some(object) = value.as_object() {
                if object.get("type").and_then(Value::as_str) == Some("object") {
                    assert_eq!(
                        object.get("additionalProperties"),
                        Some(&Value::Bool(false)),
                        "{path} is an object without `additionalProperties: false`"
                    );
                    let declared: Vec<&String> = object
                        .get("properties")
                        .and_then(Value::as_object)
                        .map(|properties| properties.keys().collect())
                        .unwrap_or_default();
                    let required: Vec<&str> = object
                        .get("required")
                        .and_then(Value::as_array)
                        .map(|names| names.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    for name in declared {
                        assert!(
                            required.contains(&name.as_str()),
                            "{path}.{name} is declared and not required, which strict mode refuses"
                        );
                    }
                }
                for (key, child) in object {
                    walk(child, &format!("{path}.{key}"));
                }
            }
        }
        walk(&schema, "schema");
    }

    #[test]
    fn voice_openai_request_makes_every_param_nullable() {
        // The consequence of requiring every property: a paramless action has
        // to be able to say so, and `null` is how strict mode spells optional.
        let schema = &body("show me the tester")["response_format"]["json_schema"]["schema"];
        assert_eq!(
            schema["properties"]["params"]["properties"]["agent"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(schema["properties"]["params"]["required"], json!(["agent"]));
    }

    #[test]
    fn voice_openai_request_puts_the_instructions_and_state_in_system_and_the_utterance_last() {
        let body = body("show me the tester");
        let system = body["messages"][0]["content"].as_str().expect("a string");
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(system.contains(TOOL_INSTRUCTIONS), "{system}");
        assert!(system.contains("open_agent"), "{system}");
        assert!(system.contains("tester"), "{system}");
        // The volatile half is the user turn, so consecutive utterances share a
        // prefix rather than differing at the front of it.
        assert!(!system.contains("show me the tester"), "{system}");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "show me the tester");
        assert_eq!(body["messages"].as_array().expect("an array").len(), 2);
    }

    #[test]
    fn voice_openai_request_bounds_the_answer_with_the_current_spelling() {
        // `max_tokens` is the deprecated name and newer models reject it.
        let body = body("show me the tester");
        assert_eq!(body["max_completion_tokens"], 256);
        assert!(body["max_tokens"].is_null(), "{body}");
    }

    #[test]
    fn voice_openai_request_never_carries_a_rows_report_wording() {
        let rendered = body("show me the tester").to_string();
        for row in table().rows() {
            assert!(!rendered.contains(&row.report), "`{}` reached it", row.id);
        }
    }

    // -- the response shape ------------------------------------------------

    fn reply(content: &str) -> Value {
        json!({
            "choices": [{
                "finish_reason": "stop",
                "index": 0,
                "message": { "role": "assistant", "content": content },
            }],
        })
    }

    #[test]
    fn voice_openai_parses_a_schema_constrained_answer() {
        let answer = parse_response(&reply(
            r#"{"action":"open_agent","params":{"agent":"tester"}}"#,
        ))
        .expect("parses");
        assert_eq!(answer.action, "open_agent");
        assert_eq!(answer.params.get("agent"), Some(&"tester".to_string()));
    }

    #[test]
    fn voice_openai_drops_the_nulls_strict_mode_forces_the_model_to_send() {
        // The whole reason `params` enumerates and requires every name: an
        // action that takes none still has to fill the object.
        let answer = parse_response(&reply(r#"{"action":"open_deck","params":{"agent":null}}"#))
            .expect("parses");
        assert_eq!(answer.action, "open_deck");
        assert!(
            answer.params.is_empty(),
            "a null param must arrive as absent, not as an empty match: {:?}",
            answer.params
        );
    }

    #[test]
    fn voice_openai_parses_the_none_escape() {
        let answer =
            parse_response(&reply(r#"{"action":"none","params":{"agent":null}}"#)).expect("parses");
        assert!(answer.is_no_match());
    }

    #[test]
    fn voice_openai_reads_an_answer_a_server_fenced_anyway() {
        // Under the nested envelope the content is grammar-enforced and this
        // does not arise. It arises on a server that took the envelope and did
        // not enforce it, which is a real shape — and a fenced but correct
        // answer is not a reason to show a failure sentence.
        let answer = parse_response(&reply(
            "```json\n{\"action\":\"open_overview\",\"params\":{\"agent\":null}}\n```",
        ))
        .expect("parses");
        assert_eq!(answer.action, "open_overview");
    }

    #[test]
    fn voice_openai_keeps_an_action_outside_the_table() {
        // Not this function's refusal to make — `handle_utterance` turns it
        // into UnknownAction, which says something different.
        let answer =
            parse_response(&reply(r#"{"action":"launch_missiles","params":{}}"#)).expect("parses");
        assert_eq!(answer.action, "launch_missiles");
    }

    #[test]
    fn voice_openai_reports_a_refusal_as_its_own_failure() {
        let payload = json!({
            "choices": [{
                "finish_reason": "stop",
                "message": { "role": "assistant", "content": null, "refusal": "I can't help with that." },
            }],
        });
        let error = parse_response(&payload).expect_err("fails");
        assert!(error.detail().contains("declined"), "{error}");
    }

    #[test]
    fn voice_openai_reports_an_answer_cut_off_at_the_ceiling() {
        let payload = json!({
            "choices": [{
                "finish_reason": "length",
                "message": { "role": "assistant", "content": "{\"action\":\"open_ag" },
            }],
        });
        let error = parse_response(&payload).expect_err("fails");
        assert!(error.detail().contains("cut off"), "{error}");
    }

    #[test]
    fn voice_openai_reports_a_reply_with_no_content() {
        let payload = json!({ "choices": [] });
        let error = parse_response(&payload).expect_err("fails");
        assert!(
            error.detail().contains("without picking an action"),
            "{error}"
        );
    }

    #[test]
    fn voice_openai_reports_unreadable_content() {
        let error = parse_response(&reply("I think you want the tester.")).expect_err("fails");
        assert!(error.detail().contains("could not read"), "{error}");
        let error = parse_response(&reply(r#"{"action":7}"#)).expect_err("fails");
        assert!(error.detail().contains("could not read"), "{error}");
    }
}
