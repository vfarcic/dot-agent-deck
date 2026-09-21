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

use crate::model_service::TokenCeiling;

use super::prompt::{action_enum, param_names, state};
use super::remote::truncated_at;
use super::resolver::{IntentAnswer, IntentError, IntentRequest};
use super::schema::{AnnotatedCommand, TOOL_INSTRUCTIONS, TOOL_NAME};

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
///
/// **`max_tokens` is the older spelling of the ceiling and is deliberately not
/// sent.** OpenAI deprecated it for chat completions and its newer models
/// reject it outright, while every server measured here accepts
/// `max_completion_tokens` — including Anthropic's compatibility endpoint,
/// checked rather than assumed. The two spellings carry the same
/// [`TokenCeiling`], which is why the settings field is one number and not one
/// per dialect.
///
/// # `reasoning_effort` is added only when it is handed one
///
/// It is an **OpenAI-family** parameter and not part of the
/// `/v1/chat/completions` shape every server implementing that path accepts, so
/// an unconditional field here would be a 400 on somebody's gateway or local
/// server — the `llama.cpp` server probed for PRD #802 wanted
/// `chat_template_kwargs` instead, and even `api.openai.com` refuses it for
/// models that do not reason. This function therefore has no opinion about when
/// to send it: [`crate::settings::IntentSettings::reasoning_effort`] decides,
/// against the endpoint and the model together, and `None` leaves the key out
/// of the body entirely rather than sending a null.
pub fn request_body(
    request: &IntentRequest<'_>,
    model: &str,
    max_tokens: TokenCeiling,
    reasoning_effort: Option<&str>,
) -> Value {
    let mut body = json!({
        "model": model,
        "max_completion_tokens": max_tokens.get(),
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
    });
    if let Some(effort) = reasoning_effort {
        // Inserted rather than declared with a null default: a provider that
        // does not know the field should see a body that does not mention it,
        // not one that mentions it emptily.
        body["reasoning_effort"] = Value::String(effort.to_string());
    }
    body
}

/// The answer, out of `choices[0].message.content`.
///
/// Three replies that are not an answer are told apart, because the remedies
/// differ: a **refusal** (the model declined, which is not the backend being
/// broken), a reply cut off at the token ceiling, and a reply with no content at
/// all.
///
/// The ceiling is passed in so the truncation sentence can **name it**. That
/// used to read *"the command backend's answer was cut off before it finished"*,
/// which told the user a fact about the request and nothing they could act on —
/// honestly so, at the time, because the ceiling was a `const` nobody could
/// change. Now that it is a settings field, a sentence that does not name the
/// number or the row that changes it is withholding the whole remedy. See
/// [`truncated_at`].
pub fn parse_response(
    payload: &Value,
    max_tokens: TokenCeiling,
) -> Result<IntentAnswer, IntentError> {
    let choice = &payload["choices"][0];
    if let Some(refusal) = choice["message"]["refusal"].as_str()
        && !refusal.trim().is_empty()
    {
        return Err(IntentError::Backend(
            "the command backend declined to answer that".into(),
        ));
    }
    if choice["finish_reason"] == "length" {
        return Err(truncated_at(max_tokens));
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
    use crate::model_service::{DEFAULT_TOKEN_CEILING, MAX_TOKEN_CEILING, MIN_TOKEN_CEILING};
    use crate::settings::OPENAI_COMMAND_REASONING_EFFORT;
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
            TokenCeiling::default(),
            // The default body carries NO reasoning parameter, which is the
            // case every assertion below but one is about: the preset decides
            // to send it, this function never does.
            None,
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
                "close",
                "open_settings",
                "voice_off",
                "list_commands",
                "dictate_to_agent",
                "submit_prompt",
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
        assert_eq!(
            schema["properties"]["params"]["properties"]["prefix"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            schema["properties"]["params"]["required"],
            json!(["agent", "prefix"])
        );
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
        assert_eq!(body["max_completion_tokens"], DEFAULT_TOKEN_CEILING);
        assert!(body["max_tokens"].is_null(), "{body}");
    }

    /// Scenario: the ceiling in the request body is the one the settings carry,
    /// and it is spelled `max_completion_tokens` at every value.
    ///
    /// The defect this field exists for is specific to this dialect:
    /// `max_completion_tokens` counts reasoning tokens, so a hardwired 256 made
    /// every reasoning model return `finish_reason: "length"` with nothing
    /// written. A configured ceiling that did not reach the wire would leave
    /// that exactly as it was.
    #[test]
    fn voice_openai_request_carries_the_configured_ceiling() {
        let commands = commands();
        let agents = vec![agent("1", "tester")];
        let transcript = Transcript::new("show me the tester");
        let request = IntentRequest {
            transcript: &transcript,
            commands: &commands,
            agents: &agents,
        };
        for ceiling in [MIN_TOKEN_CEILING, 1024, MAX_TOKEN_CEILING] {
            let ceiling = TokenCeiling::parse(i64::from(ceiling)).expect("in range");
            let body = request_body(&request, "a-model", ceiling, None);
            assert_eq!(body["max_completion_tokens"], ceiling.get());
            assert!(body["max_tokens"].is_null(), "{body}");
        }
    }

    /// Scenario: the body carries `reasoning_effort` when the caller hands one
    /// over, and does not mention the key at all when it does not.
    ///
    /// **"Does not mention" is the assertion, not "sends null".** A server that
    /// has never heard of this field should see a body without it; a `null` is
    /// still an unknown key to a strict parser, which is the failure this
    /// scoping exists to avoid. `IntentSettings::reasoning_effort` is what
    /// decides, and `resolver.rs` pins the deciding.
    #[test]
    fn voice_openai_request_carries_the_reasoning_parameter_only_when_handed_one() {
        let commands = commands();
        let agents = vec![agent("1", "tester")];
        let transcript = Transcript::new("show me the tester");
        let request = IntentRequest {
            transcript: &transcript,
            commands: &commands,
            agents: &agents,
        };

        let bare = request_body(&request, "a-model", TokenCeiling::default(), None);
        assert!(
            !bare
                .as_object()
                .expect("an object")
                .contains_key("reasoning_effort"),
            "an absent parameter must not appear as a null: {bare}"
        );

        let effort = request_body(
            &request,
            "gpt-5-mini",
            TokenCeiling::default(),
            Some(OPENAI_COMMAND_REASONING_EFFORT),
        );
        assert_eq!(effort["reasoning_effort"], "minimal");
        // And it changes nothing else about the body.
        assert_eq!(effort["response_format"], bare["response_format"]);
        assert_eq!(effort["messages"], bare["messages"]);
    }

    #[test]
    fn voice_openai_request_never_carries_a_rows_report_wording() {
        let rendered = body("show me the tester").to_string();
        for row in table().rows() {
            assert!(!rendered.contains(&row.report), "`{}` reached it", row.id);
        }
    }

    // -- the response shape ------------------------------------------------

    /// The parse under this build's default ceiling.
    ///
    /// Every case below is about the CONTENT of the reply, so the ceiling is a
    /// constant here; the one test that is about the ceiling passes its own.
    fn parse(payload: &Value) -> Result<IntentAnswer, IntentError> {
        parse_response(payload, TokenCeiling::default())
    }

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
        let answer = parse(&reply(
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
        let answer =
            parse(&reply(r#"{"action":"open_deck","params":{"agent":null}}"#)).expect("parses");
        assert_eq!(answer.action, "open_deck");
        assert!(
            answer.params.is_empty(),
            "a null param must arrive as absent, not as an empty match: {:?}",
            answer.params
        );
    }

    #[test]
    fn voice_openai_parses_the_none_escape() {
        let answer = parse(&reply(r#"{"action":"none","params":{"agent":null}}"#)).expect("parses");
        assert!(answer.is_no_match());
    }

    #[test]
    fn voice_openai_reads_an_answer_a_server_fenced_anyway() {
        // Under the nested envelope the content is grammar-enforced and this
        // does not arise. It arises on a server that took the envelope and did
        // not enforce it, which is a real shape — and a fenced but correct
        // answer is not a reason to show a failure sentence.
        let answer = parse(&reply(
            "```json\n{\"action\":\"open_overview\",\"params\":{\"agent\":null}}\n```",
        ))
        .expect("parses");
        assert_eq!(answer.action, "open_overview");
    }

    #[test]
    fn voice_openai_keeps_an_action_outside_the_table() {
        // Not this function's refusal to make — `handle_utterance` turns it
        // into UnknownAction, which says something different.
        let answer = parse(&reply(r#"{"action":"launch_missiles","params":{}}"#)).expect("parses");
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
        let error = parse(&payload).expect_err("fails");
        assert!(error.detail().contains("declined"), "{error}");
    }

    /// Scenario: the reply comes back `finish_reason: "length"`. The failure
    /// sentence names the ceiling that cut it off and the settings row that
    /// changes it.
    ///
    /// **The sentence used to name neither**, and was honest about it: the
    /// ceiling was a `const`, so there was nothing to point at. It is a
    /// settings field now, which makes *"cut off before it finished"* a
    /// withheld remedy rather than a complete answer. Naming the CURRENT value
    /// is the half that carries information — it is what tells the user whether
    /// raising it is plausible or whether the model is looping.
    #[test]
    fn voice_openai_reports_an_answer_cut_off_at_the_ceiling() {
        let payload = json!({
            "choices": [{
                "finish_reason": "length",
                "message": { "role": "assistant", "content": "{\"action\":\"open_ag" },
            }],
        });
        let error = parse(&payload).expect_err("fails");
        assert_eq!(
            error.detail(),
            format!(
                "the command backend's answer was cut off at its {DEFAULT_TOKEN_CEILING}-token \
                 ceiling — raise Max tokens under Settings → Voice"
            ),
        );

        // The number is the CONFIGURED one, not a constant this file owns —
        // which is the whole difference between a remedy and a restatement.
        let error = parse_response(&payload, TokenCeiling::parse(512).expect("in range"))
            .expect_err("fails");
        assert!(error.detail().contains("512-token ceiling"), "{error}");
        assert!(error.detail().contains("Max tokens"), "{error}");
    }

    #[test]
    fn voice_openai_reports_a_reply_with_no_content() {
        let payload = json!({ "choices": [] });
        let error = parse(&payload).expect_err("fails");
        assert!(
            error.detail().contains("without picking an action"),
            "{error}"
        );
    }

    #[test]
    fn voice_openai_reports_unreadable_content() {
        let error = parse(&reply("I think you want the tester.")).expect_err("fails");
        assert!(error.detail().contains("could not read"), "{error}");
        let error = parse(&reply(r#"{"action":7}"#)).expect_err("fails");
        assert!(error.detail().contains("could not read"), "{error}");
    }
}
