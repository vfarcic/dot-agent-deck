//! The command table: what it is, how it is parsed, and every way a row can be
//! refused.
//!
//! The file itself is `commands.toml` beside this one, embedded with
//! `include_str!` so it ships inside the binary and cannot drift from the build
//! that compiled it.

use std::fmt;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// The table's source, compiled in.
pub const TABLE_SOURCE: &str = include_str!("commands.toml");

/// The action the model answers with when nothing in the table fits.
///
/// **This is most of the safety, not a polish item.** A closed enum with no
/// escape forces a pick, which turns "what time is it?" into a real action;
/// with the escape present the model answered `{"action":"none"}` for exactly
/// that utterance in the PRD's measurement. It is reserved: a row claiming this
/// id is refused by [`TableError::ReservedId`].
pub const NO_MATCH_ACTION: &str = "none";

/// Which top-level surface is mounted — the three `DeckView` kinds in
/// `desktop/src/types.ts`, and the closed set the `screens` column draws from.
///
/// It is three because `DeckView` is three. Notably it cannot express *"an
/// overlay is open"*: five of the rail's seven buttons toggle booleans that are
/// not in `DeckView` at all, so a command whose availability depends on one of
/// those is a change to where that state lives and not a change to this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Screen {
    Deck,
    Overview,
    Agent,
}

impl Screen {
    pub const ALL: [Screen; 3] = [Screen::Deck, Screen::Overview, Screen::Agent];

    pub fn as_str(self) -> &'static str {
        match self {
            Screen::Deck => "deck",
            Screen::Overview => "overview",
            Screen::Agent => "agent",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|screen| screen.as_str() == value)
    }
}

impl fmt::Display for Screen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The closed set of resolver kinds a param can have.
///
/// One entry today. It is an enum rather than a string because the kind selects
/// a resolver — `agent_ref` resolves a spoken reference against the live agent
/// snapshot — so an unknown kind is a row nothing can execute, which is why
/// [`TableError::UnknownParamKind`] refuses it at parse time instead of letting
/// it surface at runtime as nothing happening.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    AgentRef,
}

impl ParamKind {
    pub const ALL: [ParamKind; 1] = [ParamKind::AgentRef];

    pub fn as_str(self) -> &'static str {
        match self {
            ParamKind::AgentRef => "agent_ref",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
}

impl fmt::Display for ParamKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One declared parameter of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamSpec {
    pub name: String,
    pub kind: ParamKind,
}

/// One row: a command the model may pick and the app may dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRow {
    pub id: String,
    /// A prompt the model picks on, not documentation. Never shown to the user
    /// — the user-facing wording is [`CommandRow::report`] and
    /// [`CommandRow::unavailable_hint`].
    pub description: String,
    /// The frontend action registry entry to dispatch (M2). Stringly typed on
    /// purpose, with M3's guard keeping it honest — a typed enum would make
    /// adding a command a three-file change and the one-file promise is the
    /// whole design.
    pub invoke: String,
    /// Where the command can run. **Empty means everywhere**, which is what an
    /// absent `screens` key parses to.
    pub screens: Vec<Screen>,
    pub unavailable_hint: String,
    /// The sentence a successful dispatch shows — the pipeline's stage 6.
    ///
    /// **Named `report` rather than `confirmation` deliberately.** PRD #802's
    /// D5 is *destructive commands behind confirmation*, so rows in this same
    /// file will later carry a genuine confirm-before-acting flag, and the two
    /// are close to opposite: this describes an action that happened, that one
    /// blocks an action before it does. `commands.toml` says so at the column.
    pub report: String,
    pub params: Vec<ParamSpec>,
}

impl CommandRow {
    /// Whether this row can run on `screen`.
    ///
    /// An empty `screens` list is "available everywhere" rather than "available
    /// nowhere", so an absent column is the permissive default.
    pub fn callable_on(&self, screen: Screen) -> bool {
        self.screens.is_empty() || self.screens.contains(&screen)
    }
}

/// The parsed table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandTable {
    commands: Vec<CommandRow>,
}

impl CommandTable {
    /// Parse a table from TOML, refusing each malformation by name.
    ///
    /// Every rejection is its own [`TableError`] variant rather than a generic
    /// parse failure, because these are read by whoever just edited the table
    /// and "invalid TOML" does not say which row or which column.
    pub fn parse(source: &str) -> Result<Self, TableError> {
        let raw: RawTable =
            toml_edit::de::from_str(source).map_err(|error| TableError::Malformed {
                detail: error.to_string(),
            })?;

        let mut commands: Vec<CommandRow> = Vec::with_capacity(raw.commands.len());
        for (index, row) in raw.commands.into_iter().enumerate() {
            let id = row
                .id
                .filter(|id| !id.trim().is_empty())
                .ok_or(TableError::MissingId { index })?;
            if id == NO_MATCH_ACTION {
                return Err(TableError::ReservedId { id });
            }
            if commands.iter().any(|existing| existing.id == id) {
                return Err(TableError::DuplicateId { id });
            }

            let invoke = row
                .invoke
                .filter(|invoke| !invoke.trim().is_empty())
                .ok_or_else(|| TableError::MissingInvoke { id: id.clone() })?;
            let description = required(row.description, &id, "description")?;
            let unavailable_hint = required(row.unavailable_hint, &id, "unavailable_hint")?;
            let report = required(row.report, &id, "report")?;

            let mut screens = Vec::new();
            for screen in row.screens.unwrap_or_default() {
                let parsed = Screen::parse(&screen).ok_or_else(|| TableError::UnknownScreen {
                    id: id.clone(),
                    screen: screen.clone(),
                })?;
                if !screens.contains(&parsed) {
                    screens.push(parsed);
                }
            }

            let mut params: Vec<ParamSpec> = Vec::with_capacity(row.params.len());
            for (param_index, param) in row.params.into_iter().enumerate() {
                let name = param
                    .name
                    .filter(|name| !name.trim().is_empty())
                    .ok_or_else(|| TableError::MissingParamName {
                        id: id.clone(),
                        index: param_index,
                    })?;
                let kind_text = param.kind.unwrap_or_default();
                let kind =
                    ParamKind::parse(&kind_text).ok_or_else(|| TableError::UnknownParamKind {
                        id: id.clone(),
                        param: name.clone(),
                        kind: kind_text,
                    })?;
                if params.iter().any(|existing| existing.name == name) {
                    return Err(TableError::DuplicateParam {
                        id: id.clone(),
                        param: name,
                    });
                }
                params.push(ParamSpec { name, kind });
            }

            // The report is the one column with a stringly reference INSIDE
            // it, so it gets the same treatment `invoke` gets from M3's guard:
            // a placeholder naming no declared param is refused here rather
            // than rendering as a literal `{agent}` to the user.
            for placeholder in placeholders(&report, &id)? {
                if !params.iter().any(|param| param.name == placeholder) {
                    return Err(TableError::UnknownPlaceholder {
                        id: id.clone(),
                        placeholder,
                    });
                }
            }

            commands.push(CommandRow {
                id,
                description,
                invoke,
                screens,
                unavailable_hint,
                report,
                params,
            });
        }

        Ok(Self { commands })
    }

    pub fn rows(&self) -> &[CommandRow] {
        &self.commands
    }

    pub fn row(&self, id: &str) -> Option<&CommandRow> {
        self.commands.iter().find(|row| row.id == id)
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

fn required(value: Option<String>, id: &str, field: &'static str) -> Result<String, TableError> {
    value
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| TableError::MissingField {
            id: id.to_string(),
            field,
        })
}

/// The `{name}` placeholders in a report, in order.
///
/// There is no escape for a literal `{` and nothing in the table needs one; an
/// unclosed brace is refused rather than passed through, so a report that
/// looks interpolated and is not cannot reach a user.
fn placeholders(text: &str, id: &str) -> Result<Vec<String>, TableError> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let close = after.find('}').ok_or_else(|| TableError::MalformedReport {
            id: id.to_string(),
            detail: "an opening `{` with no closing `}`".to_string(),
        })?;
        let name = after[..close].trim().to_string();
        if name.is_empty() {
            return Err(TableError::MalformedReport {
                id: id.to_string(),
                detail: "an empty `{}` placeholder".to_string(),
            });
        }
        found.push(name);
        rest = &after[close + 1..];
    }
    Ok(found)
}

/// Every way the table can be refused.
///
/// Named individually because the reader is whoever just edited
/// `commands.toml`: "invalid TOML" does not say which row or which column, and
/// a row that parses but cannot be dispatched is the failure this whole design
/// exists to keep off the runtime path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableError {
    /// Not valid TOML, a value of the wrong type, or a column that is not one
    /// of the table's own (the raw shape is `deny_unknown_fields`, so a typo'd
    /// column is refused rather than silently ignored).
    Malformed { detail: String },
    /// A row with no `id`, named by position because it has no name.
    MissingId { index: usize },
    /// A row claiming the reserved "none of these" action.
    ReservedId { id: String },
    /// Two rows sharing an `id`.
    DuplicateId { id: String },
    /// A row with no `invoke`: nothing could dispatch it.
    MissingInvoke { id: String },
    /// A row missing one of the other required columns.
    MissingField { id: String, field: &'static str },
    /// A `screens` entry that is not one of the three `DeckView` kinds.
    UnknownScreen { id: String, screen: String },
    /// A param `kind` outside the closed resolver set.
    UnknownParamKind {
        id: String,
        param: String,
        kind: String,
    },
    /// A param with no `name`.
    MissingParamName { id: String, index: usize },
    /// Two params of one row sharing a `name`.
    DuplicateParam { id: String, param: String },
    /// A `report` placeholder naming no declared param.
    UnknownPlaceholder { id: String, placeholder: String },
    /// A `report` whose braces do not pair up.
    MalformedReport { id: String, detail: String },
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableError::Malformed { detail } => {
                write!(f, "the command table is not valid: {detail}")
            }
            TableError::MissingId { index } => {
                write!(f, "command #{index} has no `id`")
            }
            TableError::ReservedId { id } => write!(
                f,
                "command `{id}` claims the reserved `{NO_MATCH_ACTION}` action, which means \"none of these\""
            ),
            TableError::DuplicateId { id } => write!(f, "two commands share the id `{id}`"),
            TableError::MissingInvoke { id } => {
                write!(
                    f,
                    "command `{id}` has no `invoke`, so nothing could dispatch it"
                )
            }
            TableError::MissingField { id, field } => {
                write!(f, "command `{id}` has no `{field}`")
            }
            TableError::UnknownScreen { id, screen } => write!(
                f,
                "command `{id}` names the unknown screen `{screen}`; the screens are {}",
                joined(Screen::ALL.iter().map(|screen| screen.as_str()))
            ),
            TableError::UnknownParamKind { id, param, kind } => write!(
                f,
                "command `{id}`'s param `{param}` has the unknown kind `{kind}`; the kinds are {}",
                joined(ParamKind::ALL.iter().map(|kind| kind.as_str()))
            ),
            TableError::MissingParamName { id, index } => {
                write!(f, "command `{id}`'s param #{index} has no `name`")
            }
            TableError::DuplicateParam { id, param } => {
                write!(f, "command `{id}` declares the param `{param}` twice")
            }
            TableError::UnknownPlaceholder { id, placeholder } => write!(
                f,
                "command `{id}`'s report names `{{{placeholder}}}`, which is not one of its params"
            ),
            TableError::MalformedReport { id, detail } => {
                write!(f, "command `{id}`'s report has {detail}")
            }
        }
    }
}

impl std::error::Error for TableError {}

fn joined<'a>(values: impl Iterator<Item = &'a str>) -> String {
    values
        .map(|value| format!("`{value}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The embedded table, parsed once at first use.
///
/// `OnceLock` at first use rather than eagerly at startup, deliberately. The
/// source is `include_str!`d, so its bytes are fixed when the binary is built
/// and nothing a user does at run time reaches this parse. A failure therefore
/// arrives with a source change — `commands.toml`, the parser above it, or a
/// `toml_edit` bump that changes what its deserializer accepts — rather than
/// with an input, and `voice_table_embedded_table_parses` below refuses each of
/// those under `cargo test-fast --workspace` and therefore in the required
/// `build` job. So for a build that went through that gate, being lazy defers
/// no failure onto a user; it just means a desktop process that never speaks
/// never parses, and voice is opt-in.
///
/// It panics rather than returning a `Result` for the same reason: there is
/// nothing here a caller could handle differently, and a `Result` every
/// call site has to thread would be a permanent cost paid against a case the
/// gate above already refuses. The message names the file and the rejection, so
/// whoever broke the table reads what they broke.
pub fn table() -> &'static CommandTable {
    static TABLE: OnceLock<CommandTable> = OnceLock::new();
    TABLE.get_or_init(|| match CommandTable::parse(TABLE_SOURCE) {
        Ok(table) => table,
        Err(error) => panic!("desktop/src-tauri/src/voice/commands.toml is invalid: {error}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed single row, for tests that mutate one thing about it.
    fn one_row() -> String {
        [
            "[[commands]]",
            "id = \"open_agent\"",
            "description = \"Open one agent.\"",
            "invoke = \"openAgent\"",
            "screens = [\"deck\"]",
            "unavailable_hint = \"it works from the deck\"",
            "report = \"Opening {agent}.\"",
            "",
            "[[commands.params]]",
            "name = \"agent\"",
            "kind = \"agent_ref\"",
        ]
        .join("\n")
    }

    #[test]
    fn voice_table_embedded_table_parses() {
        // The gate the `table()` doc comment leans on: a malformed embedded
        // table fails here, in `cargo test-fast --workspace` and so in the
        // required `build` job, rather than at a user's first utterance.
        let table = CommandTable::parse(TABLE_SOURCE).expect("the embedded table parses");
        assert!(!table.is_empty());
        assert_eq!(table, *super::table());
    }

    #[test]
    fn voice_table_embedded_table_has_the_rows_the_milestones_build_against() {
        // M2 has to make each `invoke` real and M3's guard has to resolve it,
        // so the shape of the shipped table is pinned rather than described.
        let table = super::table();
        let rows: Vec<(&str, &str, Vec<&str>)> = table
            .rows()
            .iter()
            .map(|row| {
                (
                    row.id.as_str(),
                    row.invoke.as_str(),
                    row.screens.iter().map(|screen| screen.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("open_agent", "openAgent", vec!["deck", "overview"]),
                ("open_overview", "openOverview", vec!["deck"]),
                ("open_deck", "openDeck", vec!["overview"]),
                ("close_agent_view", "closeAgentView", vec!["agent"]),
                ("open_settings", "openSettings", vec!["deck"]),
            ]
        );
    }

    #[test]
    fn voice_table_embedded_table_has_at_least_three_rows_and_one_param() {
        // PRD #802 M6: at least three rows, because a one-command vocabulary
        // makes the model map everything to it and tests neither disambiguation
        // nor the no-match path.
        let table = super::table();
        assert!(table.rows().len() >= 3, "got {} rows", table.rows().len());
        let open_agent = table.row("open_agent").expect("open_agent is in the table");
        assert_eq!(
            open_agent.params,
            vec![ParamSpec {
                name: "agent".to_string(),
                kind: ParamKind::AgentRef,
            }]
        );
    }

    #[test]
    fn voice_table_embedded_hints_are_sentence_fragments() {
        // They are rendered INTO a sentence, so a leading capital or a trailing
        // period would produce "Not here — Opening an agent works.."
        for row in super::table().rows() {
            let hint = &row.unavailable_hint;
            assert!(
                !hint.ends_with('.'),
                "`{}`'s hint ends with a period",
                row.id
            );
            assert!(
                hint.starts_with(|c: char| !c.is_uppercase()),
                "`{}`'s hint starts with a capital",
                row.id
            );
        }
    }

    #[test]
    fn voice_table_parses_a_minimal_row() {
        let table = CommandTable::parse(&one_row()).expect("parses");
        assert_eq!(table.rows().len(), 1);
        let row = &table.rows()[0];
        assert_eq!(row.id, "open_agent");
        assert_eq!(row.invoke, "openAgent");
        assert_eq!(row.screens, vec![Screen::Deck]);
        assert_eq!(row.params.len(), 1);
    }

    #[test]
    fn voice_table_rejects_malformed_toml() {
        let error = CommandTable::parse("[[commands]\nid = \"x\"").expect_err("refused");
        assert!(
            matches!(error, TableError::Malformed { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn voice_table_rejects_a_wrongly_typed_value() {
        let source = one_row().replace("screens = [\"deck\"]", "screens = \"deck\"");
        let error = CommandTable::parse(&source).expect_err("refused");
        assert!(
            matches!(error, TableError::Malformed { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn voice_table_rejects_an_unknown_column() {
        // `deny_unknown_fields` on the raw shape: a typo'd column name is a
        // column nothing reads, which is the silent failure this refuses.
        let source = format!("{}\n", one_row().replace("screens =", "screen ="));
        let error = CommandTable::parse(&source).expect_err("refused");
        assert!(
            matches!(error, TableError::Malformed { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn voice_table_rejects_a_row_with_no_id() {
        let source = one_row().replace("id = \"open_agent\"\n", "");
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(error, TableError::MissingId { index: 0 });
    }

    #[test]
    fn voice_table_rejects_the_reserved_none_id() {
        let source = one_row().replace("id = \"open_agent\"", "id = \"none\"");
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::ReservedId {
                id: "none".to_string()
            }
        );
    }

    #[test]
    fn voice_table_rejects_a_duplicate_id() {
        let source = format!("{}\n{}", one_row(), one_row());
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::DuplicateId {
                id: "open_agent".to_string()
            }
        );
    }

    #[test]
    fn voice_table_rejects_a_missing_invoke() {
        let source = one_row().replace("invoke = \"openAgent\"\n", "");
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::MissingInvoke {
                id: "open_agent".to_string()
            }
        );
        assert!(error.to_string().contains("nothing could dispatch it"));
    }

    #[test]
    fn voice_table_rejects_an_empty_invoke() {
        let source = one_row().replace("invoke = \"openAgent\"", "invoke = \"   \"");
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::MissingInvoke {
                id: "open_agent".to_string()
            }
        );
    }

    #[test]
    fn voice_table_rejects_each_missing_required_column() {
        for (line, field) in [
            ("description = \"Open one agent.\"\n", "description"),
            (
                "unavailable_hint = \"it works from the deck\"\n",
                "unavailable_hint",
            ),
            ("report = \"Opening {agent}.\"\n", "report"),
        ] {
            let source = one_row().replace(line, "");
            let error = CommandTable::parse(&source).expect_err("refused");
            assert_eq!(
                error,
                TableError::MissingField {
                    id: "open_agent".to_string(),
                    field,
                }
            );
        }
    }

    #[test]
    fn voice_table_rejects_an_unknown_screen() {
        let source =
            one_row().replace("screens = [\"deck\"]", "screens = [\"deck\", \"settings\"]");
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::UnknownScreen {
                id: "open_agent".to_string(),
                screen: "settings".to_string(),
            }
        );
        assert!(error.to_string().contains("`deck`, `overview`, `agent`"));
    }

    #[test]
    fn voice_table_rejects_an_unknown_param_kind() {
        let source = one_row().replace("kind = \"agent_ref\"", "kind = \"deck_ref\"");
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::UnknownParamKind {
                id: "open_agent".to_string(),
                param: "agent".to_string(),
                kind: "deck_ref".to_string(),
            }
        );
        assert!(error.to_string().contains("`agent_ref`"));
    }

    #[test]
    fn voice_table_rejects_a_param_with_no_name() {
        let source = one_row().replace("name = \"agent\"\n", "");
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::MissingParamName {
                id: "open_agent".to_string(),
                index: 0,
            }
        );
    }

    #[test]
    fn voice_table_rejects_a_param_with_no_kind() {
        // `kind` is the last line of the fixture, so it carries no newline.
        let source = one_row().replace("kind = \"agent_ref\"", "");
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::UnknownParamKind {
                id: "open_agent".to_string(),
                param: "agent".to_string(),
                kind: String::new(),
            }
        );
    }

    #[test]
    fn voice_table_rejects_a_duplicate_param() {
        let source = format!(
            "{}\n\n[[commands.params]]\nname = \"agent\"\nkind = \"agent_ref\"",
            one_row()
        );
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::DuplicateParam {
                id: "open_agent".to_string(),
                param: "agent".to_string(),
            }
        );
    }

    #[test]
    fn voice_table_rejects_a_report_placeholder_that_names_no_param() {
        let source = one_row().replace(
            "report = \"Opening {agent}.\"",
            "report = \"Opening {pane}.\"",
        );
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::UnknownPlaceholder {
                id: "open_agent".to_string(),
                placeholder: "pane".to_string(),
            }
        );
    }

    #[test]
    fn voice_table_rejects_an_unclosed_report_placeholder() {
        let source = one_row().replace(
            "report = \"Opening {agent}.\"",
            "report = \"Opening {agent.\"",
        );
        let error = CommandTable::parse(&source).expect_err("refused");
        assert!(
            matches!(error, TableError::MalformedReport { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn voice_table_rejects_an_empty_report_placeholder() {
        let source = one_row().replace("report = \"Opening {agent}.\"", "report = \"Opening {}.\"");
        let error = CommandTable::parse(&source).expect_err("refused");
        assert!(
            matches!(error, TableError::MalformedReport { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn voice_table_accepts_an_empty_table() {
        // Not a rejection: an empty table is a deck with no voice commands,
        // which the schema then renders as the escape alone.
        let table = CommandTable::parse("").expect("parses");
        assert!(table.is_empty());
    }

    #[test]
    fn voice_table_callable_on_reads_screens() {
        let table = CommandTable::parse(&one_row()).expect("parses");
        let row = &table.rows()[0];
        assert!(row.callable_on(Screen::Deck));
        assert!(!row.callable_on(Screen::Overview));
        assert!(!row.callable_on(Screen::Agent));
    }

    #[test]
    fn voice_table_absent_screens_is_callable_everywhere() {
        let source = one_row().replace("screens = [\"deck\"]\n", "");
        let table = CommandTable::parse(&source).expect("parses");
        let row = &table.rows()[0];
        assert!(row.screens.is_empty());
        for screen in Screen::ALL {
            assert!(row.callable_on(screen), "not callable on {screen}");
        }
    }

    #[test]
    fn voice_table_empty_screens_is_callable_everywhere() {
        let source = one_row().replace("screens = [\"deck\"]", "screens = []");
        let table = CommandTable::parse(&source).expect("parses");
        let row = &table.rows()[0];
        for screen in Screen::ALL {
            assert!(row.callable_on(screen), "not callable on {screen}");
        }
    }

    #[test]
    fn voice_table_each_shipped_row_is_callable_and_not_callable_somewhere() {
        // Every shipped row names its screens, so each has both a true and a
        // false case — which is what makes the unavailable hint reachable.
        for row in super::table().rows() {
            assert!(
                Screen::ALL.iter().any(|&screen| row.callable_on(screen)),
                "`{}` is callable nowhere",
                row.id
            );
            assert!(
                Screen::ALL.iter().any(|&screen| !row.callable_on(screen)),
                "`{}` is callable everywhere, so its hint is unreachable",
                row.id
            );
        }
    }

    #[test]
    fn voice_table_callable_per_screen_for_the_shipped_rows() {
        let table = super::table();
        let callable = |screen: Screen| {
            table
                .rows()
                .iter()
                .filter(|row| row.callable_on(screen))
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>()
        };
        // `open_settings` is on the deck and NOT on the overview: the Settings
        // overlay is reachable only from the deck rail, and a row exposes
        // behaviour that already exists rather than adding a route of its own.
        assert_eq!(
            callable(Screen::Deck),
            vec!["open_agent", "open_overview", "open_settings"]
        );
        assert_eq!(callable(Screen::Overview), vec!["open_agent", "open_deck"]);
        assert_eq!(callable(Screen::Agent), vec!["close_agent_view"]);
    }

    #[test]
    fn voice_table_screen_round_trips_its_spelling() {
        for screen in Screen::ALL {
            assert_eq!(Screen::parse(screen.as_str()), Some(screen));
        }
        assert_eq!(Screen::parse("settings"), None);
        assert_eq!(Screen::parse("Deck"), None);
    }

    #[test]
    fn voice_table_param_kind_round_trips_its_spelling() {
        for kind in ParamKind::ALL {
            assert_eq!(ParamKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ParamKind::parse("agentRef"), None);
    }

    #[test]
    fn voice_table_row_lookup_is_by_exact_id() {
        let table = super::table();
        assert!(table.row("open_agent").is_some());
        assert!(table.row("open_Agent").is_none());
        assert!(table.row(NO_MATCH_ACTION).is_none());
    }
}

// ---------------------------------------------------------------------------
// The raw shape, deserialized before anything is validated.
//
// Every required column is `Option` here so its absence becomes a NAMED
// rejection above rather than serde's own "missing field", which is a generic
// parse failure and does not say which row.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTable {
    #[serde(default)]
    commands: Vec<RawCommand>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommand {
    id: Option<String>,
    description: Option<String>,
    invoke: Option<String>,
    screens: Option<Vec<String>>,
    unavailable_hint: Option<String>,
    report: Option<String>,
    #[serde(default)]
    params: Vec<RawParam>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawParam {
    name: Option<String>,
    kind: Option<String>,
}
