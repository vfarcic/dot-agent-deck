//! The keyed remote intent backend: one HTTPS request, tool-use, a constrained
//! enum.
//!
//! This was the answer to PRD #802's own risk entry — *the default intent
//! backend is slow enough to feel broken*. Measured against the same command
//! table on 2026-09-18, this backend answers in a **median of 0.91 s** where
//! the agent-CLI backend it has since replaced took **3.1–4.7 s**. That
//! backend is gone (PRD #802's provider work; [`crate::settings::IntentBackend`]
//! has the decision), so this is not the fast one of two any more — it is the
//! only one, and the numbers below are kept because they are what the choice of
//! request shape was made against.
//!
//! # PRD #802 Open Question 4, answered: tool-use with a constrained enum
//!
//! The question was whether to use the tool-use API, which gives a genuinely
//! constrained enum, or a plain constrained-JSON prompt, which is less code —
//! and the PRD said to decide against a measured latency and prefer the
//! constrained shape if the latency is comparable. It is comparable, so the
//! constrained shape wins. Four runs of each of four utterances against
//! `claude-haiku-4-5`, wall clock on this box:
//!
//! | shape | min | median | max | correct |
//! | --- | --- | --- | --- | --- |
//! | tool-use, `strict: true` | 0.81 s | **0.91 s** | 1.39 s | 8/8 |
//! | tool-use, no `strict` | 0.66 s | 0.76 s | 0.93 s | 8/8 |
//! | plain constrained JSON | 0.63 s | 0.75 s | 1.37 s | 8/8 |
//!
//! The constrained shape costs **~0.16 s** against the unconstrained one, on a
//! path whose alternative backend costs 3.1–4.7 s. It buys a guarantee the
//! prompt cannot give: under `strict: true` the API validates the arguments
//! against the schema, so an action outside the enum is not representable
//! rather than merely unlikely. That is the distinction PRD #802 draws between
//! this and grammar-constrained decoding, and this is the closest a hosted API
//! gets to it.
//!
//! **The one cost measured and not assumed: a NEW schema pays a one-time
//! compilation.** A novel strict schema took 1.81 s then 1.12 s; the same
//! schema afterwards took 0.91 s and 0.85 s, and a novel *non-strict* schema
//! took 0.84 s — so the ~0.9 s is the strict compile and not a cold connection.
//! It is cached for 24 hours. The user-visible shape of that is one slower
//! utterance after `commands.toml` changes or after a day of not using voice,
//! which M6 renders honestly like any other latency.
//!
//! # The model: `claude-haiku-4-5`
//!
//! The cheapest current tier at **$1.00 / $5.00 per 1M tokens**, and the right
//! one rather than merely the cheap one: this is a short, closed-set,
//! latency-sensitive classification with no reasoning to do, which is exactly
//! the workload that tier is for — and it answered all eight measured cases
//! correctly, including the `none` escape for *"what time is it"*. At the ~1280
//! input / ~40 output tokens one request measured, an utterance costs about
//! **$0.0015**, against the deleted agent-CLI backend's $0.0036–$0.0126 (whose
//! price was that CLI's own session context, not the task).
//!
//! No `thinking` parameter is sent: on this model thinking is off unless asked
//! for, and a routing decision this small does not want it.
//!
//! **It was a constant and is now the PRESET of a settings field**, which is
//! the change PRD #802's provider work made. The old reasoning — that having
//! answered Open Question 4, a field would offer a choice whose only correct
//! value this file knew — was true of the measurement and false of the product:
//! a user cannot know which key to paste when the endpoint is a secret of the
//! build's, and cannot use a provider this build did not pick.
//!
//! # Why the request is made here and not in the webview
//!
//! `tauri.conf.json`'s CSP declares `connect-src ipc: http://ipc.localhost` and
//! nothing else, so the frontend cannot reach a network origin and read the
//! reply. PRD #802 records this as simplifying rather than awkward: every
//! network hop is Rust-side, which is also where the credential already is.
//!
//! # The credential never crosses into the webview
//!
//! It is read from [`SecretStore`] **at call time**, in this process, and goes
//! straight into a request header. There is deliberately no IPC command that
//! reads a secret back — `desktop_secret_status`, `…_store` and `…_forget`
//! exist and `load` does not — because a credential arriving in the webview is
//! one `JSON.stringify` from the `localStorage` half of PRD #803's rule. Read
//! at call time rather than cached at construction so a key stored in settings
//! works on the next utterance instead of after a restart.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::model_service::{ModelId, ServiceUrl};
use crate::secrets::{SecretId, SecretStore, load_off_runtime};

use super::prompt::{action_enum, param_names, state};
use super::resolver::{IntentAnswer, IntentError, IntentRequest, IntentResolver, ResolveFuture};
use super::schema::{AnnotatedCommand, TOOL_INSTRUCTIONS, TOOL_NAME};

/// The API version header this request shape was measured against.
const API_VERSION: &str = "2023-06-01";

/// A ceiling on the answer, not a budget for it.
///
/// The measured replies were 33–58 output tokens. 256 leaves an order of
/// magnitude of headroom while making a runaway generation impossible — and a
/// truncated tool call would be an unreadable answer, which is an `Err`.
const MAX_TOKENS: u32 = 256;

/// How long the request gets before the attempt is abandoned.
///
/// Measured at 0.63–1.81 s including the one-time strict-schema compile, so
/// this is roughly eight times the slowest. A backstop against a hung
/// connection, not a latency budget — a slow answer is better than a failure
/// sentence for a user who has already waited.
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(15);

/// Resolve intent by asking a hosted model with a key of the app's own.
pub struct RemoteResolver {
    secrets: Arc<dyn SecretStore>,
    /// `None` when no client could be built, which this backend reports as a
    /// failure rather than papering over — see [`super::http::client`].
    client: Option<reqwest::Client>,
    endpoint: ServiceUrl,
    model: ModelId,
}

impl RemoteResolver {
    /// Both coordinates are arguments rather than constants, which is the
    /// change PRD #802's provider work made here: the model used to be a
    /// `const` with a doc comment explaining that a settings field would offer
    /// a choice whose only correct value this file knew. That was true of the
    /// measurement and false of the product — a user cannot know which key to
    /// paste when the endpoint is a secret of the build's, and cannot use a
    /// provider this build did not pick.
    pub fn new(secrets: Arc<dyn SecretStore>, endpoint: ServiceUrl, model: ModelId) -> Self {
        Self {
            secrets,
            // Built once and reused, which is how `reqwest` is meant to be
            // used: the connection pool and the TLS session cache live on the
            // client, and rebuilding one per utterance would pay a fresh
            // handshake on the path this backend exists to make fast.
            //
            // The TIMEOUT is still set per REQUEST rather than on the builder,
            // and that reasoning is unchanged: `ClientBuilder::build` is
            // fallible, and `.timeout(..).build().unwrap_or_default()` silently
            // yields a client with no timeout on the failure path — precisely
            // the bound this backend must not lose, so it goes somewhere that
            // cannot fail to be applied.
            //
            // What the builder now carries is the REDIRECT policy, which has no
            // per-request spelling. PRD #802's audit found `x-api-key` — a
            // custom header, so not one reqwest strips — being forwarded across
            // an origin change. `super::http::client` refuses to follow one at
            // all, and hands back `None` rather than a permissive fallback if
            // it cannot be built; `run` turns that into a sentence.
            client: super::http::client(),
            endpoint,
            model,
        }
    }

    async fn run(&self, request: IntentRequest<'_>) -> Result<IntentAnswer, IntentError> {
        // Read at call time, Rust-side, and dropped with this scope — and on
        // a blocking thread, because a keychain read is entitled to prompt and
        // this is an async fn on a shared runtime.
        let secret = match load_off_runtime(Arc::clone(&self.secrets), SecretId::VoiceIntent).await
        {
            Ok(Some(secret)) => secret,
            Ok(None) => {
                return Err(IntentError::NotConfigured(format!(
                    "no key is stored for {} — add one in Settings → Voice",
                    self.endpoint.host()
                )));
            }
            // The keychain itself failed. `SecretError::public` is already a
            // complete sentence naming what did not happen, which is the whole
            // point of PRD #802 M4's refusal to report a failed read as
            // "nothing stored".
            Err(error) => return Err(IntentError::NotConfigured(error.public())),
        };

        let Some(client) = self.client.as_ref() else {
            // Fail closed: the one thing this must never do is fall back to a
            // client that follows redirects with the key attached.
            return Err(IntentError::Backend(
                "the command backend could not start a secure connection".into(),
            ));
        };

        let body = request_body(&request, self.model.as_str());
        let response = client
            .post(self.endpoint.as_str())
            .timeout(REMOTE_TIMEOUT)
            .header("content-type", "application/json")
            .header("anthropic-version", API_VERSION)
            .header("x-api-key", secret.expose())
            .json(&body)
            .send()
            .await
            .map_err(|error| IntentError::Backend(transport_detail(&error)))?;

        let status = response.status();
        // Bounded BEFORE the bytes become text or JSON. `Response::json` used
        // to collect the whole body first, so a body streamed fast enough could
        // exhaust this process inside the timeout — PRD #802's audit. The bound
        // applies to the non-success path too, because the error detail below
        // is quoted out of that same body.
        let body = match super::http::capped_body(response, super::http::MAX_BODY_BYTES).await {
            Ok(body) => body,
            Err(super::http::BodyError::TooLarge) => {
                return Err(IntentError::Backend(format!(
                    "the command backend answered {status} with more than {} bytes",
                    super::http::MAX_BODY_BYTES
                )));
            }
            Err(super::http::BodyError::Transport) => {
                return Err(IntentError::Backend(
                    "the request to the command backend failed".into(),
                ));
            }
        };
        let payload: Value = serde_json::from_slice(&body).map_err(|_| {
            IntentError::Backend(format!("the command backend answered {status} unreadably"))
        })?;
        if !status.is_success() {
            return Err(IntentError::Backend(api_error_detail(status, &payload)));
        }
        parse_response(&payload)
    }
}

impl IntentResolver for RemoteResolver {
    fn resolve<'a>(&'a self, request: IntentRequest<'a>) -> ResolveFuture<'a> {
        Box::pin(self.run(request))
    }

    fn backend_name(&self) -> &'static str {
        "remote"
    }
}

/// The tool definition: one tool, `action` as an enum, `strict: true`.
///
/// **The params object enumerates its properties rather than being an open map
/// of strings**, and that is what `strict: true` costs. A strict schema may not
/// carry `additionalProperties` set to anything but `false`, so
/// `{"type":"object","additionalProperties":{"type":"string"}}` — which is what
/// [`super::schema::tool_schema`] uses for the app's own shape — is not
/// expressible here. The table is what makes the constrained shape reachable at
/// all: it declares every param name statically, so
/// [`super::prompt::param_names`] can enumerate them, and a row that adds a
/// param adds a property without anyone editing **the code in** this file. That
/// is the no-implementation promise holding on a path that could easily have
/// broken it — and the emphasis is M8's, which measured the difference: adding
/// `open_settings` DID edit this file, in one `mod tests` assertion pinning the
/// request body by value. The generator did not move; the pinning test did,
/// which is what a pinning test is for.
///
/// Every property is optional — only `action` is `required` — because a
/// paramless command supplies none, and the model must be able to answer
/// `none` with an empty object. A missing param is
/// [`super::VoiceOutcome::ParamMissing`], which is a sentence the app renders,
/// not a schema violation.
pub fn tool_definition(commands: &[AnnotatedCommand]) -> Value {
    let properties: serde_json::Map<String, Value> = param_names(commands)
        .into_iter()
        .map(|name| (name, json!({ "type": "string" })))
        .collect();
    json!({
        "name": TOOL_NAME,
        "description": TOOL_INSTRUCTIONS,
        "strict": true,
        "input_schema": {
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
    — or the agent's `label`, for a reference the user made by state. The app resolves each one \
    against live state.",
                    "properties": properties,
                    "additionalProperties": false,
                },
            },
            "required": ["action"],
            "additionalProperties": false,
        },
    })
}

/// The whole request body.
///
/// `tool_choice` names the tool rather than being `auto`: there is exactly one
/// tool and exactly one thing to do with this turn, and a model that answered
/// in prose would be an unreadable answer. (Forced tool choice is rejected by
/// some newer models and accepted by this one; the model is a SETTING since
/// PRD #802's provider work, so that is a fact about the preset rather than
/// about every value the field can hold — a user who points this at a model
/// that refuses a forced tool choice gets an unreadable-answer error, which is
/// the honest failure for a coordinate they chose.)
///
/// The state goes in `system` and the utterance in the one user turn, which is
/// the split that keeps the volatile half last: the commands and the agent list
/// are the same across consecutive utterances and the transcript is not.
pub fn request_body(request: &IntentRequest<'_>, model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "system": format!(
            "You route ONE spoken utterance to ONE action in a desktop app.\n\nState:\n{}",
            state(request)
        ),
        "tools": [tool_definition(request.commands)],
        "tool_choice": { "type": "tool", "name": TOOL_NAME },
        "messages": [{ "role": "user", "content": request.transcript.text() }],
    })
}

/// The answer, out of the one `tool_use` block.
///
/// A `refusal` stop reason is reported as its own failure rather than falling
/// through to "no tool call": the two have different remedies, and a user whose
/// utterance was declined should not be told the backend is broken.
pub fn parse_response(payload: &Value) -> Result<IntentAnswer, IntentError> {
    if payload["stop_reason"] == "refusal" {
        return Err(IntentError::Backend(
            "the command backend declined to answer that".into(),
        ));
    }
    let block = payload["content"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|block| block["type"] == "tool_use" && block["name"] == TOOL_NAME)
        .ok_or_else(|| {
            IntentError::Backend("the command backend answered without picking an action".into())
        })?;
    serde_json::from_value::<IntentAnswer>(block["input"].clone()).map_err(|_| {
        IntentError::Backend(
            "the command backend picked an action this build could not read".into(),
        )
    })
}

/// What a non-2xx reply becomes.
///
/// The API's own `error.message` is quoted because it is the difference between
/// "your key is wrong" and "you are rate limited", which the user must be able
/// to act on. It goes through the same scrubbing as every other backend detail
/// at the render seam ([`super::outcome`]); the status is always included so a
/// reply with no readable body still says something.
fn api_error_detail(status: reqwest::StatusCode, payload: &Value) -> String {
    match payload["error"]["message"].as_str() {
        Some(message) if !message.trim().is_empty() => {
            format!("the command backend refused ({status}): {}", message.trim())
        }
        _ => format!("the command backend refused ({status})"),
    }
}

/// What a transport failure becomes.
///
/// Classified rather than stringified, because `reqwest`'s own `Display`
/// carries the URL and a chain of source errors — which is debugging output, not
/// a sentence for someone who just pressed a button.
fn transport_detail(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        format!(
            "the command backend did not answer within {}s",
            REMOTE_TIMEOUT.as_secs()
        )
    } else if error.is_connect() {
        "the command backend could not be reached".to_string()
    } else {
        "the request to the command backend failed".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{MemorySecretStore, Secret, SecretErrorKind, ThreadRecordingStore};
    use crate::settings::HOSTED_COMMAND_MODEL;
    use crate::voice::Transcript;
    use crate::voice::fixtures::role_agent as agent;
    use crate::voice::schema::annotate;
    use crate::voice::table::{Screen, table};

    // Every test here is pure: a `Value` in and a `Value` out. PRD #802 M5's
    // rule is that nothing in the merge-blocking tier needs a credential or the
    // network, and the strongest form of that is a suite that opens no socket
    // at all — not one that opens a loopback one. What the request and the
    // reply look like on the wire is exactly what these assert; the transport
    // itself belongs to M9's credentialed lane.

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
            HOSTED_COMMAND_MODEL,
        )
    }

    /// A keyed resolver pointed somewhere unroutable on purpose: reaching it
    /// would be a failure of the test's premise, not a flake.
    fn resolver(secrets: Arc<dyn SecretStore>) -> RemoteResolver {
        RemoteResolver::new(
            secrets,
            ServiceUrl::parse("https://voice-intent.invalid/never").expect("valid"),
            ModelId::parse(HOSTED_COMMAND_MODEL).expect("valid"),
        )
    }

    // -- the request shape -------------------------------------------------

    #[test]
    fn voice_remote_request_pins_the_cheap_fast_tier_and_bounds_the_answer() {
        let body = body("show me the tester");
        assert_eq!(body["model"], "claude-haiku-4-5");
        assert_eq!(body["max_tokens"], 256);
        // A routing decision this small does not want thinking, and this model
        // does none unless asked.
        assert!(body["thinking"].is_null());
    }

    #[test]
    fn voice_remote_request_constrains_the_action_to_the_table_plus_the_escape() {
        let body = body("show me the tester");
        let tool = &body["tools"][0];
        assert_eq!(tool["name"], TOOL_NAME);
        assert_eq!(tool["strict"], true);
        let actions: Vec<&str> = tool["input_schema"]["properties"]["action"]["enum"]
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
                "none"
            ]
        );
    }

    #[test]
    fn voice_remote_request_forces_the_one_tool() {
        let body = body("show me the tester");
        assert_eq!(body["tool_choice"]["type"], "tool");
        assert_eq!(body["tool_choice"]["name"], TOOL_NAME);
        assert_eq!(body["tools"].as_array().expect("an array").len(), 1);
    }

    #[test]
    fn voice_remote_request_schema_is_strict_legal_everywhere() {
        // `strict: true` refuses `additionalProperties` set to anything but
        // `false`, ANYWHERE in the schema. This is the assertion that catches a
        // future param map being reintroduced as an open one — which would be
        // a 400 at run time and silence here otherwise.
        let body = body("show me the tester");
        let schema = &body["tools"][0]["input_schema"];
        fn walk(value: &Value, path: &str) {
            if let Some(object) = value.as_object() {
                if object.get("type").and_then(Value::as_str) == Some("object") {
                    assert_eq!(
                        object.get("additionalProperties"),
                        Some(&Value::Bool(false)),
                        "{path} is an object without `additionalProperties: false`"
                    );
                }
                for (key, child) in object {
                    walk(child, &format!("{path}.{key}"));
                }
            }
        }
        walk(schema, "input_schema");
        assert_eq!(schema["required"], json!(["action"]));
    }

    #[test]
    fn voice_remote_request_enumerates_the_tables_params_rather_than_an_open_map() {
        let body = body("show me the tester");
        let params = &body["tools"][0]["input_schema"]["properties"]["params"];
        assert_eq!(params["properties"]["agent"]["type"], "string");
        assert_eq!(params["additionalProperties"], false);
        // Optional: a paramless command supplies none, and `none` supplies an
        // empty object.
        assert!(params["required"].is_null());
    }

    #[test]
    fn voice_remote_request_puts_the_state_in_system_and_the_utterance_last() {
        let body = body("show me the tester");
        let system = body["system"].as_str().expect("a string");
        assert!(system.contains("open_agent"), "{system}");
        assert!(system.contains("tester"), "{system}");
        // The volatile half is the user turn, so consecutive utterances share a
        // prefix rather than differing at the front of it.
        assert!(!system.contains("show me the tester"), "{system}");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "show me the tester");
    }

    #[test]
    fn voice_remote_request_never_carries_a_rows_report_wording() {
        let rendered = body("show me the tester").to_string();
        for row in table().rows() {
            assert!(!rendered.contains(&row.report), "`{}` reached it", row.id);
        }
    }

    // -- the response shape ------------------------------------------------

    fn reply(input: Value) -> Value {
        json!({
            "stop_reason": "tool_use",
            "content": [{
                "type": "tool_use",
                "id": "toolu_abc123",
                "name": TOOL_NAME,
                "input": input,
            }],
        })
    }

    #[test]
    fn voice_remote_parses_a_tool_use_answer() {
        let answer = parse_response(&reply(
            json!({"action": "open_agent", "params": {"agent": "tester"}}),
        ))
        .expect("parses");
        assert_eq!(answer.action, "open_agent");
        assert_eq!(answer.params.get("agent"), Some(&"tester".to_string()));
    }

    #[test]
    fn voice_remote_parses_the_none_escape() {
        let answer = parse_response(&reply(json!({"action": "none"}))).expect("parses");
        assert!(answer.is_no_match());
    }

    #[test]
    fn voice_remote_skips_a_text_block_before_the_tool_call() {
        let payload = json!({
            "stop_reason": "tool_use",
            "content": [
                {"type": "text", "text": "Let me route that."},
                {"type": "tool_use", "name": TOOL_NAME, "input": {"action": "open_deck"}},
            ],
        });
        assert_eq!(
            parse_response(&payload).expect("parses").action,
            "open_deck"
        );
    }

    #[test]
    fn voice_remote_keeps_an_action_outside_the_table() {
        // Not this function's refusal to make — `handle_utterance` turns it
        // into UnknownAction, which says something different.
        let answer = parse_response(&reply(json!({"action": "launch_missiles"}))).expect("parses");
        assert_eq!(answer.action, "launch_missiles");
    }

    #[test]
    fn voice_remote_reports_a_reply_with_no_tool_call() {
        let payload = json!({
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "I think you want the tester."}],
        });
        let error = parse_response(&payload).expect_err("fails");
        assert!(
            error.detail().contains("without picking an action"),
            "{error}"
        );
    }

    #[test]
    fn voice_remote_reports_a_refusal_as_its_own_failure() {
        let payload = json!({"stop_reason": "refusal", "content": []});
        let error = parse_response(&payload).expect_err("fails");
        assert!(error.detail().contains("declined"), "{error}");
    }

    #[test]
    fn voice_remote_reports_an_unreadable_tool_input() {
        let error = parse_response(&reply(json!({"action": 7}))).expect_err("fails");
        assert!(error.detail().contains("could not read"), "{error}");
        let error = parse_response(&reply(json!({}))).expect_err("fails");
        assert!(error.detail().contains("could not read"), "{error}");
    }

    #[test]
    fn voice_remote_reports_an_api_error_with_its_message() {
        let detail = api_error_detail(
            reqwest::StatusCode::UNAUTHORIZED,
            &json!({"error": {"type": "authentication_error", "message": "invalid x-api-key"}}),
        );
        assert!(detail.contains("401"), "{detail}");
        assert!(detail.contains("invalid x-api-key"), "{detail}");
    }

    #[test]
    fn voice_remote_reports_an_api_error_with_no_readable_body() {
        let detail = api_error_detail(reqwest::StatusCode::BAD_GATEWAY, &json!({}));
        assert!(detail.contains("502"), "{detail}");
    }

    // -- the credential ----------------------------------------------------

    #[tokio::test]
    async fn voice_remote_without_a_key_is_not_configured_and_opens_no_socket() {
        // The endpoint is unroutable on purpose: reaching it would be a
        // failure of this test's premise, not a flake.
        let resolver = resolver(Arc::new(MemorySecretStore::new()));
        let commands = commands();
        let transcript = Transcript::new("show me the tester");
        let error = resolver
            .resolve(IntentRequest {
                transcript: &transcript,
                commands: &commands,
                agents: &[],
            })
            .await
            .expect_err("fails");
        assert!(
            matches!(&error, IntentError::NotConfigured(detail) if detail.contains("no key is stored")),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn voice_remote_reports_an_unreachable_keychain_rather_than_a_missing_key() {
        // PRD #802 M4's rule, one layer up: "I could not find out" must not
        // render as "nothing stored", or the user retypes their key into a
        // store that cannot hold it.
        let store = MemorySecretStore::failing(SecretErrorKind::Unavailable, "test");
        let resolver = resolver(Arc::new(store));
        let commands = commands();
        let transcript = Transcript::new("show me the tester");
        let error = resolver
            .resolve(IntentRequest {
                transcript: &transcript,
                commands: &commands,
                agents: &[],
            })
            .await
            .expect_err("fails");
        assert!(
            matches!(&error, IntentError::NotConfigured(_)),
            "got {error:?}"
        );
        assert!(!error.detail().contains("no key is stored"), "{error}");
    }

    /// Scenario: resolve one utterance and note which thread the keychain was
    /// read on. It is not the one the resolver is running on.
    ///
    /// The regression: `run` called `SecretStore::load` directly inside its
    /// own `async fn`, so a keychain entitled to raise an unlock prompt parked
    /// a shared runtime worker for as long as the prompt stayed on screen.
    /// Invisible in the answer, which is why it is asserted as an identity.
    #[tokio::test]
    async fn voice_remote_reads_the_keychain_off_the_runtime() {
        let store = Arc::new(ThreadRecordingStore::new());
        let resolver = resolver(Arc::clone(&store) as Arc<dyn SecretStore>);
        let commands = commands();
        let transcript = Transcript::new("show me the tester");
        let error = resolver
            .resolve(IntentRequest {
                transcript: &transcript,
                commands: &commands,
                agents: &[],
            })
            .await
            .expect_err("no key is stored");
        assert!(
            matches!(&error, IntentError::NotConfigured(detail) if detail.contains("no key is stored")),
            "got {error:?}"
        );
        assert_ne!(
            store.read_on().expect("the store was read"),
            std::thread::current().id(),
            "the keychain read ran on the runtime thread resolving the utterance"
        );
    }

    #[test]
    fn voice_remote_never_puts_the_credential_in_the_body() {
        // The key travels in a header and nowhere else. A body carrying it
        // would reach any logging or error path that prints a request.
        let store = MemorySecretStore::new();
        store
            .store(
                SecretId::VoiceIntent,
                &Secret::new("sk-not-a-real-key-0123"),
            )
            .expect("stores");
        let rendered = body("show me the tester").to_string();
        assert!(!rendered.contains("sk-not-a-real-key"), "{rendered}");
        assert!(!rendered.contains("x-api-key"), "{rendered}");
    }

    #[test]
    fn voice_remote_names_itself_for_the_surface() {
        assert_eq!(
            resolver(Arc::new(MemorySecretStore::new())).backend_name(),
            "remote"
        );
    }

    #[test]
    fn voice_remote_transport_failures_carry_no_debugging_output() {
        // `reqwest::Error`'s own Display carries the URL and a source chain.
        // What reaches a sentence is this file's wording.
        assert!(transport_detail_is_clean(&timeout_error()));
    }

    fn timeout_error() -> reqwest::Error {
        // The only way to obtain a typed `reqwest::Error` without a socket is
        // to let the builder produce one; a malformed URL yields `is_builder`,
        // which lands in the catch-all arm — the arm most likely to leak.
        reqwest::Client::new()
            .post("not a url")
            .build()
            .expect_err("a builder error")
    }

    fn transport_detail_is_clean(error: &reqwest::Error) -> bool {
        let detail = transport_detail(error);
        detail == "the request to the command backend failed"
    }
}
