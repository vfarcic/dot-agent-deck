//! Consumer 1 of the table: the model's tool definition.
//!
//! **One tool taking `action` as an enum**, rather than a tool per command.
//! With a tool per command, *listing* a tool means the model may call it, so a
//! per-entry availability flag is advisory at best; with one tool the output
//! stays constrained while every entry can still carry its own flag.
//!
//! **Every command is sent on every request, each annotated `callable`** —
//! there is no filtering path here and none is wanted. Filtering produces a
//! false answer: asking to open an agent from a screen where that is impossible
//! would yield "I don't know how to do that", when the truthful answer names
//! the prerequisite. The trade is worth restating because it is easy to forget:
//! filtering *prevents* wrong calls, flagging *explains* them, and flagging
//! leans entirely on validation to refuse an action the model called anyway.

use serde::Serialize;
use serde_json::{Value, json};

use super::table::{CommandTable, NO_MATCH_ACTION, ParamKind, Screen};

/// The one tool the model is given.
pub const TOOL_NAME: &str = "run_deck_action";

/// What the model is told to do, in one paragraph.
///
/// **A prompt, reviewed as an interface.** It is a `const` rather than a
/// literal inside [`tool_schema`] because M5 gave it a second consumer: the
/// keyed remote backend sends it as the tool's `description` and the agent-CLI
/// backend, which has no tool-use envelope to put it in, sends the same words
/// in its prompt. One wording, two backends — a copy in each would let the two
/// drift, and PRD #802's phrase fixtures are authoritative against one backend
/// only, so nothing would catch the drift.
///
/// Written as a `\`-continued literal, which rustfmt indents; the continuation
/// strips the newline **and** the leading whitespace, so the text is one
/// paragraph. `voice_schema_tool_description_reads_as_prose` asserts that
/// rather than trusting it.
pub const TOOL_INSTRUCTIONS: &str = "Pick the deck action the user asked for. Pick exactly one. \
    Every action is listed whether or not it can run right now: `callable: false` \
    means it exists but the current screen cannot run it, and picking it is the \
    right answer when that is what the user asked for. Answer `none` when the \
    request does not match any action listed — do not force a pick. \
    `agents_on_screen` carries each agent's LIVE state as the deck holds it: \
    `status` is the daemon's own word for what it is doing (`working`, `thinking`, \
    `compacting`, `waiting_for_input`, `idle`, `error`, `unknown`, `running`), and \
    `tool` is what it is running right now. A user refers to an agent by state as \
    readily as by name — \"the one that is stuck\", \"whichever is waiting\" — so \
    resolve such a reference against those fields and answer with that agent's \
    `label`. For a reference the user made by name, answer with the words the user \
    used and let the app resolve them. Write no prose; \
    the app writes what the user reads.";

/// One row as the model sees it, with its availability on the screen the
/// request was built for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnnotatedCommand {
    pub id: String,
    pub description: String,
    /// Whether this command can run on the current screen. Computed from the
    /// row's `screens`; an empty list is available everywhere.
    pub callable: bool,
    /// The prerequisite, so the model has the reason rather than inventing one.
    /// The app still renders the sentence — this is never echoed back as prose.
    pub unavailable_hint: String,
    pub params: Vec<AnnotatedParam>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnnotatedParam {
    pub name: String,
    pub kind: ParamKind,
}

/// The full table annotated for one screen — every row, in table order.
pub fn annotate(table: &CommandTable, screen: Screen) -> Vec<AnnotatedCommand> {
    table
        .rows()
        .iter()
        .map(|row| AnnotatedCommand {
            id: row.id.clone(),
            description: row.description.clone(),
            callable: row.callable_on(screen),
            unavailable_hint: row.unavailable_hint.clone(),
            params: row
                .params
                .iter()
                .map(|param| AnnotatedParam {
                    name: param.name.clone(),
                    kind: param.kind,
                })
                .collect(),
        })
        .collect()
}

/// The `action` enum: every row id, plus the "none of these" escape last.
///
/// The enum is **not** narrowed to the callable rows. A model that picks an
/// unavailable command is the case the hint exists to explain, so it has to be
/// able to name one.
pub fn action_enum(table: &CommandTable) -> Vec<String> {
    table
        .rows()
        .iter()
        .map(|row| row.id.clone())
        .chain(std::iter::once(NO_MATCH_ACTION.to_string()))
        .collect()
}

/// The whole payload for one request: the tool definition plus the annotated
/// command list.
///
/// The shape is this app's own. M5's backends adapt it to whatever envelope
/// their API wants — a tool-use block for a keyed remote backend, a prompt for
/// the agent-CLI one — which is why the annotated list travels beside the
/// schema rather than being folded into a description string that no backend
/// could read structurally.
pub fn tool_schema(table: &CommandTable, screen: Screen) -> Value {
    let commands = annotate(table, screen);
    json!({
        "name": TOOL_NAME,
        "description": TOOL_INSTRUCTIONS,
        "input_schema": {
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "The id of the action to run, or `none` when nothing listed matches.",
                    "enum": action_enum(table),
                },
                "params": {
                    "type": "object",
                    "description": "The params the chosen action declares, as the user referred to them. \
    Verbatim from the utterance — the app resolves each one against live state.",
                    "additionalProperties": { "type": "string" },
                },
            },
            "required": ["action"],
            "additionalProperties": false,
        },
        "screen": screen.as_str(),
        "commands": commands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::table::{CommandTable, table};

    #[test]
    fn voice_schema_action_enum_is_exactly_the_ids_plus_the_escape() {
        let enumeration = action_enum(table());
        assert_eq!(
            enumeration,
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
    fn voice_schema_escape_is_present_even_for_an_empty_table() {
        // The escape is not derived from the rows, so a table with none still
        // leaves the model able to answer "none of these".
        let empty = CommandTable::parse("").expect("parses");
        assert_eq!(action_enum(&empty), vec![NO_MATCH_ACTION.to_string()]);
    }

    #[test]
    fn voice_schema_enum_matches_the_table_and_holds_no_stray_id() {
        let table = table();
        let enumeration = action_enum(table);
        for row in table.rows() {
            assert!(
                enumeration.contains(&row.id),
                "`{}` missing from the enum",
                row.id
            );
        }
        for action in &enumeration {
            assert!(
                action == NO_MATCH_ACTION || table.row(action).is_some(),
                "`{action}` is in the enum and not in the table"
            );
        }
        assert_eq!(enumeration.len(), table.rows().len() + 1);
    }

    #[test]
    fn voice_schema_tool_schema_carries_the_enum_and_the_escape() {
        let schema = tool_schema(table(), Screen::Deck);
        assert_eq!(schema["name"], TOOL_NAME);
        let enumeration = schema["input_schema"]["properties"]["action"]["enum"]
            .as_array()
            .expect("the action enum is an array");
        let ids: Vec<&str> = enumeration
            .iter()
            .map(|value| value.as_str().expect("string"))
            .collect();
        assert_eq!(
            ids,
            vec![
                "open_agent",
                "open_overview",
                "open_deck",
                "close_agent_view",
                "none"
            ]
        );
        assert_eq!(schema["input_schema"]["required"][0], "action");
        assert_eq!(schema["input_schema"]["additionalProperties"], false);
    }

    #[test]
    fn voice_schema_tool_description_reads_as_prose() {
        // The tool description is a PROMPT, and it is written as a
        // `\`-continued literal that rustfmt indents. The continuation strips
        // the newline AND the leading whitespace, so the text is one paragraph
        // — this asserts that rather than trusting it, since a reformat that
        // broke it would push ragged indentation into the model's input.
        let schema = tool_schema(table(), Screen::Deck);
        let description = schema["description"].as_str().expect("a string");
        assert!(
            !description.contains("  "),
            "double space in {description:?}"
        );
        assert!(!description.contains('\n'), "newline in {description:?}");
        assert!(description.contains("exactly one. Every action is listed"));
        // The escape has to be spelled out, or the model has no reason to use it.
        assert!(description.contains("Answer `none`"));
        assert!(description.contains("do not force a pick"));
    }

    #[test]
    fn voice_schema_sends_every_row_on_every_request() {
        // No filtering path exists; the list is the whole table on every screen.
        for screen in Screen::ALL {
            let commands = annotate(table(), screen);
            assert_eq!(commands.len(), table().rows().len(), "filtered on {screen}");
        }
    }

    #[test]
    fn voice_schema_annotates_callable_per_screen() {
        let flags = |screen: Screen| {
            annotate(table(), screen)
                .into_iter()
                .map(|command| (command.id, command.callable))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            flags(Screen::Deck),
            vec![
                ("open_agent".to_string(), true),
                ("open_overview".to_string(), true),
                ("open_deck".to_string(), false),
                ("close_agent_view".to_string(), false),
            ]
        );
        assert_eq!(
            flags(Screen::Overview),
            vec![
                ("open_agent".to_string(), true),
                ("open_overview".to_string(), false),
                ("open_deck".to_string(), true),
                ("close_agent_view".to_string(), false),
            ]
        );
        assert_eq!(
            flags(Screen::Agent),
            vec![
                ("open_agent".to_string(), false),
                ("open_overview".to_string(), false),
                ("open_deck".to_string(), false),
                ("close_agent_view".to_string(), true),
            ]
        );
    }

    #[test]
    fn voice_schema_annotates_an_absent_screens_row_callable_everywhere() {
        let source = [
            "[[commands]]",
            "id = \"anywhere\"",
            "description = \"Works anywhere.\"",
            "invoke = \"anywhere\"",
            "unavailable_hint = \"unreachable\"",
            "report = \"Done.\"",
        ]
        .join("\n");
        let parsed = CommandTable::parse(&source).expect("parses");
        for screen in Screen::ALL {
            let commands = annotate(&parsed, screen);
            assert!(commands[0].callable, "not callable on {screen}");
        }
    }

    #[test]
    fn voice_schema_carries_the_hint_and_the_params() {
        let commands = annotate(table(), Screen::Agent);
        let open_agent = commands
            .iter()
            .find(|command| command.id == "open_agent")
            .expect("present");
        assert!(!open_agent.callable);
        assert_eq!(
            open_agent.unavailable_hint,
            "opening an agent works from the deck or the agent overview"
        );
        assert_eq!(
            open_agent.params,
            vec![AnnotatedParam {
                name: "agent".to_string(),
                kind: ParamKind::AgentRef,
            }]
        );
    }

    #[test]
    fn voice_schema_names_the_screen_the_request_was_built_for() {
        for screen in Screen::ALL {
            assert_eq!(tool_schema(table(), screen)["screen"], screen.as_str());
        }
    }

    #[test]
    fn voice_schema_serializes_a_param_kind_in_its_toml_spelling() {
        let json = serde_json::to_value(AnnotatedParam {
            name: "agent".to_string(),
            kind: ParamKind::AgentRef,
        })
        .expect("serializes");
        assert_eq!(json["kind"], "agent_ref");
    }

    #[test]
    fn voice_schema_never_leaks_the_report_wording_to_the_model() {
        // The app renders every user-facing sentence. The model is given the
        // hint (so it can reason about availability) and nothing it could
        // parrot back as prose.
        let schema = tool_schema(table(), Screen::Deck).to_string();
        for row in table().rows() {
            assert!(
                !schema.contains(&row.report),
                "`{}`'s report reached the model",
                row.id
            );
        }
    }
}
