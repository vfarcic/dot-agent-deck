//! The command table: what it is, how it is parsed, and every way a row can be
//! refused.
//!
//! The file itself is `commands.toml` beside this one, embedded with
//! `include_str!` so it ships inside the binary and cannot drift from the build
//! that compiled it.

use std::fmt;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use super::{VoiceDirectories, VoiceNewAgent};

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
/// It is an enum rather than a string because the kind selects a resolver — so
/// an unknown kind is a row nothing can execute, which is why
/// [`TableError::UnknownParamKind`] refuses it at parse time instead of letting
/// it surface at runtime as nothing happening.
///
/// # The kinds resolve against different things, and that is the point of
/// `spoken_prefix`
///
/// [`ParamKind::AgentRef`] resolves a spoken reference against **live state**:
/// the agent snapshot decides whether *"the tester"* names one agent, none, or
/// several. [`ParamKind::DeckRef`] is the same shape one level up (PRD #1223):
/// the fleet the desktop observes decides whether *"the build box"* names one
/// deck, none, or several — and it is a kind of its own rather than a
/// dialog's private parser because issue #1195's "switch deck" needs exactly
/// the same resolution. [`ParamKind::DirRef`] is that shape again, against the
/// one thing in this app that is a set of names on screen and is NOT in any
/// snapshot: the children the New agent dialog's directory browser is showing
/// (PRD #1223, [`super::VoiceDirectories`]). It never searches — "open dir
/// billing" means the child called billing in the level on screen, and a
/// directory anywhere else on the deck is out of its reach by construction.
/// [`ParamKind::ModeRef`] and [`ParamKind::AgentTypeRef`] are the same shape
/// once more, against the New agent form's two closed sets as they are ON
/// SCREEN ([`super::VoiceNewAgentForm`]): the Mode chips the dialog offers and
/// the Agent picker's entries. Two kinds rather than one "choice" kind with the
/// param name picking the set, because the kind is what selects a resolver, and
/// a refusal has to say which of the two had no match.
/// [`ParamKind::OrchestrationRef`] resolves against the orchestrations among
/// the live agents, grouped exactly as the overview groups them into cards —
/// by orchestration id, and an agent with none as a card of its own — and
/// named by the card's title.
/// [`ParamKind::SpokenPrefix`] resolves against **the transcript
/// itself**, and nothing else — it is the words that introduced a dictation,
/// and what it resolves *to* is the rest of what the user said, taken verbatim
/// from the transcript this very utterance produced.
///
/// **A param that is checked against the transcript is a new shape here and it
/// is deliberate** (PRD #802 D6, rebuilt). The alternative was a free-text
/// *content* param, which would mean the model retyping the user's words on
/// their way into an agent's prompt. Marking a boundary is a job a model can do
/// and be checked on; supplying the content is a job it would do and could not
/// be checked on. So the kind exists to make the difference structural rather
/// than a matter of care: every value of this kind goes through
/// [`super::dictation::strip_opening`], which either finds the marked words at
/// the front of our own transcript or refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    AgentRef,
    DeckRef,
    DirRef,
    ModeRef,
    AgentTypeRef,
    OrchestrationRef,
    SpokenPrefix,
}

impl ParamKind {
    pub const ALL: [ParamKind; 7] = [
        ParamKind::AgentRef,
        ParamKind::DeckRef,
        ParamKind::DirRef,
        ParamKind::ModeRef,
        ParamKind::AgentTypeRef,
        ParamKind::OrchestrationRef,
        ParamKind::SpokenPrefix,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ParamKind::AgentRef => "agent_ref",
            ParamKind::DeckRef => "deck_ref",
            ParamKind::DirRef => "dir_ref",
            ParamKind::ModeRef => "mode_ref",
            ParamKind::AgentTypeRef => "agent_type_ref",
            ParamKind::OrchestrationRef => "orchestration_ref",
            ParamKind::SpokenPrefix => "spoken_prefix",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    /// Whether a value of this kind is one of the NAMES the app observed — an
    /// agent, a deck, a directory on screen, a Mode chip, an Agent picker
    /// entry, an orchestration (PRD #1223, audit finding A1).
    ///
    /// Such a param is resolvable only when the model was shown the names it
    /// resolves against, so with the voice settings' `labels = "withheld"` a
    /// row that takes one reports itself unavailable instead of resolving
    /// against a model that saw none of them. Exhaustive, so a new kind has to
    /// decide which side it is on.
    pub fn names_something_observed(self) -> bool {
        match self {
            ParamKind::AgentRef
            | ParamKind::DeckRef
            | ParamKind::DirRef
            | ParamKind::ModeRef
            | ParamKind::AgentTypeRef
            | ParamKind::OrchestrationRef => true,
            // The user's own words, verified against the transcript; nothing
            // observed is involved.
            ParamKind::SpokenPrefix => false,
        }
    }
}

impl fmt::Display for ParamKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a row needs besides a screen — the closed set the `requires` column
/// draws from (PRD #1223).
///
/// # Why this is a column and not a fourth `Screen`
///
/// [`Screen`] is `DeckView`, and a dialog is not a view: the New agent dialog
/// is a `useState` in the overview, mounted over it. Its directory browser is
/// the first thing the table addresses INSIDE a mounted dialog rather than at
/// app level, and the facts a row there depends on — is a listing on screen,
/// does it have a parent — are not screens at all. A pseudo-screen would have
/// made `overview` stop meaning "the overview", so every `screens =
/// ["overview"]` row would silently stop being callable while the dialog was
/// up. A second column leaves the first one meaning what it says.
///
/// Each one is answered by what the webview DECLARED with the utterance
/// ([`super::VoiceDirectories`]), never by a guess: absent declaration, every
/// requirement is unmet and the row is `callable: false`, which is what
/// "dispatching into a closed dialog" is refused as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    /// The dialog is showing a directory listing: open, a deck chosen, a
    /// listing landed, and no start in flight.
    DirectoryListing,
    /// That listing has a parent — `..` is on screen. Implies
    /// [`Requirement::DirectoryListing`].
    ParentDirectory,
    /// The New agent form's fields are live: the dialog is open with a deck
    /// and a directory chosen, no start in flight, and no start confirmation
    /// showing — the state in which a click on a chip or the picker would take.
    NewAgentForm,
    /// The New agent dialog is open at all — whatever state its form is in.
    /// What a spoken "start it" needs, because an incomplete form is a
    /// sentence the dialog says ("choose a directory first"), not a hint about
    /// being somewhere else.
    NewAgentDialog,
}

impl Requirement {
    pub const ALL: [Requirement; 4] = [
        Requirement::DirectoryListing,
        Requirement::ParentDirectory,
        Requirement::NewAgentForm,
        Requirement::NewAgentDialog,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Requirement::DirectoryListing => "directory_listing",
            Requirement::ParentDirectory => "parent_directory",
            Requirement::NewAgentForm => "new_agent_form",
            Requirement::NewAgentDialog => "new_agent_dialog",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    /// The requirement as a user-facing clause — "while the New agent dialog
    /// is open" — for a refusal that depends on it
    /// ([`CommandRow::grounding_while`]).
    pub fn while_phrase(self) -> &'static str {
        match self {
            Requirement::DirectoryListing => "while a directory listing is showing",
            Requirement::ParentDirectory => "while a directory with a parent is showing",
            Requirement::NewAgentForm => "while the New agent form is open",
            Requirement::NewAgentDialog => "while the New agent dialog is open",
        }
    }

    /// Whether what the webview declared meets this requirement.
    pub fn met_by(
        self,
        directories: Option<&VoiceDirectories>,
        new_agent: Option<&VoiceNewAgent>,
    ) -> bool {
        match self {
            Requirement::DirectoryListing => directories.is_some(),
            Requirement::ParentDirectory => directories.is_some_and(|listing| listing.has_parent),
            Requirement::NewAgentForm => new_agent.is_some_and(|dialog| dialog.form.is_some()),
            Requirement::NewAgentDialog => new_agent.is_some(),
        }
    }
}

impl fmt::Display for Requirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One declared parameter of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamSpec {
    pub name: String,
    pub kind: ParamKind,
    /// Whether the row still dispatches when the model supplies no value
    /// (`optional = true` in the TOML; absent means required).
    ///
    /// **Absence is not a refusal for an optional param, and a value that
    /// fails to resolve still is.** *"new agent"* with no deck named opens the
    /// dialog with nothing preselected, which is a complete command; *"new
    /// agent on the ghost box"* names a deck the fleet does not have, and
    /// dispatching as though it had not been said would silently drop what the
    /// user asked for. So only the missing case changes.
    ///
    /// A report may not interpolate an optional param
    /// ([`TableError::OptionalPlaceholder`]): with nothing supplied there is
    /// nothing to put in the sentence.
    pub optional: bool,
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
    /// adding a command cost a variant and a match arm, which is *implementation*,
    /// and "a new command changes no implementation" is the whole design.
    ///
    /// **What a new command DOES cost was measured by M8 and is not zero:** seven
    /// files, five of them test-only, because nine tests in this crate pin the
    /// shipped row set **by value**. That is this file's own choice — the shape
    /// of the shipped table is pinned rather than described — so the cost is one
    /// edit per place the row set is written down, and not plumbing left undone.
    pub invoke: String,
    /// Where the command can run. **Empty means everywhere**, which is what an
    /// absent `screens` key parses to.
    pub screens: Vec<Screen>,
    /// What else must hold for the row to run (PRD #1223). **Empty means
    /// nothing**, which is what an absent `requires` key parses to — every row
    /// that predates the column reads exactly as it did.
    pub requires: Vec<Requirement>,
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
    /// What in a transcript counts as the user having asked for THIS action
    /// (PRD #1223, closing audit F1). See [`ActionGrounding`].
    pub grounding: ActionGrounding,
    /// A stricter grounding that replaces [`CommandRow::grounding`] while a
    /// requirement holds — the `heard_as_whole_while` column (PRD #1223,
    /// closing audit H1). Empty for every row but `open_deck` and `close`. See
    /// [`ContextGrounding`] and [`CommandRow::grounding_for`].
    pub grounding_while: Vec<ContextGrounding>,
}

/// A row's grounding while one [`Requirement`] holds (PRD #1223, closing
/// audit H1): always [`ActionGrounding::HeardAsWhole`], and always NARROWER than
/// the row's own `heard_as` — every entry contains one of those words, which
/// the parser checks ([`TableError::LooserGroundingWhile`]), so a context can
/// only take phrasings away.
///
/// # Why grounding needed a context at all
///
/// The grounding mode was a static property of a row, chosen by one criterion:
/// is the action irreversible? For `close` that had one answer — it closes a
/// VIEW, and a view reopens — until the view is the New agent dialog, whose
/// deck, directory, Mode, Agent, Name and hand-edited Command are
/// component-local state that unmounting discards. So the same row is
/// reversible over an agent's pane and destructive over a filled form, and
/// the declaration that says which is the one the webview already sends with
/// every utterance ([`super::VoiceNewAgent`], present while the dialog is
/// mounted). Moving `close` wholesale into `heard_as_whole` would have made
/// "I'm done with this agent" stop closing a pane, which costs nothing to
/// undo; this keeps that and holds only the destructive case to the whole
/// utterance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextGrounding {
    pub requires: Requirement,
    pub grounding: ActionGrounding,
}

/// How a row's ACTION is held against the transcript before it dispatches
/// (PRD #1223, closing audit F1) — the `heard_as` / `heard_as_whole` /
/// `ungrounded` columns, of which every row declares exactly one.
///
/// **Why the action and not only its references.** The model is shown names
/// from repositories, configuration files and remote machines, and a name can
/// be written to steer it. Grounding a row's *references* (`outcome::grounding`)
/// stops a steered model from naming a thing the user did not name, but a row
/// with no reference — `submit_prompt`, which presses Enter in the open agent's
/// prompt, `go_to_parent`, `use_this_directory`, `close`, `voice_off` — had
/// nothing to hold against the transcript at all. So every row now also says
/// which words ask for it, and a pick whose words the user did not say is
/// refused (`VoiceOutcome::ActionUngrounded`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionGrounding {
    /// The row is asked for when the transcript contains at least one of these
    /// words or phrases. Each is drawn from the row's own `id` or `description`
    /// — the parser refuses one that is not ([`TableError::ForeignHeardAs`]) —
    /// so the vocabulary is the one the model is already picking on, curated
    /// rather than scraped, and reviewable beside the row.
    HeardAs(Vec<String>),
    /// The row is asked for only when the WHOLE utterance is one of these
    /// words or phrases — case, punctuation and a leading or trailing
    /// politeness word ("okay", "please", "now", …) aside — never when one
    /// merely occurs in it (PRD #1223, closing audit G1). Drawn from the row's own
    /// words under the same rule as [`ActionGrounding::HeardAs`].
    ///
    /// **For a row whose action cannot be taken back**, and today that is
    /// exactly `submit_prompt`. Token presence is evidence that the user
    /// talked ABOUT a thing, and for most rows that is enough: a wrong
    /// navigation is one more utterance to undo. A prompt submitted to an
    /// agent cannot be recalled once the agent has it, and this row's
    /// vocabulary is ordinary English — "tell it to put END after the report",
    /// "tell it the build has finished" — so an unrelated occurrence of one of
    /// its words would otherwise ground a steered pick. Whole-utterance equality is the same
    /// defence the local fast path already relies on
    /// (`dictation::SUBMIT_PHRASES`), applied to the model's path too.
    ///
    /// Do not reach for it on an ordinary row: it makes every phrasing but the
    /// listed ones fail, which is the right trade only where a false positive
    /// is unrecoverable.
    HeardAsWhole(Vec<String>),
    /// The row cannot be grounded by words, and this is why. Enumerated per
    /// row, like a `no_voice` reason, rather than being an implicit exemption;
    /// `voice_table_no_row_is_exempt_from_action_grounding` pins the set.
    Exempt(String),
}

impl CommandRow {
    /// The grounding that holds this row with `directories` and `new_agent`
    /// declared, and the requirement that selected it: the first
    /// [`CommandRow::grounding_while`] entry whose requirement is met, or the
    /// row's own [`CommandRow::grounding`] with `None`.
    pub fn grounding_for(
        &self,
        directories: Option<&VoiceDirectories>,
        new_agent: Option<&VoiceNewAgent>,
    ) -> (&ActionGrounding, Option<Requirement>) {
        self.grounding_while
            .iter()
            .find(|context| context.requires.met_by(directories, new_agent))
            .map_or((&self.grounding, None), |context| {
                (&context.grounding, Some(context.requires))
            })
    }

    /// Whether this row can run on `screen`.
    ///
    /// An empty `screens` list is "available everywhere" rather than "available
    /// nowhere", so an absent column is the permissive default.
    pub fn callable_on(&self, screen: Screen) -> bool {
        self.screens.is_empty() || self.screens.contains(&screen)
    }

    /// Whether this row can run on `screen` with `directories` and
    /// `new_agent` declared: the screen rule AND every
    /// [`CommandRow::requires`] entry. One refusal for both halves — the row's
    /// `unavailable_hint` — because from where the user stands both are "not
    /// here".
    pub fn callable(
        &self,
        screen: Screen,
        directories: Option<&VoiceDirectories>,
        new_agent: Option<&VoiceNewAgent>,
    ) -> bool {
        self.callable_on(screen)
            && self
                .requires
                .iter()
                .all(|requirement| requirement.met_by(directories, new_agent))
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

            let mut requires = Vec::new();
            for requirement in row.requires.unwrap_or_default() {
                let parsed = Requirement::parse(&requirement).ok_or_else(|| {
                    TableError::UnknownRequirement {
                        id: id.clone(),
                        requirement: requirement.clone(),
                    }
                })?;
                if !requires.contains(&parsed) {
                    requires.push(parsed);
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
                // The placeholder check below needs this, so a report that
                // interpolates an optional param is refused before it can
                // render with a hole in it.
                let optional = param.optional.unwrap_or(false);
                params.push(ParamSpec {
                    name,
                    kind,
                    optional,
                });
            }

            // The report is the one column with a stringly reference INSIDE
            // it, so it gets the same treatment `invoke` gets from M3's guard:
            // a placeholder naming no declared param is refused here rather
            // than rendering as a literal `{agent}` to the user.
            for placeholder in placeholders(&report, &id)? {
                match params.iter().find(|param| param.name == placeholder) {
                    None => {
                        return Err(TableError::UnknownPlaceholder {
                            id: id.clone(),
                            placeholder,
                        });
                    }
                    Some(param) if param.optional => {
                        return Err(TableError::OptionalPlaceholder {
                            id: id.clone(),
                            placeholder,
                        });
                    }
                    Some(_) => {}
                }
            }

            // Exactly one grounding mode per row, so no row is exempt by
            // omission and none is ambiguous about which rule holds it.
            let declared = [
                row.heard_as.is_some(),
                row.heard_as_whole.is_some(),
                row.ungrounded.is_some(),
            ];
            if declared.iter().filter(|&&present| present).count() > 1 {
                return Err(TableError::ConflictingGrounding { id: id.clone() });
            }
            // Drawn from the row's own words, so a vocabulary cannot quietly
            // grow a word the model is never told the row answers to.
            let own_words = |phrases: Vec<String>| -> Result<Vec<String>, TableError> {
                if phrases.is_empty() {
                    return Err(TableError::MissingGrounding { id: id.clone() });
                }
                let own = spoken_words(&format!("{id} {description}"));
                for phrase in &phrases {
                    let words = spoken_words(phrase);
                    if words.is_empty() || !own.windows(words.len()).any(|window| window == words) {
                        return Err(TableError::ForeignHeardAs {
                            id: id.clone(),
                            phrase: phrase.clone(),
                        });
                    }
                }
                Ok(phrases)
            };
            let grounding = if let Some(phrases) = row.heard_as {
                ActionGrounding::HeardAs(own_words(phrases)?)
            } else if let Some(phrases) = row.heard_as_whole {
                ActionGrounding::HeardAsWhole(own_words(phrases)?)
            } else if let Some(reason) = row.ungrounded {
                if reason.trim().is_empty() {
                    return Err(TableError::MissingGrounding { id: id.clone() });
                }
                ActionGrounding::Exempt(reason)
            } else {
                return Err(TableError::MissingGrounding { id: id.clone() });
            };

            // Only a token-grounded row can be made stricter in a context: a
            // whole-utterance row is already as strict as this gets, and an
            // exempt one has no words to be strict about.
            let mut grounding_while = Vec::new();
            for (requirement, phrases) in row.heard_as_whole_while.unwrap_or_default() {
                let ActionGrounding::HeardAs(base) = &grounding else {
                    return Err(TableError::MisplacedGroundingWhile { id: id.clone() });
                };
                let requires = Requirement::parse(&requirement).ok_or_else(|| {
                    TableError::UnknownRequirement {
                        id: id.clone(),
                        requirement: requirement.clone(),
                    }
                })?;
                let phrases = own_words(phrases)?;
                // Narrower only: each entry, said on its own, must already be
                // grounded by the row's `heard_as` — checked as a contiguous
                // run of words, which is stricter than the transcript check,
                // so it implies it.
                for phrase in &phrases {
                    let words = spoken_words(phrase);
                    let covered = base.iter().any(|entry| {
                        let wanted = spoken_words(entry);
                        !wanted.is_empty()
                            && words.windows(wanted.len()).any(|window| window == wanted)
                    });
                    if !covered {
                        return Err(TableError::LooserGroundingWhile {
                            id: id.clone(),
                            requirement: requires,
                            phrase: phrase.clone(),
                        });
                    }
                }
                grounding_while.push(ContextGrounding {
                    requires,
                    grounding: ActionGrounding::HeardAsWhole(phrases),
                });
            }

            commands.push(CommandRow {
                id,
                description,
                invoke,
                screens,
                requires,
                unavailable_hint,
                report,
                params,
                grounding,
                grounding_while,
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

/// A text's words, lowercased, with every non-alphanumeric character spoken as
/// a space — so `billing.api`, `billing-api` and `schedule: issues` are the
/// words a transcriber writes for them. The one tokenisation every grounding
/// check uses: the parser's `heard_as` check here, and `outcome`'s transcript
/// checks.
pub(crate) fn spoken_words(text: &str) -> Vec<String> {
    text.chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .map(str::to_string)
        .collect()
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
    /// A `requires` entry outside the closed [`Requirement`] set.
    UnknownRequirement { id: String, requirement: String },
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
    /// A `report` placeholder naming an OPTIONAL param, which would render
    /// as a literal `{name}` whenever the user named nothing.
    OptionalPlaceholder { id: String, placeholder: String },
    /// A `report` whose braces do not pair up.
    MalformedReport { id: String, detail: String },
    /// A row with none of a non-empty `heard_as`, a non-empty
    /// `heard_as_whole` or an `ungrounded` reason: nothing would say whether
    /// the user asked for it.
    MissingGrounding { id: String },
    /// A row declaring more than one of `heard_as`, `heard_as_whole` and
    /// `ungrounded`.
    ConflictingGrounding { id: String },
    /// A `heard_as` or `heard_as_whole` entry that is not words of the row's
    /// own `id` or `description`.
    ForeignHeardAs { id: String, phrase: String },
    /// A `heard_as_whole_while` on a row whose own grounding is not
    /// `heard_as` — there is nothing for a context to narrow.
    MisplacedGroundingWhile { id: String },
    /// A `heard_as_whole_while` entry that none of the row's `heard_as`
    /// entries grounds, which would make the row LOOSER in that context.
    LooserGroundingWhile {
        id: String,
        requirement: Requirement,
        phrase: String,
    },
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
            TableError::UnknownRequirement { id, requirement } => write!(
                f,
                "command `{id}` requires the unknown `{requirement}`; the requirements are {}",
                joined(
                    Requirement::ALL
                        .iter()
                        .map(|requirement| requirement.as_str())
                )
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
            TableError::OptionalPlaceholder { id, placeholder } => write!(
                f,
                "command `{id}`'s report names `{{{placeholder}}}`, which is optional and so may have nothing to say"
            ),
            TableError::MalformedReport { id, detail } => {
                write!(f, "command `{id}`'s report has {detail}")
            }
            TableError::MissingGrounding { id } => write!(
                f,
                "command `{id}` has no `heard_as` or `heard_as_whole` (the words that ask for it) and no `ungrounded` reason"
            ),
            TableError::ConflictingGrounding { id } => write!(
                f,
                "command `{id}` declares more than one of `heard_as`, `heard_as_whole` and `ungrounded`; it is exactly one"
            ),
            TableError::ForeignHeardAs { id, phrase } => write!(
                f,
                "command `{id}`'s grounding entry `{phrase}` is not words of its own id or description"
            ),
            TableError::MisplacedGroundingWhile { id } => write!(
                f,
                "command `{id}` declares `heard_as_whole_while` without `heard_as`; only a token-grounded row can be narrowed in a context"
            ),
            TableError::LooserGroundingWhile {
                id,
                requirement,
                phrase,
            } => write!(
                f,
                "command `{id}`'s `heard_as_whole_while.{requirement}` entry `{phrase}` contains none of its `heard_as` entries, so it would loosen the row there rather than narrow it"
            ),
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
            "heard_as = [\"open\"]",
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
                // No screens: callable everywhere. `close` is here because the
                // voice surface's own overlay can be up on any of the three and
                // `screens` cannot express "an overlay is open"; the precedence
                // between that overlay and an agent pane is decided at dispatch.
                ("close", "closeTopmost", vec![]),
                ("open_settings", "openSettings", vec!["deck"]),
                // No screens: see the row's own comment — stopping must never
                // be unavailable.
                ("voice_off", "stopVoice", vec![]),
                ("list_commands", "showVoiceCommands", vec![]),
                // `agent` and nothing else: the dictation pair's whole
                // targeting rule is "the pane the user is looking at", so with
                // no pane open there is no one agent to mean.
                ("dictate_to_agent", "dictateToAgent", vec!["agent"]),
                ("submit_prompt", "submitAgentPrompt", vec!["agent"]),
                // `overview` alone: the dialog lives there (PRD #1223).
                ("open_new_agent", "openNewAgent", vec!["overview"]),
                // The directory browser inside that dialog — `overview`, plus
                // a `requires` the next test pins (PRD #1223).
                ("open_dir", "openDirectory", vec!["overview"]),
                ("go_to_parent", "goToParentDirectory", vec!["overview"]),
                ("use_this_directory", "useThisDirectory", vec!["overview"]),
                // The rest of the New agent form — `overview`, plus
                // `requires = ["new_agent_form"]` (PRD #1223).
                ("choose_mode", "chooseNewAgentMode", vec!["overview"]),
                ("choose_agent_type", "chooseNewAgentType", vec!["overview"]),
                ("name_new_agent", "nameNewAgent", vec!["overview"]),
                // PRD #802 D5's set: each only opens a confirmation.
                ("start_new_agent", "confirmStartNewAgent", vec!["overview"]),
                ("stop_agent", "confirmStopAgent", vec!["overview"]),
                (
                    "close_orchestration",
                    "confirmCloseOrchestration",
                    vec!["overview"]
                ),
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
                optional: false,
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
        // `deck_ref` used to be this fixture, back when the set was two; it is
        // a real kind now (PRD #1223), so the negative case is one no resolver
        // will ever claim.
        let source = one_row().replace("kind = \"agent_ref\"", "kind = \"project_ref\"");
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::UnknownParamKind {
                id: "open_agent".to_string(),
                param: "agent".to_string(),
                kind: "project_ref".to_string(),
            }
        );
        let message = error.to_string();
        assert!(
            message.contains(
                "`agent_ref`, `deck_ref`, `dir_ref`, `mode_ref`, `agent_type_ref`, `orchestration_ref`, `spoken_prefix`"
            ),
            "{message}"
        );
    }

    #[test]
    fn voice_table_open_new_agent_takes_one_optional_deck_ref() {
        // PRD #1223: the first row with a `deck_ref`, and the first optional
        // param. Pinned by value because both halves are load-bearing — a
        // required `deck` would refuse the bare "new agent", and an
        // `agent_ref` would resolve the deck's name against the wrong list.
        let row = super::table()
            .row("open_new_agent")
            .expect("open_new_agent is in the table");
        assert_eq!(row.invoke, "openNewAgent");
        assert_eq!(row.screens, vec![Screen::Overview]);
        assert_eq!(
            row.params,
            vec![ParamSpec {
                name: "deck".to_string(),
                kind: ParamKind::DeckRef,
                optional: true,
            }]
        );
        assert_eq!(row.report, "Opening the New agent dialog.");
    }

    #[test]
    fn voice_table_params_are_required_unless_marked_optional() {
        let required = CommandTable::parse(&one_row()).expect("parses");
        assert!(!required.rows()[0].params[0].optional);

        let source = one_row()
            .replace("report = \"Opening {agent}.\"", "report = \"Opening.\"")
            .replace(
                "kind = \"agent_ref\"",
                "kind = \"deck_ref\"\noptional = true",
            );
        let optional = CommandTable::parse(&source).expect("parses");
        assert_eq!(
            optional.rows()[0].params[0],
            ParamSpec {
                name: "agent".to_string(),
                kind: ParamKind::DeckRef,
                optional: true,
            }
        );
    }

    #[test]
    fn voice_table_rejects_a_report_placeholder_naming_an_optional_param() {
        // With nothing supplied there is nothing to interpolate, so the
        // sentence would reach the user as a literal `{agent}`.
        let source = one_row().replace(
            "kind = \"agent_ref\"",
            "kind = \"agent_ref\"\noptional = true",
        );
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::OptionalPlaceholder {
                id: "open_agent".to_string(),
                placeholder: "agent".to_string(),
            }
        );
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

    // -- action grounding (PRD #1223, closing audit F1) --------------------

    #[test]
    fn voice_table_rejects_a_row_that_says_nothing_about_grounding() {
        let source = one_row().replace("heard_as = [\"open\"]\n", "");
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::MissingGrounding {
                id: "open_agent".to_string()
            }
        );
        for empty in ["heard_as = []", "ungrounded = \"  \""] {
            let source = one_row().replace("heard_as = [\"open\"]", empty);
            let error = CommandTable::parse(&source).expect_err("refused");
            assert!(
                matches!(error, TableError::MissingGrounding { .. }),
                "{empty}: {error:?}"
            );
        }
    }

    #[test]
    fn voice_table_rejects_a_row_both_grounded_and_exempt() {
        let source = one_row().replace(
            "heard_as = [\"open\"]",
            "heard_as = [\"open\"]\nungrounded = \"because\"",
        );
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::ConflictingGrounding {
                id: "open_agent".to_string()
            }
        );
    }

    #[test]
    fn voice_table_rejects_a_heard_as_entry_not_in_the_rows_own_words() {
        // "Open one agent." is the description, so `open agent` (the id's
        // words) and `one agent` are its own; `send` and `agent one` are not.
        for good in ["open", "one agent", "open agent", "OPEN"] {
            let source =
                one_row().replace("heard_as = [\"open\"]", &format!("heard_as = [\"{good}\"]"));
            assert!(CommandTable::parse(&source).is_ok(), "{good}");
        }
        for foreign in ["send", "agent one", "", "  "] {
            let source = one_row().replace(
                "heard_as = [\"open\"]",
                &format!("heard_as = [\"{foreign}\"]"),
            );
            let error = CommandTable::parse(&source).expect_err("refused");
            assert_eq!(
                error,
                TableError::ForeignHeardAs {
                    id: "open_agent".to_string(),
                    phrase: foreign.to_string()
                },
                "{foreign:?}"
            );
        }
    }

    #[test]
    fn voice_table_accepts_an_exempt_row_with_its_reason() {
        let source = one_row().replace(
            "heard_as = [\"open\"]",
            "ungrounded = \"a reason someone can disagree with\"",
        );
        let table = CommandTable::parse(&source).expect("parses");
        assert_eq!(
            table.rows()[0].grounding,
            ActionGrounding::Exempt("a reason someone can disagree with".to_string())
        );
    }

    /// Every shipped row is grounded by its own words, and the set of rows
    /// exempt from that is pinned — empty today — so an exemption is a
    /// decision someone makes in review rather than an omission.
    #[test]
    fn voice_table_no_row_is_exempt_from_action_grounding() {
        let exempt: Vec<&str> = super::table()
            .rows()
            .iter()
            .filter(|row| matches!(row.grounding, ActionGrounding::Exempt(_)))
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(exempt, Vec::<&str>::new());
        for row in super::table().rows() {
            let (ActionGrounding::HeardAs(phrases) | ActionGrounding::HeardAsWhole(phrases)) =
                &row.grounding
            else {
                continue;
            };
            assert!(!phrases.is_empty(), "{}", row.id);
        }
    }

    /// The rows held to the WHOLE utterance rather than a word in it (PRD
    /// #1223, closing audit G1), pinned: exactly the row whose action cannot
    /// be taken back. Adding one makes every phrasing but its listed ones
    /// fail, so it is a decision for review, not a default.
    #[test]
    fn voice_table_whole_utterance_rows_are_the_deliberate_set() {
        let whole: Vec<&str> = super::table()
            .rows()
            .iter()
            .filter(|row| matches!(row.grounding, ActionGrounding::HeardAsWhole(_)))
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(whole, vec!["submit_prompt"]);
    }

    /// The rows whose grounding changes with a declared context (PRD #1223,
    /// closing audit H1), pinned with the context and its phrases: `open_deck`
    /// and `close`, over the New agent dialog — the two rows whose dispatch
    /// unmounts it — and nothing else.
    #[test]
    fn voice_table_context_grounded_rows_are_the_deliberate_set() {
        let contextual: Vec<(&str, Requirement)> = super::table()
            .rows()
            .iter()
            .flat_map(|row| {
                row.grounding_while
                    .iter()
                    .map(move |context| (row.id.as_str(), context.requires))
            })
            .collect();
        assert_eq!(
            contextual,
            vec![
                ("open_deck", Requirement::NewAgentDialog),
                ("close", Requirement::NewAgentDialog),
            ]
        );
        let open_deck = super::table().row("open_deck").expect("row");
        assert_eq!(
            open_deck.grounding_while[0].grounding,
            ActionGrounding::HeardAsWhole(
                [
                    "go back to the deck",
                    "back to the deck",
                    "return to the deck",
                    "the deck",
                    "deck",
                    "show the terminals",
                ]
                .map(String::from)
                .to_vec()
            )
        );
        let close = super::table().row("close").expect("row");
        assert_eq!(
            close.grounding_while[0].grounding,
            ActionGrounding::HeardAsWhole(
                [
                    "close",
                    "close this",
                    "close it",
                    "close the dialog",
                    "close this dialog",
                    "dismiss",
                    "dismiss this",
                    "hide this",
                    "get rid of this",
                ]
                .map(String::from)
                .to_vec()
            )
        );
    }

    #[test]
    fn voice_table_parses_a_context_grounding_under_the_own_words_rule() {
        let with = |column: &str| one_row().replace("heard_as = [\"open\"]", column);
        let table = CommandTable::parse(&with(
            "heard_as = [\"open\"]\nheard_as_whole_while.new_agent_dialog = [\"open agent\"]",
        ))
        .expect("parses");
        let row = &table.rows()[0];
        assert_eq!(
            row.grounding_while,
            vec![ContextGrounding {
                requires: Requirement::NewAgentDialog,
                grounding: ActionGrounding::HeardAsWhole(vec!["open agent".to_string()]),
            }]
        );
        // Outside the context the row's own grounding holds.
        assert_eq!(
            row.grounding_for(None, None),
            (&ActionGrounding::HeardAs(vec!["open".to_string()]), None)
        );

        // A context on a row with nothing to narrow.
        for base in ["heard_as_whole = [\"open\"]", "ungrounded = \"because\""] {
            let error = CommandTable::parse(&with(&format!(
                "{base}\nheard_as_whole_while.new_agent_dialog = [\"open\"]"
            )))
            .expect_err("refused");
            assert_eq!(
                error,
                TableError::MisplacedGroundingWhile {
                    id: "open_agent".to_string()
                },
                "{base}"
            );
        }
        // An unknown context.
        let error = CommandTable::parse(&with(
            "heard_as = [\"open\"]\nheard_as_whole_while.somewhere = [\"open\"]",
        ))
        .expect_err("refused");
        assert!(
            matches!(error, TableError::UnknownRequirement { ref requirement, .. } if requirement == "somewhere"),
            "{error:?}"
        );
        // A foreign phrase, and an empty list.
        let error = CommandTable::parse(&with(
            "heard_as = [\"open\"]\nheard_as_whole_while.new_agent_dialog = [\"send\"]",
        ))
        .expect_err("refused");
        assert!(
            matches!(error, TableError::ForeignHeardAs { .. }),
            "{error:?}"
        );
        let error = CommandTable::parse(&with(
            "heard_as = [\"open\"]\nheard_as_whole_while.new_agent_dialog = []",
        ))
        .expect_err("refused");
        assert!(
            matches!(error, TableError::MissingGrounding { .. }),
            "{error:?}"
        );
    }

    /// A context may only take phrasings away: an entry that none of the
    /// row's `heard_as` grounds would make the row LOOSER there.
    #[test]
    fn voice_table_rejects_a_context_grounding_that_loosens_the_row() {
        let source = one_row().replace(
            "heard_as = [\"open\"]",
            "heard_as = [\"open\"]\nheard_as_whole_while.new_agent_dialog = [\"one agent\"]",
        );
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::LooserGroundingWhile {
                id: "open_agent".to_string(),
                requirement: Requirement::NewAgentDialog,
                phrase: "one agent".to_string(),
            }
        );
        assert!(
            error.to_string().contains("would loosen the row"),
            "{error}"
        );
    }

    #[test]
    fn voice_table_rejects_a_row_declaring_more_than_one_grounding_mode() {
        for extra in [
            "heard_as = [\"open\"]\nheard_as_whole = [\"open\"]",
            "heard_as_whole = [\"open\"]\nungrounded = \"because\"",
            "heard_as = [\"open\"]\nheard_as_whole = [\"open\"]\nungrounded = \"because\"",
        ] {
            let source = one_row().replace("heard_as = [\"open\"]", extra);
            let error = CommandTable::parse(&source).expect_err("refused");
            assert_eq!(
                error,
                TableError::ConflictingGrounding {
                    id: "open_agent".to_string()
                },
                "{extra}"
            );
        }
    }

    #[test]
    fn voice_table_parses_a_whole_utterance_row_under_the_own_words_rule() {
        let source =
            one_row().replace("heard_as = [\"open\"]", "heard_as_whole = [\"open agent\"]");
        let table = CommandTable::parse(&source).expect("parses");
        assert_eq!(
            table.rows()[0].grounding,
            ActionGrounding::HeardAsWhole(vec!["open agent".to_string()])
        );
        let empty = one_row().replace("heard_as = [\"open\"]", "heard_as_whole = []");
        assert_eq!(
            CommandTable::parse(&empty).expect_err("refused"),
            TableError::MissingGrounding {
                id: "open_agent".to_string()
            }
        );
        let foreign = one_row().replace("heard_as = [\"open\"]", "heard_as_whole = [\"send\"]");
        assert_eq!(
            CommandTable::parse(&foreign).expect_err("refused"),
            TableError::ForeignHeardAs {
                id: "open_agent".to_string(),
                phrase: "send".to_string()
            }
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
    fn voice_table_each_shipped_row_is_callable_somewhere() {
        // A row callable nowhere is a row nothing can ever run, which no column
        // would report and no test would otherwise catch.
        for row in super::table().rows() {
            assert!(
                Screen::ALL.iter().any(|&screen| row.callable_on(screen)),
                "`{}` is callable nowhere",
                row.id
            );
        }
    }

    #[test]
    fn voice_table_rows_callable_everywhere_are_the_deliberate_set() {
        // This test used to be the second half of the one above, asserting that
        // EVERY row also had a screen it could not run on — "so its hint is
        // unreachable". That was true of a navigation-only table and stopped
        // being true the moment a row had to be callable everywhere: stopping
        // must never be unavailable, and neither must the phrase that lists
        // what can be said.
        //
        // The property is therefore pinned rather than asserted universally. A
        // row in this list carries an `unavailable_hint` nothing renders, which
        // is the cost of keeping that column unconditional — so the list is
        // short on purpose and a new entry in it is a decision someone made.
        let everywhere: Vec<&str> = super::table()
            .rows()
            .iter()
            .filter(|row| Screen::ALL.iter().all(|&screen| row.callable_on(screen)))
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(everywhere, vec!["close", "voice_off", "list_commands"]);
        // And every OTHER row still has both cases, which is what keeps the
        // not-here sentence reachable for the rows that can produce it.
        for row in super::table().rows() {
            if everywhere.contains(&row.id.as_str()) {
                continue;
            }
            assert!(
                Screen::ALL.iter().any(|&screen| !row.callable_on(screen)),
                "`{}` is callable everywhere and is not in the pinned set",
                row.id
            );
        }
    }

    #[test]
    fn voice_table_the_go_back_claims_stay_scoped_to_the_bare_phrase() {
        // Two rows claim the phrase "go back" for their own screen, and each
        // scopes its claim to the BARE form with the word `bare`. That word is
        // what keeps `open-settings-deck-collision` — "go back to settings" on
        // the overview, where the answer is the NOT-callable `open_settings` —
        // out of the callability tie-break's reach, since `open_deck` is
        // callable there and claims the phrase (see `schema::TOOL_INSTRUCTIONS`).
        //
        // Pinned here rather than left to the phrase fixtures because those
        // need a credential and run in no CI, so dropping the word would go
        // unnoticed until somebody next spent a key on the suite.
        for id in ["open_deck", "close"] {
            let row = super::table().row(id).expect("a shipped row");
            assert!(
                row.description.contains("\"go back\""),
                "`{id}` stopped claiming the phrase: {}",
                row.description
            );
            assert!(
                row.description.contains("bare \"go back\""),
                "`{id}` claims \"go back\" unscoped — the word `bare` is what keeps                  open-settings-deck-collision off the tie-break: {}",
                row.description
            );
        }
    }

    #[test]
    fn voice_table_callable_per_screen_for_the_shipped_rows() {
        let table = super::table();
        // With nothing declared: the directory rows `requires` a listing, so
        // they are callable on no screen until the dialog shows one (PRD
        // #1223, `voice_table_directory_rows_are_gated_by_requires`).
        let callable = |screen: Screen| {
            table
                .rows()
                .iter()
                .filter(|row| row.callable(screen, None, None))
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>()
        };
        // `open_settings` is on the deck and NOT on the overview: the Settings
        // overlay is reachable only from the deck rail, and a row exposes
        // behaviour that already exists rather than adding a route of its own.
        assert_eq!(
            callable(Screen::Deck),
            vec![
                "open_agent",
                "open_overview",
                "close",
                "open_settings",
                "voice_off",
                "list_commands"
            ]
        );
        assert_eq!(
            callable(Screen::Overview),
            vec![
                "open_agent",
                "open_deck",
                "close",
                "voice_off",
                "list_commands",
                "open_new_agent",
                // PRD #802 D5's two stops: on the overview, where their
                // controls are. Each only opens a confirmation.
                "stop_agent",
                "close_orchestration"
            ]
        );
        assert_eq!(
            callable(Screen::Agent),
            vec![
                "close",
                "voice_off",
                "list_commands",
                "dictate_to_agent",
                "submit_prompt"
            ]
        );
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
        assert_eq!(ParamKind::parse("deck_ref"), Some(ParamKind::DeckRef));
        assert_eq!(ParamKind::parse("dir_ref"), Some(ParamKind::DirRef));
        assert_eq!(ParamKind::parse("mode_ref"), Some(ParamKind::ModeRef));
        assert_eq!(
            ParamKind::parse("orchestration_ref"),
            Some(ParamKind::OrchestrationRef)
        );
        assert_eq!(
            ParamKind::parse("agent_type_ref"),
            Some(ParamKind::AgentTypeRef)
        );
        assert_eq!(ParamKind::parse("agentRef"), None);
        assert_eq!(ParamKind::parse("deckRef"), None);
        assert_eq!(ParamKind::parse("dirRef"), None);
    }

    #[test]
    fn voice_table_requirement_round_trips_its_spelling() {
        for requirement in Requirement::ALL {
            assert_eq!(Requirement::parse(requirement.as_str()), Some(requirement));
        }
        assert_eq!(
            Requirement::parse("directory_listing"),
            Some(Requirement::DirectoryListing)
        );
        assert_eq!(
            Requirement::parse("parent_directory"),
            Some(Requirement::ParentDirectory)
        );
        assert_eq!(
            Requirement::parse("new_agent_form"),
            Some(Requirement::NewAgentForm)
        );
        assert_eq!(
            Requirement::parse("new_agent_dialog"),
            Some(Requirement::NewAgentDialog)
        );
        assert_eq!(Requirement::parse("dialog_open"), None);
    }

    fn listing(has_parent: bool) -> VoiceDirectories {
        VoiceDirectories {
            deck_id: "deck-local".to_string(),
            path: "/home/dev".to_string(),
            has_parent,
            entries: Vec::new(),
        }
    }

    /// PRD #1223: each directory row pinned by value — invoke, screens,
    /// requires, params, report. `requires` is the load-bearing half: without
    /// it a row would be callable on the overview with the dialog closed and
    /// dispatch into nothing.
    #[test]
    fn voice_table_directory_rows_are_pinned_by_value() {
        let table = super::table();
        let open_dir = table.row("open_dir").expect("open_dir is in the table");
        assert_eq!(open_dir.invoke, "openDirectory");
        assert_eq!(open_dir.screens, vec![Screen::Overview]);
        assert_eq!(open_dir.requires, vec![Requirement::DirectoryListing]);
        assert_eq!(
            open_dir.params,
            vec![ParamSpec {
                name: "dir".to_string(),
                kind: ParamKind::DirRef,
                optional: false,
            }]
        );
        assert_eq!(open_dir.report, "Opening {dir}.");

        let parent = table
            .row("go_to_parent")
            .expect("go_to_parent is in the table");
        assert_eq!(parent.invoke, "goToParentDirectory");
        assert_eq!(parent.screens, vec![Screen::Overview]);
        assert_eq!(parent.requires, vec![Requirement::ParentDirectory]);
        assert!(parent.params.is_empty());
        assert_eq!(parent.report, "Going up.");

        let confirm = table
            .row("use_this_directory")
            .expect("use_this_directory is in the table");
        assert_eq!(confirm.invoke, "useThisDirectory");
        assert_eq!(confirm.screens, vec![Screen::Overview]);
        assert_eq!(confirm.requires, vec![Requirement::DirectoryListing]);
        assert!(confirm.params.is_empty());
        assert_eq!(confirm.report, "Using this directory.");

        // The directory rows require the browser, and nothing else in the
        // table does.
        let needs_browser: Vec<&str> = table
            .rows()
            .iter()
            .filter(|row| {
                row.requires.iter().any(|requirement| {
                    matches!(
                        requirement,
                        Requirement::DirectoryListing | Requirement::ParentDirectory
                    )
                })
            })
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(
            needs_browser,
            vec!["open_dir", "go_to_parent", "use_this_directory"]
        );
    }

    fn form() -> VoiceNewAgent {
        VoiceNewAgent {
            form: Some(super::super::VoiceNewAgentForm {
                deck_id: "deck-local".to_string(),
                path: "/home/dev/code".to_string(),
                modes: Vec::new(),
                agent_types: Vec::new(),
                withheld_modes: Vec::new(),
            }),
        }
    }

    /// PRD #1223: the three fill rows pinned by value. Command has no row —
    /// `voice_table_no_row_fills_the_command` says so as a property.
    #[test]
    fn voice_table_form_rows_are_pinned_by_value() {
        let table = super::table();
        let pinned = |id: &str, invoke: &str, param: &str, kind: ParamKind, report: &str| {
            let row = table
                .row(id)
                .unwrap_or_else(|| panic!("{id} is in the table"));
            assert_eq!(row.invoke, invoke, "{id}");
            assert_eq!(row.screens, vec![Screen::Overview], "{id}");
            assert_eq!(row.requires, vec![Requirement::NewAgentForm], "{id}");
            assert_eq!(
                row.params,
                vec![ParamSpec {
                    name: param.to_string(),
                    kind,
                    optional: false,
                }],
                "{id}"
            );
            assert_eq!(row.report, report, "{id}");
        };
        pinned(
            "choose_mode",
            "chooseNewAgentMode",
            "mode",
            ParamKind::ModeRef,
            "Mode: {mode}.",
        );
        pinned(
            "choose_agent_type",
            "chooseNewAgentType",
            "agent_type",
            ParamKind::AgentTypeRef,
            "Agent: {agent_type}.",
        );
        pinned(
            "name_new_agent",
            "nameNewAgent",
            "prefix",
            ParamKind::SpokenPrefix,
            "Name set.",
        );
    }

    /// The form rows run only while the form is live, and only on the
    /// overview: a closed dialog, a dialog with no form, and another screen
    /// each refuse.
    #[test]
    fn voice_table_form_rows_are_gated_by_the_declared_form() {
        let table = super::table();
        let live = form();
        let no_form = VoiceNewAgent { form: None };
        for id in ["choose_mode", "choose_agent_type", "name_new_agent"] {
            let row = table.row(id).expect("present");
            assert!(!row.callable(Screen::Overview, None, None), "{id}");
            assert!(
                !row.callable(Screen::Overview, None, Some(&no_form)),
                "{id}"
            );
            assert!(row.callable(Screen::Overview, None, Some(&live)), "{id}");
            assert!(!row.callable(Screen::Deck, None, Some(&live)), "{id}");
            assert!(!row.callable(Screen::Agent, None, Some(&live)), "{id}");
        }
    }

    /// PRD #802 D5, as a property of the table: the rows that start or stop
    /// something are exactly these three, each dispatches a registry entry
    /// that only opens a confirmation, and each tells the model — and the
    /// user, in its report — that nothing has happened yet.
    #[test]
    fn voice_table_d5_rows_only_ask() {
        let table = super::table();
        let asking: Vec<(&str, &str)> = table
            .rows()
            .iter()
            .filter(|row| row.invoke.starts_with("confirm"))
            .map(|row| (row.id.as_str(), row.invoke.as_str()))
            .collect();
        assert_eq!(
            asking,
            vec![
                ("start_new_agent", "confirmStartNewAgent"),
                ("stop_agent", "confirmStopAgent"),
                ("close_orchestration", "confirmCloseOrchestration"),
            ]
        );
        for (id, _) in &asking {
            let row = table.row(id).expect("present");
            assert!(
                row.report.contains("nothing has"),
                "{id}'s report must not claim an act: {}",
                row.report
            );
            assert!(
                row.description.contains("by itself") && row.description.contains("confirm"),
                "{id}'s description must say it only asks: {}",
                row.description
            );
            assert_eq!(row.screens, vec![Screen::Overview], "{id}");
        }
        let start = table.row("start_new_agent").expect("present");
        assert_eq!(start.requires, vec![Requirement::NewAgentDialog]);
        assert!(start.params.is_empty());
        let stop = table.row("stop_agent").expect("present");
        assert_eq!(stop.params[0].kind, ParamKind::AgentRef);
        let close = table.row("close_orchestration").expect("present");
        assert_eq!(close.params[0].kind, ParamKind::OrchestrationRef);
    }

    /// "start it" needs the dialog OPEN, not a complete form: an incomplete
    /// form is the dialog's sentence to say, so the row must reach it.
    #[test]
    fn voice_table_start_needs_the_dialog_open_and_nothing_more() {
        let table = super::table();
        let start = table.row("start_new_agent").expect("present");
        let open_no_form = VoiceNewAgent { form: None };
        assert!(!start.callable(Screen::Overview, None, None));
        assert!(start.callable(Screen::Overview, None, Some(&open_no_form)));
        assert!(start.callable(Screen::Overview, None, Some(&form())));
        assert!(!start.callable(Screen::Deck, None, Some(&open_no_form)));
    }

    /// The Command decision, as a property: no row's description offers to
    /// fill the command line, and every fill row says so. It is the one field
    /// that executes, so it stays typed by hand (see `commands.toml`).
    #[test]
    fn voice_table_no_row_fills_the_command() {
        let table = super::table();
        for id in ["choose_mode", "name_new_agent"] {
            let row = table.row(id).expect("present");
            assert!(
                row.description.contains("typed by hand"),
                "{id} must keep the command line out of reach: {}",
                row.description
            );
        }
        let form_rows: Vec<&str> = table
            .rows()
            .iter()
            .filter(|row| row.requires.contains(&Requirement::NewAgentForm))
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(
            form_rows,
            vec!["choose_mode", "choose_agent_type", "name_new_agent"],
            "a new form row is a decision about the Command field too — see commands.toml"
        );
    }

    #[test]
    fn voice_table_directory_rows_are_gated_by_requires() {
        let table = super::table();
        let with_parent = listing(true);
        let at_root = listing(false);
        let row = |id: &str| table.row(id).expect("present");
        for id in ["open_dir", "use_this_directory"] {
            assert!(
                !row(id).callable(Screen::Overview, None, None),
                "{id}: dialog closed"
            );
            assert!(
                row(id).callable(Screen::Overview, Some(&with_parent), None),
                "{id}"
            );
            assert!(
                row(id).callable(Screen::Overview, Some(&at_root), None),
                "{id}"
            );
            assert!(
                !row(id).callable(Screen::Deck, Some(&with_parent), None),
                "{id}: off the overview"
            );
        }
        assert!(!row("go_to_parent").callable(Screen::Overview, None, None));
        assert!(row("go_to_parent").callable(Screen::Overview, Some(&with_parent), None));
        assert!(
            !row("go_to_parent").callable(Screen::Overview, Some(&at_root), None),
            "a root has no `..` to go to"
        );
        assert!(!row("go_to_parent").callable(Screen::Agent, Some(&with_parent), None));
    }

    #[test]
    fn voice_table_absent_requires_needs_nothing() {
        let parsed = CommandTable::parse(&one_row()).expect("parses");
        let row = &parsed.rows()[0];
        assert!(row.requires.is_empty());
        assert!(row.callable(Screen::Deck, None, None));
    }

    #[test]
    fn voice_table_parses_requires_and_rejects_an_unknown_requirement() {
        let source = one_row().replace(
            "screens = [\"deck\"]",
            "screens = [\"deck\"]\nrequires = [\"parent_directory\", \"parent_directory\"]",
        );
        let parsed = CommandTable::parse(&source).expect("parses");
        // A repeat is folded, as a repeated screen is.
        assert_eq!(
            parsed.rows()[0].requires,
            vec![Requirement::ParentDirectory]
        );

        let source = one_row().replace(
            "screens = [\"deck\"]",
            "screens = [\"deck\"]\nrequires = [\"dialog_open\"]",
        );
        let error = CommandTable::parse(&source).expect_err("refused");
        assert_eq!(
            error,
            TableError::UnknownRequirement {
                id: "open_agent".to_string(),
                requirement: "dialog_open".to_string(),
            }
        );
        let message = error.to_string();
        assert!(
            message.contains("`directory_listing`, `parent_directory`"),
            "{message}"
        );
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
    requires: Option<Vec<String>>,
    unavailable_hint: Option<String>,
    report: Option<String>,
    heard_as: Option<Vec<String>>,
    heard_as_whole: Option<Vec<String>>,
    heard_as_whole_while: Option<std::collections::BTreeMap<String, Vec<String>>>,
    ungrounded: Option<String>,
    #[serde(default)]
    params: Vec<RawParam>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawParam {
    name: Option<String>,
    kind: Option<String>,
    optional: Option<bool>,
}
