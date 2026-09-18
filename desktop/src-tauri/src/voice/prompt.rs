//! What both M5 backends send, and how both read the answer back.
//!
//! The two backends are as different as two backends get — one spawns a
//! pre-authenticated CLI and reads its stdout, the other makes an HTTPS request
//! with a key of the app's own — and they still agree on three things, which is
//! why those three live here rather than twice:
//!
//! - **the action enum**, every row id plus the `none` escape;
//! - **the state the model is given**, the annotated command list and the
//!   agents named the way the deck names them;
//! - **the shape of the answer**, [`IntentAnswer`], and the tolerant reader
//!   that recovers one from output that was not promised to be clean.
//!
//! Everything a backend does not share — the envelope, the transport, the
//! timeout, the failure wording — stays in the backend.

use serde_json::{Value, json};

use super::outcome::display_label;
use super::resolver::{IntentAnswer, IntentRequest};
use super::schema::AnnotatedCommand;
use super::table::NO_MATCH_ACTION;

/// How many `{` candidates [`extract_answer`] will try before giving up.
///
/// The scan is quadratic in the worst case — every candidate re-parses from its
/// own offset — so it needs a bound, and a real answer is found at the first or
/// second candidate. A backend that has emitted 64 opening braces without one
/// of them starting an [`IntentAnswer`] is not about to.
const MAX_JSON_CANDIDATES: usize = 64;

/// How much of a backend's output is scanned for that JSON object.
///
/// The answer is a few dozen bytes and arrives at the end of a short reply, so
/// this is generous by three orders of magnitude while keeping the scan's cost
/// independent of how much a confused CLI decided to print.
const MAX_SCAN_BYTES: usize = 64 * 1024;

/// Why an answer could not be recovered from a backend's output.
///
/// Two reasons and not one, because they mean different things to whoever
/// reads the sentence: a backend that printed nothing is usually not installed,
/// not authenticated, or was killed, while a backend that printed something
/// unreadable ran and did not honour the format. Both are
/// [`super::VoiceOutcome::ResolutionFailed`]; the caller prefixes its own name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractError {
    /// Nothing usable on the stream at all.
    Empty,
    /// Output arrived and held no JSON object this build could read as an
    /// answer.
    NoAnswer,
}

impl ExtractError {
    /// The clause a backend appends to its own name.
    pub fn reason(self) -> &'static str {
        match self {
            ExtractError::Empty => "produced no output",
            ExtractError::NoAnswer => "produced no answer this build could read",
        }
    }
}

/// The `action` enum for one request: every command's id, plus the escape.
///
/// Derived from the annotated list rather than from the table, because the
/// annotated list is what a backend is handed — so the enum a backend
/// constrains on and the list it was shown cannot disagree.
///
/// **Not narrowed to the callable rows**, for [`super::schema::action_enum`]'s
/// reason: a model that picks an unavailable command is the case the hint
/// exists to explain, so it has to be able to name one.
pub fn action_enum(commands: &[AnnotatedCommand]) -> Vec<String> {
    commands
        .iter()
        .map(|command| command.id.clone())
        .chain(std::iter::once(NO_MATCH_ACTION.to_string()))
        .collect()
}

/// Every param name any command declares, deduplicated, in table order.
///
/// The keyed remote backend needs this because a schema under `strict: true`
/// may not carry `additionalProperties` set to anything but `false` — so the
/// params object has to enumerate its properties rather than being an open map
/// of strings. That the table can supply them is what makes the constrained
/// shape reachable at all; see [`super::remote`].
pub fn param_names(commands: &[AnnotatedCommand]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for command in commands {
        for param in &command.params {
            if !names.contains(&param.name) {
                names.push(param.name.clone());
            }
        }
    }
    names
}

/// The live state the model is given, as one JSON value.
///
/// Two keys: the annotated commands verbatim, and the agents **named the way
/// the deck names them**. The labels go through
/// [`super::outcome::display_label`] rather than being derived here, so the
/// name the model is shown is the name the app will match a spoken reference
/// against — one derivation, not two that can drift.
///
/// Ids are deliberately absent. A backend answers with a param *as the user
/// referred to it* and the app resolves it; handing over ids would invite a
/// backend to assert that an agent exists, which is the app's job.
pub fn state(request: &IntentRequest<'_>) -> Value {
    json!({
        "commands": request.commands,
        "agents_on_screen": request
            .agents
            .iter()
            .map(|agent| display_label(agent, request.agents))
            .collect::<Vec<_>>(),
    })
}

/// The whole prompt for a backend with no tool-use envelope to put it in.
///
/// One string: the instructions, the state, the answer format, and the
/// utterance last. The agent-CLI backend passes it as a single argv element —
/// so there is no shell and nothing to quote — and the keyed remote backend
/// uses [`state`] and the tool schema instead, because it has a better place to
/// put each piece.
///
/// **The utterance is untrusted text and goes in last, labelled.** It cannot
/// reach a shell, and what it can do to the *model* is bounded by everything
/// downstream: the answer is validated against the table, an action outside it
/// becomes [`super::VoiceOutcome::UnknownAction`], and no free text this
/// function produces can dispatch anything. A successful injection buys the
/// attacker one navigation the user could have performed by clicking.
pub fn cli_prompt(request: &IntentRequest<'_>) -> String {
    let actions = action_enum(request.commands).join(", ");
    format!(
        "You route ONE spoken utterance to ONE action in a desktop app.\n\n\
         {instructions}\n\n\
         State:\n{state}\n\n\
         Reply with ONE JSON object and nothing else — no prose, no markdown \
         fence, no explanation:\n\
         {{\"action\": \"<one of: {actions}>\", \"params\": {{\"<name>\": \"<value>\"}}}}\n\n\
         Utterance: {utterance}",
        instructions = super::schema::TOOL_INSTRUCTIONS,
        state = state(request),
        utterance = request.transcript.text(),
    )
}

/// Recover an [`IntentAnswer`] from output that was not promised to be clean.
///
/// **Measured, not defensive.** Three runs of `claude -p --model
/// claude-haiku-4-5` with the prompt above, on 2026-09-18, came back wrapped in
/// a ```` ```json ```` fence **every time** despite the instruction forbidding
/// one, and PRD #802's own survey additionally caught a warning about
/// `ANTHROPIC_API_KEY` printed to stdout *before* the JSON. `opencode run
/// --pure` returned a bare object with no fence. So neither "stdout is JSON"
/// nor "stdout is a fenced block" is a property, and this reads output that is
/// either.
///
/// Two passes, in this order:
///
/// 1. **inside a fence**, when the output has one. Preferring the fence is what
///    stops a banner line that happens to be JSON from being read as the
///    answer — which is a real shape for a CLI that logs structured events to
///    stdout, and the reason this is not left to the scan below alone.
/// 2. **the whole output**, scanning forward from each `{` and taking the first
///    one that starts something this build can read as an answer. A streaming
///    deserializer stops at the end of the first value, so trailing text — the
///    fence's own closing ```` ``` ````, a footer, a second object — costs
///    nothing.
///
/// A candidate that parses but names a blank action is rejected: `{"action":
/// ""}` is not an answer, and letting it through would turn a malformed reply
/// into an [`super::VoiceOutcome::UnknownAction`] whose sentence blames the
/// user's phrasing.
pub fn extract_answer(output: &str) -> Result<IntentAnswer, ExtractError> {
    if output.trim().is_empty() {
        return Err(ExtractError::Empty);
    }
    if let Some(fenced) = fenced_block(output)
        && let Some(answer) = scan(fenced)
    {
        return Ok(answer);
    }
    scan(output).ok_or(ExtractError::NoAnswer)
}

/// The first [`MAX_SCAN_BYTES`] of `text`, truncated at a character boundary.
///
/// **Not `&text[..MAX_SCAN_BYTES]`**, which panics when the cap lands inside a
/// multi-byte character — and a banner in any non-ASCII language is exactly the
/// input that would do it. Walking back to the nearest boundary costs at most
/// three bytes and turns a panic into three fewer bytes scanned.
fn window(text: &str) -> &str {
    let mut end = text.len().min(MAX_SCAN_BYTES);
    while end < text.len() && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The contents of the first ```` ``` ````-delimited block, if the output has a
/// closed one.
///
/// The opening fence's info string (`json`, `JSON`, nothing) is whatever is
/// left on that line and is skipped with it. An *unclosed* fence returns
/// `None` rather than "everything after the opener", because truncated output
/// is exactly the case where the fallback scan over the whole string is the
/// better answer.
fn fenced_block(output: &str) -> Option<&str> {
    let after_open = output.find("```")? + 3;
    let body_start = after_open + output[after_open..].find('\n')? + 1;
    let body = output.get(body_start..)?;
    let close = body.find("```")?;
    Some(&body[..close])
}

/// The first `{` at or after which a whole [`IntentAnswer`] parses.
fn scan(text: &str) -> Option<IntentAnswer> {
    let window = window(text);
    window
        .char_indices()
        .filter(|(_, c)| *c == '{')
        .take(MAX_JSON_CANDIDATES)
        .find_map(|(at, _)| {
            serde_json::Deserializer::from_str(&window[at..])
                .into_iter::<IntentAnswer>()
                .next()?
                .ok()
                .filter(|answer| !answer.action.trim().is_empty())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::fixtures::role_agent as agent;
    use crate::voice::schema::annotate;
    use crate::voice::table::{Screen, table};
    use crate::voice::{DesktopAgent, Transcript};

    fn commands() -> Vec<AnnotatedCommand> {
        annotate(table(), Screen::Deck)
    }

    // -- the enum and the param union --------------------------------------

    #[test]
    fn voice_prompt_action_enum_is_the_listed_ids_plus_the_escape() {
        assert_eq!(
            action_enum(&commands()),
            vec![
                "open_agent".to_string(),
                "open_overview".to_string(),
                "open_deck".to_string(),
                "close_agent_view".to_string(),
                "none".to_string(),
            ]
        );
    }

    #[test]
    fn voice_prompt_action_enum_holds_the_escape_for_an_empty_list() {
        assert_eq!(action_enum(&[]), vec![NO_MATCH_ACTION.to_string()]);
    }

    #[test]
    fn voice_prompt_param_names_are_the_union_in_table_order() {
        assert_eq!(param_names(&commands()), vec!["agent".to_string()]);
    }

    #[test]
    fn voice_prompt_param_names_deduplicate_across_rows() {
        // The strict schema enumerates properties, so a name declared by two
        // rows must appear once or the object is malformed.
        let source = [
            "[[commands]]",
            "id = \"one\"",
            "description = \"One.\"",
            "invoke = \"one\"",
            "unavailable_hint = \"nope\"",
            "report = \"Done.\"",
            "  [[commands.params]]",
            "  name = \"agent\"",
            "  kind = \"agent_ref\"",
            "[[commands]]",
            "id = \"two\"",
            "description = \"Two.\"",
            "invoke = \"two\"",
            "unavailable_hint = \"nope\"",
            "report = \"Done.\"",
            "  [[commands.params]]",
            "  name = \"agent\"",
            "  kind = \"agent_ref\"",
        ]
        .join("\n");
        let parsed = crate::voice::table::CommandTable::parse(&source).expect("parses");
        assert_eq!(
            param_names(&annotate(&parsed, Screen::Deck)),
            vec!["agent".to_string()]
        );
    }

    // -- the state block ---------------------------------------------------

    fn request<'a>(
        transcript: &'a Transcript,
        commands: &'a [AnnotatedCommand],
        agents: &'a [DesktopAgent],
    ) -> IntentRequest<'a> {
        IntentRequest {
            transcript,
            commands,
            agents,
        }
    }

    #[test]
    fn voice_prompt_state_names_agents_the_way_the_deck_does() {
        let commands = commands();
        let agents = vec![agent("1", "tester"), agent("2", "orchestrator")];
        let transcript = Transcript::new("show me the tester");
        let state = state(&request(&transcript, &commands, &agents));
        assert_eq!(
            state["agents_on_screen"],
            serde_json::json!(["tester", "orchestrator"])
        );
        assert_eq!(state["commands"].as_array().expect("array").len(), 4);
    }

    #[test]
    fn voice_prompt_state_carries_no_agent_id() {
        // A backend answers with a name and the app resolves it. Handing over
        // ids would invite a backend to assert that an agent exists.
        let commands = commands();
        let agents = vec![agent("agent-id-7f3a", "tester")];
        let transcript = Transcript::new("show me the tester");
        let rendered = state(&request(&transcript, &commands, &agents)).to_string();
        assert!(!rendered.contains("agent-id-7f3a"), "{rendered}");
    }

    #[test]
    fn voice_prompt_cli_prompt_carries_the_utterance_last_and_the_enum() {
        let commands = commands();
        let transcript = Transcript::new("show me the tester");
        let prompt = cli_prompt(&request(&transcript, &commands, &[]));
        assert!(
            prompt.ends_with("Utterance: show me the tester"),
            "{prompt}"
        );
        assert!(prompt.contains("open_agent, open_overview, open_deck, close_agent_view, none"));
        assert!(prompt.contains("Answer `none`"));
    }

    #[test]
    fn voice_prompt_cli_prompt_never_carries_a_rows_report_wording() {
        // The app renders every user-facing sentence, so no backend is shown
        // one it could parrot back as prose.
        let commands = commands();
        let transcript = Transcript::new("show me the tester");
        let prompt = cli_prompt(&request(&transcript, &commands, &[]));
        for row in table().rows() {
            assert!(!prompt.contains(&row.report), "`{}` reached it", row.id);
        }
    }

    // -- extraction --------------------------------------------------------

    fn answer(output: &str) -> IntentAnswer {
        extract_answer(output).expect("extracts")
    }

    #[test]
    fn voice_prompt_extracts_plain_json() {
        let extracted = answer(r#"{"action":"open_overview"}"#);
        assert_eq!(extracted.action, "open_overview");
        assert!(extracted.params.is_empty());
    }

    #[test]
    fn voice_prompt_extracts_fence_wrapped_json() {
        // Measured: `claude -p` fenced its reply on every run despite the
        // prompt forbidding one. This is the shape that actually arrives.
        let extracted = answer(
            "```json\n{\"action\": \"open_agent\", \"params\": {\"agent\": \"tester\"}}\n```",
        );
        assert_eq!(extracted.action, "open_agent");
        assert_eq!(extracted.params.get("agent"), Some(&"tester".to_string()));
    }

    #[test]
    fn voice_prompt_extracts_a_bare_fence_with_no_info_string() {
        let extracted = answer("```\n{\"action\":\"open_deck\"}\n```");
        assert_eq!(extracted.action, "open_deck");
    }

    #[test]
    fn voice_prompt_tolerates_a_leading_banner() {
        // PRD #802's survey caught exactly this: a warning about
        // ANTHROPIC_API_KEY printed to stdout before the JSON.
        let extracted = answer(concat!(
            "Warning: ANTHROPIC_API_KEY takes precedence over your claude.ai login.\n",
            "{\"action\":\"none\"}\n"
        ));
        assert!(extracted.is_no_match());
    }

    #[test]
    fn voice_prompt_tolerates_a_banner_and_a_fence_together() {
        let extracted = answer(concat!(
            "Warning: something happened.\n",
            "```json\n{\"action\":\"open_overview\"}\n```\n"
        ));
        assert_eq!(extracted.action, "open_overview");
    }

    #[test]
    fn voice_prompt_prefers_the_fenced_block_over_a_json_banner_line() {
        // The reason the fence is tried FIRST rather than left to the scan: a
        // CLI that logs structured events to stdout emits objects before the
        // answer, and one of them could parse.
        let extracted = answer(concat!(
            "{\"action\":\"init\",\"params\":{}}\n",
            "```json\n{\"action\":\"open_deck\"}\n```\n"
        ));
        assert_eq!(extracted.action, "open_deck");
    }

    #[test]
    fn voice_prompt_skips_a_leading_object_that_is_not_an_answer() {
        let extracted = answer(concat!(
            "{\"type\":\"system\",\"subtype\":\"init\"}\n",
            "{\"action\":\"open_overview\"}\n"
        ));
        assert_eq!(extracted.action, "open_overview");
    }

    #[test]
    fn voice_prompt_tolerates_trailing_prose_after_the_object() {
        let extracted = answer("{\"action\":\"open_deck\"}\n\nHope that helps!");
        assert_eq!(extracted.action, "open_deck");
    }

    #[test]
    fn voice_prompt_reads_an_unclosed_fence_by_falling_back_to_the_scan() {
        let extracted = answer("```json\n{\"action\":\"open_deck\"}");
        assert_eq!(extracted.action, "open_deck");
    }

    #[test]
    fn voice_prompt_refuses_empty_output() {
        assert_eq!(extract_answer(""), Err(ExtractError::Empty));
        assert_eq!(extract_answer("   \n\t "), Err(ExtractError::Empty));
        assert_eq!(ExtractError::Empty.reason(), "produced no output");
    }

    #[test]
    fn voice_prompt_refuses_malformed_output() {
        assert_eq!(
            extract_answer("not json at all"),
            Err(ExtractError::NoAnswer)
        );
        assert_eq!(
            extract_answer("{\"action\": \"open_deck\""),
            Err(ExtractError::NoAnswer)
        );
        assert_eq!(
            extract_answer("{\"params\":{}}"),
            Err(ExtractError::NoAnswer)
        );
        assert_eq!(extract_answer("[1, 2, 3]"), Err(ExtractError::NoAnswer));
    }

    #[test]
    fn voice_prompt_refuses_a_blank_action() {
        // Letting this through would render as UnknownAction, whose sentence
        // blames the user's phrasing for a malformed reply.
        assert_eq!(
            extract_answer(r#"{"action":"   "}"#),
            Err(ExtractError::NoAnswer)
        );
    }

    #[test]
    fn voice_prompt_keeps_an_action_outside_the_table() {
        // NOT this function's refusal to make: `handle_utterance` turns it into
        // UnknownAction, which is a different sentence from "unreadable".
        assert_eq!(
            answer(r#"{"action":"launch_missiles"}"#).action,
            "launch_missiles"
        );
    }

    #[test]
    fn voice_prompt_scan_is_bounded_against_a_flood_of_braces() {
        // A backend that printed 64 opening braces without an answer is not
        // about to produce one, and the scan must not spend the whole output
        // proving it.
        let mut flood = "{\"noise\":1}\n".repeat(MAX_JSON_CANDIDATES * 4);
        flood.push_str("{\"action\":\"open_deck\"}");
        assert_eq!(extract_answer(&flood), Err(ExtractError::NoAnswer));
    }

    #[test]
    fn voice_prompt_scan_is_bounded_against_a_flood_of_bytes() {
        let mut flood = "x".repeat(MAX_SCAN_BYTES + 16);
        flood.push_str("{\"action\":\"open_deck\"}");
        assert_eq!(extract_answer(&flood), Err(ExtractError::NoAnswer));
    }

    #[test]
    fn voice_prompt_scan_window_truncates_on_a_character_boundary() {
        // `&text[..MAX_SCAN_BYTES]` would PANIC here: the cap lands inside a
        // three-byte character. A banner in any non-ASCII language is exactly
        // this input.
        let mut flood = "\u{2026}".repeat(MAX_SCAN_BYTES); // 3 bytes each
        flood.push_str("{\"action\":\"open_deck\"}");
        assert_eq!(extract_answer(&flood), Err(ExtractError::NoAnswer));
        assert!(window(&flood).len() <= MAX_SCAN_BYTES);
        assert!(flood.is_char_boundary(window(&flood).len()));
    }

    #[test]
    fn voice_prompt_extraction_survives_multibyte_output() {
        // `char_indices` rather than `find('{')` in a byte loop: a banner in
        // any language must not put the scan on a non-boundary.
        let extracted = answer("……… ⟨banner⟩ …\n{\"action\":\"open_deck\"}");
        assert_eq!(extracted.action, "open_deck");
    }
}
